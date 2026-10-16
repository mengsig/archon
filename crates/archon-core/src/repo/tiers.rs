//! A repository's verification tiers (issue #82): the integration branch
//! items fork from and deliver to, the release branch it is promoted into,
//! and the fast and full check commands.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::ArchonError;

/// What `.archon/repo.toml` names, so every machine reads the same values.
pub const REPO_FILE_NAME: &str = "repo.toml";

/// The release branch when none is set.
pub const DEFAULT_RELEASE_BRANCH: &str = "main";

/// The shape of `.archon/repo.toml` and of the registry's `tiers`; the
/// registry's values win field by field ([`RepoTiers::over`]).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RepoTiers {
    /// Items fork from it and their pull requests target it; unset uses
    /// the repository's base branch.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub integration_branch: Option<String>,
    /// Where the integration branch is created from when it is missing.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub release_branch: Option<String>,
    #[serde(skip_serializing_if = "Checks::is_empty")]
    pub checks: Checks,
}

/// The check tiers: `fast` runs in every item before it commits; `full`
/// is the whole suite, which items never run.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Checks {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fast: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub full: Option<String>,
}

impl Checks {
    pub fn is_empty(&self) -> bool {
        self.fast.is_none() && self.full.is_none()
    }
}

impl RepoTiers {
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    /// `self` where it sets a value, else `below`.
    pub fn over(self, below: Self) -> Self {
        Self {
            integration_branch: self.integration_branch.or(below.integration_branch),
            release_branch: self.release_branch.or(below.release_branch),
            checks: Checks {
                fast: self.checks.fast.or(below.checks.fast),
                full: self.checks.full.or(below.checks.full),
            },
        }
    }

    pub fn release_branch(&self) -> &str {
        self.release_branch
            .as_deref()
            .unwrap_or(DEFAULT_RELEASE_BRANCH)
    }

    /// Refuse a branch name git would read as an option or a revision
    /// range, and a command that is blank or spans lines.
    pub fn validate(&self) -> Result<(), ArchonError> {
        let branches = [
            ("integration_branch", &self.integration_branch),
            ("release_branch", &self.release_branch),
        ];
        for (key, branch) in branches {
            if let Some(name) = branch.as_deref().filter(|b| !plain_branch(b)) {
                return Err(ArchonError::Invalid(format!(
                    "{key} {name:?} is not a plain branch name"
                )));
            }
        }
        let commands = [
            ("checks.fast", &self.checks.fast),
            ("checks.full", &self.checks.full),
        ];
        for (key, command) in commands {
            if let Some(text) = command.as_deref().filter(|c| !one_line(c)) {
                return Err(ArchonError::Invalid(format!(
                    "{key} {text:?} must be one non-empty line"
                )));
            }
        }
        if self.integration_branch.is_some()
            && self.integration_branch.as_deref() == Some(self.release_branch())
        {
            return Err(ArchonError::Invalid(
                "integration_branch must differ from release_branch".into(),
            ));
        }
        Ok(())
    }

    /// `<repo>/.archon/repo.toml`; a missing file sets nothing, and a
    /// malformed or invalid one is an error naming the file.
    pub fn load(repo_root: &Path) -> Result<Self, ArchonError> {
        assert!(repo_root.is_absolute(), "registered paths are absolute");
        let path = crate::paths::repo_archon_dir(repo_root).join(REPO_FILE_NAME);
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(e) => return Err(ArchonError::io(format!("reading {}", path.display()), e)),
        };
        let tiers: Self = toml::from_str(&text)
            .map_err(|e| ArchonError::Config(format!("{}: {e}", path.display())))?;
        tiers
            .validate()
            .map_err(|e| ArchonError::Config(format!("{}: {e}", path.display())))?;
        Ok(tiers)
    }
}

fn plain_branch(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with(['-', '/'])
        && !name.ends_with(['/', '.'])
        && !name.contains("..")
        && !name.contains(|c: char| c.is_whitespace() || c.is_control() || "~^:?*[\\@{".contains(c))
}

fn one_line(command: &str) -> bool {
    !command.trim().is_empty() && !command.contains(['\n', '\r'])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tiers(integration: Option<&str>, fast: Option<&str>) -> RepoTiers {
        RepoTiers {
            integration_branch: integration.map(str::to_owned),
            release_branch: None,
            checks: Checks {
                fast: fast.map(str::to_owned),
                full: None,
            },
        }
    }

    #[test]
    fn the_registry_wins_field_by_field_over_the_committed_file() {
        let file = RepoTiers {
            release_branch: Some("trunk".into()),
            checks: Checks {
                fast: Some("make quick".into()),
                full: Some("make ci".into()),
            },
            ..tiers(Some("next"), None)
        };
        let merged = tiers(Some("staging"), None).over(file);
        assert_eq!(merged.integration_branch.as_deref(), Some("staging"));
        assert_eq!(merged.release_branch(), "trunk");
        assert_eq!(merged.checks.fast.as_deref(), Some("make quick"));
        assert_eq!(merged.checks.full.as_deref(), Some("make ci"));
        assert_eq!(RepoTiers::default().release_branch(), "main");
        assert!(RepoTiers::default().over(RepoTiers::default()).is_empty());
    }

    #[test]
    fn a_missing_file_sets_nothing_and_a_committed_one_is_read() {
        let dir = tempfile::tempdir().unwrap();
        assert!(RepoTiers::load(dir.path()).unwrap().is_empty());
        let file = crate::paths::repo_archon_dir(dir.path()).join(REPO_FILE_NAME);
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        let text = "integration_branch = \"next\"\n\n[checks]\nfast = \"cargo test -p a\"\n\
                    full = \"make ci\"\n";
        std::fs::write(&file, text).unwrap();
        let read = RepoTiers::load(dir.path()).unwrap();
        assert_eq!(read.integration_branch.as_deref(), Some("next"));
        assert_eq!(read.release_branch(), "main");
        assert_eq!(read.checks.fast.as_deref(), Some("cargo test -p a"));
        assert_eq!(read.checks.full.as_deref(), Some("make ci"));
    }

    #[test]
    fn a_broken_or_invalid_file_is_refused_naming_it() {
        let dir = tempfile::tempdir().unwrap();
        let file = crate::paths::repo_archon_dir(dir.path()).join(REPO_FILE_NAME);
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        for text in [
            "integration = \"next\"\n",
            "integration_branch = \"--force\"\n",
            "integration_branch = \"main\"\n",
            "[checks]\nfast = \"  \"\n",
        ] {
            std::fs::write(&file, text).unwrap();
            let refused = RepoTiers::load(dir.path()).unwrap_err().to_string();
            assert!(refused.contains("repo.toml"), "{text}: {refused}");
        }
    }

    #[test]
    fn branch_names_and_commands_are_checked() {
        for good in ["next", "release/next", "v1.2-int"] {
            assert!(tiers(Some(good), None).validate().is_ok(), "{good}");
        }
        for bad in ["", "-x", "a..b", "a b", "a~1", "x/", "HEAD@{1}"] {
            assert!(tiers(Some(bad), None).validate().is_err(), "{bad:?}");
        }
        assert!(tiers(None, Some("make test")).validate().is_ok());
        assert!(tiers(None, Some("a\nb")).validate().is_err());
    }
}
