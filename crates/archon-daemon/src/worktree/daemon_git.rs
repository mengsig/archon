//! The daemon's own git wherever an agent can write the repository, its
//! config or its tree (ARCHITECTURE §13.4): nothing an agent wrote runs.
//!
//! Every command drops the caller's `GIT_*` environment, never prompts, has
//! a time limit, and forces [`FORCED`] over any config it reads: no hook,
//! fsmonitor, attributes file, signing or submodule recursion, and no
//! transport but local files. The config it reads depends on [`Config`].

use std::io::Read;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use archon_core::error::ArchonError;

/// Most a local git command may take.
pub(crate) const LOCAL_TIMEOUT: Duration = Duration::from_secs(120);
/// Most a fetch or push may take.
pub(crate) const NETWORK_TIMEOUT: Duration = Duration::from_secs(180);
/// How often a running git is checked on.
const POLL: Duration = Duration::from_millis(10);
/// Bytes of stderr kept for an error.
const MAX_STDERR_BYTES: usize = 8192;

/// Settings naming a program, forced off for every git the daemon runs.
const NO_PROGRAM: [&str; 6] = [
    "core.hooksPath=/dev/null",
    "core.fsmonitor=false",
    "log.showSignature=false",
    "commit.gpgSign=false",
    "tag.gpgSign=false",
    "gpg.program=/bin/false",
];
/// Forced over every config file a [`DaemonGit`] reads, besides [`no_programs`].
const FORCED: [&str; 21] = [
    "core.attributesFile=/dev/null",
    "core.askPass=",
    // A repository's `protocol.<name>.allow` outranks `protocol.allow`.
    "protocol.allow=never",
    "protocol.ext.allow=never",
    "protocol.fd.allow=never",
    "protocol.git.allow=never",
    "protocol.ssh.allow=never",
    "protocol.http.allow=never",
    "protocol.https.allow=never",
    "protocol.ftp.allow=never",
    "protocol.ftps.allow=never",
    "protocol.file.allow=always",
    "gc.auto=0",
    "maintenance.auto=false",
    "commit.gpgSign=false",
    "push.gpgSign=false",
    "merge.verifySignatures=false",
    "submodule.recurse=false",
    "fetch.recurseSubmodules=false",
    "push.recurseSubmodules=no",
    "advice.detachedHead=false",
];
/// The fixed settings that keep git from running any program a repository's
/// config or hooks directory names: no hooks directory, fsmonitor or
/// signature check. Hooks a config defines by name need
/// [`config_hook_settings`] besides.
pub(crate) fn no_program_settings() -> Vec<(String, String)> {
    let split = |setting: &str| {
        let (key, value) = setting.split_once('=').expect("key=value");
        (key.to_owned(), value.to_owned())
    };
    let pairs: Vec<(String, String)> = NO_PROGRAM.iter().map(|s| split(s)).collect();
    assert_eq!(pairs.len(), NO_PROGRAM.len());
    assert!(pairs.iter().all(|(key, _)| !key.is_empty()));
    pairs
}

/// [`no_program_settings`] as `-c` options.
pub(crate) fn no_programs() -> Vec<String> {
    let args: Vec<String> = no_program_settings()
        .into_iter()
        .flat_map(|(key, value)| ["-c".to_owned(), format!("{key}={value}")])
        .collect();
    assert!(args.iter().any(|arg| arg == "core.hooksPath=/dev/null"));
    args
}

/// Git 2.54 runs the hooks a config defines (`hook.<name>.command` and
/// `hook.<name>.event`) besides the hooks directory. They are keyed by a name
/// the config picks, and git has no switch that turns them all off or that
/// names an event to silence: only `hook.<name>.enabled=false` per name, which
/// a later `-c` outranks. So the names are read first, by a git that cannot
/// run anything yet, and the caller disables each; a failed read is an error,
/// never a git left running them.
///
/// `probe` is the git that will run, with its options and environment but no
/// subcommand; it only lists config keys.
fn hook_names(mut probe: Command, place: &Path) -> Result<Vec<String>, ArchonError> {
    let what = "git config --get-regexp ^hook\\.";
    probe
        .args(["config", "-z", "--name-only", "--get-regexp", "^hook\\."])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    let mut child = probe
        .spawn()
        .map_err(|e| ArchonError::io(format!("listing hooks in {}", place.display()), e))?;
    let stdout = reader(child.stdout.take().expect("piped"), usize::MAX);
    let stderr = reader(child.stderr.take().expect("piped"), MAX_STDERR_BYTES);
    let status = wait(&mut child, LOCAL_TIMEOUT);
    let (stdout, stderr) = (joined(stdout, what)?, joined(stderr, what)?);
    let status = status?.ok_or_else(|| {
        ArchonError::Unavailable(format!("listing hooks in {} timed out", place.display()))
    })?;
    // Exit 1: no key matches.
    match status.code() {
        Some(0) => {}
        Some(1) => return Ok(Vec::new()),
        _ => {
            return Err(ArchonError::io(
                format!("listing the hooks config defines in {}", place.display()),
                std::io::Error::other(format!(
                    "{status}: {}",
                    String::from_utf8_lossy(&stderr).trim()
                )),
            ));
        }
    }
    let keys = String::from_utf8(stdout).map_err(|_| {
        ArchonError::Invalid(format!("hook names in {} are not UTF-8", place.display()))
    })?;
    let mut names: Vec<String> = keys
        .split('\0')
        .filter_map(|key| key.strip_prefix("hook.")?.rsplit_once('.'))
        .map(|(name, _)| name.to_owned())
        .collect();
    names.sort();
    names.dedup();
    Ok(names)
}

/// The `-c` and `-C` options at the start of `args`, before the subcommand.
fn leading_options(args: &[String]) -> &[String] {
    let mut end = 0;
    while args.len() >= end + 2 && (args[end] == "-c" || args[end] == "-C") {
        end += 2;
    }
    &args[..end]
}

/// A git with `command`'s options, environment and directory, to list config
/// with: it reads what `command` will, `globals` being options still to come.
fn probe_of(command: &Command, globals: &[String]) -> Command {
    let mut probe = Command::new("git");
    probe.args(command.get_args()).args(globals);
    for (key, value) in command.get_envs() {
        match value {
            Some(value) => probe.env(key, value),
            None => probe.env_remove(key),
        };
    }
    if let Some(dir) = command.get_current_dir() {
        probe.current_dir(dir);
    }
    probe
}

/// `-c` options disabling every hook the config that `command` (a `git` with
/// options and environment but no subcommand yet) will read defines.
pub(crate) fn config_hook_options_of(
    command: &Command,
    dir: &Path,
) -> Result<Vec<String>, ArchonError> {
    as_options(disable_each(hook_names(probe_of(command, &[]), dir)?))
}

/// `hook.<name>.enabled=false` for each of `names`.
fn disable_each(names: Vec<String>) -> Vec<(String, String)> {
    names
        .into_iter()
        .map(|name| (format!("hook.{name}.enabled"), "false".to_owned()))
        .collect()
}

/// The settings that disable every hook the config git reads in `dir` defines,
/// for a git that [`Config`] describes: add them to [`no_program_settings`].
pub(crate) fn config_hook_settings(
    dir: &Path,
    config: Config,
) -> Result<Vec<(String, String)>, ArchonError> {
    assert!(dir.is_absolute(), "daemon git runs at absolute paths");
    let mut probe = Command::new("git");
    clean_env(&mut probe, config);
    probe.arg("-C").arg(dir);
    Ok(disable_each(hook_names(probe, dir)?))
}

/// [`config_hook_settings`] as `-c` options.
pub(crate) fn config_hook_options(dir: &Path, config: Config) -> Result<Vec<String>, ArchonError> {
    as_options(config_hook_settings(dir, config)?)
}

/// `word` as one shell word.
pub(crate) fn shell_quote(word: &str) -> String {
    format!("'{}'", word.replace('\'', "'\\''"))
}

/// A name with `=` cannot be given that way, and is refused.
fn as_options(settings: Vec<(String, String)>) -> Result<Vec<String>, ArchonError> {
    let mut args = Vec::new();
    for (key, value) in settings {
        if key.contains(['=', '\n']) {
            return Err(ArchonError::Invalid(format!(
                "a hook named {key:?} cannot be disabled on a command line; refusing to run git"
            )));
        }
        args.extend(["-c".to_owned(), format!("{key}={value}")]);
    }
    Ok(args)
}

/// The transports a fetch or push may also use.
const NETWORK: [&str; 2] = ["protocol.https.allow=always", "protocol.ssh.allow=always"];
/// The caller's environment a network command keeps: how the operator
/// reaches a remote over ssh.
const OPERATOR_ENV: [&str; 3] = ["GIT_SSH_COMMAND", "GIT_SSH", "GIT_SSH_VARIANT"];

/// Which config files git reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Config {
    /// Only the repository's own, which may be an agent's: no system or
    /// global file, and no credential helper at all.
    Sealed,
    /// The operator's system and global files too (credential helpers, `insteadOf`, ssh,
    /// filters); only on a git directory the daemon writes, or [`DaemonGit::configured_in`].
    Operator,
}

/// `command` without the caller's `GIT_*` environment and, for a sealed
/// `config`, without any config file but the repository's.
fn clean_env(command: &mut Command, config: Config) {
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("GIT_") {
            command.env_remove(key);
        }
    }
    // Git's identity of last resort: the daemon's is never the repository's.
    command.env_remove("EMAIL");
    if config == Config::Sealed {
        command
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .args(["-c", "credential.helper="]);
    }
}

/// One daemon git command, built then run once.
pub(crate) struct DaemonGit {
    command: Command,
    shown: Vec<String>,
    place: PathBuf,
    limit: Duration,
}

/// Not derived: the command's environment can carry a secret.
impl std::fmt::Debug for DaemonGit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DaemonGit")
            .field("args", &self.shown)
            .field("place", &self.place)
            .field("limit", &self.limit)
            .finish_non_exhaustive()
    }
}

impl DaemonGit {
    /// Git in `dir`, a work tree or repository agents may write.
    pub(crate) fn sealed_in(dir: &Path) -> Self {
        assert!(dir.is_absolute(), "daemon git runs at absolute paths");
        let mut git = Self::new(Config::Sealed, dir);
        git.command.arg("-C").arg(dir);
        git
    }

    /// Git in the operator's repository `repo` with every config file git
    /// reads there: only for what that config says, such as `git var`.
    pub(crate) fn configured_in(repo: &Path) -> Self {
        assert!(repo.is_absolute(), "daemon git runs at absolute paths");
        let mut git = Self::new(Config::Operator, repo);
        git.command.arg("-C").arg(repo);
        git
    }

    /// Git on `git_dir`, which only the daemon writes, and `work_tree` if any: `--git-dir`
    /// names it, so no `.git` or `core.worktree` written elsewhere is followed.
    pub(crate) fn owned(git_dir: &Path, work_tree: Option<&Path>, config: Config) -> Self {
        assert!(git_dir.is_absolute(), "daemon git directories are absolute");
        let place = work_tree.unwrap_or(git_dir);
        let mut git = Self::new(config, place);
        git.command.arg("--git-dir").arg(git_dir);
        if let Some(tree) = work_tree {
            assert!(tree.is_absolute(), "work trees are absolute");
            git.command.arg("--work-tree").arg(tree);
        }
        git.command.current_dir(place);
        git
    }

    fn new(config: Config, place: &Path) -> Self {
        let mut command = Command::new("git");
        clean_env(&mut command, config);
        command
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_ASKPASS", "")
            .env("GIT_ATTR_NOSYSTEM", "1")
            .env("GIT_NO_REPLACE_OBJECTS", "1")
            .env("GIT_NO_LAZY_FETCH", "1");
        command.args(no_programs());
        for setting in FORCED {
            command.args(["-c", setting]);
        }
        Self {
            command,
            shown: Vec::new(),
            place: place.to_owned(),
            limit: LOCAL_TIMEOUT,
        }
    }

    /// Also allow https and ssh, for a fetch or push of the operator's
    /// remote; only a daemon-owned git directory reaches a network.
    pub(crate) fn network(mut self) -> Self {
        for setting in NETWORK {
            self.command.args(["-c", setting]);
        }
        for name in OPERATOR_ENV {
            if let Some(value) = std::env::var_os(name) {
                self.command.env(name, value);
            }
        }
        self.limit = NETWORK_TIMEOUT;
        self
    }

    /// Read and write objects in `objects`, a repository's object store,
    /// instead of the git directory's own.
    pub(crate) fn objects(mut self, objects: &Path) -> Self {
        assert!(objects.is_absolute(), "object stores are absolute");
        self.command.env("GIT_OBJECT_DIRECTORY", objects);
        self
    }

    /// The directory it runs in or on.
    pub(crate) fn place(&self) -> &Path {
        &self.place
    }

    /// Allow it `limit` instead of the default for its kind.
    pub(crate) fn limit(mut self, limit: Duration) -> Self {
        assert!(!limit.is_zero(), "a time limit is positive");
        self.limit = limit;
        self
    }

    /// Read `settings` as config given on the command line, through the
    /// environment: shown in no argument list, error or debug output.
    pub(crate) fn secret_config(mut self, settings: &[(String, String)]) -> Self {
        assert!(!settings.is_empty(), "something to set");
        assert!(
            settings.iter().all(|(key, _)| key.contains('.')),
            "section.key"
        );
        for (n, (key, value)) in settings.iter().enumerate() {
            self.command
                .env(format!("GIT_CONFIG_KEY_{n}"), key)
                .env(format!("GIT_CONFIG_VALUE_{n}"), value);
        }
        self.command
            .env("GIT_CONFIG_COUNT", settings.len().to_string());
        self
    }

    pub(crate) fn env(mut self, key: &str, value: &str) -> Self {
        assert!(key.starts_with("GIT_"), "only git's own variables are set");
        self.command.env(key, value);
        self
    }

    pub(crate) fn args<'a>(mut self, args: impl IntoIterator<Item = &'a str>) -> Self {
        self.shown.extend(args.into_iter().map(str::to_owned));
        self
    }

    /// Run it and require success; its stdout, trimmed.
    pub(crate) fn text(self) -> Result<String, ArchonError> {
        let ran = self.output()?;
        if !ran.status.success() {
            return Err(ran.failure());
        }
        ran.stdout_text()
    }

    /// Run it with no input, killing it and all it started at the limit.
    pub(crate) fn output(mut self) -> Result<Ran, ArchonError> {
        assert!(!self.shown.is_empty(), "a git command needs arguments");
        let what = format!("git {}", self.shown.join(" "));
        // Before any git that reads the repository's config runs a hook. The
        // options in front of the subcommand can switch conditional includes on.
        let globals = leading_options(&self.shown);
        let probe = probe_of(&self.command, globals);
        // A place that is gone has no config to read: git fails on it itself.
        let hooks = match self.place.is_dir() {
            true => as_options(disable_each(hook_names(probe, &self.place)?))?,
            false => Vec::new(),
        };
        self.command.args(hooks).args(&self.shown);
        self.command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0);
        let mut child = self.command.spawn().map_err(|e| {
            ArchonError::io(format!("running {what} in {}", self.place.display()), e)
        })?;
        let stdout = reader(child.stdout.take().expect("piped"), usize::MAX);
        let stderr = reader(child.stderr.take().expect("piped"), MAX_STDERR_BYTES);
        let status = wait(&mut child, self.limit);
        let (stdout, stderr) = (joined(stdout, &what)?, joined(stderr, &what)?);
        let Some(status) = status? else {
            return Err(ArchonError::Unavailable(format!(
                "{what} in {} timed out after {}s",
                self.place.display(),
                self.limit.as_secs()
            )));
        };
        Ok(Ran {
            status,
            stdout,
            stderr,
            what,
            place: self.place,
        })
    }
}

/// What a finished daemon git printed.
#[derive(Debug)]
pub(crate) struct Ran {
    pub status: ExitStatus,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    what: String,
    place: PathBuf,
}

impl Ran {
    pub(crate) fn stdout_text(&self) -> Result<String, ArchonError> {
        let text = std::str::from_utf8(&self.stdout)
            .map_err(|_| ArchonError::Invalid(format!("{} printed non-UTF-8", self.what)))?;
        Ok(text.trim().to_owned())
    }

    /// Stdout as UTF-8, with its NULs and whitespace kept.
    pub(crate) fn stdout_text_untrimmed(&self) -> Result<String, ArchonError> {
        String::from_utf8(self.stdout.clone())
            .map_err(|_| ArchonError::Invalid(format!("{} printed non-UTF-8", self.what)))
    }

    pub(crate) fn stderr_text(&self) -> String {
        String::from_utf8_lossy(&self.stderr).trim().to_owned()
    }

    /// The error of a command that exited unsuccessfully.
    pub(crate) fn failure(&self) -> ArchonError {
        assert!(!self.status.success(), "only a failure is reported");
        ArchonError::io(
            format!("{} in {}", self.what, self.place.display()),
            std::io::Error::other(format!("{}: {}", self.status, self.stderr_text())),
        )
    }
}

/// Wait for `child` up to `limit`; `None` once it was killed for overrunning.
fn wait(child: &mut Child, limit: Duration) -> Result<Option<ExitStatus>, ArchonError> {
    assert!(!limit.is_zero(), "a time limit is positive");
    let started = Instant::now();
    loop {
        let waited = child.try_wait();
        let status = waited.map_err(|e| ArchonError::io("waiting for git", e))?;
        if let Some(status) = status {
            return Ok(Some(status));
        }
        if started.elapsed() >= limit {
            let group = rustix::process::Pid::from_child(child);
            // Already gone is fine: the wait below reaps it either way.
            let _ = rustix::process::kill_process_group(group, rustix::process::Signal::KILL);
            child
                .wait()
                .map_err(|e| ArchonError::io("reaping a git that overran", e))?;
            return Ok(None);
        }
        std::thread::sleep(POLL);
    }
}

type Reader = JoinHandle<std::io::Result<Vec<u8>>>;

/// Drain `stream` on its own thread, keeping at most `keep` bytes, so a
/// chatty git never blocks on a full pipe.
fn reader(mut stream: impl Read + Send + 'static, keep: usize) -> Reader {
    assert!(keep > 0, "a stream worth reading keeps something");
    std::thread::spawn(move || {
        let mut kept = Vec::new();
        let mut chunk = [0u8; 8192];
        loop {
            let read = match stream.read(&mut chunk) {
                Ok(0) => return Ok(kept),
                Ok(read) => read,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            };
            let room = keep.saturating_sub(kept.len()).min(read);
            kept.extend_from_slice(&chunk[..room]);
        }
    })
}

/// What `reader` read of `what`'s output: all of it, or an error, never a
/// silently shortened output.
fn joined(reader: Reader, what: &str) -> Result<Vec<u8>, ArchonError> {
    let read = reader
        .join()
        .unwrap_or_else(|panic| std::panic::resume_unwind(panic));
    read.map_err(|e| ArchonError::io(format!("reading the output of {what}"), e))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().canonicalize().unwrap();
        DaemonGit::sealed_in(&path)
            .args(["init", "--quiet"])
            .text()
            .unwrap();
        (dir, path)
    }

    /// A program that leaves `marker` behind when anything runs it.
    fn planted(path: &Path, marker: &Path) -> String {
        use std::os::unix::fs::PermissionsExt;
        let script = path.join("planted.sh");
        std::fs::write(&script, format!("#!/bin/sh\ntouch {}\n", marker.display())).unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        script.display().to_string()
    }

    /// A commit carrying a signature header, which `log` would verify.
    fn signed_commit(path: &Path) -> String {
        let tree = DaemonGit::sealed_in(path)
            .args(["write-tree"])
            .text()
            .unwrap();
        let body = format!(
            "tree {tree}\nauthor a <a@example.invalid> 0 +0000\ncommitter a <a@example.invalid> \
             0 +0000\ngpgsig -----BEGIN PGP SIGNATURE-----\n \n abc\n -----END PGP \
             SIGNATURE-----\n\nx\n"
        );
        let file = path.join("commit.txt");
        std::fs::write(&file, body).unwrap();
        let file = file.to_str().unwrap();
        let args = ["hash-object", "-t", "commit", "-w", file];
        DaemonGit::sealed_in(path).args(args).text().unwrap()
    }

    #[test]
    fn a_repository_config_names_no_program_the_daemon_runs() {
        let (_dir, path) = repo();
        let marker = path.join("ran");
        let planted = planted(&path, &marker);
        for (key, value) in [
            ("core.fsmonitor", planted.as_str()),
            ("hook.planted.command", planted.as_str()),
            ("hook.planted.event", "reference-transaction"),
            ("log.showSignature", "true"),
            ("gpg.program", planted.as_str()),
            ("protocol.ext.allow", "always"),
        ] {
            let set = DaemonGit::sealed_in(&path).args(["config", key, value]);
            set.text().unwrap();
        }
        std::fs::write(path.join("a.txt"), "a\n").unwrap();
        let status = DaemonGit::sealed_in(&path).args(["status", "--porcelain"]);
        assert!(status.text().unwrap().contains("a.txt"));
        let commit = signed_commit(&path);
        let update = ["update-ref", "refs/heads/main", commit.as_str()];
        DaemonGit::sealed_in(&path).args(update).text().unwrap();
        let log = DaemonGit::sealed_in(&path).args(["log", "--format=%H", "-1", "main"]);
        assert_eq!(log.text().unwrap(), commit);
        let ext = format!("ext::{planted}");
        let fetch = DaemonGit::sealed_in(&path).args(["fetch", &ext, "HEAD"]);
        assert!(!fetch.output().unwrap().status.success(), "ext is refused");
        assert!(!marker.exists(), "nothing the config names ran");

        assert!(
            !config_hooks_exist() || plain_git_runs_planted(&path, &marker, &commit),
            "the planted hook is real: plain git runs it"
        );
    }

    /// Whether the installed git runs hooks a config defines (2.54), found by
    /// making it run one, not by its version.
    fn config_hooks_exist() -> bool {
        let (_dir, path) = repo();
        let marker = path.join("probe-ran");
        let planted = planted(&path, &marker);
        let commit = signed_commit(&path);
        plain_git(&path, &planted, "probe", &commit);
        let supported = marker.exists();
        if !supported {
            eprintln!("skipped: this git has no config-defined hooks, so none can be planted");
        }
        supported
    }

    /// Plain git, with no daemon seal, firing `reference-transaction` in `path`
    /// under a hook that `planted` defines by config.
    fn plain_git(path: &Path, planted: &str, name: &str, commit: &str) {
        let command = format!("hook.{name}.command={planted}");
        let event = format!("hook.{name}.event=reference-transaction");
        // Whether the update itself succeeds is beside the point.
        Command::new("git")
            .arg("-C")
            .arg(path)
            .args([
                "-c",
                "core.hooksPath=/dev/null",
                "-c",
                &command,
                "-c",
                &event,
            ])
            .args(["update-ref", "refs/heads/probe", commit])
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .stderr(Stdio::null())
            .status()
            .unwrap();
    }

    fn plain_git_runs_planted(path: &Path, marker: &Path, commit: &str) -> bool {
        let status = Command::new("git")
            .arg("-C")
            .arg(path)
            .args([
                "-c",
                "core.hooksPath=/dev/null",
                "update-ref",
                "refs/heads/b",
                commit,
            ])
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .status()
            .unwrap();
        assert!(status.success());
        marker.exists()
    }

    /// Hooks are named by their config, whatever the name looks like and
    /// wherever the config is read from.
    #[test]
    fn every_hook_a_config_names_is_disabled_whatever_its_name() {
        let (_dir, path) = repo();
        let marker = path.join("ran");
        let planted = planted(&path, &marker);
        let included = path.join("included.config");
        let body = format!(
            "[hook \"from.include\"]\n\tcommand = {planted}\n\tevent = reference-transaction\n"
        );
        std::fs::write(&included, body).unwrap();
        let sets = [
            ("hook.dotted.name.here.command", planted.as_str()),
            ("hook.dotted.name.here.event", "reference-transaction"),
            ("hook.Mixed Case.command", planted.as_str()),
            ("hook.Mixed Case.event", "reference-transaction"),
            ("include.path", included.to_str().unwrap()),
        ];
        for (key, value) in sets {
            let set = DaemonGit::sealed_in(&path).args(["config", key, value]);
            set.text().unwrap();
        }
        let commit = signed_commit(&path);
        let update = ["update-ref", "refs/heads/main", commit.as_str()];
        DaemonGit::sealed_in(&path).args(update).text().unwrap();
        assert!(!marker.exists(), "no named hook ran");
        let names = config_hook_settings(&path, Config::Sealed).unwrap();
        let keys: Vec<&str> = names.iter().map(|(key, _)| key.as_str()).collect();
        assert_eq!(
            keys,
            [
                "hook.Mixed Case.enabled",
                "hook.dotted.name.here.enabled",
                "hook.from.include.enabled",
            ]
        );
    }

    /// A hook named like its event: newer git rejects the config outright,
    /// older git runs it as any other hook. Whichever this git does, the
    /// sealed git does the same minus the program.
    #[test]
    fn a_hook_named_like_its_event_runs_nothing_under_the_seal() {
        let (_dir, path) = repo();
        let marker = path.join("ran");
        let planted = planted(&path, &marker);
        let commit = signed_commit(&path);
        let sets = [
            ("hook.reference-transaction.command", planted.as_str()),
            ("hook.reference-transaction.event", "reference-transaction"),
        ];
        for (key, value) in sets {
            let set = DaemonGit::sealed_in(&path).args(["config", key, value]);
            set.text().unwrap();
        }
        // Found by asking plain git, not by its version.
        let plain = Command::new("git")
            .arg("-C")
            .arg(&path)
            .args([
                "-c",
                "core.hooksPath=/dev/null",
                "update-ref",
                "refs/heads/plain",
                &commit,
            ])
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .stderr(Stdio::null())
            .status()
            .unwrap();
        let update = ["update-ref", "refs/heads/main", commit.as_str()];
        let sealed = DaemonGit::sealed_in(&path).args(update).output();
        if plain.success() {
            assert!(sealed.unwrap().status.success(), "git accepts the config");
        } else {
            let sealed = sealed.map(|ran| ran.status.success());
            assert!(
                matches!(sealed, Ok(false) | Err(_)),
                "refused, so fails closed"
            );
            eprintln!("this git rejects a hook named like its event");
        }
        assert!(!marker.exists(), "the hook named like its event ran");
    }

    /// The probe reads what the command will: its environment's config and the
    /// options in front of its subcommand, conditional includes included.
    #[test]
    fn the_probe_sees_the_config_its_command_is_given() {
        let (_dir, path) = repo();
        let mut command = Command::new("git");
        command
            .arg("-C")
            .arg(&path)
            .env("GIT_CONFIG_COUNT", "1")
            .env("GIT_CONFIG_KEY_0", "hook.from-env.command")
            .env("GIT_CONFIG_VALUE_0", "true");
        let options = config_hook_options_of(&command, &path).unwrap();
        assert_eq!(options, ["-c", "hook.from-env.enabled=false"]);

        let included = path.join("cond.config");
        std::fs::write(&included, "[hook \"cond\"]\n\tcommand = true\n").unwrap();
        let key = "includeif.hasconfig:remote.*.url:*://example.invalid/**.path";
        let set = DaemonGit::sealed_in(&path).args(["config", key, included.to_str().unwrap()]);
        set.text().unwrap();
        let globals = ["-c", "remote.r.url=https://example.invalid/x"].map(str::to_owned);
        let mut git = Command::new("git");
        git.arg("-C").arg(&path);
        let seen = hook_names(probe_of(&git, &globals), &path).unwrap();
        assert_eq!(seen, ["cond"]);
        assert!(hook_names(probe_of(&git, &[]), &path).unwrap().is_empty());
    }

    #[test]
    fn a_hook_name_a_command_line_cannot_carry_is_refused() {
        let (_dir, path) = repo();
        let set = DaemonGit::sealed_in(&path).args(["config", "hook.a=b.event", "update"]);
        set.text().unwrap();
        let refused = DaemonGit::sealed_in(&path).args(["status"]).output();
        assert!(
            refused.is_err(),
            "git is not run with a hook it cannot disable"
        );
        // A directory that is gone has no config: git itself fails on it.
        let gone = DaemonGit::sealed_in(&path.join("missing")).args(["status"]);
        assert!(!gone.output().unwrap().status.success());
    }

    #[test]
    fn a_git_that_overruns_is_killed_with_what_it_started() {
        let (_dir, path) = repo();
        let git = DaemonGit::sealed_in(&path)
            .args(["-c", "alias.slow=!sleep 30", "slow"])
            .limit(Duration::from_millis(200));
        let started = Instant::now();
        let error = git.output().unwrap_err();
        assert!(error.to_string().contains("timed out"), "{error}");
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "not waited out"
        );
    }
}
