//! `archon remote` against a private archond: the listener survives a
//! restart, and setup mounts archond on a stand-in Aether, publishing it
//! with a stand-in `tailscale` and checking it with a stand-in `curl`.
mod common;

use std::io::{Read as _, Write as _};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::process::Output;

use common::{Env, start_daemon, stop_daemon};
use serde_json::Value;

/// The browsers' origin: Aether's listener as `tailscale serve` publishes it.
const ORIGIN: &str = "https://localhost";

fn config_on(port: u16) -> String {
    format!("[aether]\nmode = \"shared\"\n[remote]\naddress = \"127.0.0.1:{port}\"\n")
}

fn free_port() -> u16 {
    let held = TcpListener::bind("127.0.0.1:0").unwrap();
    held.local_addr().unwrap().port()
}

/// A `tailscale` signed in as `localhost`, whose serve config is a file that
/// `serve --bg` writes and `serve --https=N off` clears, and a `curl` that
/// finds archond healthy. Every call is appended to `calls`.
fn stand_ins(dir: &Path, backend_state: &str) {
    std::fs::create_dir_all(dir).unwrap();
    let status = serde_json::json!({
        "BackendState": backend_state,
        "Self": {"DNSName": "localhost."},
        "CurrentTailnet": {"MagicDNSEnabled": true},
        "CertDomains": ["localhost"],
    });
    std::fs::write(dir.join("status.json"), status.to_string()).unwrap();
    let tailscale = r#"#!/bin/sh
dir=$(dirname "$0")
echo "tailscale $*" >> "$dir/calls"
case "$*" in
version) echo 1.90.0 ;;
"status --json") cat "$dir/status.json" ;;
"serve status --json") cat "$dir/serve.json" 2>/dev/null || echo '{}' ;;
"serve --bg --https="*)
    port=${3#--https=}
    printf '{"Web":{"localhost:%s":{"Handlers":{"/":{"Proxy":"%s"}}}}}' "$port" "$4" > "$dir/serve.json"
    ;;
"serve --https="*" off") rm -f "$dir/serve.json" ;;
*) echo "unexpected: $*" >&2; exit 2 ;;
esac
"#;
    archon_core::testing::write_script(&dir.join("tailscale"), tailscale);
    let curl =
        "#!/bin/sh\necho \"curl $*\" >> \"$(dirname \"$0\")/calls\"\necho '{\"status\":\"ok\"}'\n";
    archon_core::testing::write_script(&dir.join("curl"), curl);
}

/// An `aether` whose listener opens on `port` and whose origins and mounts
/// are files. It refuses a mount under a Funnel, or when `refuse` exists,
/// with Aether's own words.
fn aether_stand_in(dir: &Path, port: u16) {
    let script = r#"#!/bin/sh
dir=$(dirname "$0")
echo "aether $*" >> "$dir/calls"
origins() {
    printf '['; sep=''
    if [ -f "$dir/origins" ]; then
        while IFS= read -r o; do printf '%s"%s"' "$sep" "$o"; sep=','; done < "$dir/origins"
    fi
    printf ']'
}
case "$*" in
"remote mount list --json") cat "$dir/mounts.json" 2>/dev/null || echo '[]' ;;
"remote status --json")
    on=false; listening=''
    if [ -f "$dir/aether-on" ]; then on=true; listening=',"listening":"127.0.0.1:PORT"'; fi
    printf '{"status":{"revision":1,"settings":{"enabled":%s,"pairing":true,"public_origins":%s,"push":false},"address":"127.0.0.1:PORT"%s},"devices":[]}\n' "$on" "$(origins)" "$listening"
    ;;
"remote enable --json") touch "$dir/aether-on"; echo '{}' ;;
"remote enable --json --origin "*)
    touch "$dir/aether-on"; : > "$dir/origins"; shift 3
    while [ $# -gt 0 ]; do
        if [ "$1" = --origin ]; then echo "$2" >> "$dir/origins"; shift 2; else shift; fi
    done
    echo '{}'
    ;;
"remote mount add /archon --target "*)
    if [ -f "$dir/refuse" ] || grep -q AllowFunnel "$dir/serve.json" 2>/dev/null; then
        echo "Error: a mount cannot be added while this listener is published through Tailscale Funnel (\`tailscale funnel reset\` stops it); mounted daemons are never served through Funnel" >&2
        exit 1
    fi
    printf '[{"prefix":"/archon","kind":"archon","target":"%s"}]' "$6" > "$dir/mounts.json"
    echo '{}'
    ;;
*) echo "unexpected: $*" >&2; exit 2 ;;
esac
"#;
    let script = script.replace("PORT", &port.to_string());
    archon_core::testing::write_script(&dir.join("aether"), &script);
}

fn wizard(env: &Env, tools: &Path, args: &[&str]) -> Output {
    let path = std::env::join_paths(std::iter::once(tools.to_path_buf()).chain(
        std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()),
    ))
    .unwrap();
    env.command(args)
        .env("PATH", path)
        .env("ARCHON_TAILSCALE", tools.join("tailscale"))
        .output()
        .unwrap()
}

fn json(out: &Output) -> Value {
    assert!(out.status.success(), "{out:?}");
    serde_json::from_slice(&out.stdout).unwrap()
}

/// `archon --json remote …`, which must succeed.
fn remote_json(env: &Env, args: &[&str]) -> Value {
    let args: Vec<&str> = ["--json", "remote"].iter().chain(args).copied().collect();
    json(&env.command(&args).output().unwrap())
}

fn calls(tools: &Path) -> String {
    std::fs::read_to_string(tools.join("calls")).unwrap_or_default()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

#[test]
fn the_listener_is_off_until_enabled_and_comes_back_after_a_restart() {
    let env = Env::with_config(&config_on(0));
    let daemon = start_daemon(&env);
    let off = env.run(&["remote"]);
    assert!(String::from_utf8_lossy(&off.stdout).contains("Remote access is off"));
    let enabled = remote_json(&env, &["enable"]);
    assert_eq!(enabled["enabled"], true, "{enabled}");
    let listening = enabled["listening"].as_str().unwrap().to_owned();
    assert!(listening.starts_with("127.0.0.1:"), "{enabled}");
    let paired = remote_json(&env, &["pair"]);
    assert_eq!(paired["code"].as_str().unwrap().len(), 9, "{paired}");
    let page = http(port_of(&listening), "GET", "/", &[], "");
    assert_eq!(page.status, 200, "{}", page.body);
    assert!(
        page.body.contains("<base href=\"./\">") && page.body.contains("src/main.js"),
        "a direct visit gets the app: {}",
        page.body
    );
    let mode = std::fs::metadata(env.path("data/remote.json")).unwrap();
    assert_eq!(
        std::os::unix::fs::PermissionsExt::mode(&mode.permissions()) & 0o777,
        0o600
    );
    stop_daemon(&env, daemon);
    let daemon = start_daemon(&env);
    let status = remote_json(&env, &["status"]);
    assert!(
        status["listening"].is_string(),
        "re-opened after a restart: {status}"
    );
    assert!(
        status["pairing_expires_at"].is_null(),
        "a pairing never outlives archond"
    );
    let disabled = remote_json(&env, &["disable"]);
    assert_eq!(disabled["enabled"], false);
    assert!(disabled["listening"].is_null());
    let refused = env.command(&["remote", "pair"]).output().unwrap();
    assert!(!refused.status.success());
    assert!(stderr(&refused).contains("remote access is off"));
    let revoke = env
        .command(&["remote", "revoke", "nobody"])
        .output()
        .unwrap();
    assert!(!revoke.status.success(), "an unknown device is an error");
    stop_daemon(&env, daemon);
}

#[test]
fn setup_mounts_archon_on_aether_as_the_one_origin_and_a_rerun_changes_nothing() {
    let archond_port = free_port();
    let aether = TcpListener::bind("127.0.0.1:0").unwrap();
    let aether_port = aether.local_addr().unwrap().port();
    let env = Env::with_config(&config_on(archond_port));
    let daemon = start_daemon(&env);
    let tools = env.path("tools");
    stand_ins(&tools, "Running");
    aether_stand_in(&tools, aether_port);
    // An earlier setup published archond itself on 443; the one origin is Aether's.
    let direct = format!(
        r#"{{"Web":{{"localhost:443":{{"Handlers":{{"/":{{"Proxy":"http://127.0.0.1:{archond_port}"}}}}}}}}}}"#
    );
    std::fs::write(tools.join("serve.json"), direct).unwrap();
    let first = wizard(&env, &tools, &["remote", "setup", "--yes"]);
    assert!(!first.status.success(), "{first:?}");
    assert!(
        stderr(&first).contains("archon daemon restart"),
        "{first:?}"
    );
    let config = std::fs::read_to_string(env.path("config/config.toml")).unwrap();
    assert!(
        config.contains("mounted_at = \"/archon\"\ntrusted_origins = [\"https://localhost\"]"),
        "{config}"
    );
    let made = calls(&tools);
    for call in [
        "tailscale serve --https=443 off".to_owned(),
        format!("tailscale serve --bg --https=443 http://127.0.0.1:{aether_port}"),
        "aether remote enable --json --origin https://localhost".to_owned(),
        format!("aether remote mount add /archon --target http://127.0.0.1:{archond_port}"),
    ] {
        assert!(made.contains(&call), "{call} in {made}");
    }
    assert!(
        !made.contains("tailscale funnel"),
        "Funnel is never touched: {made}"
    );
    stop_daemon(&env, daemon);
    let daemon = start_daemon(&env);
    let second = json(&wizard(
        &env,
        &tools,
        &["--json", "remote", "setup", "--yes"],
    ));
    assert_eq!(second["origin"], ORIGIN, "{second}");
    assert_eq!(
        second["changed"], false,
        "the restart was all that was owed: {second}"
    );
    assert_eq!(second["remote"]["mounted_at"], "/archon", "{second}");
    assert_eq!(
        second["remote"]["trusted_origins"],
        serde_json::json!([ORIGIN])
    );
    assert_eq!(second["remote"]["public_origins"], serde_json::json!([]));
    assert_eq!(
        second["pairing"]["urls"][0],
        "https://localhost/archon/pair"
    );
    assert!(calls(&tools).contains("curl -fsS --max-time 45 https://localhost/archon/api/health"));
    browser_reaches_both_daemons(&env, aether, archond_port, &second);
    std::fs::remove_file(tools.join("calls")).unwrap();
    let config_before = std::fs::read_to_string(env.path("config/config.toml")).unwrap();
    let again = wizard(&env, &tools, &["remote", "setup", "--no-pair"]);
    let text = String::from_utf8_lossy(&again.stdout).into_owned();
    assert!(again.status.success(), "{again:?}");
    assert!(text.contains("Nothing changed"), "{text}");
    let made = calls(&tools);
    for change in ["serve --bg", "off", "remote enable", "mount add"] {
        assert!(
            !made.contains(change),
            "a rerun changes nothing ({change}): {made}"
        );
    }
    let config_after = std::fs::read_to_string(env.path("config/config.toml")).unwrap();
    assert_eq!(config_before, config_after);
    stop_daemon(&env, daemon);
}

/// A browser on the Aether origin pairs with Aether and, through the
/// mount, with archond: two devices, one per daemon, nothing shared.
fn browser_reaches_both_daemons(env: &Env, aether: TcpListener, archond_port: u16, setup: &Value) {
    let aether_port = aether.local_addr().unwrap().port();
    std::thread::spawn(move || serve_as_aether(&aether, archond_port));
    let origin = [("Origin", ORIGIN), ("Content-Type", "application/json")];
    let mine = http(aether_port, "POST", "/api/pair/claim", &origin, "{}");
    assert!(mine.body.contains("aether-device-1"), "{}", mine.body);
    let code = setup["pairing"]["code"].as_str().unwrap();
    let claim = serde_json::json!({"code": code, "device_name": "Browser"}).to_string();
    let paired = http(
        aether_port,
        "POST",
        "/archon/api/pair/claim",
        &origin,
        &claim,
    );
    assert_eq!(paired.status, 200, "{}", paired.body);
    let set = paired.header("set-cookie");
    assert!(set.contains("Path=/archon;"), "{set}");
    let cookie = set.split(';').next().unwrap().to_owned();
    let with_cookie = [
        ("Cookie", cookie.as_str()),
        ("Sec-Fetch-Site", "same-origin"),
    ];
    let session = http(aether_port, "GET", "/archon/api/session", &with_cookie, "");
    assert_eq!(session.status, 200, "{}", session.body);
    assert!(session.body.contains("Browser"), "{}", session.body);
    let aether_only = [("Cookie", "aether_device=aether-device-1")];
    let refused = http(aether_port, "GET", "/archon/api/session", &aether_only, "");
    assert_eq!(
        refused.status, 401,
        "Aether's device is no Archon device: {}",
        refused.body
    );
    let devices = remote_json(env, &["devices"]);
    let names: Vec<&str> = devices["devices"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        ["Browser"],
        "archond holds its own device only: {devices}"
    );
}

/// Aether's listener as far as the browser needs it: its own pairing at
/// `/api/pair/claim`, and `/archon/*` forwarded to archond as the mount
/// does it (prefix stripped, `Host` set to the target, `Origin` passed on).
fn serve_as_aether(listener: &TcpListener, archond_port: u16) {
    for stream in listener.incoming() {
        let mut browser = stream.unwrap();
        let (head, body) = read_request(&mut browser);
        let path = head.split(' ').nth(1).unwrap_or_default().to_owned();
        if path == "/api/pair/claim" {
            let reply = r#"{"device":{"id":"aether-device-1","role":"operator"}}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}",
                reply.len()
            );
            browser.write_all(response.as_bytes()).unwrap();
            continue;
        }
        let Some(rest) = path.strip_prefix("/archon") else {
            browser
                .write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n")
                .unwrap();
            continue;
        };
        let mut lines: Vec<String> = Vec::new();
        for (index, line) in head.split("\r\n").enumerate() {
            if index == 0 {
                lines.push(line.replacen(&path, rest, 1));
            } else if line.to_ascii_lowercase().starts_with("host:") {
                lines.push(format!("Host: 127.0.0.1:{archond_port}"));
            } else {
                lines.push(line.to_owned());
            }
        }
        let mut archond = TcpStream::connect(("127.0.0.1", archond_port)).unwrap();
        archond.write_all(lines.join("\r\n").as_bytes()).unwrap();
        archond.write_all(b"\r\n\r\n").unwrap();
        archond.write_all(&body).unwrap();
        let mut answer = Vec::new();
        archond.read_to_end(&mut answer).unwrap();
        browser.write_all(&answer).unwrap();
    }
}

/// A request's head (without the blank line) and its body.
fn read_request(stream: &mut TcpStream) -> (String, Vec<u8>) {
    let mut bytes = Vec::new();
    let mut byte = [0u8; 1];
    while !bytes.ends_with(b"\r\n\r\n") {
        assert_eq!(stream.read(&mut byte).unwrap(), 1, "the head ends");
        bytes.push(byte[0]);
    }
    let head = String::from_utf8(bytes[..bytes.len() - 4].to_vec()).unwrap();
    let length: usize = head
        .lines()
        .find_map(|l| {
            l.to_ascii_lowercase()
                .strip_prefix("content-length:")
                .map(|v| v.trim().parse().unwrap())
        })
        .unwrap_or(0);
    let mut body = vec![0u8; length];
    stream.read_exact(&mut body).unwrap();
    (head, body)
}

struct Response {
    status: u16,
    head: String,
    body: String,
}

impl Response {
    fn header(&self, name: &str) -> String {
        let wanted = format!("{}:", name.to_ascii_lowercase());
        self.head
            .lines()
            .find(|l| l.to_ascii_lowercase().starts_with(&wanted))
            .map(|l| l[wanted.len()..].trim().to_owned())
            .unwrap_or_default()
    }
}

/// One request to `127.0.0.1:port` as a browser on the Aether origin sends it.
fn http(port: u16, method: &str, path: &str, headers: &[(&str, &str)], body: &str) -> Response {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(10)))
        .unwrap();
    let mut request = format!(
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\nContent-Length: {}\r\n",
        body.len()
    );
    for (name, value) in headers {
        request.push_str(&format!("{name}: {value}\r\n"));
    }
    request.push_str("\r\n");
    request.push_str(body);
    stream.write_all(request.as_bytes()).unwrap();
    let mut answer = String::new();
    stream.read_to_string(&mut answer).unwrap();
    let (head, body) = answer.split_once("\r\n\r\n").unwrap_or((&answer, ""));
    let status = head.split(' ').nth(1).unwrap_or("0").parse().unwrap();
    Response {
        status,
        head: head.to_owned(),
        body: body.to_owned(),
    }
}

fn port_of(listening: &str) -> u16 {
    listening.rsplit_once(':').unwrap().1.parse().unwrap()
}

#[test]
fn funnel_is_refused_with_aethers_reason_before_anything_is_opened() {
    let env = Env::with_config(&config_on(free_port()));
    let daemon = start_daemon(&env);
    let tools = env.path("tools");
    stand_ins(&tools, "Running");
    let aether_port = free_port();
    aether_stand_in(&tools, aether_port);
    let reason = "mounted daemons are never served through Funnel";
    // Aether's listener is already published through a Funnel.
    let funnel = format!(
        r#"{{"Web":{{"localhost:443":{{"Handlers":{{"/":{{"Proxy":"http://127.0.0.1:{aether_port}"}}}}}}}},"AllowFunnel":{{"localhost:443":true}}}}"#
    );
    std::fs::write(tools.join("serve.json"), funnel).unwrap();
    let refused = wizard(&env, &tools, &["remote", "setup", "--yes"]);
    assert!(!refused.status.success());
    let errors = stderr(&refused);
    let named =
        format!("Aether's listener (http://127.0.0.1:{aether_port}) is published on port 443");
    assert!(
        errors.contains(&named) && errors.contains(reason),
        "{errors}"
    );
    // A Funnel on any other port of this machine is refused as well.
    let elsewhere = r#"{"Web":{"localhost:8443":{"Handlers":{"/":{"Proxy":"http://127.0.0.1:3000"}}}},"AllowFunnel":{"localhost:8443":true}}"#;
    std::fs::write(tools.join("serve.json"), elsewhere).unwrap();
    let refused = wizard(&env, &tools, &["remote", "setup", "--yes"]);
    let errors = stderr(&refused);
    assert!(!refused.status.success());
    assert!(
        errors.contains("localhost:8443 is published") && errors.contains(reason),
        "{errors}"
    );
    let made = calls(&tools);
    for change in ["serve --bg", "remote enable", "mount add", "funnel"] {
        assert!(
            !made.contains(change),
            "nothing is opened or published ({change}): {made}"
        );
    }
    // Aether's own refusal, from a Funnel that appears meanwhile, is passed on.
    std::fs::remove_file(tools.join("serve.json")).unwrap();
    std::fs::write(tools.join("refuse"), "").unwrap();
    let refused = wizard(&env, &tools, &["remote", "setup", "--yes"]);
    let errors = stderr(&refused);
    assert!(
        errors.contains("Aether refused to mount /archon") && errors.contains(reason),
        "{errors}"
    );
    assert!(
        errors.contains("switched remote access back off"),
        "{errors}"
    );
    let status = remote_json(&env, &["status"]);
    assert_eq!(
        status["enabled"], false,
        "a refused setup leaves archond's listener off: {status}"
    );
    let config = std::fs::read_to_string(env.path("config/config.toml")).unwrap();
    assert!(!config.contains("mounted_at"), "{config}");
    stop_daemon(&env, daemon);
}

#[test]
fn setup_stops_with_the_fix_for_an_old_aether_or_an_unready_tailscale() {
    let env = Env::with_config(&config_on(free_port()));
    let daemon = start_daemon(&env);
    let tools = env.path("tools");
    stand_ins(&tools, "Running");
    let old = "#!/bin/sh\necho \"error: unrecognized subcommand 'mount'\" >&2\nexit 2\n";
    archon_core::testing::write_script(&tools.join("aether"), old);
    let refused = wizard(&env, &tools, &["remote", "setup", "--yes"]);
    assert!(!refused.status.success());
    let errors = stderr(&refused);
    assert!(errors.contains("needs Aether 1.3.0 or later"), "{errors}");
    aether_stand_in(&tools, free_port());
    stand_ins(&tools, "Stopped");
    let stopped = wizard(&env, &tools, &["remote", "setup", "--yes"]);
    assert!(!stopped.status.success());
    assert!(stderr(&stopped).contains("tailscale up"), "{stopped:?}");
    stand_ins(&tools, "Running");
    // No terminal and no --yes: nothing is published without consent.
    let unasked = wizard(&env, &tools, &["remote", "setup"]);
    assert!(!unasked.status.success());
    assert!(stderr(&unasked).contains("pass --yes"), "{unasked:?}");
    assert!(!calls(&tools).contains("serve --bg"), "{}", calls(&tools));
    let status = remote_json(&env, &["status"]);
    assert_eq!(
        status["enabled"], false,
        "a stopped setup opens nothing: {status}"
    );
    stop_daemon(&env, daemon);
}

/// The binary carries the mobile web app, embedded by the crate's build
/// script, with its runtime files and none of its tooling.
#[test]
fn the_build_embeds_the_archon_web_app() {
    let crate_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../archon-remote");
    assert!(
        crate_dir.join("web/index.html").exists(),
        "crates/archon-remote/web is the app"
    );
    let manifest = std::fs::read_to_string(crate_dir.join("Cargo.toml")).unwrap();
    assert!(manifest.contains("build = \"build.rs\""), "{manifest}");
    let binary = std::fs::read(env!("CARGO_BIN_EXE_archon")).unwrap();
    for marker in ["manifest.webmanifest", "navigator.serviceWorker"] {
        let found = binary.windows(marker.len()).any(|w| w == marker.as_bytes());
        assert!(found, "the archon binary does not carry {marker}");
    }
}
