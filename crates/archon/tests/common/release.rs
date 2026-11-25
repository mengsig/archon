//! A fake release on a local HTTP server, and stand-ins for the programs the
//! installer and `archon update` run, so no test reaches github.com.
#![allow(dead_code, reason = "each test binary uses a different subset")]

use std::io::{BufRead as _, BufReader, Write as _};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use archon_core::testing::write_script;

/// Serves the files under one directory over HTTP until dropped.
pub struct Server {
    address: SocketAddr,
    requests: Arc<Mutex<Vec<String>>>,
    stalled: Arc<Mutex<Option<String>>>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Server {
    pub fn serve(root: &Path) -> Self {
        assert!(root.is_dir(), "{} is not a directory", root.display());
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let stalled = Arc::new(Mutex::new(None::<String>));
        let (root, seen, stopping) = (root.to_path_buf(), requests.clone(), stop.clone());
        let stalling = stalled.clone();
        let thread = std::thread::spawn(move || {
            // Stalled downloads stay open until the server stops.
            let mut held = Vec::new();
            for stream in listener.incoming() {
                if stopping.load(Ordering::SeqCst) {
                    return;
                }
                let stall = stalling.lock().unwrap().clone();
                held.extend(answer(stream.unwrap(), &root, &seen, stall.as_deref()));
            }
        });
        Self {
            address,
            requests,
            stalled,
            stop,
            thread: Some(thread),
        }
    }

    /// The address without a trailing slash, e.g. `http://127.0.0.1:4321`.
    pub fn url(&self) -> String {
        format!("http://{}", self.address)
    }

    /// From now on a file whose path ends with `suffix` is sent only in
    /// part, and its connection stays open: a download that never ends.
    pub fn stall(&self, suffix: &str) {
        assert!(!suffix.is_empty(), "an empty suffix stalls every request");
        *self.stalled.lock().unwrap() = Some(suffix.to_owned());
    }

    /// The path of every request so far, in order.
    pub fn requests(&self) -> Vec<String> {
        self.requests.lock().unwrap().clone()
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        // Wakes the accept loop so it sees the flag.
        let _woken = TcpStream::connect(self.address);
        if let Some(thread) = self.thread.take() {
            thread.join().unwrap();
        }
    }
}

/// Answer one GET with the file it names, or 404. A proxy-style request
/// (`GET http://host/path`) is answered by its path. Returns the connection
/// when the file is one to `stall` on: the caller keeps it open.
fn answer(
    mut stream: TcpStream,
    root: &Path,
    seen: &Mutex<Vec<String>>,
    stall: Option<&str>,
) -> Option<TcpStream> {
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let mut lines = BufReader::new(stream.try_clone().unwrap()).lines();
    let request = lines.next()?.ok()?;
    for header in lines {
        if header.map_or(true, |line| line.is_empty()) {
            break;
        }
    }
    let target = request.split(' ').nth(1).unwrap_or_default();
    let path = match target.strip_prefix("http://") {
        Some(rest) => rest.find('/').map_or("/", |at| &rest[at..]),
        None => target,
    };
    seen.lock().unwrap().push(path.to_owned());
    let file = root.join(path.trim_start_matches('/'));
    let body = match path.contains("..") {
        true => None,
        false => std::fs::read(&file).ok(),
    };
    let (status, body) = match body {
        Some(body) => ("200 OK", body),
        None => ("404 Not Found", b"not found\n".to_vec()),
    };
    let head = format!(
        "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let stalls = status == "200 OK" && stall.is_some_and(|suffix| path.ends_with(suffix));
    let sent = match stalls {
        true => &body[..body.len() / 2],
        false => &body[..],
    };
    // A client that hung up early is its own test's failure, not this one's.
    let _sent = stream
        .write_all(head.as_bytes())
        .and_then(|()| stream.write_all(sent))
        .and_then(|()| stream.flush());
    stalls.then_some(stream)
}

/// A stand-in `archon` that reports `version` and speaks Aether protocol
/// 14 to 17, as a release binary answers the installer and the updater.
pub fn archon_script(version: &str) -> String {
    const SCRIPT: &str = r#"#!/bin/sh
case "$*" in
--version) echo 'archon VERSION' ;;
'--json update --requirements') echo '{"min":14,"max":17,"minimum_version":"0.9.0"}' ;;
'update --requirements') printf 'aether_protocol_min 14\naether_protocol_max 17\n' ;;
*) echo "fake archon VERSION: $*" ;;
esac
"#;
    SCRIPT.replace("VERSION", version)
}

/// A stand-in `aether` that serves protocols `min` to `max`, with the
/// top-level layout `aether schema` prints.
pub fn aether_script(min: u32, max: u32) -> String {
    const SCRIPT: &str = r#"#!/bin/sh
case "$1" in
--version) echo 'aether 0.9.0' ;;
schema) cat <<'JSON'
{
  "$defs": {
    "Hello": {
      "protocol_version": {
        "minimum": 0
      }
    }
  },
  "min_protocol_version": MIN,
  "protocol_version": MAX,
  "title": "Aether daemon protocol"
}
JSON
;;
*) exit 64 ;;
esac
"#;
    SCRIPT
        .replace("MIN", &min.to_string())
        .replace("MAX", &max.to_string())
}

/// The SHA-256 of `path` as hex, by whichever system tool exists.
pub fn sha256_hex(path: &Path) -> String {
    let output = Command::new("/bin/sh")
        .args([
            "-c",
            "sha256sum \"$1\" 2>/dev/null || shasum -a 256 \"$1\"",
            "sha256",
        ])
        .arg(path)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "hashing {}: {output:?}",
        path.display()
    );
    let text = String::from_utf8(output.stdout).unwrap();
    let digest = text.split_whitespace().next().unwrap().to_owned();
    assert_eq!(digest.len(), 64, "a SHA-256 is 64 hex digits: {text}");
    digest
}

/// What a release archive holds, in the order it is packed.
pub const ARCHIVE_MEMBERS: [&str; 3] = ["archon", "LICENSE", "THIRD-PARTY-NOTICES"];

const PACKAGE_SCRIPT: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../scripts/package-release.sh"
);

/// The names in a gzipped tar archive, in order.
pub fn archive_members(archive: &Path) -> Vec<String> {
    let listing = Command::new("tar").arg("-tzf").arg(archive).output();
    let listing = listing.unwrap();
    assert!(listing.status.success(), "listing {}", archive.display());
    let names = String::from_utf8(listing.stdout).unwrap();
    names.lines().map(str::to_owned).collect()
}

/// One member of a gzipped tar archive, as bytes.
pub fn archive_member(archive: &Path, member: &str) -> Vec<u8> {
    let extracted = Command::new("tar")
        .arg("-xzOf")
        .arg(archive)
        .arg(member)
        .output()
        .unwrap();
    assert!(extracted.status.success(), "no {member}: {extracted:?}");
    extracted.stdout
}

/// Publish the archive for `target`, packed as the release workflow packs
/// it, with its `.sha256` and `VERSION` under `download/<tag>/` (returned),
/// and under `latest/download/` when `latest`.
pub fn publish(root: &Path, tag: &str, target: &str, archon: &str, latest: bool) -> PathBuf {
    assert!(tag.starts_with('v'), "a release tag starts with v: {tag}");
    let build = tempfile::tempdir_in(root).unwrap();
    write_script(&build.path().join("archon"), archon);
    let name = format!("archon-{target}.tar.gz");
    let dir = root.join("download").join(tag);
    std::fs::create_dir_all(&dir).unwrap();
    let archive = dir.join(&name);
    let status = Command::new("sh")
        .arg(PACKAGE_SCRIPT)
        .arg(build.path().join("archon"))
        .arg(&archive)
        .status()
        .unwrap();
    assert!(status.success(), "packing {} failed", archive.display());
    assert_eq!(archive_members(&archive), ARCHIVE_MEMBERS);
    let line = format!("{}  {name}\n", sha256_hex(&archive));
    std::fs::write(dir.join(format!("{name}.sha256")), line).unwrap();
    std::fs::write(dir.join("VERSION"), format!("{tag}\n")).unwrap();
    if latest {
        let latest = root.join("latest/download");
        std::fs::create_dir_all(&latest).unwrap();
        for file in [name.clone(), format!("{name}.sha256"), "VERSION".to_owned()] {
            std::fs::copy(dir.join(&file), latest.join(&file)).unwrap();
        }
    }
    archive
}

/// Fill `dir` with links to `names` from PATH, so a PATH of it alone hides
/// every other program. Returns the names that are not installed.
pub fn tool_dir(dir: &Path, names: &[&str]) -> Vec<String> {
    std::fs::create_dir_all(dir).unwrap();
    let path = std::env::var_os("PATH").expect("PATH is set");
    let mut absent = Vec::new();
    for name in names {
        let found = std::env::split_paths(&path)
            .map(|dir| dir.join(name))
            .find(|candidate| candidate.is_file());
        match found {
            Some(program) => std::os::unix::fs::symlink(program, dir.join(name)).unwrap(),
            None => absent.push((*name).to_owned()),
        }
    }
    absent
}
