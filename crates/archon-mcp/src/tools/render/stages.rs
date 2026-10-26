//! `get_stage` as text: the stage runs of one of an item's runs, or one
//! stage's output page by page. Output is never clipped; a page that is
//! not the last names the call that reads the next one, run included.

use std::fmt::Write as _;

use archon_core::protocol::{StageDetail, StageRunView, TextPage};
use archon_core::run::StageState;

use super::{LINE_CHARS, clip, snake, stage_ref, usd};

pub(super) fn detail(detail: &StageDetail) -> String {
    let mut text = format!("Item {} run {}", detail.item, detail.run);
    let Some(stage) = &detail.stage else {
        listing(&mut text, &detail.stages);
        return text;
    };
    assert!(
        detail.stages.iter().all(|s| &s.stage_id == stage),
        "only the stage asked for is shown"
    );
    let attempt = detail.stages.first().map_or(0, |s| s.attempt);
    let _ = write!(
        text,
        ": stage {stage} attempt {attempt}, {} stage run(s).",
        detail.stages.len()
    );
    for (view, retry) in detail.stages.iter().zip(retries(&detail.stages)) {
        let _ = write!(text, "\n\n## {}", view_line(view, retry));
        if let Some(output) = &view.output {
            text.push('\n');
            text.push_str(&output.text);
            text.push_str(&footer(detail, view, output));
        }
    }
    text
}

fn listing(text: &mut String, stages: &[StageRunView]) {
    assert!(
        stages.iter().all(|s| s.output.is_none()),
        "a listing carries no output"
    );
    if stages.is_empty() {
        text.push_str(": no stage has run yet.");
        return;
    }
    let _ = write!(
        text,
        ": {} stage run(s). Pass `stage` (and `attempt` or `slot` to narrow) for a stage's \
         full summary and structured result.",
        stages.len()
    );
    for (view, retry) in stages.iter().zip(retries(stages)) {
        let _ = write!(text, "\n- {}", view_line(view, retry));
    }
}

fn retries(stages: &[StageRunView]) -> Vec<u32> {
    archon_core::run::slot_retries(
        stages
            .iter()
            .map(|s| (s.stage_id.as_str(), s.attempt, s.slot)),
    )
}

/// Stage, attempt, slot, model, state, verdict, cost and agent; a failure's
/// error is clipped here, as the stage's output holds it whole.
fn view_line(view: &StageRunView, retry: u32) -> String {
    let state = match &view.state {
        StageState::Failed { error } => format!("failed: {}", clip(error, LINE_CHARS)),
        other => snake(other).replace('_', " "),
    };
    let mut line = format!(
        "{} attempt {} slot {}{}: {} {} [{state}]",
        view.stage_id,
        view.attempt,
        view.slot,
        archon_core::run::retry_label(retry),
        view.choice.model,
        view.choice.effort
    );
    if let Some(verdict) = view.verdict {
        let _ = write!(line, " verdict {}", snake(&verdict));
    }
    let _ = write!(line, ", cost {}", usd(view.cost_usd));
    if let Some(agent) = &view.aether_agent {
        let _ = write!(line, ", agent {agent}");
    }
    if let Some(delegation) = &view.delegation {
        let _ = write!(line, "; cost split: {}", delegation.line());
    }
    line
}

/// Nothing for a whole output; otherwise which characters this page holds
/// and, unless it is the last, the call for the next one. That call names
/// the run, so a retry starting between pages does not switch runs.
fn footer(detail: &StageDetail, view: &StageRunView, page: &TextPage) -> String {
    let end = page.offset + page.text.chars().count();
    assert!(
        end <= page.total_chars.max(page.offset),
        "a page lies within its text"
    );
    if page.offset == 0 && end == page.total_chars {
        return String::new();
    }
    if page.text.is_empty() {
        return format!(
            "\n[offset {} is past the end: {} characters in all]",
            page.offset, page.total_chars
        );
    }
    let span = format!(
        "\n[characters {}-{end} of {}",
        page.offset, page.total_chars
    );
    match page.next_offset() {
        None => format!("{span}: the end]"),
        Some(next) => format!(
            "{span}; the rest: {}, offset {next}]",
            stage_ref(
                &detail.item,
                Some(&detail.run),
                &view.stage_id,
                view.attempt,
                view.slot
            )
        ),
    }
}
