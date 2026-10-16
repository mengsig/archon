//! The command-line surface. `archon` with no subcommand opens the terminal
//! UI, or the line chat where the terminal UI cannot run (`cmd::bare`).

use std::ffi::OsString;
use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};

#[derive(Debug, Parser)]
#[command(
    name = "archon",
    version,
    about = "Hierarchical orchestration on top of Aether",
    after_help = "With no command, `archon` opens the terminal UI (`archon tui`). \
                  `archon chat` is the line chat."
)]
pub struct Cli {
    /// Print machine-readable JSON instead of text. A bare `archon`, chat,
    /// tui, aether, mcp and a foreground `daemon run` refuse it.
    #[arg(long, global = true)]
    pub json: bool,
    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Debug, Default, Args)]
pub struct TuiArgs {
    /// Start with Aether's standard transcript instead of calm (dedicated mode).
    #[arg(long)]
    pub normal: bool,
    /// Aether theme for this run; otherwise use its environment and ui.toml.
    #[arg(long)]
    pub theme: Option<String>,
    /// Use Aether's ASCII UI glyphs; message text is unchanged.
    #[arg(long)]
    pub ascii: bool,
    /// Keep Aether's pulsing and shimmer still.
    #[arg(long)]
    pub reduced_motion: bool,
    /// Use Aether's line transcript instead of its screen UI.
    #[arg(long)]
    pub plain: bool,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Inspect, record and manage learned advice.
    Lessons(crate::cmd::lessons::LessonsArgs),
    /// Correct one stage of a finished run and curate the memory that misled it.
    #[command(subcommand)]
    Learning(crate::cmd::learning::LearningCommand),
    /// Open the quiet line chat with the orchestrator.
    Chat {
        /// Also show progress notifications.
        #[arg(long)]
        progress: bool,
    },
    /// Send one message to the orchestrator and exit.
    Say {
        /// The message; every word is kept.
        text: Vec<String>,
    },
    /// Answer an open question by its number or id.
    Answer {
        /// The question's number as `archon questions` and the chat show it
        /// (2 or Q2), or its q_… id. Numbers are permanent and never reused,
        /// so a question answered elsewhere since is refused, not replaced.
        question: String,
        /// The answer; a bare option number picks that option.
        text: Vec<String>,
    },
    /// List open questions.
    Questions,
    /// Merge a succeeded item's pull request now. archond merges only an
    /// open, mergeable pull request at the reviewed commit, on its base,
    /// with green checks; never with --admin or --auto.
    Merge {
        /// The item's qi_… id, or enough of its start to name one item.
        item: String,
        /// Merge although checks are failing, pending or missing.
        #[arg(long)]
        ignore_checks: bool,
    },
    /// Daemon, orchestrator, caps, spend and Aether link status. Never
    /// starts Archon or Aether.
    Status {
        /// Kept for older scripts: `status` never starts anything.
        #[arg(long, hide = true)]
        no_start: bool,
        /// Also ask GitHub for the latest archon and aether releases, which
        /// archond never does by itself.
        #[arg(long)]
        check_updates: bool,
    },
    /// Spend over a range of days, grouped, with a total.
    ///
    /// Amounts are what the budgets count: reported cost, plus API-equivalent
    /// estimates unless `limits.count_estimated_cost` is off. Spend is kept by
    /// local calendar day, the day the daily budget counts.
    Cost {
        /// First day: a date like 2026-09-01, or days back like 7d (the last
        /// 7 days with today) or 2w. The last 7 days by default.
        #[arg(long)]
        since: Option<String>,
        /// Last day: a date, or days back (1d ends yesterday). Today by default.
        #[arg(long)]
        until: Option<String>,
        /// What to group spend by; `-` gathers spend with no such owner, such
        /// as the orchestrator's.
        #[arg(long, value_enum, default_value_t = CostBy::Day)]
        by: CostBy,
    },
    /// Recorded stage cost, duration and verdict evidence (no recommendations).
    ModelEvidence {
        /// Only attempts in this repository (name or repo_… id).
        #[arg(long)]
        repo: Option<String>,
        /// Only attempts in this pipeline.
        #[arg(long)]
        pipeline: Option<String>,
        /// Only attempts for this stage id.
        #[arg(long)]
        stage: Option<String>,
        /// First local day of attempt starts: YYYY-MM-DD, 7d or 2w; default 30d.
        #[arg(long)]
        since: Option<String>,
    },
    /// Everything that happened (the feed with progress), or what one item
    /// or kind of entry said.
    Log {
        /// How many of the newest matching entries to show.
        #[arg(long, default_value_t = 100)]
        limit: usize,
        /// Only entries about this item: its qi_… id or the id's start.
        #[arg(long)]
        item: Option<String>,
        /// Only entries of this kind; repeat for several.
        #[arg(long, value_enum)]
        kind: Vec<LogKind>,
        /// Keep printing new entries as they arrive, until interrupted.
        /// With --json, one JSON object per line.
        #[arg(long)]
        follow: bool,
    },
    /// Register and configure repositories.
    #[command(subcommand)]
    Repo(RepoCommand),
    /// Inspect and change queue items and runtime caps.
    #[command(subcommand)]
    Queue(QueueCommand),
    /// Coordinated requests across repositories.
    Initiatives {
        #[command(subcommand)]
        command: Option<InitiativeCommand>,
    },
    /// List and validate pipelines.
    #[command(subcommand)]
    Pipelines(PipelineCommand),
    /// Autonomy defaults and ceilings, and the operator's standing directives.
    Autonomy(AutonomyArgs),
    /// Saved operator overrides and effective values with their sources.
    Settings {
        /// Repository name or id; omit for global settings.
        #[arg(long)]
        repo: Option<String>,
        /// Replace this scope's overlay with a JSON object.
        #[arg(long, conflicts_with = "reset")]
        set: Option<String>,
        /// Clear this scope's overlay and restore inheritance.
        #[arg(long)]
        reset: bool,
    },
    /// Published reports with their state and where they were shown.
    ///
    /// A disconnected or ended report can be reopened, so it is kept while its
    /// HTML file exists. Any other report, and one whose file is gone, is
    /// forgotten 7 days after its last change.
    Reports {
        #[command(subcommand)]
        command: Option<ReportCommand>,
    },
    /// Route `@archon` on GitHub issues and pull requests to the orchestrator,
    /// through Archon's own Aether's watcher (docs/github-watcher.md).
    GithubWatcher {
        #[command(subcommand)]
        command: Option<GithubWatcherCommand>,
    },
    /// Hold dispatch globally, or for one repo or pipeline.
    Pause(PauseArgs),
    /// Lift a pause.
    Resume(PauseArgs),
    /// Set up Archon's own Aether (dedicated mode), or register Archon's MCP
    /// server in the operator's Aether config (shared mode).
    Setup {
        /// Print the MCP block instead of writing anything.
        #[arg(long)]
        print: bool,
        /// Re-seed the dedicated instance's config.toml from the operator's.
        #[arg(long, conflicts_with = "print")]
        resync: bool,
    },
    /// Check Aether, the MCP registration, lavish, git and the config.
    Doctor,
    /// Open the terminal UI: Aether's TUI on Archon's main orchestrator
    /// (what a bare `archon` opens).
    Tui(TuiArgs),
    /// Run the `aether` CLI against Archon's own Aether, e.g. `archon aether agents`.
    #[command(disable_help_flag = true)]
    Aether {
        /// Arguments for `aether`, passed through untouched (`--json` too).
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<OsString>,
    },
    /// Delete finished runs' build caches and remove their old worktrees.
    Gc {
        /// Remove worktrees whose runs finished this long ago, e.g. 12h, 7d
        /// or 2w; `worktrees.gc_after` (7d) by default.
        #[arg(long)]
        older_than: Option<String>,
        /// Also remove worktrees with uncommitted changes.
        #[arg(long)]
        force: bool,
        /// Collect without archond, which must not be running: for a disk
        /// too full to start it. Reads the store without writing to it.
        #[arg(long)]
        offline: bool,
    },
    /// Run, start, stop or check archond.
    #[command(subcommand)]
    Daemon(DaemonCommand),
    /// Serve MCP on stdio (spawned by Aether; not for interactive use).
    Mcp,
    /// The orchestrator agent's status, or restart it.
    #[command(subcommand)]
    Orchestrator(OrchestratorCommand),
    /// Install the latest released archon over this one.
    ///
    /// Downloads the release for this machine, verifies its SHA-256 and
    /// replaces this binary in place. Restarts nothing: a running archond
    /// keeps the old version until you restart it. Exit 0: up to date or
    /// updated; 10 with --check: a newer release exists; 1: error.
    #[command(after_help = "Example: archon update --check || archon update")]
    Update(UpdateArgs),
}

#[derive(Debug, Default, Args)]
pub struct UpdateArgs {
    /// Only report whether a newer release exists (exit 10 when one does).
    #[arg(long, conflicts_with = "version")]
    pub check: bool,
    /// Install this release instead of the latest, e.g. v1.2.3; an older one
    /// is a downgrade.
    #[arg(long, value_name = "VERSION")]
    pub version: Option<String>,
    /// Print the Aether protocols this archon speaks, for the installer.
    #[arg(long, conflicts_with_all = ["check", "version"])]
    pub requirements: bool,
}

#[derive(Debug, Subcommand)]
pub enum RepoCommand {
    /// Register a git repository by the path of its top level.
    Add {
        /// The repository's top-level directory.
        path: PathBuf,
        /// Name to refer to it by; defaults to the directory name.
        #[arg(long)]
        name: Option<String>,
        /// How finished work ships; defaults to .archon/config.toml, else pr.
        #[arg(long, value_enum)]
        ship: Option<Ship>,
        /// Branch worktrees start from; defaults to the current HEAD.
        #[arg(long)]
        base: Option<String>,
        /// Pipeline used when a request names none.
        #[arg(long)]
        pipeline: Option<String>,
        /// Allow merge=auto for this repository (its autonomy ceiling).
        #[arg(long)]
        auto_merge: bool,
        /// Also watch the repository's GitHub origin for `@archon`, as
        /// `archon github-watcher add` does (dedicated mode only).
        #[arg(long)]
        github: bool,
    },
    /// List registered repositories.
    List,
    /// Forget a repository; its files are untouched.
    Remove {
        /// The repository's name or repo_… id.
        repo: String,
    },
    /// Change a repository's settings and record its git identity again;
    /// with no option, only the identity.
    Set {
        /// The repository's name or repo_… id.
        repo: String,
        /// How finished work ships.
        #[arg(long, value_enum)]
        ship: Option<Ship>,
        /// Base branch; an empty value clears it.
        #[arg(long)]
        base: Option<String>,
        /// Default pipeline; an empty value clears it.
        #[arg(long)]
        pipeline: Option<String>,
        /// Autonomy ceiling as knob=value pairs, e.g. `review=none,merge=auto`.
        /// Replaces the whole ceiling; an empty value clears it.
        #[arg(long, conflicts_with = "auto_merge")]
        autonomy_max: Option<String>,
        /// Only the ceiling's merge knob: true allows merge=auto, false caps it
        /// at manual.
        #[arg(long)]
        auto_merge: Option<bool>,
        /// Whether the orchestrator merges on your plain request without a
        /// confirmation question; independent of the ceiling.
        #[arg(long)]
        grant_merge: Option<bool>,
        /// What its pull requests attribute: `item-id` ends the body with the
        /// `Archon item:` line; `none` omits it and refuses attribution lines.
        #[arg(long)]
        attribution: Option<archon_core::repo::Attribution>,
        /// Verification tiers over the repository's `.archon/repo.toml`.
        #[command(flatten)]
        tiers: TierArgs,
    },
}

/// `archon repo set`'s verification tiers; an empty value clears one.
#[derive(Debug, Clone, Default, Args)]
pub struct TierArgs {
    /// Branch items fork from and deliver to, created from the release
    /// branch when missing; unset uses the base branch.
    #[arg(long)]
    pub integration_branch: Option<String>,
    /// Branch the integration branch is promoted into (default `main`).
    #[arg(long)]
    pub release_branch: Option<String>,
    /// Command every item runs before it commits.
    #[arg(long)]
    pub fast_check: Option<String>,
    /// The whole suite, which items never run (e.g. `make ci`).
    #[arg(long)]
    pub full_check: Option<String>,
}

#[derive(Debug, Args)]
#[command(args_conflicts_with_subcommands = true)]
pub struct AutonomyArgs {
    /// Show the defaults and ceiling for this repository.
    #[arg(long)]
    pub repo: Option<String>,
    #[command(subcommand)]
    pub command: Option<AutonomyCommand>,
}

#[derive(Debug, Subcommand)]
pub enum AutonomyCommand {
    /// Record a standing directive the orchestrator follows, within the ceiling.
    Direct {
        /// The directive, in plain words.
        #[arg(required = true)]
        text: Vec<String>,
        /// Expire after this many hours (1 to 720); default: until cleared.
        #[arg(long)]
        hours: Option<u32>,
    },
    /// Clear one directive by id, or every directive with --all.
    Clear {
        /// The directive's dir_… id, as `archon autonomy` lists it.
        #[arg(required_unless_present = "all", conflicts_with = "all")]
        directive: Option<String>,
        /// Clear every directive.
        #[arg(long)]
        all: bool,
    },
}

#[derive(Debug, Subcommand)]
pub enum GithubWatcherCommand {
    /// The watcher's settings, operator logins, repositories, targets,
    /// allowlists and health (default).
    #[command(visible_alias = "list")]
    Status,
    /// Watch a repository for `@archon`, and switch the watcher on.
    Add {
        /// The repository as owner/name.
        repository: String,
        /// Only this GitHub login may dispatch (repeat, or separate with
        /// commas); each still needs write access. Omitted keeps a watched
        /// repository's list.
        #[arg(long, value_name = "LOGIN", value_delimiter = ',')]
        allow: Vec<String>,
    },
    /// Replace who may dispatch for a watched repository, or `--clear` to
    /// let every collaborator with write access dispatch again.
    Allow {
        /// The repository as owner/name.
        repository: String,
        /// GitHub logins that alone may dispatch (space or comma separated);
        /// each still needs write access.
        #[arg(
            value_name = "LOGIN",
            value_delimiter = ',',
            required_unless_present = "clear",
            conflicts_with = "clear"
        )]
        logins: Vec<String>,
        /// Remove the allowlist.
        #[arg(long)]
        clear: bool,
    },
    /// Replace the GitHub logins whose own requests the orchestrator takes
    /// as your instructions, or `--clear` so that none does (the default).
    ///
    /// A listed account has your authority over Archon wherever it may
    /// dispatch: whoever controls it, or a token that posts as it, can direct
    /// Archon as you can from this terminal. List only accounts you would
    /// hand your terminal to. A repository's allowlist that leaves a login
    /// out keeps its requests out of that repository altogether. Needs an
    /// aetherd that speaks Aether protocol 17, and is refused while the
    /// orchestrator that takes GitHub requests runs an older Archon's
    /// instructions: run `archon orchestrator restart` first. `--clear`
    /// never waits for it.
    Operators {
        /// GitHub logins that speak as the operator (space or comma
        /// separated); each still needs write access where it asks.
        #[arg(
            value_name = "LOGIN",
            value_delimiter = ',',
            required_unless_present = "clear",
            conflicts_with = "clear"
        )]
        logins: Vec<String>,
        /// Remove the list: every GitHub request is an external request.
        #[arg(long)]
        clear: bool,
    },
    /// Stop watching a repository.
    Remove {
        /// The repository as owner/name.
        repository: String,
    },
    /// Switch the watcher on, or one watched repository.
    Enable {
        /// A watched repository as owner/name; default: the whole watcher.
        repository: Option<String>,
    },
    /// Switch the watcher off, or one watched repository.
    Disable {
        /// A watched repository as owner/name; default: the whole watcher.
        repository: Option<String>,
    },
    /// Change what the watcher matches and how often it polls.
    #[command(group(
        clap::ArgGroup::new("setting")
            .required(true)
            .multiple(true)
            .args(["mention_regex", "label_regex", "poll_interval"])
    ))]
    Configure {
        /// Regex a comment must match to be a request (Rust regex syntax).
        #[arg(long)]
        mention_regex: Option<String>,
        /// Regex a newly added label must match to be a request.
        #[arg(long)]
        label_regex: Option<String>,
        /// Seconds between polls of each repository (60 to 86400).
        #[arg(long)]
        poll_interval: Option<u64>,
    },
}

/// What an `archon log` entry is: who spoke, or the notice's kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum LogKind {
    /// The operator's chat lines.
    Operator,
    /// The orchestrator's replies.
    Orchestrator,
    /// Questions asked, and the notices that announce them.
    Question,
    /// Answers to questions.
    Answer,
    /// Choices the orchestrator made.
    Decision,
    /// Work that cannot go on without the operator.
    Blocker,
    /// Items and runs that finished.
    Outcome,
    /// Progress notices.
    Progress,
    /// Published reports.
    Report,
    /// archond's own notices.
    System,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum CostBy {
    Day,
    Repo,
    Pipeline,
    Model,
    Provider,
    Item,
    Stage,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Budgets {
    On,
    Off,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Ship {
    Pr,
    Local,
}

#[derive(Debug, Subcommand)]
pub enum QueueCommand {
    /// Enqueue work using the daemon's model and autonomy policy checks.
    #[command(
        after_help = "Example: archon queue add --title 'Explain caching' --pipeline ask --brief-file brief.md --model-plan-file plan.json"
    )]
    Add(Box<QueueAddArgs>),
    /// Wait on a running daemon until every item succeeds, fails or is cancelled.
    /// Exit 0: all succeeded; 3: any failed/cancelled; 4: timeout; 1: error.
    #[command(after_help = "Example: archon queue wait qi_… qi_… --timeout 30m --json")]
    Wait {
        /// Item ids or unambiguous prefixes; at least one is required.
        #[arg(required = true)]
        items: Vec<String>,
        /// Maximum total wait, including connection/requests: Ns, Nm, Nh, Nd or Nw.
        #[arg(long, default_value = "30m")]
        timeout: String,
    },
    /// Queue items, newest first: state, stage, age, cost and pull request.
    List {
        /// Only items in this state; repeat for several.
        #[arg(long)]
        state: Vec<String>,
        /// Only items for this repository (name or repo_… id).
        #[arg(long)]
        repo: Option<String>,
        /// At most this many items (default 50).
        #[arg(long)]
        limit: Option<usize>,
    },
    /// One item with its runs, stages, stage summaries and pull request.
    Show {
        /// The item's qi_… id, or enough of its start to name one item.
        item: String,
        /// Show each stage's whole summary instead of its first lines.
        #[arg(long)]
        full: bool,
    },
    /// Cancel an item.
    Cancel {
        /// The item's qi_… id, or enough of its start to name one item.
        item: String,
        /// Why, recorded with the cancellation.
        #[arg(long)]
        reason: Option<String>,
    },
    /// Queue a finished item again, in its worktree and branch unless `--fresh`;
    /// a succeeded one delivers into its pull request and corrects the run.
    Retry {
        /// The item's qi_… id, or enough of its start to name one item.
        item: String,
        /// Start again from the base branch's head on a new branch; the old
        /// worktree is moved aside with everything in it and the old branch
        /// is kept, as is any pull request open from it. Worktree gc deletes
        /// the set-aside worktree, even with uncommitted changes,
        /// `worktrees.gc_after` (default 7d) after the item's last run.
        #[arg(long)]
        fresh: bool,
        /// Guidance appended to the brief under a dated "Amendment" heading,
        /// which every stage of the new run reads.
        #[arg(long, value_name = "TEXT")]
        append: Option<String>,
    },
    /// Merge a finished item's base branch, fetched from `origin`, into its
    /// branch: a new item resolves conflicts and updates the same pull request.
    Refresh {
        /// The item's qi_… id, or enough of its start to name one item.
        item: String,
    },
    /// Change an item without cancelling it: its brief, a stage's model, its
    /// budget or deadline, or the stages that have not started.
    #[command(
        after_help = "Examples:\n  archon queue steer qi_… --note 'Use the v2 client'\n  archon queue steer qi_… --stage implement --model claude-code/claude-opus-5-5 --effort high\n  archon queue steer qi_… --abort --goto implement\n  archon queue steer qi_… --insert verify --from refactor --after implement"
    )]
    Steer(Box<QueueSteerArgs>),
    /// Show the runtime caps, or change them until archond restarts (the
    /// config's `[limits]` apply again then).
    Caps {
        /// Stage agents running at once across all repositories; at least 1.
        #[arg(long)]
        max_parallel_agents: Option<u32>,
        /// Stage agents running at once per repository; 0 means no limit.
        #[arg(long)]
        max_per_repo: Option<u32>,
        /// Dollars to spend per day; 0 means no limit.
        #[arg(long)]
        budget_daily: Option<f64>,
        /// Dollars to spend in total; 0 means no limit.
        #[arg(long)]
        budget_total: Option<f64>,
        /// `off` stops enforcing every dollar budget (spend is still
        /// recorded); `on` enforces them again.
        #[arg(long, value_enum)]
        budgets: Option<Budgets>,
    },
}

/// What a run does once `--abort` stopped its stage.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum AfterAbort {
    /// Start the stage again with a new agent.
    Retry,
    /// Pass the stage over and go on with the next one.
    Skip,
}

/// One steer; archond checks it against the item, its run and the catalog.
#[derive(Debug, Args)]
pub struct QueueSteerArgs {
    /// The item's qi_… id, or enough of its start to name one item.
    pub item: String,
    /// Guidance added to the brief and sent to the agents working now.
    #[arg(long, value_name = "TEXT")]
    pub note: Option<String>,
    /// The stage --model, --effort and --abort are about; the running one
    /// when omitted.
    #[arg(long)]
    pub stage: Option<String>,
    /// The stage's model, from its agents' next turn on.
    #[arg(long, value_name = "PROVIDER/MODEL")]
    pub model: Option<archon_core::model::ModelRef>,
    /// The stage's effort: low, medium, high, x_high or max.
    #[arg(long)]
    pub effort: Option<archon_core::model::Effort>,
    /// The item's new budget in USD.
    #[arg(long, value_name = "USD")]
    pub budget_usd: Option<f64>,
    /// Minutes added to the deadline of the stage running now.
    #[arg(long, value_name = "MINUTES")]
    pub extend: Option<u32>,
    /// Stop the stage's agents now, then retry it (or --then, --goto).
    #[arg(long)]
    pub abort: bool,
    /// After --abort: retry the stage or skip it.
    #[arg(long, value_enum, requires = "abort", conflicts_with = "goto")]
    pub then: Option<AfterAbort>,
    /// After --abort: continue at this stage.
    #[arg(long, value_name = "STAGE", requires = "abort")]
    pub goto: Option<String>,
    /// Remove a stage that has not started.
    #[arg(long, value_name = "STAGE", conflicts_with_all = ["stage", "abort", "model", "effort"])]
    pub remove: Option<String>,
    /// Add this stage among those not started.
    #[arg(long, value_name = "STAGE", conflicts_with = "switch")]
    pub insert: Option<String>,
    /// The pipeline --insert copies the stage from; the item's own when omitted.
    #[arg(long, value_name = "PIPELINE", requires = "insert")]
    pub from: Option<String>,
    /// The stage --insert follows; before the next unstarted one when omitted.
    #[arg(long, value_name = "STAGE", requires = "insert")]
    pub after: Option<String>,
    /// Replace the stages not started with those this pipeline runs next.
    #[arg(long, value_name = "PIPELINE")]
    pub switch: Option<String>,
}

/// File inputs are decoded here; policy and pipeline validation belong to archond.
#[derive(Debug, Args)]
pub struct QueueAddArgs {
    /// Work item title.
    #[arg(long)]
    pub title: String,
    /// Registered repository name or repo_… id; optional for repo-free pipelines.
    #[arg(long)]
    pub repo: Option<String>,
    /// Pipeline name; omitted uses the daemon's repository/autonomy defaults.
    #[arg(long)]
    pub pipeline: Option<String>,
    /// UTF-8 brief file, or - to read stdin.
    #[arg(long, value_name = "PATH|-")]
    pub brief_file: PathBuf,
    /// JSON or TOML stage-to-choice-array map, as MCP model_plan. Stages it
    /// leaves out, or all without it, get the daemon's defaults.
    #[arg(long)]
    pub model_plan_file: Option<PathBuf>,
    /// Optional JSON or TOML stage-to-reason map, as MCP model_reasons.
    #[arg(long)]
    pub model_reasons_file: Option<PathBuf>,
    /// low, normal, high or critical (default normal).
    #[arg(long, default_value = "normal")]
    pub priority: String,
    /// Dependency item id or unambiguous prefix; repeat for several.
    #[arg(long)]
    pub depends_on: Vec<String>,
    /// Item tag; repeat for several.
    #[arg(long)]
    pub tag: Vec<String>,
    /// Retried requests with the same key and arguments return the original item.
    #[arg(long)]
    pub idempotency_key: Option<String>,
    /// Item budget in USD.
    #[arg(long)]
    pub budget_usd: Option<f64>,
    /// Review family: strict, standard or none; omitted uses daemon defaults.
    #[arg(long)]
    pub review: Option<archon_core::autonomy::ReviewLevel>,
    /// Proposal mode: lavish or off; omitted uses daemon defaults.
    #[arg(long)]
    pub proposal: Option<archon_core::autonomy::ProposalMode>,
    /// Report mode: before-pr, with-pr or off; omitted uses daemon defaults.
    #[arg(long)]
    pub report: Option<archon_core::autonomy::ReportMode>,
    /// Merge mode: manual or auto; omitted uses daemon defaults.
    #[arg(long)]
    pub merge: Option<archon_core::autonomy::MergeMode>,
    /// Tool access: guarded or yolo; omitted uses daemon defaults.
    #[arg(long)]
    pub tools: Option<archon_core::autonomy::ToolsMode>,
}

#[derive(Debug, Subcommand)]
pub enum PipelineCommand {
    /// List proposed pipelines, or show one proposal and its files.
    Proposals {
        /// Proposal name to inspect; omit to list.
        name: Option<String>,
    },
    /// Activate a validated proposed user pipeline.
    Accept {
        /// Name of the proposed pipeline, without .proposed.
        name: String,
    },
    /// The catalog, with problems.
    List {
        /// Include this repository's pipelines: its directory, or the name of
        /// a registered repository (a name needs archond running).
        #[arg(long)]
        repo: Option<String>,
    },
    /// Validate pipeline directories; non-zero exit on any problem.
    Check {
        /// Include this repository's pipelines: its directory, or the name of
        /// a registered repository (a name needs archond running).
        #[arg(long)]
        repo: Option<String>,
    },
}

#[derive(Debug, Subcommand)]
pub enum ReportCommand {
    /// Show a report again: resume its Lavish session, even one ended from
    /// the browser, or print its path without Lavish.
    Reopen {
        /// The report's rpt_… id, as `archon reports` lists it.
        report: String,
    },
}

#[derive(Debug, Args)]
pub struct PauseArgs {
    /// Only this repository (name or repo_… id).
    #[arg(long, conflicts_with = "pipeline")]
    pub repo: Option<String>,
    /// Only items using this pipeline.
    #[arg(long)]
    pub pipeline: Option<String>,
    /// Also pause running stage agents in Aether (global only).
    #[arg(long)]
    pub hard: bool,
}

#[derive(Debug, Subcommand)]
pub enum DaemonCommand {
    /// Run in the foreground (or detach with --detach).
    Run {
        /// Start in the background, wait until it listens, then exit.
        #[arg(long)]
        detach: bool,
    },
    /// Start archond in the background unless it is running.
    Start,
    /// Ask archond to shut down and wait until it has; in dedicated mode,
    /// then Archon's own aetherd too.
    Stop {
        /// Require stopping Archon's own aetherd: refused in shared mode.
        #[arg(long, visible_alias = "all", conflicts_with = "keep_aether")]
        aether: bool,
        /// Leave Archon's own aetherd running; only archond stops.
        #[arg(long)]
        keep_aether: bool,
    },
    /// Whether archond is running (exit code 3 when not).
    Status,
    /// Hold dispatch until running stage agents finish, then restart archond
    /// and, in dedicated mode, Archon's own aetherd. Running work survives.
    Restart {
        #[command(flatten)]
        drain: DrainArgs,
        /// Restart archond alone; stage agents keep running in Aether.
        #[arg(long)]
        keep_aether: bool,
    },
    /// Drain, install archon and/or aether, then restart as `archon daemon restart`
    /// does. A cargo build is rebuilt with `cargo install --git`; an
    /// installer's binary updates itself with `<program> update`.
    #[command(after_help = "Examples:\n  archon daemon upgrade --archon latest\n  \
                            archon daemon upgrade --archon main --aether 1.0.7 --wait 600")]
    Upgrade {
        /// What archon to install: a release such as 1.2.3, latest, or main.
        #[arg(long, value_name = "TARGET", required_unless_present = "aether")]
        archon: Option<String>,
        /// What aether to install: a release, latest, or main (dedicated
        /// mode only: in shared mode the aetherd is the operator's).
        #[arg(long, value_name = "TARGET")]
        aether: Option<String>,
        #[command(flatten)]
        drain: DrainArgs,
    },
    /// The restart helper an upgrade starts: waits for archond to exit, then
    /// starts it again. Not for direct use.
    #[command(hide = true)]
    Relaunch {
        /// Restart archond alone, leaving Archon's own aetherd running.
        #[arg(long)]
        keep_aether: bool,
    },
}

#[derive(Debug, Args)]
pub struct DrainArgs {
    /// Longest wait, in seconds, for running stage agents to finish.
    #[arg(long, value_name = "SECS", default_value_t = archon_core::ops::DEFAULT_DRAIN_SECS)]
    pub wait: u64,
    /// When the wait runs out, restart anyway: stages stopped with aetherd
    /// are retried. Without it the restart is given up.
    #[arg(long)]
    pub requeue: bool,
}

#[derive(Debug, Subcommand)]
pub enum OrchestratorCommand {
    /// The orchestrator agent, its model and its state.
    Status,
    /// Cancel and recreate the orchestrator agent.
    Restart,
}

#[derive(Debug, Subcommand)]
pub enum InitiativeCommand {
    /// List coordinated initiatives.
    List,
    /// Start one from a .json or .toml file holding what `enqueue_initiative`
    /// takes: title, contract, atomic, members and idempotency_key.
    #[command(after_help = "Example: archon initiatives create rename-account.json")]
    Create {
        /// The initiative's request file.
        file: PathBuf,
    },
    /// Show the shared contract and member states.
    Show {
        /// Initiative id (ini_…) or its start.
        initiative: String,
    },
    /// Append to the shared contract, or replace it; running members are
    /// told the whole new contract.
    Update {
        /// Initiative id (ini_…) or its start.
        initiative: String,
        /// File with the text to append (`-` reads stdin).
        #[arg(long, value_name = "FILE", required_unless_present = "contract_file")]
        append_file: Option<PathBuf>,
        /// File with the whole new contract (`-` reads stdin).
        #[arg(long, value_name = "FILE", conflicts_with = "append_file")]
        contract_file: Option<PathBuf>,
    },
    /// Stop unfinished members, preserving successful work and PRs.
    Cancel {
        /// Initiative id (ini_…) or its start.
        initiative: String,
    },
    /// Requeue unsuccessful members on their existing branches.
    Retry {
        /// Initiative id (ini_…) or its start.
        initiative: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn queue_wait_has_a_finite_default_and_add_requires_file_inputs() {
        let cli = Cli::try_parse_from(["archon", "queue", "wait", "qi_example"]).unwrap();
        let Some(Command::Queue(QueueCommand::Wait { items, timeout })) = cli.command else {
            panic!("queue wait must parse");
        };
        assert_eq!(items, ["qi_example"]);
        assert_eq!(archon_core::age::Age::parse(&timeout).unwrap().secs(), 1800);
        assert!(Cli::try_parse_from(["archon", "queue", "add", "--title", "work"]).is_err());
    }

    #[test]
    fn cli_definition_is_consistent() {
        Cli::command().debug_assert();
    }

    /// Walks `command` and its subcommands: every argument path goes into
    /// `seen`, and each argument or subcommand without help into `missing`.
    fn walk_help(
        command: &clap::Command,
        path: &str,
        seen: &mut Vec<String>,
        missing: &mut Vec<String>,
    ) {
        for arg in command.get_arguments() {
            let name = format!("{path} {}", arg.get_id());
            if arg.get_help().is_none() && arg.get_long_help().is_none() {
                missing.push(name.clone());
            }
            seen.push(name);
        }
        for sub in command.get_subcommands() {
            let name = format!("{path} {}", sub.get_name());
            if sub.get_about().is_none() && sub.get_long_about().is_none() {
                missing.push(name.clone());
            }
            walk_help(sub, &name, seen, missing);
        }
    }

    #[test]
    fn every_argument_and_subcommand_has_help() {
        let mut command = Cli::command();
        // Propagates the global --json and adds the generated --help flags.
        command.build();
        let (mut seen, mut missing) = (Vec::new(), Vec::new());
        walk_help(&command, "archon", &mut seen, &mut missing);
        assert!(missing.is_empty(), "no help text for {missing:?}");
        for deep in ["archon queue caps max_per_repo", "archon daemon run json"] {
            assert!(seen.iter().any(|s| s == deep), "the walk missed {deep}");
        }
    }

    #[test]
    fn a_bare_invocation_has_no_command_and_help_says_what_it_opens() {
        let cli = Cli::try_parse_from(["archon"]).unwrap();
        assert!(cli.command.is_none(), "{:?}", cli.command);
        assert!(!cli.json);
        let mut command = Cli::command();
        let help = command.render_help().to_string();
        let said = "With no command, `archon` opens the terminal UI (`archon tui`). \
                    `archon chat` is the line chat.";
        assert!(help.contains(said), "{help}");
        let about = |name: &str| {
            let sub = command.find_subcommand(name).expect(name);
            sub.get_about().expect(name).to_string()
        };
        assert!(about("tui").contains("what a bare `archon` opens"));
        assert!(!about("chat").contains("default"), "{}", about("chat"));
    }

    #[test]
    fn chat_and_tui_stay_explicit_commands() {
        let chat = Cli::try_parse_from(["archon", "chat", "--progress"]).unwrap();
        assert!(matches!(
            chat.command,
            Some(Command::Chat { progress: true })
        ));
        let tui = Cli::try_parse_from(["archon", "tui", "--normal"]).unwrap();
        let Some(Command::Tui(args)) = tui.command else {
            panic!("`archon tui` parses as the terminal UI");
        };
        assert!(args.normal && !args.plain && args.theme.is_none());
        Cli::try_parse_from(["archon", "--normal"]).expect_err("the flags belong to `archon tui`");
    }

    #[test]
    fn repo_add_takes_a_ship_mode() {
        let cli = Cli::try_parse_from(["archon", "repo", "add", ".", "--ship", "local"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Repo(RepoCommand::Add {
                ship: Some(Ship::Local),
                ..
            }))
        ));
    }

    #[test]
    fn ship_offers_exactly_pr_and_local() {
        let names: Vec<String> = Ship::value_variants()
            .iter()
            .filter_map(|ship| ship.to_possible_value())
            .map(|value| value.get_name().to_owned())
            .collect();
        assert_eq!(names, ["pr", "local"]);
        let listed = "[possible values: pr, local]";
        let refused = Cli::try_parse_from(["archon", "repo", "add", ".", "--ship", "yolo"])
            .unwrap_err()
            .to_string();
        assert!(refused.contains(listed), "{refused}");
        for action in ["add", "set"] {
            let help = Cli::try_parse_from(["archon", "repo", action, "--help"])
                .unwrap_err()
                .to_string();
            assert!(help.contains(listed), "repo {action}: {help}");
        }
    }

    fn parse(args: &[&str]) -> Command {
        let mut argv = vec!["archon"];
        argv.extend_from_slice(args);
        Cli::try_parse_from(argv).unwrap().command.unwrap()
    }

    #[test]
    fn daemon_upgrade_and_restart_parse_targets_and_drain_flags() {
        let Command::Daemon(DaemonCommand::Upgrade {
            archon,
            aether,
            drain,
        }) = parse(&[
            "daemon",
            "upgrade",
            "--archon",
            "main",
            "--aether",
            "1.0.7",
            "--wait",
            "600",
            "--requeue",
        ])
        else {
            panic!("upgrade");
        };
        assert_eq!(archon.as_deref(), Some("main"));
        assert_eq!(aether.as_deref(), Some("1.0.7"));
        assert_eq!(drain.wait, 600);
        assert!(drain.requeue);
        let Command::Daemon(DaemonCommand::Upgrade {
            archon,
            aether,
            drain,
        }) = parse(&["daemon", "upgrade", "--aether", "latest"])
        else {
            panic!("aether-only upgrade");
        };
        assert!(archon.is_none());
        assert_eq!(aether.as_deref(), Some("latest"));
        assert_eq!(drain.wait, archon_core::ops::DEFAULT_DRAIN_SECS);
        assert!(!drain.requeue);
        let Command::Daemon(DaemonCommand::Restart { drain, keep_aether }) = parse(&[
            "daemon",
            "restart",
            "--keep-aether",
            "--wait",
            "30",
            "--requeue",
        ]) else {
            panic!("restart");
        };
        assert_eq!(drain.wait, 30);
        assert!(drain.requeue && keep_aether);
        let Command::Daemon(DaemonCommand::Restart { drain, keep_aether }) =
            parse(&["daemon", "restart"])
        else {
            panic!("restart defaults");
        };
        assert_eq!(drain.wait, archon_core::ops::DEFAULT_DRAIN_SECS);
        assert!(!drain.requeue && !keep_aether);
        for args in [
            vec!["daemon", "upgrade"],
            vec!["daemon", "upgrade", "--archon"],
            vec!["daemon", "restart", "--wait", "later"],
            vec!["daemon", "restart", "--archon", "latest"],
        ] {
            assert!(Cli::try_parse_from([vec!["archon"], args].concat()).is_err());
        }
    }

    #[test]
    fn status_update_checks_and_repo_github_watch_are_explicit() {
        assert!(matches!(
            parse(&["status", "--check-updates"]),
            Command::Status {
                check_updates: true,
                ..
            }
        ));
        assert!(matches!(
            parse(&["status"]),
            Command::Status {
                check_updates: false,
                ..
            }
        ));
        assert!(matches!(
            parse(&["repo", "add", ".", "--github"]),
            Command::Repo(RepoCommand::Add { github: true, .. })
        ));
        assert!(matches!(
            parse(&["repo", "add", "."]),
            Command::Repo(RepoCommand::Add { github: false, .. })
        ));
    }

    #[test]
    fn talk_commands_keep_every_word() {
        assert!(matches!(
            parse(&["chat", "--progress"]),
            Command::Chat { progress: true }
        ));
        let Command::Say { text } = parse(&["say", "ship", "the", "fix"]) else {
            panic!()
        };
        assert_eq!(text, ["ship", "the", "fix"]);
        let Command::Answer { question, text } = parse(&["answer", "2", "yes", "please"]) else {
            panic!()
        };
        assert_eq!((question.as_str(), text.len()), ("2", 2));
        assert!(matches!(
            parse(&["log"]),
            Command::Log {
                limit: 100,
                item: None,
                follow: false,
                ..
            }
        ));
        let Command::Log {
            limit,
            item,
            kind,
            follow,
        } = parse(&[
            "log", "--limit", "5", "--item", "01k2", "--kind", "outcome", "--kind", "blocker",
            "--follow",
        ])
        else {
            panic!()
        };
        assert_eq!((limit, item.as_deref(), follow), (5, Some("01k2"), true));
        assert_eq!(kind, [LogKind::Outcome, LogKind::Blocker]);
        assert!(Cli::try_parse_from(["archon", "log", "--kind", "chatter"]).is_err());
    }

    #[test]
    fn queue_and_repo_flags_parse() {
        let Command::Queue(QueueCommand::List { state, repo, limit }) = parse(&[
            "queue", "list", "--state", "running", "--state", "failed", "--repo", "api",
        ]) else {
            panic!()
        };
        assert_eq!(
            (state.len(), repo.as_deref(), limit),
            (2, Some("api"), None)
        );
        let Command::Queue(QueueCommand::Caps {
            max_parallel_agents,
            budget_daily,
            ..
        }) = parse(&[
            "queue",
            "caps",
            "--max-parallel-agents",
            "4",
            "--budget-daily",
            "12.5",
        ])
        else {
            panic!()
        };
        assert_eq!((max_parallel_agents, budget_daily), (Some(4), Some(12.5)));
        let Command::Queue(QueueCommand::Caps { budgets, .. }) =
            parse(&["queue", "caps", "--budgets", "off"])
        else {
            panic!()
        };
        assert_eq!(budgets, Some(Budgets::Off));
        assert!(Cli::try_parse_from(["archon", "queue", "caps", "--budgets", "maybe"]).is_err());
        let Command::Repo(RepoCommand::Set {
            auto_merge, base, ..
        }) = parse(&["repo", "set", "api", "--auto-merge", "false", "--base", ""])
        else {
            panic!()
        };
        assert_eq!((auto_merge, base.as_deref()), (Some(false), Some("")));
        let Command::Repo(RepoCommand::Set { autonomy_max, .. }) =
            parse(&["repo", "set", "api", "--autonomy-max", "merge=auto"])
        else {
            panic!()
        };
        assert_eq!(autonomy_max.as_deref(), Some("merge=auto"));
        let Command::Repo(RepoCommand::Set { repo, ship, .. }) = parse(&["repo", "set", "api"])
        else {
            panic!("a bare repo set records the git identity again")
        };
        assert_eq!((repo.as_str(), ship), ("api", None));
        let both = [
            "repo",
            "set",
            "api",
            "--autonomy-max",
            "",
            "--auto-merge",
            "true",
        ];
        assert!(Cli::try_parse_from([&["archon"], both.as_slice()].concat()).is_err());
        let Command::Repo(RepoCommand::Set { grant_merge, .. }) = parse(&[
            "repo",
            "set",
            "api",
            "--grant-merge",
            "true",
            "--autonomy-max",
            "merge=manual",
        ]) else {
            panic!("a merge grant is independent of the ceiling")
        };
        assert_eq!(grant_merge, Some(true));
        let Command::Repo(RepoCommand::Set { attribution, .. }) =
            parse(&["repo", "set", "api", "--attribution", "none"])
        else {
            panic!("an attribution policy is a repository setting")
        };
        assert_eq!(attribution, Some(archon_core::repo::Attribution::None));
        let full = ["archon", "repo", "set", "api", "--attribution", "full"];
        assert!(Cli::try_parse_from(full).is_err());
    }

    #[test]
    fn merge_names_one_item_and_may_acknowledge_the_checks() {
        let Command::Merge {
            item,
            ignore_checks,
        } = parse(&["merge", "qi_01", "--ignore-checks"])
        else {
            panic!()
        };
        assert_eq!((item.as_str(), ignore_checks), ("qi_01", true));
        assert!(matches!(
            parse(&["merge", "qi_01"]),
            Command::Merge {
                ignore_checks: false,
                ..
            }
        ));
        for bad in [&["merge"][..], &["merge", "qi_01", "--admin"]] {
            assert!(Cli::try_parse_from([&["archon"], bad].concat()).is_err());
        }
    }

    #[test]
    fn autonomy_commands_parse() {
        let Command::Autonomy(args) = parse(&["autonomy", "--repo", "api"]) else {
            panic!()
        };
        assert!(args.command.is_none() && args.repo.as_deref() == Some("api"));
        let Command::Autonomy(AutonomyArgs {
            command: Some(AutonomyCommand::Direct { text, hours }),
            ..
        }) = parse(&[
            "autonomy",
            "direct",
            "auto-merge",
            "everything",
            "--hours",
            "8",
        ])
        else {
            panic!()
        };
        assert_eq!((text.len(), hours), (2, Some(8)));
        assert!(matches!(
            parse(&["autonomy", "clear", "--all"]),
            Command::Autonomy(AutonomyArgs {
                command: Some(AutonomyCommand::Clear {
                    directive: None,
                    all: true
                }),
                ..
            })
        ));
        for bad in [
            &["autonomy", "clear"][..],
            &["autonomy", "clear", "dir_x", "--all"],
        ] {
            assert!(
                Cli::try_parse_from([&["archon"], bad].concat()).is_err(),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn reports_list_or_reopen_one() {
        assert!(matches!(
            parse(&["reports"]),
            Command::Reports { command: None }
        ));
        let Command::Reports {
            command: Some(ReportCommand::Reopen { report }),
        } = parse(&["reports", "reopen", "rpt_1"])
        else {
            panic!()
        };
        assert_eq!(report, "rpt_1");
        assert!(
            Cli::try_parse_from(["archon", "reports", "reopen"]).is_err(),
            "a report id is required"
        );
    }

    #[test]
    fn cost_defaults_to_days_and_takes_bounds() {
        assert!(matches!(
            parse(&["cost"]),
            Command::Cost {
                since: None,
                until: None,
                by: CostBy::Day
            }
        ));
        let Command::Cost { since, until, by } = parse(&[
            "cost",
            "--since",
            "2026-09-01",
            "--until",
            "1d",
            "--by",
            "model",
        ]) else {
            panic!()
        };
        assert_eq!(
            (since.as_deref(), until.as_deref()),
            (Some("2026-09-01"), Some("1d"))
        );
        assert_eq!(by, CostBy::Model);
        assert!(Cli::try_parse_from(["archon", "cost", "--by", "week"]).is_err());
        let cli = Cli::try_parse_from(["archon", "cost", "--json"]).unwrap();
        assert!(cli.json && matches!(cli.command, Some(Command::Cost { .. })));
    }

    #[test]
    fn update_takes_a_check_or_a_version_but_not_both() {
        let Command::Update(args) = parse(&["update"]) else {
            panic!()
        };
        assert!(!args.check && !args.requirements && args.version.is_none());
        let Command::Update(args) = parse(&["update", "--version", "v1.2.3"]) else {
            panic!()
        };
        assert_eq!(args.version.as_deref(), Some("v1.2.3"));
        let Command::Update(args) = parse(&["update", "--check"]) else {
            panic!()
        };
        assert!(args.check);
        for refused in [
            &["update", "--check", "--version", "v1.2.3"][..],
            &["update", "--requirements", "--check"],
            &["update", "--version"],
        ] {
            let args = [&["archon"], refused].concat();
            assert!(Cli::try_parse_from(args).is_err(), "{refused:?}");
        }
    }

    #[test]
    fn pause_daemon_gc_and_json_parse() {
        let Command::Pause(args) = parse(&["pause", "--hard"]) else {
            panic!()
        };
        assert!(args.hard && args.repo.is_none());
        assert!(
            Cli::try_parse_from(["archon", "pause", "--repo", "a", "--pipeline", "b"]).is_err()
        );
        assert!(matches!(
            parse(&["daemon", "run", "--detach"]),
            Command::Daemon(DaemonCommand::Run { detach: true })
        ));
        let Command::Gc {
            older_than,
            force,
            offline,
        } = parse(&["gc", "--force"])
        else {
            panic!()
        };
        assert_eq!((older_than, force, offline), (None, true, false));
        let Command::Gc {
            older_than,
            offline,
            ..
        } = parse(&["gc", "--offline", "--older-than", "2d"])
        else {
            panic!()
        };
        assert_eq!((older_than.as_deref(), offline), (Some("2d"), true));
        let cli = Cli::try_parse_from(["archon", "status", "--json"]).unwrap();
        assert!(cli.json && matches!(cli.command, Some(Command::Status { .. })));
        assert!(
            Cli::try_parse_from(["archon", "queue", "show"]).is_err(),
            "an item id is required"
        );
        assert!(matches!(
            parse(&["queue", "show", "01k2", "--full"]),
            Command::Queue(QueueCommand::Show { full: true, .. })
        ));
        assert!(Cli::try_parse_from(["archon", "repo", "add", ".", "--ship", "yolo"]).is_err());
    }

    #[test]
    fn queue_steer_takes_one_abort_decision_and_one_shape_change() {
        let Command::Queue(QueueCommand::Steer(args)) =
            parse(&["queue", "steer", "qi_1", "--abort", "--goto", "implement"])
        else {
            panic!()
        };
        assert!(args.abort && args.then.is_none());
        assert_eq!(args.goto.as_deref(), Some("implement"));
        let refused: [&[&str]; 6] = [
            &["--then", "skip"],
            &["--goto", "review"],
            &["--abort", "--then", "skip", "--goto", "review"],
            &["--remove", "review", "--abort"],
            &["--insert", "verify", "--switch", "quick"],
            &["--from", "deep-review"],
        ];
        for flags in refused {
            let line = [&["archon", "queue", "steer", "qi_1"], flags].concat();
            assert!(Cli::try_parse_from(line).is_err(), "{flags:?}");
        }
        let model = ["--model", "claude-opus-5-5"];
        let line = [&["archon", "queue", "steer", "qi_1"], &model[..]].concat();
        assert!(
            Cli::try_parse_from(line).is_err(),
            "a model names its provider"
        );
    }

    #[test]
    fn queue_retry_takes_fresh_and_an_amendment() {
        let Command::Queue(QueueCommand::Retry {
            item,
            fresh,
            append,
        }) = parse(&["queue", "retry", "qi_1", "--fresh", "--append", "do Y"])
        else {
            panic!()
        };
        assert_eq!(
            (item.as_str(), fresh, append.as_deref()),
            ("qi_1", true, Some("do Y"))
        );
        let Command::Queue(QueueCommand::Retry { fresh, append, .. }) =
            parse(&["queue", "retry", "qi_1"])
        else {
            panic!()
        };
        assert!(
            !fresh && append.is_none(),
            "a plain retry reuses the worktree"
        );
    }

    #[test]
    fn queue_refresh_names_one_item() {
        let Command::Queue(QueueCommand::Refresh { item }) = parse(&["queue", "refresh", "qi_1"])
        else {
            panic!()
        };
        assert_eq!(item, "qi_1");
        let missing = Cli::try_parse_from(["archon", "queue", "refresh"]);
        assert!(missing.is_err(), "a refresh names its item");
    }

    #[test]
    fn instance_commands_parse() {
        assert!(matches!(
            parse(&["setup", "--resync"]),
            Command::Setup {
                print: false,
                resync: true
            }
        ));
        assert!(Cli::try_parse_from(["archon", "setup", "--print", "--resync"]).is_err());
        for flag in ["--aether", "--all"] {
            assert!(matches!(
                parse(&["daemon", "stop", flag]),
                Command::Daemon(DaemonCommand::Stop {
                    aether: true,
                    keep_aether: false
                })
            ));
        }
        assert!(matches!(
            parse(&["daemon", "stop"]),
            Command::Daemon(DaemonCommand::Stop {
                aether: false,
                keep_aether: false
            })
        ));
        assert!(matches!(
            parse(&["daemon", "stop", "--keep-aether"]),
            Command::Daemon(DaemonCommand::Stop {
                aether: false,
                keep_aether: true
            })
        ));
        let both = ["archon", "daemon", "stop", "--aether", "--keep-aether"];
        assert!(Cli::try_parse_from(both).is_err());
        assert!(matches!(parse(&["tui"]), Command::Tui(_)));
    }

    #[test]
    fn aether_passes_every_argument_through() {
        for args in [
            vec!["agents", "--json"],
            vec!["--help"],
            vec!["events", "--after", "5", "-h"],
        ] {
            let Command::Aether { args: got } = parse(&[&["aether"], args.as_slice()].concat())
            else {
                panic!()
            };
            assert_eq!(got, args, "passed through untouched");
        }
    }
}
