//! One function per `Request` (ARCHITECTURE §10, §11). The
//! operator-channel requests call into `supervisor`, `feed` and `reports`.

mod autonomy;
mod control;
mod cost;
mod initiatives;
mod item_views;
mod items;
mod learning;

pub use learning::memory_feedback::{
    resume as resume_memory_feedback, run as deliver_memory_feedback,
};
mod lessons;
mod model_evidence;
mod operator;
mod repos;
mod stages;
mod steer;
mod waiting;

use archon_core::aether_ids::AetherAgentId;
use archon_core::error::ArchonError;
use archon_core::id::QuestionId;
use archon_core::notify::{AnswerVia, Question};
use archon_core::protocol::{ClientKind, Reply, Request};

use crate::context::DaemonContext;
use crate::{github, reports, standby, supervisor};

/// Validate and execute one request. Operator-only requests are refused
/// with `ErrorCode::Forbidden` over MCP and for any agent, whatever the
/// client; a request that changes Archon for an agent is made only for the
/// orchestrator archond has now (ARCHITECTURE §11). In standby only reads
/// are served.
pub async fn handle(
    ctx: &DaemonContext,
    client: ClientKind,
    request: Request,
) -> Result<Reply, ArchonError> {
    let (caller, request) = match request {
        Request::ForAgent { caller, request } => (Some(caller), *request),
        request => (None, request),
    };
    if matches!(request, Request::ForAgent { .. }) {
        return Err(ArchonError::Invalid(
            "for_agent carries one request, not another for_agent".into(),
        ));
    }
    if for_an_agent(client, caller.as_ref()) && request.is_operator_only() {
        return Err(ArchonError::Forbidden(format!(
            "{} is reserved for the operator's archon CLI",
            command_name(&request)
        )));
    }
    if let Some(refusal) = standby::refusal(ctx, &request) {
        return Err(refusal);
    }
    check_named_caller(caller.as_ref(), &request)?;
    // Held to the end of the call: the orchestrator is not replaced under it.
    let _authority = if orchestrators_alone(client, caller.as_ref(), &request) {
        Some(supervisor::authorize(ctx, caller.as_ref()).await?)
    } else {
        supervisor::saw_call(ctx, caller.as_ref()).await?;
        None
    };
    route(ctx, client, request).await
}

/// Whether only the orchestrator may make this call: any change to Archon for
/// an agent but a stage agent's message, which checks its own caller (§5.5).
fn orchestrators_alone(
    client: ClientKind,
    caller: Option<&AetherAgentId>,
    request: &Request,
) -> bool {
    let stage_message = matches!(
        request,
        Request::MessageOrchestrator { .. } | Request::PublishArtifact { .. }
    );
    for_an_agent(client, caller) && !request.is_read_only() && !stage_message
}

/// Whether a request is made for an agent: over MCP, or forwarded for one
/// by any client.
fn for_an_agent(client: ClientKind, caller: Option<&AetherAgentId>) -> bool {
    client == ClientKind::ArchonMcp || caller.is_some()
}

/// A request that names its caller itself names the agent it is forwarded for.
fn check_named_caller(
    forwarded_for: Option<&AetherAgentId>,
    request: &Request,
) -> Result<(), ArchonError> {
    let named = match request {
        Request::MessageOrchestrator { caller, .. }
        | Request::PublishArtifact { caller, .. }
        | Request::AskOperator { caller, .. }
        | Request::RecordChatAnswer { caller, .. } => caller.as_ref(),
        _ => None,
    };
    match (forwarded_for, named) {
        (Some(outer), Some(inner)) if outer != inner => Err(ArchonError::Invalid(format!(
            "{} names caller {inner} but was forwarded for {outer}",
            command_name(request)
        ))),
        _ => Ok(()),
    }
}

/// One function per request. Enqueue validates the pipeline, the model plan in
/// the effective policy and the dependencies, then pokes the dispatcher.
async fn route(
    ctx: &DaemonContext,
    client: ClientKind,
    request: Request,
) -> Result<Reply, ArchonError> {
    match request {
        Request::MessageOrchestrator { text, kind, caller } => {
            crate::agent_messages::send(ctx, caller.as_ref(), kind, text).await
        }
        Request::PublishArtifact { path, name, caller } => {
            crate::artifacts::publish_for(ctx, caller.as_ref(), path, name).await
        }
        Request::ListArtifacts { item } => crate::artifacts::list_for(ctx, &item).await,
        Request::ReadArtifact {
            item,
            name,
            offset,
            limit,
        } => crate::artifacts::read_for(ctx, &item, name, offset, limit).await,
        Request::Hello { .. } => Err(ArchonError::Invalid(
            "hello is only valid as a connection's first frame".into(),
        )),
        Request::SubscribeFeed { .. } => Err(ArchonError::Invalid(
            "subscribe_feed is served by the connection, not as a plain request".into(),
        )),
        // The server stops once this reply is sent.
        Request::Shutdown => Ok(Reply::Ok),
        Request::RepoAdd(add) => repos::add(ctx, add).await,
        Request::RepoList => repos::list(ctx).await,
        Request::RepoRemove { repo } => repos::remove(ctx, &repo).await,
        Request::RepoUpdate { repo, patch } => repos::update(ctx, &repo, patch).await,
        Request::Pause(target) => control::pause(ctx, client, target, true).await,
        Request::Resume(target) => control::pause(ctx, client, target, false).await,
        Request::Autonomy { repo } => autonomy::view(ctx, repo.as_ref()).await,
        Request::SetDirective {
            text,
            expires_in_hours,
        } => autonomy::set_directive(ctx, client, &text, expires_in_hours).await,
        Request::ClearDirective { directive } => {
            autonomy::clear_directive(ctx, directive.as_ref()).await
        }
        request => handle_adaptive(ctx, client, request).await,
    }
}

/// Initiatives, steering, lessons and operator corrections.
async fn handle_adaptive(
    ctx: &DaemonContext,
    client: ClientKind,
    request: Request,
) -> Result<Reply, ArchonError> {
    match request {
        Request::EnqueueInitiative(enqueue) => initiatives::enqueue(ctx, enqueue).await,
        Request::ListInitiatives => initiatives::list(ctx).await,
        Request::CancelInitiative { initiative } => initiatives::cancel(ctx, &initiative).await,
        Request::RetryInitiative { initiative } => initiatives::retry(ctx, &initiative).await,
        Request::GetInitiative { initiative } => initiatives::get(ctx, &initiative).await,
        Request::UpdateInitiative { initiative, change } => {
            initiatives::update(ctx, &initiative, &change).await
        }
        Request::SteerItem { item, steer } => steer::steer(ctx, client, &item, steer).await,
        Request::UpdateItem { item, patch } => steer::update(ctx, client, &item, patch).await,
        Request::RecordLesson(draft) => lessons::record(ctx, client, draft).await,
        Request::Lessons(query) => lessons::list(ctx, &query).await,
        Request::UpdateLesson { lesson, patch } => lessons::update(ctx, &lesson, patch).await,
        Request::LessonsWhy { item } => lessons::why(ctx, &item).await,
        Request::LessonStats { repo } => lessons::stats(ctx, repo.as_ref()).await,
        Request::LearningCorrect(correction) => learning::correct(ctx, correction).await,
        Request::LearningWhy { item } => learning::why(ctx, &item).await,
        Request::LearningUndo { feedback } => learning::undo(ctx, &feedback).await,
        Request::LearningRestoreMemory { feedback } => {
            learning::restore_memory(ctx, &feedback).await
        }
        Request::LearningRetryMemory { feedback } => learning::retry_memory(ctx, &feedback).await,
        request => handle_queue(ctx, request).await,
    }
}

/// The one answer path: `archon answer` and the operator's `/answer` chat
/// command both end here, differing only in the surface recorded.
pub(crate) async fn answer_question(
    ctx: &DaemonContext,
    question: &QuestionId,
    text: &str,
    via: AnswerVia,
) -> Result<Question, ArchonError> {
    operator::answer(ctx, question, text, via).await
}

async fn handle_queue(ctx: &DaemonContext, request: Request) -> Result<Reply, ArchonError> {
    match request {
        Request::Enqueue(enqueue) => items::enqueue(ctx, enqueue).await,
        Request::ListItems(query) => items::list(ctx, &query).await,
        Request::GetItem { item } => items::get(ctx, &item).await,
        Request::GetStage(query) => stages::get(ctx, &query).await,
        Request::ListItemSummaries(query) => item_views::summaries(ctx, &query).await,
        Request::ResolveItem { prefix } => item_views::resolve(ctx, &prefix).await,
        Request::CancelItem { item, reason } => items::cancel(ctx, &item, reason).await,
        Request::RetryItem {
            item,
            model_plan,
            model_reasons,
            fresh,
            brief_append,
        } => {
            let retry = items::Retry {
                model_plan,
                model_reasons,
                fresh,
                brief_append,
            };
            items::retry(ctx, &item, retry).await
        }
        Request::RefreshItem { item } => items::refresh(ctx, &item).await,
        Request::Status => control::status(ctx).await,
        Request::SetCaps { caps } => control::set_caps(ctx, caps).await,
        Request::ListPipelines { repo, pipeline } => {
            control::list_pipelines(ctx, repo.as_ref(), pipeline.as_deref()).await
        }
        Request::ModelPolicy { repo } => control::model_policy(ctx, repo.as_ref()).await,
        Request::CostReport(query) => cost::report(ctx, &query).await,
        Request::ModelEvidence(query) => model_evidence::report(ctx, &query).await,
        Request::OrchestratorStatus => control::orchestrator_status(ctx).await,
        Request::OrchestratorAttach => Ok(Reply::Orchestrator(
            supervisor::attach_orchestrator(ctx).await?,
        )),
        Request::OrchestratorRestart => control::orchestrator_restart(ctx).await,
        Request::Gc {
            older_than_secs,
            force,
        } => {
            let older_than = older_than_secs.map(archon_core::age::Age::from_secs);
            let report = crate::gc::collect(ctx, older_than, force).await?;
            Ok(Reply::Gc(Box::new(report)))
        }
        request => handle_operator(ctx, request).await,
    }
}

async fn handle_operator(ctx: &DaemonContext, request: Request) -> Result<Reply, ArchonError> {
    match request {
        Request::Say { text } => operator::say(ctx, &text).await,
        Request::Notify { kind, text, item } => operator::notify(ctx, kind, &text, item).await,
        Request::AskOperator {
            text,
            item,
            options,
            caller,
        } => operator::ask(ctx, &text, item, options, caller).await,
        Request::Answer { question, text } => {
            answer_question(ctx, &question, &text, AnswerVia::Archon)
                .await
                .map(Reply::Question)
        }
        Request::RecordChatAnswer {
            question,
            answer,
            caller,
        } => operator::record_chat_answer(ctx, &question, &answer, caller.as_ref()).await,
        Request::ListQuestions { open_only } => operator::questions(ctx, open_only).await,
        Request::FeedHistory {
            before,
            limit,
            verbose,
        } => operator::history(ctx, before, limit, verbose).await,
        Request::PublishReport { title, path, item } => {
            operator::publish_report(ctx, &title, &path, item.as_ref()).await
        }
        Request::ListReports => Ok(Reply::Reports(reports::list(ctx).await?)),
        Request::ReopenReport { report } => Ok(Reply::Report(reports::reopen(ctx, &report).await?)),
        Request::GithubWatcher => Ok(Reply::GithubWatcher(Box::new(github::watcher(ctx).await?))),
        Request::ConfigureGithubWatcher { change } => {
            let view = github::configure(ctx, change).await?;
            Ok(Reply::GithubWatcher(Box::new(view)))
        }
        Request::GithubReply { thread, body } => {
            let url = github::post_reply(ctx, &thread, &body).await?;
            Ok(Reply::GithubComment { url })
        }
        other => unreachable!("{} is routed above", command_name(&other)),
    }
}

/// The wire name of a request (`cmd`), for error messages.
pub(crate) fn command_name(request: &Request) -> String {
    let value = serde_json::to_value(request).expect("requests always serialize");
    let name = value["cmd"].as_str().expect("requests are tagged by cmd");
    assert!(!name.is_empty(), "a command has a name");
    name.to_owned()
}
