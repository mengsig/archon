//! Reaching `archond` from the CLI. A command that only reads never starts it; one that changes
//! something, or opens the chat or the terminal UI, starts it and says so.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::Context as _;
use archon_client::ArchonClient;
use archon_core::config::{AetherMode, Config};
use archon_core::error::ArchonError;
use archon_core::protocol::{AetherLinkStatus, ClientKind, Reply, Request};

use crate::instance::lifecycle;
use crate::instance::{AETHER_PROGRAM, Instance};

/// How long a command waits for archond to connect to an aetherd that was
/// just started. archond asks for an absent one every second.
const LINK_WAIT: Duration = Duration::from_secs(5);
const LINK_POLL: Duration = Duration::from_millis(100);

/// This executable, as `archon daemon run --detach` and Aether's MCP
/// registration need it.
pub fn archon_bin() -> anyhow::Result<PathBuf> {
    let exe = std::env::current_exe().context("locating the archon executable")?;
    let bin = exe
        .canonicalize()
        .with_context(|| format!("resolving {}", exe.display()))?;
    assert!(bin.is_absolute(), "canonical paths are absolute");
    Ok(bin)
}

/// What a command that only reads says when no archond answers.
pub const NOT_RUNNING: &str = "archond is not running (start it with `archon daemon start`)";

/// Connect to the running archond for a command that only reads: it never
/// starts archond or Aether.
pub async fn connect_running() -> anyhow::Result<ArchonClient> {
    match connect_existing().await? {
        Some(client) => Ok(client),
        None => Err(ArchonError::Unavailable(not_running_text()).into()),
    }
}

/// [`NOT_RUNNING`], and where the config dir, data dir and socket mix moved
/// and default locations, why `archon daemon start` would refuse too.
pub fn not_running_text() -> String {
    let socket = archon_client::default_socket();
    match archon_core::paths::partial_override(&socket) {
        Some(mix) => format!("{NOT_RUNNING}. Note: {mix}"),
        None => NOT_RUNNING.to_owned(),
    }
}

/// Connect for a command that changes something or is interactive, starting archond when nothing
/// listens (Archon's own aetherd first in dedicated mode), unless the locations mix.
pub async fn connect_or_start() -> anyhow::Result<ArchonClient> {
    let socket = archon_client::default_socket();
    if let Some(mix) = archon_core::paths::partial_override(&socket) {
        return match connect_existing().await? {
            Some(client) => Ok(client),
            None => Err(ArchonError::Unavailable(format!(
                "archond is not running, and it is not started for you: {mix}. To start \
                 this mix on purpose, run `archon daemon run --detach`"
            ))
            .into()),
        };
    }
    let fresh_aether = ensure_aether().await?;
    let client = match connect_existing().await? {
        Some(client) => client,
        None => start(&socket).await?,
    };
    if fresh_aether {
        await_link(&client, LINK_WAIT).await?;
    }
    Ok(client)
}

/// Start archond on `socket`, saying so first: the operator learns which
/// data directory the new daemon owns.
async fn start(socket: &Path) -> anyhow::Result<ArchonClient> {
    assert!(
        archon_core::paths::partial_override(socket).is_none(),
        "a mix of moved and default locations is never started"
    );
    let bin = archon_bin()?;
    eprintln!(
        "archon: archond is not running; starting it (data {}, socket {})",
        archon_core::paths::data_dir().display(),
        socket.display()
    );
    let client = ArchonClient::connect_or_start(socket, ClientKind::ArchonCli, &bin).await?;
    assert!(client.welcome().pid > 0, "a started archond names its pid");
    Ok(client)
}

/// In dedicated mode, start Archon's own aetherd unless it answers, so the archond
/// started next finds it; `true` when it was (re)started. Shared mode never starts Aether.
async fn ensure_aether() -> anyhow::Result<bool> {
    let config = Config::load_default()?;
    if config.aether.mode != AetherMode::Dedicated {
        return Ok(false);
    }
    let instance = Instance::resolve()?;
    // Said before the restart, which takes seconds: the command is not stuck.
    if lifecycle::restart_is_due(&instance)? {
        eprintln!(
            "archon: an older `archon setup` registered Archon's tools without \
             `send_caller_meta = true`, which archond now needs to tell its orchestrator \
             from other agents. Updating the registration and restarting Archon's own \
             aetherd (socket {}) to load it; this command goes on once archond has \
             reconnected, in a few seconds.",
            instance.socket.display()
        );
    }
    let start = tokio::task::spawn_blocking(move || {
        lifecycle::ensure_running(&instance, Path::new(AETHER_PROGRAM))
    })
    .await
    .context("starting Archon's own aetherd")??;
    tracing::debug!(?start, "Archon's own aetherd answers");
    Ok(start.is_fresh())
}

/// Wait, for `limit` at most, until archond is connected to the aetherd just
/// started. A command that needs no Aether still runs and says what it did not wait out.
pub async fn await_link(client: &ArchonClient, limit: Duration) -> anyhow::Result<()> {
    assert!(LINK_POLL < LINK_WAIT, "the wait allows a second look");
    let deadline = Instant::now() + limit;
    loop {
        let status = match client.request(Request::Status(Default::default())).await? {
            Reply::Status(status) => status,
            other => return Err(unexpected(&other, "status")),
        };
        if matches!(status.aether, AetherLinkStatus::Connected { .. }) {
            return Ok(());
        }
        if Instant::now() >= deadline {
            eprintln!(
                "archon: archond has not connected to aetherd after {limit:?}; it keeps \
                 trying, and `archon status` shows when it has."
            );
            return Ok(());
        }
        tokio::time::sleep(LINK_POLL).await;
    }
}

/// Connect to a running daemon; `None` when none is listening.
pub async fn connect_existing() -> anyhow::Result<Option<ArchonClient>> {
    let socket = archon_client::default_socket();
    match ArchonClient::connect(&socket, ClientKind::ArchonCli).await {
        Ok(client) => Ok(Some(client)),
        Err(ArchonError::Unavailable(_)) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// The wire name of a reply kind, e.g. `items`.
pub fn reply_kind(reply: &Reply) -> String {
    serde_json::to_value(reply)
        .ok()
        .and_then(|v| v.get("reply").and_then(|k| k.as_str()).map(str::to_owned))
        .unwrap_or_else(|| "unknown".to_owned())
}

/// The error for a reply of the wrong kind: a daemon from another build.
pub fn unexpected(reply: &Reply, wanted: &str) -> anyhow::Error {
    anyhow::anyhow!(
        "archond answered with `{}` where `{wanted}` was expected; is it from another build? \
         Try `archon daemon stop --keep-aether`",
        reply_kind(reply)
    )
}

/// A reply's data as JSON, for `--json` output (`{"ok":true}` for `ok`).
pub fn reply_data(reply: &Reply) -> anyhow::Result<serde_json::Value> {
    let value = serde_json::to_value(reply).context("encoding the reply as JSON")?;
    Ok(value
        .get("data")
        .cloned()
        .unwrap_or_else(|| serde_json::json!({"ok": true})))
}

pub fn print_json(value: &serde_json::Value) -> anyhow::Result<()> {
    let text = serde_json::to_string_pretty(value).context("encoding JSON output")?;
    println!("{text}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use archon_core::protocol::{ClientFrame, PROTOCOL_VERSION, ServerFrame, Welcome};
    use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};
    use tokio::net::UnixListener;
    use tokio::task::JoinHandle;

    use super::*;

    fn link(connected: bool) -> AetherLinkStatus {
        match connected {
            true => AetherLinkStatus::Connected {
                daemon_version: "1.0.3".into(),
                protocol_version: 17,
                instance_id: "01TEST".into(),
            },
            false => AetherLinkStatus::Disconnected {
                error: "unavailable: not connected to aetherd".into(),
                retry_at: None,
            },
        }
    }

    /// A daemon whose link to Aether is down for its first `gaps` status
    /// replies. The task ends with the number of requests it answered.
    fn daemon_linking_after(listener: UnixListener, gaps: usize) -> JoinHandle<usize> {
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (read, mut write) = stream.into_split();
            let mut lines = BufReader::new(read).lines();
            let mut answered = 0;
            while let Some(line) = lines.next_line().await.unwrap() {
                let frame: ClientFrame = serde_json::from_str(&line).unwrap();
                let reply = match frame.request {
                    Request::Hello { .. } => Reply::Welcome(Welcome {
                        protocol_version: PROTOCOL_VERSION,
                        daemon_version: "test".into(),
                        pid: 1,
                    }),
                    Request::Status(_) => {
                        answered += 1;
                        let status = crate::format::tests::status_with(link(answered > gaps));
                        Reply::Status(Box::new(status))
                    }
                    other => panic!("unexpected request {other:?}"),
                };
                let reply = ServerFrame::Reply {
                    id: frame.id,
                    result: Ok(reply),
                };
                let line = format!("{}\n", serde_json::to_string(&reply).unwrap());
                write.write_all(line.as_bytes()).await.unwrap();
            }
            answered
        })
    }

    /// After a restart of Archon's aetherd the command goes on only once
    /// archond is connected to it, so it does not report the gap.
    #[tokio::test]
    async fn a_command_waits_until_archond_is_connected_to_the_new_aetherd() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("archond.sock");
        let daemon = daemon_linking_after(UnixListener::bind(&socket).unwrap(), 3);
        let client = ArchonClient::connect(&socket, ClientKind::ArchonCli)
            .await
            .unwrap();
        await_link(&client, LINK_WAIT).await.unwrap();
        drop(client);
        assert_eq!(daemon.await.unwrap(), 4, "three gaps, then connected");
    }

    /// An archond that never connects holds no command for longer than the limit.
    #[tokio::test]
    async fn the_wait_for_archonds_link_is_bounded() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("archond.sock");
        let daemon = daemon_linking_after(UnixListener::bind(&socket).unwrap(), usize::MAX);
        let client = ArchonClient::connect(&socket, ClientKind::ArchonCli)
            .await
            .unwrap();
        let limit = LINK_POLL * 3;
        let began = Instant::now();
        await_link(&client, limit).await.unwrap();
        assert!(began.elapsed() >= limit, "it waited the limit out");
        assert!(began.elapsed() < LINK_WAIT, "and no longer");
        drop(client);
        assert!(daemon.await.unwrap() >= 2, "it looked more than once");
    }

    #[test]
    fn reply_kinds_and_data_follow_the_wire_shape() {
        assert_eq!(reply_kind(&Reply::Ok), "ok");
        assert_eq!(
            reply_data(&Reply::Ok).unwrap(),
            serde_json::json!({"ok": true})
        );
        let repos = Reply::Repos(Vec::new());
        assert_eq!(reply_kind(&repos), "repos");
        assert_eq!(reply_data(&repos).unwrap(), serde_json::json!([]));
        assert!(unexpected(&repos, "items").to_string().contains("`repos`"));
    }
}
