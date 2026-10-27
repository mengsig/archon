//! `archon update`: replace this archon with a released one, from the assets
//! and with the SHA-256 check of `install.sh`; then, when asked, update Aether
//! and restart what runs them (`restart`).

mod aether;
mod interrupt;
mod launch;
mod location;
mod release;
mod restart;
mod tools;
pub(super) mod version;

use std::cmp::Ordering;
use std::os::fd::{AsFd as _, BorrowedFd};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, SystemTime};

use anyhow::Context as _;

use self::aether::Requirement;
use self::interrupt::{Interrupt, Interrupts};
use self::release::{BINARIES, Source};
use self::version::Version;
use super::{emit, to_json};
use crate::cli::UpdateArgs;
use crate::connect::{archon_bin, connect_existing};
use archon_core::config::{AetherMode, Config};

/// `archon update --check` exits with this when a newer release exists.
const UPDATE_AVAILABLE: u8 = 10;
const SMALL_FILE_TIMEOUT: Duration = Duration::from_secs(30);
const ARCHIVE_TIMEOUT: Duration = Duration::from_secs(600);
/// How often the question looks for the operator's answer.
const ANSWER_POLL: Duration = Duration::from_millis(50);
/// A VERSION or .sha256 asset is one short line.
const SMALL_FILE_MAX_BYTES: u64 = 1024;

/// What a release means for the archon that is running.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    UpToDate,
    /// This archon is newer than the latest release.
    Ahead,
    Upgrade,
    /// Only a release asked for by version replaces a newer archon.
    Downgrade,
}

impl Step {
    fn decide(current: &Version, wanted: &Version, pinned: bool) -> Self {
        match (wanted.cmp(current), pinned) {
            (Ordering::Equal, _) => Self::UpToDate,
            (Ordering::Greater, _) => Self::Upgrade,
            (Ordering::Less, true) => Self::Downgrade,
            (Ordering::Less, false) => Self::Ahead,
        }
    }

    /// What an install of this step is called while it runs and once done.
    fn words(self) -> (&'static str, &'static str) {
        match self {
            Self::Downgrade => ("Downgrading", "Downgraded"),
            _ => ("Updating", "Updated"),
        }
    }
}

/// Before `archon`, `archon chat` or `archon tui` opens: check for newer
/// releases and offer the one-step update. After an accepted update this
/// process is replaced by the new archon with the same arguments.
pub async fn before_launch() -> anyhow::Result<()> {
    use std::os::unix::process::CommandExt as _;
    // Resolved first: once the update replaces it, the running file has no path.
    let bin = archon_bin()?;
    // A broken config is reported by the command that follows.
    let Ok(config) = Config::load_default() else {
        return Ok(());
    };
    if launch::offer(&config.update).await != launch::Launch::Again {
        return Ok(());
    }
    let error = std::process::Command::new(bin)
        .args(std::env::args_os().skip(1))
        .env(launch::NO_CHECK_ENV, "1")
        .exec();
    Err(error).context("starting the updated archon")
}

/// What `archon update` prints. Under `--json --yes` the install report is
/// kept, so the restart adds to it and one object is printed.
struct Report {
    json: bool,
    kept: Option<serde_json::Value>,
    defer: bool,
}

impl Report {
    fn say(&mut self, value: serde_json::Value, text: &str) -> anyhow::Result<()> {
        if self.json && self.defer {
            self.kept = Some(value);
            return Ok(());
        }
        emit(self.json, || Ok(value), text)
    }
}

pub async fn run(args: UpdateArgs, json: bool) -> anyhow::Result<ExitCode> {
    if args.requirements {
        let own = Requirement::own();
        emit(json, || to_json(&own), &own.render())?;
        return Ok(ExitCode::SUCCESS);
    }
    let mut report = Report {
        json,
        kept: None,
        defer: args.yes,
    };
    // Listened for before anything is staged, so no interrupt leaves it behind.
    let mut interrupts = Interrupts::listen()?;
    // Refused before any request: an archon that cannot be replaced asks nothing.
    let place = match args.check {
        true => None,
        false => Some(Place::of_this_archon()?),
    };
    let prepared = tokio::select! {
        biased;
        // The preparation is dropped where it waited, which kills its download.
        interrupt = interrupts.next() => return interrupted(interrupt, place),
        prepared = prepare(&args, place.as_ref(), &mut report) => prepared?,
    };
    // Nothing here waits: the swap and its report end whole.
    let (code, installed) = match (prepared, place) {
        (Prepared::Reported(code), _) => (code, None),
        (Prepared::Staged(staged), Some(place)) => {
            (commit(&staged, &place, &mut report)?, Some(staged.wanted))
        }
        (Prepared::Staged(_), None) => unreachable!("--check stages nothing"),
    };
    if args.check || args.no_restart {
        return Ok(code);
    }
    let progress = restart::Progress::new();
    let restart = restart_after(&args, &mut report, code, installed, &progress);
    match restart::until_interrupted(restart, &progress, interrupts.next()).await {
        restart::Ended::Done(result) => result.with_context(|| {
            let during = progress.current();
            format!("the update's restart stopped while {}", during.describe())
        }),
        restart::Ended::Interrupted { by, during } => {
            eprintln!(
                "archon: update interrupted by {} while {}. The new archon is installed; {}.",
                by.name(),
                during.describe(),
                during.remedy()
            );
            Ok(ExitCode::from(by.exit_code()))
        }
    }
}

/// Update Aether and restart, after the operator confirms or `--yes`. A run
/// that cannot ask has printed the commands for it, and stops there.
async fn restart_after(
    args: &UpdateArgs,
    report: &mut Report,
    code: ExitCode,
    installed: Option<Version>,
    progress: &restart::Progress,
) -> anyhow::Result<ExitCode> {
    use restart::Steps as _;
    let config = Config::load_default().context("loading the config to restart from")?;
    let plan = restart::Plan {
        installed: installed
            .clone()
            .unwrap_or_else(Version::current)
            .to_string(),
        archon_changed: installed.is_some(),
        dedicated: config.aether.mode == AetherMode::Dedicated,
        drain_secs: args.wait,
        requeue: args.requeue,
    };
    if !args.yes && !can_ask(report.json) {
        return Ok(code);
    }
    // The archond a partial environment reaches may be another setup's.
    super::daemon::refuse_mixed_locations("update")?;
    let running = restart::Live.running_archond().await?;
    if !args.yes && !confirmed(&plan, running.as_ref(), progress).await? {
        return Ok(code);
    }
    let outcome = restart::apply(&restart::Live, &plan, progress).await?;
    let value = serde_json::json!({
        "restarted": outcome.restarted, "steps": outcome.lines, "warnings": outcome.warnings,
    });
    match report.kept.take() {
        Some(mut install) if report.json => {
            install["restart"] = value;
            report.defer = false;
            report.say(install, "")?;
        }
        _ => report.say(value, &outcome.lines.join("\n"))?,
    }
    for warning in &outcome.warnings {
        eprintln!("archon: warning: {warning}");
    }
    Ok(match outcome.warnings.is_empty() {
        true => code,
        false => ExitCode::FAILURE,
    })
}

/// Whether a question can be put: a terminal on both ends, and no `--json`.
fn can_ask(json: bool) -> bool {
    use std::io::IsTerminal as _;
    !json && std::io::stdin().is_terminal() && std::io::stdout().is_terminal()
}

/// Say what will update and restart, and ask. Archon already current still
/// offers `aether update`, which restarts archond only if it changes Aether.
async fn confirmed(
    plan: &restart::Plan,
    running: Option<&restart::Running>,
    progress: &restart::Progress,
) -> anyhow::Result<bool> {
    assert!(can_ask(false), "a question needs a terminal on both ends");
    let stale = running
        .is_some_and(|archond| archon_core::ops::bare_version(&archond.version) != plan.installed);
    if plan.archon_changed || stale {
        println!("{}", restart::announce(plan, running));
        print!("Update Aether and restart now? [Y/n] ");
    } else {
        println!(
            "archon {} is current. Aether is updated with `aether update`; archond restarts \
             only if that changes Aether.",
            plan.installed
        );
        print!("Check Aether now? [Y/n] ");
    }
    std::io::Write::flush(&mut std::io::stdout()).context("writing the question")?;
    progress.mark(restart::Phase::Asking);
    let answer = read_answer(std::io::stdin().as_fd()).await?;
    Ok(restart::is_yes(&answer))
}

/// One line from `input`, up to its line end or end of input, gathered from
/// reads taken only when they return at once: a blocked read would hold up
/// a signal's exit, and a terminal's Ctrl-D hands over half a line.
async fn read_answer(input: BorrowedFd<'_>) -> anyhow::Result<String> {
    let (mut answer, mut chunk) = (Vec::new(), [0_u8; 256]);
    loop {
        if !has_input(input)? {
            tokio::time::sleep(ANSWER_POLL).await;
            continue;
        }
        let read = match rustix::io::read(input, &mut chunk[..]) {
            Ok(read) => read,
            Err(rustix::io::Errno::INTR | rustix::io::Errno::AGAIN) => continue,
            Err(error) => return Err(error).context("reading the answer"),
        };
        assert!(read <= chunk.len(), "a read fills at most its buffer");
        answer.extend_from_slice(&chunk[..read]);
        if read == 0 || chunk[..read].contains(&b'\n') {
            return Ok(String::from_utf8_lossy(&answer).into_owned());
        }
    }
}

/// Whether a read of `input` returns at once.
fn has_input(input: BorrowedFd<'_>) -> anyhow::Result<bool> {
    use rustix::event::{PollFd, PollFlags, Timespec, poll};
    let mut fds = [PollFd::new(&input, PollFlags::IN)];
    let now = Timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    match poll(&mut fds, Some(&now)) {
        Ok(ready) => Ok(ready > 0),
        // The signal that interrupted it is the select's to handle.
        Err(rustix::io::Errno::INTR) => Ok(false),
        Err(error) => Err(error).context("waiting for the answer"),
    }
}

/// How the part of an update that a signal may still stop has ended.
#[derive(Debug)]
enum Prepared {
    /// Nothing is installed, and the release's standing was reported.
    Reported(ExitCode),
    Staged(Staged),
}

/// A verified release in the staging directory with everything its report
/// needs, so that only the swap is left.
#[derive(Debug)]
struct Staged {
    binaries: PathBuf,
    step: Step,
    current: Version,
    wanted: Version,
    aether: (Option<bool>, String),
    archond: (Option<bool>, String),
}

/// Remove what the interrupted update staged, say so, and exit as a shell
/// reports the signal.
fn interrupted(interrupt: Interrupt, place: Option<Place>) -> anyhow::Result<ExitCode> {
    let mut line = format!("archon: update interrupted by {}", interrupt.name());
    if let Some(Place { dir, staging }) = place {
        let path = staging.path().to_path_buf();
        staging
            .close()
            .with_context(|| format!("removing {} after {}", path.display(), interrupt.name()))?;
        line.push_str(&format!("; nothing staged is left in {}", dir.display()));
    }
    eprintln!("{line}.");
    Ok(ExitCode::from(interrupt.exit_code()))
}

/// Everything before the swap: `place` is where to stage, `None` to only
/// report. Changes nothing outside the staging directory.
async fn prepare(
    args: &UpdateArgs,
    place: Option<&Place>,
    report: &mut Report,
) -> anyhow::Result<Prepared> {
    assert_eq!(
        place.is_none(),
        args.check,
        "only --check runs without a place to install in"
    );
    let current = Version::current();
    let source = Source::from_env()?;
    let wanted = match &args.version {
        Some(text) => text.parse()?,
        None => latest(&source).await?,
    };
    let step = Step::decide(&current, &wanted, args.version.is_some());
    let installs = matches!(step, Step::Upgrade | Step::Downgrade);
    if let Some(place) = place.filter(|_| installs) {
        let restarts = args.yes && !args.no_restart;
        let staged = stage(
            &source,
            place,
            step,
            (current, wanted),
            report.json,
            restarts,
        )
        .await?;
        return Ok(Prepared::Staged(staged));
    }
    let (status, text) = standing(step, &current, &wanted);
    let value = serde_json::json!({
        "status": status, "current": current.to_string(), "release": wanted.to_string(),
    });
    report.say(value, &text)?;
    let available = args.check && step == Step::Upgrade;
    let code = if available { UPDATE_AVAILABLE } else { 0 };
    Ok(Prepared::Reported(ExitCode::from(code)))
}

/// The directory this archon is installed in, with a staging directory
/// in it: proof that an update may write there. No older one is left in it.
#[derive(Debug)]
struct Place {
    dir: PathBuf,
    staging: tempfile::TempDir,
}

impl Place {
    fn of_this_archon() -> anyhow::Result<Self> {
        let exe = archon_bin()?;
        if let Some(refused) = location::refusal(&exe, location::cargo_bin_dir().as_deref()) {
            anyhow::bail!(refused);
        }
        let dir = exe
            .parent()
            .context("the archon executable has no directory")?
            .to_path_buf();
        let staging = location::stage_in(&dir)?;
        location::remove_stale(&dir, staging.path(), SystemTime::now())?;
        Ok(Self { dir, staging })
    }
}

/// The status name and the line for a release that is not being installed.
fn standing(step: Step, current: &Version, wanted: &Version) -> (&'static str, String) {
    match step {
        Step::UpToDate => ("up_to_date", format!("archon {current} is up to date.")),
        Step::Ahead => (
            "ahead",
            format!("archon {current} is newer than the latest release, {wanted}; nothing to do."),
        ),
        Step::Upgrade => (
            "update_available",
            format!("archon {wanted} is available; this is {current}. Run `archon update`."),
        ),
        Step::Downgrade => unreachable!("--check takes no version, so it never downgrades"),
    }
}

/// The latest release's version, from its `VERSION` asset.
async fn latest(source: &Source) -> anyhow::Result<Version> {
    let dir = tempfile::tempdir().context("creating a temporary directory")?;
    let path = dir.path().join("VERSION");
    let url = source.latest_version_url();
    tools::download(&url, &path, SMALL_FILE_TIMEOUT).await?;
    let text = read_small(&path).with_context(|| format!("reading {url}"))?;
    text.trim()
        .parse()
        .with_context(|| format!("{url} does not name a release"))
}

fn read_small(path: &Path) -> anyhow::Result<String> {
    let size = std::fs::metadata(path)?.len();
    anyhow::ensure!(
        size <= SMALL_FILE_MAX_BYTES,
        "{size} bytes where one short line was expected"
    );
    Ok(std::fs::read_to_string(path)?)
}

/// Download, verify and diagnose `wanted`, all before the swap: a signal that
/// ends any of it leaves the installed archon as it was. `restarts`: `--yes`
/// restarts archond next, so its report names no command to run by hand.
async fn stage(
    source: &Source,
    place: &Place,
    step: Step,
    (current, wanted): (Version, Version),
    json: bool,
    restarts: bool,
) -> anyhow::Result<Staged> {
    assert_ne!(current, wanted, "an equal version is never installed");
    assert!(
        matches!(step, Step::Upgrade | Step::Downgrade),
        "{step:?} installs nothing"
    );
    let Place { dir, staging } = place;
    if !json {
        let doing = step.words().0;
        println!("{doing} archon {current} to {wanted} in {}.", dir.display());
    }
    let binaries = stage_release(source, &wanted, staging.path())
        .await
        .with_context(|| format!("archon {current} is unchanged; installing {wanted} failed"))?;
    let needed = Requirement::of(&binaries.join(BINARIES[0])).await;
    let aether = aether_report(&wanted, needed).await;
    let archond = archond_report(&wanted, restarts).await;
    Ok(Staged {
        binaries,
        step,
        current,
        wanted,
        aether,
        archond,
    })
}

/// Swap the staged release in and report it. Nothing here waits, so no
/// signal can end an update between its swap and its report.
fn commit(staged: &Staged, place: &Place, report: &mut Report) -> anyhow::Result<ExitCode> {
    let Staged {
        binaries,
        step,
        current,
        wanted,
        aether: (aether_ok, aether_line),
        archond: (running, archond_line),
    } = staged;
    assert!(
        binaries.starts_with(place.staging.path()),
        "{} was not staged for {}",
        binaries.display(),
        place.dir.display()
    );
    assert_ne!(current, wanted, "an equal version is never installed");
    location::replace(binaries, &place.dir, BINARIES)?;

    let done = step.words().1;
    let value = serde_json::json!({
        "status": done.to_lowercase(), "current": wanted.to_string(),
        "previous": current.to_string(), "directory": place.dir,
        "archond_running": running, "aether_compatible": aether_ok,
    });
    let text = format!("{done} archon {current} to {wanted}.\n{archond_line}\n{aether_line}");
    report.say(value, &text)?;
    Ok(ExitCode::SUCCESS)
}

/// Download the archive for this machine into `staging`, verify it against
/// the published SHA-256, and unpack binaries that report `version`.
async fn stage_release(
    source: &Source,
    version: &Version,
    staging: &Path,
) -> anyhow::Result<PathBuf> {
    let name = release::archive_name(release::current_target()?);
    let url = source.asset_url(version, &name);
    let (archive, sum) = (staging.join(&name), staging.join(format!("{name}.sha256")));
    tools::download(&url, &archive, ARCHIVE_TIMEOUT).await?;
    tools::download(&format!("{url}.sha256"), &sum, SMALL_FILE_TIMEOUT).await?;
    let published = read_small(&sum)
        .and_then(|text| tools::parse_checksum(&text))
        .with_context(|| format!("reading {url}.sha256"))?;
    let downloaded = tools::sha256(&archive).await?;
    anyhow::ensure!(
        published == downloaded,
        "checksum mismatch for {url}: published {published}, downloaded {downloaded}"
    );
    let unpacked = staging.join("unpacked");
    std::fs::create_dir(&unpacked).with_context(|| format!("creating {}", unpacked.display()))?;
    tools::unpack(&archive, &unpacked, BINARIES).await?;
    let reported = reported_version(&unpacked.join(BINARIES[0])).await?;
    anyhow::ensure!(
        &reported == version,
        "the archon in {url} reports version {reported}, not {version}"
    );
    Ok(unpacked)
}

/// The version `archon --version` prints: proof the binary runs here.
async fn reported_version(archon: &Path) -> anyhow::Result<Version> {
    let output = tools::run(
        archon,
        &[std::ffi::OsStr::new("--version")],
        SMALL_FILE_TIMEOUT,
    )
    .await
    .context("the downloaded archon does not run on this machine")?;
    let line = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    line.strip_prefix("archon ")
        .with_context(|| format!("the downloaded archon printed `{line}` as its version"))?
        .parse()
}

/// Whether the installed Aether serves what the new archon speaks, and the
/// line that says so. Never fails: what cannot be checked stops no update.
async fn aether_report(
    new: &Version,
    needed: anyhow::Result<Requirement>,
) -> (Option<bool>, String) {
    let unknown = |why: anyhow::Error| {
        let line = format!("Aether: could not be checked ({why:#}); run `archon doctor`.");
        (None, line)
    };
    let needed = match needed {
        Ok(needed) => needed,
        Err(error) => return unknown(error),
    };
    match aether::installed().await {
        Ok(served) => {
            let (ok, line) = aether::verdict(&format!("archon {new}"), needed, served.as_ref());
            (Some(ok), line)
        }
        Err(error) => unknown(error),
    }
}

/// Whether an archond is running, and what to do about it after an update.
/// Never fails: what cannot be asked stops no update.
async fn archond_report(new: &Version, restarts: bool) -> (Option<bool>, String) {
    const BY_HAND: &str = "run `archon daemon stop --keep-aether`, then `archon aether mcp refresh archon` \
                           and `archon orchestrator restart`";
    const BELOW: &str = "`--yes` restarts it now";
    let restart = if restarts { BELOW } else { BY_HAND };
    match connect_existing().await {
        Ok(Some(client)) => {
            let welcome = client.welcome();
            let line = format!(
                "archond (pid {}) still runs {}, and the update itself interrupts no running \
                 item; to switch it to {new}, {restart}.",
                welcome.pid, welcome.daemon_version
            );
            (Some(true), line)
        }
        Ok(None) => {
            let line = format!(
                "archond is not running; `archon daemon start`, or the next command that \
                 needs it, starts {new}."
            );
            (Some(false), line)
        }
        Err(error) => {
            let line = format!(
                "archond could not be asked whether it runs ({error:#}); if it does, {restart}."
            );
            (None, line)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn version(text: &str) -> Version {
        text.parse().unwrap()
    }

    /// A terminal's two ends; the reading end does not block, so a reader
    /// that waits for a whole line fails here rather than hangs.
    fn terminal_pair() -> (
        std::os::unix::net::UnixStream,
        std::os::unix::net::UnixStream,
    ) {
        let (operator, terminal) = std::os::unix::net::UnixStream::pair().unwrap();
        terminal.set_nonblocking(true).unwrap();
        (operator, terminal)
    }

    #[tokio::test]
    async fn half_an_answer_leaves_the_wait_open_to_a_signal() {
        use std::io::Write as _;
        let (mut operator, terminal) = terminal_pair();
        operator.write_all(b"n").unwrap();
        let wait = Duration::from_millis(300);
        let waiting = tokio::time::timeout(wait, read_answer(terminal.as_fd())).await;
        assert!(waiting.is_err(), "no line end yet: {waiting:?}");

        let (mut operator, terminal) = terminal_pair();
        operator.write_all(b"n").unwrap();
        drop(operator);
        let ended = tokio::time::timeout(wait, read_answer(terminal.as_fd())).await;
        assert_eq!(ended.expect("input ended").unwrap(), "n");
    }

    #[tokio::test]
    async fn an_answer_is_read_to_its_line_end() {
        use std::io::Write as _;
        let (mut operator, terminal) = terminal_pair();
        let answering = async {
            tokio::time::sleep(ANSWER_POLL * 2).await;
            operator.write_all(b"no\n").unwrap();
        };
        let wait = Duration::from_secs(5);
        let (answer, ()) = tokio::join!(
            tokio::time::timeout(wait, read_answer(terminal.as_fd())),
            answering
        );
        assert_eq!(answer.expect("a whole line").unwrap(), "no\n");
        assert!(!restart::is_yes("no\n"));
    }

    #[test]
    fn only_a_pinned_older_release_is_a_downgrade() {
        let current = version("1.2.0");
        let decide = |wanted: &str, pinned| Step::decide(&current, &version(wanted), pinned);
        assert_eq!(decide("1.2.0", false), Step::UpToDate);
        assert_eq!(decide("v1.2.0", true), Step::UpToDate);
        assert_eq!(decide("1.10.0", false), Step::Upgrade);
        assert_eq!(decide("1.2.1", true), Step::Upgrade);
        assert_eq!(decide("1.1.9", false), Step::Ahead);
        assert_eq!(decide("1.1.9", true), Step::Downgrade);
        assert_eq!(decide("1.2.0-rc.1", false), Step::Ahead);
    }

    #[test]
    fn each_standing_names_both_versions_or_that_nothing_changes() {
        let (current, wanted) = (version("1.0.0"), version("1.1.0"));
        let (status, text) = standing(Step::Upgrade, &current, &wanted);
        assert_eq!(status, "update_available");
        assert!(
            text.contains("archon 1.1.0 is available; this is 1.0.0"),
            "{text}"
        );
        assert!(text.contains("`archon update`"), "{text}");
        let (status, text) = standing(Step::UpToDate, &current, &current);
        assert_eq!(
            (status, text.as_str()),
            ("up_to_date", "archon 1.0.0 is up to date.")
        );
        let (status, text) = standing(Step::Ahead, &wanted, &current);
        assert_eq!(status, "ahead");
        assert!(
            text.contains("archon 1.1.0 is newer than the latest release, 1.0.0"),
            "{text}"
        );
    }

    #[test]
    fn a_small_file_is_read_and_a_large_one_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("VERSION");
        std::fs::write(&path, "v1.2.3\n").unwrap();
        assert_eq!(read_small(&path).unwrap(), "v1.2.3\n");
        std::fs::write(&path, "x".repeat(2000)).unwrap();
        let error = read_small(&path).unwrap_err().to_string();
        assert!(error.contains("2000 bytes"), "{error}");
        assert!(read_small(&dir.path().join("missing")).is_err());
    }
}
