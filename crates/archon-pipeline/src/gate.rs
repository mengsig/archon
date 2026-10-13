//! Deciding one finished stage group (ARCHITECTURE §6.4 step 2): did the
//! gate pass, and if not, what feedback goes back to the looped-to stage.

use archon_core::catalog::{Gate, StageKind};
use archon_core::run::{StageRun, StageState, Verdict};

use crate::review::Evidence;
use crate::spec::StageSpec;

/// Feedback handed to a looped-to stage is capped so a verbose reviewer
/// cannot blow up the next agent's objective.
pub const MAX_FEEDBACK_CHARS: usize = 32_000;

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum GroupResult {
    /// Summary of the group: the completed members' summaries joined.
    Pass { summary: String },
    /// `headline` is one line; `report` details every failing member.
    Fail { headline: String, report: String },
}

/// Evaluate a stage group whose every slot is terminal. `members` holds the
/// latest stage run per slot, by fan index; `holds_review` as for `reviewed`.
pub(crate) fn evaluate(
    stage: &StageSpec,
    attempt: u32,
    members: &[&StageRun],
    holds_review: bool,
) -> GroupResult {
    assert_eq!(members.len(), stage.fan_out as usize, "one member per slot");
    assert!(
        members.iter().all(|m| m.state.is_terminal()),
        "only finished groups are evaluated"
    );
    let total = members.len();
    let passes = members.iter().filter(|m| member_passes(stage, m)).count();
    let ok = match stage.gate {
        Gate::None | Gate::AllPass => passes == total,
        Gate::Majority => passes * 2 > total,
        Gate::Any => passes >= 1,
    };
    if ok && holds_review {
        return reviewed(stage, attempt, members);
    }
    if ok {
        return GroupResult::Pass {
            summary: joined_summary(stage, members),
        };
    }
    let headline = match stage.gate {
        Gate::None if stage.kind == StageKind::Team => team_headline(stage, members),
        Gate::None => format!("{} of {total} agent(s) did not complete", total - passes),
        gate => format!(
            "gate {} failed: {passes} of {total} passed",
            gate_name(gate)
        ),
    };
    // Bold labels, not headings: feedback is embedded at any heading depth.
    let mut report = format!("**Stage `{}` attempt {attempt}: {headline}**\n", stage.id);
    for member in members.iter().filter(|m| !member_passes(stage, m)) {
        report.push('\n');
        report.push_str(&member_report(stage, member));
    }
    GroupResult::Fail {
        headline,
        report: clip(&report, MAX_FEEDBACK_CHARS),
    }
}

/// A completed stage that holds its pipeline's review passes only with the
/// review on record; its summary then ends with Archon's line saying so.
fn reviewed(stage: &StageSpec, attempt: u32, members: &[&StageRun]) -> GroupResult {
    let [member] = members else {
        panic!("validated: one agent holds a pipeline's review")
    };
    assert_eq!(member.state, StageState::Completed, "it passed its gate");
    let summary = joined_summary(stage, members);
    match Evidence::read(member.structured.as_ref()) {
        Ok(evidence) => GroupResult::Pass {
            summary: format!("{summary}\n\n{}", evidence.line()),
        },
        Err(why) => {
            let headline = format!("the review this pipeline declares is not on record: {why}");
            let report = format!(
                "**Stage `{}` attempt {attempt}: {headline}**\n\n{summary}\n",
                stage.id
            );
            GroupResult::Fail {
                headline,
                report: clip(&report, MAX_FEEDBACK_CHARS),
            }
        }
    }
}

/// Which roles of a team group did not complete, and how.
fn team_headline(stage: &StageSpec, members: &[&StageRun]) -> String {
    let outcomes: Vec<String> = members
        .iter()
        .filter(|m| m.state != StageState::Completed)
        .map(|m| role_outcome(stage, m))
        .collect();
    assert!(
        !outcomes.is_empty(),
        "a failed team group has a failed role"
    );
    outcomes.join(", ")
}

/// How one team role ended, e.g. "role `api` failed".
pub(crate) fn role_outcome(stage: &StageSpec, member: &StageRun) -> String {
    let role = stage
        .roles
        .get(member.fan_index as usize)
        .expect("validated: one role per team slot");
    let how = match &member.state {
        StageState::Completed => "completed",
        StageState::Failed { .. } => "failed",
        StageState::TimedOut => "timed out",
        StageState::Cancelled => "was cancelled",
        StageState::Spawning | StageState::Running => "is still running",
    };
    format!("role `{role}` {how}")
}

/// A slot passes when its agent completed and, for gated stages, its
/// verdict is `pass`. `unknown` counts as fail.
fn member_passes(stage: &StageSpec, member: &StageRun) -> bool {
    if member.state != StageState::Completed {
        return false;
    }
    stage.gate == Gate::None || verdict_of(member) == Verdict::Pass
}

/// The persisted verdict, else the one [`Verdict::extract`] reads from the
/// summary and structured result.
pub(crate) fn verdict_of(member: &StageRun) -> Verdict {
    member.verdict.unwrap_or_else(|| {
        Verdict::extract(
            member.summary.as_deref().unwrap_or_default(),
            member.structured.as_ref(),
        )
    })
}

pub(crate) fn gate_name(gate: Gate) -> &'static str {
    match gate {
        Gate::None => "none",
        Gate::AllPass => "all_pass",
        Gate::Majority => "majority",
        Gate::Any => "any",
    }
}

/// How a slot is named in summaries: its role on a team stage, else `#n`.
fn slot_label(stage: &StageSpec, fan_index: u32) -> String {
    match stage.roles.get(fan_index as usize) {
        Some(role) => format!("role {role}"),
        None => format!("#{}", fan_index + 1),
    }
}

/// Completed members' summaries; fan-out slots are labelled `[#n]`, team
/// slots `[role <name>]`.
pub(crate) fn joined_summary(stage: &StageSpec, members: &[&StageRun]) -> String {
    let completed: Vec<&StageRun> = members
        .iter()
        .copied()
        .filter(|m| m.state == StageState::Completed)
        .collect();
    let text = |m: &StageRun| m.summary.as_deref().unwrap_or_default().trim().to_owned();
    if members.len() == 1 {
        return completed.first().map(|m| text(m)).unwrap_or_default();
    }
    completed
        .iter()
        .map(|m| format!("[{}] {}", slot_label(stage, m.fan_index), text(m)))
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn member_report(stage: &StageSpec, member: &StageRun) -> String {
    let label = format!("**`{}` {}", stage.id, slot_label(stage, member.fan_index));
    let summary = member.summary.as_deref().unwrap_or_default().trim();
    match &member.state {
        StageState::Completed => {
            let verdict = match verdict_of(member) {
                Verdict::Fail => "fail",
                _ => "missing or unreadable (counts as fail)",
            };
            let findings = findings_text(member.structured.as_ref());
            format!("{label}: verdict {verdict}**\n\n{summary}\n{findings}")
        }
        StageState::Failed { error } => format!("{label}: failed**\n\n{}\n", error.trim()),
        StageState::TimedOut => format!(
            "{label}: timed out after {} minutes**\n",
            stage.timeout_minutes
        ),
        StageState::Cancelled if summary.is_empty() => {
            format!("{label}: cancelled before it finished**\n")
        }
        StageState::Cancelled => format!("{label}: cancelled before it finished**\n\n{summary}\n"),
        StageState::Spawning | StageState::Running => {
            unreachable!("evaluate() asserts every member is terminal")
        }
    }
}

/// `structured.findings` as a Markdown list; tolerant of missing fields
/// because agents write it by hand.
fn findings_text(structured: Option<&serde_json::Value>) -> String {
    let Some(findings) = structured
        .and_then(|s| s.get("findings"))
        .and_then(|f| f.as_array())
        .filter(|f| !f.is_empty())
    else {
        return String::new();
    };
    let field = |f: &serde_json::Value, key: &str| {
        f.get(key)
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .trim()
            .to_owned()
    };
    let mut out = String::from("\nFindings:\n");
    for finding in findings {
        let severity = field(finding, "severity");
        let path = field(finding, "path");
        let mut line = format!("- [{}] {}", or_dash(&severity), field(finding, "title"));
        if !path.is_empty() {
            line.push_str(&format!(" ({path})"));
        }
        out.push_str(&line);
        out.push('\n');
        let detail = field(finding, "detail");
        if !detail.is_empty() {
            out.push_str(&format!("  {}\n", detail.replace('\n', "\n  ")));
        }
    }
    out
}

fn or_dash(text: &str) -> &str {
    if text.is_empty() { "-" } else { text }
}

/// At most `max` characters, cut on a char boundary with a visible marker.
pub(crate) fn clip(text: &str, max: usize) -> String {
    match text.char_indices().nth(max) {
        None => text.to_owned(),
        Some((cut, _)) => format!("{}\n\n[… truncated by Archon]", &text[..cut]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clip_keeps_short_text_and_cuts_on_char_boundaries() {
        assert_eq!(clip("abc", 3), "abc");
        let cut = clip("ééé", 2);
        assert!(cut.starts_with("éé\n"), "{cut}");
        assert!(cut.ends_with("[… truncated by Archon]"));
    }

    #[test]
    fn findings_render_with_missing_fields() {
        let structured = serde_json::json!({"findings": [
            {"severity": "high", "title": "off by one", "path": "src/a.rs:3", "detail": "loop\nends early"},
            {"title": "nit"}
        ]});
        let text = findings_text(Some(&structured));
        assert!(text.contains("- [high] off by one (src/a.rs:3)"), "{text}");
        assert!(text.contains("  loop\n  ends early"), "{text}");
        assert!(text.contains("- [-] nit"), "{text}");
        assert_eq!(findings_text(None), "");
    }
}
