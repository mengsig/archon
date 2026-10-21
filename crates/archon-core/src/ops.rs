//! Operator upkeep of Archon itself: the drained upgrade and restart
//! (`archon daemon upgrade`, the `/upgrade` chat command) and the versions
//! `status` compares.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::error::ArchonError;

/// How long an upgrade waits for running stage agents unless told otherwise.
pub const DEFAULT_DRAIN_SECS: u64 = 30 * 60;
/// The longest wait an upgrade accepts.
pub const MAX_DRAIN_SECS: u64 = 24 * 60 * 60;
/// Longest version an upgrade accepts, e.g. `v1.2.3-rc.1`.
const MAX_VERSION_CHARS: usize = 40;

/// What an upgrade installs for one program.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UpgradeTarget {
    /// The latest published release.
    Latest,
    /// The `main` branch, built by `cargo install --git`.
    Main,
    /// This release, without a leading `v`.
    Version(String),
}

impl UpgradeTarget {
    /// `latest`, `main`, or a release such as `1.2.3` or `v1.2.3-rc.1`. A
    /// version becomes a program argument, so only a plain one is accepted.
    pub fn parse(text: &str) -> Result<Self, ArchonError> {
        let text = text.trim();
        match text.to_ascii_lowercase().as_str() {
            "latest" => return Ok(Self::Latest),
            "main" => return Ok(Self::Main),
            _ => {}
        }
        let version = text.strip_prefix(['v', 'V']).unwrap_or(text);
        let plain = version.len() <= MAX_VERSION_CHARS
            && version.starts_with(|c: char| c.is_ascii_digit())
            && version
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '+'));
        if !plain {
            return Err(ArchonError::Invalid(format!(
                "{text:?} is not `latest`, `main` or a release version such as 1.2.3"
            )));
        }
        Ok(Self::Version(version.to_owned()))
    }
}

impl fmt::Display for UpgradeTarget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Latest => f.write_str("the latest release"),
            Self::Main => f.write_str("main"),
            Self::Version(version) => write!(f, "{version}"),
        }
    }
}

/// An operator's upgrade: drain, install what is named, restart archond
/// and, in dedicated mode, Archon's own aetherd. With neither program
/// named it is a drained restart.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpgradeRequest {
    #[serde(default)]
    pub archon: Option<UpgradeTarget>,
    #[serde(default)]
    pub aether: Option<UpgradeTarget>,
    /// Longest wait for running stage agents to finish.
    pub drain_secs: u64,
    /// When the wait runs out, restart anyway and let the interrupted stages
    /// be retried, instead of giving the restart up.
    #[serde(default)]
    pub requeue: bool,
    /// Restart archond alone; Archon's own aetherd and the stage agents in
    /// it keep running.
    #[serde(default)]
    pub keep_aether: bool,
}

impl UpgradeRequest {
    pub fn check(&self) -> Result<(), ArchonError> {
        if self.drain_secs == 0 || self.drain_secs > MAX_DRAIN_SECS {
            return Err(ArchonError::Invalid(format!(
                "the drain wait must be 1 to {MAX_DRAIN_SECS} seconds, not {}",
                self.drain_secs
            )));
        }
        if self.keep_aether && self.aether.is_some() {
            return Err(ArchonError::Invalid(
                "a new aether runs only once its aetherd restarts; drop keep_aether".into(),
            ));
        }
        Ok(())
    }

    /// One line naming what is installed, for the feed and replies.
    pub fn describe(&self) -> String {
        let named = [("archon", &self.archon), ("aether", &self.aether)]
            .into_iter()
            .filter_map(|(program, target)| Some(format!("{program} to {}", target.as_ref()?)))
            .collect::<Vec<_>>();
        match named.is_empty() {
            true => "a drained restart".to_owned(),
            false => format!("upgrade {}", named.join(" and ")),
        }
    }
}

/// One program's versions as archond last saw them; `None` is unknown.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProgramVersions {
    /// What runs now: archond itself, or the aetherd it is connected to.
    pub running: Option<String>,
    /// What the binary on disk reports.
    pub installed: Option<String>,
    /// The latest release; known only once the operator checked
    /// (`archon status --check-updates`), as archond never asks by itself.
    pub latest: Option<String>,
}

impl ProgramVersions {
    /// Whether two of the known versions disagree.
    pub fn differ(&self) -> bool {
        let known: Vec<&str> = [&self.running, &self.installed, &self.latest]
            .into_iter()
            .flatten()
            .map(|version| bare_version(version))
            .collect();
        known
            .first()
            .is_some_and(|first| known.iter().any(|v| v != first))
    }

    /// `running 1.0.6, installed 1.0.7, latest 1.0.7`, unknown ones left out.
    pub fn describe(&self) -> String {
        [
            ("running", &self.running),
            ("installed", &self.installed),
            ("latest", &self.latest),
        ]
        .into_iter()
        .filter_map(|(what, version)| Some(format!("{what} {}", version.as_deref()?)))
        .collect::<Vec<_>>()
        .join(", ")
    }
}

impl ProgramVersions {
    /// The latest release when it is newer than what runs or is installed.
    pub fn newer_release(&self) -> Option<&str> {
        let latest = bare_version(self.latest.as_deref()?);
        let latest_core = release_core(latest)?;
        let behind = [&self.running, &self.installed]
            .into_iter()
            .flatten()
            .filter_map(|v| release_core(bare_version(v)))
            .any(|have| have < latest_core);
        behind.then_some(latest)
    }
}

/// `(1, 2, 3)` from a plain `1.2.3`; `None` for a pre-release or anything else.
fn release_core(version: &str) -> Option<(u64, u64, u64)> {
    let mut parts = version.split('.').map(|p| p.parse::<u64>().ok());
    let core = (parts.next()??, parts.next()??, parts.next()??);
    parts.next().is_none().then_some(core)
}

/// Archon's and Aether's versions, for `status` and `queue_status`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Versions {
    pub archon: ProgramVersions,
    pub aether: ProgramVersions,
}

impl Versions {
    pub fn differ(&self) -> bool {
        self.archon.differ() || self.aether.differ()
    }

    /// One line per program whose versions disagree.
    pub fn lines(&self) -> Vec<String> {
        [("archon", &self.archon), ("aether", &self.aether)]
            .into_iter()
            .filter(|(_, versions)| versions.differ())
            .map(|(program, versions)| format!("{program} {}", versions.describe()))
            .collect()
    }
}

impl Versions {
    /// The one command that brings both programs to the latest release,
    /// when either is behind it.
    pub fn update_hint(&self) -> Option<String> {
        let named: Vec<String> = [("archon", &self.archon), ("aether", &self.aether)]
            .into_iter()
            .filter_map(|(program, v)| Some(format!("{program} {}", v.newer_release()?)))
            .collect();
        (!named.is_empty()).then(|| {
            format!(
                "update available ({}); run `archon update` (waits for running agents)",
                named.join(", ")
            )
        })
    }
}

/// `1.2.3` from `1.2.3`, `v1.2.3` or a `--version` line like `archon 1.2.3`.
pub fn bare_version(text: &str) -> &str {
    let last = text.split_whitespace().last().unwrap_or("");
    last.strip_prefix(['v', 'V']).unwrap_or(last)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_target_is_latest_main_or_a_plain_version() {
        assert_eq!(
            UpgradeTarget::parse(" Latest ").unwrap(),
            UpgradeTarget::Latest
        );
        assert_eq!(UpgradeTarget::parse("main").unwrap(), UpgradeTarget::Main);
        let version = |text: &str| UpgradeTarget::parse(text).unwrap();
        assert_eq!(version("v1.2.3"), UpgradeTarget::Version("1.2.3".into()));
        assert_eq!(
            version("1.0.7-rc.1"),
            UpgradeTarget::Version("1.0.7-rc.1".into())
        );
        for bad in [
            "",
            "v",
            "--locked",
            "1.2 3",
            "1.2;rm",
            "x1.2",
            &"1".repeat(41),
        ] {
            assert!(UpgradeTarget::parse(bad).is_err(), "{bad:?} parsed");
        }
    }

    #[test]
    fn a_request_names_what_it_installs_and_bounds_its_wait() {
        let mut request = UpgradeRequest {
            archon: Some(UpgradeTarget::Main),
            aether: Some(UpgradeTarget::Version("1.0.7".into())),
            drain_secs: DEFAULT_DRAIN_SECS,
            requeue: false,
            keep_aether: false,
        };
        assert_eq!(
            request.describe(),
            "upgrade archon to main and aether to 1.0.7"
        );
        assert!(request.check().is_ok());
        request.drain_secs = 0;
        assert!(request.check().is_err());
        request.drain_secs = MAX_DRAIN_SECS + 1;
        assert!(request.check().is_err());
        request.drain_secs = 1;
        request.keep_aether = true;
        assert!(
            request.check().is_err(),
            "a new aether needs its aetherd restarted"
        );
        request.archon = None;
        request.aether = None;
        assert_eq!(request.describe(), "a drained restart");
    }

    #[test]
    fn a_newer_latest_release_gives_the_update_hint() {
        let program = |running: &str, latest: Option<&str>| ProgramVersions {
            running: Some(running.into()),
            installed: None,
            latest: latest.map(Into::into),
        };
        let versions = Versions {
            archon: program("1.2.3", Some("v1.2.4")),
            aether: program("1.0.7", Some("1.0.7")),
        };
        let hint = versions.update_hint().unwrap();
        assert!(
            hint.contains("archon 1.2.4") && !hint.contains("aether"),
            "{hint}"
        );
        assert!(hint.contains("`archon update`"), "{hint}");
        let current = Versions {
            archon: program("1.2.3", Some("1.2.3")),
            aether: program("1.0.9", Some("1.0.7")),
        };
        assert_eq!(current.update_hint(), None, "same or older latest");
        let unchecked = Versions {
            archon: program("1.2.3", None),
            aether: program("1.0.7", Some("1.0.8-rc.1")),
        };
        assert_eq!(unchecked.update_hint(), None, "unknown or pre-release");
    }

    #[test]
    fn versions_differ_only_when_two_known_ones_disagree() {
        let versions =
            |running: &str, installed: Option<&str>, latest: Option<&str>| ProgramVersions {
                running: Some(running.into()),
                installed: installed.map(Into::into),
                latest: latest.map(Into::into),
            };
        assert!(!versions("1.0.6", None, None).differ());
        assert!(!versions("1.0.6", Some("aether 1.0.6"), Some("v1.0.6")).differ());
        assert!(versions("1.0.6", Some("1.0.7"), None).differ());
        assert!(versions("1.0.6", None, Some("1.0.7")).differ());
        assert!(versions("1.0.7", Some("1.0.6"), Some("1.0.7")).differ());
        let both = Versions {
            archon: versions("1.2.2", Some("1.2.3"), None),
            aether: versions("1.0.7", Some("1.0.7"), None),
        };
        assert!(both.differ());
        assert_eq!(both.lines(), ["archon running 1.2.2, installed 1.2.3"]);
    }
}
