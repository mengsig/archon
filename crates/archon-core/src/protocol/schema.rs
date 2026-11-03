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

/// A spelling a string enum still reads through `#[serde(alias)]`, which
/// schemars does not see; the derive takes it as `#[schemars(transform)]`.
pub(crate) struct AlsoReads(pub &'static str);

impl schemars::transform::Transform for AlsoReads {
    fn transform(&mut self, schema: &mut schemars::Schema) {
        let alias = self.0;
        let schema = schema
            .as_object_mut()
            .expect("an enum's schema is an object");
        if let Some(Value::Array(names)) = schema.get_mut("enum") {
            assert!(!names.contains(&alias.into()), "{alias} is already a name");
            names.push(alias.into());
            return;
        }
        let Some(Value::Array(variants)) = schema.get_mut("oneOf") else {
            panic!("{alias} is an alias of a string enum, not of {schema:?}");
        };
        variants.push(json!({
            "type": "string",
            "const": alias,
            "description": "A legacy spelling: read, never written.",
        }));
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

    /// A model is accepted as `provider/model` or as `{provider, model}`, so
    /// the schema names both; it once named only the object.
    #[test]
    fn a_model_reference_takes_either_shape_the_daemon_reads() {
        let schema = schema();
        let shapes = definition(&schema, "ModelRef")["anyOf"]
            .as_array()
            .expect("a choice of shapes");
        let types: Vec<&str> = shapes.iter().filter_map(|s| s["type"].as_str()).collect();
        assert_eq!(types, ["string", "object"], "{shapes:?}");
        assert_eq!(
            definition(&schema, "ModelChoiceDraft")["properties"]["model"]["anyOf"][0]["$ref"],
            "#/$defs/ModelRef"
        );
        let retry = |model: Value| {
            let frame = json!({
                "req": 1, "cmd": "retry_item", "item": crate::id::QueueItemId::new(),
                "model_plan": {"build": [{"model": model}]}
            });
            let frame: ClientFrame = serde_json::from_value(frame).expect("a retry frame");
            let super::super::Request::RetryItem { model_plan, .. } = frame.request else {
                panic!("{:?}", frame.request);
            };
            model_plan.expect("a plan").0["build"][0].model.clone()
        };
        let text = retry(json!("codex/astra"));
        assert_eq!(text, retry(json!({"provider": "codex", "model": "astra"})));
        assert_eq!(text.expect("a model").to_string(), "codex/astra");
    }

    /// The strings a string enum's definition admits, aliases among them.
    fn admitted(schema: &Value, name: &str) -> Vec<String> {
        let def = definition(schema, name);
        let variants = def["oneOf"]
            .as_array()
            .cloned()
            .unwrap_or_else(|| vec![def.clone()]);
        let mut names = Vec::new();
        for variant in variants {
            names.extend(variant["const"].as_str().map(str::to_owned));
            let listed = variant["enum"].as_array().into_iter().flatten();
            names.extend(listed.filter_map(Value::as_str).map(str::to_owned));
        }
        assert!(!names.is_empty(), "{name} is a string enum: {def}");
        names
    }

    /// A legacy spelling the daemon reads is one the schema admits, so a
    /// request that decodes also validates.
    #[test]
    fn the_schema_admits_the_legacy_spellings_serde_reads() {
        let schema = schema();
        let frame = json!({
            "req": 1, "cmd": "retry_item", "item": crate::id::QueueItemId::new(),
            "model_plan": {"build": [{"tier": "strongest", "effort": "xhigh"}]}
        });
        let frame: ClientFrame = serde_json::from_value(frame).expect("a retry frame");
        let super::super::Request::RetryItem { model_plan, .. } = frame.request else {
            panic!("{:?}", frame.request);
        };
        let choice = &model_plan.expect("a plan").0["build"][0];
        assert_eq!(choice.tier, Some(crate::model_tier::Tier::Strong));
        assert_eq!(choice.effort, Some(crate::model::Effort::XHigh));
        let done: crate::initiative::InitiativeState =
            serde_json::from_value(json!("succeeded")).expect("the old name of done");
        assert_eq!(done, crate::initiative::InitiativeState::Done);
        for (name, alias, canonical) in [
            ("Tier", "strongest", "strong"),
            ("Effort", "xhigh", "x_high"),
            ("InitiativeState", "succeeded", "done"),
        ] {
            let names = admitted(&schema, name);
            assert!(names.iter().any(|n| n == alias), "{name}: {names:?}");
            assert!(names.iter().any(|n| n == canonical), "{name}: {names:?}");
        }
    }

    /// Every `#[serde(alias)]` in this crate shows in the schema: a new one
    /// fails here until its type takes `AlsoReads`.
    #[test]
    fn every_serde_alias_is_in_the_schema() {
        let text = schema_text();
        let mut pending = vec![std::path::PathBuf::from(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/src"
        ))];
        let mut aliases = Vec::new();
        while let Some(dir) = pending.pop() {
            for entry in std::fs::read_dir(&dir).expect("the crate's sources") {
                let path = entry.expect("a source entry").path();
                if path.is_dir() {
                    pending.push(path);
                    continue;
                }
                let source = std::fs::read_to_string(&path).expect("a source file");
                let marker = concat!("#[serde(", "alias = \"");
                for (at, _) in source.match_indices(marker) {
                    let rest = &source[at + marker.len()..];
                    aliases.push(rest.split('"').next().expect("a quoted alias").to_owned());
                }
            }
        }
        assert!(
            aliases.len() >= 3,
            "xhigh, strongest and succeeded at least: {aliases:?}"
        );
        for alias in aliases {
            assert!(
                text.contains(&format!("\"{alias}\"")),
                "{alias} is missing from the schema"
            );
        }
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
