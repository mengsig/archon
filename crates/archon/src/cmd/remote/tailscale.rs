//! The `tailscale` command line: whether this machine can serve HTTPS by
//! name, what it publishes already, and publishing archond's listener.

use std::collections::BTreeMap;
use std::time::Duration;

use anyhow::{Context as _, bail};
use serde::Deserialize;

use super::runner::{Output, RunError, display, run};

const QUICK: Duration = Duration::from_secs(5);
const STATUS: Duration = Duration::from_secs(15);
/// `serve` may wait on the control server for a certificate.
const PUBLISH: Duration = Duration::from_secs(60);
/// Names a `tailscale` to use instead of the one on PATH (tests use a fake).
const PROGRAM_VAR: &str = "ARCHON_TAILSCALE";
const PROGRAMS: [&str; 2] = [
    "tailscale",
    "/Applications/Tailscale.app/Contents/MacOS/Tailscale",
];

/// Ports `tailscale serve --https` accepts.
pub(super) const SERVE_PORTS: [u16; 3] = [443, 8443, 10000];

/// This machine's place on the tailnet once everything needed is in order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Node {
    /// `box.tail1234.ts.net`, without the trailing dot.
    pub(super) dns_name: String,
}

/// How far from usable Tailscale is on this machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum TailscaleState {
    /// The program answers but its background service does not.
    NotRunning {
        detail: String,
    },
    Stopped,
    LoggedOut {
        login_url: Option<String>,
    },
    /// Signed in, waiting for a tailnet admin to approve this machine.
    NeedsApproval,
    Connecting,
    NoMagicDns,
    NoHttpsCerts,
    Ready(Node),
}

#[derive(Debug)]
pub(super) struct Tailscale {
    program: String,
}

impl Tailscale {
    /// The first `tailscale` program that answers `version`, or `None`
    /// when none is installed.
    pub(super) async fn locate() -> anyhow::Result<Option<Self>> {
        let chosen = std::env::var(PROGRAM_VAR).ok().filter(|p| !p.is_empty());
        let candidates: Vec<String> = match chosen {
            Some(program) => vec![program],
            None => PROGRAMS.iter().map(|p| (*p).to_owned()).collect(),
        };
        assert!(!candidates.is_empty(), "there is a program to try");
        for program in candidates {
            match run(&program, &["version"], QUICK).await {
                Ok(_) => return Ok(Some(Self { program })),
                Err(RunError::NotFound) => {}
                Err(error) => bail!("`{}`: {error}", display(&program, &["version"])),
            }
        }
        Ok(None)
    }

    pub(super) fn program(&self) -> &str {
        &self.program
    }

    pub(super) async fn state(&self) -> anyhow::Result<TailscaleState> {
        let args = ["status", "--json"];
        let output = run(&self.program, &args, STATUS)
            .await
            .with_context(|| format!("running `{}`", display(&self.program, &args)))?;
        if !output.success() {
            return Ok(TailscaleState::NotRunning {
                detail: format!(
                    "`{}` failed: {}",
                    display(&self.program, &args),
                    output.complaint()
                ),
            });
        }
        parse_state(&output.stdout)
            .with_context(|| format!("reading the output of `{}`", display(&self.program, &args)))
    }

    pub(super) async fn serve_config(&self) -> anyhow::Result<ServeConfig> {
        let args = ["serve", "status", "--json"];
        let output = run(&self.program, &args, STATUS)
            .await
            .with_context(|| format!("running `{}`", display(&self.program, &args)))?;
        if !output.success() {
            bail!(
                "`{}` failed: {}",
                display(&self.program, &args),
                output.complaint()
            );
        }
        parse_serve(&output.stdout)
            .with_context(|| format!("reading the output of `{}`", display(&self.program, &args)))
    }

    /// Publish `target` on this machine's HTTPS `port`, to the tailnet only.
    pub(super) async fn publish(&self, port: u16, target: &str) -> anyhow::Result<PublishOutcome> {
        assert!(SERVE_PORTS.contains(&port), "port {port} is not servable");
        assert!(
            target.starts_with("http://"),
            "target {target} is not archond's plain-HTTP listener"
        );
        let https = format!("--https={port}");
        let args = ["serve", "--bg", https.as_str(), target];
        let command = display(&self.program, &args);
        match run(&self.program, &args, PUBLISH).await {
            Ok(output) => Ok(classify_publish(&output, &command)),
            Err(RunError::TimedOut { limit, partial }) => timed_out(&partial, &command, limit),
            Err(error) => Err(error).with_context(|| format!("running `{command}`")),
        }
    }
}

/// `serve` waits, printing the page that enables the feature,
/// until someone does: what they printed before being stopped says why.
fn timed_out(partial: &Output, command: &str, limit: Duration) -> anyhow::Result<PublishOutcome> {
    match classify_publish(partial, command) {
        outcome @ (PublishOutcome::NotEnabled { .. } | PublishOutcome::NotPermitted { .. }) => {
            Ok(outcome)
        }
        _ => bail!(
            "`{command}` did not finish within {}s; it said: {}",
            limit.as_secs(),
            partial.complaint()
        ),
    }
}

/// What `tailscale serve` answered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum PublishOutcome {
    Published,
    /// This user may not change Tailscale's settings (Linux operator).
    NotPermitted {
        command: String,
    },
    /// A feature is off for the tailnet; the page that turns it on.
    NotEnabled {
        url: Option<String>,
        command: String,
    },
    Failed {
        command: String,
        complaint: String,
    },
}

fn classify_publish(output: &Output, command: &str) -> PublishOutcome {
    if output.success() {
        return PublishOutcome::Published;
    }
    let text = format!("{}\n{}", output.stderr, output.stdout);
    let lower = text.to_ascii_lowercase();
    let command = command.to_owned();
    if lower.contains("access denied") || lower.contains("--operator") {
        return PublishOutcome::NotPermitted { command };
    }
    if lower.contains("not enabled") || lower.contains("to enable") {
        return PublishOutcome::NotEnabled {
            url: first_tailscale_url(&text),
            command,
        };
    }
    PublishOutcome::Failed {
        command,
        complaint: output.complaint(),
    }
}

/// The first `https://….tailscale.com/…` address in `text`.
fn first_tailscale_url(text: &str) -> Option<String> {
    text.split_whitespace()
        .map(|word| word.trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '/'))
        .find(|word| word.starts_with("https://") && word.contains(".tailscale.com/"))
        .map(str::to_owned)
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
struct RawStatus {
    backend_state: String,
    #[serde(rename = "AuthURL")]
    auth_url: String,
    #[serde(rename = "Self")]
    me: Option<RawSelf>,
    current_tailnet: Option<RawTailnet>,
    cert_domains: Option<Vec<String>>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct RawSelf {
    #[serde(rename = "DNSName")]
    dns_name: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct RawTailnet {
    #[serde(rename = "MagicDNSEnabled")]
    magic_dns_enabled: bool,
}

/// `tailscale status --json`, read for the one question the wizard asks:
/// can this machine be reached by name over HTTPS.
pub(super) fn parse_state(json: &str) -> anyhow::Result<TailscaleState> {
    let raw: RawStatus = serde_json::from_str(json).context("not the JSON that was expected")?;
    let state = match raw.backend_state.as_str() {
        "Running" => return running_state(&raw),
        "Stopped" => TailscaleState::Stopped,
        "NeedsLogin" => TailscaleState::LoggedOut {
            login_url: Some(raw.auth_url).filter(|url| !url.is_empty()),
        },
        "NeedsMachineAuth" => TailscaleState::NeedsApproval,
        "Starting" | "NoState" => TailscaleState::Connecting,
        other => TailscaleState::NotRunning {
            detail: format!("Tailscale reports the state `{other}`"),
        },
    };
    Ok(state)
}

fn running_state(raw: &RawStatus) -> anyhow::Result<TailscaleState> {
    let dns_name = raw
        .me
        .as_ref()
        .map(|me| me.dns_name.trim_end_matches('.').to_ascii_lowercase())
        .unwrap_or_default();
    let magic_dns = raw
        .current_tailnet
        .as_ref()
        .is_none_or(|t| t.magic_dns_enabled)
        && !dns_name.is_empty();
    if !magic_dns {
        return Ok(TailscaleState::NoMagicDns);
    }
    let certs = raw.cert_domains.as_deref().unwrap_or_default();
    if !certs.iter().any(|d| d.eq_ignore_ascii_case(&dns_name)) {
        return Ok(TailscaleState::NoHttpsCerts);
    }
    assert!(
        !dns_name.ends_with('.') && !dns_name.contains(char::is_whitespace),
        "`{dns_name}` is not a usable host name"
    );
    assert!(certs.len() <= 64, "implausibly many certificate domains");
    Ok(TailscaleState::Ready(Node { dns_name }))
}

/// What this machine already publishes with `tailscale serve`.
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct ServeConfig {
    /// `host:port` → the target of its `/` handler, when it has one.
    roots: BTreeMap<String, Option<String>>,
    tcp_ports: Vec<u16>,
    funnel: Vec<String>,
    /// Parts of the status this wizard does not know how to read.
    unrecognized: Vec<String>,
}

/// What stands on one HTTPS port.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum PortUse {
    Free,
    /// Already publishing the target; `funnel` when open to the internet.
    Ours {
        funnel: bool,
    },
    /// Something else; what it forwards to.
    Taken(String),
    /// The status has parts this wizard cannot read, so the port may be in
    /// use; what it could not read.
    Unknown(String),
}

/// One scope of the serve configuration. The top level is the background
/// one; each `Foreground` session and each `Services` entry has its own.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct RawServe {
    #[serde(rename = "TCP")]
    tcp: BTreeMap<String, serde_json::Value>,
    #[serde(rename = "Web")]
    web: BTreeMap<String, RawWeb>,
    #[serde(rename = "AllowFunnel")]
    allow_funnel: BTreeMap<String, bool>,
    /// Sessions of `tailscale serve` run without `--bg`, by session id.
    #[serde(rename = "Foreground")]
    foreground: BTreeMap<String, RawServe>,
    /// Tailscale Services, which answer on their own names.
    #[serde(rename = "Services")]
    services: BTreeMap<String, RawServe>,
    #[serde(rename = "ETag")]
    _etag: Option<String>,
    #[serde(flatten)]
    other: BTreeMap<String, serde_json::Value>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct RawWeb {
    #[serde(rename = "Handlers")]
    handlers: BTreeMap<String, RawHandler>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct RawHandler {
    #[serde(rename = "Proxy")]
    proxy: Option<String>,
    #[serde(rename = "Path")]
    path: Option<String>,
    #[serde(rename = "Text")]
    text: Option<String>,
}

/// `tailscale serve status --json`. No configuration is `{}` or nothing.
pub(super) fn parse_serve(json: &str) -> anyhow::Result<ServeConfig> {
    if json.trim().is_empty() {
        return Ok(ServeConfig::default());
    }
    let raw: RawServe = serde_json::from_str(json).context("not the JSON that was expected")?;
    let mut config = ServeConfig::default();
    config.absorb(raw, Scope::Node);
    Ok(config)
}

/// Whose ports a part of the configuration describes.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Scope {
    /// This machine's own name: the background configuration and every
    /// foreground session.
    Node,
    /// A Tailscale Service, which has its own address; only the web hosts it
    /// names can collide with ours.
    Service,
}

impl ServeConfig {
    fn absorb(&mut self, raw: RawServe, scope: Scope) {
        for (key, web) in raw.web {
            let root = web.handlers.get("/").map(describe_handler);
            let slot = self.roots.entry(key.to_ascii_lowercase()).or_default();
            if slot.is_none() {
                *slot = root;
            }
        }
        if scope == Scope::Node {
            self.tcp_ports
                .extend(raw.tcp.keys().filter_map(|p| p.parse::<u16>().ok()));
        }
        self.funnel.extend(
            raw.allow_funnel
                .into_iter()
                .filter_map(|(key, on)| on.then(|| key.to_ascii_lowercase())),
        );
        if scope == Scope::Node {
            let keys = raw.other.into_keys();
            self.unrecognized.extend(keys.map(|key| format!("`{key}`")));
        }
        for session in raw.foreground.into_values() {
            self.absorb(session, scope);
        }
        for service in raw.services.into_values() {
            self.absorb(service, Scope::Service);
        }
    }
}

fn describe_handler(handler: &RawHandler) -> String {
    match (&handler.proxy, &handler.path, &handler.text) {
        (Some(proxy), _, _) => proxy.clone(),
        (None, Some(path), _) => format!("files at {path}"),
        (None, None, Some(_)) => "a fixed text".to_owned(),
        (None, None, None) => "something else".to_owned(),
    }
}

/// A forwarding target compared without regard to `localhost` or a trailing `/`.
fn same_target(a: &str, b: &str) -> bool {
    let normal = |t: &str| {
        t.trim_end_matches('/')
            .to_ascii_lowercase()
            .replace("localhost", "127.0.0.1")
    };
    normal(a) == normal(b)
}

impl ServeConfig {
    /// What `host`'s HTTPS `port` is doing, given that `target` is the
    /// address the wizard wants served.
    pub(super) fn port_use(&self, host: &str, port: u16, target: &str) -> PortUse {
        let key = format!("{}:{port}", host.to_ascii_lowercase());
        match self.roots.get(&key) {
            Some(Some(existing)) if same_target(existing, target) => PortUse::Ours {
                funnel: self.funnel.contains(&key),
            },
            Some(Some(existing)) => PortUse::Taken(existing.clone()),
            Some(None) => PortUse::Taken("other paths of its own".to_owned()),
            None if self.tcp_ports.contains(&port) => PortUse::Taken("a TCP forward".to_owned()),
            None if !self.unrecognized.is_empty() => PortUse::Unknown(self.unrecognized.join(", ")),
            None => PortUse::Free,
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    const NODE: &str = "box.tail1234.ts.net";
    const TARGET: &str = "http://127.0.0.1:8378";

    fn tailscale_json(state: &str, magic_dns: bool, certs: bool) -> String {
        json!({
            "BackendState": state,
            "AuthURL": if state == "NeedsLogin" { "https://login.tailscale.com/a/abc123" } else { "" },
            "Self": {"DNSName": format!("{NODE}.")},
            "CurrentTailnet": {"Name": "someone@example.com", "MagicDNSEnabled": magic_dns},
            "CertDomains": if certs { vec![NODE] } else { vec![] },
        })
        .to_string()
    }

    fn serve_json(port: u16, target: &str, funnel: bool) -> String {
        let key = format!("{NODE}:{port}");
        let mut config = json!({
            "TCP": {port.to_string(): {"HTTPS": true}},
            "Web": {key.clone(): {"Handlers": {"/": {"Proxy": target}}}},
        });
        if funnel {
            config["AllowFunnel"] = json!({key: true});
        }
        config.to_string()
    }

    #[test]
    fn a_refusal_to_serve_is_told_apart_from_a_feature_switched_off() {
        let denied = Output {
            code: Some(1),
            stdout: String::new(),
            stderr: "Access denied: serve config denied; use sudo tailscale set --operator=$USER"
                .into(),
        };
        assert!(matches!(
            classify_publish(&denied, "tailscale serve"),
            PublishOutcome::NotPermitted { .. }
        ));
        let off = Output {
            code: Some(1),
            stdout: "Serve is not enabled on your tailnet.\nTo enable, visit:\n  https://login.tailscale.com/f/serve?node=n1".into(),
            stderr: String::new(),
        };
        let PublishOutcome::NotEnabled { url, .. } = classify_publish(&off, "tailscale serve")
        else {
            panic!("not recognized as switched off");
        };
        assert_eq!(
            url.as_deref(),
            Some("https://login.tailscale.com/f/serve?node=n1")
        );
    }

    #[test]
    fn a_running_tailnet_with_names_and_certificates_is_ready() {
        let state = parse_state(&tailscale_json("Running", true, true)).expect("parses");
        let TailscaleState::Ready(node) = state else {
            panic!("not ready: {state:?}");
        };
        assert_eq!(node.dns_name, NODE, "the trailing dot is gone");
    }

    #[test]
    fn each_backend_state_maps_to_what_the_person_must_do() {
        let case = |state: &str, magic, certs| {
            parse_state(&tailscale_json(state, magic, certs)).expect("parses")
        };
        assert_eq!(case("Stopped", true, true), TailscaleState::Stopped);
        assert_eq!(
            case("NeedsLogin", true, true),
            TailscaleState::LoggedOut {
                login_url: Some("https://login.tailscale.com/a/abc123".to_owned())
            }
        );
        assert_eq!(
            case("NeedsMachineAuth", true, true),
            TailscaleState::NeedsApproval
        );
        assert_eq!(case("Starting", true, true), TailscaleState::Connecting);
        assert_eq!(case("Running", false, true), TailscaleState::NoMagicDns);
        assert_eq!(case("Running", true, false), TailscaleState::NoHttpsCerts);
        assert!(matches!(
            case("Mystery", true, true),
            TailscaleState::NotRunning { .. }
        ));
    }

    #[test]
    fn a_certificate_for_another_name_is_not_https_for_this_machine() {
        let json = r#"{"BackendState":"Running","Self":{"DNSName":"box.t.ts.net."},"CertDomains":["other.t.ts.net"]}"#;
        assert_eq!(
            parse_state(json).expect("parses"),
            TailscaleState::NoHttpsCerts
        );
    }

    #[test]
    fn output_that_is_not_json_is_an_error_not_a_state() {
        assert!(parse_state("Tailscale is stopped.").is_err());
    }

    #[test]
    fn serve_status_tells_free_from_ours_from_taken() {
        let ours = parse_serve(&serve_json(443, TARGET, true)).expect("parses");
        assert_eq!(
            ours.port_use(NODE, 443, TARGET),
            PortUse::Ours { funnel: true }
        );
        assert_eq!(ours.port_use(NODE, 8443, TARGET), PortUse::Free);

        let other = parse_serve(&serve_json(443, "http://127.0.0.1:3000", false)).expect("parses");
        assert_eq!(
            other.port_use(NODE, 443, TARGET),
            PortUse::Taken("http://127.0.0.1:3000".to_owned())
        );

        let localhost =
            parse_serve(&serve_json(443, "http://localhost:8378/", false)).expect("parses");
        assert_eq!(
            localhost.port_use(NODE, 443, TARGET),
            PortUse::Ours { funnel: false }
        );
    }

    #[test]
    fn nothing_served_is_empty_or_blank_output() {
        for text in ["{}", "", "  \n"] {
            let config = parse_serve(text).expect("parses");
            assert_eq!(config.port_use(NODE, 443, TARGET), PortUse::Free);
        }
    }

    #[test]
    fn a_tcp_forward_on_a_port_takes_it() {
        let config =
            parse_serve(r#"{"TCP":{"8443":{"TCPForward":"127.0.0.1:22"}}}"#).expect("parses");
        assert!(matches!(
            config.port_use(NODE, 8443, TARGET),
            PortUse::Taken(_)
        ));
    }

    #[test]
    fn a_foreground_session_holds_its_port() {
        let json = format!(
            r#"{{"Foreground":{{"session-1":{{"Web":{{"{NODE}:443":{{"Handlers":{{"/":{{"Proxy":"http://127.0.0.1:3000"}}}}}}}}}}}}}}"#
        );
        let config = parse_serve(&json).expect("parses");
        assert_eq!(
            config.port_use(NODE, 443, TARGET),
            PortUse::Taken("http://127.0.0.1:3000".to_owned())
        );
        assert_eq!(config.port_use(NODE, 8443, TARGET), PortUse::Free);
    }

    #[test]
    fn a_foreground_tcp_forward_and_funnel_are_seen() {
        let json = format!(
            r#"{{"Foreground":{{"s":{{"TCP":{{"8443":{{"TCPForward":"127.0.0.1:22"}}}},"AllowFunnel":{{"{NODE}:443":true}},"Web":{{"{NODE}:443":{{"Handlers":{{"/":{{"Proxy":"{TARGET}"}}}}}}}}}}}}}}"#
        );
        let config = parse_serve(&json).expect("parses");
        assert_eq!(
            config.port_use(NODE, 443, TARGET),
            PortUse::Ours { funnel: true }
        );
        assert!(matches!(
            config.port_use(NODE, 8443, TARGET),
            PortUse::Taken(_)
        ));
    }

    #[test]
    fn a_service_does_not_take_the_machines_own_ports() {
        let json = r#"{"Services":{"svc:docs":{"TCP":{"443":{"HTTPS":true}},"Web":{"docs.t.ts.net:443":{"Handlers":{"/":{"Proxy":"http://127.0.0.1:9000"}}}},"Tun":false}}}"#;
        let config = parse_serve(json).expect("parses");
        assert_eq!(config.port_use(NODE, 443, TARGET), PortUse::Free);
        assert_eq!(
            config.port_use("docs.t.ts.net", 443, TARGET),
            PortUse::Taken("http://127.0.0.1:9000".to_owned())
        );
    }

    #[test]
    fn a_port_with_only_other_paths_is_not_free() {
        let json = format!(
            r#"{{"Web":{{"{NODE}:443":{{"Handlers":{{"/docs":{{"Path":"/srv/docs"}}}}}}}}}}"#
        );
        let config = parse_serve(&json).expect("parses");
        assert!(matches!(
            config.port_use(NODE, 443, TARGET),
            PortUse::Taken(_)
        ));
    }

    #[test]
    fn a_status_with_parts_it_cannot_read_is_never_reported_free() {
        let config = parse_serve(r#"{"Mystery":{"x":1}}"#).expect("parses");
        assert_eq!(
            config.port_use(NODE, 443, TARGET),
            PortUse::Unknown("`Mystery`".to_owned())
        );
        let known = parse_serve(&serve_json(443, TARGET, false)).expect("parses");
        assert_eq!(known.port_use(NODE, 8443, TARGET), PortUse::Free);
    }
}
