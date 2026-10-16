//! Summary generation through the public JSON envelope, including legacy input.

use archon_core::id::{PipelineRunId, QuestionId, QueueItemId, ReportId};
use archon_daemon::triggers::TriggerPayload;
use serde_json::{Value, json};

fn serialize(events: Vec<Value>) -> Value {
    let original = json!({
        "archon": 1, "events": events, "directives": [], "directives_in_force": 0
    });
    let payload: TriggerPayload = serde_json::from_value(original.clone()).unwrap();
    let mut output = serde_json::to_value(payload).unwrap();
    let summary = output.as_object_mut().unwrap().remove("summary").unwrap();
    assert_eq!(output, original, "all existing fields are unchanged");
    let text = summary.as_str().unwrap();
    assert!(!text.is_empty());
    assert!(text.chars().count() <= 160, "{text}");
    assert!(!text.chars().any(char::is_control), "{text:?}");
    output["summary"] = summary;
    output
}

fn finished(item: &QueueItemId, title: &str) -> Value {
    json!({
        "type": "item_finished", "item": item, "title": title, "state": "succeeded",
        "summary": "full stage outcome\nkept intact", "pr_url": null, "cost_usd": 1.25
    })
}

fn message(item: &QueueItemId, stage: Option<&str>, text: &str) -> Value {
    json!({
        "type": "agent_message", "item": item, "run": PipelineRunId::new(),
        "stage": stage, "stage_run": null, "attempt": null, "agent": null,
        "kind": "info", "text": text, "original_sender": {"actor": "system"},
        "addressed_to_run_node": null
    })
}

#[test]
fn single_finished_event_names_item_state_and_title() {
    let item = QueueItemId::new();
    let output = serialize(vec![finished(&item, "fix(cost): label estimates")]);
    assert_eq!(
        output["summary"],
        format!("item finished: {item} succeeded — fix(cost): label estimates")
    );
}

#[test]
fn batch_names_each_event_without_relay_bodies() {
    let item = QueueItemId::new();
    let output = serialize(vec![
        finished(&item, "a long title is unnecessary in a batch"),
        message(&item, Some("review"), "full relay body"),
    ]);
    assert_eq!(
        output["summary"],
        format!("2 events: item finished: {item} succeeded, stage info from review ({item})")
    );
}

#[test]
fn long_unicode_titles_are_clipped_at_160_characters() {
    let item = QueueItemId::new();
    let title = format!("fix: {}", "界é".repeat(200));
    let output = serialize(vec![finished(&item, &title)]);
    let text = output["summary"].as_str().unwrap();
    assert_eq!(text.chars().count(), 160);
    assert!(text.ends_with('…'));
    assert!(text.starts_with(&format!("item finished: {item} succeeded — fix: ")));
}

#[test]
fn exactly_160_characters_are_not_clipped() {
    let item = QueueItemId::new();
    let prefix = format!("item finished: {item} succeeded — ");
    for length in [159, 160, 161] {
        let title = "x".repeat(length - prefix.chars().count());
        let output = serialize(vec![finished(&item, &title)]);
        let text = output["summary"].as_str().unwrap();
        assert_eq!(text.chars().count(), length.min(160));
        assert_eq!(text.ends_with('…'), length > 160);
    }
}

#[test]
fn multiline_relay_excerpt_is_one_line_and_run_node_is_named() {
    let item = QueueItemId::new();
    let output = serialize(vec![message(
        &item,
        None,
        " first\r\n\tsecond\u{2028}third  \0\x1b",
    )]);
    assert_eq!(
        output["summary"].as_str().unwrap(),
        format!("stage info from run node ({item}): first second third")
    );
}

#[test]
fn stage_messages_name_each_kind() {
    let item = QueueItemId::new();
    for kind in ["relay", "escalation", "info"] {
        let mut event = message(&item, Some("implement"), "hello\nworld");
        event["kind"] = json!(kind);
        assert_eq!(
            serialize(vec![event])["summary"],
            format!("stage {kind} from implement ({item}): hello world")
        );
    }
}

#[test]
fn all_other_event_types_have_human_labels() {
    let item = QueueItemId::new();
    let question = QuestionId::new();
    let report = ReportId::new();
    let cases = [
        (
            json!({"type": "item_blocked", "item": item, "reason": "dependency\nfailed"}),
            format!("item blocked: {item} — dependency failed"),
        ),
        (
            json!({"type": "operator_answer", "question": question, "text": "yes", "number": null, "question_text": "", "options": [], "item": null, "asked_at": null}),
            format!("operator answered {question}: yes"),
        ),
        (
            json!({"type": "lavish_feedback", "report": report, "item": item, "text": "raw", "prompts": []}),
            format!("report feedback: {report} ({item})"),
        ),
        (
            json!({"type": "lavish_feedback", "report": report, "item": null, "text": "raw", "prompts": []}),
            format!("report feedback: {report}"),
        ),
        (
            json!({"type": "budget", "scope": "daily", "spent_usd": 51.0, "limit_usd": 50.0}),
            "budget limit reached: daily".into(),
        ),
        (
            json!({"type": "policy_violation", "agent": "agt_test", "model": "model", "action": "cancelled"}),
            "policy violation: agt_test (model) — cancelled".into(),
        ),
        (
            json!({"type": "news_digest", "dropped": {"budget": 2, "item_finished": 3}, "note": "see list_queue"}),
            "news digest: 5 older events omitted".into(),
        ),
    ];
    for (event, expected) in cases {
        assert_eq!(serialize(vec![event])["summary"], expected);
    }
}

#[test]
fn empty_and_large_batches_stay_nonempty_and_bounded() {
    assert_eq!(serialize(vec![])["summary"], "Archon update: no events");
    let output = serialize(
        (0..100)
            .map(|_| finished(&QueueItemId::new(), "title"))
            .collect(),
    );
    let text = output["summary"].as_str().unwrap();
    assert!(text.starts_with("100 events: item finished:"));
    assert_eq!(text.chars().count(), 160);
    assert!(text.ends_with('…'));
}

#[test]
fn summary_is_derived_even_when_input_has_a_stale_label() {
    let mut input = serialize(vec![finished(&QueueItemId::new(), "before")]);
    input["summary"] = json!("stale label");
    input["events"][0]["title"] = json!("after");
    let payload: TriggerPayload = serde_json::from_value(input).unwrap();
    let output = serde_json::to_value(payload).unwrap();
    assert!(output["summary"].as_str().unwrap().ends_with(" — after"));
}

#[test]
fn display_summary_strips_unicode_format_controls_and_keeps_the_body() {
    let controls = "\u{061c}\u{200e}\u{200f}\u{202a}\u{202b}\u{202c}\u{202d}\u{202e}\u{2066}\u{2067}\u{2068}\u{2069}\u{200b}\u{feff}\u{00ad}\u{e0001}";
    let title = format!("before{controls}after");
    let output = serialize(vec![finished(&QueueItemId::new(), &title)]);
    assert!(output["summary"].as_str().unwrap().ends_with("beforeafter"));
    assert_eq!(output["events"][0]["title"], title);
}

#[test]
fn operator_answer_summary_identifies_the_question_and_answer() {
    let event = json!({
        "type": "operator_answer", "question": QuestionId::new(), "text": "yes",
        "number": 12, "question_text": "Ship to prod?", "options": ["yes", "no"],
        "item": null, "asked_at": "2026-10-16T09:00:00Z"
    });
    let output = serialize(vec![event.clone()]);
    assert_eq!(output["summary"], "Q12 answered: yes (re: Ship to prod?)");
    assert_eq!(output["events"][0], event);
}

#[test]
fn legacy_operator_answers_remain_readable() {
    let question = QuestionId::new();
    let payload: TriggerPayload = serde_json::from_value(json!({
        "archon": 1, "events": [{"type": "operator_answer", "question": question, "text": "yes"}],
        "directives_in_force": 0
    }))
    .unwrap();
    let output = serde_json::to_value(payload).unwrap();
    assert_eq!(output["events"][0]["question"], json!(question));
    assert_eq!(
        output["summary"],
        format!("operator answered {question}: yes")
    );
    assert!(output["events"][0]["asked_at"].is_null());
}
