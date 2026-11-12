//! The Aether half of `archon remote setup`, driven only through Aether's
//! own command line (`aether remote …`): its listener, its public origins
//! and the `/archon` mount. Each call reads first and changes what differs.

use std::ffi::OsString;
use std::time::Duration;

use anyhow::{Context as _, bail};
use archon_core::config::{AetherMode, Config};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use super::runner::{Output, RunError, run_command};
use super::setup::Steps;

/// The Aether release whose listener mounts other daemons.
pub(super) const NEEDED_AETHER: &str = "1.3.0";
/// The prefix Archon is served under on Aether's listener.
pub(super) const PREFIX: &str = "/archon";
const CALL_TIMEOUT: Duration = Duration::from_secs(30);

/// What `aether remote status --json` says, as far as setup reads it.
#[derive(Debug, Clone, Deserialize)]
struct StatusReply {
    status: Status,
}

#[derive(Debug, Clone, Deserialize)]
pub(super) struct Status {
    pub(super) settings: Settings,
    /// `[remote] address` as Aether's config sets it.
    #[serde(default)]
    pub(super) address: String,
    /// The address Aether's listener is bound to; absent while it is closed.
    #[serde(default)]
    pub(super) listening: Option<String>,
    #[serde(default)]
    pub(super) error: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub(super) struct Settings {
    #[serde(default)]
    pub(super) enabled: bool,
    #[serde(default)]
    pub(super) pairing: bool,
    #[serde(default)]
    pub(super) public_origins: Vec<String>,
}

/// One prefix of Aether's listener forwarded to a loopback daemon.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct Mount {
    pub(super) prefix: String,
    pub(super) kind: String,
    pub(super) target: String,
}

/// The `aether` command line for the Aether that Archon's config names:
/// the operator's in shared mode, Archon's own in dedicated mode.
pub(super) struct Aether {
    config: Config,
}

impl Aether {
    pub(super) fn new(config: Config) -> anyhow::Result<Self> {
        if config.aether.mode == AetherMode::Shared && config.aether.address.is_some() {
            bail!(
                "[aether] address names Aether over TCP; setup drives Aether through its command \
                 line, which needs its Unix socket: set [aether] socket instead"
            );
        }
        Ok(Self { config })
    }

    /// The mounts Aether has; refused with the version needed when this
    /// Aether cannot mount at all.
    pub(super) async fn mounts(&self) -> anyhow::Result<Vec<Mount>> {
        let output = self.call(&["remote", "mount", "list", "--json"]).await?;
        if !output.success() {
            let said = format!("{}\n{}", output.stderr, output.stdout);
            if said.contains("unrecognized subcommand") || said.contains("unexpected argument") {
                bail!(
                    "{}",
                    too_old("its `aether remote mount` command is missing")
                );
            }
            bail!(
                "`aether remote mount list` failed: {}. Is aetherd running? Start it, then run \
                 this again",
                output.complaint()
            );
        }
        let mounts: Vec<Mount> = parse(&output, "aether remote mount list --json")?;
        assert!(mounts.len() <= 4, "Aether carries at most 4 mounts");
        Ok(mounts)
    }

    pub(super) async fn status(&self) -> anyhow::Result<Status> {
        let output = self.call(&["remote", "status", "--json"]).await?;
        anyhow::ensure!(
            output.success(),
            "`aether remote status` failed: {}",
            output.complaint()
        );
        let reply: StatusReply = parse(&output, "aether remote status --json")?;
        Ok(reply.status)
    }

    /// Open Aether's listener unless it is open; returns its status then.
    pub(super) async fn ensure_listening(&self, steps: &mut Steps) -> anyhow::Result<Status> {
        let status = self.status().await?;
        if let Some(listening) = &status.listening {
            steps.note(&format!(
                "Aether's listener is already open on {listening}."
            ));
            return Ok(status);
        }
        // `enable` sets pairing from its flag, so keep what the operator chose.
        let mut args = vec!["remote", "enable", "--json"];
        if !status.settings.pairing {
            args.push("--no-pairing");
        }
        let output = self.call(&args).await?;
        anyhow::ensure!(
            output.success(),
            "`aether remote enable` failed: {}",
            output.complaint()
        );
        let status = self.status().await?;
        let Some(listening) = &status.listening else {
            bail!(
                "Aether's listener did not open: {}",
                status.error.as_deref().unwrap_or("Aether gave no reason")
            );
        };
        steps.changed(&format!("Opened Aether's listener on {listening}."));
        Ok(status)
    }

    /// Make `origin` one of Aether's public origins, keeping the others and
    /// whether pairing is allowed; returns them all.
    pub(super) async fn ensure_origin(
        &self,
        status: &Status,
        origin: &str,
        steps: &mut Steps,
    ) -> anyhow::Result<Vec<String>> {
        let settings = &status.settings;
        assert!(settings.enabled, "the listener was opened first");
        if settings
            .public_origins
            .iter()
            .any(|o| same_origin(o, origin))
        {
            steps.note(&format!("Aether already answers browsers at {origin}."));
            return Ok(settings.public_origins.clone());
        }
        let mut origins = settings.public_origins.clone();
        origins.push(origin.to_owned());
        let mut args: Vec<String> = vec!["remote".into(), "enable".into(), "--json".into()];
        for each in &origins {
            args.extend(["--origin".to_owned(), each.clone()]);
        }
        if !settings.pairing {
            args.push("--no-pairing".into());
        }
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let output = self.call(&args).await?;
        anyhow::ensure!(
            output.success(),
            "`aether remote enable --origin {origin}` failed: {}",
            output.complaint()
        );
        let after = self.status().await?.settings.public_origins;
        anyhow::ensure!(
            after.iter().any(|o| same_origin(o, origin)),
            "Aether answered without {origin} among its origins"
        );
        steps.changed(&format!("Aether now answers browsers at {origin}."));
        Ok(after)
    }

    /// Mount `target` at [`PREFIX`] unless it is mounted there already.
    /// Aether checks Funnel itself and refuses with its own reason.
    pub(super) async fn ensure_mount(
        &self,
        mounts: &[Mount],
        target: &str,
        steps: &mut Steps,
    ) -> anyhow::Result<()> {
        assert!(
            target.starts_with("http://"),
            "archond's listener is plain HTTP"
        );
        if let Some(mount) = mounts.iter().find(|m| m.prefix == PREFIX) {
            if mount.target == target {
                steps.note(&format!("Aether already serves archond under {PREFIX}."));
                return Ok(());
            }
            bail!(
                "Aether already mounts {PREFIX} on {}, not on archond's listener {target}. If \
                 that is no other daemon, remove it with `aether remote mount remove {PREFIX}`, \
                 then run this again",
                mount.target
            );
        }
        let args = [
            "remote", "mount", "add", PREFIX, "--target", target, "--kind", "archon", "--json",
        ];
        let output = self.call(&args).await?;
        if !output.success() {
            bail!(
                "Aether refused to mount {PREFIX}: {}. (An aetherd older than {NEEDED_AETHER} \
                 cannot mount; restart it after an upgrade.)",
                output.complaint()
            );
        }
        let after = self.mounts().await?;
        anyhow::ensure!(
            after
                .iter()
                .any(|m| m.prefix == PREFIX && m.target == target),
            "Aether answered without {PREFIX} mounted on {target}"
        );
        steps.changed(&format!(
            "Aether now serves archond ({target}) under {PREFIX}."
        ));
        Ok(())
    }

    async fn call(&self, args: &[&str]) -> anyhow::Result<Output> {
        assert!(
            args.first() == Some(&"remote"),
            "setup asks only `aether remote`"
        );
        let words: Vec<OsString> = args.iter().map(OsString::from).collect();
        let mut command = super::super::instance::aether_command(&self.config, &words)?;
        if let (AetherMode::Shared, Some(socket)) =
            (self.config.aether.mode, &self.config.aether.socket)
        {
            command.env("AETHER_SOCKET", socket);
        }
        let shown = format!("aether {}", args.join(" "));
        match run_command(tokio::process::Command::from(command), CALL_TIMEOUT).await {
            Ok(output) => Ok(output),
            Err(RunError::NotFound) => bail!("{}", too_old("Aether is not installed")),
            Err(error) => Err(error).with_context(|| format!("running `{shown}`")),
        }
    }
}

/// Why setup cannot go on with this Aether, and what it needs.
fn too_old(why: &str) -> String {
    format!(
        "{why}. `archon remote setup` serves Archon through Aether's listener and needs \
         Aether {NEEDED_AETHER} or later; install or upgrade it \
         (https://github.com/mengsig/aether/releases), then run this again"
    )
}

fn parse<T: DeserializeOwned>(output: &Output, command: &str) -> anyhow::Result<T> {
    serde_json::from_str(&output.stdout)
        .with_context(|| format!("reading the output of `{command}`"))
}

/// Origins compared as browsers send them: case and a trailing `/` aside.
fn same_origin(a: &str, b: &str) -> bool {
    let normal = |o: &str| o.trim().trim_end_matches('/').to_ascii_lowercase();
    normal(a) == normal(b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_status_and_mounts_aether_prints_are_read() {
        let status = r#"{"status":{"revision":3,"settings":{"enabled":true,"pairing":false,
            "public_origins":["https://box.ts.net"],"push":false,
            "mounts":[{"prefix":"/archon","kind":"archon","target":"http://127.0.0.1:8378"}]},
            "address":"127.0.0.1:8377","listening":"127.0.0.1:8377"},"devices":[]}"#;
        let output = Output {
            code: Some(0),
            stdout: status.into(),
            stderr: String::new(),
        };
        let reply: StatusReply = parse(&output, "status").unwrap();
        assert_eq!(reply.status.listening.as_deref(), Some("127.0.0.1:8377"));
        assert_eq!(reply.status.settings.public_origins, ["https://box.ts.net"]);
        assert!(!reply.status.settings.pairing);
        let mounts = r#"[{"prefix":"/archon","kind":"archon","target":"http://127.0.0.1:8378"}]"#;
        let output = Output {
            stdout: mounts.into(),
            ..output
        };
        let mounts: Vec<Mount> = parse(&output, "mounts").unwrap();
        assert_eq!(mounts[0].prefix, PREFIX);
    }

    #[test]
    fn origins_match_as_browsers_send_them_and_too_old_names_the_version() {
        assert!(same_origin("https://Box.ts.net/", "https://box.ts.net"));
        assert!(!same_origin(
            "https://box.ts.net:8443",
            "https://box.ts.net"
        ));
        let refused = too_old("Aether is not installed");
        assert!(refused.contains(NEEDED_AETHER), "{refused}");
    }
}
