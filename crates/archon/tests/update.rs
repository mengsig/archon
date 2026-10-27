//! `archon update` against a fake release on a local server: what it
//! installs, what it refuses, and that a refusal leaves the binary alone.

#[allow(dead_code, reason = "only the canned hello is used here")]
#[path = "common/tui.rs"]
mod archond;
#[path = "common/release.rs"]
mod release;

use std::os::unix::fs::MetadataExt as _;
use std::os::unix::process::ExitStatusExt as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime};

use archon_core::protocol::{ClientFrame, PROTOCOL_VERSION, Reply, Request, ServerFrame, Welcome};
use archon_core::testing::write_script;
use release::{ARCHIVE_MEMBERS, Server, aether_script, archive_members, archon_script, publish};

const BUILT: &str = env!("CARGO_BIN_EXE_archon");
const CURRENT: &str = env!("CARGO_PKG_VERSION");
/// `archon update --check` exits with this when a newer release exists.
const UPDATE_AVAILABLE: i32 = 10;

/// The release target of the machine the tests run on.
fn target() -> String {
    let os = match std::env::consts::OS {
        "linux" => "unknown-linux-musl",
        "macos" => "apple-darwin",
        other => panic!("no release is built for {other}"),
    };
    format!("{}-{os}", std::env::consts::ARCH)
}

/// An installed archon, a release server, and an `aether` on PATH. Under
/// the build's tmp directory, so installing the binary is a hard link.
struct Installed {
    home: tempfile::TempDir,
    /// In the system's temporary directory: a socket path has to be short.
    runtime: tempfile::TempDir,
    server: Server,
}

impl Installed {
    fn new() -> Self {
        let home = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
        for dir in ["bin", "tools", "releases"] {
            std::fs::create_dir(home.path().join(dir)).unwrap();
        }
        let server = Server::serve(&home.path().join("releases"));
        let installed = Self {
            home,
            runtime: tempfile::tempdir().unwrap(),
            server,
        };
        std::fs::hard_link(BUILT, installed.archon()).unwrap();
        write_script(&installed.path("tools/aether"), &aether_script(16, 17));
        installed
    }

    fn path(&self, name: &str) -> PathBuf {
        self.home.path().join(name)
    }

    fn archon(&self) -> PathBuf {
        self.path("bin/archon")
    }

    fn releases(&self) -> PathBuf {
        self.path("releases")
    }

    /// Nothing listens here unless a test serves it: never a real daemon.
    fn socket(&self) -> PathBuf {
        self.runtime.path().join("archond.sock")
    }

    fn command(&self, program: &Path, args: &[&str]) -> Command {
        let path = std::env::var("PATH").unwrap();
        let mut command = Command::new(program);
        command
            .args(args)
            .env("PATH", format!("{}:{path}", self.path("tools").display()))
            .env("ARCHON_INSTALL_BASE_URL", self.server.url())
            .env("ARCHON_DATA_DIR", self.path("data"))
            .env("ARCHON_CONFIG_DIR", self.path("config"))
            .env("ARCHON_SOCKET", self.socket())
            .env("CARGO_HOME", self.path("cargo"))
            .env("HOME", self.home.path())
            .env_remove("http_proxy")
            .env_remove("HTTP_PROXY")
            .env_remove("AETHER_ENGINE_AGENT")
            .env_remove("AETHER_ENGINE_RUN")
            .env("ARCHON_NO_UPDATE_CHECK", "1")
            .env("NO_COLOR", "1");
        command
    }

    fn update(&self, args: &[&str]) -> Output {
        self.command(&self.archon(), &[&["update"], args].concat())
            .output()
            .expect("running the installed archon")
    }

    /// Whether the installed binary is still the built one.
    fn unchanged(&self) -> bool {
        let installed = std::fs::metadata(self.archon()).unwrap();
        installed.ino() == std::fs::metadata(BUILT).unwrap().ino()
    }

    /// What the installed binary says its version is.
    fn version(&self) -> String {
        let out = Command::new(self.archon())
            .arg("--version")
            .output()
            .unwrap();
        String::from_utf8(out.stdout).unwrap().trim().to_owned()
    }

    /// Everything in the install directory: an update leaves only binaries.
    fn installed_files(&self) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(self.path("bin"))
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        names
    }
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// The archive in the update's staging directory, once the server was asked
/// for it: the download is under way, and the server never finishes it.
fn wait_for_partial_archive(installed: &Installed) -> PathBuf {
    let deadline = Instant::now() + Duration::from_secs(60);
    let archive = format!("archon-{}.tar.gz", target());
    loop {
        let asked = installed.server.requests();
        let partial = std::fs::read_dir(installed.path("bin"))
            .unwrap()
            .map(|entry| entry.unwrap().path().join(&archive))
            .find(|path| path.is_file());
        if let Some(partial) = partial.filter(|_| asked.iter().any(|path| path.ends_with(&archive)))
        {
            return partial;
        }
        assert!(Instant::now() < deadline, "no download started: {asked:?}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Start `archon update`, send it `signal` in the middle of the archive's
/// download, and return how it ended.
fn update_interrupted_by(installed: &Installed, signal: &str) -> Output {
    installed.server.stall(".tar.gz");
    let update = installed
        .command(&installed.archon(), &["update"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("starting the installed archon");
    let partial = wait_for_partial_archive(installed);
    let staging = partial.parent().unwrap().file_name().unwrap();
    assert!(
        staging.to_str().unwrap().starts_with(".archon-update."),
        "{partial:?}"
    );
    end_with(update, signal)
}

/// Send `signal` to a running update and return how it ended.
fn end_with(update: Child, signal: &str) -> Output {
    let sent = Command::new("kill")
        .args([format!("-{signal}"), update.id().to_string()])
        .status()
        .unwrap();
    assert!(sent.success(), "kill -{signal} failed: {sent}");
    update.wait_with_output().unwrap()
}

/// The output of `child`, which must exit within `limit`; killed otherwise.
fn exit_within(mut child: Child, limit: Duration) -> Output {
    let deadline = Instant::now() + limit;
    while child.try_wait().unwrap().is_none() {
        if Instant::now() >= deadline {
            let _already_gone = child.kill();
            let out = child.wait_with_output().unwrap();
            panic!("still running after {limit:?}: {out:?}");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    child.wait_with_output().unwrap()
}

/// A stand-in `aether` that creates `asked` and then never answers.
fn silent_aether_script(asked: &Path) -> String {
    let asked = asked.to_str().unwrap();
    assert!(!asked.contains('\''), "{asked} cannot be quoted for sh");
    format!("#!/bin/sh\n: > '{asked}'\nexec sleep 600\n")
}

#[test]
fn check_exits_10_for_a_newer_release_and_0_when_up_to_date() {
    let installed = Installed::new();
    publish(
        &installed.releases(),
        "v9.9.9",
        &target(),
        &archon_script("9.9.9"),
        true,
    );
    let out = installed.update(&["--check"]);
    assert_eq!(out.status.code(), Some(UPDATE_AVAILABLE), "{out:?}");
    let expected = format!("archon 9.9.9 is available; this is {CURRENT}. Run `archon update`.");
    assert_eq!(text(&out.stdout).trim(), expected);
    assert!(installed.unchanged(), "--check installs nothing");
    assert_eq!(installed.server.requests(), ["/latest/download/VERSION"]);

    let json = installed.update(&["--check", "--json"]);
    assert_eq!(json.status.code(), Some(UPDATE_AVAILABLE), "{json:?}");
    let value: serde_json::Value = serde_json::from_slice(&json.stdout).unwrap();
    assert_eq!(value["status"], "update_available");
    assert_eq!(value["release"], "9.9.9");

    std::fs::write(
        installed.path("releases/latest/download/VERSION"),
        format!("v{CURRENT}\n"),
    )
    .unwrap();
    let out = installed.update(&["--check"]);
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    assert_eq!(
        text(&out.stdout).trim(),
        format!("archon {CURRENT} is up to date.")
    );
    let out = installed.update(&[]);
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    assert!(installed.unchanged(), "an up-to-date archon is left alone");
    assert_eq!(installed.installed_files(), ["archon"]);
}

#[test]
fn update_replaces_the_binary_with_the_latest_release() {
    let installed = Installed::new();
    publish(
        &installed.releases(),
        "v9.9.9",
        &target(),
        &archon_script("9.9.9"),
        true,
    );
    let out = installed.update(&[]);
    let stdout = text(&out.stdout);
    assert!(out.status.success(), "{out:?}");
    assert_eq!(installed.version(), "archon 9.9.9");
    assert!(!installed.unchanged());
    assert_eq!(
        installed.installed_files(),
        ["archon"],
        "staging is removed"
    );
    assert!(
        stdout.contains(&format!("Updated archon {CURRENT} to 9.9.9.")),
        "{stdout}"
    );
    assert!(
        stdout.contains("archond is not running; `archon daemon start`, or the next command that needs it, starts 9.9.9."),
        "{stdout}"
    );
    assert!(
        stdout.contains(
            "Aether: aether 0.9.0 speaks protocol 16 to 17; archon 9.9.9 needs protocol 14 to \
             17: compatible."
        ),
        "{stdout}"
    );
    let archive = format!("/download/v9.9.9/archon-{}.tar.gz", target());
    assert_eq!(
        installed.server.requests(),
        [
            "/latest/download/VERSION".to_owned(),
            archive.clone(),
            format!("{archive}.sha256")
        ]
    );
    assert!(
        !installed.socket().exists() && !installed.path("data").exists(),
        "an update starts no daemon and writes no data"
    );
}

#[test]
fn a_running_archond_and_an_old_aether_are_named_with_what_to_run() {
    let installed = Installed::new();
    publish(
        &installed.releases(),
        "v9.9.9",
        &target(),
        &archon_script("9.9.9"),
        true,
    );
    write_script(&installed.path("tools/aether"), &aether_script(12, 13));
    let daemon = archond::serve(&installed.socket(), 1, Vec::new());
    let out = installed.update(&["--json"]);
    assert!(out.status.success(), "{out:?}");
    let value: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(value["status"], "updated");
    assert_eq!(value["archond_running"], true);
    assert_eq!(value["aether_compatible"], false);
    assert_eq!(daemon.join().unwrap().len(), 1, "only the hello was sent");

    // The same again as text, from the release that is installed now.
    let installed = Installed::new();
    publish(
        &installed.releases(),
        "v9.9.9",
        &target(),
        &archon_script("9.9.9"),
        true,
    );
    write_script(&installed.path("tools/aether"), &aether_script(12, 13));
    let daemon = archond::serve(&installed.socket(), 1, Vec::new());
    let stdout = text(&installed.update(&[]).stdout);
    daemon.join().unwrap();
    let pid = std::process::id();
    assert!(
        stdout.contains(&format!(
            "archond (pid {pid}) still runs test, and the update itself interrupts no running \
             item; to switch it to 9.9.9, run `archon daemon stop --keep-aether`, then `archon \
             aether mcp refresh archon` and `archon orchestrator restart`."
        )),
        "{stdout}"
    );
    assert!(
        stdout.contains(
            "Aether: aether 0.9.0 speaks protocol 12 to 13, too old: archon 9.9.9 needs \
             protocol 14 to 17. Run `aether update`."
        ),
        "{stdout}"
    );
}

#[test]
fn yes_updates_aether_and_restarts_nothing_when_archond_is_not_running() {
    let installed = Installed::new();
    publish(
        &installed.releases(),
        "v9.9.9",
        &target(),
        &archon_script("9.9.9"),
        true,
    );
    let updating = aether_script(16, 17).replace("*) exit 64", "update) echo done ;;\n*) exit 64");
    write_script(&installed.path("tools/aether"), &updating);
    let out = installed.update(&["--yes"]);
    let stdout = text(&out.stdout);
    assert!(out.status.success(), "{out:?}");
    assert_eq!(installed.version(), "archon 9.9.9");
    assert!(
        stdout.contains("Aether is aether 0.9.0; `aether update` changed nothing."),
        "{stdout}"
    );
    assert!(
        stdout.contains("archond is not running; nothing to restart."),
        "{stdout}"
    );
}

#[test]
fn yes_with_json_prints_one_object_holding_the_install_and_the_restart() {
    let installed = Installed::new();
    publish(
        &installed.releases(),
        "v9.9.9",
        &target(),
        &archon_script("9.9.9"),
        true,
    );
    let updating = aether_script(16, 17).replace("*) exit 64", "update) echo done ;;\n*) exit 64");
    write_script(&installed.path("tools/aether"), &updating);
    let out = installed.update(&["--yes", "--json"]);
    assert!(out.status.success(), "{out:?}");
    let value: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(value["status"], "updated");
    assert_eq!(value["restart"]["restarted"], false);
    assert!(
        value["restart"]["steps"]
            .to_string()
            .contains("nothing to restart"),
        "{value}"
    );
}

/// An archond that welcomes every connection and answers nothing else, as
/// one stuck in its restart would; `true` once it was asked to restart.
fn serve_stuck_archond(socket: &Path) -> Arc<AtomicBool> {
    use std::io::{BufRead as _, BufReader, Write as _};
    let listener = std::os::unix::net::UnixListener::bind(socket).unwrap();
    let asked = Arc::new(AtomicBool::new(false));
    let seen = asked.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let (mut writer, seen) = (stream.unwrap(), seen.clone());
            std::thread::spawn(move || {
                let reader = BufReader::new(writer.try_clone().unwrap());
                for line in reader.lines().map_while(Result::ok) {
                    let frame: ClientFrame = serde_json::from_str(&line).unwrap();
                    if matches!(frame.request, Request::Upgrade(_)) {
                        seen.store(true, Ordering::SeqCst);
                    }
                    let Request::Hello { .. } = frame.request else {
                        continue;
                    };
                    let welcome = Reply::Welcome(Welcome {
                        protocol_version: PROTOCOL_VERSION,
                        daemon_version: "test".into(),
                        pid: std::process::id(),
                    });
                    let reply = ServerFrame::Reply {
                        id: frame.id,
                        result: Ok(welcome),
                    };
                    writeln!(writer, "{}", serde_json::to_string(&reply).unwrap()).unwrap();
                }
            });
        }
    });
    asked
}

#[test]
fn yes_prints_no_manual_steps_and_a_signal_ends_a_stuck_restart_naming_its_step() {
    let installed = Installed::new();
    publish(
        &installed.releases(),
        "v9.9.9",
        &target(),
        &archon_script("9.9.9"),
        true,
    );
    let updating = aether_script(16, 17).replace("*) exit 64", "update) echo done ;;\n*) exit 64");
    write_script(&installed.path("tools/aether"), &updating);
    let asked = serve_stuck_archond(&installed.socket());
    let update = installed
        .command(&installed.archon(), &["update", "--yes"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("starting the installed archon");
    let deadline = Instant::now() + Duration::from_secs(60);
    while !asked.load(Ordering::SeqCst) {
        assert!(
            Instant::now() < deadline,
            "archond was never asked to restart"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    let sent = Instant::now();
    let signalled = Command::new("kill")
        .args(["-INT", &update.id().to_string()])
        .status()
        .unwrap();
    assert!(signalled.success(), "kill -INT failed: {signalled}");
    // Within a second, with slack for a loaded machine.
    let out = exit_within(update, Duration::from_secs(5));
    assert!(
        sent.elapsed() < Duration::from_secs(2),
        "{:?}",
        sent.elapsed()
    );
    let (stdout, stderr) = (text(&out.stdout), text(&out.stderr));
    assert_eq!(out.status.code(), Some(130), "{stdout}\n{stderr}");
    assert_eq!(installed.version(), "archon 9.9.9", "the install stays");
    assert!(
        stderr.contains("archon: update: restarting archond (pid")
            && stderr.contains("update interrupted by SIGINT while restarting archond"),
        "{stderr}"
    );
    assert!(stderr.contains("`archon orchestrator restart`"), "{stderr}");
    assert!(
        stdout.contains("to switch it to 9.9.9, `--yes` restarts it now."),
        "{stdout}"
    );
    assert!(!stdout.contains("daemon stop --keep-aether"), "{stdout}");
}

#[test]
fn a_failed_aether_update_is_a_warning_and_a_failing_exit() {
    let installed = Installed::new();
    publish(
        &installed.releases(),
        "v9.9.9",
        &target(),
        &archon_script("9.9.9"),
        true,
    );
    let out = installed.update(&["--yes"]);
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    assert_eq!(installed.version(), "archon 9.9.9", "archon stays updated");
    let stderr = text(&out.stderr);
    assert!(
        stderr.contains("warning: aether was not updated"),
        "{stderr}"
    );
}

#[test]
fn a_pinned_older_release_is_installed_as_a_downgrade() {
    let installed = Installed::new();
    publish(
        &installed.releases(),
        "v0.9.0",
        &target(),
        &archon_script("0.9.0"),
        false,
    );
    let out = installed.update(&["--version", "0.9.0"]);
    let stdout = text(&out.stdout);
    assert!(out.status.success(), "{out:?}");
    assert!(
        stdout.contains(&format!("Downgrading archon {CURRENT} to 0.9.0 in ")),
        "{stdout}"
    );
    assert!(
        stdout.contains(&format!("Downgraded archon {CURRENT} to 0.9.0.")),
        "{stdout}"
    );
    assert_eq!(installed.version(), "archon 0.9.0");
    let requests = installed.server.requests();
    assert!(
        requests
            .iter()
            .all(|path| path.starts_with("/download/v0.9.0/")),
        "a pinned version asks for nothing else: {requests:?}"
    );
}

#[test]
fn only_archon_is_installed_from_an_archive_that_carries_the_licence_files() {
    let installed = Installed::new();
    let archive = publish(
        &installed.releases(),
        "v9.9.9",
        &target(),
        &archon_script("9.9.9"),
        true,
    );
    assert_eq!(archive_members(&archive), ARCHIVE_MEMBERS);
    let out = installed.update(&[]);
    assert!(out.status.success(), "{out:?}");
    assert_eq!(installed.version(), "archon 9.9.9");
    assert_eq!(
        installed.installed_files(),
        ["archon"],
        "the licence files stay in the archive"
    );
}

#[test]
fn a_checksum_mismatch_is_refused_and_changes_nothing() {
    let installed = Installed::new();
    let archive = publish(
        &installed.releases(),
        "v9.9.9",
        &target(),
        &archon_script("9.9.9"),
        true,
    );
    let name = archive.file_name().unwrap().to_str().unwrap().to_owned();
    let wrong = format!("{}  {name}\n", "0".repeat(64));
    std::fs::write(archive.with_file_name(format!("{name}.sha256")), wrong).unwrap();
    let out = installed.update(&[]);
    let stderr = text(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    assert!(stderr.contains("checksum mismatch"), "{stderr}");
    assert!(stderr.contains(&"0".repeat(64)), "{stderr}");
    assert!(
        stderr.contains(&format!(
            "archon {CURRENT} is unchanged; installing 9.9.9 failed"
        )),
        "{stderr}"
    );
    assert!(installed.unchanged());
    assert_eq!(
        installed.installed_files(),
        ["archon"],
        "staging is removed"
    );
}

#[test]
fn an_update_interrupted_in_a_download_leaves_only_the_installed_archon() {
    for (signal, code) in [("TERM", 143), ("INT", 130), ("HUP", 129)] {
        let installed = Installed::new();
        publish(
            &installed.releases(),
            "v9.9.9",
            &target(),
            &archon_script("9.9.9"),
            true,
        );
        let out = update_interrupted_by(&installed, signal);
        let stderr = text(&out.stderr);
        assert_eq!(out.status.code(), Some(code), "SIG{signal}: {out:?}");
        assert_eq!(
            installed.installed_files(),
            ["archon"],
            "SIG{signal}: staging is removed"
        );
        assert!(
            installed.unchanged(),
            "SIG{signal}: the archon is untouched"
        );
        assert!(
            stderr.contains(&format!(
                "archon: update interrupted by SIG{signal}; nothing staged"
            )),
            "{stderr}"
        );
    }
}

#[test]
fn an_update_interrupted_while_it_asks_aether_has_replaced_nothing() {
    let installed = Installed::new();
    publish(
        &installed.releases(),
        "v9.9.9",
        &target(),
        &archon_script("9.9.9"),
        true,
    );
    let asked = installed.path("aether-asked");
    write_script(
        &installed.path("tools/aether"),
        &silent_aether_script(&asked),
    );
    let update = installed
        .command(&installed.archon(), &["update"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("starting the installed archon");
    let deadline = Instant::now() + Duration::from_secs(60);
    while !asked.exists() {
        assert!(Instant::now() < deadline, "aether was never asked");
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(installed.unchanged(), "aether is asked before the swap");

    let out = end_with(update, "TERM");
    let (stdout, stderr) = (text(&out.stdout), text(&out.stderr));
    assert_eq!(out.status.code(), Some(143), "{out:?}");
    assert!(
        installed.unchanged(),
        "an interrupted update replaces nothing"
    );
    assert_eq!(installed.version(), format!("archon {CURRENT}"));
    assert_eq!(
        installed.installed_files(),
        ["archon"],
        "staging is removed"
    );
    assert!(
        stderr.contains("archon: update interrupted by SIGTERM; nothing staged"),
        "{stderr}"
    );
    assert!(!stdout.contains("Updated archon"), "{stdout}");
}

#[test]
fn a_killed_update_leaves_staging_that_a_later_update_removes() {
    let installed = Installed::new();
    publish(
        &installed.releases(),
        "v9.9.9",
        &target(),
        &archon_script("9.9.9"),
        true,
    );
    // SIGKILL cannot be handled: the staging directory is left behind.
    let out = update_interrupted_by(&installed, "KILL");
    assert_eq!(out.status.signal(), Some(9), "{out:?}");
    let left = installed.installed_files();
    assert_eq!(left.len(), 2, "{left:?}");
    assert!(left[0].starts_with(".archon-update."), "{left:?}");
    assert!(installed.unchanged());

    // An update soon after keeps it: it might belong to one still running.
    let soon = installed.update(&["--version", "v7.7.7"]);
    assert_eq!(soon.status.code(), Some(1), "{soon:?}");
    assert_eq!(installed.installed_files(), left);

    // Two hours on it is stale, and any update removes it, even one that
    // then installs nothing.
    let stale = std::fs::File::open(installed.path("bin").join(&left[0])).unwrap();
    let two_hours = Duration::from_secs(2 * 60 * 60);
    stale.set_modified(SystemTime::now() - two_hours).unwrap();
    let later = installed.update(&["--version", "v7.7.7"]);
    assert_eq!(later.status.code(), Some(1), "{later:?}");
    assert_eq!(installed.installed_files(), ["archon"]);
    assert!(installed.unchanged());
}

#[test]
fn a_release_that_cannot_be_used_changes_nothing() {
    let installed = Installed::new();
    // Tagged 9.9.9, but the binary inside says otherwise.
    publish(
        &installed.releases(),
        "v9.9.9",
        &target(),
        &archon_script("9.9.8"),
        true,
    );
    let out = installed.update(&[]);
    let stderr = text(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    assert!(
        stderr.contains("reports version 9.9.8, not 9.9.9"),
        "{stderr}"
    );
    assert!(installed.unchanged());

    let missing = installed.update(&["--version", "v7.7.7"]);
    let stderr = text(&missing.stderr);
    assert_eq!(missing.status.code(), Some(1), "{missing:?}");
    assert!(stderr.contains("/download/v7.7.7/"), "{stderr}");
    assert!(
        stderr.contains(&format!("archon {CURRENT} is unchanged")),
        "{stderr}"
    );

    std::fs::write(
        installed.path("releases/latest/download/VERSION"),
        "<html>not a version</html>\n",
    )
    .unwrap();
    let garbled = installed.update(&[]);
    let stderr = text(&garbled.stderr);
    assert_eq!(garbled.status.code(), Some(1), "{garbled:?}");
    assert!(stderr.contains("does not name a release"), "{stderr}");
    assert!(installed.unchanged());
    assert_eq!(installed.installed_files(), ["archon"]);
}

#[test]
fn an_archon_outside_an_installer_directory_is_refused_before_any_request() {
    let installed = Installed::new();
    publish(
        &installed.releases(),
        "v9.9.9",
        &target(),
        &archon_script("9.9.9"),
        true,
    );
    // The built binary itself: a cargo build directory.
    let out = installed
        .command(Path::new(BUILT), &["update"])
        .output()
        .unwrap();
    let stderr = text(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    assert!(
        stderr.contains("`archon update` does not replace"),
        "{stderr}"
    );
    assert!(stderr.contains("cargo build --release"), "{stderr}");
    assert!(
        stderr.contains(
            "curl -fsSL https://raw.githubusercontent.com/mengsig/archon/main/install.sh | sh"
        ),
        "{stderr}"
    );

    // The same binary where `cargo install` puts it.
    let cargo_bin = installed.path("cargo/bin");
    std::fs::create_dir_all(&cargo_bin).unwrap();
    std::fs::hard_link(BUILT, cargo_bin.join("archon")).unwrap();
    let out = installed
        .command(&cargo_bin.join("archon"), &["update"])
        .output()
        .unwrap();
    let stderr = text(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    assert!(stderr.contains("was installed by cargo"), "{stderr}");
    assert!(
        stderr.contains("cargo install --locked --path crates/archon"),
        "{stderr}"
    );
    assert_eq!(
        installed.server.requests(),
        Vec::<String>::new(),
        "a refusal asks the network nothing"
    );

    // Reporting is allowed anywhere.
    let check = installed
        .command(Path::new(BUILT), &["update", "--check"])
        .output()
        .unwrap();
    assert_eq!(check.status.code(), Some(UPDATE_AVAILABLE), "{check:?}");
}

#[test]
fn requirements_print_the_aether_protocols_this_build_speaks() {
    let installed = Installed::new();
    let out = installed.update(&["--requirements"]);
    assert!(out.status.success(), "{out:?}");
    let (min, max) = (
        archon_aether::wire::MIN_PROTOCOL_VERSION,
        archon_aether::wire::PROTOCOL_VERSION,
    );
    assert_eq!(
        text(&out.stdout),
        format!("aether_protocol_min {min}\naether_protocol_max {max}\n")
    );
    assert!(installed.server.requests().is_empty());
}
