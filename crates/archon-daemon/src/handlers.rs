//! One function per `Request` (ARCHITECTURE §10, §11). The
//! operator-channel requests call into `supervisor`, `feed` and `reports`.

mod autonomy;
mod control;
mod cost;
mod item_views;
mod items;
mod model_evidence;
mod operator;
mod repos;
mod stages;
mod waiting;

use archon_core::error::ArchonError;
use archon_core::id::QuestionId;
use archon_core::notify::{AnswerVia, Question};
use archon_core::protocol::{ClientKind, Reply, Request};

use crate::context::DaemonContext;
use crate::{github, reports, standby, supervisor};

/// Validate and execute one request. `ClientKind::ArchonMcp` is refused
/// operator-only requests with `ErrorCode::Forbidden`. Enqueue validates
/// the pipeline exists for the repo, `requires_repo`, the model plan covers
/// every stage within the effective model policy, dependencies exist, and
/// then pokes the dispatcher. In standby only reads are served.
pub async fn handle(
    ctx: &DaemonContext,
    client: ClientKind,
    request: Request,
) -> Result<Reply, ArchonError> {
    if client == ClientKind::ArchonMcp && request.is_operator_only() {
        return Err(ArchonError::Forbidden(format!(
            "{} is reserved for the operator's archon CLI",
            command_name(&request)
        )));
    }
    if let Some(refusal) = standby::refusal(ctx, &request) {
        return Err(refusal);
    }
    match request {
        Request::MessageOrchestrator { text, kind, caller } => {
            crate::agent_messages::send(ctx, caller.as_ref(), kind, text).await
        }
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
        Request::UpdateItem { item, patch } => items::update(ctx, &item, patch).await,
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
        Request::Status => control::status(ctx).await,
        Request::SetCaps { caps } => control::set_caps(ctx, caps).await,
        Request::ListPipelines { repo } => control::list_pipelines(ctx, repo.as_ref()).await,
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
