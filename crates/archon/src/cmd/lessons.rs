//! Inspect and edit durable learned advice.
use super::{emit, to_json};
use archon_core::id::{LessonId, QueueItemId};
use archon_core::lesson::{LessonDraft, LessonKind, LessonPatch, LessonQuery, LessonState};
use archon_core::protocol::{Reply, RepoRef, Request};
use clap::{Args, Subcommand};
use std::process::ExitCode;

#[derive(Debug, Args)]
pub struct LessonsArgs {
    /// Repository name or repo_… ID; global lessons are included.
    #[arg(long, global = true)]
    pub repo: Option<String>,
    /// Include proposed and retracted lessons.
    #[arg(long)]
    pub all: bool,
    #[command(subcommand)]
    pub command: Option<LessonCommand>,
}

#[derive(Debug, Subcommand)]
pub enum LessonCommand {
    /// Record one short practice, pitfall, routing rule or pipeline observation.
    Add {
        /// routing, practice, pitfall or pipeline.
        #[arg(long, default_value = "practice")]
        kind: String,
        /// Restrict this lesson to one pipeline.
        #[arg(long)]
        pipeline: Option<String>,
        /// The one-line lesson, at most 300 characters.
        text: Vec<String>,
    },
    /// Replace a lesson with the operator's wording.
    Edit {
        /// The lsn_… lesson ID.
        lesson: String,
        /// The one-line lesson, at most 300 characters.
        text: Vec<String>,
    },
    /// Prevent automatic decay and retraction.
    Pin {
        /// The lsn_… lesson ID.
        lesson: String,
        /// Restore automatic decay and retraction.
        #[arg(long)]
        unpin: bool,
    },
    /// Stop giving a lesson to new runs.
    Retract {
        /// The lsn_… lesson ID.
        lesson: String,
    },
    /// Activate a proposed or retracted lesson.
    Accept {
        /// The lsn_… lesson ID.
        lesson: String,
    },
    /// Explain which advice and learned defaults shaped an item.
    Why {
        /// The qi_… item ID.
        item: String,
    },
    /// Summarize outcome, correction and routing evidence.
    Stats,
}

pub async fn run(args: LessonsArgs, json: bool) -> anyhow::Result<ExitCode> {
    let request = request(args)?;
    let client = crate::connect::connect().await?;
    let reply = client.request(request).await?;
    let (value, text) = match reply {
        Reply::Lesson(lesson) => (to_json(&lesson)?, line(&lesson)),
        Reply::Lessons(lessons) => (
            to_json(&lessons)?,
            lessons.iter().map(line).collect::<Vec<_>>().join("\n"),
        ),
        Reply::LessonsWhy(why) => {
            let text = format!(
                "Item {} run {}\n{}\n{}",
                why.item,
                why.run
                    .as_ref()
                    .map(ToString::to_string)
                    .unwrap_or_else(|| "not started".into()),
                why.lessons.iter().map(line).collect::<Vec<_>>().join("\n"),
                why.routing
                    .iter()
                    .map(|(stage, why)| format!("{stage}: {why}"))
                    .collect::<Vec<_>>()
                    .join("\n")
            );
            (to_json(&why)?, text)
        }
        Reply::LessonStats(stats) => (to_json(&stats)?, stats_text(&stats)),
        other => return Err(crate::connect::unexpected(&other, "lessons")),
    };
    emit(json, || Ok(value), &crate::wrap::sanitize(&text))?;
    Ok(ExitCode::SUCCESS)
}

fn stats_text(stats: &archon_core::lesson::LessonStats) -> String {
    assert!(stats.corrected <= stats.outcomes);
    assert!(stats.merged <= stats.outcomes);
    let mut text = format!(
        "{} runs, {} corrected; lessons: {} active, {} proposed, {} retracted\n",
        stats.outcomes, stats.corrected, stats.active, stats.proposed, stats.retracted
    );
    for p in &stats.pipelines {
        text.push_str(&format!(
            "{} / {} n={} pass={:.0}% median=${:.3} wall={:.0}s corrections={:.0}% saving/run={}\n",
            p.repo.as_ref().map_or("global", |repo| repo.as_str()),
            p.pipeline,
            p.runs,
            p.pass_rate * 100.0,
            p.median_cost_usd,
            p.median_wall_secs,
            p.correction_rate * 100.0,
            p.estimated_saving_usd
                .map(|s| format!("${s:.3}"))
                .unwrap_or_else(|| "unknown baseline".into())
        ));
    }
    for r in &stats.routes {
        text.push_str(&format!(
            "{} / {} / {}: {} {} n={} passed={}\n",
            r.repo.as_ref().map_or("global", |repo| repo.as_str()),
            r.pipeline,
            r.stage,
            r.choice.model,
            r.choice.effort,
            r.runs,
            r.passed
        ));
    }
    text
}

fn request(args: LessonsArgs) -> anyhow::Result<Request> {
    let repo = args.repo.map(RepoRef);
    Ok(match args.command {
        None => Request::Lessons(LessonQuery {
            repo,
            state: (!args.all).then_some(LessonState::Active),
            kind: None,
        }),
        Some(LessonCommand::Add {
            kind,
            pipeline,
            text,
        }) => Request::RecordLesson(LessonDraft {
            kind: parse_kind(&kind)?,
            text: text.join(" "),
            repo,
            pipeline,
            item: None,
        }),
        Some(LessonCommand::Why { item }) => Request::LessonsWhy {
            item: QueueItemId::parse(&item)?,
        },
        Some(LessonCommand::Stats) => Request::LessonStats { repo },
        Some(command) => {
            let (id, patch) = patch(command);
            Request::UpdateLesson {
                lesson: LessonId::parse(&id)?,
                patch,
            }
        }
    })
}

fn patch(command: LessonCommand) -> (String, LessonPatch) {
    match command {
        LessonCommand::Edit { lesson, text } => (
            lesson,
            LessonPatch {
                text: Some(text.join(" ")),
                ..Default::default()
            },
        ),
        LessonCommand::Pin { lesson, unpin } => (
            lesson,
            LessonPatch {
                pinned: Some(!unpin),
                ..Default::default()
            },
        ),
        LessonCommand::Retract { lesson } => (
            lesson,
            LessonPatch {
                state: Some(LessonState::Retracted),
                ..Default::default()
            },
        ),
        LessonCommand::Accept { lesson } => (
            lesson,
            LessonPatch {
                state: Some(LessonState::Active),
                ..Default::default()
            },
        ),
        _ => unreachable!("list/add/why/stats handled above"),
    }
}

fn parse_kind(kind: &str) -> anyhow::Result<LessonKind> {
    match kind {
        "routing" => Ok(LessonKind::Routing),
        "practice" => Ok(LessonKind::Practice),
        "pitfall" => Ok(LessonKind::Pitfall),
        "pipeline" => Ok(LessonKind::Pipeline),
        _ => Err(anyhow::anyhow!(
            "lesson kind is routing, practice, pitfall or pipeline"
        )),
    }
}

fn line(l: &archon_core::lesson::Lesson) -> String {
    format!(
        "{} [{}{}] {} confidence={:.2} wins={}/{}: {}",
        l.id,
        l.state.as_str(),
        if l.pinned { ", pinned" } else { "" },
        l.kind.as_str(),
        l.confidence,
        l.wins,
        l.uses,
        crate::wrap::sanitize_line(&l.text)
    )
}
