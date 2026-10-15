//! Where a stage keeps what outlives its worktree: the repository's shared
//! build cache, and the item's artifact store (ARCHITECTURE §13.3).

use std::path::Path;

use archon_core::config::ArtifactsConfig;

/// The contract's Build cache section. Empty when the repository builds
/// into per-task directories, or the run has no repository.
pub fn build_cache_rules(cargo_target: Option<&Path>) -> String {
    let Some(dir) = cargo_target else {
        return String::new();
    };
    assert!(dir.is_absolute(), "the cache is an absolute path");
    format!(
        "\n## Build cache\n\n\
         - Build into this repository's shared cache, never a target directory of your \
         own and never `/tmp`: `CARGO_TARGET_DIR={dir}`. Archon points cargo at it for \
         builds inside the worktree; `export` it yourself for builds anywhere else. \
         Concurrent cargo builds on one target directory are safe: cargo locks it.\n\
         - Archon deletes the cache once no run of the repository used it for a while, \
         and the worktree's own build output when the run is long finished: publish what \
         you need later (see Artifacts) instead of leaving it there.\n",
        dir = dir.display()
    )
}

/// The contract's Artifacts section, for every stage agent.
pub fn artifact_rules(limits: &ArtifactsConfig) -> String {
    format!(
        "\n## Artifacts\n\n\
         - Files that must outlive your worktree (evidence, logs, benchmark results, \
         gitignored output a later item needs) go through \
         `mcp__archon__publish_artifact` with a `path` inside your workspace: a file or \
         a directory of regular files, stored under `name` (default: its last component). \
         It is the one Archon mutation tool you may use besides `message_orchestrator`. \
         Archon stores it by item id, at most {file} MB a file and {item} MB an item, \
         and keeps it {keep} after the item settles.\n\
         - `mcp__archon__list_artifacts` and `mcp__archon__read_artifact` read any \
         item's artifacts by its id after its worktree is gone; a dependency's \
         directory is named under Dependencies. Publish before you finish: nothing \
         can be published after your stage settles.\n",
        file = limits.max_file_mb,
        item = limits.max_item_mb,
        keep = limits.keep_after,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_build_cache_section_names_the_directory_or_is_empty() {
        assert_eq!(build_cache_rules(None), "");
        let text = build_cache_rules(Some(Path::new("/d/cache/r/cargo-target")));
        assert!(
            text.contains("CARGO_TARGET_DIR=/d/cache/r/cargo-target"),
            "{text}"
        );
        assert!(text.contains("never `/tmp`"), "{text}");
    }

    #[test]
    fn the_artifact_section_states_the_limits() {
        let text = artifact_rules(&ArtifactsConfig::default());
        assert!(text.contains("mcp__archon__publish_artifact"), "{text}");
        assert!(
            text.contains("at most 256 MB a file and 1024 MB an item"),
            "{text}"
        );
        assert!(text.contains("keeps it 30d after"), "{text}");
    }
}
