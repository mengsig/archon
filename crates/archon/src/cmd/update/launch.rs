//! The release check of every launch of `archon` (TUI or chat): ask GitHub
//! for the latest Archon and Aether, briefly and at most once per
//! `[update] cache_for`, and offer a one-step update when one is newer. It
//! stays silent offline and for a version the operator skipped.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::Context as _;
use serde::{Deserialize, Serialize};

use super::release::Source;
use super::restart::is_yes;
use super::version::Version;
use crate::cli::UpdateArgs;
use crate::cmd::doctor::find_program;
use crate::instance::AETHER_PROGRAM;

/// The longest a launch waits for GitHub; an answer after it is not used.
const CHECK_BUDGET: Duration = Duration::from_millis(1_500);
/// After a failed check (offline), how long before the next launch asks again.
const RETRY_AFTER_FAILURE: u64 = 10 * 60;
const CACHE_FILE: &str = "update-check.json";
/// Set for the archon an accepted update starts again, and by operators who
/// want no check in one shell.
pub(crate) const NO_CHECK_ENV: &str = "ARCHON_NO_UPDATE_CHECK";

/// What the last checks learned; kept in the data directory.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub(super) struct Cache {
    /// Unix seconds of the last check that learned something.
    pub checked_at: u64,
    /// Unix seconds of the last check that failed.
    pub failed_at: u64,
    pub archon: Option<String>,
    pub aether: Option<String>,
    /// `archon 1.4.2` or `aether 1.0.8`: releases the operator skipped.
    pub skipped: Vec<String>,
}

/// Where the latest releases come from; faked in tests.
pub(super) trait Releases {
    async fn archon(&self) -> anyhow::Result<String>;
    async fn aether(&self) -> anyhow::Result<String>;
}

/// A newer release than the one installed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Offer {
    pub program: &'static str,
    pub current: String,
    pub latest: String,
}

impl Offer {
    fn key(&self) -> String {
        format!("{} {}", self.program, self.latest)
    }
}

/// Ask again only when the answer is older than `ttl` seconds and the last
/// failure is not recent. A failed ask keeps what was known.
pub(super) async fn refresh(
    cache: Cache,
    now: u64,
    ttl: u64,
    releases: &impl Releases,
    budget: Duration,
) -> Cache {
    let fresh = now.saturating_sub(cache.checked_at) < ttl;
    let failed_lately = now.saturating_sub(cache.failed_at) < RETRY_AFTER_FAILURE;
    if fresh || failed_lately {
        return cache;
    }
    let asked = tokio::time::timeout(budget, async {
        tokio::join!(releases.archon(), releases.aether())
    })
    .await;
    let Ok((archon, aether)) = asked else {
        return Cache {
            failed_at: now,
            ..cache
        };
    };
    let mut next = cache;
    if archon.is_err() || aether.is_err() {
        next.failed_at = now;
    }
    if archon.is_ok() || aether.is_ok() {
        next.checked_at = now;
    }
    next.archon = archon.ok().or(next.archon);
    next.aether = aether.ok().or(next.aether);
    next
}

/// The releases in `cache` newer than what is installed and not skipped.
pub(super) fn offers(cache: &Cache, archon: &Version, aether: Option<&Version>) -> Vec<Offer> {
    let newer = |program: &'static str, latest: &Option<String>, current: Option<&Version>| {
        let latest_text = latest.as_deref()?;
        let (latest, current) = (latest_text.parse::<Version>().ok()?, current?);
        (latest > *current).then(|| Offer {
            program,
            current: current.to_string(),
            latest: latest.to_string(),
        })
    };
    [
        newer("archon", &cache.archon, Some(archon)),
        newer("aether", &cache.aether, aether),
    ]
    .into_iter()
    .flatten()
    .filter(|offer| !cache.skipped.contains(&offer.key()))
    .collect()
}

/// The question that offers `offers`: what is new and where to read about it,
/// what the update does to running work, and the one-key answers.
pub(super) fn question(offers: &[Offer], source: &Source) -> String {
    assert!(!offers.is_empty(), "nothing is offered without a release");
    let mut text = String::new();
    for o in offers {
        let program = o.program[..1].to_uppercase() + &o.program[1..];
        text.push_str(&format!(
            "{program} {} is available (this is {}).\n",
            o.latest, o.current
        ));
        let notes = o
            .latest
            .parse::<Version>()
            .ok()
            .filter(|_| o.program == "archon");
        if let Some(version) = notes {
            text.push_str(&format!("  What's new: {}\n", source.notes_url(&version)));
        }
    }
    text.push_str(
        "Updating restarts archond at once; running agents keep running in Aether \
         (`archon update` does the same later).\n\
         Update and restart now? [Y/n/s] (s: skip these versions) ",
    );
    text
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Choice {
    Update,
    Later,
    Skip,
}

pub(super) fn choice(answer: &str) -> Choice {
    match answer.trim().to_ascii_lowercase().chars().next() {
        Some('s') => Choice::Skip,
        _ if is_yes(answer) => Choice::Update,
        _ => Choice::Later,
    }
}

/// The releases on the network, asked of the installer's assets and of `aether`.
struct Network {
    source: Source,
}

impl Releases for Network {
    async fn archon(&self) -> anyhow::Result<String> {
        super::latest(&self.source, super::SMALL_FILE_TIMEOUT)
            .await
            .map(|v| v.to_string())
    }

    async fn aether(&self) -> anyhow::Result<String> {
        let aether = find_program(Path::new(AETHER_PROGRAM)).context("no aether on PATH")?;
        let args = ["update", "--check", "--json"];
        let output = tokio::process::Command::new(&aether)
            .args(args.map(OsStr::new))
            .stdin(Stdio::null())
            .kill_on_drop(true)
            .output()
            .await
            .with_context(|| format!("running `{} update --check`", aether.display()))?;
        // 10: a newer release exists; both exits print the same object.
        anyhow::ensure!(
            matches!(output.status.code(), Some(0 | 10)),
            "`aether update --check` failed ({})",
            output.status
        );
        let value: serde_json::Value = serde_json::from_slice(&output.stdout)
            .context("reading `aether update --check --json`")?;
        let latest = value["latest"]
            .as_str()
            .context("no `latest` release named")?;
        Ok(latest.trim_start_matches('v').to_owned())
    }
}

/// The aether on PATH, by `aether --version`; `None` when it cannot be read.
async fn installed_aether() -> Option<Version> {
    let aether = find_program(Path::new(AETHER_PROGRAM))?;
    let output = tokio::process::Command::new(aether)
        .arg("--version")
        .stdin(Stdio::null())
        .kill_on_drop(true)
        .output();
    let output = tokio::time::timeout(CHECK_BUDGET, output)
        .await
        .ok()?
        .ok()?;
    let line = String::from_utf8_lossy(&output.stdout);
    line.trim().rsplit(' ').next()?.parse().ok()
}

fn cache_path() -> PathBuf {
    archon_core::paths::data_dir().join(CACHE_FILE)
}

fn load(path: &Path) -> Cache {
    match std::fs::read_to_string(path) {
        Ok(text) => serde_json::from_str(&text).unwrap_or_else(|error| {
            tracing::warn!(%error, path = %path.display(), "update-check cache unreadable; ignored");
            Cache::default()
        }),
        Err(_) => Cache::default(),
    }
}

fn store(path: &Path, cache: &Cache) -> anyhow::Result<()> {
    let dir = path.parent().context("the cache has no directory")?;
    let dir = std::path::absolute(dir).context("resolving the data directory")?;
    crate::instance::create_private_dir(&dir)?;
    let part = path.with_extension("json.part");
    std::fs::write(&part, serde_json::to_vec(cache)?)
        .with_context(|| format!("writing {}", part.display()))?;
    std::fs::rename(&part, path).with_context(|| format!("replacing {}", path.display()))
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_secs())
}

/// What a launch does after the offer.
#[derive(Debug, PartialEq)]
pub enum Launch {
    Go,
    /// The update installed a new archon: run it in this one's place.
    Again,
    /// A signal ended the update: exit with this code, opening nothing.
    Exit(std::process::ExitCode),
}

/// Check, and offer, for a launch on a terminal. Never fails the launch: a
/// check that cannot run is silent, an update that fails is reported.
pub async fn offer(config: &archon_core::config::UpdateConfig) -> Launch {
    use std::io::IsTerminal as _;
    let quiet = !config.check_on_launch
        || std::env::var_os(NO_CHECK_ENV).is_some()
        || !std::io::stdin().is_terminal()
        || !std::io::stdout().is_terminal();
    if quiet {
        return Launch::Go;
    }
    let Ok(source) = Source::from_env() else {
        return Launch::Go;
    };
    let path = cache_path();
    let before = load(&path);
    let ttl = config.cache_for.secs();
    let network = Network { source };
    // Concurrent, each bounded by the same budget: a launch waits for it once.
    let (cache, installed) = tokio::join!(
        refresh(before.clone(), unix_now(), ttl, &network, CHECK_BUDGET),
        installed_aether(),
    );
    if cache != before
        && let Err(error) = store(&path, &cache)
    {
        tracing::warn!(error = %format!("{error:#}"), "update-check cache not saved");
    }
    let found = offers(&cache, &Version::current(), installed.as_ref());
    if found.is_empty() {
        return Launch::Go;
    }
    ask(&path, cache, &found, &network.source).await
}

async fn ask(path: &Path, mut cache: Cache, found: &[Offer], source: &Source) -> Launch {
    print!("{}", question(found, source));
    let _ = std::io::Write::flush(&mut std::io::stdout());
    let mut answer = String::new();
    if let Err(error) = std::io::stdin().read_line(&mut answer) {
        eprintln!("archon: reading the answer: {error}; not updating");
        return Launch::Go;
    }
    match choice(&answer) {
        Choice::Later => Launch::Go,
        Choice::Skip => {
            cache.skipped.extend(found.iter().map(Offer::key));
            if let Err(error) = store(path, &cache) {
                eprintln!("archon: the skip was not saved: {error:#}");
            }
            Launch::Go
        }
        Choice::Update => match super::run(UpdateArgs::everything(), false).await {
            Ok(code) if code == std::process::ExitCode::SUCCESS => Launch::Again,
            Ok(code) if super::interrupt::Interrupt::ended(code).is_some() => Launch::Exit(code),
            Ok(_) => {
                eprintln!("archon: the update did not finish cleanly; opening anyway");
                Launch::Go
            }
            Err(error) => {
                eprintln!("archon: the update failed: {error:#}; opening anyway");
                Launch::Go
            }
        },
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::*;

    struct Fake {
        asked: Cell<u32>,
        archon: Result<&'static str, &'static str>,
        aether: Result<&'static str, &'static str>,
        stall: bool,
    }

    impl Fake {
        fn answering(archon: &'static str, aether: &'static str) -> Self {
            Self {
                asked: Cell::new(0),
                archon: Ok(archon),
                aether: Ok(aether),
                stall: false,
            }
        }
    }

    impl Releases for Fake {
        async fn archon(&self) -> anyhow::Result<String> {
            self.asked.set(self.asked.get() + 1);
            if self.stall {
                std::future::pending::<()>().await;
            }
            self.archon
                .map(str::to_owned)
                .map_err(|e| anyhow::anyhow!(e))
        }

        async fn aether(&self) -> anyhow::Result<String> {
            self.aether
                .map(str::to_owned)
                .map_err(|e| anyhow::anyhow!(e))
        }
    }

    const HOUR: u64 = 3_600;
    const BUDGET: Duration = Duration::from_millis(200);

    fn version(text: &str) -> Version {
        text.parse().unwrap()
    }

    #[tokio::test]
    async fn a_fresh_answer_is_reused_and_a_stale_one_is_asked_again() {
        let fake = Fake::answering("1.4.2", "1.0.8");
        let first = refresh(Cache::default(), 10_000, HOUR, &fake, BUDGET).await;
        assert_eq!(first.archon.as_deref(), Some("1.4.2"));
        assert_eq!((first.checked_at, fake.asked.get()), (10_000, 1));
        let again = refresh(first.clone(), 10_000 + HOUR - 1, HOUR, &fake, BUDGET).await;
        assert_eq!(
            (again, fake.asked.get()),
            (first.clone(), 1),
            "within the ttl"
        );
        refresh(first, 10_000 + HOUR, HOUR, &fake, BUDGET).await;
        assert_eq!(fake.asked.get(), 2, "at the ttl");
    }

    #[tokio::test]
    async fn offline_is_silent_keeps_what_was_known_and_retries_later() {
        let known = Cache {
            checked_at: 1,
            archon: Some("1.4.2".into()),
            ..Cache::default()
        };
        let mut fake = Fake::answering("x", "x");
        fake.archon = Err("offline");
        fake.aether = Err("offline");
        let failed = refresh(known.clone(), 100_000, HOUR, &fake, BUDGET).await;
        assert_eq!(failed.archon.as_deref(), Some("1.4.2"));
        assert_eq!((failed.checked_at, failed.failed_at), (1, 100_000));
        let soon = refresh(
            failed.clone(),
            100_000 + RETRY_AFTER_FAILURE - 1,
            HOUR,
            &fake,
            BUDGET,
        )
        .await;
        assert_eq!(
            (soon, fake.asked.get()),
            (failed.clone(), 1),
            "not hammered"
        );
        refresh(failed, 100_000 + RETRY_AFTER_FAILURE, HOUR, &fake, BUDGET).await;
        assert_eq!(fake.asked.get(), 2, "retried later");
    }

    #[tokio::test]
    async fn a_check_that_stalls_ends_at_its_budget_and_counts_as_failed() {
        let mut fake = Fake::answering("1.4.2", "1.0.8");
        fake.stall = true;
        let started = std::time::Instant::now();
        let cache = refresh(Cache::default(), 50_000, HOUR, &fake, BUDGET).await;
        assert!(started.elapsed() < Duration::from_secs(5));
        assert_eq!((cache.failed_at, cache.archon), (50_000, None));
    }

    #[test]
    fn only_newer_unskipped_releases_are_offered() {
        let cache = Cache {
            archon: Some("1.4.2".into()),
            aether: Some("1.0.8".into()),
            ..Cache::default()
        };
        let found = offers(&cache, &version("1.4.1"), Some(&version("1.0.7")));
        let named: Vec<_> = found.iter().map(Offer::key).collect();
        assert_eq!(named, ["archon 1.4.2", "aether 1.0.8"]);
        assert!(offers(&cache, &version("1.4.2"), Some(&version("1.0.9"))).is_empty());
        assert_eq!(offers(&cache, &version("1.4.1"), None).len(), 1);
        let skipped = Cache {
            skipped: vec!["archon 1.4.2".into()],
            ..cache
        };
        let found = offers(&skipped, &version("1.4.1"), Some(&version("1.0.7")));
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].program, "aether");
        assert!(offers(&Cache::default(), &version("1.4.1"), None).is_empty());
    }

    #[test]
    fn the_question_names_each_release_and_the_answers_map_to_choices() {
        let found = [Offer {
            program: "archon",
            current: "1.4.1".into(),
            latest: "1.4.2".into(),
        }];
        let source = Source::new("https://example.test/releases/").unwrap();
        let text = question(&found, &source);
        assert!(
            text.starts_with("Archon 1.4.2 is available (this is 1.4.1)."),
            "{text}"
        );
        assert!(
            text.contains("What's new: https://example.test/releases/tag/v1.4.2"),
            "{text}"
        );
        assert!(!text.contains("waits for running agents"), "{text}");
        assert!(text.contains("running agents keep running"), "{text}");
        assert!(text.contains("Update and restart now? [Y/n/s]"), "{text}");
        assert_eq!(choice(""), Choice::Update);
        assert_eq!(choice("Yes\n"), Choice::Update);
        assert_eq!(choice("n"), Choice::Later);
        assert_eq!(choice(" S "), Choice::Skip);
    }

    #[test]
    fn the_cache_survives_a_round_trip_and_a_broken_file_is_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub").join(CACHE_FILE);
        let cache = Cache {
            checked_at: 5,
            skipped: vec!["aether 1.0.8".into()],
            ..Cache::default()
        };
        store(&path, &cache).unwrap();
        assert_eq!(load(&path), cache);
        std::fs::write(&path, "not json").unwrap();
        assert_eq!(load(&path), Cache::default());
        assert_eq!(load(&dir.path().join("absent.json")), Cache::default());
    }
}
