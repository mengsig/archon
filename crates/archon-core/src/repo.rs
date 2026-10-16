//! The multi-repository registry: what Archon may work on and how finished
//! work leaves a repository.

use std::path::PathBuf;

use serde::{Deserialize, Deserializer, Serialize};

use crate::Timestamp;
use crate::autonomy::AutonomyPatch;
use crate::id::RepoId;

pub mod affected;
mod tiers;

pub use tiers::{Checks, DEFAULT_RELEASE_BRANCH, REPO_FILE_NAME, RepoTiers, check_command};

/// How finished work leaves a repository. Nothing merges unless
/// the item's `merge = auto` knob is within the operator's ceiling.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum ShipMode {
    /// Push the branch and open a pull request. The default.
    #[default]
    Pr,
    /// Commit on a local branch; never push.
    Local,
}

impl ShipMode {
    /// The mode `name` spells, or why it is none: one wording for the CLI,
    /// configuration and the store.
    fn named(name: &str) -> Result<Self, String> {
        match name {
            "pr" => Ok(Self::Pr),
            "local" => Ok(Self::Local),
            other => Err(format!("ship mode {other:?} is not one of pr, local")),
        }
    }
}

impl std::str::FromStr for ShipMode {
    type Err = crate::ArchonError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::named(s).map_err(crate::ArchonError::Invalid)
    }
}

/// Not derived: an unknown name is refused with the modes that exist.
impl<'de> Deserialize<'de> for ShipMode {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let name = String::deserialize(deserializer)?;
        Self::named(&name).map_err(serde::de::Error::custom)
    }
}

/// What a delivered pull request says of Archon and of the tools that made
/// it; only the operator sets it, in the registry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum Attribution {
    /// No `Archon item:` line, and delivery refuses a pull request body or
    /// commit message carrying a `Co-Authored-By:` or `Generated with` line.
    None,
    /// The body ends with the `Archon item:` line. The default.
    #[default]
    ItemId,
}

impl Attribution {
    /// The policy `name` spells, or why it is none.
    fn named(name: &str) -> Result<Self, String> {
        match name {
            "none" => Ok(Self::None),
            "item-id" => Ok(Self::ItemId),
            other => Err(format!("attribution {other:?} is not one of none, item-id")),
        }
    }

    /// The name the CLI, chat commands and the store use.
    pub fn name(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::ItemId => "item-id",
        }
    }
}

impl std::str::FromStr for Attribution {
    type Err = crate::ArchonError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::named(s).map_err(crate::ArchonError::Invalid)
    }
}

/// Not derived: an unknown name is refused with the policies that exist.
impl<'de> Deserialize<'de> for Attribution {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let name = String::deserialize(deserializer)?;
        Self::named(&name).map_err(serde::de::Error::custom)
    }
}

/// The GitHub host a [`GhUser`] logs in to.
pub const GH_HOST: &str = "github.com";

/// A github.com login whose `gh` token every `gh` call and push for a
/// repository carries; only the operator sets it, in the registry.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
pub struct GhUser(String);

impl GhUser {
    /// Longest login GitHub gives out.
    const MAX_LEN: usize = 39;

    /// The login `name` spells, or why it is none. It becomes an argument
    /// of `gh`, so it never starts with `-`.
    fn named(name: &str) -> Result<Self, String> {
        let shaped = !name.is_empty()
            && name.len() <= Self::MAX_LEN
            && !name.starts_with('-')
            && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-');
        if !shaped {
            return Err(format!(
                "{name:?} is no GitHub login: 1 to {} letters, digits or hyphens, not starting \
                 with a hyphen",
                Self::MAX_LEN
            ));
        }
        Ok(Self(name.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// `gh` arguments that print this login's token on [`GH_HOST`].
    pub fn token_args(&self) -> [&str; 6] {
        let args = ["auth", "token", "--hostname", GH_HOST, "--user", &self.0];
        assert!(!self.0.starts_with('-'), "checked when named");
        args
    }
}

/// `owner/name` of the github.com repository `url` names over https or ssh,
/// with no credentials, port or further path; `None` for any other URL.
pub fn github_slug(url: &str) -> Option<String> {
    let path = url
        .strip_prefix("https://github.com/")
        .or_else(|| url.strip_prefix("ssh://git@github.com/"))
        .or_else(|| url.strip_prefix("git@github.com:"))?;
    let path = path.strip_suffix('/').unwrap_or(path);
    let path = path.strip_suffix(".git").unwrap_or(path);
    let (owner, name) = path.split_once('/')?;
    let plain = |part: &str| {
        !part.is_empty()
            && !part.starts_with('.')
            && part
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
    };
    let slug = (plain(owner) && plain(name)).then(|| format!("{owner}/{name}"))?;
    assert_eq!(slug.matches('/').count(), 1, "owner and name");
    Some(slug)
}

impl std::fmt::Display for GhUser {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::str::FromStr for GhUser {
    type Err = crate::ArchonError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::named(s).map_err(crate::ArchonError::Invalid)
    }
}

/// Not derived: a stored or sent login is checked as a typed one is.
impl<'de> Deserialize<'de> for GhUser {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let name = String::deserialize(deserializer)?;
        Self::named(&name).map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Repo {
    pub id: RepoId,
    /// Short unique label used in `--repo`, tags and worktree paths.
    pub name: String,
    /// Canonical path of the repository's main working tree.
    pub path: PathBuf,
    pub ship: ShipMode,
    /// Branch new work forks from; `None` uses the checked-out HEAD.
    pub base_branch: Option<String>,
    /// Pipeline used when an enqueue names none.
    pub default_pipeline: Option<String>,
    /// The operator's autonomy ceiling for this repository; set knobs
    /// replace the global `[autonomy] max` (ARCHITECTURE §18.2).
    #[serde(default)]
    pub autonomy_max: AutonomyPatch,
    /// The operator lets the orchestrator merge an item's pull request when
    /// the operator asks it to, without a confirmation question; the
    /// `merge` knob and its ceiling are separate (ARCHITECTURE §18.6).
    #[serde(default)]
    pub grant_merge: bool,
    /// What its pull requests say of Archon and the tools; operator only.
    #[serde(default)]
    pub attribution: Attribution,
    /// The GitHub login its `gh` calls and pushes run as; `None` uses the
    /// daemon's own login. Operator only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gh_user: Option<GhUser>,
    /// The operator's verification tiers, over the committed `.archon/repo.toml`.
    #[serde(default, skip_serializing_if = "RepoTiers::is_empty")]
    pub tiers: RepoTiers,
    /// The committed file's fast check the operator confirmed, word for
    /// word: the ship gate runs that file's check only while it reads so.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confirmed_fast_check: Option<String>,
    /// The committed file's full check the operator confirmed, word for
    /// word: the integration branch's verification runs it only while so.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confirmed_full_check: Option<String>,
    /// The committed file's release hook the operator confirmed, word for
    /// word: a promotion runs it only while the file reads so.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confirmed_release_check: Option<String>,
    pub added_at: Timestamp,
}

impl Repo {
    /// A name that is safe in a path and a git ref.
    pub fn valid_name(name: &str) -> bool {
        !name.is_empty()
            && name.len() <= 64
            && name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
            && !name.starts_with('.')
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ship_mode_wire_names_match_their_parsed_names() {
        for (mode, name) in [(ShipMode::Pr, "pr"), (ShipMode::Local, "local")] {
            let wire = serde_json::to_string(&mode).unwrap();
            assert_eq!(wire, format!("\"{name}\""));
            assert_eq!(serde_json::from_str::<ShipMode>(&wire).unwrap(), mode);
            assert_eq!(name.parse::<ShipMode>().unwrap(), mode);
        }
        assert!("merge".parse::<ShipMode>().is_err());
    }

    #[test]
    fn a_value_that_is_no_ship_mode_is_refused_listing_the_two_modes() {
        for unknown in [concat!("no-", "mistakes"), "PR", "", "merge"] {
            let parsed = unknown.parse::<ShipMode>().unwrap_err();
            assert_eq!(
                parsed.to_string(),
                format!("invalid argument: ship mode {unknown:?} is not one of pr, local")
            );
            let decoded = serde_json::from_value::<ShipMode>(unknown.into()).unwrap_err();
            assert_eq!(
                decoded.to_string(),
                format!("ship mode {unknown:?} is not one of pr, local")
            );
        }
        assert!(serde_json::from_str::<ShipMode>("1").is_err());
    }

    #[test]
    fn attribution_names_round_trip_and_an_unknown_one_lists_the_policies() {
        for policy in [Attribution::None, Attribution::ItemId] {
            let wire = serde_json::to_string(&policy).unwrap();
            assert_eq!(wire, format!("\"{}\"", policy.name()));
            assert_eq!(serde_json::from_str::<Attribution>(&wire).unwrap(), policy);
            assert_eq!(policy.name().parse::<Attribution>().unwrap(), policy);
        }
        assert_eq!(Attribution::default(), Attribution::ItemId);
        let refused = "full".parse::<Attribution>().unwrap_err().to_string();
        assert!(
            refused.ends_with("is not one of none, item-id"),
            "{refused}"
        );
        let repo = Repo {
            id: RepoId::new(),
            name: "a".into(),
            path: "/a".into(),
            ship: ShipMode::Pr,
            base_branch: None,
            default_pipeline: None,
            autonomy_max: AutonomyPatch::default(),
            grant_merge: false,
            attribution: Attribution::None,
            gh_user: None,
            tiers: RepoTiers::default(),
            confirmed_fast_check: None,
            confirmed_full_check: None,
            confirmed_release_check: None,
            added_at: crate::now(),
        };
        let mut stored = serde_json::to_value(&repo).unwrap();
        assert_eq!(stored["attribution"], "none");
        stored.as_object_mut().unwrap().remove("attribution");
        let old: Repo = serde_json::from_value(stored).unwrap();
        assert_eq!(
            old.attribution,
            Attribution::ItemId,
            "stored before it existed"
        );
    }

    #[test]
    fn a_gh_user_is_a_plain_login_and_is_checked_when_read() {
        for login in ["devGunnin", "a", "a-b-1", &"x".repeat(39)] {
            let user: GhUser = login.parse().unwrap();
            assert_eq!(user.as_str(), login);
            let wire = serde_json::to_string(&user).unwrap();
            assert_eq!(serde_json::from_str::<GhUser>(&wire).unwrap(), user);
        }
        for bad in ["", "-u", "--user=other", "a b", "a/b", "ü", &"x".repeat(40)] {
            assert!(bad.parse::<GhUser>().is_err(), "{bad:?}");
            let decoded = serde_json::from_value::<GhUser>(bad.into());
            assert!(decoded.is_err(), "a stored {bad:?} is refused too");
        }
        let user: GhUser = "devGunnin".parse().unwrap();
        let args = [
            "auth",
            "token",
            "--hostname",
            "github.com",
            "--user",
            "devGunnin",
        ];
        assert_eq!(user.token_args(), args);
    }

    #[test]
    fn only_a_github_com_repository_over_https_or_ssh_has_a_slug() {
        for url in [
            "https://github.com/o/r",
            "https://github.com/o/r.git",
            "https://github.com/o/r/",
            "git@github.com:o/r.git",
            "ssh://git@github.com/o/r.git",
        ] {
            assert_eq!(github_slug(url).as_deref(), Some("o/r"), "{url}");
        }
        for url in [
            "http://github.com/o/r.git",
            "https://user:pw@github.com/o/r.git",
            "https://github.com:8443/o/r.git",
            "https://github.com.evil.example/o/r.git",
            "https://ghe.example.com/o/r.git",
            "git@ghe.example.com:o/r.git",
            "git@github.com-work:o/r.git",
            "https://github.com/o/r/extra",
            "https://github.com/../r.git",
            "https://github.com/o/r?x=1",
            "/srv/git/r.git",
            "file:///srv/git/r.git",
        ] {
            assert_eq!(github_slug(url), None, "{url}");
        }
    }

    #[test]
    fn a_repo_stored_before_gh_user_existed_has_none() {
        let repo = serde_json::json!({
            "id": RepoId::new(), "name": "a", "path": "/a", "ship": "pr",
            "base_branch": null, "default_pipeline": null, "added_at": crate::now(),
        });
        let old: Repo = serde_json::from_value(repo).unwrap();
        assert_eq!(old.gh_user, None);
        let mut with = old.clone();
        with.gh_user = Some("devGunnin".parse().unwrap());
        let stored = serde_json::to_value(&with).unwrap();
        assert_eq!(stored["gh_user"], "devGunnin");
        assert!(serde_json::to_value(&old).unwrap().get("gh_user").is_none());
    }

    #[test]
    fn repo_names_are_path_safe() {
        assert!(Repo::valid_name("aether"));
        assert!(Repo::valid_name("my_repo-2.0"));
        assert!(!Repo::valid_name(""));
        assert!(!Repo::valid_name("../x"));
        assert!(!Repo::valid_name("a b"));
    }
}
