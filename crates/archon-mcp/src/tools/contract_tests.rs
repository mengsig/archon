//! Each advertised schema checked against the argument handler, so the two
//! cannot drift: arguments the handler accepts satisfy the schema, and
//! arguments the schema refuses the handler refuses too.

use std::collections::BTreeSet;

use serde_json::{Map, json};

use super::tests::{plan, qi, samples};
use super::*;

#[test]
fn messaging_defaults_to_info_and_returns_a_non_blocking_receipt() {
    let arguments = json!({"text": "é".repeat(archon_core::agent_message::MAX_MESSAGE_CHARS)});
    assert!(schema_verdict("message_orchestrator", &arguments).is_ok());
    assert!(matches!(
        to_request("message_orchestrator", arguments).unwrap(),
        Request::MessageOrchestrator {
            kind: archon_core::agent_message::MessageKind::Info,
            caller: None,
            ..
        }
    ));
    assert!(to_request("message_orchestrator", json!({"text": " \n\t"})).is_err());
    let receipt = archon_core::id::FeedItemId::new();
    let rendered = render(
        "message_orchestrator",
        &Reply::MessageReceipt {
            receipt: receipt.clone(),
        },
    );
    assert!(rendered.contains(receipt.as_str()));
    assert!(rendered.contains("Keep working") && rendered.contains("do not wait"));
}

/// Every keyword `validate` checks. A schema using another one would be
/// checked only in part, so `schemas_use_only_checked_keywords` forbids it.
const CHECKED_KEYWORDS: &[&str] = &[
    "type",
    "description",
    "properties",
    "required",
    "additionalProperties",
    "enum",
    "items",
    "minItems",
    "minProperties",
    "minimum",
    "maximum",
    "exclusiveMinimum",
    "minLength",
    "maxLength",
    "anyOf",
];

fn keywords(schema: &Value, found: &mut BTreeSet<String>) {
    let Some(schema) = schema.as_object() else {
        return;
    };
    found.extend(schema.keys().cloned());
    let properties = schema.get("properties").and_then(Value::as_object);
    let branches = schema.get("anyOf").and_then(Value::as_array);
    let nested = properties
        .into_iter()
        .flat_map(Map::values)
        .chain(branches.into_iter().flatten())
        .chain(schema.get("items"))
        .chain(schema.get("additionalProperties"));
    for child in nested {
        keywords(child, found);
    }
}

/// A JSON Schema check for the keywords in `CHECKED_KEYWORDS`.
fn validate(schema: &Value, value: &Value, at: &str) -> Result<(), String> {
    assert!(schema.is_object(), "{at}: a schema is an object");
    if let Some(branches) = schema.get("anyOf").and_then(Value::as_array) {
        assert!(!branches.is_empty(), "{at}: anyOf lists branches");
        if !branches.iter().any(|b| validate(b, value, at).is_ok()) {
            return Err(format!("{at}: {value} matches no anyOf branch"));
        }
    }
    let kind = schema.get("type").and_then(Value::as_str);
    if let Some(kind) = kind.filter(|kind| !has_type(value, kind)) {
        return Err(format!("{at}: {value} is not of type {kind}"));
    }
    let allowed = schema.get("enum").and_then(Value::as_array);
    if allowed.is_some_and(|allowed| !allowed.contains(value)) {
        return Err(format!("{at}: {value} is not one of {allowed:?}"));
    }
    check_bounds(schema, value).map_err(|why| format!("{at}: {why}"))?;
    match value {
        Value::Object(fields) => validate_object(schema, fields, at),
        Value::Array(items) => match schema.get("items") {
            Some(item) => items
                .iter()
                .enumerate()
                .try_for_each(|(i, value)| validate(item, value, &format!("{at}[{i}]"))),
            None => Ok(()),
        },
        _ => Ok(()),
    }
}

fn has_type(value: &Value, kind: &str) -> bool {
    match kind {
        "object" => value.is_object(),
        "array" => value.is_array(),
        "string" => value.is_string(),
        "integer" => value.is_i64() || value.is_u64(),
        "number" => value.is_number(),
        "boolean" => value.is_boolean(),
        other => panic!("type {other:?} is not checked"),
    }
}

/// Numeric bounds, string length and array or object size.
fn check_bounds(schema: &Value, value: &Value) -> Result<(), String> {
    let bound = |key: &str| schema.get(key).and_then(Value::as_f64);
    let count = |key: &str| schema.get(key).and_then(Value::as_u64);
    if let Some(n) = value.as_f64() {
        let below = bound("minimum").is_some_and(|min| n < min);
        let above = bound("maximum").is_some_and(|max| n > max);
        let not_above = bound("exclusiveMinimum").is_some_and(|min| n <= min);
        if below || above || not_above {
            return Err(format!("{n} is out of bounds"));
        }
    }
    let size = match value {
        Value::String(text) => text.chars().count(),
        Value::Array(items) => items.len(),
        Value::Object(fields) => fields.len(),
        _ => return Ok(()),
    };
    let min = ["minLength", "minItems", "minProperties"]
        .into_iter()
        .find_map(count)
        .unwrap_or(0);
    if count("maxLength").is_some_and(|max| size as u64 > max) {
        return Err(format!("{value} exceeds maxLength"));
    }
    match size as u64 >= min {
        true => Ok(()),
        false => Err(format!("{value} is shorter than {min}")),
    }
}

fn validate_object(schema: &Value, fields: &Map<String, Value>, at: &str) -> Result<(), String> {
    let required = schema.get("required").and_then(Value::as_array);
    let required = required.into_iter().flatten().filter_map(Value::as_str);
    if let Some(missing) = required.into_iter().find(|key| !fields.contains_key(*key)) {
        return Err(format!("{at}: `{missing}` is required"));
    }
    let properties = schema.get("properties");
    for (key, field) in fields {
        let at = format!("{at}.{key}");
        let declared = properties.and_then(|p| p.get(key));
        match (declared, schema.get("additionalProperties")) {
            (Some(property), _) => validate(property, field, &at)?,
            (None, Some(Value::Bool(false))) => return Err(format!("{at} is not declared")),
            (None, Some(extra)) if extra.is_object() => validate(extra, field, &at)?,
            (None, _) => {}
        }
    }
    Ok(())
}

/// Omitted `arguments` stand for an empty object, as the handler reads them.
fn schema_verdict(tool: &str, arguments: &Value) -> Result<(), String> {
    let schema = &definition(tool).expect("a defined tool").input_schema;
    let empty = json!({});
    let arguments = if arguments.is_null() {
        &empty
    } else {
        arguments
    };
    validate(schema, arguments, tool)
}

#[test]
fn schemas_use_only_checked_keywords() {
    let mut found = BTreeSet::new();
    for definition in definitions() {
        keywords(&definition.input_schema, &mut found);
    }
    let unchecked: Vec<&String> = found
        .iter()
        .filter(|k| !CHECKED_KEYWORDS.contains(&k.as_str()))
        .collect();
    assert!(unchecked.is_empty(), "teach `validate` {unchecked:?}");
    let nested = found.contains("anyOf") && found.contains("exclusiveMinimum");
    assert!(nested, "the walk reaches nested schemas: {found:?}");
}

#[test]
fn arguments_the_handler_accepts_satisfy_the_schema() {
    let alias =
        json!({"implement": [{"model": {"provider": "codex", "model": "m"}, "effort": "xhigh"}]});
    let mut accepted = samples();
    accepted.push(("retry_item", json!({"item": qi(), "model_plan": alias})));
    accepted.push(("queue_status", json!({})));
    accepted.push(("cost_report", json!({})));
    accepted.push(("model_evidence", json!({})));
    for tool in ["enqueue", "update_item", "retry_item"] {
        let mut arguments = if tool == "enqueue" {
            enqueue_with("model_reasons", json!({})).1
        } else {
            json!({"item": qi()})
        };
        arguments["model_reasons"] = json!({"implement": "é".repeat(300)});
        accepted.push((tool, arguments));
    }
    for (tool, arguments) in accepted {
        assert!(
            to_request(tool, arguments.clone()).is_ok(),
            "{tool} {arguments}"
        );
        if let Err(why) = schema_verdict(tool, &arguments) {
            panic!("the schema refuses what {tool} accepts: {why}");
        }
    }
}

fn enqueue_with(key: &str, value: Value) -> (&'static str, Value) {
    let mut arguments = json!({"title": "t", "brief": "b", "model_plan": plan()});
    arguments[key] = value;
    ("enqueue", arguments)
}

/// One choice for the `implement` stage of enqueue's plan, the only plan
/// whose shape the schemas spell out.
fn plan_with(choice: Value) -> (&'static str, Value) {
    enqueue_with("model_plan", json!({"implement": [choice]}))
}

/// One violation of each keyword the schemas use, at the top level and in
/// nested model choices and autonomy knobs.
fn schema_violations() -> Vec<(&'static str, Value)> {
    let mut violations = reading_tool_violations();
    violations.extend(acting_tool_violations());
    violations
}

/// Violations in the tools that read Archon or message the orchestrator.
fn reading_tool_violations() -> Vec<(&'static str, Value)> {
    vec![
        ("message_orchestrator", json!({})),
        ("message_orchestrator", json!({"text": ""})),
        (
            "message_orchestrator",
            json!({"text": "hi", "kind": "urgent"}),
        ),
        ("message_orchestrator", json!({"text": 7})),
        (
            "message_orchestrator",
            json!({"text": "hi", "caller": "agt_1"}),
        ),
        (
            "message_orchestrator",
            json!({"text": "é".repeat(archon_core::agent_message::MAX_MESSAGE_CHARS + 1)}),
        ),
        ("get_item", json!({})),
        ("get_stage", json!({"item": qi(), "stage": ""})),
        (
            "get_stage",
            json!({"item": qi(), "stage": "review", "attempt": 0}),
        ),
        (
            "get_stage",
            json!({"item": qi(), "stage": "review", "slot": -1}),
        ),
        (
            "get_stage",
            json!({"item": qi(), "stage": "review", "limit": 0}),
        ),
        (
            "get_stage",
            json!({"item": qi(), "stage": "review", "limit": archon_core::protocol::MAX_STAGE_PAGE_CHARS + 1}),
        ),
        ("list_repos", json!({"verbose": true})),
        ("model_policy", json!({"repo": 5})),
        ("list_queue", json!({"states": ["done"]})),
        ("cost_report", json!({"by": "week"})),
        ("model_evidence", json!({"pipeline": ""})),
        ("model_evidence", json!({"stage": 3})),
        ("model_evidence", json!({"since": ""})),
        ("model_evidence", json!({"repo": 7})),
        ("cost_report", json!({"since": ""})),
        ("list_queue", json!({"limit": 0})),
        ("list_queue", json!({"limit": "5"})),
    ]
}

/// Violations in the tools that change Archon or reach the operator.
fn acting_tool_violations() -> Vec<(&'static str, Value)> {
    vec![
        enqueue_with("model_reasons", json!({"implement": "a".repeat(301)})),
        (
            "retry_item",
            json!({"item": qi(), "model_reasons": {"review": 1}}),
        ),
        ("update_item", json!({"item": qi(), "model_reasons": []})),
        (
            "set_directive",
            json!({"text": "t", "expires_in_hours": 721}),
        ),
        ("update_item", json!({"item": qi(), "budget_usd": 0})),
        ("register_repo", json!({"path": "/srv/web", "ship": "push"})),
        ("notify", json!({"kind": "shout", "text": "hi"})),
        ("pause", json!({"scope": "everything"})),
        ("clear_directive", json!({"all": "yes"})),
        ("ask_operator", json!({"text": "q", "options": "yes"})),
        ("record_chat_answer", json!({"answer": "yes"})),
        (
            "record_chat_answer",
            json!({"question": 3, "answer": "yes"}),
        ),
        (
            "record_chat_answer",
            json!({"question": "q_1", "answer": "yes", "caller": "agt_1"}),
        ),
        enqueue_with("tags", json!([""])),
        enqueue_with("priority", json!("urgent")),
        enqueue_with("model_plan", json!({})),
        enqueue_with("autonomy", json!({"merge": "always"})),
        enqueue_with("autonomy", json!({"speed": "fast"})),
        enqueue_with("model_plan", json!({"implement": []})),
        ("retry_item", json!({"item": qi(), "model_plan": []})),
        ("update_item", json!({"item": qi(), "model_plan": 7})),
        plan_with(json!({"model": 7, "effort": "high"})),
        plan_with(json!({"model": "ab", "effort": "high"})),
        plan_with(json!({"model": {"provider": "codex"}, "effort": "high"})),
        plan_with(json!({"model": {"provider": "", "model": "m"}, "effort": "high"})),
        plan_with(
            json!({"model": {"provider": "codex", "model": "m", "region": "eu"}, "effort": "high"}),
        ),
        plan_with(json!({"model": "codex/m", "effort": "high", "temperature": 1})),
        plan_with(json!({"model": "codex/m", "effort": "extreme"})),
        plan_with(json!({"model": "codex/m"})),
    ]
}

#[test]
fn arguments_the_schema_refuses_the_handler_refuses() {
    for (tool, arguments) in schema_violations() {
        assert!(
            schema_verdict(tool, &arguments).is_err(),
            "the schema accepts {tool} {arguments}"
        );
        if let Ok(request) = to_request(tool, arguments.clone()) {
            panic!("{tool} accepted {arguments}, which its schema refuses, as {request:?}");
        }
    }
}

#[test]
fn descriptions_promise_only_what_the_daemon_does() {
    let report = definition("publish_report").expect("defined");
    assert!(
        report.description.contains("`.html` or `.htm`"),
        "{}",
        report.description
    );
    assert!(
        report
            .description
            .contains("Markdown included, are refused")
    );
    let path = &report.input_schema["properties"]["path"]["description"];
    assert!(path.as_str().is_some_and(|p| p.contains(".html")), "{path}");
    let update = definition("update_item").expect("defined").description;
    assert!(
        update.contains("the call is refused") && update.contains("retry_item"),
        "{update}"
    );
    assert!(!update.contains("keep their models"), "{update}");
}
