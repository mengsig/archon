//! The read-only host commands (`/status`, `/queue`, `/item`, `/questions`,
//! `/repos`, `/lessons`): archond answers each in a note in the TUI at once,
//! so the answer never waits for an orchestrator turn.

use archon_aether::wire::NoteLevel;
use archon_core::error::ArchonError;
use archon_core::lesson::{LessonQuery, LessonState};
use archon_core::protocol::{
    AetherLinkStatus, ClientKind, DaemonStatus, ItemQuery, Reply, Request,
};
use archon_core::queue::QueueState;

use crate::context::DaemonContext;
use crate::executor::clip;

/// Items, questions or lessons a note names before it points elsewhere.
const LISTED: usize = 5;
const TITLE_CHARS: usize = 60;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ReadCommand {
    Status,
    Queue(Vec<String>),
    Item(String),
    Questions,
    Repos,
    Lessons,
}

impl ReadCommand {
    /// The command a whole message is, with exactly the arguments it takes.
    pub(crate) fn parse(text: &str) -> Option<Self> {
        let mut words = text.split_whitespace();
        let name = words.next()?.to_ascii_lowercase();
        let args: Vec<String> = words.map(str::to_owned).collect();
        let command = match (name.as_str(), args.as_slice()) {
            ("/status", []) => Self::Status,
            ("/queue", _) => Self::Queue(args),
            ("/item", [id]) => Self::Item(id.clone()),
            ("/questions", []) => Self::Questions,
            ("/repos", []) => Self::Repos,
            ("/lessons", []) => Self::Lessons,
            _ => return None,
        };
        Some(command)
    }

    fn label(&self) -> &'static str {
        match self {
            Self::Status => "/status",
            Self::Queue(_) => "/queue",
            Self::Item(_) => "/item",
            Self::Questions => "/questions",
            Self::Repos => "/repos",
            Self::Lessons => "/lessons",
        }
    }
}

/// Answer `command` in a note. Where no note can be shown, the orchestrator
/// is told the answer instead, to restate it.
pub(crate) async fn answer(ctx: &DaemonContext, command: &ReadCommand) -> Result<(), ArchonError> {
    // A plain note answers; only a refusal is coloured.
    let (level, text) = match read(ctx, command).await {
        Ok(text) => (None, text),
        Err(error) if error.is_store() => return Err(error),
        Err(error) => (Some(NoteLevel::Refused), error.to_string()),
    };
    assert!(!text.trim().is_empty(), "every answer says something");
    if crate::command_notice::show_level(ctx, command.label(), level, &text).await {
        return Ok(());
    }
    let news = crate::triggers::TriggerEvent::ChatAnswer {
        number: None,
        applied: level.is_none(),
        text: format!("{}: {text}", command.label()),
    };
    crate::triggers::push(ctx, news).await
}

async fn read(ctx: &DaemonContext, command: &ReadCommand) -> Result<String, ArchonError> {
    match command {
        ReadCommand::Status => match ask(ctx, Request::Status(Default::default())).await? {
            Reply::Status(status) => Ok(status_line(&status)),
            other => unexpected(&other),
        },
        ReadCommand::Queue(states) => queue(ctx, states).await,
        ReadCommand::Item(prefix) => {
            let resolve = Request::ResolveItem {
                prefix: prefix.clone(),
            };
            match ask(ctx, resolve).await? {
                Reply::Item(item) => Ok(format!(
                    "{} · {} · {} · {}",
                    item.id,
                    item.state.kind(),
                    item.pipeline,
                    clip(&item.title, TITLE_CHARS)
                )),
                other => unexpected(&other),
            }
        }
        ReadCommand::Questions => questions(ctx).await,
        ReadCommand::Repos => match ask(ctx, Request::RepoList).await? {
            Reply::Repos(repos) if repos.is_empty() => {
                Ok("no repositories yet: add one with `archon repo add <path>`".into())
            }
            Reply::Repos(repos) => {
                let names: Vec<&str> = repos.iter().map(|r| r.name.as_str()).collect();
                Ok(format!(
                    "{} repositories: {}",
                    names.len(),
                    names.join(", ")
                ))
            }
            other => unexpected(&other),
        },
        ReadCommand::Lessons => lessons(ctx).await,
    }
}

/// The same read `archon` makes over the socket. Boxed: `route` reaches this
/// module again through operator commands sent on the socket.
async fn ask(ctx: &DaemonContext, request: Request) -> Result<Reply, ArchonError> {
    assert!(
        request.is_read_only(),
        "a read command only reads: {request:?}"
    );
    Box::pin(crate::handlers::route(ctx, ClientKind::ArchonCli, request)).await
}

fn unexpected(reply: &Reply) -> ! {
    unreachable!("each read answers with its own reply, not {reply:?}")
}

fn status_line(status: &DaemonStatus) -> String {
    let aether = match &status.aether {
        AetherLinkStatus::Connected { .. } => "Aether connected",
        AetherLinkStatus::Disconnected { .. } => "Aether not reachable",
    };
    let paused = match status.pause.global {
        true => " · dispatch paused",
        false => "",
    };
    format!(
        "archond {} · {aether} · {} running, {} queued, {} blocked · {} open question(s), \
         {} gate(s) · ${:.2} today, ${:.2} in all{paused}",
        status.daemon_version,
        status.active_runs,
        status.queued_items,
        status.blocked_items,
        status.open_questions,
        status.open_gates,
        status.spend_today_usd,
        status.spend_total_usd,
    )
}

async fn queue(ctx: &DaemonContext, states: &[String]) -> Result<String, ArchonError> {
    let states: Vec<String> = states.iter().map(|s| s.to_ascii_lowercase()).collect();
    if let Some(bad) = states
        .iter()
        .find(|s| !QueueState::KINDS.contains(&s.as_str()))
    {
        return Err(ArchonError::Invalid(format!(
            "unknown queue state {bad:?}; use one of {}",
            QueueState::KINDS.join(", ")
        )));
    }
    let query = ItemQuery {
        states,
        limit: Some(LISTED + 1),
        ..ItemQuery::default()
    };
    let rows = match ask(ctx, Request::ListItemSummaries(query)).await? {
        Reply::ItemSummaries(rows) => rows,
        other => unexpected(&other),
    };
    assert!(rows.len() <= LISTED + 1, "the query is limited");
    if rows.is_empty() {
        return Ok("no items".into());
    }
    let shown: Vec<String> = rows
        .iter()
        .take(LISTED)
        .map(|row| {
            let item = &row.item;
            let title = clip(&item.title, TITLE_CHARS);
            format!("{} {} {title}", item.id, item.state.kind())
        })
        .collect();
    Ok(listed(
        shown,
        rows.len() > LISTED,
        "`archon queue` lists them all",
    ))
}

async fn questions(ctx: &DaemonContext) -> Result<String, ArchonError> {
    let open = match ask(ctx, Request::ListQuestions { open_only: true }).await? {
        Reply::Questions(open) => open,
        other => unexpected(&other),
    };
    assert!(open.iter().all(|q| q.answer.is_none()), "only open ones");
    if open.is_empty() {
        return Ok("no open questions".into());
    }
    let shown: Vec<String> = open
        .iter()
        .take(LISTED)
        .map(|q| format!("Q{}: {}", q.number, clip(&q.text, TITLE_CHARS)))
        .collect();
    let text = listed(
        shown,
        open.len() > LISTED,
        "`archon questions` lists them all",
    );
    Ok(format!("{text}; answer with /answer N <text>"))
}

async fn lessons(ctx: &DaemonContext) -> Result<String, ArchonError> {
    let query = LessonQuery {
        state: Some(LessonState::Proposed),
        ..LessonQuery::default()
    };
    let proposed = match ask(ctx, Request::Lessons(query)).await? {
        Reply::Lessons(proposed) => proposed,
        other => unexpected(&other),
    };
    assert!(proposed.iter().all(|l| l.state == LessonState::Proposed));
    if proposed.is_empty() {
        return Ok("no proposed lessons".into());
    }
    let shown: Vec<String> = proposed
        .iter()
        .take(LISTED)
        .map(|l| format!("{}: {}", l.id, clip(&l.text, TITLE_CHARS)))
        .collect();
    let text = listed(
        shown,
        proposed.len() > LISTED,
        "`archon lessons` lists them all",
    );
    Ok(format!(
        "{text}; accept or retract one with `archon lessons accept|retract <id>`"
    ))
}

/// `shown` joined, and where the rest is when there is more.
fn listed(shown: Vec<String>, more: bool, rest: &str) -> String {
    assert!(!shown.is_empty(), "an empty list is said in words");
    let joined = shown.join("; ");
    match more {
        true => format!("{joined}; more: {rest}"),
        false => joined,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_whole_read_command_with_its_arguments_is_one() {
        assert_eq!(ReadCommand::parse("/status"), Some(ReadCommand::Status));
        assert_eq!(ReadCommand::parse(" /STATUS "), Some(ReadCommand::Status));
        assert_eq!(
            ReadCommand::parse("/queue running failed"),
            Some(ReadCommand::Queue(vec!["running".into(), "failed".into()]))
        );
        assert_eq!(
            ReadCommand::parse("/item qi_01"),
            Some(ReadCommand::Item("qi_01".into()))
        );
        for text in [
            "/status please",
            "/item",
            "/item a b",
            "/repos x",
            "what is the /status",
            "/statuses",
            "",
        ] {
            assert_eq!(ReadCommand::parse(text), None, "{text:?}");
        }
    }

    #[test]
    fn a_long_list_points_to_the_rest() {
        let shown = vec!["a".to_owned(), "b".to_owned()];
        assert_eq!(listed(shown.clone(), false, "x"), "a; b");
        assert_eq!(listed(shown, true, "x"), "a; b; more: x");
    }
}
