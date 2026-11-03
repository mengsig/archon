//! The Archon protocol as one JSON Schema (2020-12) document: `$defs` holds
//! every type reachable from the two frames, and `client_frame` and
//! `server_frame` name the roots. `archon schema` prints it; the committed
//! copy is `crates/archon-core/protocol.schema.json`.

use serde_json::{Value, json};

use super::{ClientFrame, MIN_WEB_PROTOCOL_VERSION, PROTOCOL_VERSION, ServerFrame, feature};

/// The committed schema, relative to the repository root.
pub const SCHEMA_PATH: &str = "crates/archon-core/protocol.schema.json";

/// The whole protocol as a document.
pub fn schema() -> Value {
    let mut generator = schemars::generate::SchemaSettings::draft2020_12().into_generator();
    let client = generator.subschema_for::<ClientFrame>();
    let server = generator.subschema_for::<ServerFrame>();
    let defs = generator.take_definitions(true);
    assert!(!defs.is_empty(), "the frames reach at least one definition");
    json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "title": "Archon daemon protocol",
        "protocol_version": PROTOCOL_VERSION,
        "min_web_protocol_version": MIN_WEB_PROTOCOL_VERSION,
        "features": feature::ALL,
        "client_frame": client,
        "server_frame": server,
        "$defs": defs,
    })
}

/// The document as the committed file holds it: pretty JSON, sorted keys
/// and a final newline, so the same types always give the same bytes.
pub fn schema_text() -> String {
    let value = sorted(schema());
    let mut text = serde_json::to_string_pretty(&value).expect("a JSON value always encodes");
    text.push('\n');
    text
}

/// Sort every object's keys: the bytes must not hang on whether some crate
/// in the build turns on serde_json's `preserve_order`.
fn sorted(value: Value) -> Value {
    match value {
        Value::Object(map) => {
            let entries: std::collections::BTreeMap<String, Value> =
                map.into_iter().map(|(k, v)| (k, sorted(v))).collect();
            Value::Object(entries.into_iter().collect())
        }
        Value::Array(items) => Value::Array(items.into_iter().map(sorted).collect()),
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn definition<'a>(schema: &'a Value, name: &str) -> &'a Value {
        schema["$defs"]
            .get(name)
            .unwrap_or_else(|| panic!("{name} is missing from the schema"))
    }

    /// The committed file is what the types give now. When this fails, run
    /// `cargo run -p archon -- schema --out crates/archon-core/protocol.schema.json`.
    #[test]
    fn the_committed_schema_is_current() {
        let committed = include_str!("../../protocol.schema.json");
        assert!(
            committed == schema_text(),
            "{SCHEMA_PATH} is stale: run `cargo run -p archon -- schema --out {SCHEMA_PATH}`"
        );
    }

    #[test]
    fn the_schema_text_is_deterministic() {
        let first = schema_text();
        assert_eq!(first, schema_text());
        assert!(first.ends_with("}\n"), "one final newline");
    }

    /// Both roots resolve, and protocol 24's types are all defined;
    /// `ChangeSubject` is flattened into `Change`, so it has no definition.
    #[test]
    fn the_schema_carries_both_frames_and_protocol_24() {
        let schema = schema();
        assert_eq!(schema["protocol_version"], PROTOCOL_VERSION);
        for root in ["client_frame", "server_frame"] {
            let reference = schema[root]["$ref"].as_str().expect("a reference");
            let name = reference.strip_prefix("#/$defs/").expect("a local one");
            definition(&schema, name);
        }
        for name in [
            "Welcome",
            "ErrorCode",
            "Change",
            "TaskGraph",
            "TaskNode",
            "TaskEdge",
            "RunPlan",
            "GithubRef",
            "PrRef",
            "GithubLink",
            "GithubObject",
            "ItemTransition",
        ] {
            definition(&schema, name);
        }
        let subjects = definition(&schema, "Change")["oneOf"]
            .as_array()
            .expect("subjects");
        assert_eq!(subjects.len(), 8, "one variant per ChangeSubject");
    }

    /// Requests are flattened into the frame: a generator sees each command
    /// as one variant tagged by `cmd`, protocol 24's among them.
    #[test]
    fn every_new_command_is_a_tagged_variant() {
        let text = schema_text();
        for cmd in [
            "subscribe_changes",
            "changes_page",
            "item_history",
            "task_graph",
            "run_plan",
            "item_links",
            "issue_items",
            "link_issue",
            "unlink_issue",
            "list_github_objects",
            "import_issue",
        ] {
            assert!(text.contains(&format!("\"const\": \"{cmd}\"")), "{cmd}");
        }
        assert!(text.contains("\"const\": \"change\""), "the change frame");
    }
}
