//! Where a stage keeps what outlives its worktree: the repository's shared
//! build cache, and the item's artifact store (ARCHITECTURE §13.5).

use std::path::Path;

use archon_core::config::ArtifactsConfig;

/// The contract's Build cache section. Empty when the repository builds
/// into its worktrees, or the run has no repository. `shared`: the directory
/// is the repository's, not the item's.
pub fn build_cache_rules(cargo_target: Option<&Path>, shared: bool) -> String {
    let Some(dir) = cargo_target else {
        return String::new();
    };
    assert!(dir.is_absolute(), "the cache is an absolute path");
    let (whose, ends) = if shared {
        (
            "this repository's shared cache",
            "Archon deletes the cache once idle",
        )
    } else {
        (
            "this item's own cache, kept across its retries",
            "Archon deletes it the moment the item ends",
        )
    };
    format!(
        "\n## Build cache\n\n\
         - Build into {whose}, never a target directory of your own or `/tmp`: \
         `CARGO_TARGET_DIR={dir}`. `export` it for every cargo you run. Concurrent \
         builds are safe: cargo locks it.\n\
         - {ends}, and the worktree's build output after the run is long finished: \
         publish what you need later (see Artifacts).\n",
        dir = dir.display()
    )
}

/// The contract's Artifacts section, for every stage agent.
pub fn artifact_rules(limits: &ArtifactsConfig) -> String {
    format!(
        "\n## Artifacts\n\n\
         - Files that must outlive your worktree (evidence, logs, benchmark results, \
         gitignored output a later item needs): `mcp__archon__publish_artifact` with a \
         `path` in your workspace (a file or a directory of regular files), stored under \
         `name` (default: its last component). Besides `message_orchestrator`, the only \
         Archon mutation tool you may use. Limits: {file} MB a file, {item} MB an item; \
         kept {keep} after the item settles.\n\
         - `mcp__archon__list_artifacts` and `mcp__archon__read_artifact` read any \
         item's artifacts by id once its worktree is gone; a dependency's directory is \
         named under Dependencies. Publish before you finish.\n",
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
        assert_eq!(build_cache_rules(None, false), "");
        let dir = Path::new("/d/cache/r/items/qi_1/cargo-target");
        let text = build_cache_rules(Some(dir), false);
        assert!(
            text.contains(&format!("CARGO_TARGET_DIR={}", dir.display())),
            "{text}"
        );
        assert!(text.contains("or `/tmp`"), "{text}");
        assert!(text.contains("this item's own cache"), "{text}");
        assert!(text.contains("the moment the item ends"), "{text}");
        let shared = build_cache_rules(Some(dir), true);
        assert!(shared.contains("shared cache"), "{shared}");
    }

    #[test]
    fn the_artifact_section_states_the_limits() {
        let text = artifact_rules(&ArtifactsConfig::default());
        assert!(text.contains("mcp__archon__publish_artifact"), "{text}");
        assert!(text.contains("256 MB a file, 1024 MB an item"), "{text}");
        assert!(text.contains("kept 30d after"), "{text}");
    }
}
