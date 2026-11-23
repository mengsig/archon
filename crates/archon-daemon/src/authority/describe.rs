//! A confirmation's question in plain English: the fields that change as
//! "old -> new". Display only; the stored action and snapshot stay exact.

use archon_core::authority::OperatorChannel;
use archon_core::protocol::Request;
use serde_json::{Map, Value};

/// The whole question text for `action`, observed as `before`.
pub(super) fn question(action: &Request, before: &Value, channel: &OperatorChannel) -> String {
    let (title, lines) = describe(action, before);
    let mut text = title;
    for line in lines {
        text.push_str("\n- ");
        text.push_str(&line);
    }
    text.push_str(&format!("\n{}", origin(channel)));
    text.push_str(
        "\nReply yes or no (or /answer N yes). A bare reply counts only as your next message, while this is the only open confirmation.",
    );
    text
}

fn origin(channel: &OperatorChannel) -> String {
    match channel {
        OperatorChannel::Github {
            repository,
            number,
            login,
        } => format!("Requested on GitHub, {repository}#{number}, by {login}."),
        OperatorChannel::AetherChat => "Requested in Aether chat.".to_owned(),
        OperatorChannel::ArchonChat | OperatorChannel::Cli => "Requested in chat.".to_owned(),
    }
}

fn describe(action: &Request, before: &Value) -> (String, Vec<String>) {
    let mut value = serde_json::to_value(action).unwrap_or(Value::Null);
    let cmd = value
        .get("cmd")
        .and_then(Value::as_str)
        .unwrap_or("request")
        .to_owned();
    let Some(fields) = value.as_object_mut() else {
        return (format!("Run \"{cmd}\"?"), Vec::new());
    };
    fields.remove("cmd");
    let repo = fields
        .get("repo")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let scope = match &repo {
        Some(name) => format!("repository \"{name}\""),
        None => "the global settings".to_owned(),
    };
    match cmd.as_str() {
        "repo_update" => {
            let patch = fields.get("patch").cloned().unwrap_or(Value::Null);
            (
                format!("Change repository \"{}\"?", repo.unwrap_or_default()),
                patch_lines(&patch, before),
            )
        }
        "repo_remove" => (
            format!("Remove repository \"{}\"?", repo.unwrap_or_default()),
            Vec::new(),
        ),
        "repo_add" => (
            format!(
                "Add repository \"{}\"?",
                fields
                    .get("name")
                    .and_then(Value::as_str)
                    .or_else(|| fields.get("path").and_then(Value::as_str))
                    .unwrap_or("")
            ),
            list(fields, &[]),
        ),
        "set_settings" => {
            let new = fields.get("settings").cloned().unwrap_or(Value::Null);
            let old = before.get("scoped").cloned().unwrap_or(Value::Null);
            (
                format!("Change settings of {scope}?"),
                replace_lines(&old, &new),
            )
        }
        "reset_settings" => {
            let old = before.get("scoped").cloned().unwrap_or(Value::Null);
            (
                format!("Reset settings of {scope} to their defaults?"),
                replace_lines(&old, &Value::Null),
            )
        }
        "set_caps" => {
            let old = before
                .pointer("/effective/caps")
                .cloned()
                .unwrap_or(Value::Null);
            let new = fields.get("caps").cloned().unwrap_or(Value::Null);
            (
                "Change the spending and parallelism caps?".to_owned(),
                changed_lines(&old, &new),
            )
        }
        "update_item" => {
            let item = fields.get("item").and_then(Value::as_str).unwrap_or("");
            let patch = fields.get("patch").cloned().unwrap_or(Value::Null);
            let mut lines = Vec::new();
            if let Some(new) = patch.get("autonomy").filter(|v| !v.is_null()) {
                let old = before.get("autonomy").cloned().unwrap_or(Value::Null);
                lines.push(format!(
                    "Autonomy: {} -> {}",
                    plain("autonomy", &old),
                    plain("autonomy", new)
                ));
            }
            (format!("Change item {item}?"), lines)
        }
        "pause" | "resume" => {
            let verb = if cmd == "pause" { "Pause" } else { "Resume" };
            let target = fields.get("0").or_else(|| fields.get("target"));
            let title = match target.and_then(|t| t.get("scope").or(Some(t))) {
                Some(t) => format!("{verb} {}?", target_text(t)),
                None => format!("{verb} dispatch?"),
            };
            (title, Vec::new())
        }
        "configure_github_watcher" => {
            let mut change = fields
                .get("change")
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default();
            let what = change
                .remove("action")
                .and_then(|a| a.as_str().map(|s| s.replace('_', " ")))
                .unwrap_or_default();
            (
                format!("Change the GitHub watcher ({what})?"),
                list(&change, &[]),
            )
        }
        "set_directive" => ("Record a standing directive?".to_owned(), list(fields, &[])),
        "clear_directive" => (
            match fields.get("directive").filter(|v| !v.is_null()) {
                Some(_) => "Clear a standing directive?".to_owned(),
                None => "Clear every standing directive?".to_owned(),
            },
            Vec::new(),
        ),
        other => (
            format!("Run \"{}\"?", other.replace('_', " ")),
            list(fields, &[]),
        ),
    }
}

fn target_text(target: &Value) -> String {
    match target {
        Value::Object(map) => match map.get("name").and_then(Value::as_str) {
            Some(name) => name.to_owned(),
            None if map.get("hard") == Some(&Value::Bool(true)) => "all dispatch (hard)".to_owned(),
            None => "all dispatch".to_owned(),
        },
        other => plain("", other),
    }
}

/// Repository patch fields, each against the stored repository.
fn patch_lines(patch: &Value, before: &Value) -> Vec<String> {
    let mut lines = Vec::new();
    let Some(patch) = patch.as_object() else {
        return lines;
    };
    const ORDER: [&str; 9] = [
        "ship",
        "base_branch",
        "default_pipeline",
        "autonomy_max",
        "grant_merge",
        "merge_waive_checks",
        "attribution",
        "gh_user",
        "tiers",
    ];
    let mut ordered: Vec<(&String, &Value)> = patch.iter().collect();
    ordered.sort_by_key(|(key, _)| ORDER.iter().position(|k| k == key).unwrap_or(ORDER.len()));
    for (key, new) in ordered {
        // Absent, null and "keep" all leave the field as it is.
        if new.is_null() || new == "keep" {
            continue;
        }
        if key == "tiers" {
            for (tier, change) in new.as_object().into_iter().flatten() {
                if change == "keep" {
                    continue;
                }
                let old = before.pointer(&format!("/tiers/{tier}")).cloned();
                lines.push(line(&format!("Tier {}", label(tier)), tier, old, change));
            }
            continue;
        }
        let old = before.get(key).cloned();
        lines.push(line(&label(key), key, old, new));
    }
    lines
}

/// "Label: old -> new", reading a Clearable or a plain value.
fn line(label: &str, key: &str, old: Option<Value>, new: &Value) -> String {
    let old = plain(key, &old.unwrap_or(Value::Null));
    let new = if new == "clear" {
        "not set".to_owned()
    } else if let Some(set) = new.get("set") {
        plain(key, set)
    } else {
        plain(key, new)
    };
    format!("{label}: {old} -> {new}")
}

/// A whole-overlay replacement: keys that differ, including those dropped.
fn replace_lines(old: &Value, new: &Value) -> Vec<String> {
    let empty = Map::new();
    let (o, n) = (
        old.as_object().unwrap_or(&empty),
        new.as_object().unwrap_or(&empty),
    );
    let mut keys: Vec<&String> = o.keys().chain(n.keys()).collect();
    keys.sort();
    keys.dedup();
    keys.into_iter()
        .filter(|k| o.get(*k) != n.get(*k))
        .map(|k| {
            format!(
                "{}: {} -> {}",
                label(k),
                plain(k, o.get(k).unwrap_or(&Value::Null)),
                plain(k, n.get(k).unwrap_or(&Value::Null))
            )
        })
        .collect()
}

/// Like [`replace_lines`], but a key missing from `new` stays as it was.
fn changed_lines(old: &Value, new: &Value) -> Vec<String> {
    let empty = Map::new();
    let (o, n) = (
        old.as_object().unwrap_or(&empty),
        new.as_object().unwrap_or(&empty),
    );
    n.iter()
        .filter(|(k, v)| o.get(*k) != Some(*v))
        .map(|(k, v)| {
            format!(
                "{}: {} -> {}",
                label(k),
                plain(k, o.get(k).unwrap_or(&Value::Null)),
                plain(k, v)
            )
        })
        .collect()
}

/// A compact "Label: value" list of the fields that carry something.
fn list(fields: &Map<String, Value>, skip: &[&str]) -> Vec<String> {
    fields
        .iter()
        .filter(|(k, v)| {
            !skip.contains(&k.as_str()) && !v.is_null() && *v != "keep" && *v != &Value::Bool(false)
        })
        .map(|(k, v)| format!("{}: {}", label(k), plain(k, v)))
        .collect()
}

fn label(key: &str) -> String {
    let mut text = key.replace('_', " ");
    if let Some(first) = text.get_mut(..1) {
        first.make_ascii_uppercase();
    }
    text
}

fn secret(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    ["token", "secret", "password", "credential", "key"]
        .iter()
        .any(|word| key.contains(word))
}

/// A value as a short phrase; never JSON, never a secret.
fn plain(key: &str, value: &Value) -> String {
    if secret(key) && !value.is_null() {
        return "(hidden)".to_owned();
    }
    match value {
        Value::Null => "not set".to_owned(),
        Value::Bool(true) => "yes".to_owned(),
        Value::Bool(false) => "no".to_owned(),
        Value::String(s) if key == "ship" && s == "pr" => "pull request".to_owned(),
        Value::String(s) if key == "ship" && s == "local" => "local branch only".to_owned(),
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        Value::Array(items) if items.is_empty() => "none".to_owned(),
        Value::Array(items) => items
            .iter()
            .map(|v| plain(key, v))
            .collect::<Vec<_>>()
            .join(", "),
        Value::Object(map) if map.is_empty() => "default".to_owned(),
        Value::Object(map) => match map.get("set") {
            Some(set) if map.len() == 1 => plain(key, set),
            _ => map
                .iter()
                .map(|(k, v)| format!("{} {}", k.replace('_', " "), plain(k, v)))
                .collect::<Vec<_>>()
                .join(", "),
        },
    }
}

#[cfg(test)]
mod tests {
    use archon_core::protocol::{RepoPatch, RepoRef};
    use archon_core::repo::ShipMode;
    use serde_json::json;

    use super::*;

    fn chat() -> OperatorChannel {
        OperatorChannel::ArchonChat
    }

    fn repo_before() -> Value {
        json!({"added_at":"2026-10-29T12:07:32Z","attribution":"item-id","autonomy_max":{},
            "base_branch":"main","default_pipeline":null,
            "effective_autonomy":{"ceiling":{"merge":"ask"}},"grant_merge":false,
            "id":"repo_01M63RXT1NS7VTZKT184FNJQMJ","name":"archon",
            "path":"/home/x/archon","ship":"pr"})
    }

    #[test]
    fn repo_update_shows_only_changed_fields() {
        let action: Request = serde_json::from_value(json!({"cmd":"repo_update","repo":"archon",
            "patch":{"ship":"local","base_branch":{"set":"next"},"default_pipeline":"keep","autonomy_max":null}}))
        .unwrap();
        let text = question(&action, &repo_before(), &chat());
        assert_eq!(
            text,
            "Change repository \"archon\"?\n- Ship: pull request -> local branch only\n- Base branch: main -> next\nRequested in chat.\nReply yes or no (or /answer N yes). A bare reply counts only as your next message, while this is the only open confirmation."
        );
        for leaked in [
            "{",
            "repo_01",
            "added_at",
            "/home/x",
            "Default pipeline",
            "effective",
        ] {
            assert!(!text.contains(leaked), "{leaked} leaked: {text}");
        }
    }

    #[test]
    fn github_origin_and_flags_read_plainly() {
        let action = Request::RepoUpdate {
            repo: RepoRef("archon".into()),
            patch: RepoPatch {
                grant_merge: Some(true),
                ship: Some(ShipMode::Pr),
                ..RepoPatch::default()
            },
        };
        let channel = OperatorChannel::Github {
            repository: archon_core::github::GithubRepo::try_from("tiiuae/archon".to_owned())
                .unwrap(),
            number: 7,
            login: "ops".into(),
        };
        let text = question(&action, &repo_before(), &channel);
        assert!(text.contains("- Grant merge: no -> yes"), "{text}");
        assert!(
            text.contains("- Ship: pull request -> pull request"),
            "{text}"
        );
        assert!(
            text.contains("Requested on GitHub, tiiuae/archon#7, by ops."),
            "{text}"
        );
    }

    #[test]
    fn settings_caps_and_item_requests_read_plainly() {
        let set: Request = serde_json::from_value(json!({"cmd":"set_settings","repo":"archon",
            "settings":{"max_per_repo":3}}))
        .unwrap();
        let before = json!({"scoped":{"max_per_repo":1,"default_pipeline":"fast"},"effective":{}});
        let text = question(&set, &before, &chat());
        assert!(
            text.starts_with("Change settings of repository \"archon\"?"),
            "{text}"
        );
        assert!(text.contains("- Max per repo: 1 -> 3"), "{text}");
        assert!(
            text.contains("- Default pipeline: fast -> not set"),
            "{text}"
        );

        let reset: Request =
            serde_json::from_value(json!({"cmd":"reset_settings","repo":null})).unwrap();
        let text = question(&reset, &before, &chat());
        assert!(
            text.starts_with("Reset settings of the global settings"),
            "{text}"
        );
        assert!(!text.contains('{'), "{text}");

        let old_caps = archon_core::queue::Caps {
            max_per_repo: 2,
            ..Default::default()
        };
        let new_caps = archon_core::queue::Caps {
            max_per_repo: 4,
            ..old_caps.clone()
        };
        let caps = Request::SetCaps { caps: new_caps };
        let before = json!({"effective":{"caps":old_caps}});
        let text = question(&caps, &before, &chat());
        assert!(text.contains("- Max per repo: 2 -> 4"), "{text}");
        assert!(!text.contains("Max parallel agents"), "{text}");
    }

    #[test]
    fn other_kinds_fall_back_to_a_key_value_list_without_json_or_secrets() {
        let action: Request = serde_json::from_value(json!({"cmd":"configure_github_watcher",
            "change":{"action":"add","repository":"tiiuae/archon","allowed_users":["ann","bob"]}}))
        .unwrap();
        let text = question(&action, &Value::Null, &chat());
        assert!(
            text.starts_with("Change the GitHub watcher (add)?"),
            "{text}"
        );
        assert!(text.contains("- Repository: tiiuae/archon"), "{text}");
        assert!(text.contains("- Allowed users: ann, bob"), "{text}");
        assert!(
            !text.contains('{') && !text.contains('"') || text.starts_with("Change the GitHub"),
            "{text}"
        );

        assert_eq!(plain("api_token", &json!("abc")), "(hidden)");
        assert_eq!(plain("x", &json!({"a_b": true})), "a b yes");
    }
}
