//! `archon remote setup`: serve archond through the `/archon` mount of an
//! Aether listener, publish that listener as the one `tailscale serve`
//! origin, and pair a browser there, saying what each step does. Every step
//! reads what is there first and changes only what differs, so a rerun
//! changes nothing and says so.

use std::io::IsTerminal as _;
use std::process::ExitCode;
use std::time::Duration;

use anyhow::{Context as _, bail};
use archon_client::ArchonClient;
use archon_core::config::{AetherMode, CONFIG_FILE_NAME, Config};
use archon_core::protocol::Request;
use archon_core::remote::{
    MAX_TRUSTED_ORIGINS, PairingCode, RemoteChange, RemoteView, normalize_trusted_origin,
};
use serde::Serialize;

use super::runner::{RunError, run};
use super::setup_aether::{Aether, PREFIX, Status};
use super::setup_publish::Tailnet;
use super::tailscale::{PortUse, PublishOutcome, TailscaleState};
use super::{open_pairing, remote_view};
use crate::cli::RemoteSetupArgs;
use crate::connect::connect_or_start;

/// The first request through `tailscale serve` may wait on a certificate.
const VERIFY_TIMEOUT: Duration = Duration::from_secs(45);
/// The Aether app's Archon pairing screen, relative to its origin.
const PAIR_SCREEN: &str = "/#/pair/archon";

/// What `--json` prints once every step is done.
#[derive(Debug, Serialize)]
struct Outcome {
    remote: RemoteView,
    /// The Aether origin a browser opens; archond answers under its mount.
    origin: String,
    mount: MountOutcome,
    /// Whether this run changed anything.
    changed: bool,
    pairing: Option<PairingCode>,
}

#[derive(Debug, Serialize)]
struct MountOutcome {
    prefix: &'static str,
    target: String,
}

/// Step lines, kept off stdout under `--json` so it stays one document.
pub(super) struct Steps {
    pub(super) json: bool,
    count: u8,
    changed: bool,
}

impl Steps {
    fn begin(&mut self, title: &str, why: &str) {
        self.count += 1;
        if !self.json {
            println!("\n{}. {title}\n   {why}", self.count);
        }
    }

    pub(super) fn note(&self, text: &str) {
        if !self.json {
            println!("   {text}");
        }
    }

    /// Say what this run changed.
    pub(super) fn changed(&mut self, text: &str) {
        self.changed = true;
        self.note(text);
    }
}

/// What a finished bring-up serves.
struct Mounted {
    origin: String,
    target: String,
    /// archond runs without the mount config.toml now names.
    restart_owed: bool,
}

pub(crate) async fn setup(args: RemoteSetupArgs, json: bool) -> anyhow::Result<ExitCode> {
    let mut steps = Steps {
        json,
        count: 0,
        changed: false,
    };
    set_up_archon(&mut steps).await?;
    steps.begin(
        "Start archond",
        "Aether's listener forwards to archond, so it must run; nothing else changes here.",
    );
    let client = connect_or_start().await?;
    let was_enabled = remote_view(&client, Request::RemoteStatus).await?.enabled;
    let config = Config::load_default()?;
    let aether_mode = config.aether.mode;
    let mounted = match bring_up(&client, &args, config, &mut steps).await {
        Ok(mounted) => mounted,
        Err(error) if was_enabled => return Err(error),
        Err(error) => return Err(switch_back_off(&client, error).await),
    };
    if mounted.restart_owed {
        bail!(
            "archond reads [remote] mounted_at and trusted_origins only when it starts. \
             Restart it with `archon daemon restart` (running work survives), then run \
             `archon remote setup` again to check the address and pair"
        );
    }
    let remote = remote_view(&client, Request::RemoteStatus).await?;
    let pairing = match args.no_pair {
        true => None,
        false => Some(pair(&client, &mounted.origin, &mut steps).await?),
    };
    let outcome = Outcome {
        remote,
        origin: mounted.origin,
        mount: MountOutcome {
            prefix: PREFIX,
            target: mounted.target,
        },
        changed: steps.changed,
        pairing,
    };
    if json {
        crate::connect::print_json(&super::to_json(&outcome)?)?;
    } else {
        println!("\n{}", closing_words(&outcome, aether_mode));
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

/// Every step after archond runs; the caller switches archond's listener
/// back off when one fails and it was off before.
async fn bring_up(
    client: &ArchonClient,
    args: &RemoteSetupArgs,
    config: Config,
    steps: &mut Steps,
) -> anyhow::Result<Mounted> {
    steps.begin(
        "Check Aether and Tailscale",
        "Archon's screens are part of the Aether app. Nothing is opened, published or \
         mounted while Tailscale Funnel publishes anything on this machine.",
    );
    let aether = Aether::new(config)?;
    let mounts = aether.mounts().await?;
    let before = aether.status().await?;
    let mut tailnet = Tailnet::read().await?;
    tailnet.refuse_funnel(listener_target(&before, true).ok().as_deref())?;
    steps.note("Aether can mount Archon, and nothing is published through Funnel.");
    let target = enable(client, steps).await?;
    let status = aether.ensure_listening(steps).await?;
    let aether_target = listener_target(&status, false)?;
    steps.begin(
        "Publish Aether's listener on your tailnet",
        "`tailscale serve` gives it an HTTPS address only devices signed in to your tailnet \
         can reach; it is the one address, and archond stays on loopback.",
    );
    tailnet.withdraw_archond(&target, args, steps).await?;
    let origin = tailnet.publish_aether(&aether_target, args, steps).await?;
    let public = aether.ensure_origin(&status, &origin, steps).await?;
    steps.begin(
        "Mount Archon on Aether",
        "Aether forwards /archon to archond, which then answers only Aether's origins.",
    );
    withdraw_public_origins(client, steps).await?;
    aether.ensure_mount(&mounts, &target, steps).await?;
    let trusted = trusted_origins(&public, &origin)?;
    let restart_owed = restart_owed(client, &trusted, steps).await?;
    if !restart_owed {
        verify(&origin, steps).await?;
    }
    Ok(Mounted {
        origin,
        target,
        restart_owed,
    })
}

/// Switch archond's listener on and return the loopback address Aether
/// forwards to.
async fn enable(client: &ArchonClient, steps: &mut Steps) -> anyhow::Result<String> {
    let view = remote_view(client, Request::RemoteStatus).await?;
    let view = match (&view.listening, view.enabled) {
        (Some(_), true) => view,
        _ => {
            let change = RemoteChange {
                enabled: Some(true),
                ..RemoteChange::default()
            };
            steps.changed("Opening archond's listener.");
            remote_view(client, Request::ConfigureRemote { change }).await?
        }
    };
    let Some(listening) = &view.listening else {
        bail!(
            "archond's listener did not open: {}. Change [remote] address in config.toml if \
             another program holds the port, then run this again",
            view.error.as_deref().unwrap_or("no reason was given")
        );
    };
    let bound: std::net::SocketAddr = listening
        .parse()
        .with_context(|| format!("archond listens on {listening}, which is not an address"))?;
    anyhow::ensure!(
        bound.ip().is_loopback(),
        "archond listens on {bound}; Aether mounts only loopback listeners, so set [remote] \
         address = \"127.0.0.1:{}\" in config.toml and restart archond",
        bound.port()
    );
    steps.note(&format!(
        "archond listens on {listening}, on this machine only."
    ));
    Ok(archon_core::remote::local_target(bound))
}

/// Where this machine reaches Aether's listener: where it is bound, or with
/// `configured`, where it will bind while it is still closed.
fn listener_target(status: &Status, configured: bool) -> anyhow::Result<String> {
    let address = match (&status.listening, configured) {
        (Some(listening), _) => listening,
        (None, true) => &status.address,
        (None, false) => bail!("Aether's listener is not open"),
    };
    let bound: std::net::SocketAddr = address
        .parse()
        .with_context(|| format!("Aether listens on {address}, which is not an address"))?;
    Ok(archon_core::remote::local_target(bound))
}

/// Aether's public origins as archond's `trusted_origins`, the published
/// one among them.
fn trusted_origins(public: &[String], published: &str) -> anyhow::Result<Vec<String>> {
    let mut trusted: Vec<String> = Vec::with_capacity(public.len());
    for origin in public {
        let origin = normalize_trusted_origin(origin)
            .with_context(|| format!("Aether's public origin {origin} cannot be trusted"))?;
        if !trusted.contains(&origin) {
            trusted.push(origin);
        }
    }
    anyhow::ensure!(
        trusted.len() <= MAX_TRUSTED_ORIGINS,
        "Aether has {} public origins; archond trusts at most {MAX_TRUSTED_ORIGINS}",
        trusted.len()
    );
    anyhow::ensure!(
        trusted.iter().any(|o| o == published),
        "Aether's origins lack the published {published}"
    );
    Ok(trusted)
}

/// Write the mount into config.toml; whether the running archond still
/// serves without it, so a restart is owed.
async fn restart_owed(
    client: &ArchonClient,
    trusted: &[String],
    steps: &mut Steps,
) -> anyhow::Result<bool> {
    let path = archon_core::paths::config_dir().join(CONFIG_FILE_NAME);
    if super::setup_config::ensure_mounted(&path, PREFIX, trusted)? {
        steps.changed(&format!(
            "Wrote [remote] mounted_at and trusted_origins to {}.",
            path.display()
        ));
    } else {
        steps.note(&format!("{} already names the mount.", path.display()));
    }
    let live = remote_view(client, Request::RemoteStatus).await?;
    let serving = live.mounted_at.as_deref() == Some(PREFIX) && live.trusted_origins == trusted;
    Ok(!serving)
}

/// A failed step leaves archond's listener as this run found it: off, so
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

/// Drop the origins an earlier, direct publication of archond allowed:
/// mounted, archond answers Aether's origins alone.
async fn withdraw_public_origins(client: &ArchonClient, steps: &mut Steps) -> anyhow::Result<()> {
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
    steps.changed(&format!(
        "archond no longer answers {} directly.",
        before.public_origins.join(", ")
    ));
    Ok(())
}

/// A port already serving the target first, else the first free one. A
/// target that Funnel opens to the internet is refused, never reused.
pub(super) fn choose_port(uses: &[(u16, PortUse)]) -> anyhow::Result<u16> {
    assert!(!uses.is_empty(), "at least one port is considered");
    if let Some((port, _)) = uses
        .iter()
        .find(|(_, u)| *u == PortUse::Ours { funnel: true })
    {
        bail!(
            "port {port} publishes it to the whole internet through Tailscale Funnel. \
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
        "no HTTPS port is free for tailscale serve: {}. Free one, or pass --https-port",
        taken.join("; ")
    )
}

/// What to do about a Tailscale that cannot serve HTTPS by name yet.
pub(super) fn not_ready(state: &TailscaleState, program: &str) -> String {
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

pub(super) fn publish_refusal(outcome: &PublishOutcome) -> String {
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
pub(super) async fn confirm(question: &str, json: bool) -> anyhow::Result<bool> {
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

/// GET archond's health check through Aether's published origin and its
/// mount, as a browser would.
async fn verify(origin: &str, steps: &mut Steps) -> anyhow::Result<()> {
    steps.begin(
        "Check the address",
        "Fetch archond's health check through Aether's HTTPS address and its mount.",
    );
    let url = format!("{origin}{PREFIX}/api/health");
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
            "{url} did not answer: {}. Check `aether remote status`: Aether forwards to \
             archond only while no Funnel is published",
            output.complaint()
        );
    }
    steps.note("It answers.");
    Ok(())
}

async fn pair(
    client: &ArchonClient,
    origin: &str,
    steps: &mut Steps,
) -> anyhow::Result<PairingCode> {
    steps.begin(
        "Pair your browser with Archon",
        "A one-time code: the browser that types it first gets its own Archon device, which \
         `archon remote revoke` takes back. Aether pairs the same browser on its own.",
    );
    let code = open_pairing(client).await?;
    let mounted = format!("{origin}{PREFIX}/pair");
    anyhow::ensure!(
        code.urls.contains(&mounted),
        "archond offers no pairing at {mounted}; restart it with `archon daemon restart`"
    );
    if !steps.json {
        println!("\n{}", pairing_text(&code, origin)?);
    }
    Ok(code)
}

/// The QR code of the Aether app's Archon pairing screen and the code to
/// type there. The code never goes in the link: only the browser sees it.
fn pairing_text(code: &PairingCode, origin: &str) -> anyhow::Result<String> {
    let screen = format!("{origin}{PAIR_SCREEN}");
    let minutes = (code.expires_at - archon_core::now()).num_minutes().max(1);
    Ok(format!(
        "{}\nScan it, or open {screen} in the browser you pair with Aether, and type the \
         code\n\n    {}\n\nIt works once and expires in {minutes} minutes ({} UTC).",
        super::qr::render(&screen)?,
        code.code,
        code.expires_at.format("%H:%M")
    ))
}

fn closing_words(outcome: &Outcome, mode: AetherMode) -> String {
    let aether_pair = match mode {
        AetherMode::Shared => "aether remote pair",
        AetherMode::Dedicated => "archon aether remote pair",
    };
    let unchanged = match outcome.changed {
        true => "",
        false => "Nothing changed: everything was already set up.\n",
    };
    format!(
        "{unchanged}Done: open {} for Aether and Archon. Pair the browser with Aether too \
         (`{aether_pair}`); each daemon gives it its own device. `archon remote` shows \
         archond's side, and `archon remote pair` pairs another browser with Archon.",
        outcome.origin
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_port_already_serving_aether_is_reused_before_a_free_one() {
        let uses = [
            (443, PortUse::Taken("http://127.0.0.1:3000".into())),
            (8443, PortUse::Free),
            (10000, PortUse::Ours { funnel: false }),
        ];
        assert_eq!(choose_port(&uses).unwrap(), 10000);
        assert_eq!(choose_port(&uses[..2]).unwrap(), 8443);
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
    fn aether_origins_become_trusted_origins_with_the_published_one() {
        let public = [
            "https://Box.ts.net".to_owned(),
            "https://box.ts.net".to_owned(),
            "https://box.ts.net:8443".to_owned(),
        ];
        let trusted = trusted_origins(&public, "https://box.ts.net").unwrap();
        assert_eq!(trusted, ["https://box.ts.net", "https://box.ts.net:8443"]);
        assert!(trusted_origins(&public, "https://other.ts.net").is_err());
        assert!(trusted_origins(&["ftp://x".to_owned()], "ftp://x").is_err());
    }

    #[test]
    fn the_pairing_link_opens_the_archon_screen_without_the_code() {
        let code = PairingCode {
            code: "7K2M-9QXD".into(),
            expires_at: archon_core::now() + chrono::Duration::minutes(5),
            urls: vec!["https://box.ts.net/archon/pair".into()],
        };
        let text = pairing_text(&code, "https://box.ts.net").unwrap();
        assert!(
            text.contains("open https://box.ts.net/#/pair/archon in"),
            "{text}"
        );
        assert!(text.contains("    7K2M-9QXD\n"), "{text}");
        assert!(
            !text.contains("code=7K2M"),
            "the code is typed, never linked"
        );
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
