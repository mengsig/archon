//! Finding, launching and reaping the Chromium child.

use std::fs;
use std::path::{Path, PathBuf};

use crate::BrowserError;
use crate::Viewport;

/// Everything needed to start one browser.
#[derive(Debug, Clone)]
pub struct LaunchSpec {
    /// The Chromium binary.
    pub binary: PathBuf,
    /// Fresh profile directory (created mode 0700 by the launcher).
    pub profile_dir: PathBuf,
    /// Keep Chromium's sandbox on (`--no-sandbox` only when false).
    pub sandbox: bool,
    /// Hosts the repository allows beyond loopback.
    pub allow_hosts: Vec<String>,
    /// Initial window size.
    pub viewport: Viewport,
    /// `PATH` for the child.
    pub path_env: String,
    /// `LANG` for the child.
    pub lang: String,
    /// `TZ` for the child.
    pub tz: String,
}

/// Shell wrapper: Chromium reads CDP on fd 3 and writes on fd 4, with no stdio of its own.
pub(crate) const SHELL_SCRIPT: &str = r#"exec 3<&0 4>&1 </dev/null >/dev/null; exec "$0" "$@""#;

const BASE_BYPASS: &str = "<-loopback>;localhost;127.0.0.1;[::1]";

fn valid_host(h: &str) -> bool {
    let h = h.strip_prefix("*.").unwrap_or(h);
    !h.is_empty()
        && h.len() <= 253
        && h.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-')
}

/// The `--proxy-bypass-list` value: loopback plus each valid allowed host.
pub fn proxy_bypass_list(allow_hosts: &[String]) -> String {
    let mut s = BASE_BYPASS.to_string();
    for h in allow_hosts {
        let h = h.trim();
        if valid_host(h) {
            s.push(';');
            s.push_str(h);
        }
    }
    s
}

/// Chromium's command-line arguments (without the binary).
pub fn launch_args(spec: &LaunchSpec) -> Vec<String> {
    let mut a: Vec<String> = vec![
        "--headless=new".into(),
        "--remote-debugging-pipe".into(),
        format!("--user-data-dir={}", spec.profile_dir.display()),
        "--no-first-run".into(),
        "--no-default-browser-check".into(),
        "--disable-extensions".into(),
        "--disable-sync".into(),
        "--disable-background-networking".into(),
        "--disable-component-update".into(),
        "--mute-audio".into(),
        "--js-flags=--max-old-space-size=1024".into(),
        "--proxy-server=http://127.0.0.1:9".into(),
        format!(
            "--proxy-bypass-list={}",
            proxy_bypass_list(&spec.allow_hosts)
        ),
        format!(
            "--window-size={},{}",
            spec.viewport.width, spec.viewport.height
        ),
    ];
    if !spec.sandbox {
        a.push("--no-sandbox".into());
    }
    a.push("about:blank".into());
    a
}

/// The child's whole environment.
pub fn launch_env(spec: &LaunchSpec) -> Vec<(String, String)> {
    vec![
        ("HOME".into(), spec.profile_dir.display().to_string()),
        ("PATH".into(), spec.path_env.clone()),
        ("LANG".into(), spec.lang.clone()),
        ("TZ".into(), spec.tz.clone()),
    ]
}

fn not_found_hint() -> String {
    "install Chromium (`sudo apt install chromium` or google-chrome-stable), or run \
     `npx playwright install chromium-headless-shell`, or set [design] browser = \"/path/to/chrome\""
        .into()
}

/// The sandbox fix shown to the operator.
pub(crate) fn sandbox_hint() -> String {
    "Chromium's sandbox is unavailable on this host. Allow unprivileged user namespaces \
     (`sudo sysctl -w kernel.unprivileged_userns_clone=1`; on Ubuntu 24.04+ \
     `sudo sysctl -w kernel.apparmor_restrict_unprivileged_userns=0`), or set \
     browser_sandbox = \"off\" in the host config to run an unsandboxed preview"
        .into()
}

fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    fs::metadata(p)
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

/// Locate the browser: configured, else PATH, else Playwright's newest headless shell.
pub fn find_browser(
    configured: Option<&Path>,
    path_env: &str,
    home: &Path,
) -> Result<PathBuf, BrowserError> {
    if let Some(c) = configured {
        return if is_executable(c) {
            Ok(c.to_path_buf())
        } else {
            Err(BrowserError::NotFound {
                hint: format!(
                    "configured browser {} is not executable; {}",
                    c.display(),
                    not_found_hint()
                ),
            })
        };
    }
    for name in ["google-chrome", "chromium", "chromium-browser"] {
        for dir in path_env.split(':').filter(|d| !d.is_empty()) {
            let p = Path::new(dir).join(name);
            if is_executable(&p) {
                return Ok(p);
            }
        }
    }
    let pw = home.join(".cache/ms-playwright");
    let mut dirs: Vec<(u64, PathBuf)> = fs::read_dir(&pw)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            let rev = name
                .strip_prefix("chromium_headless_shell-")?
                .parse()
                .ok()?;
            Some((rev, e.path()))
        })
        .collect();
    dirs.sort_by_key(|a| std::cmp::Reverse(a.0));
    for (_, d) in dirs {
        let mut subs: Vec<PathBuf> = fs::read_dir(&d)
            .into_iter()
            .flatten()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().starts_with("chrome-"))
            .map(|e| e.path())
            .collect();
        subs.sort();
        for s in subs {
            let bin = s.join("chrome-headless-shell");
            if is_executable(&bin) {
                return Ok(bin);
            }
        }
    }
    Err(BrowserError::NotFound {
        hint: not_found_hint(),
    })
}

/// One line for `archon doctor`.
pub fn doctor_line(configured: Option<&Path>, path_env: &str, home: &Path) -> String {
    match find_browser(configured, path_env, home) {
        Ok(p) => format!("design preview browser: {}", p.display()),
        Err(BrowserError::NotFound { hint }) => {
            format!("design preview browser: not found; {hint}")
        }
        Err(e) => format!("design preview browser: {e}"),
    }
}

/// Start time (clock ticks since boot) of `pid`: `/proc/<pid>/stat` field 22.
pub fn process_start_time(pid: u32) -> Option<u64> {
    let s = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    parse_start_time(&s)
}

pub(crate) fn parse_start_time(stat: &str) -> Option<u64> {
    let rest = &stat[stat.rfind(')')? + 1..];
    // After the comm field the first token is field 3 (state); field 22 is index 19.
    rest.split_whitespace().nth(19)?.parse().ok()
}

pub(crate) fn kill_group(pid: u32) -> bool {
    let Some(p) = i32::try_from(pid)
        .ok()
        .and_then(rustix::process::Pid::from_raw)
    else {
        return false;
    };
    rustix::process::kill_process_group(p, rustix::process::Signal::KILL).is_ok()
}

/// Kill the process group led by `pid`, only if its start time still equals `started_at`.
pub fn kill_if_matches(pid: u32, started_at: u64) -> bool {
    if process_start_time(pid) != Some(started_at) {
        return false;
    }
    kill_group(pid)
}

/// Remove every directory under `root` that is not in `keep`; returns how many were removed.
pub fn sweep_profiles(root: &Path, keep: &[PathBuf]) -> std::io::Result<usize> {
    let rd = match fs::read_dir(root) {
        Ok(r) => r,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(e),
    };
    let mut n = 0;
    for e in rd {
        let p = e?.path();
        if keep.iter().any(|k| k == &p) {
            continue;
        }
        if p.is_dir() {
            fs::remove_dir_all(&p)?;
            n += 1;
        }
    }
    Ok(n)
}

/// Classify Chromium's stderr: a sandbox failure becomes [`BrowserError::SandboxUnavailable`].
pub(crate) fn classify_stderr(stderr: &str) -> Option<BrowserError> {
    let l = stderr.to_ascii_lowercase();
    let hit = l.contains("no usable sandbox")
        || l.contains("setuid sandbox")
        || l.contains("failed to move to new namespace")
        || l.contains("sandbox_host_linux")
        || (l.contains("sandbox") && l.contains("operation not permitted"))
        || (l.contains("namespace") && l.contains("sandbox"));
    hit.then(|| BrowserError::SandboxUnavailable {
        hint: sandbox_hint(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(sandbox: bool, hosts: &[&str]) -> LaunchSpec {
        LaunchSpec {
            binary: "/usr/bin/chromium".into(),
            profile_dir: "/tmp/p/ds_1".into(),
            sandbox,
            allow_hosts: hosts.iter().map(|s| s.to_string()).collect(),
            viewport: Viewport {
                width: 1440,
                height: 900,
                touch: false,
                dpr: 1.0,
            },
            path_env: "/usr/bin:/bin".into(),
            lang: "C.UTF-8".into(),
            tz: "UTC".into(),
        }
    }

    // T-U7
    #[test]
    fn launch_line_and_env() {
        let on = launch_args(&spec(true, &[]));
        assert!(!on.iter().any(|a| a == "--no-sandbox"));
        assert_eq!(on.last().unwrap(), "about:blank");
        for want in [
            "--headless=new",
            "--remote-debugging-pipe",
            "--user-data-dir=/tmp/p/ds_1",
            "--proxy-server=http://127.0.0.1:9",
            "--js-flags=--max-old-space-size=1024",
            "--window-size=1440,900",
            "--proxy-bypass-list=<-loopback>;localhost;127.0.0.1;[::1]",
        ] {
            assert!(on.iter().any(|a| a == want), "missing {want}");
        }
        let off = launch_args(&spec(false, &[]));
        assert!(off.iter().any(|a| a == "--no-sandbox"));

        let env = launch_env(&spec(true, &[]));
        let keys: Vec<_> = env.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(keys, ["HOME", "PATH", "LANG", "TZ"]);
        assert!(env.iter().all(|(k, _)| {
            !k.starts_with("ARCHON_") && !k.starts_with("AETHER_") && !k.starts_with("GIT_")
        }));
        assert_eq!(env[0].1, "/tmp/p/ds_1");
    }

    #[test]
    fn bypass_list_keeps_only_valid_hosts() {
        let l = proxy_bypass_list(&[
            "dev.example.com".into(),
            "*.cdn.test".into(),
            "bad;host".into(),
            "".into(),
            "a b".into(),
        ]);
        assert_eq!(
            l,
            "<-loopback>;localhost;127.0.0.1;[::1];dev.example.com;*.cdn.test"
        );
    }

    #[test]
    fn start_time_field_22() {
        let stat = "123 (we ird) name) S 1 123 123 0 -1 4194560 100 0 0 0 1 2 0 0 20 0 1 0 987654 1000 100 ";
        assert_eq!(parse_start_time(stat), Some(987654));
        assert!(process_start_time(std::process::id()).is_some());
    }

    #[test]
    fn kill_mismatch_does_nothing() {
        let mut c = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let t = process_start_time(c.id()).unwrap();
        assert!(!kill_if_matches(c.id(), t + 1));
        assert!(c.try_wait().unwrap().is_none());
        c.kill().unwrap();
        c.wait().unwrap();
    }

    #[test]
    fn sweep_keeps_listed() {
        let d = tempfile::tempdir().unwrap();
        for n in ["a", "b", "c"] {
            fs::create_dir(d.path().join(n)).unwrap();
        }
        let n = sweep_profiles(d.path(), &[d.path().join("b")]).unwrap();
        assert_eq!(n, 2);
        assert!(d.path().join("b").exists() && !d.path().join("a").exists());
        assert_eq!(sweep_profiles(&d.path().join("nope"), &[]).unwrap(), 0);
    }

    #[test]
    fn stderr_sandbox_detected() {
        assert!(matches!(
            classify_stderr("FATAL:zygote_host_impl_linux.cc(128)] No usable sandbox!"),
            Some(BrowserError::SandboxUnavailable { .. })
        ));
        assert!(classify_stderr("some warning").is_none());
    }

    #[test]
    fn find_prefers_configured_then_path_then_playwright() {
        let d = tempfile::tempdir().unwrap();
        let mk = |p: &Path| {
            use std::os::unix::fs::PermissionsExt;
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(p, "#!/bin/sh\n").unwrap();
            fs::set_permissions(p, fs::Permissions::from_mode(0o755)).unwrap();
        };
        let home = d.path().join("home");
        mk(&home.join(
            ".cache/ms-playwright/chromium_headless_shell-1243/chrome-linux/chrome-headless-shell",
        ));
        mk(&home.join(
            ".cache/ms-playwright/chromium_headless_shell-1255/chrome-linux/chrome-headless-shell",
        ));
        let got = find_browser(None, "", &home).unwrap();
        assert!(got.to_string_lossy().contains("1255"));
        let bin = d.path().join("bin/chromium");
        mk(&bin);
        let got = find_browser(None, &d.path().join("bin").display().to_string(), &home).unwrap();
        assert_eq!(got, bin);
        assert_eq!(find_browser(Some(&bin), "", &home).unwrap(), bin);
        assert!(matches!(
            find_browser(None, "", d.path()),
            Err(BrowserError::NotFound { .. })
        ));
        assert!(doctor_line(None, "", d.path()).contains("not found"));
    }
}
