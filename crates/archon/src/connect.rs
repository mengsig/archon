//! Reaching `archond` from the CLI: autostarting for ordinary commands,
//! connect-only where starting a daemon would be surprising.

use std::path::{Path, PathBuf};

use anyhow::Context as _;
use archon_client::ArchonClient;
use archon_core::config::{AetherMode, Config};
use archon_core::error::ArchonError;
use archon_core::protocol::{ClientKind, Reply};

use crate::instance::lifecycle::{self, Start};
use crate::instance::{AETHER_PROGRAM, Instance};

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

/// Connect, starting the daemon (and in dedicated mode Archon's own
/// aetherd first) when nothing listens.
pub async fn connect() -> anyhow::Result<ArchonClient> {
    ensure_aether().await?;
    let socket = archon_client::default_socket();
    let bin = archon_bin()?;
    ArchonClient::connect_or_start(&socket, ClientKind::ArchonCli, &bin)
        .await
        .map_err(anyhow::Error::from)
}

/// In dedicated mode, start Archon's own aetherd unless it answers, so the
/// archond started next finds it. Shared mode never starts Aether.
pub async fn ensure_aether() -> anyhow::Result<()> {
    let config = Config::load_default()?;
    if config.aether.mode != AetherMode::Dedicated {
        return Ok(());
    }
    let instance = Instance::resolve()?;
    let socket = instance.socket.clone();
    let start = tokio::task::spawn_blocking(move || {
        lifecycle::ensure_running(&instance, Path::new(AETHER_PROGRAM))
    })
    .await
    .context("starting Archon's own aetherd")??;
    match start {
        Start::AlreadyRunning => {}
        Start::Started => {
            tracing::info!(socket = %socket.display(), "started Archon's own aetherd");
        }
        Start::Restarted => eprintln!(
            "archon: an older `archon setup` registered Archon's tools without \
             `send_caller_meta = true`, which archond now needs to tell its orchestrator \
             from other agents. The registration was updated and Archon's own aetherd \
             (socket {}) restarted to load it.",
            socket.display()
        ),
    }
    Ok(())
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
         Try `archon daemon stop`",
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
    use super::*;

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
