//! The commented `config.toml` that `archon setup` writes when there is none,
//! and the `archon.toml` of Archon before 1.0, which nothing reads any more.

use std::io::Write as _;
use std::path::{Path, PathBuf};

use crate::config::CONFIG_FILE_NAME;
use crate::error::ArchonError;

/// The config file of Archon before 1.0.
pub const LEGACY_CONFIG_FILE_NAME: &str = "archon.toml";

/// Every key commented out, so loading it gives the defaults.
pub const STARTER_CONFIG: &str = r#"# Archon's configuration. Every key is optional, and every line below is
# commented out: this file changes nothing until you edit it.
# Reference: https://github.com/mengsig/archon/blob/main/docs/configuration.md
# An unknown key is an error, so a typo cannot silently disable a limit.

# The agent you talk to. This is the only model you name: the orchestrator
# picks a model and effort for every stage of every item itself.
# [orchestrator]
# model = "claude-code/claude-opus-5-5"   # an entry of `archon aether models`
# effort = "high"
# wake_on_success = true    # false: a routine success waits for its next turn

# Optional narrowing. Without [models] every model Aether reports ready may
# be used. Tiers (fast, balanced, strong, frontier) come from Aether's catalog.
# [models]
# allow = ["claude-code/claude-opus-5-5", "codex/gpt-6.1-sol"]
# max_effort = "high"
# max_default_tier = "strong"   # the most a default picks; a plan may ask higher
# provider_weights = { "claude-code" = 7, codex = 3 }   # share of tier-only items
# reserve_percent = { orchestrator = 15, interactive = 10 }   # quota stages never spend
# interactive_providers = ["claude-code"]   # providers also used by hand

# Reviewers without an explicit <stage>.review model plan.
# [review]
# default_tier = "balanced"
# default_effort = "low"
# prefer_other_provider = true

# [limits]
# max_parallel_agents = 8   # the hard cap on running stage agents
# budgets_enabled = true    # false: no dollar budget is enforced (spend still recorded)
# budget_usd_daily = 50.0   # 0 = no daily budget
# min_free_gb = 10          # nothing new starts below this much free disk

# [learning]
# enabled = true            # outcomes, learned defaults, lessons and distillation
# distill_daily_budget_usd = 3.0 # global UTC daily reservation for distillation
# distill_every = 20        # new outcomes per repository before a budgeted errand
# explore_rate = 0.1        # deterministic cheaper-choice exploration; 0..0.1

# [worktrees]
# auto_gc = true            # archond collects finished runs' worktrees itself
# gc_after = "7d"           # "0s" disables expiry

# [storage]
# retention_days = 14       # settled reports and log age; 0 disables age expiry
# log_max_mb = 10           # 0 disables size rotation
# log_max_files = 3         # archives beyond the active log; 0 disables count limit

# Archon runs its own aetherd. "shared" uses yours instead.
# [aether]
# mode = "dedicated"
# host_context = "full"     # "lean": Claude Code skips skills and non-Aether MCP
"#;

/// Write [`STARTER_CONFIG`] as `dir`'s `config.toml` unless one is there,
/// which is never touched. Returns the path when it was written.
pub fn write_starter(dir: &Path) -> Result<Option<PathBuf>, ArchonError> {
    let path = dir.join(CONFIG_FILE_NAME);
    std::fs::create_dir_all(dir)
        .map_err(|e| ArchonError::io(format!("creating {}", dir.display()), e))?;
    let created = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path);
    let mut file = match created {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => return Ok(None),
        Err(e) => return Err(ArchonError::io(format!("creating {}", path.display()), e)),
    };
    file.write_all(STARTER_CONFIG.as_bytes())
        .map_err(|e| ArchonError::io(format!("writing {}", path.display()), e))?;
    Ok(Some(path))
}

/// What to tell the operator about an `archon.toml` in `dir`, which Archon
/// ignores; `None` when there is none.
pub fn legacy_config_notice(dir: &Path) -> Option<String> {
    let legacy = dir.join(LEGACY_CONFIG_FILE_NAME);
    if !legacy.exists() {
        return None;
    }
    let read = dir.join(CONFIG_FILE_NAME);
    let state = if read.exists() {
        ""
    } else {
        ", which does not exist, so the defaults apply (`archon setup` writes a commented one)"
    };
    Some(format!(
        "{} is ignored: it is the config file of Archon before 1.0, and its keys are not \
         today's. Archon reads only {}{state}. Put the settings you still want there \
         (docs/configuration.md lists the keys), then remove the old file.",
        legacy.display(),
        read.display()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    /// The starter with its example sections and keys uncommented.
    fn uncommented() -> String {
        let lines = STARTER_CONFIG.lines().map(|line| {
            let body = line.strip_prefix("# ").unwrap_or(line);
            let is_setting = body.starts_with('[') || body.contains(" = ");
            if is_setting { body } else { line }
        });
        lines.collect::<Vec<_>>().join("\n")
    }

    #[test]
    fn the_starter_changes_no_default_and_its_examples_are_valid() {
        let dir = tempfile::tempdir().unwrap();
        let written = write_starter(dir.path()).unwrap().expect("none was there");
        assert_eq!(written, dir.path().join(CONFIG_FILE_NAME));
        assert_eq!(Config::load(&written).unwrap(), Config::default());

        let edited = uncommented();
        assert!(edited.contains("\n[models]\nallow = ["), "{edited}");
        std::fs::write(&written, edited).unwrap();
        let config = Config::load(&written).expect("every example is a valid key");
        assert_eq!(config.limits.max_parallel_agents, 8);
        assert_eq!(config.models.allow.len(), 2);
    }

    #[test]
    fn an_existing_config_is_never_touched() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(CONFIG_FILE_NAME);
        std::fs::write(&path, "[limits]\nmax_parallel_agents = 7\n").unwrap();
        assert_eq!(write_starter(dir.path()).unwrap(), None);
        let kept = std::fs::read_to_string(&path).unwrap();
        assert_eq!(kept, "[limits]\nmax_parallel_agents = 7\n");
    }

    #[test]
    fn a_legacy_file_is_reported_as_ignored_with_the_file_that_is_read() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(legacy_config_notice(dir.path()), None);
        let legacy = dir.path().join(LEGACY_CONFIG_FILE_NAME);
        std::fs::write(&legacy, "[models]\ntiers = {}\n").unwrap();

        let alone = legacy_config_notice(dir.path()).expect("it is there");
        let read = dir.path().join(CONFIG_FILE_NAME);
        assert!(alone.starts_with(&format!("{} is ignored", legacy.display())));
        assert!(alone.contains(&read.display().to_string()), "{alone}");
        assert!(alone.contains("does not exist"), "{alone}");

        write_starter(dir.path()).unwrap();
        let beside = legacy_config_notice(dir.path()).expect("still there");
        assert!(!beside.contains("does not exist"), "{beside}");
        assert_eq!(
            Config::load(&read).unwrap(),
            Config::default(),
            "nothing of the legacy file is read"
        );
    }
}
