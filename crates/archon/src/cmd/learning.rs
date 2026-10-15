//! `archon learning`: correct one stage of a finished run, see what the
//! corrections of an item do, and undo them or their memory curation.
use super::{emit, to_json};
use archon_core::aether_ids::AetherMemoryId;
use archon_core::feedback::{
    CorrectionRequest, Credit, CurationState, FeedbackReceipt, FeedbackView, LearningWhy,
};
use archon_core::id::{FeedbackId, PipelineRunId, QueueItemId};
use archon_core::outcome::CorrectionCause;
use archon_core::protocol::{Reply, Request};
use clap::{Subcommand, ValueEnum};
use std::process::ExitCode;

#[derive(Debug, Subcommand)]
pub enum LearningCommand {
    /// Record that one stage of a finished run went wrong; optionally forget
    /// the one Aether memory that misled it.
    Correct {
        /// The qi_… item.
        item: String,
        /// The prun_… run of the item that produced the stage.
        #[arg(long)]
        run: String,
        /// The stage's id in that run, e.g. implement.
        #[arg(long)]
        stage: String,
        /// quality counts against the stage's model; the others only inform.
        #[arg(long, value_enum)]
        cause: Cause,
        /// What went wrong, at most 1000 bytes.
        #[arg(long)]
        reason: String,
        /// A mem_… Aether memory to forget, so fresh stage sessions stop recalling it.
        #[arg(long)]
        forget_memory: Option<String>,
    },
    /// Show the corrections of an item's runs and what each does now.
    Why {
        /// The qi_… item.
        item: String,
    },
    /// Revoke a correction's model penalty and memory feedback; a forgotten
    /// memory stays forgotten.
    Undo {
        /// The fb_… correction.
        feedback: String,
    },
    /// Restore the memory a correction forgot.
    RestoreMemory {
        /// The fb_… correction.
        feedback: String,
    },
    /// Retry pending memory feedback, retractions, or a failed memory forget.
    RetryMemory {
        /// The fb_… correction.
        feedback: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Cause {
    /// The stage's work was wrong: counts against its model.
    Quality,
    /// The task asked for the wrong thing.
    Scope,
    /// The environment failed, not the model.
    Infrastructure,
    /// The work stopped for its budget.
    Budget,
}

impl From<Cause> for CorrectionCause {
    fn from(cause: Cause) -> Self {
        match cause {
            Cause::Quality => Self::Quality,
            Cause::Scope => Self::Scope,
            Cause::Infrastructure => Self::Infrastructure,
            Cause::Budget => Self::Budget,
        }
    }
}

pub async fn run(command: LearningCommand, json: bool) -> anyhow::Result<ExitCode> {
    let request = request(command)?;
    let read_only = request.is_read_only();
    let client = if read_only {
        crate::connect::connect_running().await?
    } else {
        crate::connect::connect_or_start().await?
    };
    let (value, text, failed) = match client.request(request).await? {
        Reply::Feedback(receipt) => {
            let failed = curation_failed(&receipt.view);
            (to_json(&receipt)?, receipt_text(&receipt), failed)
        }
        Reply::LearningWhy(why) => (to_json(&why)?, why_text(&why), false),
        other => return Err(crate::connect::unexpected(&other, "feedback")),
    };
    emit(json, || Ok(value), &crate::wrap::sanitize(&text))?;
    Ok(if failed {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    })
}

fn request(command: LearningCommand) -> anyhow::Result<Request> {
    Ok(match command {
        LearningCommand::Correct {
            item,
            run,
            stage,
            cause,
            reason,
            forget_memory,
        } => Request::LearningCorrect(CorrectionRequest {
            item: QueueItemId::parse(&item)?,
            run: PipelineRunId::parse(&run)?,
            stage,
            cause: cause.into(),
            reason,
            forget_memory: forget_memory.map(AetherMemoryId::try_from).transpose()?,
        }),
        LearningCommand::Why { item } => Request::LearningWhy {
            item: QueueItemId::parse(&item)?,
        },
        LearningCommand::Undo { feedback } => Request::LearningUndo {
            feedback: FeedbackId::parse(&feedback)?,
        },
        LearningCommand::RestoreMemory { feedback } => Request::LearningRestoreMemory {
            feedback: FeedbackId::parse(&feedback)?,
        },
        LearningCommand::RetryMemory { feedback } => Request::LearningRetryMemory {
            feedback: FeedbackId::parse(&feedback)?,
        },
    })
}

/// A requested forget that Aether has not applied, or that was restored.
fn curation_failed(view: &FeedbackView) -> bool {
    view.feedback
        .curation
        .as_ref()
        .is_some_and(|c| matches!(c.state, CurationState::Pending | CurationState::Failed))
}

fn receipt_text(receipt: &FeedbackReceipt) -> String {
    let view = &receipt.view;
    let feedback = &view.feedback;
    let mut lines = Vec::new();
    if receipt.replayed {
        lines.push(format!(
            "{} was already recorded; nothing new was.",
            feedback.id
        ));
    }
    if let Some(at) = feedback.revoked_at {
        lines.push(format!("Reward revoked at {at}."));
    } else {
        lines.push(recorded_line(view));
        lines.extend(standing_line(view));
        lines.push("Review requirement unchanged. Applies to newly chosen defaults.".into());
    }
    lines.extend(memory_lines(view));
    lines.extend(feedback_lines(view));
    lines.join("\n")
}

fn recorded_line(view: &FeedbackView) -> String {
    let feedback = &view.feedback;
    match (&feedback.attribution.credit, feedback.penalized()) {
        (_, Some((_, choice))) => format!(
            "Recorded {} for {} / {} / {} ({}).",
            feedback.id, feedback.run, feedback.stage, choice.model, choice.effort
        ),
        (Credit::Model { choice, .. }, None) => format!(
            "Recorded {} for {} / {} / {} ({}); a {} correction is informational: no model \
             penalty.",
            feedback.id,
            feedback.run,
            feedback.stage,
            choice.model,
            choice.effort,
            feedback.cause.as_str()
        ),
        (Credit::Unavailable { why }, None) => format!(
            "Recorded observation {} for {} / {}; no model penalty because {why}.",
            feedback.id, feedback.run, feedback.stage
        ),
    }
}

/// The penalized choice's standing now, and what its cohort selects.
fn standing_line(view: &FeedbackView) -> Option<String> {
    let corrected = view.corrected.as_ref()?;
    let mut line = format!(
        "This choice has {}/{} successful stage results{}.",
        corrected.passed,
        corrected.runs,
        if view.penalizing {
            " after explicit quality feedback"
        } else {
            "; this correction no longer counts"
        }
    );
    match &view.selected {
        Some(selected) if selected.choice != corrected.choice => line.push_str(&format!(
            " New defaults select {} ({}) from {}/{} results.",
            selected.choice.model, selected.choice.effort, selected.passed, selected.runs
        )),
        Some(_) => line.push_str(" New defaults still select it."),
        None => line.push_str(" New defaults keep the static fallback."),
    }
    Some(line)
}

fn memory_lines(view: &FeedbackView) -> Vec<String> {
    let Some(curation) = &view.feedback.curation else {
        return Vec::new();
    };
    let id = &view.feedback.id;
    let instance = &curation.instance_id;
    let mut lines = vec![match curation.state {
        CurationState::Applied => format!(
            "Memory {} forgotten in Aether instance {instance}; applies to fresh stage \
             sessions. Undo keeps it forgotten: use `archon learning restore-memory {id}`.",
            curation.memory
        ),
        CurationState::Restored => format!(
            "Memory {} restored in Aether instance {instance}.",
            curation.memory
        ),
        CurationState::Pending | CurationState::Failed => format!(
            "Correction recorded; memory forget {}: {}. Retry with `archon learning \
             retry-memory {id}`.",
            curation.state.as_str(),
            curation.error.as_deref().unwrap_or("not sent yet")
        ),
    }];
    lines.extend(memory_state(view));
    lines
}

/// The memory as Aether holds it now: who wrote it, when, where it applies.
fn memory_state(view: &FeedbackView) -> Option<String> {
    if let Some(error) = &view.memory_error {
        return Some(format!("Memory state unavailable: {error}"));
    }
    let memory = view.memory.as_ref()?;
    let state = match (&memory.superseded_by, memory.forgotten_at) {
        (Some(newer), _) => format!("superseded by {newer}"),
        (None, Some(at)) => format!("forgotten {at}"),
        (None, None) => "current".into(),
    };
    Some(format!(
        "Memory {}: {}, {} {}, {}{}; {state}.",
        memory.id,
        memory.origin,
        memory.kind,
        memory.created_at.format("%Y-%m-%d"),
        memory.scope,
        if memory.pinned { ", pinned" } else { "" }
    ))
}

fn feedback_lines(view: &FeedbackView) -> Vec<String> {
    let Some(ledger) = &view.feedback.memory_feedback else {
        return Vec::new();
    };
    let counts = ledger.counts();
    assert_eq!(counts, view.memory_feedback, "text and JSON share counts");
    assert!(ledger.check().is_ok());
    let mut lines = vec![format!(
        "Memory feedback: {} sent, {} pending, {} skipped, {} retracted; {} partial tasks.",
        counts.sent, counts.pending, counts.skipped, counts.retracted, counts.partial_tasks
    )];
    lines.extend(
        ledger
            .skipped
            .iter()
            .map(|s| format!("Memory feedback skipped: {s}.")),
    );
    for task in &ledger.tasks {
        if let Some(note) = task.skipped.as_ref().or(task.error.as_ref()) {
            lines.push(format!("Memory feedback {}: {note}.", task.task));
        }
        for entry in &task.records {
            if let Some(reason) = &entry.failed {
                lines.push(format!(
                    "Memory feedback {} / {} skipped: {reason}.",
                    entry.memory, task.task
                ));
            }
            if let Some(error) = &entry.error {
                lines.push(format!(
                    "Memory feedback {} / {} pending: {error}.",
                    entry.memory, task.task
                ));
            }
        }
    }
    if counts.pending > 0 {
        lines.push(format!(
            "Retry with `archon learning retry-memory {}`.",
            view.feedback.id
        ));
    }
    lines
}

fn why_text(why: &LearningWhy) -> String {
    if why.feedback.is_empty() {
        return format!("No operator corrections for item {}.", why.item);
    }
    let mut lines = vec![format!("Item {}", why.item)];
    for view in &why.feedback {
        let feedback = &view.feedback;
        let status = match (feedback.revoked_at, view.penalizing) {
            (Some(_), _) => "revoked",
            (None, true) => "active",
            (None, false) if feedback.penalized().is_some() => "expired",
            (None, false) => "informational",
        };
        lines.push(format!(
            "{} {} / {} {} ({status}): {}",
            feedback.id,
            feedback.run,
            feedback.stage,
            feedback.cause.as_str(),
            feedback.reason
        ));
        lines.push(format!("  {}", recorded_line(view)));
        lines.extend(standing_line(view).map(|l| format!("  {l}")));
        if let Some(frozen) = &view.frozen_explanation {
            lines.push(format!("  When chosen: {frozen}"));
        }
        lines.extend(memory_lines(view).into_iter().map(|l| format!("  {l}")));
        lines.extend(feedback_lines(view).into_iter().map(|l| format!("  {l}")));
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use clap::Parser as _;

    use super::*;
    use crate::cli::{Cli, Command};
    use archon_core::feedback::{Attribution, MemoryCuration, OperatorFeedback};

    #[test]
    fn a_correction_parses_into_its_request() {
        let cli = Cli::try_parse_from([
            "archon",
            "learning",
            "correct",
            "qi_01J00000000000000000000000",
            "--run",
            "prun_01J00000000000000000000000",
            "--stage",
            "implement",
            "--cause",
            "quality",
            "--reason",
            "ignored the identity rule",
            "--forget-memory",
            "mem_01J00000000000000000000000",
        ])
        .unwrap();
        let Some(Command::Learning(command)) = cli.command else {
            panic!("learning")
        };
        let Request::LearningCorrect(request) = request(command).unwrap() else {
            panic!("correct")
        };
        assert_eq!(request.cause, CorrectionCause::Quality);
        assert_eq!(request.stage, "implement");
        assert!(request.forget_memory.is_some());
        let bad =
            Cli::try_parse_from(["archon", "learning", "correct", "qi_1", "--cause", "blame"]);
        assert!(bad.is_err(), "an unknown cause is refused");
    }

    fn view(credit: Credit, curation: Option<MemoryCuration>) -> FeedbackView {
        let now = archon_core::now();
        let feedback = OperatorFeedback {
            id: FeedbackId::new(),
            request_key: "k".into(),
            item: QueueItemId::new(),
            run: PipelineRunId::new(),
            stage: "implement".into(),
            cause: CorrectionCause::Quality,
            reason: "ignored the identity rule".into(),
            attribution: Attribution {
                repo: None,
                pipeline: "direct".into(),
                context: None,
                delivery_commit: None,
                credit,
            },
            observed_at: now,
            revoked_at: None,
            curation,
            memory_feedback: None,
        };
        FeedbackView {
            memory_feedback: Default::default(),
            feedback,
            penalizing: false,
            corrected: None,
            selected: None,
            frozen_explanation: None,
            memory: None,
            memory_error: None,
        }
    }

    #[test]
    fn a_forget_that_did_not_apply_fails_the_command_and_says_so() {
        let failed = MemoryCuration {
            memory: AetherMemoryId::parse("mem_01J00000000000000000000000").unwrap(),
            instance_id: "aether-instance-1".into(),
            state: CurationState::Failed,
            before: None,
            forgotten_at: None,
            restored_at: None,
            error: Some("not connected to aetherd".into()),
            updated_at: archon_core::now(),
        };
        let why = "the stage ran several agents at once".to_owned();
        let failing = view(Credit::Unavailable { why }, Some(failed));
        assert!(curation_failed(&failing));
        let text = receipt_text(&FeedbackReceipt {
            view: failing,
            replayed: false,
        });
        assert!(text.contains("no model penalty because the stage ran several agents"));
        assert!(text.contains("Correction recorded; memory forget failed: not connected"));
        assert!(text.contains("retry-memory fb_"), "{text}");
        let why = "the run has not finished".to_owned();
        let plain = view(Credit::Unavailable { why }, None);
        assert!(!curation_failed(&plain), "no forget asked for, none failed");
    }

    #[test]
    fn memory_feedback_text_shows_counts_partial_context_and_retry_errors() {
        use archon_core::aether_ids::AetherTaskId;
        use archon_core::feedback::{MemoryFeedback, MemoryFeedbackTask};

        let mut view = view(
            Credit::Unavailable {
                why: "no model credit".into(),
            },
            None,
        );
        let ledger = MemoryFeedback {
            instance_id: "instance".into(),
            skipped: None,
            tasks: vec![MemoryFeedbackTask {
                task: AetherTaskId::parse("tsk_1").unwrap(),
                discovered: false,
                partial: false,
                skipped: None,
                error: Some("context unavailable".into()),
                records: Vec::new(),
            }],
        };
        view.memory_feedback = ledger.counts();
        view.feedback.memory_feedback = Some(ledger);
        let lines = feedback_lines(&view).join("\n");
        assert!(lines.contains("0 sent, 1 pending, 0 skipped, 0 retracted; 0 partial tasks"));
        assert!(lines.contains("tsk_1: context unavailable"));
        assert!(lines.contains("archon learning retry-memory fb_"));
        let why = why_text(&LearningWhy {
            item: view.feedback.item.clone(),
            feedback: vec![view.clone()],
        });
        assert!(why.contains(lines.lines().next().unwrap()));
        let ledger = view.feedback.memory_feedback.as_mut().unwrap();
        ledger.tasks[0].discovered = true;
        ledger.tasks[0].partial = true;
        ledger.tasks[0].error = None;
        ledger.tasks[0].skipped = Some("named memory was not listed".into());
        view.memory_feedback = ledger.counts();
        let lines = feedback_lines(&view).join("\n");
        assert!(lines.contains("0 sent, 0 pending, 1 skipped"));
        assert!(lines.contains("1 partial tasks"));
        assert!(!lines.contains("retry-memory"));
    }
    #[test]
    fn terminal_feedback_shows_its_reason_without_pending_or_retry_advice() {
        use archon_core::aether_ids::AetherTaskId;
        use archon_core::feedback::{MemoryFeedback, MemoryFeedbackTask};
        let mut view = view(
            Credit::Unavailable {
                why: "informational".into(),
            },
            None,
        );
        let ledger = MemoryFeedback {
            instance_id: "instance".into(),
            skipped: None,
            tasks: vec![MemoryFeedbackTask {
                task: AetherTaskId::parse("tsk_1").unwrap(),
                discovered: false,
                partial: false,
                skipped: Some("aether has no such task".into()),
                error: None,
                records: Vec::new(),
            }],
        };
        view.memory_feedback = ledger.counts();
        view.feedback.memory_feedback = Some(ledger);
        let text = why_text(&LearningWhy {
            item: view.feedback.item.clone(),
            feedback: vec![view],
        });
        assert!(text.contains("0 pending, 1 skipped"));
        assert!(text.contains("aether has no such task"));
        assert!(!text.contains("Retry with"));
    }
}
