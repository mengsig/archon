//! Human-readable Aether agent names (ARCHITECTURE §4.5). Aether's TUI and
//! `aether agents` truncate names, so the part that tells agents apart
//! comes first and the item's title last.

use archon_core::id::QueueItemId;
use archon_pipeline::SpawnPlan;

use super::clip;

/// Titles are clipped to this many characters in agent names.
const MAX_TITLE_CHARS: usize = 40;
/// Stage ids (valid up to 64 characters) are clipped to this many, so a
/// long id cannot push the title out of a truncated name column.
const MAX_STAGE_CHARS: usize = 24;

/// Item id and stage lead so narrow board columns distinguish work.
pub(crate) fn stage_agent_name(
    item: &QueueItemId,
    plan: &SpawnPlan,
    retry: Option<usize>,
    title: &str,
) -> String {
    assert!(plan.attempt >= 1, "attempts are counted from one");
    assert!(
        plan.fan_index < plan.fan_count,
        "slot {} outside fan_out {}",
        plan.fan_index,
        plan.fan_count
    );
    assert!(!plan.stage_id.is_empty(), "stages have ids");
    let mut name = format!(
        "#{} {}",
        crate::worktree::item_short(item),
        clip(&plan.stage_id, MAX_STAGE_CHARS)
    );
    if plan.fan_count > 1 {
        name.push_str(&format!(" {}/{}", plan.fan_index + 1, plan.fan_count));
    }
    let mut notes = Vec::new();
    if plan.attempt > 1 {
        notes.push(format!("attempt {}", plan.attempt));
    }
    if let Some(retry) = retry {
        assert!(retry >= 1, "retries are counted from one");
        if retry > 1 {
            notes.push(format!("retry {retry}"));
        }
    }
    if !notes.is_empty() {
        name.push_str(&format!(" ({})", notes.join(", ")));
    }
    let title = short_title(title);
    if !title.is_empty() {
        name.push_str(" · ");
        name.push_str(&title);
    }
    name
}

/// Titles are untrusted: drop control/bidi characters, flatten and clip.
pub(crate) fn short_title(title: &str) -> String {
    // Tabs and newlines are controls too; they collapse with the rest.
    let safe: String = title
        .chars()
        .filter(|c| c.is_whitespace() || !is_unsafe(*c))
        .collect();
    let collapsed = safe.split_whitespace().collect::<Vec<_>>().join(" ");
    let short = clip(&collapsed, MAX_TITLE_CHARS);
    debug_assert!(short.chars().count() <= MAX_TITLE_CHARS);
    debug_assert!(!short.chars().any(is_unsafe));
    short
}

/// What Aether's own `strip_controls` drops: C0 and C1 controls, and the
/// bidi embedding, override and isolate marks (`Cf`, so not "control").
fn is_unsafe(c: char) -> bool {
    c.is_control()
        || matches!(c, '\u{80}'..='\u{9f}')
        || matches!(c, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
}

#[cfg(test)]
mod tests {
    use super::*;
    use archon_core::model::{Effort, ModelChoice};

    fn item() -> QueueItemId {
        QueueItemId::parse("qi_01ARZ3NDEKTSV4RRFFQ69G5FAV").unwrap()
    }

    fn plan(stage: &str, attempt: u32, fan_index: u32, fan_count: u32) -> SpawnPlan {
        SpawnPlan {
            stage_id: stage.into(),
            attempt,
            fan_index,
            fan_count,
            choice: ModelChoice {
                model: "claude-code/claude-sonnet-5".parse().unwrap(),
                effort: Effort::Medium,
            },
            prompt: "do it".into(),
            instructions: "contract".into(),
            timeout_minutes: 30,
            allow_delegation: false,
            workspace: archon_core::catalog::WorkspaceMode::Worktree,
            team: None,
            keep: false,
            resume: None,
        }
    }

    #[test]
    fn the_stage_leads_and_the_title_follows() {
        let name = stage_agent_name(&item(), &plan("implement", 1, 0, 1), None, "add a greeting");
        assert_eq!(name, "#q69g5fav implement · add a greeting");
    }

    #[test]
    fn fan_out_slots_count_from_one() {
        let name = stage_agent_name(&item(), &plan("review", 1, 1, 3), None, "add a greeting");
        assert_eq!(name, "#q69g5fav review 2/3 · add a greeting");
        let single = stage_agent_name(&item(), &plan("ship", 1, 0, 1), None, "add a greeting");
        assert!(!single.contains('/'), "{single}");
    }

    #[test]
    fn attempts_and_retries_show_only_when_they_happened() {
        let second = stage_agent_name(&item(), &plan("implement", 2, 0, 1), None, "t");
        assert_eq!(second, "#q69g5fav implement (attempt 2) · t");
        let retried = stage_agent_name(&item(), &plan("review", 1, 0, 2), Some(1), "t");
        assert_eq!(retried, "#q69g5fav review 1/2 · t");
        let both = stage_agent_name(&item(), &plan("implement", 3, 0, 1), Some(2), "t");
        assert_eq!(both, "#q69g5fav implement (attempt 3, retry 2) · t");
    }

    #[test]
    fn control_and_bidi_characters_never_reach_a_name() {
        let hostile = "fix \u{1b}[31mred\u{7} \u{202e}gnp.exe\u{2069} \u{85}end";
        assert_eq!(short_title(hostile), "fix [31mred gnp.exe end");
        assert_eq!(short_title("\u{1b}\u{7}"), "");
        assert_eq!(short_title("naïve · ünïcode ✓"), "naïve · ünïcode ✓");
    }

    #[test]
    fn titles_are_flattened_and_clipped() {
        assert_eq!(short_title("  add\n\ta   greeting "), "add a greeting");
        let long = "x".repeat(100);
        let short = short_title(&long);
        assert_eq!(short.chars().count(), MAX_TITLE_CHARS);
        assert!(short.ends_with('…'));
        assert_eq!(
            stage_agent_name(&item(), &plan("ship", 1, 0, 1), None, "   "),
            "#q69g5fav ship"
        );
    }

    #[test]
    fn a_long_stage_id_leaves_the_title_visible() {
        let stage = "a".repeat(64);
        assert!(
            archon_pipeline::spec::valid_id(&stage),
            "the longest valid id"
        );
        let name = stage_agent_name(&item(), &plan(&stage, 2, 1, 2), Some(1), "add a greeting");
        let clipped = format!("{}…", "a".repeat(MAX_STAGE_CHARS - 1));
        assert_eq!(
            name,
            format!("#q69g5fav {clipped} 2/2 (attempt 2) · add a greeting")
        );
        assert!(name.chars().count() < 80, "{name}");
        let short = stage_agent_name(
            &item(),
            &plan(&"b".repeat(MAX_STAGE_CHARS), 1, 0, 1),
            None,
            "t",
        );
        assert!(
            !short.contains('…'),
            "an id within the limit is whole: {short}"
        );
    }

    #[test]
    fn names_are_deterministic_and_distinguish_items_with_the_same_title() {
        let stage = plan("implement", 1, 0, 1);
        let name = stage_agent_name(&item(), &stage, None, "same title");
        assert_eq!(name, stage_agent_name(&item(), &stage, None, "same title"));
        let other = QueueItemId::parse("qi_01ARZ3NDEKTSV4RRFFQ69G5FAW").unwrap();
        assert_eq!(
            stage_agent_name(&other, &stage, None, "same title"),
            "#q69g5faw implement · same title"
        );
        assert_ne!(name, stage_agent_name(&other, &stage, None, "same title"));
        assert_eq!(
            stage_agent_name(&item(), &stage, None, "a\n\tb\u{202e}\u{1b}"),
            "#q69g5fav implement · a b"
        );
    }

    #[test]
    #[should_panic(expected = "outside fan_out")]
    fn a_slot_beyond_the_fan_out_is_a_bug() {
        stage_agent_name(&item(), &plan("review", 1, 2, 2), None, "t");
    }
}
