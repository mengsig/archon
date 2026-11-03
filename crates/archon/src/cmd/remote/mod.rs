//! `archon remote [setup | status | enable | disable | pair | devices |
//! revoke | origin]`: the mobile web app archond serves (docs/remote.md).

mod qr;
mod runner;
mod setup;
mod tailscale;

use std::fmt::Write as _;
use std::process::ExitCode;

use archon_client::ArchonClient;
use archon_core::protocol::{DeviceRole, Reply, Request};
use archon_core::remote::{PairingCode, RemoteChange, RemoteView, normalize_origin};

use super::{emit, to_json};
use crate::cli::{RemoteCommand, RemoteOriginCommand};
use crate::connect::{connect_or_start, connect_running, unexpected};

pub(crate) use setup::setup;

pub async fn run(command: Option<RemoteCommand>, json: bool) -> anyhow::Result<ExitCode> {
    let command = command.unwrap_or(RemoteCommand::Status);
    let change = |change: RemoteChange| Request::ConfigureRemote { change };
    let request = match command {
        RemoteCommand::Setup(args) => return setup(args, json).await,
        RemoteCommand::Pair { observer } => {
            let role = match observer {
                true => DeviceRole::Observer,
                false => DeviceRole::Operator,
            };
            return pair(role, json).await;
        }
        RemoteCommand::Status | RemoteCommand::Devices => {
            let view = remote_view(&connect_running().await?, Request::RemoteStatus).await?;
            return show(&view, matches!(command, RemoteCommand::Devices), json);
        }
        RemoteCommand::Enable => change(RemoteChange {
            enabled: Some(true),
            ..RemoteChange::default()
        }),
        RemoteCommand::Disable => change(RemoteChange {
            enabled: Some(false),
            ..RemoteChange::default()
        }),
        RemoteCommand::Revoke { device } => Request::RemoteRevoke { device },
        RemoteCommand::Origin(RemoteOriginCommand::Add { url }) => change(RemoteChange {
            add_origins: vec![normalize_origin(&url)?],
            ..RemoteChange::default()
        }),
        RemoteCommand::Origin(RemoteOriginCommand::Remove { url }) => change(RemoteChange {
            remove_origins: vec![normalize_origin(&url)?],
            ..RemoteChange::default()
        }),
    };
    let view = remote_view(&connect_or_start().await?, request).await?;
    show(&view, false, json)
}

/// Send a request answered with the remote view.
async fn remote_view(client: &ArchonClient, request: Request) -> anyhow::Result<RemoteView> {
    match client.request(request).await? {
        Reply::Remote(view) => Ok(*view),
        other => Err(unexpected(&other, "remote")),
    }
}

fn show(view: &RemoteView, devices_only: bool, json: bool) -> anyhow::Result<ExitCode> {
    let text = match devices_only {
        true => devices_text(view),
        false => status_text(view),
    };
    emit(json, || to_json(view), &text)?;
    let broken = view.enabled && view.listening.is_none();
    Ok(if broken {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    })
}

async fn pair(role: DeviceRole, json: bool) -> anyhow::Result<ExitCode> {
    let client = connect_or_start().await?;
    let code = request_pairing(&client, role).await?;
    let mut text = pairing_text(&code)?;
    if role == DeviceRole::Observer {
        text.push_str(
            "\nThe device it pairs is an observer: it reads the queue and changes nothing.",
        );
    }
    emit(json, || to_json(&code), &text)?;
    Ok(ExitCode::SUCCESS)
}

/// A pairing for an operator device, as setup opens one.
async fn open_pairing(client: &ArchonClient) -> anyhow::Result<PairingCode> {
    request_pairing(client, DeviceRole::Operator).await
}

async fn request_pairing(client: &ArchonClient, role: DeviceRole) -> anyhow::Result<PairingCode> {
    // An operator pairing is sent as before, so the request reads the same.
    let role = (role == DeviceRole::Observer).then_some(role);
    let code = match client.request(Request::RemotePair { role }).await? {
        Reply::RemotePairing(code) => code,
        other => return Err(unexpected(&other, "remote_pairing")),
    };
    anyhow::ensure!(!code.urls.is_empty(), "archond named no address to pair at");
    anyhow::ensure!(
        code.code.len() == 9,
        "archond sent a malformed pairing code"
    );
    Ok(code)
}

/// The QR code of the first address (a public one when there is one), the
/// code to type, and every address it works at.
fn pairing_text(code: &PairingCode) -> anyhow::Result<String> {
    let first = &code.urls[0];
    // In the fragment, so the code never reaches a server or a proxy log.
    let link = format!("{first}#code={}", code.code);
    let mut text = qr::render(&link)?;
    let minutes = (code.expires_at - archon_core::now()).num_minutes().max(1);
    let _ = write!(
        text,
        "\nScan it with your phone's camera, or open {first} and type the code\n\n    {}\n\n\
         It works once and expires in {minutes} minutes ({} UTC).",
        code.code,
        code.expires_at.format("%H:%M")
    );
    if code.urls.len() > 1 {
        let _ = write!(text, "\nIt is accepted at: {}", code.urls.join(", "));
    }
    Ok(text)
}

fn status_text(view: &RemoteView) -> String {
    let mut text = match (&view.listening, view.enabled) {
        (Some(at), _) => format!("Remote access is on, listening on {at}."),
        (None, true) => format!(
            "Remote access is on but not listening: {}",
            view.error.as_deref().unwrap_or("the listener is closed")
        ),
        (None, false) => "Remote access is off. `archon remote setup` sets it up.".to_owned(),
    };
    match (&view.mounted_at, view.public_origins.as_slice()) {
        (Some(prefix), _) => {
            let _ = write!(
                text,
                "\nServed under {prefix} by an Aether listener; only {} may call it.",
                view.trusted_origins.join(", ")
            );
        }
        (None, []) => text.push_str("\nPublished at: nowhere (loopback only)."),
        (None, origins) => {
            let _ = write!(text, "\nPublished at: {}", origins.join(", "));
        }
    }
    if let Some(expires) = view.pairing_expires_at {
        let _ = write!(
            text,
            "\nA pairing code is open until {} UTC.",
            expires.format("%H:%M")
        );
    }
    text.push('\n');
    text.push_str(&devices_text(view));
    text
}

fn devices_text(view: &RemoteView) -> String {
    if view.devices.is_empty() {
        return "No device is paired. `archon remote pair` pairs one.".to_owned();
    }
    let mut text = format!("{} paired device(s):", view.devices.len());
    for device in &view.devices {
        let seen = device.last_seen_at.map_or_else(
            || "never connected".to_owned(),
            |at| format!("last seen {}", at.format("%Y-%m-%d %H:%M UTC")),
        );
        let role = match device.role {
            DeviceRole::Operator => "operator",
            DeviceRole::Observer => "observer",
        };
        let _ = write!(
            text,
            "\n  {}  {}  {role}, paired {}, {seen}, {} session(s) open",
            device.id,
            device.name,
            device.paired_at.format("%Y-%m-%d"),
            device.sessions
        );
    }
    text
}

#[cfg(test)]
mod tests {
    use archon_core::remote::DeviceView;

    use super::*;

    fn view() -> RemoteView {
        RemoteView {
            enabled: true,
            address: "127.0.0.1:8378".into(),
            listening: Some("127.0.0.1:8378".into()),
            error: None,
            public_origins: vec!["https://box.tail1234.ts.net".into()],
            mounted_at: None,
            trusted_origins: vec![],
            devices: vec![DeviceView {
                id: "dev_0123456789abcdef01234567".into(),
                name: "Phone".into(),
                role: DeviceRole::Observer,
                paired_at: archon_core::now(),
                last_seen_at: None,
                sessions: 1,
            }],
            pairing_expires_at: None,
        }
    }

    #[test]
    fn the_status_names_the_listener_the_origins_and_each_device() {
        let text = status_text(&view());
        assert!(text.contains("listening on 127.0.0.1:8378"), "{text}");
        assert!(text.contains("https://box.tail1234.ts.net"), "{text}");
        assert!(
            text.contains("dev_0123456789abcdef01234567  Phone  observer, paired"),
            "{text}"
        );
        let mounted = RemoteView {
            mounted_at: Some("/archon".into()),
            trusted_origins: vec!["https://box.tail1234.ts.net".into()],
            ..view()
        };
        let text = status_text(&mounted);
        assert!(
            text.contains("Served under /archon") && !text.contains("Published at"),
            "{text}"
        );
        let off = RemoteView {
            enabled: false,
            listening: None,
            public_origins: vec![],
            devices: vec![],
            ..view()
        };
        let text = status_text(&off);
        assert!(
            text.contains("off") && text.contains("loopback only"),
            "{text}"
        );
    }

    #[test]
    fn the_pairing_link_keeps_the_code_in_the_fragment() {
        let code = PairingCode {
            code: "7K2M-9QXD".into(),
            expires_at: archon_core::now() + chrono::Duration::minutes(5),
            urls: vec![
                "https://box.tail1234.ts.net/pair".into(),
                "http://127.0.0.1:8378/pair".into(),
            ],
        };
        let text = pairing_text(&code).unwrap();
        assert!(
            text.contains("open https://box.tail1234.ts.net/pair and type"),
            "{text}"
        );
        assert!(text.contains("    7K2M-9QXD\n"), "{text}");
        assert!(text.contains("http://127.0.0.1:8378/pair"), "{text}");
        assert!(
            !text.contains("pair?"),
            "the code is never in a query string"
        );
    }
}
