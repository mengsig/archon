//! One readable sentence per operator settings change, for the feed. The
//! exact request stays in the audit record (`FeedEntry::Authority`).

use archon_core::protocol::Request;

/// The feed line for a request that came over the operator socket.
pub(crate) fn request_sentence(request: &Request) -> String {
    let scope = |repo: &Option<archon_core::protocol::RepoRef>| match repo {
        Some(repo) => format!("repository {}", repo.0),
        None => "global".to_owned(),
    };
    match request {
        Request::SetSettings { repo, .. } => format!("Changed {} settings.", scope(repo)),
        Request::ResetSettings { repo } => format!("Reset {} settings.", scope(repo)),
        Request::SetCaps { .. } => "Changed the spending caps.".to_owned(),
        Request::RepoUpdate { repo, patch } => {
            let changed = serde_json::to_value(patch)
                .ok()
                .and_then(|v| v.as_object().cloned())
                .unwrap_or_default()
                .into_iter()
                .filter(|(_, value)| !value.is_null() && value != "keep")
                .map(|(key, _)| key)
                .collect::<Vec<_>>();
            match changed.is_empty() {
                true => format!("Updated repository {}.", repo.0),
                false => format!("Updated {} of repository {}.", changed.join(", "), repo.0),
            }
        }
        other => format!("Ran operator request {}.", request_name(other)),
    }
}

fn request_name(request: &Request) -> String {
    serde_json::to_value(request)
        .ok()
        .and_then(|v| v.get("cmd").and_then(|c| c.as_str().map(str::to_owned)))
        .unwrap_or_else(|| "(unnamed)".to_owned())
}

/// The feed line for an applied `/archon` command, from the command text.
pub(crate) fn command_sentence(text: &str) -> String {
    let words: Vec<&str> = text.split_whitespace().collect();
    match words.as_slice() {
        [_, "set", "--repo", name, settings @ ..] => {
            format!("Set {} for repository {name}.", settings.join(" "))
        }
        [_, "set", settings @ ..] => format!("Set {}.", settings.join(" ")),
        [_, "pause", "--hard"] => "Paused dispatch (hard).".to_owned(),
        [_, "resume", "--hard"] => "Resumed dispatch (hard).".to_owned(),
        [_, "reset"] => "Reset the global settings.".to_owned(),
        [_, "reset", "--repo", name] => format!("Reset the settings of repository {name}."),
        [_, "repo", "remove", name] => format!("Removed repository {name}."),
        [_, "undo", seq] => format!("Undid authority change {seq}."),
        [_, verb, ..] => format!("Applied /archon {verb}."),
        _ => "Applied the command.".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use archon_core::protocol::{RepoPatch, RepoRef};

    use super::*;

    #[test]
    fn commands_read_as_sentences() {
        assert_eq!(
            command_sentence("/archon set max_parallel_agents=4"),
            "Set max_parallel_agents=4."
        );
        assert_eq!(
            command_sentence("/archon pause --hard"),
            "Paused dispatch (hard)."
        );
        assert_eq!(
            command_sentence("/archon set --repo scratch base_branch=main"),
            "Set base_branch=main for repository scratch."
        );
    }

    #[test]
    fn a_repo_update_names_its_changed_fields_not_json() {
        let patch = RepoPatch {
            grant_merge: Some(true),
            ..RepoPatch::default()
        };
        let line = request_sentence(&Request::RepoUpdate {
            repo: RepoRef("scratch".into()),
            patch,
        });
        assert_eq!(line, "Updated grant_merge of repository scratch.");
        assert!(!line.contains("{\"cmd\""));
    }
}
