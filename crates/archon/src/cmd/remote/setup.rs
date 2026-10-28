//! `archon remote setup`: switch the listener on, publish it over HTTPS,
//! check that it answers there and pair a phone, saying what each step
//! does. Every step finds what an earlier run did, so running it again is safe.

use std::io::IsTerminal as _;
use std::process::ExitCode;
use std::time::Duration;

use anyhow::{Context as _, bail};
use archon_client::ArchonClient;
use archon_core::protocol::Request;
use archon_core::remote::{PairingCode, RemoteChange, RemoteView, normalize_origin};
use serde::Serialize;

use super::runner::{RunError, display, run};
use super::tailscale::{PortUse, PublishOutcome, SERVE_PORTS, Tailscale, TailscaleState};
use super::{open_pairing, pairing_text, remote_view};
use crate::cli::RemoteSetupArgs;
use crate::connect::connect_or_start;

/// The first request through `tailscale serve` may wait on a certificate.
const VERIFY_TIMEOUT: Duration = Duration::from_secs(45);

/// What `--json` prints once every step is done.
#[derive(Debug, Serialize)]
struct Outcome {
    remote: RemoteView,
    origin: Option<String>,
    pairing: Option<PairingCode>,
}

/// Step lines, kept off stdout under `--json` so it stays one document.
struct Steps {
    json: bool,
    count: u8,
}

impl Steps {
    fn begin(&mut self, title: &str, why: &str) {
        self.count += 1;
        if !self.json {
            println!("\n{}. {title}\n   {why}", self.count);
        }
    }

    fn note(&self, text: &str) {
        if !self.json {
            println!("   {text}");
        }
    }
}

pub(crate) async fn setup(args: RemoteSetupArgs, json: bool) -> anyhow::Result<ExitCode> {
    let mut steps = Steps { json, count: 0 };
    set_up_archon(&mut steps).await?;
    steps.begin(
        "Start archond",
        "The app talks to archond, so it must run; nothing else changes here.",
    );
    let client = connect_or_start().await?;
    let was_enabled = remote_view(&client, Request::RemoteStatus).await?.enabled;
    let target = enable(&client, &mut steps).await?;
    let origin = match publish(&client, &args, &target, json, &mut steps).await {
        Ok(origin) => origin,
        Err(error) if was_enabled => return Err(error),
        Err(error) => return Err(switch_back_off(&client, error).await),
    };
    if let Some(origin) = &origin {
        verify(origin, &mut steps).await?;
    }
    let remote = remote_view(&client, Request::RemoteStatus).await?;
    let pairing = match args.no_pair {
        true => None,
        false => Some(pair(&client, &mut steps).await?),
    };
    let outcome = Outcome {
        remote,
        origin,
        pairing,
    };
    if json {
        crate::connect::print_json(&super::to_json(&outcome)?)?;
    } else {
        println!("\n{}", closing_words(&outcome));
    }
    Ok(ExitCode::SUCCESS)
}

/// Archon's own Aether first, as `archon setup` does, when it has no config.
async fn set_up_archon(steps: &mut Steps) -> anyhow::Result<()> {
    let config = archon_core::config::Config::load_default()?;
    if !super::super::first_run::needs_setup(&config)? {
        return Ok(());
    }
    if steps.json {
        bail!("Archon is not set up yet; run `archon setup`, then this again");
    }
    steps.begin(
        "Set up Archon",
        "Archon's own Aether has no config yet; this is what `archon setup` does.",
    );
    let code = super::super::instance::setup(false, false, false).await?;
    anyhow::ensure!(code == ExitCode::SUCCESS, "`archon setup` did not finish");
    Ok(())
}

/// Switch the listener on and return the loopback address it serves.
async fn enable(client: &ArchonClient, steps: &mut Steps) -> anyhow::Result<String> {
    steps.begin(
        "Open the listener",
        "archond serves the app on this machine's loopback only; nothing off it can \
         connect until a proxy publishes it.",
    );
    let change = RemoteChange {
        enabled: Some(true),
        ..RemoteChange::default()
    };
    let view = remote_view(client, Request::ConfigureRemote { change }).await?;
    let Some(listening) = view.listening else {
        bail!(
            "the listener did not open: {}. Change [remote] address in config.toml if \
             another program holds the port, then run this again",
            view.error.as_deref().unwrap_or("no reason was given")
        );
    };
    let bound: std::net::SocketAddr = listening
        .parse()
        .with_context(|| format!("archond listens on {listening}, which is not an address"))?;
    assert!(bound.port() > 0, "a bound listener has a port");
    steps.note(&format!("Listening on {listening}."));
    Ok(archon_core::remote::local_origin(bound))
}

/// A publish that failed leaves the listener as this run found it: off, so
/// nothing an earlier run published keeps answering.
async fn switch_back_off(client: &ArchonClient, error: anyhow::Error) -> anyhow::Error {
    let change = RemoteChange {
        enabled: Some(false),
        ..RemoteChange::default()
    };
    match remote_view(client, Request::ConfigureRemote { change }).await {
        Ok(view) if !view.enabled && view.listening.is_none() => {
            error.context("setup stopped and switched remote access back off")
        }
        Ok(_) => error
            .context("setup stopped, but remote access is still on; run `archon remote disable`"),
        Err(off) => error.context(format!(
            "setup stopped, and switching remote access back off failed ({off:#}); run \
             `archon remote disable`"
        )),
    }
}

/// Publish the listener and allow its origin; `None` with `--local`.
async fn publish(
    client: &ArchonClient,
    args: &RemoteSetupArgs,
    target: &str,
    json: bool,
    steps: &mut Steps,
) -> anyhow::Result<Option<String>> {
    if args.local {
        withdraw_public_origins(client, steps).await?;
        return Ok(None);
    }
    let origin = match &args.proxy_url {
        Some(url) => {
            steps.begin(
                "Use your HTTPS proxy",
                "Your proxy terminates TLS and forwards to the listener; archond answers \
                 only the origins it was told about.",
            );
            let origin = normalize_origin(url)?;
            steps.note(&format!(
                "Forward {origin} to {target}, passing the Host header through unchanged."
            ));
            origin
        }
        None => tailscale_origin(args, target, json, steps).await?,
    };
    let change = RemoteChange {
        add_origins: vec![origin.clone()],
        ..RemoteChange::default()
    };
    remote_view(client, Request::ConfigureRemote { change }).await?;
    steps.note(&format!("archond now answers browsers at {origin}."));
    Ok(Some(origin))
}

/// `--local`: drop the origins earlier runs allowed, so no proxy that still
/// forwards to the listener is answered.
async fn withdraw_public_origins(client: &ArchonClient, steps: &mut Steps) -> anyhow::Result<()> {
    steps.begin(
        "Keep it on this machine",
        "Not published (--local); archond refuses every browser origin but this machine's.",
    );
    let before = remote_view(client, Request::RemoteStatus).await?;
    if before.public_origins.is_empty() {
        return Ok(());
    }
    let change = RemoteChange {
        remove_origins: before.public_origins.clone(),
        ..RemoteChange::default()
    };
    let after = remote_view(client, Request::ConfigureRemote { change }).await?;
    anyhow::ensure!(
        after.public_origins.is_empty(),
        "archond still answers {}; remove them with `archon remote origin remove`",
        after.public_origins.join(", ")
    );
    steps.note(&format!(
        "No longer answers {}. Stop the proxy that published it too.",
        before.public_origins.join(", ")
    ));
    Ok(())
}

/// Publish `target` with `tailscale serve`, reusing a port that already
/// serves it, and return the HTTPS origin.
async fn tailscale_origin(
    args: &RemoteSetupArgs,
    target: &str,
    json: bool,
    steps: &mut Steps,
) -> anyhow::Result<String> {
    steps.begin(
        "Publish on your tailnet",
        "`tailscale serve` gives the listener an HTTPS address only devices signed in \
         to your tailnet can reach; it is never opened to the internet.",
    );
    let Some(tailscale) = Tailscale::locate().await? else {
        bail!(
            "Tailscale is not installed. Install it from https://tailscale.com/download, sign \
             in, and run this again; or pass --proxy-url for your own HTTPS proxy, or --local"
        );
    };
    let node = match tailscale.state().await? {
        TailscaleState::Ready(node) => node,
        other => bail!("{}", not_ready(&other, tailscale.program())),
    };
    let served = tailscale.serve_config().await?;
    let ports = match args.https_port {
        Some(port) if SERVE_PORTS.contains(&port) => vec![port],
        Some(port) => {
            bail!("--https-port {port}: tailscale serves HTTPS only on 443, 8443 or 10000")
        }
        None => SERVE_PORTS.to_vec(),
    };
    let uses: Vec<(u16, PortUse)> = ports
        .iter()
        .map(|&port| (port, served.port_use(&node.dns_name, port, target)))
        .collect();
    let port = choose_port(&uses)?;
    let origin = match port {
        443 => format!("https://{}", node.dns_name),
        port => format!("https://{}:{port}", node.dns_name),
    };
    if uses.contains(&(port, PortUse::Ours { funnel: false })) {
        steps.note(&format!("Already published at {origin}."));
        return Ok(origin);
    }
    let command = display(
        tailscale.program(),
        &["serve", "--bg", &format!("--https={port}"), target],
    );
    if !args.yes && !confirm(&format!("Run `{command}`?"), json).await? {
        bail!("not published. Run this again and answer yes, or pass --yes");
    }
    match tailscale.publish(port, target).await? {
        PublishOutcome::Published => {}
        refused => bail!("{}", publish_refusal(&refused)),
    }
    steps.note(&format!("Published at {origin}."));
    Ok(origin)
}

/// A port already serving the target first, else the first free one. A
/// target that Funnel opens to the internet is refused, never reused.
fn choose_port(uses: &[(u16, PortUse)]) -> anyhow::Result<u16> {
    assert!(!uses.is_empty(), "at least one port is considered");
    if let Some((port, _)) = uses
        .iter()
        .find(|(_, u)| *u == PortUse::Ours { funnel: true })
    {
        bail!(
            "port {port} publishes archond to the whole internet through Tailscale Funnel. \
             Turn that off with `tailscale funnel --https={port} off`, then run this again"
        );
    }
    if let Some((port, _)) = uses
        .iter()
        .find(|(_, u)| *u == PortUse::Ours { funnel: false })
    {
        return Ok(*port);
    }
    if let Some((port, _)) = uses.iter().find(|(_, u)| *u == PortUse::Free) {
        return Ok(*port);
    }
    let taken: Vec<String> = uses
        .iter()
        .map(|(port, used)| match used {
            PortUse::Taken(by) => format!("{port} serves {by}"),
            PortUse::Unknown(what) => format!("{port} may be in use ({what})"),
            PortUse::Free | PortUse::Ours { .. } => unreachable!("handled above"),
        })
        .collect();
    bail!(
        "no HTTPS port is free for tailscale serve: {}. Free one, or pass --proxy-url",
        taken.join("; ")
    )
}

/// What to do about a Tailscale that cannot serve HTTPS by name yet.
fn not_ready(state: &TailscaleState, program: &str) -> String {
    let admin = "https://login.tailscale.com/admin/dns";
    match state {
        TailscaleState::NotRunning { detail } => {
            format!(
                "Tailscale is installed but its service is not running ({detail}); start tailscaled"
            )
        }
        TailscaleState::Stopped => format!("Tailscale is stopped; run `{program} up`"),
        TailscaleState::LoggedOut { login_url } => match login_url {
            Some(url) => format!("Tailscale is signed out; sign in at {url}"),
            None => format!("Tailscale is signed out; run `{program} up`"),
        },
        TailscaleState::NeedsApproval => {
            "a tailnet admin must approve this machine in the Tailscale admin console".to_owned()
        }
        TailscaleState::Connecting => {
            "Tailscale is still connecting; try again in a moment".to_owned()
        }
        TailscaleState::NoMagicDns => {
            format!("MagicDNS is off for your tailnet; turn it on at {admin}")
        }
        TailscaleState::NoHttpsCerts => {
            format!("HTTPS certificates are off for your tailnet; turn them on at {admin}")
        }
        TailscaleState::Ready(_) => unreachable!("a ready Tailscale needs nothing"),
    }
}

fn publish_refusal(outcome: &PublishOutcome) -> String {
    match outcome {
        PublishOutcome::NotPermitted { command } => format!(
            "`{command}` was refused: this user may not change Tailscale. Run \
             `sudo tailscale set --operator=$USER` once, then run this again"
        ),
        PublishOutcome::NotEnabled { url, command } => format!(
            "`{command}` says Serve is off for your tailnet; turn it on at {}, then run this again",
            url.as_deref()
                .unwrap_or("https://login.tailscale.com/admin")
        ),
        PublishOutcome::Failed { command, complaint } => format!("`{command}` failed: {complaint}"),
        PublishOutcome::Published => unreachable!("publishing succeeded"),
    }
}

/// Ask on the terminal; no terminal (or `--json`) is never a yes.
async fn confirm(question: &str, json: bool) -> anyhow::Result<bool> {
    let terminal = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
    if json || !terminal {
        return Ok(false);
    }
    let question = question.to_owned();
    tokio::task::spawn_blocking(move || {
        let mut input = std::io::stdin().lock();
        super::super::first_run::ask(&mut input, &mut std::io::stdout(), &question, true)
    })
    .await
    .context("asking on the terminal")?
}

/// GET `/api/health` through the published origin, as a phone would.
async fn verify(origin: &str, steps: &mut Steps) -> anyhow::Result<()> {
    steps.begin(
        "Check the address",
        "Fetch the app's health check through the HTTPS address, as your phone will.",
    );
    let url = format!("{origin}/api/health");
    let limit = VERIFY_TIMEOUT.as_secs().to_string();
    let args = ["-fsS", "--max-time", limit.as_str(), url.as_str()];
    let output = match run("curl", &args, VERIFY_TIMEOUT + Duration::from_secs(5)).await {
        Ok(output) => output,
        Err(RunError::NotFound) => {
            steps.note(&format!(
                "curl is not installed, so open {url} yourself to check."
            ));
            return Ok(());
        }
        Err(error) => bail!("checking {url}: {error}"),
    };
    if !output.success() || !output.stdout.contains("\"ok\"") {
        bail!(
            "{url} did not answer: {}. Check that the proxy forwards to archond's listener \
             and passes the Host header, then run this again",
            output.complaint()
        );
    }
    steps.note("It answers.");
    Ok(())
}

async fn pair(client: &ArchonClient, steps: &mut Steps) -> anyhow::Result<PairingCode> {
    steps.begin(
        "Pair your phone",
        "A one-time code: the device that types it first gets its own credential, which \
         `archon remote revoke` takes back.",
    );
    let code = open_pairing(client).await?;
    if !steps.json {
        println!("\n{}", pairing_text(&code)?);
    }
    Ok(code)
}

fn closing_words(outcome: &Outcome) -> String {
    let place = outcome
        .origin
        .as_deref()
        .map_or_else(|| "this machine's loopback".to_owned(), str::to_owned);
    format!(
        "Done: the app is at {place}. `archon remote` shows its state, `archon remote pair` \
         pairs another device, and `archon remote disable` closes it."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_port_already_serving_archond_is_reused_before_a_free_one() {
        let uses = [
            (443, PortUse::Taken("http://127.0.0.1:3000".into())),
            (8443, PortUse::Free),
            (10000, PortUse::Ours { funnel: false }),
        ];
        assert_eq!(choose_port(&uses).unwrap(), 10000);
        assert_eq!(choose_port(&uses[..2]).unwrap(), 8443);
    }

    #[test]
    fn funnel_is_refused_and_a_full_tailnet_names_what_holds_each_port() {
        let public = [(443, PortUse::Ours { funnel: true }), (8443, PortUse::Free)];
        let refused = choose_port(&public).unwrap_err().to_string();
        assert!(
            refused.contains("Funnel") && refused.contains("off"),
            "{refused}"
        );
        let full = [
            (443, PortUse::Taken("http://127.0.0.1:3000".into())),
            (8443, PortUse::Unknown("`Mystery`".into())),
        ];
        let refused = choose_port(&full).unwrap_err().to_string();
        assert!(
            refused.contains("443 serves http://127.0.0.1:3000"),
            "{refused}"
        );
        assert!(refused.contains("8443 may be in use"), "{refused}");
    }

    #[test]
    fn each_unready_state_names_its_fix() {
        let texts = [
            not_ready(&TailscaleState::Stopped, "tailscale"),
            not_ready(&TailscaleState::NoMagicDns, "tailscale"),
            not_ready(&TailscaleState::NoHttpsCerts, "tailscale"),
            not_ready(&TailscaleState::LoggedOut { login_url: None }, "tailscale"),
        ];
        assert!(texts[0].contains("tailscale up"));
        assert!(texts[1].contains("MagicDNS") && texts[2].contains("HTTPS certificates"));
        assert!(texts[3].contains("tailscale up"));
    }
}
