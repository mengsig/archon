//! Bind a preview to a declared service in this run's worktree. Inspect the
//! server side of the actual connection before the proxy sends request bytes.

use std::collections::BTreeSet;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use archon_core::error::ArchonError;
use archon_design_browser::{BrowserError, UpstreamGuard};
use tokio::net::TcpStream;

/// How long a connection may wait for the server to accept it before it is refused.
const ACCEPT_WAIT: Duration = Duration::from_secs(3);
/// Looks at an accepted socket's owner, a few ms apart, before it is refused.
const OWNERSHIP_CHECKS: u32 = 5;

#[derive(Debug)]
struct Service {
    worktree: PathBuf,
    leaders: BTreeSet<i32>,
}

pub(super) async fn verify(
    worktree: &Path,
    url: &str,
) -> Result<Arc<dyn UpstreamGuard>, ArchonError> {
    if !cfg!(target_os = "linux") {
        return Err(ArchonError::Unavailable(
            "live design requires Linux to verify the process serving the preview".into(),
        ));
    }
    let root = worktree.to_owned();
    let service = tokio::task::spawn_blocking(move || {
        let worktree = root.canonicalize().map_err(|e| ArchonError::io("resolve preview worktree", e))?;
        let leaders = crate::reaper::services::declared(&worktree)?;
        if leaders.is_empty() {
            return Err(ArchonError::Invalid("start the dev server in this worktree and record its PID in .archon/services/<name>.pid".into()));
        }
        Ok(Service { worktree, leaders })
    }).await.map_err(|e| ArchonError::Unavailable(format!("inspect preview service: {e}")))??;
    let addresses = endpoints(url)?;
    let probe = tokio::time::timeout(
        Duration::from_secs(2),
        TcpStream::connect(addresses.as_slice()),
    )
    .await
    .map_err(|_| ArchonError::Unavailable("connecting to the preview timed out".into()))?
    .map_err(|e| ArchonError::io("connect to the preview server", e))?;
    service
        .verify_peer(&probe)
        .await
        .map_err(|e| ArchonError::Invalid(e.to_string()))?;
    Ok(Arc::new(service))
}

pub(super) fn endpoints(url: &str) -> Result<Vec<SocketAddr>, ArchonError> {
    archon_core::design::validate_preview_url(url)?;
    let authority = url["http://".len()..]
        .split(['/', '?', '#'])
        .next()
        .unwrap_or_default();
    let (host, port) = match authority.rsplit_once(':') {
        Some((host, port)) if !port.contains(']') => (host, port.parse::<u16>().ok()),
        _ => (authority, Some(80)),
    };
    let port = port
        .filter(|p| *p != 0)
        .ok_or_else(|| ArchonError::Invalid("preview requires a nonzero TCP port".into()))?;
    let v4 = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    let v6 = SocketAddr::from((Ipv6Addr::LOCALHOST, port));
    match host {
        "127.0.0.1" => Ok(vec![v4]),
        "[::1]" => Ok(vec![v6]),
        "localhost" => Ok(vec![v4, v6]),
        _ => Err(ArchonError::Invalid(
            "preview requires a loopback host".into(),
        )),
    }
}

#[async_trait::async_trait]
impl UpstreamGuard for Service {
    async fn verify_peer(&self, peer: &TcpStream) -> Result<(), BrowserError> {
        let address =
            |e: std::io::Error| BrowserError::Protocol(format!("inspect preview connection: {e}"));
        let client = peer.local_addr().map_err(address)?;
        let server = peer.peer_addr().map_err(address)?;
        if !client.ip().is_loopback() || !server.ip().is_loopback() {
            return Err(BrowserError::Protocol(
                "preview connection is not on loopback".into(),
            ));
        }
        let root = self.worktree.clone();
        let leaders = self.leaders.clone();
        tokio::task::spawn_blocking(move || {
            // The kernel exposes the peer's inode only once the server accepts, which a dev
            // server busy compiling its first page can delay by seconds.
            let deadline = std::time::Instant::now() + ACCEPT_WAIT;
            let mut ownership_checks = 0;
            while ownership_checks < OWNERSHIP_CHECKS {
                match peer_inode(server, client).map_err(address)? {
                    Some(inode) => {
                        if owns_socket(&root, &leaders, inode).map_err(address)? {
                            return Ok(());
                        }
                        ownership_checks += 1;
                    }
                    None if std::time::Instant::now() >= deadline => break,
                    None => {}
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            debug_assert!(ownership_checks <= OWNERSHIP_CHECKS, "the ownership retries are bounded");
            Err(BrowserError::Protocol("the process serving this port must run inside the item's worktree under its declared service PID or process group; a port a container publishes does not count, so serve the page from a host process".into()))
        }).await.map_err(|e| BrowserError::Protocol(format!("verify preview connection: {e}")))?
    }
}

fn peer_inode(server: SocketAddr, client: SocketAddr) -> std::io::Result<Option<u64>> {
    for (table, v6) in [("/proc/net/tcp", false), ("/proc/net/tcp6", true)] {
        let Some(local) = proc_address(server, v6) else {
            continue;
        };
        let Some(remote) = proc_address(client, v6) else {
            continue;
        };
        let rows = match std::fs::read_to_string(table) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            rows => rows?,
        };
        for row in rows.lines().skip(1) {
            let mut fields = row.split_whitespace().skip(1);
            if fields.next() == Some(&local)
                && fields.next() == Some(&remote)
                && fields.next() == Some("01")
            {
                return Ok(fields
                    .nth(5)
                    .and_then(|i| i.parse().ok())
                    .filter(|i| *i > 0));
            }
        }
    }
    Ok(None)
}

fn proc_address(address: SocketAddr, v6: bool) -> Option<String> {
    use std::fmt::Write;
    let bytes = match (address.ip(), v6) {
        (IpAddr::V4(ip), false) => {
            return Some(format!(
                "{:08X}:{:04X}",
                u32::from_ne_bytes(ip.octets()),
                address.port()
            ));
        }
        (IpAddr::V4(ip), true) => ip.to_ipv6_mapped().octets(),
        (IpAddr::V6(ip), true) => ip.octets(),
        (IpAddr::V6(_), false) => return None,
    };
    let mut key = String::with_capacity(37);
    for chunk in bytes.as_chunks::<4>().0 {
        write!(key, "{:08X}", u32::from_ne_bytes(*chunk)).expect("writing to String");
    }
    write!(key, ":{:04X}", address.port()).expect("writing to String");
    Some(key)
}

fn owns_socket(root: &Path, leaders: &BTreeSet<i32>, inode: u64) -> std::io::Result<bool> {
    let socket = PathBuf::from(format!("socket:[{inode}]"));
    for pid in leaders {
        if process_owns(root, leaders, *pid, &socket) {
            return Ok(true);
        }
    }
    for process in std::fs::read_dir("/proc")? {
        let process = process?;
        let Some(pid) = process
            .file_name()
            .to_str()
            .and_then(|s| s.parse::<i32>().ok())
        else {
            continue;
        };
        if !leaders.contains(&pid) && process_owns(root, leaders, pid, &socket) {
            return Ok(true);
        }
    }
    Ok(false)
}

fn process_owns(root: &Path, leaders: &BTreeSet<i32>, pid: i32, socket: &Path) -> bool {
    let dir = PathBuf::from(format!("/proc/{pid}"));
    if !std::fs::read_link(dir.join("cwd")).is_ok_and(|cwd| cwd.starts_with(root)) {
        return false;
    }
    if !leaders.contains(&pid) {
        let Ok(stat) = std::fs::read_to_string(dir.join("stat")) else {
            return false;
        };
        let group = stat
            .rsplit_once(')')
            .and_then(|(_, fields)| fields.split_whitespace().nth(2))
            .and_then(|field| field.parse::<i32>().ok());
        if !group.is_some_and(|id| leaders.contains(&id)) {
            return false;
        }
    }
    let Ok(fds) = std::fs::read_dir(dir.join("fd")) else {
        return false;
    };
    fds.filter_map(Result::ok)
        .any(|fd| std::fs::read_link(fd.path()).is_ok_and(|target| target == socket))
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    #[tokio::test]
    async fn verification_requires_both_worktree_and_server_ownership() {
        tokio::time::timeout(Duration::from_secs(4), async {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let (client, accepted) = tokio::join!(
                TcpStream::connect(listener.local_addr().unwrap()),
                listener.accept()
            );
            let client = client.unwrap();
            let _accepted = accepted.unwrap();
            let leaders = BTreeSet::from([i32::try_from(std::process::id()).unwrap()]);
            let cwd = std::env::current_dir().unwrap().canonicalize().unwrap();
            let owned = Service {
                worktree: cwd.clone(),
                leaders: leaders.clone(),
            };
            assert!(owned.verify_peer(&client).await.is_ok());
            let other = tempfile::tempdir().unwrap();
            let wrong_dir = Service {
                worktree: other.path().to_owned(),
                leaders,
            };
            assert!(wrong_dir.verify_peer(&client).await.is_err());
            let undeclared = Service {
                worktree: cwd,
                leaders: BTreeSet::new(),
            };
            assert!(undeclared.verify_peer(&client).await.is_err());
        })
        .await
        .expect("bounded socket ownership check");
    }

    /// S2: Vite accepts late while it compiles its first page; the preview's
    /// own connection a moment after `design_preview` was refused, so the
    /// screen stayed on "Starting" until a reload. A late accept still passes.
    #[tokio::test]
    async fn a_server_that_accepts_late_still_owns_its_connection() {
        let outcome = tokio::time::timeout(Duration::from_secs(4), async {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let client = TcpStream::connect(listener.local_addr().unwrap())
                .await
                .unwrap();
            let late = tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(400)).await;
                listener.accept().await.unwrap()
            });
            let cwd = std::env::current_dir().unwrap().canonicalize().unwrap();
            let service = Service {
                worktree: cwd,
                leaders: BTreeSet::from([i32::try_from(std::process::id()).unwrap()]),
            };
            let started = std::time::Instant::now();
            let verified = service.verify_peer(&client).await;
            assert!(
                started.elapsed() >= Duration::from_millis(300),
                "the check waited for the accept"
            );
            let _accepted = late.await.unwrap();
            verified
        })
        .await;
        outcome
            .expect("bounded socket ownership check")
            .expect("the late accept is the worktree's");
    }

    /// A port a container publishes belongs to docker-proxy, a process outside
    /// the worktree; a declared service running there does not make it the page's.
    #[tokio::test]
    async fn a_port_owned_by_a_process_outside_the_worktree_is_refused() {
        let worktree = tempfile::tempdir().unwrap();
        let mut declared = std::process::Command::new("sleep")
            .arg("30")
            .current_dir(worktree.path())
            .spawn()
            .expect("start a declared worktree service");
        let outcome = tokio::time::timeout(Duration::from_secs(4), async {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let (client, accepted) = tokio::join!(
                TcpStream::connect(listener.local_addr().unwrap()),
                listener.accept()
            );
            let client = client.unwrap();
            let _accepted = accepted.unwrap();
            let cwd = std::env::current_dir().unwrap().canonicalize().unwrap();
            let root = worktree.path().canonicalize().unwrap();
            assert!(!cwd.starts_with(&root), "the socket owner runs elsewhere");
            let service = Service {
                worktree: root,
                leaders: BTreeSet::from([i32::try_from(declared.id()).unwrap()]),
            };
            service.verify_peer(&client).await
        })
        .await;
        declared.kill().expect("stop the declared service");
        declared.wait().expect("reap the declared service");
        let refused = outcome.expect("bounded socket ownership check");
        let error = refused
            .expect_err("the outside owner is refused")
            .to_string();
        assert!(
            error.contains("a port a container publishes does not count"),
            "{error}"
        );
    }

    #[test]
    fn localhost_resolves_only_to_numeric_loopback() {
        assert_eq!(
            endpoints("http://localhost:5173/").unwrap(),
            [
                SocketAddr::from((Ipv4Addr::LOCALHOST, 5173)),
                SocketAddr::from((Ipv6Addr::LOCALHOST, 5173))
            ]
        );
        assert_eq!(
            endpoints("http://[::1]").unwrap(),
            [SocketAddr::from((Ipv6Addr::LOCALHOST, 80))]
        );
        assert!(endpoints("http://localhost:0").is_err());
    }
}
