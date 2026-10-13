use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::thread::JoinHandle;

use super::*;

/// A fake `aether` that logs its verb and environment, one line per call.
struct Shim {
    _root: tempfile::TempDir,
    program: PathBuf,
    log: PathBuf,
    instance: Instance,
}

fn shim(body: &str) -> Shim {
    let root = tempfile::tempdir().unwrap();
    let base = root.path().canonicalize().unwrap();
    let log = base.join("calls.log");
    let program = base.join("aether");
    let script = format!(
        "#!/bin/sh\nprintf '%s|%s|%s|%s|%s|%s|%s|%s\\n' \"$*\" \"$AETHER_CONFIG_DIR\" \
         \"$AETHER_DATA_DIR\" \"$AETHER_SOCKET\" \"$ARCHON_CONFIG_DIR\" \"$ARCHON_DATA_DIR\" \
         \"$ARCHON_SOCKET\" \"$(pwd -P)\" >> '{}'\n{body}\n",
        log.display()
    );
    archon_core::testing::write_script(&program, &script);
    let instance = Instance::from_archon_dirs(
        base.join("config"),
        base.join("data"),
        base.join("run").join("archond.sock"),
    );
    std::fs::create_dir_all(&instance.config_dir).unwrap();
    std::fs::write(instance.config_path(), "").unwrap();
    Shim {
        _root: root,
        program,
        log,
        instance,
    }
}

impl Shim {
    fn calls(&self) -> Vec<Vec<String>> {
        let text = std::fs::read_to_string(&self.log).unwrap_or_default();
        text.lines()
            .map(|line| line.split('|').map(str::to_owned).collect())
            .collect()
    }
}

/// Stands in for the daemon the shim "starts": listens while the log shows
/// more `daemon` than `stop` calls, and ends after `lives` stops.
fn fake_aetherd(log: PathBuf, socket: PathBuf, lives: usize) -> JoinHandle<()> {
    assert!(lives > 0, "a daemon that is never stopped never ends");
    std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(30);
        let mut listener = None;
        while Instant::now() < deadline {
            let text = std::fs::read_to_string(&log).unwrap_or_default();
            let count = |verb: &str| text.lines().filter(|l| l.starts_with(verb)).count();
            let (starts, stops) = (count("daemon|"), count("stop|"));
            if listener.is_none() && starts > stops {
                std::fs::create_dir_all(socket.parent().unwrap()).unwrap();
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

#[test]
fn start_and_stop_call_aether_with_the_instance_environment() {
    let shim = shim("sleep 0.2");
    let daemon = fake_aetherd(shim.log.clone(), shim.instance.socket.clone(), 1);
    let started = ensure_running(&shim.instance, &shim.program).unwrap();
    assert_eq!(started, Start::Started);
    assert!(is_listening(&shim.instance.socket));
    assert_eq!(
        ensure_running(&shim.instance, &shim.program).unwrap(),
        Start::AlreadyRunning
    );
    assert_eq!(stop(&shim.instance, &shim.program).unwrap(), Stop::Stopped);
    assert_eq!(
        stop(&shim.instance, &shim.program).unwrap(),
        Stop::NotRunning
    );
    daemon.join().unwrap();

    let calls = shim.calls();
    let verbs: Vec<_> = calls.iter().map(|c| c[0].as_str()).collect();
    assert_eq!(verbs, ["daemon", "stop"], "one start, one stop");
    let i = &shim.instance;
    for call in &calls {
        let expected = [
            &i.config_dir,
            &i.data_dir,
            &i.socket,
            &i.archon_config_dir,
            &i.archon_data_dir,
            &i.archon_socket,
            &i.data_dir,
        ];
        let got: Vec<_> = call[1..].iter().map(PathBuf::from).collect();
        let expected: Vec<_> = expected.into_iter().cloned().collect();
        assert_eq!(got, expected, "env and working directory of {:?}", call[0]);
    }
}

#[test]
fn concurrent_starts_launch_one_daemon() {
    let shim = shim("sleep 0.3");
    let daemon = fake_aetherd(shim.log.clone(), shim.instance.socket.clone(), 1);
    let results: Vec<Start> = std::thread::scope(|scope| {
        let starts: Vec<_> = (0..4)
            .map(|_| scope.spawn(|| ensure_running(&shim.instance, &shim.program).unwrap()))
            .collect();
        starts.into_iter().map(|h| h.join().unwrap()).collect()
    });
    assert_eq!(shim.calls().len(), 1, "the lock let one start through");
    let started = results.iter().filter(|r| **r == Start::Started).count();
    assert_eq!(started, 1, "{results:?}");
    stop(&shim.instance, &shim.program).unwrap();
    daemon.join().unwrap();
}

#[test]
fn a_failed_start_carries_aethers_words() {
    let shim = shim("echo 'no providers configured' >&2; exit 2");
    let error = ensure_running(&shim.instance, &shim.program).unwrap_err();
    let text = format!("{error:#}");
    assert!(text.contains("no providers configured"), "{text}");
    assert!(text.contains("exit status: 2"), "{text}");
    assert!(!is_listening(&shim.instance.socket));
    assert_eq!(shim.calls().len(), 1, "only a held store is retried");
}

#[test]
fn an_instance_without_setup_is_never_started() {
    let shim = shim("exit 0");
    std::fs::remove_file(shim.instance.config_path()).unwrap();
    let error = ensure_running(&shim.instance, &shim.program).unwrap_err();
    assert!(error.to_string().contains("archon setup"), "{error}");
    assert!(shim.calls().is_empty(), "aether was not run");
}

/// The registration an `archon setup` wrote before archond needed callers named.
const OLD_REGISTRATION: &str =
    "[[mcp.servers]]\nname = \"archon\"\ncommand = \"/opt/archon\"\nargs = [\"mcp\"]\n";

fn names_callers(instance: &Instance) -> bool {
    let text = std::fs::read_to_string(instance.config_path()).unwrap();
    let server = crate::setup::find_archon_server(&text).unwrap();
    server.is_some_and(|server| server.names_callers)
}

#[test]
fn an_old_registration_is_repaired_before_the_first_start() {
    let shim = shim("exit 0");
    std::fs::write(shim.instance.config_path(), OLD_REGISTRATION).unwrap();
    let daemon = fake_aetherd(shim.log.clone(), shim.instance.socket.clone(), 1);
    let started = ensure_running(&shim.instance, &shim.program).unwrap();
    assert_eq!(started, Start::Started, "nothing ran with the old one");
    assert!(names_callers(&shim.instance));
    assert!(!seed::reload_is_owed(&shim.instance).unwrap());
    stop(&shim.instance, &shim.program).unwrap();
    daemon.join().unwrap();
    let verbs: Vec<_> = shim.calls().into_iter().map(|c| c[0].clone()).collect();
    assert_eq!(verbs, ["daemon", "stop"]);
}

/// The upgrade: aetherd runs with what an older setup registered. The next
/// archon command repairs the file and restarts aetherd, which alone loads it.
#[test]
fn a_running_aetherd_with_an_old_registration_is_restarted_once() {
    let shim = shim("exit 0");
    let daemon = fake_aetherd(shim.log.clone(), shim.instance.socket.clone(), 2);
    assert_eq!(
        ensure_running(&shim.instance, &shim.program).unwrap(),
        Start::Started
    );
    std::fs::write(shim.instance.config_path(), OLD_REGISTRATION).unwrap();

    let restarted = ensure_running(&shim.instance, &shim.program).unwrap();
    assert_eq!(restarted, Start::Restarted);
    assert!(names_callers(&shim.instance));
    assert!(is_listening(&shim.instance.socket));
    assert_eq!(
        ensure_running(&shim.instance, &shim.program).unwrap(),
        Start::AlreadyRunning,
        "the restart loaded it"
    );
    stop(&shim.instance, &shim.program).unwrap();
    daemon.join().unwrap();
    let verbs: Vec<_> = shim.calls().into_iter().map(|c| c[0].clone()).collect();
    assert_eq!(verbs, ["daemon", "stop", "daemon", "stop"]);
}

/// `archon setup` already rewrote the file (or a repair was cut short
/// before its restart): the file is current, the daemon is not.
#[test]
fn a_reload_owed_from_an_earlier_command_restarts_the_running_aetherd() {
    let shim = shim("exit 0");
    let daemon = fake_aetherd(shim.log.clone(), shim.instance.socket.clone(), 2);
    ensure_running(&shim.instance, &shim.program).unwrap();
    std::fs::write(shim.instance.config_path(), OLD_REGISTRATION).unwrap();
    assert!(seed::repair_registration(&shim.instance).unwrap());
    assert!(seed::reload_is_owed(&shim.instance).unwrap());

    let restarted = ensure_running(&shim.instance, &shim.program).unwrap();
    assert_eq!(restarted, Start::Restarted);
    assert!(!seed::reload_is_owed(&shim.instance).unwrap());
    stop(&shim.instance, &shim.program).unwrap();
    daemon.join().unwrap();
}

/// `aether stop` returns once the socket is closed; the old aetherd lets go
/// of its store a moment later, and a start in between is refused.
#[test]
fn a_start_that_races_the_stopped_aetherd_is_retried_until_the_store_is_free() {
    let shim = shim(
        "if [ \"$(grep -c '^daemon|' \"$(dirname \"$0\")/calls.log\")\" -le 2 ]; then\n\
         echo 'aetherd exited (exit status: 1) before it listened: Error: another aether \
         daemon is already running for /data/aether/aether.db' >&2\nexit 1\nfi",
    );
    let daemon = fake_aetherd(shim.log.clone(), shim.instance.socket.clone(), 1);
    let started = ensure_running(&shim.instance, &shim.program).unwrap();
    assert_eq!(started, Start::Started);
    let verbs: Vec<_> = shim.calls().into_iter().map(|c| c[0].clone()).collect();
    assert_eq!(verbs, ["daemon", "daemon", "daemon"], "two refusals");
    stop(&shim.instance, &shim.program).unwrap();
    daemon.join().unwrap();
}
