//! The Tailscale half of `archon remote setup`: refuse while anything is
//! published through Funnel, withdraw a direct publication of archond, and
//! publish Aether's listener as the one `tailscale serve` origin.

use std::time::Duration;

use anyhow::bail;

use super::runner::{display, run};
use super::setup::{Steps, choose_port, confirm, not_ready, publish_refusal};
use super::tailscale::{
    Node, PortUse, PublishOutcome, SERVE_PORTS, ServeConfig, Tailscale, TailscaleState,
};
use crate::cli::RemoteSetupArgs;

const UNPUBLISH_TIMEOUT: Duration = Duration::from_secs(30);

/// Aether's own reason for refusing a mount under Funnel, word for word.
pub(super) const AETHER_FUNNEL_REASON: &str = "a mount cannot be added while this listener is \
    published through Tailscale Funnel (`tailscale funnel reset` stops it); mounted daemons \
    are never served through Funnel";

/// This machine on the tailnet, and what it publishes now.
pub(super) struct Tailnet {
    tailscale: Tailscale,
    node: Node,
    served: ServeConfig,
}

impl Tailnet {
    /// Tailscale, ready to serve HTTPS by name, and its serve configuration.
    pub(super) async fn read() -> anyhow::Result<Self> {
        let Some(tailscale) = Tailscale::locate().await? else {
            bail!(
                "Tailscale is not installed. Install it from https://tailscale.com/download and \
                 sign in, then run this again: Aether's listener is published with \
                 `tailscale serve`, and Aether reads Tailscale to keep mounts off Funnel"
            );
        };
        let node = match tailscale.state().await? {
            TailscaleState::Ready(node) => node,
            other => bail!("{}", not_ready(&other, tailscale.program())),
        };
        let served = tailscale.serve_config().await?;
        Ok(Self {
            tailscale,
            node,
            served,
        })
    }

    /// Refuse while Funnel opens any port of this machine to the internet:
    /// Aether's listener shares the name, so Origin checks cannot tell them apart.
    /// `aether_target` names Aether's listener in the refusal when it is known.
    pub(super) fn refuse_funnel(&self, aether_target: Option<&str>) -> anyhow::Result<()> {
        let funnels = self.served.funnels();
        if funnels.is_empty() {
            return Ok(());
        }
        let aether_port = aether_target.and_then(|target| {
            let port = SERVE_PORTS.into_iter().find(|&port| {
                self.served.port_use(&self.node.dns_name, port, target)
                    == PortUse::Ours { funnel: true }
            });
            port.map(|port| (port, target))
        });
        let what = match aether_port {
            Some((port, target)) => {
                format!("Aether's listener ({target}) is published on port {port}")
            }
            None => format!("{} is published", funnels.join(", ")),
        };
        bail!(
            "{what} through Tailscale Funnel, open to the whole internet. Aether refuses: \
             {AETHER_FUNNEL_REASON}. Nothing was mounted or published"
        )
    }

    /// Withdraw every port that publishes archond's own listener: the one
    /// origin is Aether's, and archond stays on loopback.
    pub(super) async fn withdraw_archond(
        &mut self,
        archond_target: &str,
        args: &RemoteSetupArgs,
        steps: &mut Steps,
    ) -> anyhow::Result<()> {
        let ports: Vec<u16> = SERVE_PORTS
            .into_iter()
            .filter(|&port| {
                matches!(
                    self.served
                        .port_use(&self.node.dns_name, port, archond_target),
                    PortUse::Ours { .. }
                )
            })
            .collect();
        for &port in &ports {
            let https = format!("--https={port}");
            let words = ["serve", https.as_str(), "off"];
            let command = display(self.tailscale.program(), &words);
            let question = format!("Stop publishing archond directly: run `{command}`?");
            if !args.yes && !confirm(&question, steps.json).await? {
                bail!(
                    "archond is still published at port {port}. Run this again and answer yes, or pass --yes"
                );
            }
            let output = run(self.tailscale.program(), &words, UNPUBLISH_TIMEOUT).await?;
            anyhow::ensure!(
                output.success(),
                "`{command}` failed: {}",
                output.complaint()
            );
            steps.changed(&format!("archond is no longer published on port {port}."));
        }
        if !ports.is_empty() {
            self.served = self.tailscale.serve_config().await?;
        }
        Ok(())
    }

    /// Publish `aether_target` with `tailscale serve`, reusing a port that
    /// already serves it, and return its HTTPS origin.
    pub(super) async fn publish_aether(
        &self,
        aether_target: &str,
        args: &RemoteSetupArgs,
        steps: &mut Steps,
    ) -> anyhow::Result<String> {
        let ports = match args.https_port {
            Some(port) if SERVE_PORTS.contains(&port) => vec![port],
            Some(port) => {
                bail!("--https-port {port}: tailscale serves HTTPS only on 443, 8443 or 10000")
            }
            None => SERVE_PORTS.to_vec(),
        };
        let uses: Vec<(u16, PortUse)> = ports
            .iter()
            .map(|&port| {
                (
                    port,
                    self.served
                        .port_use(&self.node.dns_name, port, aether_target),
                )
            })
            .collect();
        let port = choose_port(&uses)?;
        let origin = match port {
            443 => format!("https://{}", self.node.dns_name),
            port => format!("https://{}:{port}", self.node.dns_name),
        };
        if uses.contains(&(port, PortUse::Ours { funnel: false })) {
            steps.note(&format!(
                "Aether's listener is already published at {origin}."
            ));
            return Ok(origin);
        }
        let program = self.tailscale.program();
        let command = display(
            program,
            &["serve", "--bg", &format!("--https={port}"), aether_target],
        );
        if !args.yes && !confirm(&format!("Run `{command}`?"), steps.json).await? {
            bail!("not published. Run this again and answer yes, or pass --yes");
        }
        match self.tailscale.publish(port, aether_target).await? {
            PublishOutcome::Published => {}
            refused => bail!("{}", publish_refusal(&refused)),
        }
        steps.changed(&format!("Published Aether's listener at {origin}."));
        Ok(origin)
    }
}
