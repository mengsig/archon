//! Whether the `archon` server aetherd runs is this archon: one started from
//! an older binary keeps the tools it listed then, until it is refreshed.

use std::collections::BTreeSet;

use archon_aether::AetherClient;
use archon_aether::wire::Command;
use archon_core::config::AetherMode;
use archon_core::error::ArchonError;
use archon_core::mcp_tools::SERVER_NAME;
use serde_json::Value;

use super::{Check, Level, check};

const NAME: &str = "mcp tools";
/// The differing tool names a stale verdict spells out before it counts the rest.
const NAMED: usize = 4;

/// Compare the tools aetherd discovered from `archon` with the ones this
/// binary serves; `None` when the server is not ready, which `mcp health` reports.
pub(super) async fn check_mcp_tools(
    aether: Option<&AetherClient>,
    mode: AetherMode,
) -> Option<Check> {
    let client = aether?;
    let command = Command::McpServers {
        refresh: false,
        server: Some(SERVER_NAME.to_owned()),
    };
    let servers = match client.request(command, "mcp_servers").await {
        Ok(servers) => servers,
        Err(ArchonError::NotFound(_)) => return None,
        Err(e) => return Some(check(NAME, Level::Warn, format!("not checked: {e}"))),
    };
    let served = archon_mcp::tools::definitions();
    let served: BTreeSet<&str> = served.iter().map(|d| d.name).collect();
    tools_verdict(&servers, &served, mode)
}

/// The verdict on the `archon` entry of an `mcp_servers` reply against the
/// tool names this binary serves.
fn tools_verdict(servers: &Value, served: &BTreeSet<&str>, mode: AetherMode) -> Option<Check> {
    assert!(!served.is_empty(), "this archon serves tools");
    let server = servers
        .as_array()?
        .iter()
        .find(|s| s.get("name").and_then(Value::as_str) == Some(SERVER_NAME))?;
    if server.get("state").and_then(Value::as_str) != Some("ready") {
        return None;
    }
    let Some(listed) = server.get("tools").and_then(Value::as_array) else {
        return Some(check(
            NAME,
            Level::Warn,
            "not checked: aetherd listed no tools",
        ));
    };
    let listed: BTreeSet<&str> = listed
        .iter()
        .filter_map(|t| t.get("name").and_then(Value::as_str))
        .collect();
    if listed == *served {
        let detail = format!("aetherd has this archon's {} tools", served.len());
        return Some(check(NAME, Level::Ok, detail));
    }
    let missing: Vec<&str> = served.difference(&listed).copied().collect();
    let extra: Vec<&str> = listed.difference(served).copied().collect();
    assert!(
        !missing.is_empty() || !extra.is_empty(),
        "unequal sets differ somewhere"
    );
    Some(check(
        NAME,
        Level::Fail,
        stale_detail(&missing, &extra, mode),
    ))
}

fn stale_detail(missing: &[&str], extra: &[&str], mode: AetherMode) -> String {
    let refresh = match mode {
        AetherMode::Dedicated => "archon aether mcp refresh archon",
        AetherMode::Shared => "aether mcp refresh archon",
    };
    let mut differences = Vec::new();
    if !missing.is_empty() {
        differences.push(format!("lacks {}", named(missing)));
    }
    if !extra.is_empty() {
        differences.push(format!("still has {}", named(extra)));
    }
    assert!(!differences.is_empty(), "a stale registration differs");
    format!(
        "aetherd runs a stale archon server: it {}; run `{refresh}`, or `archon setup`",
        differences.join(" and ")
    )
}

fn named(tools: &[&str]) -> String {
    assert!(!tools.is_empty(), "only differing tools are named");
    let shown = tools[..tools.len().min(NAMED)].join(", ");
    match tools.len().checked_sub(NAMED) {
        Some(rest) if rest > 0 => format!("{shown} and {rest} more"),
        _ => shown,
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn reply(state: &str, tools: &[&str]) -> Value {
        let tools: Vec<Value> = tools.iter().map(|name| json!({ "name": name })).collect();
        json!([{"name": "other", "state": "ready", "tools": [{"name": "x"}]},
               {"name": SERVER_NAME, "state": state, "tools": tools}])
    }

    #[test]
    fn a_registration_listing_other_tools_is_stale_and_names_the_fix() {
        let served = BTreeSet::from(["status", "submit", "watch"]);
        let current = reply("ready", &["status", "submit", "watch"]);
        let fresh = tools_verdict(&current, &served, AetherMode::Shared).unwrap();
        assert_eq!(fresh.level, Level::Ok, "{}", fresh.detail);
        let old = reply("ready", &["status", "submit", "retired"]);
        let stale = tools_verdict(&old, &served, AetherMode::Dedicated).unwrap();
        assert_eq!(stale.level, Level::Fail);
        assert!(
            stale.detail.contains("lacks watch and still has retired"),
            "{}",
            stale.detail
        );
        assert!(stale.detail.contains("`archon aether mcp refresh archon`"));
        let shared = tools_verdict(&old, &served, AetherMode::Shared).unwrap();
        assert!(
            shared
                .detail
                .contains("run `aether mcp refresh archon`, or `archon setup`")
        );
    }

    #[test]
    fn a_server_that_is_not_ready_or_absent_is_left_to_mcp_health() {
        let served = BTreeSet::from(["status"]);
        let mode = AetherMode::Dedicated;
        assert_eq!(tools_verdict(&reply("unknown", &[]), &served, mode), None);
        assert_eq!(tools_verdict(&reply("failed", &[]), &served, mode), None);
        assert_eq!(tools_verdict(&json!([]), &served, mode), None);
        let bare = json!([{"name": SERVER_NAME, "state": "ready"}]);
        assert_eq!(
            tools_verdict(&bare, &served, mode).unwrap().level,
            Level::Warn
        );
    }

    #[test]
    fn a_long_difference_is_counted_past_the_first_names() {
        assert_eq!(named(&["a"]), "a");
        assert_eq!(named(&["a", "b", "c", "d"]), "a, b, c, d");
        assert_eq!(
            named(&["a", "b", "c", "d", "e", "f"]),
            "a, b, c, d and 2 more"
        );
    }

    #[test]
    fn this_binary_serves_the_tools_aether_would_list() {
        let served = archon_mcp::tools::definitions();
        let names: BTreeSet<&str> = served.iter().map(|d| d.name).collect();
        assert_eq!(names.len(), served.len(), "tool names are unique");
        let listed: Vec<&str> = names.iter().copied().collect();
        let mode = AetherMode::Dedicated;
        let verdict = tools_verdict(&reply("ready", &listed), &names, mode).unwrap();
        assert_eq!(verdict.level, Level::Ok);
    }
}
