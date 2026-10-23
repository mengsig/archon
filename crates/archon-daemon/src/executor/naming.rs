//! Human-readable Aether agent names (ARCHITECTURE §4.5). Under a run node
//! the node's row carries the item and its title, so a stage agent is named
//! by its stage alone; without one the name leads with the item and ends
//! with the title.

use archon_core::id::QueueItemId;
use archon_pipeline::SpawnPlan;

use super::clip;

/// Titles are clipped to this many characters in agent names.
const MAX_TITLE_CHARS: usize = 40;
/// Stage ids (valid up to 64 characters) are clipped to this many, so a
/// long id cannot push the title out of a truncated name column.
const MAX_STAGE_CHARS: usize = 24;

/// Where a stage agent sits in Aether's tree.
pub(crate) enum Placement<'a> {
    /// Under the run node, which names the item and its title.
    UnderRunNode,
    /// A root agent, which has to name its item and title itself. Every
    /// stage Archon spawns today is bound to a node, so only tests build it.
    #[cfg_attr(not(test), allow(dead_code))]
    Standalone {
        item: &'a QueueItemId,
        title: &'a str,
    },
}

/// The stage, fan slot and attempt notes, led by the item and followed by
/// the title when the agent has no run node above it.
pub(crate) fn stage_agent_name(
    placement: Placement<'_>,
    plan: &SpawnPlan,
    retry: Option<usize>,
) -> String {
    assert!(plan.attempt >= 1, "attempts are counted from one");
    assert!(
        plan.fan_index < plan.fan_count,
        "slot {} outside fan_out {}",
        plan.fan_index,
        plan.fan_count
    );
    assert!(!plan.stage_id.is_empty(), "stages have ids");
    let stage = clip(&plan.stage_id, MAX_STAGE_CHARS);
    let mut name = match placement {
        Placement::UnderRunNode => stage,
        Placement::Standalone { item, .. } => {
            format!("#{} {stage}", crate::worktree::item_short(item))
        }
    };
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
    if let Placement::Standalone { title, .. } = placement {
        let title = short_title(title);
        if !title.is_empty() {
            name.push_str(" · ");
            name.push_str(&title);
        }
    }
    assert!(!name.is_empty(), "an agent name is never empty");
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
            kind: archon_core::catalog::StageKind::Work,
            attempt,
            fan_index,
            fan_count,
            choice: ModelChoice {
                model: "claude-code/claude-sonnet-5".parse().unwrap(),
                effort: Effort::Medium,
                tier: None,
            },
            prompt: "do it".into(),
            instructions: "contract".into(),
            timeout_minutes: 30,
            allow_delegation: false,
            workspace: archon_core::catalog::WorkspaceMode::Worktree,
            team: None,
            keep: false,
            resume: None,
            reviewers: None,
        }
    }

    fn named(plan: &SpawnPlan, retry: Option<usize>, title: &str) -> String {
        stage_agent_name(
            Placement::Standalone {
                item: &item(),
                title,
            },
            plan,
            retry,
        )
    }

    fn under_node(plan: &SpawnPlan, retry: Option<usize>) -> String {
        stage_agent_name(Placement::UnderRunNode, plan, retry)
    }

    #[test]
    fn a_standalone_agent_leads_with_the_item_and_ends_with_the_title() {
        let name = named(&plan("implement", 1, 0, 1), None, "add a greeting");
        assert_eq!(name, "#q69g5fav implement · add a greeting");
    }

    #[test]
    fn under_a_run_node_only_the_stage_and_its_slot_remain() {
        assert_eq!(under_node(&plan("implement", 1, 0, 1), None), "implement");
        assert_eq!(under_node(&plan("review", 1, 1, 3), None), "review 2/3");
        assert_eq!(
            under_node(&plan("implement", 3, 0, 1), Some(2)),
            "implement (attempt 3, retry 2)"
        );
        let name = under_node(&plan("review", 2, 0, 2), Some(1));
        assert!(!name.contains('#') && !name.contains('·'), "{name}");
    }

    #[test]
    fn fan_out_slots_count_from_one() {
        let name = named(&plan("review", 1, 1, 3), None, "add a greeting");
        assert_eq!(name, "#q69g5fav review 2/3 · add a greeting");
        let single = named(&plan("ship", 1, 0, 1), None, "add a greeting");
        assert!(!single.contains('/'), "{single}");
    }

    #[test]
    fn attempts_and_retries_show_only_when_they_happened() {
        let second = named(&plan("implement", 2, 0, 1), None, "t");
        assert_eq!(second, "#q69g5fav implement (attempt 2) · t");
        let retried = named(&plan("review", 1, 0, 2), Some(1), "t");
        assert_eq!(retried, "#q69g5fav review 1/2 · t");
        let both = named(&plan("implement", 3, 0, 1), Some(2), "t");
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
        assert_eq!(named(&plan("ship", 1, 0, 1), None, "   "), "#q69g5fav ship");
    }

    #[test]
    fn a_long_stage_id_is_clipped_in_both_placements() {
        let stage = "a".repeat(64);
        assert!(
            archon_pipeline::spec::valid_id(&stage),
            "the longest valid id"
        );
        let clipped = format!("{}…", "a".repeat(MAX_STAGE_CHARS - 1));
        let name = named(&plan(&stage, 2, 1, 2), Some(1), "add a greeting");
        assert_eq!(
            name,
            format!("#q69g5fav {clipped} 2/2 (attempt 2) · add a greeting")
        );
        assert_eq!(
            under_node(&plan(&stage, 2, 1, 2), Some(1)),
            format!("{clipped} 2/2 (attempt 2)")
        );
        let short = named(&plan(&"b".repeat(MAX_STAGE_CHARS), 1, 0, 1), None, "t");
        assert!(
            !short.contains('…'),
            "an id within the limit is whole: {short}"
        );
    }

    #[test]
    fn standalone_names_are_deterministic_and_distinguish_items_with_the_same_title() {
        let stage = plan("implement", 1, 0, 1);
        let name = named(&stage, None, "same title");
        assert_eq!(name, named(&stage, None, "same title"));
        let other = QueueItemId::parse("qi_01ARZ3NDEKTSV4RRFFQ69G5FAW").unwrap();
        let other_name = stage_agent_name(
            Placement::Standalone {
                item: &other,
                title: "same title",
            },
            &stage,
            None,
        );
        assert_eq!(other_name, "#q69g5faw implement · same title");
        assert_ne!(name, other_name);
        assert_eq!(
            named(&stage, None, "a\n\tb\u{202e}\u{1b}"),
            "#q69g5fav implement · a b"
        );
    }

    #[test]
    #[should_panic(expected = "outside fan_out")]
    fn a_slot_beyond_the_fan_out_is_a_bug() {
        named(&plan("review", 1, 2, 2), None, "t");
    }
}
