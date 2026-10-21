//! Dedicated mode end to end: `archon setup` seeds Archon's own Aether from
//! the operator's, and the lifecycle commands drive a fake `aether` on PATH
//! with the instance's environment, never the operator's directories.

mod common;
#[path = "common/tui.rs"]
mod tui_fake;

use std::collections::BTreeMap;
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use common::Daemon;

const OPERATOR_CONFIG: &str =
    "[network]\ndefault = \"allowed\"\n\n[listen]\naddress = \"127.0.0.1:7777\"\n";

struct Env {
    _root: tempfile::TempDir,
    home: PathBuf,
}

impl Env {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().canonicalize().unwrap();
        let operator = home.join("operator-aether");
        std::fs::create_dir_all(&operator).unwrap();
        std::fs::write(operator.join("config.toml"), OPERATOR_CONFIG).unwrap();
        std::fs::write(operator.join("providers.toml"), "").unwrap();
        let bin = home.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        // `tui --help` lists `--host-commands`, as an Aether that takes it does.
        let script = format!(
            "#!/bin/sh\n[ \"$*\" = 'tui --help' ] && echo '      --host-commands <PATH>' && exit 0\n\
             printf '%s|%s|%s|%s|%s|%s|%s\\n' \"$*\" \"$AETHER_CONFIG_DIR\" \
             \"$AETHER_DATA_DIR\" \"$AETHER_SOCKET\" \"$ARCHON_CONFIG_DIR\" \"$ARCHON_DATA_DIR\" \
             \"$ARCHON_SOCKET\" >> '{}'\n",
            home.join("aether-calls.log").display()
        );
        archon_core::testing::write_script(&bin.join("aether"), &script);
        Self { _root: root, home }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.home.join(name)
    }

    fn command(&self, args: &[&str]) -> Command {
        let path = format!(
            "{}:{}",
            self.path("bin").display(),
            std::env::var("PATH").unwrap_or_default()
        );
        let mut command = Command::new(env!("CARGO_BIN_EXE_archon"));
        command
            .args(args)
            .env("PATH", path)
            .env("ARCHON_CONFIG_DIR", self.path("config"))
            .env("ARCHON_DATA_DIR", self.path("data"))
            .env("ARCHON_SOCKET", self.path("run/archond.sock"))
            .env("AETHER_CONFIG_DIR", self.path("operator-aether"))
            .env_remove("AETHER_SOCKET")
            .env_remove("AETHER_DATA_DIR")
            .env("ARCHON_NO_UPDATE_CHECK", "1")
            .env("NO_COLOR", "1");
        command
    }

    fn run(&self, args: &[&str]) -> Output {
        let out = self.command(args).output().unwrap();
        assert!(out.status.success(), "archon {args:?} failed: {out:?}");
        out
    }

    /// archond in the foreground, once it answers. Started here rather than
    /// by `daemon start`, whose fixed 10 s autostart budget a loaded host
    /// can overrun while it pages in three cold debug binaries.
    fn start_archond(&self) -> Daemon {
        let log = std::fs::File::create(self.path("archond.log")).unwrap();
        let child = self
            .command(&["daemon", "run"])
            .stdin(Stdio::null())
            .stdout(log.try_clone().unwrap())
            .stderr(log)
            .spawn()
            .unwrap();
        let mut daemon = Daemon(child);
        let deadline = Instant::now() + common::START_WAIT;
        while !self
            .command(&["daemon", "status"])
            .output()
            .unwrap()
            .status
            .success()
        {
            if let Some(status) = daemon.0.try_wait().unwrap() {
                let log = std::fs::read_to_string(self.path("archond.log")).unwrap();
                panic!("archond exited early ({status}):\n{log}");
            }
            if Instant::now() >= deadline {
                let log = std::fs::read_to_string(self.path("archond.log")).unwrap();
                panic!(
                    "archond did not answer within {:?}:\n{log}",
                    common::START_WAIT
                );
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        daemon
    }

    fn calls(&self) -> Vec<Vec<String>> {
        let text = std::fs::read_to_string(self.path("aether-calls.log")).unwrap_or_default();
        // archond's own `aether --version` probe, for `status`, invokes nothing.
        text.lines()
            .map(|line| line.split('|').map(str::to_owned).collect::<Vec<_>>())
            .filter(|call| call[0] != "--version")
            .collect()
    }
}

fn contents(dir: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    std::fs::read_dir(dir)
        .unwrap()
        .map(|entry| {
            let path = entry.unwrap().path();
            let bytes = std::fs::read(&path).unwrap();
            (path, bytes)
        })
        .collect()
}

/// Listens where the instance's aetherd would while the shim was asked to
/// start it more often than to stop it, and ends after `lives` stops.
fn fake_aetherd(log: PathBuf, socket: PathBuf, lives: usize) -> JoinHandle<()> {
    assert!(lives > 0, "a daemon that is never stopped never ends");
    std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(60);
        let mut listener = None;
        while Instant::now() < deadline {
            let text = std::fs::read_to_string(&log).unwrap_or_default();
            let count = |verb: &str| text.lines().filter(|l| l.starts_with(verb)).count();
            let (starts, stops) = (count("daemon|"), count("stop|"));
            if listener.is_none() && starts > stops {
                listener = Some(UnixListener::bind(&socket).unwrap());
            }
            if listener.is_some() && stops >= starts {
                listener = None;
                std::fs::remove_file(&socket).unwrap();
            }
            if stops >= lives {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    })
}

/// Run `archon setup` twice and once as JSON: it seeds Archon's Aether config
/// from the operator's, then leaves it unchanged. The instance config dir.
fn setup_seeds_the_instance(env: &Env) -> PathBuf {
    let setup = String::from_utf8(env.run(&["setup"]).stdout).unwrap();
    assert!(setup.contains("seeded from"), "{setup}");
    assert!(setup.contains("modes/calm.toml seeded"), "{setup}");
    let instance_dir = env.path("config/aether");
    let calm = std::fs::read_to_string(instance_dir.join("modes/calm.toml")).unwrap();
    assert!(calm.contains("role = \"archon-orchestrator\""));
    let seeded = std::fs::read_to_string(instance_dir.join("config.toml")).unwrap();
    assert!(seeded.contains("name = \"archon\""), "{seeded}");
    assert!(
        seeded.contains("allowed") && !seeded.contains("7777"),
        "{seeded}"
    );
    let link = std::fs::read_link(instance_dir.join("providers.toml")).unwrap();
    assert_eq!(link, env.path("operator-aether/providers.toml"));
    let again = String::from_utf8(env.run(&["setup"]).stdout).unwrap();
    assert!(again.contains("unchanged"), "{again}");
    assert!(again.contains("modes/calm.toml installed"), "{again}");
    let json: serde_json::Value =
        serde_json::from_slice(&env.run(&["--json", "setup"]).stdout).unwrap();
    assert_eq!(json["calm_mode"], "installed");
    instance_dir
}

#[test]
fn setup_seeds_and_the_lifecycle_drives_archons_own_aether() {
    let env = Env::new();
    let operator_before = contents(&env.path("operator-aether"));
    let instance_dir = setup_seeds_the_instance(&env);

    let socket = env.path("run/aetherd.sock");
    let daemon = fake_aetherd(env.path("aether-calls.log"), socket.clone(), 1);
    let mut archond = env.start_archond();
    let started = String::from_utf8(env.run(&["daemon", "start"]).stdout).unwrap();
    assert!(started.contains("archond"), "{started}");
    let status = String::from_utf8(env.run(&["daemon", "status"]).stdout).unwrap();
    assert!(
        status.contains("Archon's own aetherd is running"),
        "{status}"
    );
    env.run(&["aether", "agents", "--json"]);
    let kept = String::from_utf8(env.run(&["daemon", "stop", "--keep-aether"]).stdout).unwrap();
    assert!(kept.contains("Stopped archond"), "{kept}");
    assert!(!kept.contains("aetherd"), "{kept}");
    assert!(
        archond.0.wait().unwrap().success(),
        "archond stopped cleanly"
    );
    assert!(socket.exists(), "Archon's own aetherd was kept running");
    let stopped = String::from_utf8(env.run(&["daemon", "stop"]).stdout).unwrap();
    assert!(stopped.contains("archond is not running"), "{stopped}");
    assert!(
        stopped.contains("Stopped Archon's own aetherd"),
        "a plain stop also stops Archon's own aetherd: {stopped}"
    );
    daemon.join().unwrap();

    let calls = env.calls();
    let verbs: Vec<_> = calls.iter().map(|c| c[0].as_str()).collect();
    assert_eq!(verbs, ["daemon", "agents --json", "stop"]);
    for call in &calls {
        let expected = [
            instance_dir.clone(),
            env.path("data/aether"),
            socket.clone(),
            env.path("config"),
            env.path("data"),
            env.path("run/archond.sock"),
        ];
        let got: Vec<_> = call[1..].iter().map(PathBuf::from).collect();
        assert_eq!(got, expected, "the environment of `aether {}`", call[0]);
    }
    assert_eq!(contents(&env.path("operator-aether")), operator_before);
}

/// An upgrade from before 1.0 leaves `archon.toml` and no `config.toml`: setup
/// says so, writes a config that changes nothing, and never touches one after.
#[test]
fn setup_writes_a_commented_config_once_and_reports_a_legacy_one() {
    let env = Env::new();
    let config_dir = env.path("config");
    std::fs::create_dir_all(&config_dir).unwrap();
    std::fs::write(config_dir.join("archon.toml"), "[steward]\n").unwrap();

    let first = String::from_utf8(env.run(&["setup"]).stderr).unwrap();
    let config = config_dir.join("config.toml");
    assert!(
        first.contains(&format!("wrote {}", config.display())),
        "{first}"
    );
    assert!(first.contains("archon.toml is ignored"), "{first}");
    let written = std::fs::read_to_string(&config).unwrap();
    let settings = written
        .lines()
        .filter(|line| !line.is_empty() && !line.starts_with('#'));
    assert_eq!(settings.count(), 0, "every key is commented out: {written}");

    let mine = "[limits]\nmax_parallel_agents = 2\n";
    std::fs::write(&config, mine).unwrap();
    let again = String::from_utf8(env.run(&["setup"]).stderr).unwrap();
    assert!(!again.contains("wrote"), "{again}");
    assert!(again.contains("archon.toml is ignored"), "{again}");
    assert_eq!(std::fs::read_to_string(&config).unwrap(), mine);

    let doctor = env.command(&["doctor"]).output().unwrap();
    let report = String::from_utf8(doctor.stdout).unwrap();
    assert!(report.contains("legacy config"), "{report}");
    assert!(report.contains("archon.toml is ignored"), "{report}");
}

/// Aether's `/voice setup` refuses a linked `voice.toml`: setup copies it, and
/// `archon tui` replaces the link an older setup made before the TUI starts.
#[test]
fn tui_start_gives_the_instance_its_own_voice_toml() {
    let env = Env::new();
    let voice = "language = \"en\"\n";
    let source = env.path("operator-aether/voice.toml");
    std::fs::write(&source, voice).unwrap();
    let operator_before = contents(&env.path("operator-aether"));

    let setup = String::from_utf8(env.run(&["setup"]).stdout).unwrap();
    assert!(setup.contains("voice.toml      copied from"), "{setup}");
    let own = env.path("config/aether/voice.toml");
    assert!(std::fs::symlink_metadata(&own).unwrap().is_file());

    // The state every install made before this change is in.
    std::fs::remove_file(&own).unwrap();
    std::os::unix::fs::symlink(&source, &own).unwrap();
    let doctor = env.command(&["doctor"]).output().unwrap();
    let doctor = String::from_utf8(doctor.stdout).unwrap();
    assert!(doctor.contains("voice.toml links to"), "{doctor}");
    std::fs::create_dir_all(env.path("run")).unwrap();
    let _aetherd = UnixListener::bind(env.path("run/aetherd.sock")).unwrap();

    let server = tui_fake::serve(
        &env.path("run/archond.sock"),
        2,
        vec![Ok(tui_fake::orchestrator()), Ok(tui_fake::orchestrator())],
    );
    let first = env.run(&["tui"]);
    let note = String::from_utf8(first.stderr).unwrap();
    assert!(note.contains("voice.toml was a link"), "{note}");
    assert!(std::fs::symlink_metadata(&own).unwrap().is_file());
    assert_eq!(std::fs::read_to_string(&own).unwrap(), voice);
    let second = env.run(&["tui"]);
    assert!(second.stderr.is_empty(), "{second:?}");
    assert_eq!(std::fs::read_to_string(&own).unwrap(), voice);

    let verbs: Vec<_> = env.calls().into_iter().map(|c| c[0].clone()).collect();
    let tui = format!(
        "tui --agent agt_selected --view-mode calm --host-commands {}",
        env.path("data/tui-host-commands.json").display()
    );
    assert_eq!(verbs, [tui.clone(), tui], "the TUI started both times");
    assert_eq!(server.join().unwrap().len(), 6);
    assert_eq!(contents(&env.path("operator-aether")), operator_before);
}

/// The registered `archon` server as a setup that predates caller naming
/// left it, written over the instance config.
fn downgrade_registration(env: &Env) {
    let config = env.path("config/aether/config.toml");
    let current = std::fs::read_to_string(&config).unwrap();
    let old = current.replace("send_caller_meta = true\n", "");
    assert_ne!(old, current, "the current setup names callers: {current}");
    std::fs::write(&config, old).unwrap();
}

fn registration_check(env: &Env) -> serde_json::Value {
    let out = env.command(&["--json", "doctor"]).output().unwrap();
    let checks: Vec<serde_json::Value> = serde_json::from_slice(&out.stdout).unwrap();
    let found = checks.into_iter().find(|c| c["check"] == "mcp registered");
    found.expect("doctor checks the registration")
}

/// An install set up before archond needed its callers named, upgraded
/// while its aetherd runs: nothing is asked of the operator.
#[test]
fn an_upgrade_repairs_the_old_registration_and_restarts_archons_aetherd() {
    let env = Env::new();
    env.run(&["setup"]);
    let config = env.path("config/aether/config.toml");
    let daemon = fake_aetherd(
        env.path("aether-calls.log"),
        env.path("run/aetherd.sock"),
        3,
    );
    let mut archond = env.start_archond();
    env.run(&["daemon", "start"]);
    downgrade_registration(&env);
    let found = registration_check(&env);
    assert_eq!(found["level"], "fail", "{found}");
    let detail = found["detail"].as_str().unwrap();
    assert!(detail.contains("send_caller_meta = true"), "{detail}");

    // A write repairs it; a read such as `status` starts and changes nothing.
    let start = env.run(&["daemon", "start"]);
    let told = String::from_utf8_lossy(&start.stderr);
    assert!(told.contains("restarting Archon's own aetherd"), "{told}");
    assert!(
        told.contains("goes on once archond has reconnected"),
        "{told}"
    );
    // The stand-in aetherd never answers archond, so the wait runs out.
    assert!(told.contains("has not connected"), "{told}");
    let repaired = std::fs::read_to_string(&config).unwrap();
    assert!(repaired.contains("send_caller_meta = true"), "{repaired}");
    assert_eq!(registration_check(&env)["level"], "ok");
    let quiet = env.run(&["daemon", "start"]);
    assert!(quiet.stderr.is_empty(), "once: {quiet:?}");

    downgrade_registration(&env);
    let setup = String::from_utf8(env.run(&["setup"]).stdout).unwrap();
    assert!(setup.contains("up to date"), "{setup}");
    assert!(setup.contains("aetherd was restarted"), "{setup}");
    let verbs: Vec<_> = env.calls().into_iter().map(|c| c[0].clone()).collect();
    assert_eq!(verbs, ["daemon", "stop", "daemon", "stop", "daemon"]);

    env.run(&["daemon", "stop", "--aether"]);
    daemon.join().unwrap();
    assert!(archond.0.wait().unwrap().success());
}

#[test]
fn status_observes_an_outage_without_invoking_aether() {
    let env = Env::new();
    let daemon = env.start_archond();
    let out = env.run(&["status"]);
    assert!(String::from_utf8_lossy(&out.stdout).contains("aether: disconnected"));
    let out = env.run(&["--json", "status"]);
    let status: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(status["aether"]["state"], "disconnected");
    assert!(env.calls().is_empty(), "status must not start Aether");
    env.run(&["daemon", "stop"]);
    common::wait_for_exit(daemon);
    let out = env.command(&["status"]).output().unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("aether: disconnected"));
    // Older scripts still pass `--no-start`, which changes nothing now.
    let out = env
        .command(&["--json", "status", "--no-start"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    let value: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(value["error"]["code"], "unavailable");
    assert!(env.calls().is_empty());
    assert!(!env.path("run/archond.sock").exists());
}
