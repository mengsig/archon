//! `install.sh` piped to a POSIX shell (dash where there is one), against a
//! fake release and Aether installer on a local server, with a minimal PATH.

#[path = "common/release.rs"]
mod release;

use std::ffi::OsString;
use std::fs::File;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::OnceLock;

use archon_core::testing::write_script;
use release::{
    ARCHIVE_MEMBERS, Server, aether_script, archive_members, archon_script, publish, tool_dir,
};

const INSTALLER: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../install.sh");
/// Everything the installer runs besides its shell, a downloader and a
/// SHA-256 tool; `cat` is for the stand-in programs.
const TOOLS: &[&str] = &[
    "uname", "mktemp", "mkdir", "rm", "mv", "chmod", "tar", "gzip", "cut", "head", "tr", "sed",
    "grep", "cat",
];
const TARGETS: [&str; 4] = [
    "x86_64-unknown-linux-musl",
    "aarch64-unknown-linux-musl",
    "x86_64-apple-darwin",
    "aarch64-apple-darwin",
];
const EXIT_FAILED: i32 = 1;
const EXIT_AETHER_FAILED: i32 = 2;

fn on_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
}

/// The shell under test: `ARCHON_TEST_SH`, else dash, else the system's sh.
/// Its first use says which one that is, so a fallback to sh is seen.
fn shell() -> PathBuf {
    static SHELL: OnceLock<PathBuf> = OnceLock::new();
    SHELL
        .get_or_init(|| {
            let (shell, why) = choose_shell(std::env::var_os("ARCHON_TEST_SH"), on_path);
            // Not `eprintln!`: the harness hides what a passing test prints.
            writeln!(
                std::io::stderr(),
                "install.sh is tested with {} ({why})",
                shell.display()
            )
            .expect("writing to stderr");
            shell
        })
        .clone()
}

/// The shell to test with and why: `requested` when set, else the first of
/// dash and sh that `find` locates.
fn choose_shell(
    requested: Option<OsString>,
    find: impl Fn(&str) -> Option<PathBuf>,
) -> (PathBuf, &'static str) {
    if let Some(shell) = requested {
        return (PathBuf::from(shell), "set by ARCHON_TEST_SH");
    }
    if let Some(dash) = find("dash") {
        return (dash, "dash");
    }
    let sh = find("sh").expect("a POSIX shell on PATH");
    (sh, "the system's sh: no dash on PATH")
}

/// A machine to install on: an empty home, a PATH of chosen programs, and
/// a server with release 9.9.9 for every target and an Aether installer.
struct Machine {
    root: tempfile::TempDir,
    server: Server,
    path: Vec<PathBuf>,
    env: Vec<(String, String)>,
}

impl Machine {
    fn new() -> Self {
        Self::with_downloaders(&["curl"], &["sha256sum", "shasum"])
    }

    /// Only the first of `sums` that exists is linked, as on a real machine.
    fn with_downloaders(downloaders: &[&str], sums: &[&str]) -> Self {
        let root = tempfile::tempdir().unwrap();
        for dir in ["home", "releases/aether"] {
            std::fs::create_dir_all(root.path().join(dir)).unwrap();
        }
        let tools = root.path().join("tools");
        let absent = tool_dir(&tools, TOOLS);
        assert!(absent.is_empty(), "the tests need {absent:?} on PATH");
        tool_dir(&tools, downloaders);
        if let Some(sum) = sums.iter().find(|name| on_path(name).is_some()) {
            tool_dir(&tools, &[sum]);
        }
        std::os::unix::fs::symlink(shell(), tools.join("sh")).unwrap();
        let releases = root.path().join("releases");
        for target in TARGETS {
            publish(&releases, "v9.9.9", target, &archon_script("9.9.9"), true);
        }
        let server = Server::serve(&releases);
        let machine = Self {
            root,
            server,
            path: vec![tools],
            env: Vec::new(),
        };
        machine.aether_installer("");
        machine
    }

    fn path(&self, name: &str) -> PathBuf {
        self.root.path().join(name)
    }

    /// Where the installer puts binaries when nothing says otherwise.
    fn bin(&self) -> PathBuf {
        self.path("home/.local/bin")
    }

    /// Serve an Aether installer that installs an aether serving protocols
    /// 16 to 17 into `$AETHER_INSTALL_DIR`, after running `first`.
    fn aether_installer(&self, first: &str) {
        let script = format!(
            "#!/bin/sh\nset -eu\n{first}\ncat > \"$AETHER_INSTALL_DIR/aether\" <<'AETHER'\n{}AETHER\n\
             chmod 755 \"$AETHER_INSTALL_DIR/aether\"\n\
             echo \"fake aether installer: $AETHER_INSTALL_DIR\"\n",
            aether_script(16, 17)
        );
        std::fs::write(self.path("releases/aether/install.sh"), script).unwrap();
    }

    fn aether_installer_url(&self) -> String {
        format!("{}/aether/install.sh", self.server.url())
    }

    /// A directory of stand-in programs, put first on PATH.
    fn programs(&mut self, programs: &[(&str, &str)]) -> PathBuf {
        let dir = self.path(&format!("programs-{}", self.path.len()));
        std::fs::create_dir(&dir).unwrap();
        for (name, script) in programs {
            write_script(&dir.join(name), script);
        }
        self.path.insert(0, dir.clone());
        dir
    }

    fn with_aether(mut self, min: u32, max: u32) -> Self {
        self.programs(&[("aether", &aether_script(min, max))]);
        self
    }

    fn env(mut self, name: &str, value: &str) -> Self {
        self.env.push((name.to_owned(), value.to_owned()));
        self
    }

    /// Run `script` piped to the shell, in this machine's environment only.
    fn pipe(&self, script: &Path) -> Output {
        let path = std::env::join_paths(&self.path).unwrap();
        let mut command = Command::new(shell());
        command
            .env_clear()
            .env("PATH", path)
            .env("HOME", self.path("home"))
            .env("ARCHON_INSTALL_BASE_URL", self.server.url())
            .env("ARCHON_AETHER_INSTALL_URL", self.aether_installer_url())
            .env("ARCHON_NONINTERACTIVE", "1")
            .envs(self.env.iter().map(|(name, value)| (name, value)))
            .current_dir(self.root.path())
            .stdin(Stdio::from(File::open(script).unwrap()));
        command.output().expect("running the installer")
    }

    fn install(&self) -> Output {
        self.pipe(Path::new(INSTALLER))
    }

    /// Every file under the home directory, relative to it and sorted.
    fn home_files(&self) -> Vec<String> {
        let home = self.path("home");
        let mut files = Vec::new();
        let mut pending = vec![home.clone()];
        while let Some(dir) = pending.pop() {
            for entry in std::fs::read_dir(&dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    pending.push(path);
                } else {
                    let relative = path.strip_prefix(&home).unwrap();
                    files.push(relative.to_str().unwrap().to_owned());
                }
            }
        }
        files.sort();
        files
    }
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

/// The release target the installer picks on the machine the tests run on.
fn host_target() -> String {
    let os = match std::env::consts::OS {
        "linux" => "unknown-linux-musl",
        "macos" => "apple-darwin",
        other => panic!("no release is built for {other}"),
    };
    format!("{}-{os}", std::env::consts::ARCH)
}

fn uname(system: &str, cpu: &str) -> String {
    format!("#!/bin/sh\ncase \"$1\" in\n-s) echo {system} ;;\n-m) echo {cpu} ;;\nesac\n")
}

#[test]
fn the_shell_under_test_is_dash_unless_another_is_asked_for_or_dash_is_absent() {
    let both = |name: &str| Some(PathBuf::from(format!("/usr/bin/{name}")));
    let no_dash = |name: &str| (name == "sh").then(|| PathBuf::from("/bin/sh"));
    assert_eq!(
        choose_shell(Some(OsString::from("/opt/bin/mksh")), both),
        (PathBuf::from("/opt/bin/mksh"), "set by ARCHON_TEST_SH")
    );
    assert_eq!(
        choose_shell(None, both),
        (PathBuf::from("/usr/bin/dash"), "dash")
    );
    assert_eq!(
        choose_shell(None, no_dash),
        (PathBuf::from("/bin/sh"), "the system's sh: no dash on PATH")
    );
}

#[test]
fn a_fresh_install_puts_the_latest_archon_in_the_home_bin_and_touches_nothing_else() {
    let machine = Machine::new().with_aether(16, 17);
    let out = machine.install();
    let text = stdout(&out);
    assert!(out.status.success(), "{out:?}");
    assert_eq!(machine.home_files(), [".local/bin/archon"]);
    let bin = machine.bin();
    let version = Command::new(bin.join("archon")).arg("--version").output();
    assert_eq!(stdout(&version.unwrap()).trim(), "archon 9.9.9");
    assert!(
        text.contains(&format!("Installed archon 9.9.9 to {}.", bin.display())),
        "{text}"
    );
    assert!(text.contains("Verified its SHA-256."), "{text}");
    assert!(text.trim_end().ends_with("Next: archon setup"), "{text}");
    let archive = format!("/latest/download/archon-{}.tar.gz", host_target());
    assert_eq!(
        machine.server.requests(),
        [archive.clone(), format!("{archive}.sha256")]
    );
}

#[test]
fn only_archon_is_installed_from_an_archive_that_carries_the_licence_files() {
    let machine = Machine::new().with_aether(16, 17);
    let name = format!("archon-{}.tar.gz", host_target());
    let archive = machine.path("releases/latest/download").join(name);
    assert_eq!(archive_members(&archive), ARCHIVE_MEMBERS);
    let bin = machine.path("opt/bin");
    let machine = machine.env("ARCHON_INSTALL_DIR", bin.to_str().unwrap());
    let out = machine.install();
    assert!(out.status.success(), "{out:?}");
    let installed: Vec<_> = std::fs::read_dir(&bin)
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    assert_eq!(
        installed,
        ["archon"],
        "the licence files stay in the archive"
    );
    assert!(machine.home_files().is_empty(), "nothing is left elsewhere");
}

#[test]
fn an_older_archon_is_replaced_and_a_pinned_version_is_downloaded_by_tag() {
    let machine = Machine::new()
        .with_aether(16, 17)
        .env("ARCHON_VERSION", "1.2.3");
    let releases = machine.path("releases");
    publish(
        &releases,
        "v1.2.3",
        &host_target(),
        &archon_script("1.2.3"),
        false,
    );
    let bin = machine.path("opt/bin");
    std::fs::create_dir_all(&bin).unwrap();
    write_script(&bin.join("archon"), &archon_script("1.0.0"));
    let machine = machine.env("ARCHON_INSTALL_DIR", bin.to_str().unwrap());

    let out = machine.install();
    let text = stdout(&out);
    assert!(out.status.success(), "{out:?}");
    assert!(
        text.contains(&format!(
            "Replaced archon 1.0.0 with archon 1.2.3 in {}.",
            bin.display()
        )),
        "{text}"
    );
    let archive = format!("/download/v1.2.3/archon-{}.tar.gz", host_target());
    assert_eq!(
        machine.server.requests(),
        [archive.clone(), format!("{archive}.sha256")]
    );
    let left: Vec<_> = std::fs::read_dir(&bin).unwrap().collect();
    assert_eq!(left.len(), 1, "only the binary remains: {left:?}");
    assert!(
        machine.home_files().is_empty(),
        "the home directory is not used"
    );

    let again = stdout(&machine.install());
    assert!(again.contains("Reinstalled archon 1.2.3 in "), "{again}");
}

#[test]
fn a_version_that_is_not_a_tag_is_refused_before_any_download() {
    for version in ["latest", "v1.2", "v1.2.3; rm -rf ~", "../v1.2.3"] {
        let machine = Machine::new().env("ARCHON_VERSION", version);
        let out = machine.install();
        assert_eq!(out.status.code(), Some(EXIT_FAILED), "{version}: {out:?}");
        assert!(
            stderr(&out).contains("ARCHON_VERSION must look like v1.2.3"),
            "{out:?}"
        );
        assert!(machine.server.requests().is_empty(), "{version}");
        assert!(machine.home_files().is_empty(), "{version}");
    }
}

#[test]
fn a_checksum_mismatch_installs_nothing_and_keeps_the_old_archon() {
    let machine = Machine::new().with_aether(16, 17);
    let name = format!("archon-{}.tar.gz.sha256", host_target());
    let wrong = format!("{}  archon.tar.gz\n", "0".repeat(64));
    std::fs::write(machine.path("releases/latest/download").join(name), wrong).unwrap();
    std::fs::create_dir_all(machine.bin()).unwrap();
    write_script(&machine.bin().join("archon"), &archon_script("1.0.0"));

    let out = machine.install();
    let errors = stderr(&out);
    assert_eq!(out.status.code(), Some(EXIT_FAILED), "{out:?}");
    assert!(
        errors.contains("checksum mismatch: expected 0000"),
        "{errors}"
    );
    assert!(errors.contains("Nothing was installed"), "{errors}");
    assert!(!stdout(&out).contains("Installed"), "{out:?}");
    assert_eq!(
        machine.home_files(),
        [".local/bin/archon"],
        "the staging directory is removed"
    );
    let version = Command::new(machine.bin().join("archon"))
        .arg("--version")
        .output();
    assert_eq!(stdout(&version.unwrap()).trim(), "archon 1.0.0");
}

#[test]
fn a_missing_release_or_checksum_installs_nothing() {
    let machine = Machine::new().env("ARCHON_VERSION", "v7.7.7");
    let out = machine.install();
    assert_eq!(out.status.code(), Some(EXIT_FAILED), "{out:?}");
    assert!(stderr(&out).contains("could not download"), "{out:?}");
    assert!(stderr(&out).contains("/download/v7.7.7/"), "{out:?}");
    assert!(machine.home_files().is_empty());

    let machine = Machine::new();
    let name = format!("archon-{}.tar.gz.sha256", host_target());
    std::fs::remove_file(machine.path("releases/latest/download").join(name)).unwrap();
    let out = machine.install();
    assert_eq!(out.status.code(), Some(EXIT_FAILED), "{out:?}");
    assert!(
        stderr(&out).contains(".sha256. Nothing was installed."),
        "{out:?}"
    );
    assert!(machine.home_files().is_empty());
}

#[test]
fn each_supported_platform_gets_its_own_archive() {
    let platforms = [
        ("Linux", "x86_64", "x86_64-unknown-linux-musl"),
        ("Linux", "aarch64", "aarch64-unknown-linux-musl"),
        ("Linux", "arm64", "aarch64-unknown-linux-musl"),
        ("Darwin", "arm64", "aarch64-apple-darwin"),
        ("Darwin", "x86_64", "x86_64-apple-darwin"),
    ];
    for (system, cpu, target) in platforms {
        let mut machine = Machine::new().with_aether(16, 17);
        machine.programs(&[("uname", &uname(system, cpu))]);
        let out = machine.install();
        assert!(out.status.success(), "{system} {cpu}: {out:?}");
        let archive = format!("/latest/download/archon-{target}.tar.gz");
        assert_eq!(
            machine.server.requests().first(),
            Some(&archive),
            "{system} {cpu}"
        );
    }
}

#[test]
fn an_unsupported_platform_is_named_and_nothing_is_downloaded() {
    let cases = [
        ("SunOS", "x86_64", "unsupported operating system: SunOS"),
        ("MINGW64_NT-10.0", "x86_64", "on Windows use WSL"),
        ("Linux", "riscv64", "unsupported CPU: riscv64"),
        ("Darwin", "i386", "unsupported CPU: i386"),
    ];
    for (system, cpu, message) in cases {
        let mut machine = Machine::new();
        machine.programs(&[("uname", &uname(system, cpu))]);
        let out = machine.install();
        assert_eq!(
            out.status.code(),
            Some(EXIT_FAILED),
            "{system} {cpu}: {out:?}"
        );
        assert!(stderr(&out).contains(message), "{system} {cpu}: {out:?}");
        assert!(machine.server.requests().is_empty(), "{system} {cpu}");
        assert!(machine.home_files().is_empty(), "{system} {cpu}");
    }
}

#[test]
fn an_aether_that_is_present_is_reported_with_what_archon_needs() {
    let machine = Machine::new().with_aether(16, 17);
    let out = machine.install();
    let text = stdout(&out);
    assert!(out.status.success(), "{out:?}");
    assert!(text.contains("Aether: aether 0.9.0 at "), "{text}");
    assert!(
        text.contains(
            "Archon needs an Aether that speaks protocol 14 to 17; this one speaks 16 to 17."
        ),
        "{text}"
    );
    assert!(!stderr(&out).contains("warning"), "{out:?}");
    let requests = machine.server.requests();
    assert!(
        !requests.contains(&"/aether/install.sh".to_owned()),
        "a present Aether is not reinstalled: {requests:?}"
    );

    let old = Machine::new().with_aether(12, 13);
    let out = old.install();
    assert!(out.status.success(), "a warning is not a failure: {out:?}");
    assert!(
        stdout(&out).contains("this one speaks 12 to 13."),
        "{out:?}"
    );
    let warning = stderr(&out);
    assert!(
        warning.contains("this Aether is too old for Archon"),
        "{warning}"
    );
    assert!(warning.contains("'aether update'"), "{warning}");

    let new = Machine::new().with_aether(18, 19);
    let warning = stderr(&new.install());
    assert!(
        warning.contains("newer than this Archon supports"),
        "{warning}"
    );
}

#[test]
fn a_missing_aether_is_installed_beside_archon_without_asking() {
    let machine = Machine::new();
    let out = machine.install();
    let text = stdout(&out);
    assert!(out.status.success(), "{out:?}");
    assert!(text.contains("no 'aether' was found"), "{text}");
    let bin = machine.bin();
    assert!(
        text.contains(&format!(
            "Installing Aether to {} without asking (ARCHON_NONINTERACTIVE=1).",
            bin.display()
        )),
        "{text}"
    );
    assert!(
        text.contains(&format!("fake aether installer: {}", bin.display())),
        "the Aether installer gets the same directory: {text}"
    );
    assert_eq!(
        machine.home_files(),
        [".local/bin/aether", ".local/bin/archon"]
    );
    assert!(
        text.contains(&format!(
            "Aether: aether 0.9.0 at {}/aether.",
            bin.display()
        )),
        "{text}"
    );
    assert!(text.contains("this one speaks 16 to 17."), "{text}");

    // An aether in the install directory counts, though it is not on PATH.
    let again = machine.install();
    assert!(again.status.success(), "{again:?}");
    assert!(
        !stdout(&again).contains("no 'aether' was found"),
        "{again:?}"
    );
    let installer_runs = machine
        .server
        .requests()
        .iter()
        .filter(|path| *path == "/aether/install.sh")
        .count();
    assert_eq!(installer_runs, 1);
}

#[test]
fn a_skipped_aether_is_not_installed_and_the_command_is_printed() {
    let machine = Machine::new().env("ARCHON_SKIP_AETHER", "1");
    let out = machine.install();
    let text = stdout(&out);
    assert!(out.status.success(), "{out:?}");
    assert_eq!(machine.home_files(), [".local/bin/archon"]);
    assert!(
        text.contains("Not installing it (ARCHON_SKIP_AETHER=1)."),
        "{text}"
    );
    let command = format!(
        "curl -fsSL {} | AETHER_INSTALL_DIR=\"{}\" sh",
        machine.aether_installer_url(),
        machine.bin().display()
    );
    assert!(text.contains(&command), "{text}");
    let requests = machine.server.requests();
    assert!(
        !requests.contains(&"/aether/install.sh".to_owned()),
        "{requests:?}"
    );
}

#[test]
fn the_mobile_web_app_is_set_up_only_when_asked_for() {
    let quiet = Machine::new().with_aether(16, 17);
    let out = quiet.install();
    assert!(out.status.success(), "{out:?}");
    assert!(!stdout(&out).contains("remote setup"), "{}", stdout(&out));
    let asked = Machine::new().with_aether(16, 17).env("ARCHON_REMOTE", "1");
    let out = asked.install();
    let text = stdout(&out);
    assert!(out.status.success(), "{out:?}");
    assert!(
        text.contains("Setting up the mobile web app (ARCHON_REMOTE=1)."),
        "{text}"
    );
    assert!(
        text.contains("fake archon 9.9.9: remote setup --yes"),
        "{text}"
    );
    let failing = Machine::new().with_aether(16, 17).env("ARCHON_REMOTE", "1");
    let refusing = archon_script("9.9.9").replace(
        "*) echo",
        "'remote setup --yes') echo 'no tailscale' >&2; exit 1 ;;\n*) echo",
    );
    for target in TARGETS {
        publish(&failing.path("releases"), "v9.9.9", target, &refusing, true);
    }
    let out = failing.install();
    let errors = stderr(&out);
    assert!(
        out.status.success(),
        "a failed remote step keeps the install: {out:?}"
    );
    assert!(
        errors.contains("the mobile web app is not set up"),
        "{errors}"
    );
    assert!(errors.contains("  archon remote setup"), "{errors}");
}

#[test]
fn a_failed_aether_step_keeps_archon_and_prints_the_retry_command() {
    let failing = Machine::new();
    failing.aether_installer("echo 'no aether for this platform' >&2; exit 1");
    let unreachable = Machine::new();
    std::fs::remove_file(unreachable.path("releases/aether/install.sh")).unwrap();
    for machine in [failing, unreachable] {
        let out = machine.install();
        let errors = stderr(&out);
        assert_eq!(out.status.code(), Some(EXIT_AETHER_FAILED), "{out:?}");
        assert_eq!(machine.home_files(), [".local/bin/archon"]);
        assert!(
            errors
                .contains("Aether could not be installed. Archon is installed and stays in place"),
            "{errors}"
        );
        let command = format!(
            "curl -fsSL {} | AETHER_INSTALL_DIR=\"{}\" sh",
            machine.aether_installer_url(),
            machine.bin().display()
        );
        assert!(errors.contains(&command), "{errors}");
        let text = stdout(&out);
        assert!(text.contains("Installed archon 9.9.9"), "{text}");
        assert!(text.trim_end().ends_with("Next: archon setup"), "{text}");
    }
}

#[test]
fn a_directory_that_is_not_on_path_gets_the_line_to_add() {
    let machine = Machine::new().with_aether(16, 17);
    let text = stdout(&machine.install());
    let bin = machine.bin();
    assert!(
        text.contains(&format!("{} is not on your PATH.", bin.display())),
        "{text}"
    );
    assert!(
        text.contains(&format!("  export PATH=\"{}:$PATH\"", bin.display())),
        "{text}"
    );

    let mut on_path = Machine::new().with_aether(16, 17);
    let bin = on_path.bin();
    on_path.path.push(bin.clone());
    let out = on_path.install();
    let text = stdout(&out);
    assert!(
        text.contains(&format!("{} is on your PATH.", bin.display())),
        "{text}"
    );
    assert!(!text.contains("export PATH"), "{text}");
    assert!(!stderr(&out).contains("warning"), "{out:?}");

    // The same directory written with a trailing slash is the same entry.
    let mut slashed = Machine::new().with_aether(16, 17);
    let bin = slashed.bin();
    slashed
        .path
        .push(PathBuf::from(format!("{}/", bin.display())));
    let out = slashed.install();
    let text = stdout(&out);
    assert!(
        text.contains(&format!("{} is on your PATH.", bin.display())),
        "{text}"
    );
    assert!(!text.contains("export PATH"), "{text}");
    assert!(!stderr(&out).contains("warning"), "{out:?}");

    // An archon earlier on PATH would run instead of the installed one.
    let mut shadowed = Machine::new().with_aether(16, 17);
    let bin = shadowed.bin();
    shadowed.path.push(bin);
    let other = shadowed.programs(&[("archon", &archon_script("0.1.0"))]);
    let warning = stderr(&shadowed.install());
    assert!(
        warning.contains(&format!("'archon' runs {}/archon", other.display())),
        "{warning}"
    );
}

#[test]
fn each_missing_program_gets_one_line_with_its_purpose_and_how_to_install_it() {
    let machine = Machine::new().with_aether(16, 17);
    let text = stdout(&machine.install());
    let lines: Vec<&str> = text
        .lines()
        .skip_while(|line| *line != "Other programs Archon uses:")
        .skip(1)
        .take_while(|line| line.starts_with("  "))
        .collect();
    let starts = [
        "  git (required: a worktree for every item): https://git-scm.com/install/",
        "  claude or codex (one required: the engine that runs agents, logged in): Claude Code:",
        "  gh (optional: the pr ship mode and the GitHub watcher): https://cli.github.com/",
        "  lavish-axi (optional: review pages): 'npm install -g lavish-axi'",
    ];
    assert_eq!(lines.len(), starts.len(), "{text}");
    for (line, start) in lines.iter().zip(starts) {
        assert!(line.starts_with(start), "{line}");
    }

    let mut complete = Machine::new().with_aether(16, 17);
    let present = "#!/bin/sh\nexit 0\n";
    complete.programs(&[
        ("git", present),
        ("codex", present),
        ("gh", present),
        ("lavish-axi", present),
    ]);
    let text = stdout(&complete.install());
    assert!(
        text.contains("Other programs Archon uses:\n  all found.\n"),
        "{text}"
    );
}

#[test]
fn wget_is_used_when_there_is_no_curl() {
    if on_path("wget").is_none() {
        eprintln!("skipped: this machine has no wget");
        return;
    }
    let machine = Machine::with_downloaders(&["wget"], &["sha256sum", "shasum"]);
    let out = machine.install();
    assert!(out.status.success(), "{out:?}");
    assert_eq!(
        machine.home_files(),
        [".local/bin/aether", ".local/bin/archon"],
        "archon and the Aether installer both came by wget"
    );
}

#[test]
fn a_machine_without_a_downloader_or_a_sha256_tool_is_refused() {
    let machine = Machine::with_downloaders(&[], &["sha256sum", "shasum"]);
    let out = machine.install();
    assert_eq!(out.status.code(), Some(EXIT_FAILED), "{out:?}");
    assert!(
        stderr(&out).contains("neither curl nor wget was found"),
        "{out:?}"
    );

    let machine = Machine::with_downloaders(&["curl"], &[]);
    let out = machine.install();
    assert_eq!(out.status.code(), Some(EXIT_FAILED), "{out:?}");
    assert!(
        stderr(&out).contains("neither sha256sum nor shasum was found"),
        "{out:?}"
    );
    assert!(
        machine.server.requests().is_empty(),
        "nothing is downloaded unverified"
    );
    assert!(machine.home_files().is_empty());
}

#[test]
fn downloads_go_through_the_proxy_the_environment_names() {
    let machine = Machine::new().with_aether(16, 17);
    let proxy = machine.server.url();
    let machine = machine
        .env("ARCHON_INSTALL_BASE_URL", "http://releases.invalid/mirror")
        .env("http_proxy", &proxy);
    // The server root is the proxy's view of http://releases.invalid/.
    let mirror = machine.path("releases/mirror");
    std::fs::create_dir(&mirror).unwrap();
    std::fs::rename(machine.path("releases/latest"), mirror.join("latest")).unwrap();
    let out = machine.install();
    assert!(out.status.success(), "{out:?}");
    let archive = format!("/mirror/latest/download/archon-{}.tar.gz", host_target());
    assert_eq!(machine.server.requests().first(), Some(&archive));
}

#[test]
fn a_truncated_download_of_the_installer_runs_nothing() {
    let script = std::fs::read(INSTALLER).unwrap();
    let call = b"\nmain \"$@\"\n";
    assert!(script.ends_with(call), "the body runs from the last line");
    let body_end = script.len() - call.len();
    let machine = Machine::new();
    let cut_script = machine.path("truncated.sh");
    for cut in (0..=body_end).step_by(211).chain([body_end]) {
        std::fs::write(&cut_script, &script[..cut]).unwrap();
        machine.pipe(&cut_script);
        assert!(machine.home_files().is_empty(), "cut at byte {cut}");
        assert!(machine.server.requests().is_empty(), "cut at byte {cut}");
    }
}
