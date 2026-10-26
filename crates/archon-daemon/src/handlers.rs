//! One function per `Request` (ARCHITECTURE §10, §11). The
//! operator-channel requests call into `supervisor`, `feed` and `reports`.

mod autonomy;
mod budget_cost;
mod control;
mod cost;
mod initiatives;
mod item_views;
mod items;
pub(crate) use items::{check_plan, check_reasons, check_stage_timeouts};
mod learning;

pub use learning::memory_feedback::{
    resume as resume_memory_feedback, run as deliver_memory_feedback,
};
mod lessons;
mod model_evidence;
mod operator;
mod repos;
mod revise;
mod settings;
mod stages;
mod steer;
mod waiting;

use archon_core::aether_ids::AetherAgentId;
use archon_core::autonomy::AutonomyPatch;
use archon_core::error::ArchonError;
use archon_core::id::{QuestionId, QueueItemId};
use archon_core::merge::MergeAuthority;
use archon_core::notify::{AnswerVia, Question};
use archon_core::protocol::{ClientKind, ItemPatch, Reply, RepoRef, Request};
use tokio::sync::RwLockReadGuard;

use crate::context::DaemonContext;
use crate::executor::operator_merge;
use crate::{github, integration, ops, recovery, reports, standby, supervisor, tracked};

/// `update_item` with only `autonomy`, above the item's ceiling. Only the
/// operator's confirmed request calls this; no request field reaches it.
pub async fn update_item_autonomy_above_ceiling(
    ctx: &DaemonContext,
    item: &QueueItemId,
    autonomy: AutonomyPatch,
) -> Result<Reply, ArchonError> {
    let patch = ItemPatch {
        autonomy: Some(autonomy),
        ..ItemPatch::default()
    };
    items::update_within(ctx, item, patch, revise::Ceiling::Waived).await
}

/// Validate and execute one request. Operator-only requests are refused
/// with `ErrorCode::Forbidden` over MCP and for any agent, whatever the
/// client; a request that changes Archon for an agent is made only for the
/// orchestrator archond has now (ARCHITECTURE §11). In standby and while
/// start-up recovery runs only reads are served. The steps run in this order.
///
/// A `tracked` change is known as pending before anything can refuse it; a
/// repeat under its idempotency key answers with the first outcome. One
/// function: a debug build's deepest request already nears a test's stack.
pub async fn handle(
    ctx: &DaemonContext,
    client: ClientKind,
    request: Request,
) -> Result<Reply, ArchonError> {
    let (caller, request) = unwrap_for_agent(request)?;
    let (tracking, request) = tracked::unwrap(request)?;
    let ticket = tracking
        .map(|tracking| tracked::enter(ctx, caller.as_ref(), tracking, &request))
        .transpose()?;
    let caller = caller.as_ref();
    refuse_early(ctx, client, caller, &request).await?;
    let ticket = match ticket {
        Some(ticket) => match tracked::claim(ticket).await? {
            tracked::Begun::Run(ticket) => Some(ticket),
            tracked::Begun::Replay(reply) => return Ok(*reply),
        },
        None => None,
    };
    let result = 'handled: {
        match prepare_watcher(ctx, &request).await {
            Ok(None) => {}
            answered => break 'handled answered.map(|reply| reply.expect("answered")),
        }
        // Held to the end of the call: the orchestrator is not replaced under it.
        let _authority = match take_authority(ctx, client, caller, &request).await {
            Ok(authority) => authority,
            Err(error) => break 'handled Err(error),
        };
        // Serialize the state whose before/after authority confirmations observe.
        // Lifecycle and chat delivery can replace the orchestrator, so they must
        // never hold this mutex while waiting for its authority write lock.
        let _changes = match request.changes_authority_state() {
            true => Some(ctx.authority_changes.lock().await),
            false => None,
        };
        special_routes(ctx, client, caller, request).await
    };
    if let Some(ticket) = ticket {
        tracked::finish(ticket, &result).await?;
    }
    result
}

/// The agent a request is forwarded for, and the request it carries.
fn unwrap_for_agent(request: Request) -> Result<(Option<AetherAgentId>, Request), ArchonError> {
    let (caller, request) = match request {
        Request::ForAgent { caller, request } => (Some(caller), *request),
        request => (None, request),
    };
    if matches!(request, Request::ForAgent { .. }) {
        return Err(ArchonError::Invalid(
            "for_agent carries one request, not another for_agent".into(),
        ));
    }
    Ok((caller, request))
}

/// The refusals that precede any lock: operator-only for an agent, then
/// recovery, standby and a mismatched caller.
async fn refuse_early(
    ctx: &DaemonContext,
    client: ClientKind,
    caller: Option<&AetherAgentId>,
    request: &Request,
) -> Result<(), ArchonError> {
    if for_an_agent(client, caller) && request.is_operator_only() {
        let shell = match (client, caller) {
            (ClientKind::ArchonCli, Some(agent)) => {
                format!(", and this archon CLI runs in a shell of Aether agent {agent}")
            }
            _ => String::new(),
        };
        return Err(ArchonError::Forbidden(format!(
            "{} is reserved for the operator's archon CLI{shell}",
            command_name(request)
        )));
    }
    // Recovery first: a change it held is then checked against the standby
    // a failed recovery may have entered meanwhile.
    if let Some(refusal) = recovery::refusal(ctx, request).await {
        return Err(refusal);
    }
    if let Some(refusal) = standby::refusal(ctx, request) {
        return Err(refusal);
    }
    check_named_caller(caller, request)
}

/// Watcher lifecycle work, which must precede authority lock acquisition.
/// A watched `RepoAdd` is answered here: registration validates and stores
/// the repo before enabling its watcher.
async fn prepare_watcher(
    ctx: &DaemonContext,
    request: &Request,
) -> Result<Option<Reply>, ArchonError> {
    if let Request::RepoAdd(add) = request
        && add.github_watch
    {
        return repos::add_from_operator(ctx, add.clone()).await.map(Some);
    }
    if let Request::ConfigureGithubWatcher { change } = request {
        github::require_dedicated(ctx)?;
        if github::enables(change) {
            github::prepare_enable(ctx).await?;
        }
    }
    Ok(None)
}

/// The orchestrator's authority for a call only it may make; any other call
/// is only seen.
async fn take_authority<'a>(
    ctx: &'a DaemonContext,
    client: ClientKind,
    caller: Option<&AetherAgentId>,
    request: &Request,
) -> Result<Option<RwLockReadGuard<'a, ()>>, ArchonError> {
    // A CLI forwards for the agent its environment names, which says nothing
    // of whether Aether names the callers of Archon's tools.
    let named_by_aether = client == ClientKind::ArchonMcp;
    if orchestrators_alone(client, caller, request) {
        return supervisor::authorize(ctx, caller, named_by_aether)
            .await
            .map(Some);
    }
    supervisor::saw_call(ctx, caller, named_by_aether).await?;
    Ok(None)
}

/// The operator's own settings changes and `Promote` go their own way;
/// every other request goes to [`route`].
async fn special_routes(
    ctx: &DaemonContext,
    client: ClientKind,
    caller: Option<&AetherAgentId>,
    request: Request,
) -> Result<Reply, ArchonError> {
    if client == ClientKind::ArchonCli
        && caller.is_none()
        && matches!(
            request,
            Request::SetSettings { .. }
                | Request::ResetSettings { .. }
                | Request::SetCaps { .. }
                | Request::RepoUpdate { .. }
        )
    {
        return crate::authority::socket_settings(ctx, request).await;
    }
    if let Request::RequestStatus { request } = &request {
        return tracked::status(ctx, caller, request).await;
    }
    if let Request::Promote { repo } = &request {
        let by = match for_an_agent(client, caller) {
            true => integration::Promoter::Agent,
            false => integration::Promoter::Operator,
        };
        return promote(ctx, repo, by).await;
    }
    route(ctx, client, request).await
}

async fn promote(
    ctx: &DaemonContext,
    repo: &RepoRef,
    by: integration::Promoter,
) -> Result<Reply, ArchonError> {
    let promotion = integration::promote(ctx, repo, by).await?;
    Ok(Reply::Promotion(Box::new(promotion)))
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
        | Request::RecordChatAnswer { caller, .. }
        | Request::RequestAuthority { caller, .. } => caller.as_ref(),
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
pub(crate) async fn route(
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
        Request::OperatorCommand { text } => {
            crate::operator_commands::from_socket(ctx, &text).await
        }
        Request::UndoAuthority { .. } => Err(ArchonError::Invalid(
            "use /archon undo N in verified operator chat, or cite it through request_authority"
                .into(),
        )),
        Request::Guide { topic } => crate::authority::guide(&topic),
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
        Request::MergeItem {
            item,
            ignore_checks,
            chat,
        } => {
            let authority = match chat {
                true => MergeAuthority::ArchonChat,
                false => MergeAuthority::Cli,
            };
            let merge = operator_merge::merge_now(ctx, &item, authority, ignore_checks).await?;
            Ok(Reply::Merge(Box::new(merge)))
        }
        Request::ItemDiff { item, full } => {
            let review = operator_merge::local_review(ctx, &item, full).await?;
            Ok(Reply::LocalReview(Box::new(review)))
        }
        Request::RequestAuthority {
            action,
            utterance,
            quote,
            reason,
            caller,
        } => {
            crate::authority::request(ctx, caller.as_ref(), utterance, &quote, &reason, *action)
                .await
        }
        // `handle` names the operator; any other way in is an agent's.
        Request::Promote { repo } => promote(ctx, &repo, integration::Promoter::Agent).await,
        Request::RequestMerge { item, reason } => {
            let merge = operator_merge::request(ctx, &item, reason.as_deref()).await?;
            Ok(Reply::Merge(Box::new(merge)))
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

/// Watch a registered repository on GitHub; the `/watch` chat command.
pub(crate) async fn watch_repo(ctx: &DaemonContext, repo: &RepoRef) -> Result<String, ArchonError> {
    repos::watch(ctx, repo).await
}

/// Enqueue an item archond itself asks for, such as an integration fix,
/// validated as any enqueue is.
pub(crate) async fn enqueue_for_archond(
    ctx: &DaemonContext,
    request: archon_core::protocol::EnqueueRequest,
) -> Result<archon_core::queue::QueueItem, ArchonError> {
    match items::enqueue(ctx, request).await? {
        Reply::Item(item) | Reply::ItemBudgetsOff(item) => Ok(item),
        other => unreachable!("an enqueue answers with its item, not {other:?}"),
    }
}

/// The one item whose id starts with `prefix`, as `archon` resolves it.
pub(crate) async fn resolve_item(
    ctx: &DaemonContext,
    prefix: &str,
) -> Result<QueueItemId, ArchonError> {
    match item_views::resolve(ctx, prefix).await? {
        Reply::Item(item) => Ok(item.id),
        other => unreachable!("an item lookup answers with an item, not {other:?}"),
    }
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
            stage_timeouts,
        } => {
            let retry = items::Retry {
                model_plan,
                model_reasons,
                fresh,
                brief_append,
                stage_timeouts,
            };
            items::retry(ctx, &item, retry).await
        }
        Request::RefreshItem {
            item,
            model_plan,
            model_reasons,
            stage_timeouts,
        } => {
            let asked = crate::executor::refresh::Asked {
                model_plan,
                model_reasons,
                stage_timeouts,
            };
            items::refresh(ctx, &item, asked).await
        }
        Request::Status(query) => control::status(ctx, query).await,
        Request::SetCaps { caps } => control::set_caps(ctx, caps).await,
        Request::Settings { repo } => settings::view(ctx, repo.as_ref()).await,
        Request::SetSettings { repo, settings } => {
            settings::set(ctx, repo.as_ref(), settings).await
        }
        Request::ResetSettings { repo } => settings::reset(ctx, repo.as_ref()).await,
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
        Request::Upgrade(request) => Ok(Reply::UpgradeStarted {
            text: ops::start(ctx, request).await?,
        }),
        Request::CheckUpdates => Ok(Reply::Versions(Box::new(ops::check_updates(ctx).await?))),
        Request::Gc {
            older_than_secs,
            force,
            preview,
        } => {
            let older_than = older_than_secs.map(archon_core::age::Age::from_secs);
            let report = if preview {
                crate::gc::preview(ctx, older_than, force).await?
            } else {
                crate::gc::collect(ctx, older_than, force).await?
            };
            Ok(Reply::Gc(Box::new(report)))
        }
        request => handle_operator(ctx, request).await,
    }
}

async fn handle_operator(ctx: &DaemonContext, request: Request) -> Result<Reply, ArchonError> {
    match request {
        Request::Say { text, images } => operator::say(ctx, &text, &images).await,
        Request::Notify { kind, text, item } => operator::notify(ctx, kind, &text, item).await,
        Request::ClearNotifications { item } => operator::clear_notifications(ctx, item).await,
        Request::AskOperator {
            text,
            item,
            options,
            caller,
        } => operator::ask(ctx, &text, item, options, caller).await,
        Request::Answer {
            question,
            text,
            chat,
        } => {
            let via = if chat {
                AnswerVia::ChatCommand
            } else {
                AnswerVia::Archon
            };
            answer_question(ctx, &question, &text, via)
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
            let view = github::configure_existing(ctx, change).await?;
            Ok(Reply::GithubWatcher(Box::new(view)))
        }
        Request::GithubReply { thread, body } => {
            let url = github::post_reply(ctx, &thread, &body).await?;
            Ok(Reply::GithubComment { url })
        }
        Request::Tracked { .. } | Request::RequestStatus { .. } | Request::ForAgent { .. } => {
            Err(ArchonError::Invalid(format!(
                "{} is only valid as a request of its own",
                command_name(&request)
            )))
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
