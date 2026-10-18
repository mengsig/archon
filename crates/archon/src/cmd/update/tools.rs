//! Downloading, hashing and unpacking with the installer's own programs: the
//! workspace has no HTTP, TLS or SHA-256 crate, and they honour system proxies.

use std::ffi::{OsStr, OsString};
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::{Output, Stdio};
use std::time::Duration;

use anyhow::Context as _;

use super::super::doctor::find_program;

const CONNECT_TIMEOUT_SECS: u64 = 20;
/// Past the downloader's own limit, so its error is the one reported.
const KILL_MARGIN: Duration = Duration::from_secs(10);
const LOCAL_TOOL_TIMEOUT: Duration = Duration::from_secs(120);
const SHA256_HEX_LEN: usize = 64;

/// Runs `program args`, killed once `timeout` passes; a non-zero exit is an
/// error that carries what the program wrote to stderr.
pub async fn run(program: &Path, args: &[&OsStr], timeout: Duration) -> anyhow::Result<Output> {
    assert!(
        !timeout.is_zero(),
        "a zero timeout never lets a command run"
    );
    let shown = program.display();
    let mut command = tokio::process::Command::new(program);
    command.args(args).stdin(Stdio::null()).kill_on_drop(true);
    let output = tokio::time::timeout(timeout, command.output())
        .await
        .map_err(|_| anyhow::anyhow!("`{shown}` did not finish within {timeout:?}"))?
        .with_context(|| format!("running `{shown}`"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("`{shown}` failed ({}): {}", output.status, stderr.trim());
    }
    Ok(output)
}

fn require(names: &[&str], purpose: &str) -> anyhow::Result<PathBuf> {
    assert!(!names.is_empty(), "at least one program can do the job");
    names
        .iter()
        .find_map(|name| find_program(Path::new(name)))
        .ok_or_else(|| {
            let wanted = names.join(" or ");
            anyhow::anyhow!("{wanted} was not found on PATH; `archon update` needs it {purpose}")
        })
}

/// The arguments that make curl or wget fetch `url` into `dest`; an
/// `https://` address is never downgraded or redirected to plain http.
fn download_args(is_curl: bool, url: &str, dest: &Path, limit: Duration) -> Vec<OsString> {
    assert!(!limit.is_zero(), "a download has a time limit");
    let seconds = limit.as_secs().max(1).to_string();
    let connect = CONNECT_TIMEOUT_SECS.to_string();
    let https = url.starts_with("https://");
    let mut args: Vec<&OsStr> = Vec::new();
    if is_curl {
        args.extend(["-fsSL", "--retry", "2", "--connect-timeout", &connect].map(OsStr::new));
        args.extend(["--max-time", &seconds].map(OsStr::new));
        if https {
            args.extend(["--proto", "=https", "--tlsv1.2"].map(OsStr::new));
        }
        args.push(OsStr::new("-o"));
    } else {
        args.extend(["-q", "-T", &connect].map(OsStr::new));
        if https {
            args.push(OsStr::new("--https-only"));
        }
        args.push(OsStr::new("-O"));
    }
    args.extend([dest.as_os_str(), OsStr::new(url)]);
    args.into_iter().map(OsString::from).collect()
}

/// Download `url` to `dest` within `limit`. A failure leaves no partial
/// file behind that could be taken for the download.
pub async fn download(url: &str, dest: &Path, limit: Duration) -> anyhow::Result<()> {
    assert!(
        url.starts_with("https://") || url.starts_with("http://"),
        "release addresses are validated when read: {url}"
    );
    let program = require(&["curl", "wget"], "to download releases")?;
    let is_curl = program.file_name() == Some(OsStr::new("curl"));
    let args = download_args(is_curl, url, dest, limit);
    let arg_refs: Vec<&OsStr> = args.iter().map(OsString::as_os_str).collect();
    let result = run(&program, &arg_refs, limit + KILL_MARGIN).await;
    if result.is_err() && dest.exists() {
        std::fs::remove_file(dest).with_context(|| format!("removing {}", dest.display()))?;
    }
    result
        .map(|_| ())
        .with_context(|| format!("downloading {url}"))
}

/// The SHA-256 of `path`, as 64 lowercase hex digits.
pub async fn sha256(path: &Path) -> anyhow::Result<String> {
    let program = require(&["sha256sum", "shasum"], "to verify downloads")?;
    let mut args: Vec<&OsStr> = Vec::new();
    if program.file_name() == Some(OsStr::new("shasum")) {
        args.extend(["-a", "256"].map(OsStr::new));
    }
    args.push(path.as_os_str());
    let output = run(&program, &args, LOCAL_TOOL_TIMEOUT).await?;
    parse_checksum(&String::from_utf8_lossy(&output.stdout))
        .with_context(|| format!("reading the SHA-256 of {}", path.display()))
}

/// The digest of a `<sha256>  <file>` line, as `sha256sum` writes it.
pub fn parse_checksum(text: &str) -> anyhow::Result<String> {
    let digest = text
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    let hex = digest.len() == SHA256_HEX_LEN && digest.bytes().all(|b| b.is_ascii_hexdigit());
    anyhow::ensure!(hex, "`{digest}` is not a SHA-256 digest");
    Ok(digest)
}

/// Extract `members` of the gzipped tar `archive` into `into`: nothing else
/// in the archive is written, and each must be a regular file.
pub async fn unpack(archive: &Path, into: &Path, members: &[&str]) -> anyhow::Result<()> {
    assert!(!members.is_empty(), "a release ships at least one binary");
    assert!(into.is_dir(), "{} is not a directory", into.display());
    let program = require(&["tar"], "to unpack releases")?;
    let mut args: Vec<&OsStr> = vec![OsStr::new("-xzf"), archive.as_os_str()];
    args.extend([OsStr::new("-C"), into.as_os_str()]);
    args.extend(members.iter().map(OsStr::new));
    run(&program, &args, LOCAL_TOOL_TIMEOUT)
        .await
        .with_context(|| format!("unpacking {}", archive.display()))?;
    for member in members {
        let path = into.join(member);
        let metadata = std::fs::symlink_metadata(&path)
            .with_context(|| format!("the release has no `{member}`"))?;
        anyhow::ensure!(
            metadata.is_file(),
            "`{member}` in the release is not a regular file"
        );
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
            .with_context(|| format!("making {} executable", path.display()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checksum_lines_yield_their_digest_and_nothing_else_does() {
        let digest = "a".repeat(64);
        let line = format!(
            "{}  archon-x86_64-apple-darwin.tar.gz\n",
            digest.to_uppercase()
        );
        assert_eq!(parse_checksum(&line).unwrap(), digest);
        assert_eq!(parse_checksum(&digest).unwrap(), digest);
        for text in [
            "",
            "\n",
            "abc123",
            &"g".repeat(64),
            &"a".repeat(65),
            "<html>404</html>",
        ] {
            let error = parse_checksum(text).unwrap_err().to_string();
            assert!(error.contains("is not a SHA-256 digest"), "{text}: {error}");
        }
    }

    #[test]
    fn wget_refuses_non_https_for_https_addresses() {
        let limit = Duration::from_secs(30);
        let dest = Path::new("/tmp/out");
        let wget = download_args(false, "https://example.com/a.tar.gz", dest, limit);
        assert!(wget.iter().any(|a| a == "--https-only"), "{wget:?}");
        let local = download_args(false, "http://127.0.0.1:1/a", dest, limit);
        assert!(!local.iter().any(|a| a == "--https-only"), "{local:?}");
        let curl = download_args(true, "https://example.com/a.tar.gz", dest, limit);
        assert!(curl.iter().any(|a| a == "=https"), "{curl:?}");
    }

    #[tokio::test]
    async fn a_failing_program_is_an_error_with_its_stderr() {
        let sh = Path::new("/bin/sh");
        let args = [OsStr::new("-c"), OsStr::new("echo boom >&2; exit 3")];
        let error = run(sh, &args, LOCAL_TOOL_TIMEOUT).await.unwrap_err();
        let text = error.to_string();
        assert!(
            text.contains("boom") && text.contains("exit status: 3"),
            "{text}"
        );
        let slow = [OsStr::new("-c"), OsStr::new("sleep 30")];
        let error = run(sh, &slow, Duration::from_millis(50)).await.unwrap_err();
        assert!(error.to_string().contains("did not finish"), "{error}");
    }

    #[tokio::test]
    async fn the_digest_of_a_file_is_computed_by_the_system_tool() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("empty");
        std::fs::write(&path, b"").unwrap();
        assert_eq!(
            sha256(&path).await.unwrap(),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }
}
