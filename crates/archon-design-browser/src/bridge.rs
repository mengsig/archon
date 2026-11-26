//! The page bridge: a script in the preview's main world reports what a
//! headless browser cannot show (new tabs, native pickers, copied text).
//! Its messages come from the page, so each is parsed strictly and capped.

use serde_json::Value;

use crate::sanitize::clean;

/// The script, installed before the app's own on every document.
pub(crate) const SCRIPT: &str = include_str!("bridge.js");
/// The binding the script calls.
pub(crate) const BINDING: &str = "__archonDesignBridge";
/// Largest message read from the page.
const MAX_MESSAGE_BYTES: usize = 256 * 1024;
/// Most options a dropdown reports.
pub const MAX_OPTIONS: usize = 500;
/// Longest copied text kept, in characters.
pub const MAX_COPY_CHARS: usize = 65_536;
/// Longest picker value kept.
const MAX_VALUE_CHARS: usize = 64;

/// One option of a native dropdown.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PickerOption {
    /// The text shown.
    pub label: String,
    /// The option's index, as the answer names it.
    pub value: String,
    /// Selected now.
    pub selected: bool,
    /// Cannot be chosen.
    pub disabled: bool,
    /// Its `<optgroup>` label, or empty.
    pub group: String,
}

/// A native picker the page opened, answered with `Browser::answer_picker`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PickerRequest {
    /// The page's id for it.
    pub id: u64,
    /// `select`, `date`, `time`, `datetime-local`, `month`, `week` or `color`.
    pub control: String,
    /// The current value (a select's selected index).
    pub value: String,
    /// `min`, when set.
    pub min: Option<String>,
    /// `max`, when set.
    pub max: Option<String>,
    /// `step`, when set.
    pub step: Option<String>,
    /// A select's options.
    pub options: Vec<PickerOption>,
}

/// A message the bridge sent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum BridgeMessage {
    /// A new tab or popup opened in the preview instead; empty for a blank popup.
    Tab(String),
    /// A native picker opened.
    Picker(PickerRequest),
    /// Text was copied.
    Copy(String),
}

const CONTROLS: [&str; 7] = [
    "select",
    "date",
    "time",
    "datetime-local",
    "month",
    "week",
    "color",
];

/// Parse one binding payload; `None` for anything malformed or oversized.
pub(crate) fn parse(payload: &str) -> Option<BridgeMessage> {
    if payload.len() > MAX_MESSAGE_BYTES {
        return None;
    }
    let message: Value = serde_json::from_str(payload).ok()?;
    match message["kind"].as_str()? {
        "tab" => Some(BridgeMessage::Tab(clean_url(message["url"].as_str()?))),
        "copy" => {
            let text: String = message["text"]
                .as_str()?
                .chars()
                .take(MAX_COPY_CHARS)
                .collect();
            let text = text.replace('\0', "");
            (!text.is_empty()).then_some(BridgeMessage::Copy(text))
        }
        "picker" => parse_picker(&message).map(BridgeMessage::Picker),
        _ => None,
    }
}

fn clean_url(url: &str) -> String {
    clean(url)
}

fn parse_picker(message: &Value) -> Option<PickerRequest> {
    let id = message["id"].as_u64().filter(|id| *id > 0)?;
    let control = message["control"].as_str()?;
    if !CONTROLS.contains(&control) {
        return None;
    }
    let field = |name: &str| {
        message[name]
            .as_str()
            .filter(|s| !s.is_empty())
            .map(|s| clean(s).chars().take(MAX_VALUE_CHARS).collect::<String>())
    };
    let options = if control == "select" {
        let list = message["options"].as_array()?;
        if list.len() > MAX_OPTIONS {
            return None;
        }
        list.iter().map(parse_option).collect::<Option<Vec<_>>>()?
    } else {
        Vec::new()
    };
    let picker = PickerRequest {
        id,
        control: control.to_owned(),
        value: field("value").unwrap_or_default(),
        min: field("min"),
        max: field("max"),
        step: field("step"),
        options,
    };
    assert!(
        picker.control == "select" || picker.options.is_empty(),
        "only a dropdown lists options"
    );
    Some(picker)
}

fn parse_option(option: &Value) -> Option<PickerOption> {
    let value = option["value"].as_str()?;
    if value.is_empty() || value.len() > 6 || !value.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some(PickerOption {
        label: clean(option["label"].as_str().unwrap_or_default()),
        value: value.to_owned(),
        selected: option["selected"].as_bool().unwrap_or(false),
        disabled: option["disabled"].as_bool().unwrap_or(false),
        group: clean(option["group"].as_str().unwrap_or_default()),
    })
}

/// The expression that answers picker `id` in the page: JSON literals only,
/// so nothing the operator typed is ever evaluated as code.
pub(crate) fn answer_expression(id: u64, value: Option<&str>) -> String {
    let value = serde_json::to_string(&value).expect("a string or null serialises");
    format!("window.__archonDesign ? window.__archonDesign.answer({id}, {value}) : false")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_each_message_kind() {
        let tab = parse(r#"{"kind":"tab","url":"http://127.0.0.1:1/a\u0007"}"#);
        assert_eq!(tab, Some(BridgeMessage::Tab("http://127.0.0.1:1/a".into())));
        let copy = parse(r#"{"kind":"copy","text":"x\u0000y"}"#);
        assert_eq!(copy, Some(BridgeMessage::Copy("xy".into())));
        let select = r#"{"kind":"picker","id":3,"control":"select","value":"1",
            "options":[{"label":"A","value":"0","selected":false,"disabled":false,"group":""},
                       {"label":"B","value":"1","selected":true,"disabled":true,"group":"G"}]}"#;
        let Some(BridgeMessage::Picker(picker)) = parse(select) else {
            panic!("a select picker parses");
        };
        assert_eq!(picker.options.len(), 2);
        assert!(picker.options[1].selected && picker.options[1].disabled);
        let date = r#"{"kind":"picker","id":4,"control":"date","value":"2026-01-02","min":"","max":"2027-01-01"}"#;
        let Some(BridgeMessage::Picker(picker)) = parse(date) else {
            panic!("a date picker parses");
        };
        assert_eq!(
            (picker.min, picker.max.as_deref()),
            (None, Some("2027-01-01"))
        );
    }

    #[test]
    fn refuses_malformed_or_oversized_messages() {
        assert_eq!(parse("not json"), None);
        assert_eq!(parse(r#"{"kind":"eval","code":"x"}"#), None);
        assert_eq!(parse(r#"{"kind":"picker","id":0,"control":"date"}"#), None);
        assert_eq!(parse(r#"{"kind":"picker","id":1,"control":"file"}"#), None);
        let bad_value =
            r#"{"kind":"picker","id":1,"control":"select","options":[{"value":"1; alert(1)"}]}"#;
        assert_eq!(parse(bad_value), None);
        let many: Vec<_> = (0..=MAX_OPTIONS)
            .map(|i| format!(r#"{{"label":"o","value":"{i}"}}"#))
            .collect();
        let too_many = format!(
            r#"{{"kind":"picker","id":1,"control":"select","options":[{}]}}"#,
            many.join(",")
        );
        assert_eq!(parse(&too_many), None);
        assert_eq!(parse(&"x".repeat(MAX_MESSAGE_BYTES + 1)), None);
        assert_eq!(parse(r#"{"kind":"copy","text":""}"#), None);
    }

    #[test]
    fn answers_are_json_literals() {
        assert_eq!(
            answer_expression(7, Some("\"); alert(1); (\"")),
            r#"window.__archonDesign ? window.__archonDesign.answer(7, "\"); alert(1); (\"") : false"#
        );
        assert!(answer_expression(7, None).contains("answer(7, null)"));
    }
}
