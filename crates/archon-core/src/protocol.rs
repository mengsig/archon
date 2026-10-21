//! The Archon daemon wire protocol (ARCHITECTURE §10). NDJSON over a Unix
//! socket, shaped like Aether's so both are debugged the same way:
//! client `{"req":N,"cmd":"...",...}`, daemon `{"frame":"reply","id":N,
//! "result":{"Ok":{"reply":"...","data":...}}}` or `{"frame":"feed",...}`.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use serde::{Deserialize, Serialize};

use crate::Timestamp;
use crate::aether_ids::AetherAgentId;
use crate::autonomy::{AutonomyPatch, AutonomyPolicy, AutonomyView, Directive};
use crate::catalog::CatalogListing;
use crate::cost::{CostQuery, CostReport};
use crate::error::ArchonError;
use crate::feedback::{CorrectionRequest, FeedbackReceipt, LearningWhy};
use crate::github::{GithubThread, GithubWatch, GithubWatcherView, WatcherChange};
use crate::id::{
    DirectiveId, FeedbackId, InitiativeId, LessonId, PipelineRunId, QuestionId, QueueItemId,
    ReportId, StageRunId,
};
use crate::initiative::{ContractChange, EnqueueInitiative, Initiative, InitiativeDetail};
use crate::lesson::{Lesson, LessonDraft, LessonPatch, LessonQuery, LessonStats, LessonWhy};
use crate::merge::MergeRequest;
use crate::model::{Effort, ModelChoice, ModelPlanDraft, ModelReasons, ModelRef};
use crate::model_evidence::{ModelEvidenceQuery, ModelEvidenceReport};
use crate::notify::{FeedItem, NotificationKind, Question};
use crate::ops::{UpgradeRequest, Versions};
use crate::policy::ModelPolicy;
use crate::pull_request::OntoTarget;
use crate::queue::{Caps, PauseState, Priority, QueueItem};
use crate::repo::{Attribution, Checks, GhUser, Repo, RepoTiers, ShipMode};
use crate::run::{OperatorGate, PipelineRun, StageRun, StageState, Verdict};
use crate::steer::Steer;

/// 3: a GitHub watcher `add` may carry an allowlist; an older archond
/// would drop it and widen who may dispatch, so it is refused at hello.
/// 4: `retry_item` may ask for a fresh retry and both it and `update_item`
/// may carry `brief_append`; an older archond would silently drop them.
/// 5: operator-only orchestrator attachment for the native TUI.
/// 6: `archon mcp` names the calling agent on every call that changes Archon
/// (`for_agent`), and archond refuses such a call that names none.
/// 7: initiatives, `steer_item` and lessons; an older archond knows none of
/// their requests.
/// 8: `archon learning` corrections, their receipts and memory curation.
/// 9: `[limits] budgets_enabled`; `Caps` carries it, and a change that sets an
/// item's budget while budgets are off answers `ItemBudgetsOff`.
/// 10: `refresh_item`; an older archond knows no such request.
/// 11: `upgrade`, `check_updates` and `repo_add`'s `github_watch`; an older
/// archond would register a repository and silently leave it unwatched.
/// 12: merging on the operator's request (`merge_item`, `request_merge`) and
/// a repository's `grant_merge`, which an older archond would drop.
/// 13: daemon-verified operator authority requests and settings.
/// 14: a repository's `attribution`, which an older archond would drop.
/// 15: a repository's `gh_user`, which an older archond would drop.
/// 16: `promote`, a repository's `release_hook` and its confirmed full and
/// release checks; an older archond knows no such request and would drop them.
/// 17: a repository's `merge_waive_checks`, which an older archond would drop.
/// 18: `say` may carry `images`, which an older archond would drop; a
/// text-only `say` is the frame it always was.
pub const PROTOCOL_VERSION: u32 = 18;

/// Said beside an item whose `budget_usd` was set while budgets are off.
pub const BUDGETS_OFF_NOTE: &str = "Dollar budgets are globally off ([limits] budgets_enabled = \
    false), so this budget_usd is stored but not enforced; spend is still recorded. \
    `archon queue caps --budgets on` turns enforcement back on.";

/// Longest accepted frame in either direction.
pub const MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;
// A `say` with all the image bytes one message takes, as base64, still fits.
const _: () =
    assert!(crate::attachment::MAX_MESSAGE_IMAGE_BYTES / 3 * 4 + (1 << 20) < MAX_FRAME_BYTES);

/// Who is on the other end. Guards against mistakes, not attacks: the
/// socket's file mode is the security boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ClientKind {
    ArchonCli,
    ArchonMcp,
    ArchonBench,
}

/// A repository by registry name or `repo_…` id.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RepoRef(pub String);

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClientFrame {
    #[serde(rename = "req")]
    pub id: u64,
    #[serde(flatten)]
    pub request: Request,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "frame", rename_all = "snake_case")]
pub enum ServerFrame {
    /// `id: 0` is the daemon explaining why it is closing the connection.
    Reply {
        id: u64,
        result: Result<Reply, ErrorReply>,
    },
    Feed {
        sub: u64,
        item: FeedItem,
    },
}

/// Everything a client can ask `archond`. MCP tools map 1:1 onto a subset
/// (ARCHITECTURE §11).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Request {
    /// A tool call `archon mcp` forwards for the Aether agent that made it:
    /// only the orchestrator of now may change Archon (ARCHITECTURE §11).
    ForAgent {
        /// Aether's out-of-band MCP caller identity, never a tool argument.
        caller: AetherAgentId,
        request: Box<Request>,
    },
    MessageOrchestrator {
        text: String,
        #[serde(default)]
        kind: crate::agent_message::MessageKind,
        /// Aether's out-of-band MCP caller identity, never a tool argument.
        caller: Option<AetherAgentId>,
    },
    /// A stage agent keeps files from its workspace in its item's artifact
    /// store (ARCHITECTURE §13.5).
    PublishArtifact {
        /// A file or directory inside the stage's workspace.
        path: String,
        /// Where it goes in the store; the path's last component if unset.
        name: Option<String>,
        /// Aether's out-of-band MCP caller identity, never a tool argument.
        caller: Option<AetherAgentId>,
    },
    ListArtifacts {
        item: QueueItemId,
    },
    ReadArtifact {
        item: QueueItemId,
        name: String,
        #[serde(default)]
        offset: u64,
        /// Bytes to read; at most [`MAX_ARTIFACT_PAGE_BYTES`].
        limit: Option<u64>,
    },
    Hello {
        protocol_version: u32,
        client: ClientKind,
        client_version: String,
    },
    Status(StatusQuery),
    Guide {
        topic: String,
    },
    /// CLI only.
    Shutdown,

    RepoAdd(RepoAdd),
    RepoList,
    /// CLI only.
    RepoRemove {
        repo: RepoRef,
    },
    /// CLI only; the one path to a repository's autonomy ceiling.
    RepoUpdate {
        repo: RepoRef,
        patch: RepoPatch,
    },

    Enqueue(EnqueueRequest),
    ListItems(ItemQuery),
    /// Like `ListItems`, each item with its live stage, spend and PR URL.
    ListItemSummaries(ItemQuery),
    GetItem {
        item: QueueItemId,
    },
    /// The stage runs of an item's latest run, or one stage's full output.
    GetStage(StageQuery),
    /// The one item whose id starts with `prefix` (see
    /// [`crate::id::queue_item_prefix`]). An unknown prefix is `NotFound`;
    /// an ambiguous one is refused, naming the items it matches.
    ResolveItem {
        prefix: String,
    },
    UpdateItem {
        item: QueueItemId,
        patch: ItemPatch,
    },
    CancelItem {
        item: QueueItemId,
        reason: Option<String>,
    },
    RetryItem {
        item: QueueItemId,
        model_plan: Option<ModelPlanDraft>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model_reasons: Option<ModelReasons>,
        /// Start again from the base branch's head, on a new branch, with
        /// the old worktree set aside; otherwise the worktree is reused.
        #[serde(default)]
        fresh: bool,
        /// Appended to the brief under a dated "Amendment" heading.
        #[serde(default)]
        brief_append: Option<String>,
        /// Stage id to minutes, merged into the item's; within the configured maximum.
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        stage_timeouts: BTreeMap<String, u32>,
    },
    /// Merge the finished item's base branch, fetched afresh, into its branch:
    /// a new item on the same branch and pull request (ARCHITECTURE §13).
    RefreshItem {
        item: QueueItemId,
    },
    Pause(PauseTarget),
    Resume(PauseTarget),
    /// Operator only; persisted in the global settings overlay.
    SetCaps {
        caps: Caps,
    },
    /// The operator settings stored for a repository, or globally, and the
    /// values in force with their sources.
    Settings {
        #[serde(default)]
        repo: Option<RepoRef>,
    },
    /// Operator only: replace the overlay stored at one scope whole.
    SetSettings {
        #[serde(default)]
        repo: Option<RepoRef>,
        settings: crate::settings::OperatorSettings,
    },
    /// Operator only: forget the overlay stored at one scope.
    ResetSettings {
        #[serde(default)]
        repo: Option<RepoRef>,
    },

    ListPipelines {
        repo: Option<RepoRef>,
        /// Only this pipeline, listed in full.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pipeline: Option<String>,
    },
    ModelPolicy {
        repo: Option<RepoRef>,
    },

    /// Operator chat line to the orchestrator. CLI only. `text` may be
    /// blank when there is at least one image.
    Say {
        text: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        images: Vec<crate::attachment::ImageAttachment>,
    },
    Notify {
        kind: NotificationKind,
        text: String,
        item: Option<QueueItemId>,
    },
    AskOperator {
        text: String,
        item: Option<QueueItemId>,
        #[serde(default)]
        options: Vec<String>,
        /// Aether's out-of-band MCP caller identity, never a tool argument;
        /// recorded as the question's asker.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        caller: Option<AetherAgentId>,
    },
    /// CLI only: the operator's answer, never the orchestrator's.
    Answer {
        question: QuestionId,
        text: String,
        /// Sent from Archon's chat, so the feed marks it as chat-given.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        chat: bool,
    },
    /// The orchestrator records that the operator answered, in chat, a
    /// question it asked with `ask_operator`. Never a gate's question.
    RecordChatAnswer {
        question: QuestionId,
        /// The operator's words.
        answer: String,
        /// Aether's out-of-band MCP caller identity, never a tool argument.
        caller: Option<AetherAgentId>,
    },
    ListQuestions {
        open_only: bool,
    },
    /// Stream feed items with `seq > after` (`None`: live only).
    SubscribeFeed {
        after: Option<u64>,
        verbose: bool,
    },
    FeedHistory {
        before: Option<u64>,
        limit: usize,
        verbose: bool,
    },

    PublishReport {
        title: String,
        path: std::path::PathBuf,
        item: Option<QueueItemId>,
    },
    /// Every recorded report, most recently changed first.
    ListReports,
    /// Show a recorded report again at the operator's request: resume its
    /// Lavish session, even one they ended, or name its path without
    /// Lavish. A failed reopen leaves the report as it was.
    ReopenReport {
        report: ReportId,
    },

    OrchestratorStatus,
    /// CLI only: ensure the orchestrator is live, without sending a message.
    OrchestratorAttach,
    /// CLI only: cancel and recreate the orchestrator agent.
    OrchestratorRestart,
    /// Operator only: install, drain, then restart archond and Archon's own
    /// aetherd. Accepted at once; each step is reported in the feed.
    Upgrade(UpgradeRequest),
    /// Operator only, as it asks the network: the latest releases of archon
    /// and aether, kept for `status` and `queue_status`.
    CheckUpdates,

    /// CLI only: collect what finished runs left (ARCHITECTURE §13.2).
    /// Worktrees go once their runs finished `older_than_secs` ago
    /// (`None`: `worktrees.gc_after`); with `force`, even with changes.
    Gc {
        older_than_secs: Option<u64>,
        force: bool,
    },

    /// Spend over a range of local days, grouped by day, repository,
    /// pipeline, model, item or stage (ARCHITECTURE §5.3).
    CostReport(CostQuery),
    ModelEvidence(ModelEvidenceQuery),

    /// Autonomy defaults, ceiling and directives, for one repository or
    /// globally (ARCHITECTURE §18).
    Autonomy {
        repo: Option<RepoRef>,
    },
    /// Record a standing operator directive. It steers the orchestrator's
    /// choices and never raises the ceiling, so the MCP proxy may send it.
    SetDirective {
        text: String,
        expires_in_hours: Option<u32>,
    },
    /// Clear one directive, or every directive when `None`.
    ClearDirective {
        directive: Option<DirectiveId>,
    },

    /// Archon's GitHub watcher in its aetherd.
    GithubWatcher,
    /// CLI only: GitHub content must never change what is watched.
    ConfigureGithubWatcher {
        change: WatcherChange,
    },
    /// Comment on a thread of a repository Archon watches; archond appends
    /// the reply marker so the comment never re-triggers the watcher.
    GithubReply {
        thread: GithubThread,
        body: String,
    },

    /// Several items, usually in several repositories, built against one
    /// shared contract.
    EnqueueInitiative(EnqueueInitiative),
    ListInitiatives,
    CancelInitiative {
        initiative: InitiativeId,
    },
    RetryInitiative {
        initiative: InitiativeId,
    },
    GetInitiative {
        initiative: InitiativeId,
    },
    /// Append to the shared contract or replace it; live members are told.
    UpdateInitiative {
        initiative: InitiativeId,
        #[serde(flatten)]
        change: ContractChange,
    },
    /// Change a queued or running item without cancelling it.
    SteerItem {
        item: QueueItemId,
        steer: Steer,
    },

    RecordLesson(LessonDraft),
    Lessons(LessonQuery),
    /// CLI only: the operator edits, pins, accepts or retracts a lesson,
    /// and that always wins over what was learned.
    UpdateLesson {
        lesson: LessonId,
        patch: LessonPatch,
    },
    /// The lessons and learned defaults that shaped an item.
    LessonsWhy {
        item: QueueItemId,
    },
    LessonStats {
        repo: Option<RepoRef>,
    },

    /// CLI only: the operator corrects one stage of a finished run.
    LearningCorrect(CorrectionRequest),
    /// The corrections of an item's runs and what each does now.
    LearningWhy {
        item: QueueItemId,
    },
    /// CLI only: revoke a correction's model penalty; its memory stays as is.
    LearningUndo {
        feedback: FeedbackId,
    },
    /// CLI only: restore the memory a correction forgot.
    LearningRestoreMemory {
        feedback: FeedbackId,
    },
    /// CLI only: retry a correction's pending or failed memory forget.
    LearningRetryMemory {
        feedback: FeedbackId,
    },

    /// CLI only: the operator merges an item's delivered pull request now,
    /// once archond's checks pass (ARCHITECTURE §18.6).
    MergeItem {
        item: QueueItemId,
        /// The operator acknowledges checks that are not all green.
        #[serde(default)]
        ignore_checks: bool,
        /// Sent by `/merge` in `archon chat`, for the audit.
        #[serde(default)]
        chat: bool,
    },
    /// Whole-message command on the operator socket; never exposed over MCP.
    OperatorCommand {
        text: String,
    },
    UndoAuthority {
        change: u64,
    },
    /// The orchestrator cites a message observed on archond's own stream.
    RequestAuthority {
        action: Box<Request>,
        utterance: u64,
        quote: String,
        reason: String,
        caller: Option<AetherAgentId>,
    },
    /// Promote the repository's integration branch into its release branch
    /// once its head is green; an agent's call merges only under `grant_merge`.
    Promote {
        repo: RepoRef,
    },
    /// Request a merge; archond confirms unless `grant_merge` permits it.
    RequestMerge {
        item: QueueItemId,
        /// What the operator said, in a line.
        #[serde(default)]
        reason: Option<String>,
    },
}

/// Who may make a request and whether it changes Archon.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestClass {
    /// Reads Archon; any client may make it.
    Read,
    /// Reads Archon, for the operator only.
    OperatorRead,
    /// Changes Archon; an agent may ask, under the orchestrator's authority.
    Change,
    /// Changes Archon, for the operator only.
    OperatorChange,
}

impl Request {
    /// The request's class. No arm is a wildcard, so a new request kind
    /// does not compile until it is classified here.
    pub fn class(&self) -> RequestClass {
        use RequestClass::{Change, OperatorChange, OperatorRead, Read};
        match self {
            Self::ForAgent { request, .. } => request.class(),
            Self::Hello { .. }
            | Self::Status(_)
            | Self::Guide { .. }
            | Self::RepoList
            | Self::ListItems(_)
            | Self::ListItemSummaries(_)
            | Self::GetItem { .. }
            | Self::GetStage(_)
            | Self::ResolveItem { .. }
            | Self::ListPipelines { .. }
            | Self::ModelPolicy { .. }
            | Self::ListQuestions { .. }
            | Self::SubscribeFeed { .. }
            | Self::FeedHistory { .. }
            | Self::ListReports
            | Self::OrchestratorStatus
            | Self::CostReport(_)
            | Self::ModelEvidence(_)
            | Self::Autonomy { .. }
            | Self::Settings { .. }
            | Self::GithubWatcher
            | Self::ListInitiatives
            | Self::GetInitiative { .. }
            | Self::Lessons(_)
            | Self::LessonsWhy { .. }
            | Self::LessonStats { .. }
            | Self::LearningWhy { .. }
            | Self::ListArtifacts { .. }
            | Self::ReadArtifact { .. } => Read,
            Self::CheckUpdates => OperatorRead,
            Self::OperatorCommand { .. }
            | Self::UndoAuthority { .. }
            | Self::Shutdown
            | Self::RepoRemove { .. }
            | Self::RepoUpdate { .. }
            | Self::SetCaps { .. }
            | Self::SetSettings { .. }
            | Self::ResetSettings { .. }
            | Self::Say { .. }
            | Self::OrchestratorRestart
            | Self::OrchestratorAttach
            | Self::Upgrade(_)
            | Self::Gc { .. }
            | Self::ConfigureGithubWatcher { .. }
            | Self::UpdateLesson { .. }
            | Self::LearningCorrect(_)
            | Self::LearningUndo { .. }
            | Self::LearningRestoreMemory { .. }
            | Self::LearningRetryMemory { .. }
            | Self::MergeItem { .. }
            | Self::Answer { .. } => OperatorChange,
            Self::RepoAdd(add) if add.auto_merge || add.github_watch => OperatorChange,
            Self::Pause(PauseTarget::Global { hard: true })
            | Self::Resume(PauseTarget::Global { hard: true }) => OperatorChange,
            Self::RepoAdd(_)
            | Self::Pause(_)
            | Self::Resume(_)
            | Self::MessageOrchestrator { .. }
            | Self::PublishArtifact { .. }
            | Self::Enqueue(_)
            | Self::UpdateItem { .. }
            | Self::CancelItem { .. }
            | Self::RetryItem { .. }
            | Self::RefreshItem { .. }
            | Self::Notify { .. }
            | Self::AskOperator { .. }
            | Self::RecordChatAnswer { .. }
            | Self::PublishReport { .. }
            | Self::ReopenReport { .. }
            | Self::SetDirective { .. }
            | Self::ClearDirective { .. }
            | Self::GithubReply { .. }
            | Self::EnqueueInitiative(_)
            | Self::CancelInitiative { .. }
            | Self::RetryInitiative { .. }
            | Self::UpdateInitiative { .. }
            | Self::SteerItem { .. }
            | Self::RecordLesson(_)
            | Self::RequestAuthority { .. }
            | Self::Promote { .. }
            | Self::RequestMerge { .. } => Change,
        }
    }

    /// Operator-only actions: archond refuses them over MCP and for any
    /// agent, whichever client forwards them.
    pub fn is_operator_only(&self) -> bool {
        matches!(
            self.class(),
            RequestClass::OperatorRead | RequestClass::OperatorChange
        )
    }

    /// Whether the request only reads Archon.
    pub fn is_read_only(&self) -> bool {
        matches!(
            self.class(),
            RequestClass::Read | RequestClass::OperatorRead
        )
    }

    /// Whether the request changes state that operator-authority
    /// confirmations compare before and after; such requests are serialized.
    pub fn changes_authority_state(&self) -> bool {
        matches!(
            self,
            Self::SetSettings { .. }
                | Self::ResetSettings { .. }
                | Self::UpdateItem { .. }
                | Self::SteerItem { .. }
                | Self::RepoUpdate { .. }
                | Self::RepoRemove { .. }
                | Self::RepoAdd(_)
                | Self::SetCaps { .. }
                | Self::Pause(_)
                | Self::Resume(_)
                | Self::ConfigureGithubWatcher { .. }
                | Self::SetDirective { .. }
                | Self::ClearDirective { .. }
                | Self::UpdateLesson { .. }
                | Self::LearningCorrect(_)
                | Self::LearningUndo { .. }
                | Self::LearningRestoreMemory { .. }
                | Self::LearningRetryMemory { .. }
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoAdd {
    pub path: std::path::PathBuf,
    pub name: Option<String>,
    pub ship: Option<ShipMode>,
    pub base_branch: Option<String>,
    pub default_pipeline: Option<String>,
    /// Registers the repository with the autonomy ceiling `merge=auto`;
    /// operator only, as it raises the ceiling. `RepoUpdate` sets the rest.
    #[serde(default)]
    pub auto_merge: bool,
    /// Also watch the repository's GitHub `origin` with Archon's watcher;
    /// operator only, as only the operator changes what is watched.
    #[serde(default)]
    pub github_watch: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoPatch {
    pub ship: Option<ShipMode>,
    #[serde(default)]
    pub base_branch: Clearable<String>,
    #[serde(default)]
    pub default_pipeline: Clearable<String>,
    /// Replaces the whole registry ceiling; the empty patch clears it.
    pub autonomy_max: Option<AutonomyPatch>,
    /// Sets or clears the repository's `grant_merge` (ARCHITECTURE §18.6).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grant_merge: Option<bool>,
    /// Replaces the base branches whose checks `grant_merge` does not wait
    /// for (ARCHITECTURE §18.6); operator only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub merge_waive_checks: Option<Vec<String>>,
    /// Sets the repository's attribution policy; operator only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attribution: Option<Attribution>,
    /// Sets or clears the GitHub login its `gh` calls and pushes run as;
    /// operator only.
    #[serde(default, skip_serializing_if = "Clearable::is_keep")]
    pub gh_user: Clearable<GhUser>,
    /// Changes the registry's verification tiers; operator only.
    #[serde(default, skip_serializing_if = "TiersPatch::is_keep")]
    pub tiers: TiersPatch,
}

/// A patch to the registry's [`RepoTiers`], field by field.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TiersPatch {
    pub integration_branch: Clearable<String>,
    pub release_branch: Clearable<String>,
    pub fast_check: Clearable<String>,
    pub full_check: Clearable<String>,
    pub release_hook: Clearable<String>,
    /// The committed file's fast check the operator confirms, word for word.
    pub confirmed_fast_check: Clearable<String>,
    /// The committed file's full check the operator confirms, word for word.
    pub confirmed_full_check: Clearable<String>,
    /// The committed file's release hook the operator confirms, word for word.
    pub confirmed_release_check: Clearable<String>,
}

impl TiersPatch {
    pub fn is_keep(&self) -> bool {
        *self == Self::default()
    }

    /// `current` after the patch; the confirmed checks are the
    /// repository's own and not applied here.
    pub fn apply(self, current: RepoTiers) -> RepoTiers {
        RepoTiers {
            integration_branch: self.integration_branch.apply(current.integration_branch),
            release_branch: self.release_branch.apply(current.release_branch),
            checks: Checks {
                fast: self.fast_check.apply(current.checks.fast),
                full: self.full_check.apply(current.checks.full),
                release: self.release_hook.apply(current.checks.release),
            },
        }
    }
}

/// A patch to an optional setting. Explicit on the wire (`"keep"`,
/// `"clear"`, `{"set":v}`) because JSON collapses `Option<Option<T>>`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Clearable<T> {
    #[default]
    Keep,
    Clear,
    Set(T),
}

impl<T> Clearable<T> {
    /// Whether it leaves the setting as it is.
    pub fn is_keep(&self) -> bool {
        matches!(self, Self::Keep)
    }

    /// `current` after the patch.
    pub fn apply(self, current: Option<T>) -> Option<T> {
        match self {
            Self::Keep => current,
            Self::Clear => None,
            Self::Set(value) => Some(value),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EnqueueRequest {
    /// Required unless the pipeline has `requires_repo = false`.
    pub repo: Option<RepoRef>,
    /// `None` uses the repository's default pipeline.
    pub pipeline: Option<String>,
    pub title: String,
    pub brief: String,
    #[serde(default)]
    pub priority: Priority,
    #[serde(default)]
    pub depends_on: Vec<QueueItemId>,
    /// An existing pull request to deliver into instead of opening one;
    /// resolved and frozen at enqueue.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub onto: Option<OntoTarget>,
    #[serde(default)]
    pub tags: Vec<String>,
    /// Choices per stage, within the model policy. Optional: a stage left
    /// out gets what was learned for the repository, else a default.
    #[serde(default)]
    pub model_plan: ModelPlanDraft,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_reasons: Option<ModelReasons>,
    pub budget_usd: Option<f64>,
    /// Stage id to minutes, within `[limits] max_stage_timeout_minutes`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub stage_timeouts: BTreeMap<String, u32>,
    /// Retrying an enqueue with the same key returns the first item.
    pub idempotency_key: Option<String>,
    /// Knobs chosen for this item; the rest come from the defaults. The
    /// daemon refuses anything above the ceiling.
    #[serde(default)]
    pub autonomy: AutonomyPatch,
    /// The GitHub thread that asked for this work, for replying when it ends.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<GithubThread>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ItemQuery {
    /// `QueueState::kind()` names; empty means all.
    #[serde(default)]
    pub states: Vec<String>,
    pub repo: Option<RepoRef>,
    pub pipeline: Option<String>,
    pub tag: Option<String>,
    pub limit: Option<usize>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ItemPatch {
    pub priority: Option<Priority>,
    pub model_plan: Option<ModelPlanDraft>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_reasons: Option<ModelReasons>,
    pub tags: Option<Vec<String>>,
    #[serde(default)]
    pub add_depends_on: Vec<QueueItemId>,
    pub budget_usd: Option<f64>,
    /// Stage id to minutes, merged into the item's; within the configured maximum.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub stage_timeouts: BTreeMap<String, u32>,
    /// Appended to the brief under a dated "Amendment" heading; a running
    /// item's live agents are sent it (ARCHITECTURE §6.6).
    #[serde(default)]
    pub brief_append: Option<String>,
    /// Knobs to change, within the item's ceiling (ARCHITECTURE §18.1).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub autonomy: Option<AutonomyPatch>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "scope", rename_all = "snake_case")]
pub enum PauseTarget {
    Global {
        /// Also pause running stage agents in Aether (`archon pause --hard`).
        /// CLI only.
        #[serde(default)]
        hard: bool,
    },
    Repo {
        name: RepoRef,
    },
    Pipeline {
        name: String,
    },
}

/// Adjacently tagged like Aether's replies: `{"reply":"item","data":{...}}`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "reply", content = "data", rename_all = "snake_case")]
pub enum Reply {
    Guide {
        text: String,
    },
    MessageReceipt {
        receipt: crate::id::FeedItemId,
    },
    Welcome(Welcome),
    Ok,
    Status(Box<DaemonStatus>),
    Repo(Repo),
    Repos(Vec<Repo>),
    Item(QueueItem),
    /// `Item`, for a change that set a dollar budget while budgets are off.
    ItemBudgetsOff(QueueItem),
    Items(Vec<QueueItem>),
    ItemSummaries(Vec<ItemSummary>),
    ItemDetail(Box<ItemDetail>),
    StageDetail(Box<StageDetail>),
    Catalog(CatalogListing),
    ModelPolicy(ModelPolicy),
    Question(Question),
    Questions(Vec<Question>),
    Subscribed {
        id: u64,
        last_seq: u64,
    },
    Feed(Vec<FeedItem>),
    Report(ReportPublished),
    Reports(Vec<ReportSummary>),
    Orchestrator(OrchestratorInfo),
    Autonomy(Box<AutonomyView>),
    Directive(Directive),
    /// How many directives a clear removed.
    Cleared(u32),
    GithubWatcher(Box<GithubWatcherView>),
    /// The comment `github_reply` posted.
    GithubComment {
        url: String,
    },
    /// An upgrade was accepted; what it will do.
    UpgradeStarted {
        text: String,
    },
    Versions(Box<Versions>),
    Gc(Box<GcReport>),
    Artifacts(Box<ArtifactListing>),
    ArtifactPage(Box<ArtifactPage>),
    CostReport(Box<CostReport>),
    ModelEvidence(Box<ModelEvidenceReport>),
    Initiative(Box<InitiativeDetail>),
    Initiatives(Vec<Initiative>),
    Lesson(Lesson),
    Lessons(Vec<Lesson>),
    LessonsWhy(Box<LessonWhy>),
    LessonStats(Box<LessonStats>),
    Feedback(Box<FeedbackReceipt>),
    LearningWhy(Box<LearningWhy>),
    Merge(Box<MergeRequest>),
    Promotion(Box<crate::integration::Promotion>),
    Settings(Box<crate::settings::SettingsView>),
}

/// What one gc pass did (ARCHITECTURE §13.2). Paths are real paths.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GcReport {
    /// Worktrees, scratch directories, what team runs and fresh retries set
    /// aside, and team directories gc emptied.
    pub removed: Vec<std::path::PathBuf>,
    /// Worktrees whose build caches were deleted; their source stays.
    pub reclaimed: Vec<Reclaimed>,
    /// Left alone on purpose, with why.
    pub skipped: Vec<(std::path::PathBuf, String)>,
    /// Attempted and failed, or could not be looked into, with why.
    pub failed: Vec<(std::path::PathBuf, String)>,
    /// Repositories' shared build caches deleted (ARCHITECTURE §13.5).
    #[serde(default)]
    pub caches: Vec<Reclaimed>,
    /// Items' published artifacts deleted after `artifacts.keep_after`.
    #[serde(default)]
    pub artifacts: Vec<Reclaimed>,
    /// Items' own build target directories deleted: at the item's end, or
    /// by gc once its item was final or gone.
    #[serde(default)]
    pub item_caches: Vec<Reclaimed>,
    /// Owned report snapshots of settled items removed after storage retention.
    #[serde(default)]
    pub reports: Vec<Reclaimed>,
    /// Disk space freed by everything above together.
    pub bytes_freed: u64,
    /// The pass ran with the shorter disk-pressure ages.
    #[serde(default)]
    pub pressure: bool,
}

/// The build caches deleted from one worktree.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reclaimed {
    pub path: std::path::PathBuf,
    pub bytes: u64,
}

impl GcReport {
    /// Add `other`'s entries to this report.
    pub fn absorb(&mut self, other: GcReport) {
        self.removed.extend(other.removed);
        self.reclaimed.extend(other.reclaimed);
        self.caches.extend(other.caches);
        self.artifacts.extend(other.artifacts);
        self.item_caches.extend(other.item_caches);
        self.reports.extend(other.reports);
        self.skipped.extend(other.skipped);
        self.failed.extend(other.failed);
        self.bytes_freed = self.bytes_freed.saturating_add(other.bytes_freed);
    }

    /// Whether the pass removed or reclaimed anything.
    pub fn freed_any(&self) -> bool {
        !self.removed.is_empty()
            || !self.reclaimed.is_empty()
            || !self.caches.is_empty()
            || !self.artifacts.is_empty()
            || !self.item_caches.is_empty()
            || !self.reports.is_empty()
    }

    /// What the pass did, in one line, for the feed and `archon status`.
    pub fn headline(&self) -> String {
        let why = if self.pressure {
            " (disk pressure)"
        } else {
            ""
        };
        format!(
            "Gc{why} freed {}: removed {} worktree(s) and set-aside directories, deleted the \
             build caches of {} worktree(s), {} shared build cache(s), the build directories of {} item(s), \
             the artifacts of {} item(s), and {} report snapshot(s).",
            gib(self.bytes_freed),
            self.removed.len(),
            self.reclaimed.len(),
            self.caches.len(),
            self.item_caches.len(),
            self.artifacts.len(),
            self.reports.len(),
        )
    }
}

/// `DiskStatus::gc`: the last pass that freed anything.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GcStatus {
    pub at: Timestamp,
    pub bytes_freed: u64,
    pub pressure: bool,
    /// [`GcReport::headline`] of that pass.
    pub headline: String,
}

/// The most bytes one `read_artifact` call returns.
pub const MAX_ARTIFACT_PAGE_BYTES: u64 = 64 * 1024;

/// One file of an item's artifact store, named relative to it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactFile {
    pub name: String,
    pub bytes: u64,
}

/// An item's artifact store: where it is and what it holds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactListing {
    pub item: QueueItemId,
    /// The directory holding the files, readable by any agent on the host.
    pub dir: std::path::PathBuf,
    /// Sorted by name; at most [`MAX_ARTIFACT_LISTED`].
    pub files: Vec<ArtifactFile>,
    /// Files beyond those listed.
    pub omitted: u64,
    pub total_bytes: u64,
    /// The files a publish just stored; empty in a plain listing.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub published: Vec<ArtifactFile>,
}

/// The most files one listing names.
pub const MAX_ARTIFACT_LISTED: usize = 500;

/// A page of one artifact's text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactPage {
    pub item: QueueItemId,
    pub name: String,
    pub path: std::path::PathBuf,
    pub offset: u64,
    pub total_bytes: u64,
    pub text: String,
    /// Where the next page starts; `None` at the end.
    pub next_offset: Option<u64>,
}

/// `Status`'s page of waiting items. The default encodes as a bare
/// `{"cmd":"status"}`, which every archond reads.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct StatusQuery {
    /// Waiting items to skip.
    #[serde(skip_serializing_if = "is_zero")]
    pub waiting_offset: usize,
    /// Waiting items per page; `None`: [`WAITING_PAGE`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub waiting_limit: Option<usize>,
}

/// Waiting items a status page holds unless asked otherwise.
pub const WAITING_PAGE: usize = 50;
/// The most waiting items one status page may ask for.
pub const MAX_WAITING_PAGE: usize = 500;

fn is_zero(n: &usize) -> bool {
    *n == 0
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Welcome {
    pub protocol_version: u32,
    pub daemon_version: String,
    pub pid: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DaemonStatus {
    /// Read-time explanations for queued, blocked and operator-waiting
    /// items: the page `StatusQuery` asked for.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub waiting_items: Vec<ItemSummary>,
    /// Waiting items on every page; `0` from an archond that predates pages.
    #[serde(default)]
    pub waiting_total: u32,
    /// Where the next page of waiting items starts; `None` at the end.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub waiting_next_offset: Option<u32>,
    pub daemon_version: String,
    pub pid: u32,
    pub started_at: Timestamp,
    pub aether: AetherLinkStatus,
    pub orchestrator: Option<OrchestratorInfo>,
    pub caps: Caps,
    pub pause: PauseState,
    pub running_agents: u32,
    pub active_runs: u32,
    pub queued_items: u32,
    pub blocked_items: u32,
    pub open_questions: u32,
    pub spend_today_usd: f64,
    pub spend_total_usd: f64,
    pub lavish_available: bool,
    /// Global autonomy defaults and ceiling.
    pub autonomy: AutonomyPolicy,
    /// Directives in force.
    pub directives: Vec<Directive>,
    pub open_gates: u32,
    /// Merge confirmations the operator has not answered, oldest first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub merges_waiting: Vec<MergeRequest>,
    /// The repositories whose `grant_merge` is on.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub merge_granted: Vec<String>,
    /// News events (triggers) held until the orchestrator can take them.
    #[serde(default)]
    pub held_news: u32,
    /// The `archon` MCP server in Aether, and archond's heals of it. `None`
    /// from an archond that predates the check.
    #[serde(default)]
    pub mcp: Option<McpStatus>,
    /// Archon's GitHub watcher. `None` in shared mode, where archond
    /// leaves the operator's Aether watcher alone, or from an older archond.
    #[serde(default)]
    pub github: Option<GithubWatch>,
    /// Free space where archond writes, and whether it holds dispatch.
    /// `None` from an archond that predates the check.
    #[serde(default)]
    pub disk: Option<DiskStatus>,
    /// Running, installed and latest versions of archon and aether as
    /// archond last read them. `None` before the first reading.
    #[serde(default)]
    pub versions: Option<Versions>,
    /// The upgrade under way, in one line.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upgrade: Option<String>,
    /// How fresh the sections archond asks Aether for are. `None` from an
    /// archond that asked Aether while answering.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub health: Option<StatusHealth>,
    /// Each provider's quota headroom for stage routing, as archond last
    /// read it from Aether. `None` before a reading or from an older archond.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quota: Option<QuotaStatus>,
    /// Start-up recovery under way; `None` once it is done or from an
    /// older archond.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recovery: Option<RecoveryProgress>,
    /// Each watched integration branch: green, red, verifying or pending,
    /// its last green commit and the merges since.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub integration: Vec<crate::integration::IntegrationStatus>,
}

/// `DaemonStatus::recovery`: how far archond is in recovering its runs.
/// Until it is done, changes are refused and nothing is dispatched.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecoveryProgress {
    pub runs_done: u32,
    /// Live runs to recover; `None` until they are counted.
    pub runs_total: Option<u32>,
}

impl RecoveryProgress {
    /// `recovering 40/120 runs`, as `status` and `queue_status` show it.
    /// Clients show what archond sent, so this checks nothing.
    pub fn describe(&self) -> String {
        match self.runs_total {
            Some(total) => format!("recovering {}/{total} runs", self.runs_done),
            None => "recovering, counting runs".to_owned(),
        }
    }
}

/// `DaemonStatus::quota`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QuotaStatus {
    /// When archond read it from Aether.
    pub as_of: Timestamp,
    pub providers: Vec<ProviderQuotaStatus>,
}

/// One provider's quota as tier-only stage routing scores it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProviderQuotaStatus {
    pub provider: String,
    /// What its weight is multiplied by, 0 to 2; `None` while its usage is
    /// unknown or stale, which routes by weight alone.
    pub headroom: Option<f64>,
    /// Percent of each window held back for the orchestrator.
    pub reserve_percent: u8,
    /// The earliest reset among its windows still ahead.
    pub next_reset: Option<Timestamp>,
    pub windows: Vec<crate::quota::QuotaWindow>,
}

impl ProviderQuotaStatus {
    /// `claude: headroom 0.40, reserve 15%, next reset …; 5h 32% left`, as
    /// `status` and `queue_status` show it.
    pub fn describe(&self) -> String {
        assert!(self.reserve_percent <= 100, "a reserve is a percentage");
        let headroom = self
            .headroom
            .map_or("unknown (routes by weight)".to_owned(), |h| {
                assert!(
                    (0.0..=crate::quota::MAX_HEADROOM).contains(&h),
                    "headroom is clamped"
                );
                format!("{h:.2}")
            });
        let reset = self.next_reset.map_or("none reported".to_owned(), |at| {
            at.format("%Y-%m-%d %H:%M UTC").to_string()
        });
        let mut line = format!(
            "{}: headroom {headroom}, reserve {}%, next reset {reset}",
            self.provider, self.reserve_percent
        );
        for (n, window) in self.windows.iter().enumerate() {
            let lead = if n == 0 { "; " } else { ", " };
            let _ = write!(
                line,
                "{lead}{} {}% left",
                window.name, window.remaining_percent
            );
        }
        line
    }
}

/// `DaemonStatus::health`. archond asks Aether off the request path and
/// shows its latest answers, so status never waits on a slow aetherd.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusHealth {
    /// Aether itself, through the orchestrator's agent record.
    pub aether: ProbeHealth,
    /// The `archon` MCP server in Aether (`DaemonStatus::mcp`).
    pub mcp: ProbeHealth,
    /// Archon's GitHub watcher; `None` in shared mode, where it is not read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub github: Option<ProbeHealth>,
}

/// How fresh one status section is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProbeHealth {
    /// When Aether last answered for this section; `None`: it never has.
    pub as_of: Option<Timestamp>,
    /// Why the latest ask failed; `None` when it answered or still runs.
    pub last_error: Option<String>,
    /// The section is not the latest answer: the ask failed, or it was
    /// still running when status answered.
    pub stale: bool,
}

impl StatusHealth {
    /// One phrase per stale section, e.g. `aether: last answer 42s ago,
    /// still asking`; empty while every section is fresh.
    pub fn stale(&self, now: Timestamp) -> Vec<String> {
        let sections = [
            ("aether", Some(&self.aether)),
            ("mcp", Some(&self.mcp)),
            ("github", self.github.as_ref()),
        ];
        sections
            .into_iter()
            .filter_map(|(name, probe)| probe.filter(|p| p.stale).map(|p| (name, p)))
            .map(|(name, probe)| format!("{name}: {}", probe.describe(now)))
            .collect()
    }
}

impl ProbeHealth {
    /// Seconds since Aether last answered, at `now`.
    pub fn age_secs(&self, now: Timestamp) -> Option<i64> {
        self.as_of.map(|at| (now - at).num_seconds().max(0))
    }

    fn describe(&self, now: Timestamp) -> String {
        let answered = self.age_secs(now).map_or_else(
            || "never answered".to_owned(),
            |age| format!("last answer {age}s ago"),
        );
        let latest = self.last_error.as_deref().map_or_else(
            || "still asking".to_owned(),
            |error| format!("latest ask failed: {error}"),
        );
        format!("{answered}, {latest}")
    }
}

/// `DaemonStatus::disk`: the fuller of the filesystems holding the data
/// dir and the worktree root (ARCHITECTURE §7.3).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiskStatus {
    /// The watched path with the least free space.
    pub path: std::path::PathBuf,
    /// `None` when it could not be read; `error` says why.
    pub free_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// `limits.min_free_gb` in bytes; 0 when the low-disk hold is off.
    pub min_free_bytes: u64,
    /// Dispatch is held for disk space.
    pub held: bool,
    /// The disk-full failure that holds dispatch, if one does.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disk_full: Option<String>,
    /// Why archond stopped its work and serves only reads until it
    /// restarts: its own store hit a full disk.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub standby: Option<String>,
    /// The last gc pass that freed anything.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gc: Option<GcStatus>,
    /// Sampled at most once a minute; paths are disjoint within each kind.
    #[serde(default)]
    pub storage: Vec<StorageUsage>,
}

/// Allocated bytes for one storage kind, without following symlinks.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StorageUsage {
    pub kind: String,
    pub path: std::path::PathBuf,
    pub bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// `bytes` in GiB with one decimal, as `df -h` shows sizes.
pub fn gib(bytes: u64) -> String {
    let gib = bytes as f64 / f64::from(1u32 << 30);
    format!("{gib:.1} GiB")
}

/// Aether's view of one MCP server (its `mcp_servers` reply).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum McpHealth {
    /// Connected; agents started from now on get its `tools`.
    Ready { tools: u32 },
    /// The last contact failed; Aether offers none of its tools.
    Failed { error: String },
    /// Configured, not contacted yet (it starts when an agent first uses it).
    Unknown,
    /// Aether has not loaded a server by that name.
    NotLoaded,
    /// archond could not ask Aether.
    Unreachable { error: String },
}

/// `DaemonStatus::mcp`: the server's health now and the supervisor's
/// latest heal (ARCHITECTURE §16).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpStatus {
    pub health: McpHealth,
    /// When archond last refreshed the server back to ready.
    pub last_heal: Option<Timestamp>,
    /// Refreshes that failed in a row since the last one that worked.
    pub failed_refreshes: u32,
    /// Restarts of the orchestrator, owed once the server is ready again,
    /// that failed in a row. `0` from an archond that predates the count.
    #[serde(default)]
    pub failed_relinks: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum AetherLinkStatus {
    Connected {
        daemon_version: String,
        protocol_version: u32,
        instance_id: String,
    },
    Disconnected {
        error: String,
        retry_at: Option<Timestamp>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OrchestratorInfo {
    pub agent: crate::aether_ids::AetherAgentId,
    pub model: ModelRef,
    pub effort: Effort,
    /// Aether's agent status kind, verbatim (`ready`, `running`, ...).
    pub status: String,
    pub restarts: u32,
    /// `0` from an archond that predates it.
    #[serde(default)]
    pub generation: u64,
    /// `None` when its creation was not recorded: an older Archon created it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub freshness: Option<OrchestratorFreshness>,
}

impl OrchestratorInfo {
    /// `generation 7, created by 1.2.3; prompt outdated; 3 new lessons`, as
    /// `status`, `doctor` and `queue_status` show it.
    pub fn describe_freshness(&self) -> String {
        let Some(fresh) = &self.freshness else {
            return format!(
                "generation {}, created before Archon recorded its prompt; a restart starts \
                 one it tracks",
                self.generation
            );
        };
        assert!(
            !fresh.created_by.is_empty(),
            "a recorded creator has a version"
        );
        let mut line = format!(
            "generation {}, created by {}",
            self.generation, fresh.created_by
        );
        match fresh.prompt_outdated {
            Some(true) => line.push_str("; prompt outdated"),
            Some(false) => {}
            None => line.push_str("; prompt unknown (the instructions file cannot be read)"),
        }
        if fresh.tools_outdated {
            line.push_str("; tools outdated");
        }
        if let Some(drift) = &fresh.settings_drift {
            let _ = write!(
                line,
                "; configured {}, running {}",
                drift.configured, drift.running
            );
        }
        match fresh.new_lessons {
            0 => {}
            1 => line.push_str("; 1 new lesson"),
            n => {
                let _ = write!(line, "; {n} new lessons");
            }
        }
        if !fresh.is_stale() {
            line.push_str("; up to date");
        }
        line
    }
}

/// How the running orchestrator generation compares with the one archond
/// would start now. A restart (`archon orchestrator restart`) catches it up.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OrchestratorFreshness {
    /// The Archon version that created it.
    pub created_by: String,
    /// Its instructions differ from today's; `None` when today's cannot be
    /// built, as with an unreadable `[orchestrator] instructions_file`.
    pub prompt_outdated: Option<bool>,
    /// Archon's tool set changed since it was created.
    pub tools_outdated: bool,
    /// Lessons a new generation would be given that it was not.
    pub new_lessons: u32,
    /// The configured `[orchestrator]` model and effort differ from the
    /// ones it was created with; `None` when they match or were not recorded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub settings_drift: Option<SettingsDrift>,
}

/// Model and effort as `provider/model effort`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SettingsDrift {
    pub configured: String,
    pub running: String,
}

impl OrchestratorFreshness {
    /// Whether a restart is owed, or may be when the prompt is unknown.
    pub fn is_stale(&self) -> bool {
        self.prompt_outdated != Some(false)
            || self.tools_outdated
            || self.new_lessons > 0
            || self.settings_drift.is_some()
    }
}

/// A queue item with what `archon queue list` shows about its runs. The
/// item's own fields are flattened in, so JSON readers of `Items` read it too.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ItemSummary {
    #[serde(flatten)]
    pub item: QueueItem,
    /// The stage its live run is at; `None` unless the item is running.
    pub stage: Option<String>,
    pub spent_usd: f64,
    /// The newest pull request any of its runs opened.
    pub pr_url: Option<String>,
    /// Derived operator wait; the stored queue state remains unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub waiting_on: Option<WaitingOn>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub waiting_reason: Option<WaitingReason>,
    /// The catalog pipeline a steer switched the item to; `item.pipeline`
    /// stays the one it was queued with.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub switched_to: Option<String>,
}

/// Stable read-time reason codes; these are never stored on a queue item.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WaitingReasonCode {
    AetherDisconnected,
    OperatorGate,
    DependencyFailed,
    DependencyWaiting,
    GlobalPause,
    /// An operator's upgrade drains Archon before it restarts.
    Draining,
    LowDisk,
    TotalBudget,
    DailyBudget,
    RepoPause,
    PipelinePause,
    Capacity,
    NextInLine,
    /// Dependencies recovered, but the item is still explicitly blocked.
    Blocked,
    /// Every provider that meets one of its stages' floors is out of quota.
    ProviderQuota,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WaitingReason {
    pub code: WaitingReasonCode,
    pub message: String,
    pub next_action: String,
}

/// What the current run needs from the operator, and how to provide it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WaitingOn {
    /// `proposal`, `report`, or `question`.
    pub subject: String,
    pub stage: String,
    pub question: Option<QuestionId>,
    pub prompt: String,
    pub next_action: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ItemDetail {
    pub item: QueueItem,
    pub runs: Vec<PipelineRun>,
    /// Stage runs of every run, oldest first.
    pub stages: Vec<StageRun>,
    pub spent_usd: f64,
    /// Operator gates of every run, oldest first.
    #[serde(default)]
    pub gates: Vec<OperatorGate>,
    /// How the cost of each stage run that delegated splits.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub delegations: BTreeMap<StageRunId, StageDelegation>,
    /// The default effort of each plan key, to see which choices exceed it.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub default_efforts: BTreeMap<String, Effort>,
    /// Every steer of the item, oldest first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub steers: Vec<crate::steer::SteerRecord>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub waiting_on: Option<WaitingOn>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub waiting_reason: Option<WaitingReason>,
    /// As in [`ItemSummary`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub switched_to: Option<String>,
    /// Requests to merge the item's pull request, oldest first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub merges: Vec<MergeRequest>,
}

/// An agent a stage agent delegated to, itself or through another.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DelegatedAgent {
    pub agent: AetherAgentId,
    /// The stage agent, or the delegated agent that delegated further.
    pub parent: AetherAgentId,
    pub model: ModelRef,
    /// `None` until Archon reads it from Aether, which it does when the
    /// stage has ended.
    pub effort: Option<Effort>,
    pub cost_usd: f64,
}

impl DelegatedAgent {
    /// `agt_… codex/astra (medium) $0.40`.
    pub fn line(&self) -> String {
        let effort = self
            .effort
            .map_or("effort unknown".to_owned(), |e| e.to_string());
        let cost = crate::cost::format_usd(self.cost_usd);
        format!("{} {} ({effort}) {cost}", self.agent, self.model)
    }
}

/// How the cost of a stage run that delegated splits; the two amounts add up
/// to the stage run's `cost_usd`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StageDelegation {
    /// What the stage agent itself was charged.
    pub direct_usd: f64,
    /// What the agents beneath it were charged, at any depth.
    pub delegated_usd: f64,
    pub agents: Vec<DelegatedAgent>,
}

impl StageDelegation {
    /// The split of `direct_usd` and what `agents` were charged.
    pub fn of(direct_usd: f64, agents: Vec<DelegatedAgent>) -> Self {
        assert!(!agents.is_empty(), "a split is of a stage that delegated");
        let delegated_usd: f64 = agents.iter().map(|agent| agent.cost_usd).sum();
        assert!(
            direct_usd >= 0.0 && delegated_usd >= 0.0,
            "the ledger holds no negative charge"
        );
        Self {
            direct_usd,
            delegated_usd,
            agents,
        }
    }

    pub fn total_usd(&self) -> f64 {
        self.direct_usd + self.delegated_usd
    }

    /// `$1.20 own + $0.80 for 2 delegated agent(s)`.
    pub fn split(&self) -> String {
        format!(
            "{} own + {} for {} delegated agent(s)",
            crate::cost::format_usd(self.direct_usd),
            crate::cost::format_usd(self.delegated_usd),
            self.agents.len()
        )
    }

    /// The split and every delegated agent on one line.
    pub fn line(&self) -> String {
        let agents: Vec<String> = self.agents.iter().map(DelegatedAgent::line).collect();
        format!("{}: {}", self.split(), agents.join("; "))
    }
}

/// Characters of a stage's output one `GetStage` page holds by default.
pub const STAGE_PAGE_CHARS: usize = 20_000;
/// Largest page `GetStage` accepts, so a fan-out's pages fit one message.
pub const MAX_STAGE_PAGE_CHARS: usize = 100_000;

/// Which stage runs of an item's run to show. Without `stage` it lists
/// them all; with it, the matching runs' output in full, paged.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StageQuery {
    pub item: QueueItemId,
    /// One of the item's runs; `None`: its latest. Attempts restart at 1
    /// in each run, so an older run's attempt needs this.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run: Option<PipelineRunId>,
    pub stage: Option<String>,
    /// `None`: the stage's latest attempt.
    pub attempt: Option<u32>,
    /// A fan-out slot; `None`: every slot of the attempt.
    pub slot: Option<u32>,
    /// Characters of each stage run's output to skip.
    #[serde(default)]
    pub offset: usize,
    /// Characters per page; `None`: [`STAGE_PAGE_CHARS`].
    pub limit: Option<usize>,
}

impl StageQuery {
    /// Refuses selectors without a stage and pages out of bounds, naming
    /// the argument at fault.
    pub fn check(&self) -> Result<(), String> {
        if self.stage.as_deref().is_some_and(|s| s.trim().is_empty()) {
            return Err("`stage` must not be empty; omit it to list the stage runs".into());
        }
        let stageless = [
            ("attempt", self.attempt.is_some()),
            ("slot", self.slot.is_some()),
            ("offset", self.offset > 0),
            ("limit", self.limit.is_some()),
        ];
        let picked = stageless.iter().find(|(_, given)| *given);
        if let (None, Some((key, _))) = (&self.stage, picked) {
            return Err(format!("`{key}` needs `stage`: it picks within one stage"));
        }
        if self.attempt == Some(0) {
            return Err("`attempt` counts from 1".into());
        }
        match self.limit {
            Some(limit) if !(1..=MAX_STAGE_PAGE_CHARS).contains(&limit) => Err(format!(
                "`limit` must be between 1 and {MAX_STAGE_PAGE_CHARS} characters, not {limit}"
            )),
            _ => Ok(()),
        }
    }

    pub fn page_chars(&self) -> usize {
        self.limit.unwrap_or(STAGE_PAGE_CHARS)
    }
}

/// `GetStage`'s answer: stage runs of the item's run asked for, oldest first.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StageDetail {
    pub item: QueueItemId,
    pub run: PipelineRunId,
    /// The stage asked for; `None` for the listing.
    pub stage: Option<String>,
    pub stages: Vec<StageRunView>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StageRunView {
    pub stage_id: String,
    pub attempt: u32,
    pub slot: u32,
    /// What its agent runs on now, a steer's live retune included.
    pub choice: ModelChoice,
    pub state: StageState,
    pub verdict: Option<Verdict>,
    /// What its agent and every agent that one delegated to were charged.
    pub cost_usd: f64,
    /// How `cost_usd` splits, when the stage run delegated.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delegation: Option<StageDelegation>,
    pub aether_agent: Option<AetherAgentId>,
    /// The stage's output, only when a stage was asked for.
    pub output: Option<TextPage>,
}

/// Characters `offset..offset + text.chars().count()` of a longer text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TextPage {
    pub text: String,
    pub offset: usize,
    pub total_chars: usize,
}

impl TextPage {
    /// The page of `full` starting `offset` characters in; empty past the end.
    pub fn of(full: &str, offset: usize, limit: usize) -> Self {
        assert!(limit > 0, "a page holds at least one character");
        let total_chars = full.chars().count();
        let text: String = full.chars().skip(offset).take(limit).collect();
        debug_assert!(text.chars().count() <= limit);
        Self {
            text,
            offset,
            total_chars,
        }
    }

    /// Where the next page starts, if any text is left.
    pub fn next_offset(&self) -> Option<usize> {
        let end = self.offset + self.text.chars().count();
        (end < self.total_chars).then_some(end)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReportPublished {
    pub report: ReportId,
    pub published: bool,
    /// Lavish session URL, or the file path when Lavish is unavailable.
    pub location: String,
    pub run: Option<PipelineRunId>,
}

/// Where a recorded report stands (ARCHITECTURE §14).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReportState {
    /// Only a file path was shown: Lavish is absent or failed to open it.
    Unpublished,
    /// A Lavish session is open and polled.
    Open,
    /// The review window went away. Its session can be resumed; only a
    /// report an open gate waits on asks the operator whether to.
    Disconnected,
    Ended,
    /// Polling failed repeatedly and was given up.
    Stopped,
}

impl ReportState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Unpublished => "unpublished",
            Self::Open => "open",
            Self::Disconnected => "disconnected",
            Self::Ended => "ended",
            Self::Stopped => "stopped",
        }
    }
}

/// One recorded report, as `ListReports` shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReportSummary {
    pub report: ReportId,
    pub title: String,
    pub path: std::path::PathBuf,
    /// Where it was last shown: the Lavish URL or the file path. `None` for
    /// a report recorded before locations were kept.
    pub location: Option<String>,
    pub state: ReportState,
    pub item: Option<QueueItemId>,
    /// The open "Reopen it?" question, asked only while a gate waits.
    pub reopen_question: Option<QuestionId>,
    pub updated_at: Timestamp,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    Incompatible,
    Forbidden,
    NotFound,
    InvalidArgument,
    InvalidTransition,
    Policy,
    Unavailable,
    /// archond is recovering its runs after a start; retry once it is done.
    Recovering,
    Internal,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[error("{code:?}: {message}")]
pub struct ErrorReply {
    pub code: ErrorCode,
    pub message: String,
}

impl From<&ArchonError> for ErrorReply {
    fn from(err: &ArchonError) -> Self {
        let code = match err {
            ArchonError::Invalid(_) | ArchonError::Config(_) | ArchonError::Pipeline(_) => {
                ErrorCode::InvalidArgument
            }
            ArchonError::NotFound(_) => ErrorCode::NotFound,
            ArchonError::InvalidTransition(_) => ErrorCode::InvalidTransition,
            ArchonError::Policy(_) => ErrorCode::Policy,
            ArchonError::Forbidden(_) => ErrorCode::Forbidden,
            ArchonError::Recovering(_) => ErrorCode::Recovering,
            ArchonError::Unavailable(_) | ArchonError::Aether(_) => ErrorCode::Unavailable,
            ArchonError::Store(_)
            | ArchonError::DiskFull(_)
            | ArchonError::Io { .. }
            | ArchonError::Protocol(_) => ErrorCode::Internal,
        };
        Self {
            code,
            message: err.to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::github::OperatorUsers;

    #[test]
    fn client_frames_flatten_the_command_beside_req() {
        let frame = ClientFrame {
            id: 7,
            request: Request::CancelItem {
                item: QueueItemId::new(),
                reason: None,
            },
        };
        let value = serde_json::to_value(&frame).unwrap();
        assert_eq!(value["req"], 7);
        assert_eq!(value["cmd"], "cancel_item");
        let back: ClientFrame = serde_json::from_value(value).unwrap();
        assert_eq!(back, frame);
    }

    #[test]
    fn report_states_print_as_they_are_stored() {
        use ReportState::*;
        for state in [Unpublished, Open, Disconnected, Ended, Stopped] {
            let wire = serde_json::to_value(state).unwrap();
            assert_eq!(wire, state.as_str(), "{state:?}");
            assert_eq!(serde_json::from_value::<ReportState>(wire).unwrap(), state);
        }
        let reopen = Request::ReopenReport {
            report: ReportId::new(),
        };
        let value = serde_json::to_value(&reopen).unwrap();
        assert_eq!(value["cmd"], "reopen_report");
        assert_eq!(serde_json::from_value::<Request>(value).unwrap(), reopen);
    }

    fn round_trip(patch: RepoPatch) -> (serde_json::Value, RepoPatch) {
        let frame = ClientFrame {
            id: 3,
            request: Request::RepoUpdate {
                repo: RepoRef("web".into()),
                patch,
            },
        };
        let text = serde_json::to_string(&frame).unwrap();
        let back: ClientFrame = serde_json::from_str(&text).unwrap();
        let Request::RepoUpdate { patch, .. } = back.request else {
            panic!("decoded {back:?}");
        };
        (serde_json::from_str(&text).unwrap(), patch)
    }

    #[test]
    fn every_repo_patch_field_survives_the_wire() {
        use crate::autonomy::MergeMode;
        let ceiling = AutonomyPatch {
            merge: Some(MergeMode::Auto),
            ..AutonomyPatch::default()
        };
        let patches = [
            RepoPatch::default(),
            RepoPatch {
                ship: Some(ShipMode::Local),
                ..RepoPatch::default()
            },
            RepoPatch {
                base_branch: Clearable::Clear,
                default_pipeline: Clearable::Clear,
                autonomy_max: Some(AutonomyPatch::default()),
                ..RepoPatch::default()
            },
            RepoPatch {
                base_branch: Clearable::Set("main".into()),
                default_pipeline: Clearable::Set("direct".into()),
                autonomy_max: Some(ceiling),
                ..RepoPatch::default()
            },
        ];
        for patch in patches {
            let (_, back) = round_trip(patch.clone());
            assert_eq!(back, patch);
        }
        let (value, _) = round_trip(RepoPatch {
            base_branch: Clearable::Clear,
            ..RepoPatch::default()
        });
        assert_eq!(value["patch"]["base_branch"], "clear");
        assert_eq!(value["patch"]["default_pipeline"], "keep");
    }

    #[test]
    fn a_missing_clearable_keeps_and_a_null_one_is_refused() {
        let missing = r#"{"req":1,"cmd":"repo_update","repo":"web","patch":{}}"#;
        let frame: ClientFrame = serde_json::from_str(missing).unwrap();
        let Request::RepoUpdate { patch, .. } = frame.request else {
            panic!("decoded {frame:?}");
        };
        assert_eq!(patch, RepoPatch::default());
        let null = r#"{"req":1,"cmd":"repo_update","repo":"web","patch":{"base_branch":null}}"#;
        assert!(serde_json::from_str::<ClientFrame>(null).is_err());
        assert_eq!(Clearable::Keep.apply(Some(1)), Some(1));
        assert_eq!(Clearable::Clear.apply(Some(1)), None);
        assert_eq!(Clearable::Set(2).apply(None), Some(2));
    }

    #[test]
    fn replies_are_adjacently_tagged_inside_a_result() {
        let frame = ServerFrame::Reply {
            id: 1,
            result: Ok(Reply::Ok),
        };
        let value = serde_json::to_value(&frame).unwrap();
        assert_eq!(value["frame"], "reply");
        assert_eq!(value["result"]["Ok"]["reply"], "ok");
        let err = ServerFrame::Reply {
            id: 2,
            result: Err(ErrorReply::from(&ArchonError::Policy("no".into()))),
        };
        let value = serde_json::to_value(&err).unwrap();
        assert_eq!(value["result"]["Err"]["code"], "policy");
        let refused = ErrorReply::from(&ArchonError::Forbidden("say".into()));
        assert_eq!(refused.code, ErrorCode::Forbidden);
    }

    #[test]
    fn item_summaries_carry_the_items_own_fields_at_the_top() {
        let item = QueueItem {
            onto: None,
            id: QueueItemId::new(),
            repo: None,
            pipeline: "ask".into(),
            title: "why".into(),
            brief: "why?".into(),
            priority: Priority::Normal,
            dependencies: vec![],
            tags: vec![],
            model_plan: crate::model::ModelPlan::default(),
            model_reasons: None,
            budget_usd: None,
            stage_timeouts: BTreeMap::new(),
            autonomy: Default::default(),
            origin: None,
            fresh_retries: 0,
            routed_provider: None,
            provider_switch: None,
            initiative: None,
            risk: None,
            state: crate::queue::QueueState::Queued,
            created_at: crate::now(),
            updated_at: crate::now(),
        };
        let summary = ItemSummary {
            waiting_on: None,
            waiting_reason: None,
            item: item.clone(),
            stage: Some("review".into()),
            spent_usd: 1.5,
            pr_url: None,
            switched_to: None,
        };
        let reply = Reply::ItemSummaries(vec![summary.clone()]);
        let value = serde_json::to_value(&reply).unwrap();
        let row = &value["data"][0];
        assert_eq!(row["id"], item.id.as_str());
        assert_eq!(row["state"]["state"], "queued");
        assert_eq!(
            (&row["stage"], &row["spent_usd"]),
            (&"review".into(), &1.5.into())
        );
        assert_eq!(serde_json::from_value::<Reply>(value).unwrap(), reply);
        let resolve = Request::ResolveItem {
            prefix: "01k2".into(),
        };
        let value = serde_json::to_value(&resolve).unwrap();
        assert_eq!(value["cmd"], "resolve_item");
        assert!(!resolve.is_operator_only());
    }

    #[test]
    fn free_space_reads_in_gib() {
        assert_eq!(gib(0), "0.0 GiB");
        assert_eq!(gib(10 << 30), "10.0 GiB");
        assert_eq!(gib((3 << 30) / 2), "1.5 GiB");
    }

    #[test]
    fn a_call_forwarded_for_an_agent_keeps_its_requests_class_and_shape() {
        let caller = AetherAgentId::parse("agt_0001").unwrap();
        let wrap = |request: Request| Request::ForAgent {
            caller: caller.clone(),
            request: Box::new(request),
        };
        assert!(wrap(Request::Status(StatusQuery::default())).is_read_only());
        assert!(
            !wrap(Request::Say {
                text: "hi".into(),
                images: Vec::new(),
            })
            .is_read_only()
        );
        assert!(
            wrap(Request::Say {
                text: "hi".into(),
                images: Vec::new(),
            })
            .is_operator_only()
        );
        let pause = wrap(Request::Pause(PauseTarget::Global { hard: false }));
        assert!(!pause.is_read_only() && !pause.is_operator_only());

        let frame = ClientFrame {
            id: 3,
            request: pause.clone(),
        };
        let value = serde_json::to_value(&frame).unwrap();
        assert_eq!(value["cmd"], "for_agent");
        assert_eq!(value["caller"], "agt_0001");
        assert_eq!(value["request"]["cmd"], "pause");
        let back: ClientFrame = serde_json::from_value(value).unwrap();
        assert_eq!(back.request, pause);
    }

    #[test]
    fn a_first_status_page_keeps_the_bare_status_command() {
        let first = Request::Status(StatusQuery::default());
        assert_eq!(
            serde_json::to_value(&first).unwrap(),
            serde_json::json!({"cmd": "status"})
        );
        let bare: Request = serde_json::from_value(serde_json::json!({"cmd": "status"})).unwrap();
        assert_eq!(bare, first);
        let later = Request::Status(StatusQuery {
            waiting_offset: 50,
            waiting_limit: Some(10),
        });
        let value = serde_json::to_value(&later).unwrap();
        assert_eq!(
            value,
            serde_json::json!({"cmd": "status", "waiting_offset": 50, "waiting_limit": 10})
        );
        assert_eq!(serde_json::from_value::<Request>(value).unwrap(), later);
    }

    #[test]
    fn operator_only_requests_are_flagged() {
        assert!(
            Request::Say {
                text: "hi".into(),
                images: Vec::new(),
            }
            .is_operator_only()
        );
        assert!(!Request::Status(StatusQuery::default()).is_operator_only());
        let edit = Request::UpdateLesson {
            lesson: LessonId::new(),
            patch: LessonPatch::default(),
        };
        assert!(edit.is_operator_only() && !edit.is_read_only());
        let merge = RepoAdd {
            path: "/r".into(),
            name: None,
            ship: None,
            base_branch: None,
            default_pipeline: None,
            auto_merge: true,
            github_watch: false,
        };
        assert!(Request::RepoAdd(merge.clone()).is_operator_only());
        let watch = RepoAdd {
            auto_merge: false,
            github_watch: true,
            ..merge
        };
        assert!(Request::RepoAdd(watch.clone()).is_operator_only());
        let plain = RepoAdd {
            github_watch: false,
            ..watch
        };
        assert!(!Request::RepoAdd(plain).is_operator_only());
        let upgrade = Request::Upgrade(crate::ops::UpgradeRequest {
            archon: Some(crate::ops::UpgradeTarget::Main),
            aether: None,
            drain_secs: 60,
            requeue: false,
            keep_aether: false,
        });
        assert!(upgrade.is_operator_only() && !upgrade.is_read_only());
        assert!(Request::CheckUpdates.is_operator_only());
        for operator_users in [None, Some(OperatorUsers::parse(&["octocat"]).unwrap())] {
            let change = WatcherChange::Operators { operator_users };
            let request = Request::ConfigureGithubWatcher { change };
            assert!(request.is_operator_only(), "{request:?}");
        }
    }

    #[test]
    fn adaptive_requests_keep_their_names_and_only_the_lookups_read() {
        let item = QueueItemId::new();
        let steer = Request::SteerItem {
            item: item.clone(),
            steer: Steer {
                note: Some("Use the v2 client.".into()),
                ..Steer::default()
            },
        };
        let wire = serde_json::to_value(&steer).unwrap();
        assert_eq!(wire["cmd"], "steer_item");
        assert_eq!(wire["steer"]["note"], "Use the v2 client.");
        assert_eq!(serde_json::from_value::<Request>(wire).unwrap(), steer);
        let lookups = [
            Request::Lessons(LessonQuery::default()),
            Request::LessonsWhy { item },
            Request::LessonStats { repo: None },
            Request::GetInitiative {
                initiative: InitiativeId::new(),
            },
        ];
        for request in lookups {
            assert!(request.is_read_only() && !request.is_operator_only());
        }
        let append = Request::UpdateInitiative {
            initiative: InitiativeId::new(),
            change: ContractChange::Append("Also rename the column.".into()),
        };
        let wire = serde_json::to_value(&append).unwrap();
        assert_eq!(wire["contract_append"], "Also rename the column.");
        assert_eq!(serde_json::from_value::<Request>(wire).unwrap(), append);
        let replace = serde_json::json!({
            "cmd": "update_initiative", "initiative": InitiativeId::new(), "contract": "New."
        });
        let Request::UpdateInitiative { change, .. } = serde_json::from_value(replace).unwrap()
        else {
            panic!("an update");
        };
        assert_eq!(change, ContractChange::Replace("New.".into()));
        let draft = Request::RecordLesson(LessonDraft {
            kind: crate::lesson::LessonKind::Pitfall,
            text: "CI needs `make lint`.".into(),
            repo: None,
            pipeline: None,
            item: None,
        });
        assert_eq!(
            serde_json::to_value(&draft).unwrap()["cmd"],
            "record_lesson"
        );
        for request in [steer, append, draft] {
            assert!(!request.is_read_only() && !request.is_operator_only());
        }
    }

    #[test]
    fn learning_mutations_are_the_operators_and_why_only_reads() {
        let feedback = FeedbackId::new();
        let correct = Request::LearningCorrect(CorrectionRequest {
            item: QueueItemId::new(),
            run: PipelineRunId::new(),
            stage: "implement".into(),
            cause: crate::outcome::CorrectionCause::Quality,
            reason: "Ignored the identity rule".into(),
            forget_memory: None,
        });
        let wire = serde_json::to_value(&correct).unwrap();
        assert_eq!(wire["cmd"], "learning_correct");
        assert_eq!(wire["cause"], "quality");
        assert_eq!(serde_json::from_value::<Request>(wire).unwrap(), correct);
        let mutations = [
            correct,
            Request::LearningUndo {
                feedback: feedback.clone(),
            },
            Request::LearningRestoreMemory {
                feedback: feedback.clone(),
            },
            Request::LearningRetryMemory { feedback },
        ];
        for request in mutations {
            assert!(
                request.is_operator_only() && !request.is_read_only(),
                "{request:?}"
            );
        }
        let why = Request::LearningWhy {
            item: QueueItemId::new(),
        };
        assert!(why.is_read_only() && !why.is_operator_only());
        let stray = serde_json::json!({
            "cmd": "learning_correct", "item": QueueItemId::new(), "run": PipelineRunId::new(),
            "stage": "implement", "cause": "quality", "reason": "r", "blame": "reviewer"
        });
        assert!(serde_json::from_value::<Request>(stray).is_err());
    }

    fn stage_query(stage: Option<&str>) -> StageQuery {
        StageQuery {
            item: QueueItemId::new(),
            run: None,
            stage: stage.map(str::to_owned),
            attempt: None,
            slot: None,
            offset: 0,
            limit: None,
        }
    }

    #[test]
    fn get_stage_and_its_detail_survive_the_wire() {
        let query = StageQuery {
            run: Some(PipelineRunId::new()),
            attempt: Some(2),
            slot: Some(1),
            offset: 40,
            limit: Some(500),
            ..stage_query(Some("review"))
        };
        let frame = ClientFrame {
            id: 9,
            request: Request::GetStage(query.clone()),
        };
        let value = serde_json::to_value(&frame).unwrap();
        assert_eq!(value["cmd"], "get_stage");
        assert_eq!(value["stage"], "review");
        assert_eq!(value["run"], query.run.as_ref().unwrap().to_string());
        assert_eq!(serde_json::from_value::<ClientFrame>(value).unwrap(), frame);
        let bare = format!(r#"{{"req":1,"cmd":"get_stage","item":"{}"}}"#, query.item);
        let ClientFrame { request, .. } = serde_json::from_str(&bare).unwrap();
        let listing = StageQuery {
            item: query.item.clone(),
            ..stage_query(None)
        };
        assert_eq!(request, Request::GetStage(listing));
        assert!(!request.is_operator_only(), "the orchestrator reads stages");

        let view = StageRunView {
            stage_id: "review".into(),
            attempt: 2,
            slot: 1,
            choice: ModelChoice {
                model: "codex/gpt-6-sol".parse().unwrap(),
                effort: Effort::High,
                tier: None,
            },
            state: StageState::Completed,
            verdict: Some(Verdict::Fail),
            cost_usd: 0.5,
            delegation: None,
            aether_agent: None,
            output: Some(TextPage::of("summary: findings", 9, 500)),
        };
        let reply = Reply::StageDetail(Box::new(StageDetail {
            item: query.item.clone(),
            run: PipelineRunId::new(),
            stage: Some("review".into()),
            stages: vec![view],
        }));
        let value = serde_json::to_value(&reply).unwrap();
        assert_eq!(value["reply"], "stage_detail");
        assert_eq!(value["data"]["stages"][0]["output"]["text"], "findings");
        assert_eq!(serde_json::from_value::<Reply>(value).unwrap(), reply);
    }

    #[test]
    fn stage_queries_refuse_picks_without_a_stage_and_bad_pages() {
        assert_eq!(stage_query(None).check(), Ok(()));
        assert_eq!(stage_query(Some("review")).check(), Ok(()));
        let picks = [
            StageQuery {
                attempt: Some(1),
                ..stage_query(None)
            },
            StageQuery {
                slot: Some(0),
                ..stage_query(None)
            },
            StageQuery {
                offset: 10,
                ..stage_query(None)
            },
        ];
        for query in picks {
            let why = query.check().unwrap_err();
            assert!(why.contains("needs `stage`"), "{why}");
        }
        let zero = StageQuery {
            attempt: Some(0),
            ..stage_query(Some("review"))
        };
        assert!(zero.check().unwrap_err().contains("`attempt`"));
        for limit in [0, MAX_STAGE_PAGE_CHARS + 1] {
            let query = StageQuery {
                limit: Some(limit),
                ..stage_query(Some("review"))
            };
            assert!(query.check().unwrap_err().contains("`limit`"), "{limit}");
        }
        assert!(stage_query(Some(" ")).check().is_err());
        assert_eq!(stage_query(None).page_chars(), STAGE_PAGE_CHARS);
    }

    #[test]
    fn text_pages_count_characters_and_name_the_next_one() {
        let full = "héllo wörld";
        let first = TextPage::of(full, 0, 5);
        assert_eq!(first.text, "héllo");
        assert_eq!((first.total_chars, first.next_offset()), (11, Some(5)));
        let last = TextPage::of(full, 5, 100);
        assert_eq!(last.text, " wörld");
        assert_eq!(last.next_offset(), None);
        let past = TextPage::of(full, 50, 5);
        assert!(past.text.is_empty() && past.next_offset().is_none());
    }

    #[test]
    fn answers_and_hard_pauses_are_operator_only() {
        let answer = Request::Answer {
            chat: false,
            question: QuestionId::new(),
            text: "yes".into(),
        };
        assert!(answer.is_operator_only());
        let hard = PauseTarget::Global { hard: true };
        assert!(Request::Pause(hard.clone()).is_operator_only());
        assert!(Request::Resume(hard).is_operator_only());
        let soft = PauseTarget::Global { hard: false };
        assert!(!Request::Pause(soft.clone()).is_operator_only());
        assert!(!Request::Resume(soft).is_operator_only());
        let repo = PauseTarget::Repo {
            name: RepoRef("web".into()),
        };
        assert!(!Request::Pause(repo).is_operator_only());
    }
}
