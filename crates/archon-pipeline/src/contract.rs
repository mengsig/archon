//! The stage contract (ARCHITECTURE §8.6): how every stage agent must work
//! and report, passed as `config.instructions` so pipeline authors cannot
//! omit it. The stage prompt says *what* to do; this says *how to finish*.

use std::path::Path;

use archon_core::catalog::{Gate, StageKind, WorkspaceMode};
use archon_core::model::{ModelChoice, ModelRef};
use archon_core::policy::ModelPolicy;
use archon_core::run::StageOutcome;
use serde::Serialize;

use crate::spec::{Owed, ReviewDuty, StageSpec};
use crate::team::TeamSlot;

mod checks;
mod initiative;
mod lessons;
mod reviewers;
mod storage;

pub use checks::{FastCheck, check_rules};
pub use initiative::initiative_rules;
pub use lessons::lesson_rules;
pub use reviewers::{
    governed_review_refusal, governed_reviewer_rules, provider_reports_cost, reviewer_profile_rules,
};
pub use storage::{artifact_rules, build_cache_rules};

/// Where a stage stands towards its pipeline's team stage.
#[derive(Debug, Clone, Copy)]
pub enum TeamPart<'a> {
    /// The pipeline has no team stage.
    Solo,
    /// Another stage of a team pipeline, in the run worktree on `branch`.
    RunTree { branch: &'a str },
    /// One role's slot of the team stage.
    Role(&'a TeamSlot),
}

/// The contract for one stage agent in `workspace`, with what its stage `owed` its run on
/// record and the item's `experiments` directory. See [`delegation_rules`] for delegators.
pub fn instructions(
    stage: &StageSpec,
    mode: WorkspaceMode,
    workspace: &str,
    team: TeamPart<'_>,
    owed: Owed,
    experiments: Option<&Path>,
) -> String {
    assert!(!workspace.is_empty(), "a stage always has a workspace");
    assert!(stage.fan_out >= 1, "validated specs have fan_out >= 1");
    let mut out = contract_header(stage, mode, workspace, team);
    out.push_str(kind_rules(stage.kind));
    if stage.kind == StageKind::Review {
        out.push_str(&review_bounds(experiments));
    }
    if let TeamPart::RunTree { branch } = team
        && stage.kind == StageKind::Work
    {
        out.push_str(&run_tree_rule(branch));
    }
    if needs_verdict(stage) {
        out.push_str(VERDICT_RULES);
    }
    // An agent that cannot delegate needs no telling that it reviews nothing.
    if let Some(duty) = owed
        .review
        .filter(|d| d.reviewers > 0 || stage.allow_delegation)
    {
        out.push_str(&review_rules(duty));
    }
    if owed.ship_record {
        out.push_str(&ship_record_rules(stage.kind));
        out.push_str(&risk_rules(relays_reviewer(owed)));
    } else if stage.kind == StageKind::Review {
        out.push_str(&review_risk_rules());
    } else if stage.kind == StageKind::Ship {
        out.push_str(&risk_rules(false));
        out.push_str(SHIP_RISK_SHAPE);
    }
    out
}

/// The section naming when Archon stops a stage of `kind` in `mode`: `minutes`
/// after its start, at `deadline`. Only a committing stage is told to commit.
pub fn deadline_rule(
    kind: StageKind,
    mode: WorkspaceMode,
    minutes: u32,
    deadline: archon_core::Timestamp,
) -> String {
    assert!(minutes >= 1, "validated stages have a timeout");
    let commits =
        mode == WorkspaceMode::Worktree && matches!(kind, StageKind::Work | StageKind::Team);
    let pace = match commits {
        true => "Commit each working step as you go, so a retry builds on it, and finish",
        false => "Finish",
    };
    let rule = format!(
        "\n## Deadline\n\n\
         - Archon stops you and every agent you delegated to at {at}, {minutes} minutes \
         after your start, and the stage fails. {pace} with time to spare.\n",
        at = deadline.format("%Y-%m-%d %H:%M UTC"),
    );
    debug_assert!(rule.ends_with('\n'), "sections end with a newline");
    rule
}

/// Whether the record stage holds a review whose reviewers judge the risk.
fn relays_reviewer(owed: Owed) -> bool {
    owed.review.is_some_and(|duty| duty.reviewers > 0)
}

/// The section for an agent that can delegate: `models` is its children's policy in words,
/// `depth` is `[limits] max_delegation_depth`, `experiments` as for [`instructions`].
pub fn delegation_rules(
    mode: WorkspaceMode,
    models: &str,
    depth: u32,
    experiments: Option<&Path>,
) -> String {
    assert!(!models.trim().is_empty(), "the policy is always stated");
    let workspaces = match mode {
        WorkspaceMode::Worktree | WorkspaceMode::Scratch => DELEGATE_WRITABLE,
        WorkspaceMode::RepoReadonly => DELEGATE_READ_ONLY,
    };
    let depth = depth_rule(depth);
    let bounds = review_bounds(experiments);
    format!(
        "\n## Delegation\n\n\
         - Aether's `delegate` hands a scoped part of this stage to a child agent. \
         {depth}\n\
         {workspaces}\
         - Choose each child's model and effort within Archon's policy for this item: \
         {models}. Archon cancels a child outside it. Aether's `list_models` lists what \
         is connected, not what is allowed.\n\
         - Wait for every child (`wait_for`) or cancel it (`cancel_agent`) before you \
         finish: Archon cancels whatever still runs when your stage ends, and that work \
         is lost. A child that finished cannot be asked again: for a second look, \
         delegate a new one.\n\
         {DELEGATE_HAND_OFF}\
         {DELEGATE_CHILD_RULES}\
         {bounds}"
    )
}

/// How deep below this agent its children may delegate.
fn depth_rule(depth: u32) -> String {
    assert!(
        (1..=archon_core::config::DELEGATION_DEPTH_CEILING).contains(&depth),
        "validated with the config"
    );
    match depth {
        1 => "Pass `leaf: true` for every child: Archon cancels an agent a second level \
              down."
            .to_owned(),
        2 => "A child may delegate once more when its part splits further: tell it to pass \
              `leaf: true` for its own children. Pass `leaf: true` yourself for a child that \
              should not. Archon cancels an agent a third level down."
            .to_owned(),
        n => format!(
            "A child may delegate in turn when its part splits further, down to {n} levels \
             below you: tell each child its level, and an agent at level {} to pass \
             `leaf: true` for its own children. Pass `leaf: true` yourself for a child that \
             should not delegate. Archon cancels an agent more than {n} levels down.",
            n - 1
        ),
    }
}

/// An item the stage's item was queued after, as Archon's store has it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Dependency {
    pub item: String,
    /// The directory of the files it published, if it published any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub artifacts: Option<String>,
    /// Its branch in the repository of the stage's item, if it has one there.
    pub branch: Option<String>,
    pub pull_request: Option<String>,
}

/// The contract's section for an item queued after `dependencies`, whose own
/// branch was forked from `base` at commit `fork`: where their work is. Empty
/// when there are none.
pub fn dependency_rules(base: &str, fork: &str, dependencies: &[Dependency]) -> String {
    if dependencies.is_empty() {
        return String::new();
    }
    assert!(!base.trim().is_empty(), "a worktree is forked from a base");
    assert!(!fork.trim().is_empty(), "a worktree knows where it forked");
    let facts: String = dependencies.iter().map(dependency_fact).collect();
    assert_eq!(
        facts.lines().count(),
        dependencies.len(),
        "one line of facts per dependency"
    );
    format!(
        "\n## Dependencies\n\nArchon prepared this branch from `{base}`, with any other \
         parent in this repository merged in, and the change's pull request is for `{base}`. \
         Diff and review against the fork point `{fork}` (`git diff {fork}...HEAD`), \
         whatever base an older prompt names, and pass this base to reviewers.\n\n{facts}"
    )
}

fn dependency_fact(dependency: &Dependency) -> String {
    assert!(!dependency.item.is_empty(), "a dependency is a queued item");
    let fact = serde_json::to_string(dependency).expect("a record of strings serializes");
    assert!(!fact.contains('\n'), "compact JSON is one line");
    format!("- {fact}\n")
}

const DELEGATE_WRITABLE: &str = "\
- A child that edits files gets its own copy: pass `workspace_mode: \"isolated\"`, \
then bring only the changes you accept into your workspace with \
`integrate_workspace`; nothing integrates by itself. A child that only reads (a \
reviewer, a researcher) shares your workspace and sees the change as it stands.\n";

const DELEGATE_HAND_OFF: &str = "\
- A child starts cold and pays to read whatever you do not tell it. Hand it what you \
hold, in a few lines: what done means, the paths that matter and what you found \
there. Never your conversation or the project's history. Read a bounded local \
question yourself instead of delegating it.\n\
- A reviewer's hand-off is the exact base and head commits, the changed paths, the \
checks you ran with their results and the commit they ran on, the risks you know of, \
and the repository guidance it must apply.\n";

const DELEGATE_READ_ONLY: &str = "\
- Your workspace is READ-ONLY, and so is every child's: each one shares it and only \
reads. Do not pass `workspace_mode: \"isolated\"`; there is nothing to integrate.\n";

fn contract_header(
    stage: &StageSpec,
    mode: WorkspaceMode,
    workspace: &str,
    team: TeamPart<'_>,
) -> String {
    assert_eq!(
        matches!(team, TeamPart::Role(_)),
        stage.kind == StageKind::Team,
        "a team slot comes with a team stage, and only with one"
    );
    assert!(!stage.id.is_empty(), "validated stages have an id");
    format!(
        "# Archon stage contract\n\n\
         You are stage `{id}` (kind `{kind}`) of an Archon pipeline, running unattended. \
         Archon reads your result mechanically: this contract overrides any conflicting \
         habit or instruction.\n\n\
         ## Workspace\n\n\
         - Work only in `{workspace}`. {mode_rule}\n\
         - Nobody is watching: never ask the operator anything or wait for a reply. Make \
         the conservative choice and state every assumption in your summary.\n\
         {PROCESS_RULE}\
         - `mcp__archon__message_orchestrator` relays an operator message meant for the \
         orchestrator (`kind=relay`, their exact text), reports a blocker outside your \
         stage (`kind=escalation`) or sends low-priority `info`. It returns at once: keep \
         working, and never wait for a reply, end your turn or complete the stage because \
         you sent one. Use no raw `send_message` to the orchestrator and no other Archon \
         mutation tool but `publish_artifact`.\n\
         {fan_rule}\n\
         ## Finishing\n\n\
         - When done, call Aether's `complete_task` exactly once. Its `summary` is concise \
         plain prose: what you did or found, how you verified it, what is left undone. \
         Later stages and the orchestrator read only this.\n\
         - Otherwise call `fail_task`, never `complete_task`, with an error that starts \
         with:\n  \
         - `{blocked}` when only a human can unblock you (missing credentials or access, a \
         contradictory brief with no safe default, an irreversible action the brief does \
         not authorise), then exactly what the operator must provide or decide. It is \
         never retried.\n  \
         - `{escalate}` when the task does not fit this pipeline (far larger or riskier \
         than the stage prompt allows for, or better split), then why and the pipeline or \
         split you recommend. Leave nothing half done: the orchestrator reshapes the work.\n  \
         - what failed, what you tried and the evidence, for any other failure.\n",
        id = stage.id,
        kind = kind_name(stage.kind),
        mode_rule = mode_rule(mode),
        fan_rule = match team {
            TeamPart::Role(slot) => team_rule(stage, slot),
            TeamPart::Solo | TeamPart::RunTree { .. } => fan_rule(stage),
        },
        blocked = StageOutcome::BLOCKED_PREFIX,
        escalate = StageOutcome::ESCALATE_PREFIX,
    )
}

/// Whether the stage's result must carry a verdict: always for review and
/// test stages, and for any gated stage.
pub fn needs_verdict(stage: &StageSpec) -> bool {
    matches!(stage.kind, StageKind::Review | StageKind::Test) || stage.gate != Gate::None
}

fn kind_name(kind: StageKind) -> &'static str {
    match kind {
        StageKind::Propose => "propose",
        StageKind::Work => "work",
        StageKind::Review => "review",
        StageKind::Test => "test",
        StageKind::Ship => "ship",
        StageKind::Report => "report",
        StageKind::Team => "team",
    }
}

fn mode_rule(mode: WorkspaceMode) -> &'static str {
    match mode {
        WorkspaceMode::Worktree => {
            "It is this run's own git worktree, shared with its other stages. Never touch \
             another worktree, another branch or the operator's own checkout. Before you \
             finish, stop every server and Compose stack you started, unless whoever gave you \
             the task asked you to leave it running; then say in your result what still \
             runs and how to stop it. To keep a server up for a later stage, write its PID to \
             `.archon/services/<name>.pid`: Archon stops it when the run ends, and any other \
             process you leave when your stage ends."
        }
        WorkspaceMode::RepoReadonly => {
            "With a repository, it is a detached snapshot of the commit the repository has \
             checked out, made for this run and deleted when it ends; the operator's own \
             checkout is elsewhere, and its uncommitted changes are not in it. It is \
             READ-ONLY for you: do not create, edit or delete files, do not switch branches, \
             commit or stash, and do not run commands that write into it (formatters, \
             installers, in-tree builds). Put any scratch files in your own temporary \
             directory outside it."
        }
        WorkspaceMode::Scratch => {
            "It is a scratch directory for this item only; nothing in it is kept or \
             shipped."
        }
    }
}

fn fan_rule(stage: &StageSpec) -> String {
    if stage.fan_out <= 1 {
        return String::new();
    }
    format!(
        "- You are one of {} agents running this stage in parallel on the same \
         workspace. Work independently, do not coordinate with the others, and do not \
         modify tracked files.\n",
        stage.fan_out
    )
}

fn team_rule(stage: &StageSpec, slot: &TeamSlot) -> String {
    assert!(
        stage.roles.contains(&slot.role),
        "the slot's role is the stage's"
    );
    format!(
        "- You are role `{role}` of roles [{roles}] of this team stage. The other roles \
         run in parallel, each in its own worktree; never edit theirs. Stay within your \
         role.\n\
         - Commit your work on branch `{branch}`, checked out in your worktree. Archon \
         merges every role's branch onto the run branch once all roles are done; \
         anything you leave uncommitted is committed there for you, except nested git \
         repositories and changes to tracked files inside submodules, which stop the \
         merge. Add submodules only with `git submodule add`. Do not push, \
         do not switch branches, and finish any merge or rebase you start.\n",
        role = slot.role,
        roles = stage.roles.join(", "),
        branch = slot.branch,
    )
}

/// A work stage of a team pipeline: the roles see only what it committed.
fn run_tree_rule(branch: &str) -> String {
    assert!(!branch.is_empty(), "team runs have a run branch");
    format!(
        "- This pipeline has a team stage. Its roles fork their branches from the last \
         commit on the run branch `{branch}`, and Archon merges them back onto it. Commit \
         your changes on `{branch}` before you finish: the roles see only what is \
         committed there, and uncommitted changes to tracked files stop the team stage. \
         Add submodules only with `git submodule add`: a repository cloned or created \
         in place and committed stops the team stage too. Do not push, and do not \
         switch branches.\n"
    )
}

fn kind_rules(kind: StageKind) -> &'static str {
    match kind {
        StageKind::Propose => PROPOSE_RULES,
        StageKind::Work => WORK_RULES,
        StageKind::Review => REVIEW_RULES,
        StageKind::Test => TEST_RULES,
        StageKind::Ship => SHIP_RULES,
        StageKind::Report => REPORT_RULES,
        StageKind::Team => TEAM_RULES,
    }
}

const PROPOSE_RULES: &str = "\n## Propose stage\n\n\
- Propose only: read what you need, but do not edit, commit or push anything. \
Write nowhere in the workspace except `<workspace>/.archon/reports/proposal/`.\n\
- Write the proposal as `<workspace>/.archon/reports/proposal/index.html`, one \
self-contained page that loads nothing from the network.\n\
- `complete_task` must include `structured` data listing the page's absolute path: \
`{\"reports\": [\"/abs/…/index.html\"]}`. The operator may approve the proposal, \
so the `summary` must state the approach on its own.\n";

/// Whose processes an agent may stop: an agent runs as the operator's user,
/// so a pattern kill reaches the operator's daemons and every stage they run.
const PROCESS_RULE: &str = "\
- Stop only processes you started, by the PID you recorded (`$!`), its process group \
or its Compose project. Never kill by name or pattern (`pkill`, `killall`, \
`pgrep -f … | xargs kill`): the operator's archond and aetherd run as your user.\n";

const DELEGATE_CHILD_RULES: &str = "\
- A child inherits none of this contract or of the stage prompt: Aether gives it \
only the `objective` and `instructions` you pass to `delegate`. In every child's \
`instructions`, state each rule below that applies to its role:\n  \
- work only in the workspace it is given, and never touch other worktrees, other \
branches or the operator's own checkout;\n  \
- do not push;\n  \
- never override the git identity: no `git -c user.name` or `user.email`, no \
`--author`, no `GIT_AUTHOR_*` or `GIT_COMMITTER_*`, no identity taken from injected \
context;\n  \
- do not ask the operator questions or wait for replies;\n  \
- stop only processes it started, by recorded PID, process group or Compose \
project, and never kill by name or pattern;\n  \
- read only, with no edits, commits, stashes or branch switches: for a reviewer \
always, and for every child when your own workspace is READ-ONLY.\n";

/// What the stage holding its run's review does and records, for `duty`.
fn review_rules(duty: ReviewDuty) -> String {
    assert!(
        !duty.may_skip || duty.reviewers == 1,
        "a skip stands in for one reviewer"
    );
    let (who, named) = match duty.reviewers {
        0 => return NO_REVIEW_RULES.to_owned(),
        1 => (
            "an agent you delegate to that wrote none of it".to_owned(),
            "\"<agent id>\"".to_owned(),
        ),
        many => (
            format!(
                "{many} agents you delegate to, each working alone and none of which \
                 wrote any of it"
            ),
            format!("\"<agent id>\", … {many} or more"),
        ),
    };
    let skip = match duty.may_skip {
        true => REVIEW_SKIP_RULE,
        false => "",
    };
    let rules = format!(
        "\n## Review on record\n\n\
         - Your stage holds this run's review: the change as it finally stands is reviewed \
         by {who}.\n\
         {REVIEWER_VERDICT_RULES}\
         - Record it in `complete_task`'s `structured` data, beside any other keys, naming \
         the agent id `delegate` returned for each reviewer the pass rests on, a failed \
         one whose findings you fixed included:\n\n\
         ```json\n{{\"review\": {{\"verdict\": \"pass\", \"reviewers\": [{named}]}}}}\n```\n\n\
         {skip}\
         {REVIEW_CHECK_RULES}"
    );
    debug_assert!(rules.ends_with('\n'), "sections end with a newline");
    rules
}

/// Which models the review-holding stage, on `own`, delegates its reviewers to:
/// the plan's, which the daemon enforces, or else ones `policy` allows.
pub fn reviewer_model_rules(
    named: Option<&[ModelChoice]>,
    own: &ModelRef,
    policy: &ModelPolicy,
) -> String {
    match named {
        Some(choices) if !choices.is_empty() => named_reviewer_rules(choices),
        _ => fallback_reviewer_rules(own, policy),
    }
}

fn named_reviewer_rules(choices: &[ModelChoice]) -> String {
    let models: Vec<String> = choices
        .iter()
        .map(|c| format!("{} at effort {}", c.model, c.effort))
        .collect();
    format!(
        "- Delegate every reviewer with role `reviewer`, each on one of these models, \
         taken in turn: {}. Archon cancels a child labelled `reviewer` on any other.\n",
        models.join("; ")
    )
}

/// A model other than `own` where `policy` allows one, else `own`: advice
/// that names only what the daemon lets a reviewer run on.
fn fallback_reviewer_rules(own: &ModelRef, policy: &ModelPolicy) -> String {
    let Some(allowed) = &policy.allow else {
        return format!(
            "- Delegate every reviewer with role `reviewer`, on a model other than yours \
             within the model policy: {}.\n",
            policy.describe()
        );
    };
    let others: Vec<&ModelRef> = allowed
        .iter()
        .filter(|m| *m != own && policy.permits_model(m))
        .collect();
    debug_assert!(
        !others.contains(&own),
        "the lead's model is offered only alone"
    );
    let cap = match policy.max_effort {
        Some(cap) => format!(", at effort at most {cap}"),
        None => String::new(),
    };
    let rules = match others.is_empty() {
        // When `own` is refused too, the spawn carrying this is refused first.
        true => format!(
            "- Delegate every reviewer with role `reviewer`, on your own model {own}{cap}: \
             this item allows no other.\n"
        ),
        false => format!(
            "- Delegate every reviewer with role `reviewer`, on one of {}{cap}: the models \
             this item allows other than yours.\n",
            others
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        ),
    };
    debug_assert!(rules.ends_with('\n'), "sections end with a newline");
    rules
}

const NO_REVIEW_RULES: &str = "\n## Review on record\n\n\
- This item's review level is `none`: delegate no reviewer and record no review, \
whatever the stage prompt says about one. Your own verification is all this change \
gets, so run it in full.\n";

const REVIEW_SKIP_RULE: &str = "\
or, only where the stage prompt lets a change go without review:\n\n\
```json\n\
{\"review\": {\"verdict\": \"skipped\", \"reason\": \"<why this change needed none>\"}}\n\
```\n\n";

const REVIEWER_VERDICT_RULES: &str = "\
- Paste this JSON into each reviewer's instructions as its `complete_task` `structured`, \
never prose alone; a result without `verdict` and `head` counts for nothing. `base` \
starts the diff read:\n\n\
```json\n\
{\"verdict\": \"pass\", \"head\": \"<commit>\", \"base\": \"<commit>\", \"findings\": []}\n\
```\n\n\
Use `\"fail\"` with findings for failures.\n\n";

/// Failing review rounds of one stage attempt after which Archon tells the operator, once.
/// A round is one commit that at least one of the lead's reviewers failed, so three
/// parallel reviewers of one head are one round. The notice lists the latest findings'
/// titles and the run goes on: nothing stops the loop but a pass, the operator or a
/// budget. The lead is told nothing; the rules below stay as they are.
pub const REVIEW_ROUNDS_BEFORE_NOTICE: usize = 4;

const REVIEW_CHECK_RULES: &str = "\
- Delegate with `wait: false`, then use `wait_for` with `any: true`. On a blocking \
fail, `cancel_agent` the reviewers still reading before you edit: they share your \
workspace, and cancelled reviews do not count. Then fix and commit. Replace each \
cancelled reviewer with a new whole-change read at the new head. Completed reads, \
even fails, need only delta passes from the head they read (one can cover several), \
and the fix at least one; an amend or rebase needs whole-change re-review.\n\
- Archon checks a pass against Aether and the branch. Each agent named must be a \
different one you delegated to yourself that completed with that verdict, and as many \
as the review asks must have passed the branch head, or an earlier head whose later \
commits a passing reviewer read with `base` at or before it. A fail stands until such \
a pass, or after an amend or rebase until a pass of the whole change. Name every \
reviewer that coverage rests on, earlier whole-change passes included. Anything else \
fails the run, and nothing ships. Never report `pass` for a \
review that did not run or did not pass: with a blocking finding open, call \
`fail_task` and say which.\n\
- Archon adds the review as the last line of your summary: write no `REVIEW:` line \
yourself.\n";

/// What the stage whose result is the ship record leaves and reports.
fn ship_record_rules(kind: StageKind) -> String {
    let leave = match kind {
        StageKind::Work => {
            "Commit all that should ship on the run branch and no build cache such as \
             `__pycache__`: Archon refuses uncommitted changes to tracked files and \
             delivers no untracked file."
        }
        StageKind::Review | StageKind::Test => {
            "You commit nothing: the record names the commit you judged, as it stands on \
             the run branch."
        }
        other => panic!("validated: a {other:?} stage writes no ship record"),
    };
    let no_change = match kind {
        StageKind::Work => NO_CHANGE_RULE,
        _ => "",
    };
    let rules = format!(
        "\n## Ship record\n\n\
         - Archon delivers this run's commits itself once every stage passed: do not push \
         and do not open a pull request. {leave}\n\
         - Name what ships in `complete_task`'s `structured` data, beside any other keys:\n\n\
         {SHIP_RECORD_JSON}\n\
         {SHIP_RECORD_SHAPE}\
         {no_change}"
    );
    debug_assert!(rules.ends_with('\n'), "sections end with a newline");
    rules
}

/// What a work stage reports when the task needs no change at all.
const NO_CHANGE_RULE: &str = "\
- If the task needs no change, commit nothing and give `\"no_change\": {\"reason\": \"<why>\"}` \
in `structured` instead of the record. The run then succeeds shipping nothing; Archon \
refuses it when the branch has a commit beyond its base or the worktree a change.\n";

/// The ship record as an agent writes it, in a fenced block.
pub const SHIP_RECORD_JSON: &str = "\
```json\n\
{\"commit\": \"<git rev-parse HEAD>\", \"tree\": \"<git rev-parse HEAD^{tree}>\",\n \
\"checks\": [\"<each verification command that ran>\"],\n \
\"pr\": {\"title\": \"<one line, in the repository's convention>\", \
\"body\": \"<what changed and why, how it was verified, assumptions>\"},\n \
\"risk\": {\"level\": \"<low, medium or high>\", \"why\": \"<the evidence for it>\", \
\"captures\": [\"<https:// link to a screenshot or capture, if any>\"]}}\n\
```\n";

/// How the record stage labels its change's risk (ARCHITECTURE §8.7), with
/// how it relays its reviewer's call when it `relays` one.
fn risk_rules(relays: bool) -> String {
    let reviewer = match relays {
        true => {
            "- Give each reviewer the rubric and your level; it returns `\"risk\": \
             {\"level\": …, \"why\": …}` in `structured`, confirming it or changing it with \
             a reason. Relay the call your pass rests on as `risk.reviewer`; it stands.\n"
        }
        false => "",
    };
    let rules = format!(
        "\n## Risk level\n\n\
         - `risk` sets the pull request's advisory `risk:` label and the \"Risk\" section \
         atop its body; nothing merges or gates by it, so never merge on its account. Low may merge \
         with no human review, medium needs one human reviewer, high two who read the code.\n\
         - Rubric:\n{rubric}\
         - `why`: areas and files touched, whether behaviour changes or only tests, docs or \
         comments, tests run with results, review findings, what pushes the level up.\n\
         - `captures`: `https://` links to screenshots or captures of a visible change; \
         never a local path.\n\
         {reviewer}\
         - Without a valid `risk` key Archon labels it `high`.\n",
        rubric = indented(archon_core::risk::RUBRIC),
    );
    debug_assert!(rules.ends_with('\n'), "sections end with a newline");
    rules
}

/// What a review stage after the record stage says of the change's risk.
fn review_risk_rules() -> String {
    let rules = format!(
        "\n## Risk level\n\n\
         - Judge the implementer's proposed advisory risk level by this rubric in \
         `\"risk\": {{\"level\": \"<low, medium or high>\", \"why\": \"…\"}}` of your \
         `structured` data: confirm it, or change it with a reason.\n\
         {rubric}",
        rubric = indented(archon_core::risk::RUBRIC),
    );
    debug_assert!(rules.ends_with('\n'), "sections end with a newline");
    rules
}

/// `list`, a Markdown list, nested one level.
fn indented(list: &str) -> String {
    assert!(list.ends_with('\n'), "a list of whole lines");
    list.lines().map(|line| format!("  {line}\n")).collect()
}

/// Where a ship agent, which has no ship record, puts its `risk` key.
const SHIP_RISK_SHAPE: &str = "- Add it to your `structured` data as `\"risk\": \
{\"level\": \"<low, medium or high>\", \"why\": \"…\", \"captures\": [\"https://…\"]}`.\n";

const SHIP_RECORD_SHAPE: &str = "\
- Read both hashes after the last commit, made under the repository's own git \
identity. Archon verifies the recorded tree against the current branch head and \
delivers that head, failing the run when they differ. It adds the review and later \
stages' results to the body.\n";

const WORK_RULES: &str = "\n## Work stage\n\n\
- Use the report format the stage prompt gives for your summary. If it gives none, \
list what changed and why, the files touched, the exact verification commands you ran \
with their results, and any known gaps.\n\
- Report the `git rev-parse HEAD^{tree}` of your last full check.\n";

const TEAM_RULES: &str = "\n## Team stage\n\n\
- Use the report format the stage prompt gives for your summary. If it gives none, \
list what your role changed and why, the files touched, the commits you made, the \
verification commands you ran with their results, any assumption you made about a \
peer's work, and any known gaps.\n\
\n## Team coordination\n\n\
- Once every role has an agent, Archon sends you one roster message: each role's name, \
Aether agent id, worktree and branch. If a role is replaced, a roster with a higher \
version follows; it supersedes the earlier one.\n\
- Your peers are other agents, not the operator: message them with Aether's \
`send_message` and the agent ids from the roster. Agree interfaces and contracts with \
the roles that depend on them early, before you build on them. Report a breaking \
change to the roles it affects as soon as you make it. Ask a peer for help when you \
debug something in its scope.\n\
- Keep messages concise: what you need or changed, with exact names, paths and \
signatures.\n\
- Stay within your role's scope and your own worktree. Never edit, commit in or run \
git in a peer's worktree; ask its role instead.\n\
- Never block on a peer for long. If no answer comes, proceed on your best assumption \
and state it in your summary.\n\
- Commit your work on your own branch: peers see it only once Archon merges every \
role's branch.\n";

const REVIEW_RULES: &str = "\n## Review stage\n\n\
- Review only; do not edit, commit or push. Your findings go back to the implementer \
verbatim if the gate fails, so make each one actionable: where (path and line), what \
is wrong, why it matters, and what a fix looks like.\n";

const REVIEW_BOUNDS_LEAD: &str = "\
- Reviewers: read the code and run the checks the review needs. Run each ad-hoc \
experiment (mutant, probe, repro) in a ";

/// Where an experiment copy goes when Archon names no directory for it.
const REVIEW_BOUNDS_FALLBACK: &str = "`mktemp -d` copy of tracked files only \
(no build outputs, no literal /tmp; delete it after)";

/// A timed-out experiment shows nothing: one that compiles may just be slow.
const REVIEW_BOUNDS_TAIL: &str = " under one `timeout 30s` for all its repeats, \
`timeout 120s` when it compiles (`timeout 30s sh -c '…'` for loops and pipes); a \
timeout is inconclusive, never a finding or a detection on its own. \
Run the project's own build and tests normally, uncapped. \
No long hang proofs or background waits. Report incomplete checks; never pass \
without the evidence the review requires.\n";

/// The reviewers' bounds, with experiment copies in the item's `experiments`
/// directory, or wherever `mktemp -d` puts them when there is none.
fn review_bounds(experiments: Option<&Path>) -> String {
    let copy = match experiments {
        Some(dir) => {
            assert!(dir.is_absolute(), "the experiments directory is absolute");
            let dir = shell_word(&dir.display().to_string());
            format!(
                "`mktemp -d -p {dir}` copy of tracked files only (no build outputs; delete it \
                 after)"
            )
        }
        None => REVIEW_BOUNDS_FALLBACK.to_owned(),
    };
    format!("{REVIEW_BOUNDS_LEAD}{copy}{REVIEW_BOUNDS_TAIL}")
}

/// `text` as one POSIX shell word: bare when every character is plainly
/// safe, otherwise single-quoted.
fn shell_word(text: &str) -> String {
    assert!(!text.is_empty(), "an empty word names nothing");
    let plain = |c: char| c.is_ascii_alphanumeric() || "/._-+:@%,=".contains(c);
    if text.chars().all(plain) {
        return text.to_owned();
    }
    let quoted = format!("'{}'", text.replace('\'', r"'\''"));
    assert!(quoted.len() > text.len(), "quoting only adds characters");
    quoted
}

const TEST_RULES: &str = "\n## Test stage\n\n\
- Verify by running things, not by reading alone. Do not edit tracked files to make a \
check pass; a failing check is a finding. Build outputs and caches are fine.\n\
- Quote the failing command and the relevant output lines in each finding's `detail`.\n";

const SHIP_RULES: &str = "\n## Ship stage\n\n\
- Deliver exactly as the stage prompt's ship mode says. Never force-push, never \
rewrite history that already left this machine, and never merge unless the prompt \
says auto-merge is enabled.\n\
- The commits you are given were verified and reviewed as they are: never amend, \
rebase, squash or reset them. Only a commit message that breaks a rule of the \
repository, so that the work cannot ship with it, may be reworded, and nothing but the \
message: `git rev-parse HEAD^{tree}` must print the same tree before and after. Say \
in your summary what you reworded and name that tree. If the tree changed, put the \
original commits back and call `fail_task`.\n\
- `complete_task` must include `structured` data naming what was delivered: \
`{\"pr_url\": \"https://…\"}` when a pull request exists, otherwise \
`{\"commit\": \"<full sha>\"}` for the commit on the run branch. Include both when \
both apply.\n";

const REPORT_RULES: &str = "\n## Report stage\n\n\
- Write each HTML report as `<workspace>/.archon/reports/<name>/index.html` with its \
assets (screenshots, CSS) in the same directory, using relative links.\n\
- `complete_task` must include `structured` data listing the absolute path of every \
report you want published: `{\"reports\": [\"/abs/…/index.html\"]}`. Use an empty \
list if you wrote none. Archon publishes exactly these files.\n";

const VERDICT_RULES: &str = "\n## Verdict\n\n\
- Put this in `complete_task`'s `structured` data, beside any other keys:\n\n\
```json\n\
{\"verdict\": \"pass\" | \"fail\",\n \
\"findings\": [{\"severity\": \"critical|high|medium|low\", \"title\": \"…\", \
\"detail\": \"…\", \"path\": \"relative/path:line\"}]}\n\
```\n\n\
- The stage prompt defines which findings make the verdict `fail`; apply its rule \
exactly. A `fail` lists at least one finding. A `pass` may still list findings the \
prompt treats as advice.\n\
- End the `summary` with a final line that is exactly `VERDICT: PASS` or \
`VERDICT: FAIL`, matching the structured verdict. A missing or unreadable verdict \
counts as a fail.\n";

#[cfg(test)]
mod tests {
    use super::*;

    fn stage(kind: StageKind, gate: Gate, fan_out: u32) -> StageSpec {
        StageSpec {
            id: "s".into(),
            kind,
            prompt: "s.md".into(),
            fan_out,
            gate,
            timeout_minutes: 10,
            effort: archon_core::model::Effort::Medium,
            min_tier: None,
            tier: None,
            allow_delegation: false,
            nudge: None,
            on_fail: None,
            roles: Vec::new(),
            role_prompts: Default::default(),
            roles_from: None,
        }
    }

    #[test]
    fn the_deadline_names_its_time_and_asks_only_committing_stages_to_commit() {
        let at: archon_core::Timestamp = "2026-10-30T14:05:00Z".parse().unwrap();
        let work = deadline_rule(StageKind::Work, WorkspaceMode::Worktree, 180, at);
        assert!(work.starts_with("\n## Deadline\n\n"), "{work}");
        assert!(work.contains("at 2026-10-30 14:05 UTC, 180 minutes after your start"));
        assert!(work.contains("every agent you delegated to"), "{work}");
        assert!(
            work.contains("Commit each working step as you go"),
            "{work}"
        );
        let team = deadline_rule(StageKind::Team, WorkspaceMode::Worktree, 90, at);
        assert!(team.contains("Commit each working step"), "{team}");
        for (kind, mode) in [
            (StageKind::Review, WorkspaceMode::Worktree),
            (StageKind::Report, WorkspaceMode::Worktree),
            (StageKind::Work, WorkspaceMode::RepoReadonly),
            (StageKind::Work, WorkspaceMode::Scratch),
        ] {
            let text = deadline_rule(kind, mode, 45, at);
            assert!(text.contains("45 minutes after your start"), "{text}");
            assert!(text.contains("Finish with time to spare."), "{text}");
            assert!(!text.contains("Commit"), "{kind:?} in {mode:?}: {text}");
        }
    }

    #[test]
    fn every_contract_names_the_workspace_and_blocked_prefix() {
        for kind in [
            StageKind::Propose,
            StageKind::Work,
            StageKind::Review,
            StageKind::Test,
            StageKind::Ship,
            StageKind::Report,
        ] {
            let text = instructions(
                &stage(kind, Gate::None, 1),
                WorkspaceMode::Worktree,
                "/w",
                TeamPart::Solo,
                Owed::default(),
                None,
            );
            assert!(text.contains("Work only in `/w`"), "{kind:?}");
            assert!(
                text.contains("write its PID to `.archon/services/<name>.pid`: Archon stops it when the run ends"),
                "{kind:?}"
            );
            assert!(text.contains("`BLOCKED:`"), "{kind:?}");
            assert!(text.contains("complete_task"), "{kind:?}");
            assert!(
                text.contains("mcp__archon__message_orchestrator"),
                "{kind:?}"
            );
            assert!(
                text.contains("It returns at once: keep working"),
                "{kind:?}"
            );
            assert!(
                text.contains("never wait for a reply, end your turn"),
                "{kind:?}"
            );
            assert!(
                text.contains("call `fail_task`, never `complete_task`"),
                "{kind:?}"
            );
            for said in [
                "- Stop only processes you started, by the PID you recorded (`$!`)",
                "Never kill by name or pattern (`pkill`, `killall`, `pgrep -f … | xargs kill`)",
                "the operator's archond and aetherd run as your user",
            ] {
                assert!(text.contains(said), "{kind:?}: missing {said:?}");
            }
        }
    }

    #[test]
    fn review_and_test_contracts_demand_a_verdict() {
        for kind in [StageKind::Review, StageKind::Test] {
            let text = instructions(
                &stage(kind, Gate::AllPass, 1),
                WorkspaceMode::Worktree,
                "/w",
                TeamPart::Solo,
                Owed::default(),
                None,
            );
            assert!(text.contains("VERDICT: PASS"), "{kind:?}");
            assert!(text.contains("\"verdict\""), "{kind:?}");
            // The contract fixes the format; the prompt owns the threshold.
            assert!(text.contains("stage prompt defines which findings"));
        }
        let work = instructions(
            &stage(StageKind::Work, Gate::None, 1),
            WorkspaceMode::Worktree,
            "/w",
            TeamPart::Solo,
            Owed::default(),
            None,
        );
        assert!(!work.contains("VERDICT: PASS"));
        assert!(work.contains("stop every server and Compose stack you started, unless"));
        assert!(work.contains("say in your result what still runs"));
    }

    /// A review stage's contract and a delegator's rules, given `experiments`.
    fn bounded(experiments: Option<&Path>) -> [String; 2] {
        let review = instructions(
            &stage(StageKind::Review, Gate::AllPass, 1),
            WorkspaceMode::Worktree,
            "/w",
            TeamPart::Solo,
            Owed::default(),
            experiments,
        );
        let delegated = delegation_rules(WorkspaceMode::Worktree, "a/one", 1, experiments);
        [review, delegated]
    }

    #[test]
    fn reviewers_bound_commands_and_reporting_without_skipping_checks() {
        let dir = Path::new("/data/experiments/itm_1");
        for (experiments, copy) in [
            (None, "`mktemp -d` copy of tracked files only"),
            (
                Some(dir),
                "`mktemp -d -p /data/experiments/itm_1` copy of tracked files only",
            ),
        ] {
            for text in bounded(experiments) {
                for said in [
                    "read the code and run the checks the review needs",
                    copy,
                    "delete it after) under one `timeout 30s` for all its repeats",
                    "`timeout 120s` when it compiles",
                    "Run the project's own build and tests normally, uncapped.",
                    "`timeout 30s sh -c '…'` for loops and pipes",
                    "a timeout is inconclusive, never a finding or a detection on its own",
                    "No long hang proofs or background waits",
                    "never pass without the evidence the review requires",
                ] {
                    assert!(text.contains(said), "missing {said:?} in {text}");
                }
                assert_eq!(text.matches(&review_bounds(experiments)).count(), 1);
            }
        }
    }

    #[test]
    fn without_a_directory_reviewers_fall_back_to_mktemp_and_no_literal_tmp() {
        let fallback = "- Reviewers: read the code and run the checks the review needs. Run each \
            ad-hoc experiment (mutant, probe, repro) in a `mktemp -d` copy of tracked files only \
            (no build outputs, no literal /tmp; delete it after) under one `timeout 30s` for all \
            its repeats, `timeout 120s` when it compiles (`timeout 30s sh -c '…'` for loops and \
            pipes); a timeout is inconclusive, never a finding or a detection on its own. Run the \
            project's own build and tests normally, uncapped. No long hang \
            proofs or background waits. Report incomplete checks; never pass without the \
            evidence the review requires.\n";
        assert_eq!(review_bounds(None), fallback);
        for text in bounded(None) {
            assert!(!text.contains("mktemp -d -p"), "{text}");
        }
        let named = review_bounds(Some(Path::new("/data/experiments/itm_1")));
        assert!(!named.contains("literal /tmp"), "{named}");
        assert!(!named.contains("`mktemp -d` "), "{named}");
    }

    #[test]
    fn an_experiments_directory_with_spaces_or_quotes_is_one_shell_word() {
        let spaced = Path::new("/home/a b/archon data/experiments/itm_1");
        for text in bounded(Some(spaced)) {
            assert!(
                text.contains("`mktemp -d -p '/home/a b/archon data/experiments/itm_1'` copy"),
                "{text}"
            );
        }
        let quote = review_bounds(Some(Path::new("/d/it's/x")));
        assert!(
            quote.contains(r"`mktemp -d -p '/d/it'\''s/x'` copy"),
            "{quote}"
        );
        assert_eq!(shell_word("/plain/path_1.2-x"), "/plain/path_1.2-x");
        assert_eq!(shell_word("/a$b"), "'/a$b'");
    }

    #[test]
    fn review_holders_fix_early_and_request_only_missing_delta_passes() {
        for reviewers in [1, 3] {
            let text = work_contract(owing(reviewers, false));
            for said in [
                "Delegate with `wait: false`",
                "use `wait_for` with `any: true`",
                "On a blocking fail, `cancel_agent` the reviewers still reading before you edit",
                "cancelled reviews do not count. Then fix and commit",
                "Replace each cancelled reviewer with a new whole-change read at the new head",
                "Completed reads, even fails, need only delta passes from the head they read (one can cover several)",
                "and the fix at least one",
                "an amend or rebase needs whole-change re-review",
                "a passing reviewer read with `base` at or before it",
                "A fail stands until such a pass",
            ] {
                assert!(text.contains(said), "missing {said:?} in {text}");
            }
        }
        let delegation = delegation_rules(WorkspaceMode::Worktree, "a/one", 1, None);
        assert!(
            delegation.contains("Wait for every child (`wait_for`) or cancel it (`cancel_agent`)")
        );
    }

    #[test]
    fn report_contract_asks_for_verdict_only_when_gated() {
        let ungated = stage(StageKind::Report, Gate::None, 2);
        let gated = stage(StageKind::Report, Gate::Majority, 2);
        let text = instructions(
            &ungated,
            WorkspaceMode::Worktree,
            "/w",
            TeamPart::Solo,
            Owed::default(),
            None,
        );
        assert!(text.contains("\"reports\""));
        assert!(text.contains("one of 2 agents"));
        assert!(!text.contains("VERDICT: PASS"));
        assert!(
            instructions(
                &gated,
                WorkspaceMode::Worktree,
                "/w",
                TeamPart::Solo,
                Owed::default(),
                None
            )
            .contains("VERDICT: PASS")
        );
    }

    #[test]
    fn ship_contract_names_structured_delivery_keys() {
        let text = instructions(
            &stage(StageKind::Ship, Gate::None, 1),
            WorkspaceMode::Worktree,
            "/w",
            TeamPart::Solo,
            Owed::default(),
            None,
        );
        assert!(text.contains("\"pr_url\""));
        assert!(text.contains("\"commit\""));
    }

    /// What a ship stage delivers is what was reviewed: at most a message
    /// differs, and the tree proves it.
    #[test]
    fn ship_contract_forbids_changing_the_commits_it_was_given() {
        let of = |kind| {
            let stage = stage(kind, Gate::None, 1);
            instructions(
                &stage,
                WorkspaceMode::Worktree,
                "/w",
                TeamPart::Solo,
                Owed::default(),
                None,
            )
        };
        let ship = of(StageKind::Ship);
        for said in [
            "never amend, rebase, squash or reset them",
            "may be reworded, and nothing but the message",
            "`git rev-parse HEAD^{tree}` must print the same tree before and after",
            "put the original commits back and call `fail_task`",
        ] {
            assert!(ship.contains(said), "missing {said:?} in {ship}");
        }
        let work = of(StageKind::Work);
        assert!(!work.contains("must print the same tree"));
        assert!(
            work.contains("Report the `git rev-parse HEAD^{tree}` of your last full check"),
            "a later stage may rely on a check bound to the tree: {work}"
        );
    }

    #[test]
    fn propose_contract_forbids_edits_and_names_the_page() {
        let text = instructions(
            &stage(StageKind::Propose, Gate::None, 1),
            WorkspaceMode::Worktree,
            "/w",
            TeamPart::Solo,
            Owed::default(),
            None,
        );
        assert!(text.contains("kind `propose`"));
        assert!(text.contains(".archon/reports/proposal/index.html"));
        assert!(!text.contains("VERDICT: PASS"));
    }

    #[test]
    fn team_contract_names_the_role_roster_and_branch() {
        let mut team = stage(StageKind::Team, Gate::None, 2);
        team.roles = vec!["api".into(), "ui".into()];
        let slot = TeamSlot {
            role: "ui".into(),
            branch: "archon/x--ui-q69g5fav-a1".into(),
            workspace: "/t/s-ui".into(),
        };
        let part = TeamPart::Role(&slot);
        let text = instructions(
            &team,
            WorkspaceMode::Worktree,
            "/t/s-ui",
            part,
            Owed::default(),
            None,
        );
        assert!(text.contains("kind `team`"));
        assert!(text.contains("role `ui` of roles [api, ui]"), "{text}");
        assert!(text.contains("Commit your work on branch `archon/x--ui-q69g5fav-a1`"));
        assert!(text.contains("Stay within your role"));
        assert!(!text.contains("do not modify tracked files"));
    }

    #[test]
    fn team_contract_explains_the_roster_and_how_to_coordinate() {
        let mut team = stage(StageKind::Team, Gate::None, 2);
        team.roles = vec!["api".into(), "ui".into()];
        let slot = TeamSlot {
            role: "api".into(),
            branch: "archon/x--api-q69g5fav-a1".into(),
            workspace: "/t/s-api".into(),
        };
        let text = instructions(
            &team,
            WorkspaceMode::Worktree,
            "/t/s-api",
            TeamPart::Role(&slot),
            Owed::default(),
            None,
        );
        for rule in [
            "Archon sends you one roster message",
            "a roster with a higher version follows; it supersedes",
            "`send_message`",
            "Agree interfaces and contracts",
            "Report a breaking change",
            "Ask a peer for help when you debug",
            "Keep messages concise",
            "Never edit, commit in or run git in a peer's worktree",
            "proceed on your best assumption and state it in your summary",
            "Commit your work on your own branch",
        ] {
            assert!(text.contains(rule), "missing {rule:?} in {text}");
        }
        let solo = instructions(
            &stage(StageKind::Work, Gate::None, 1),
            WorkspaceMode::Worktree,
            "/w",
            TeamPart::Solo,
            Owed::default(),
            None,
        );
        assert!(!solo.contains("roster"), "only team roles get a roster");
    }

    #[test]
    fn work_stages_of_a_team_pipeline_must_commit() {
        let part = TeamPart::RunTree { branch: "archon/x" };
        for kind in [StageKind::Work, StageKind::Review, StageKind::Ship] {
            let gate = match kind {
                StageKind::Review => Gate::AllPass,
                _ => Gate::None,
            };
            let text = instructions(
                &stage(kind, gate, 1),
                WorkspaceMode::Worktree,
                "/t/main",
                part,
                Owed::default(),
                None,
            );
            let rule = text.contains("Commit your changes on `archon/x` before you finish");
            assert_eq!(rule, kind == StageKind::Work, "{kind:?}");
            let submodules = text.contains("Add submodules only with `git submodule add`");
            assert_eq!(submodules, kind == StageKind::Work, "{kind:?}");
        }
        let solo = instructions(
            &stage(StageKind::Work, Gate::None, 1),
            WorkspaceMode::Worktree,
            "/w",
            TeamPart::Solo,
            Owed::default(),
            None,
        );
        assert!(!solo.contains("team stage"));
    }

    /// The engine's contract holds no delegation rules: who can delegate is
    /// decided where the agent is created, which adds [`delegation_rules`].
    #[test]
    fn delegation_rules_fit_the_workspace_and_state_the_model_policy() {
        let mut work = stage(StageKind::Work, Gate::None, 1);
        work.allow_delegation = true;
        let contract = instructions(
            &work,
            WorkspaceMode::Worktree,
            "/w",
            TeamPart::Solo,
            Owed::default(),
            None,
        );
        assert_eq!(contract.matches("delegate").count(), 0, "{contract}");

        let writable = delegation_rules(WorkspaceMode::Worktree, "a/one, b/two", 2, None);
        assert!(writable.starts_with("\n## Delegation\n"), "{writable}");
        for said in [
            "A child may delegate once more when its part splits further",
            "tell it to pass `leaf: true` for its own children",
            "Archon cancels an agent a third level down",
            "pass `workspace_mode: \"isolated\"`",
            "bring only the changes you accept into your workspace with `integrate_workspace`",
            "policy for this item: a/one, b/two. Archon cancels a child outside it.",
            "`list_models` lists what is connected, not what is allowed",
            "Wait for every child (`wait_for`) or cancel it (`cancel_agent`) before you finish",
            "A child that finished cannot be asked again",
        ] {
            assert!(writable.contains(said), "missing {said:?} in {writable}");
        }
        assert_eq!(
            writable,
            delegation_rules(WorkspaceMode::Scratch, "a/one, b/two", 2, None)
        );

        let read_only = delegation_rules(WorkspaceMode::RepoReadonly, "a/one", 2, None);
        assert!(read_only.contains("Your workspace is READ-ONLY, and so is every child's"));
        assert!(read_only.contains("Do not pass `workspace_mode: \"isolated\"`"));
        assert_eq!(
            read_only.matches("integrate_workspace").count(),
            0,
            "{read_only}"
        );
        assert!(read_only.contains("policy for this item: a/one. Archon cancels"));
    }

    /// `[limits] max_delegation_depth` decides how deep children are told
    /// they may delegate, and where Archon cancels.
    #[test]
    fn delegation_rules_follow_the_configured_depth() {
        let only_children = delegation_rules(WorkspaceMode::Worktree, "a/one", 1, None);
        assert!(only_children.contains("Pass `leaf: true` for every child"));
        assert!(only_children.contains("Archon cancels an agent a second level down."));
        assert!(!only_children.contains("delegate once more"));
        let deeper = delegation_rules(WorkspaceMode::Worktree, "a/one", 3, None);
        for said in [
            "down to 3 levels below you",
            "an agent at level 2 to pass `leaf: true` for its own children",
            "Archon cancels an agent more than 3 levels down.",
        ] {
            assert!(deeper.contains(said), "missing {said:?} in {deeper}");
        }
    }

    /// A child rereads what it is not told, and a reviewer without the exact
    /// commits reviews the wrong diff: every delegating agent is told both.
    #[test]
    fn delegation_rules_say_what_a_child_and_a_reviewer_are_handed() {
        for mode in [WorkspaceMode::Worktree, WorkspaceMode::RepoReadonly] {
            let rules = delegation_rules(mode, "a/one", 2, None);
            for said in [
                "A child starts cold and pays to read whatever you do not tell it",
                "Never your conversation or the project's history",
                "Read a bounded local question yourself instead of delegating it",
                "A reviewer's hand-off is the exact base and head commits, the changed paths",
                "the checks you ran with their results and the commit they ran on",
            ] {
                assert!(
                    rules.contains(said),
                    "{mode:?}: missing {said:?} in {rules}"
                );
            }
            let hand_off = rules.find("A child starts cold").expect("asserted above");
            let passed_on = rules
                .find("A child inherits none")
                .expect("the child rules");
            assert!(
                hand_off < passed_on,
                "the list of child rules ends the section"
            );
        }
    }

    /// Dependency facts accompany the actual base prepared by the daemon.
    #[test]
    fn dependency_rules_name_each_branch_and_the_base_of_a_stacked_change() {
        const FORK: &str = "0123456789abcdef0123456789abcdef01234567";
        assert_eq!(dependency_rules("main", FORK, &[]), "");
        let stacked = Dependency {
            item: "qi_01".into(),
            artifacts: Some("/d/artifacts/qi_01".into()),
            branch: Some("archon/first-01".into()),
            pull_request: Some("https://github.com/o/r/pull/1".into()),
        };
        let elsewhere = Dependency {
            item: "qi_02".into(),
            artifacts: None,
            branch: None,
            pull_request: None,
        };
        let rules = dependency_rules("main", FORK, &[stacked, elsewhere]);
        assert!(rules.starts_with("\n## Dependencies\n\n"), "{rules}");
        let facts: Vec<serde_json::Value> = rules
            .lines()
            .filter_map(|line| line.strip_prefix("- ").filter(|fact| fact.starts_with('{')))
            .map(|fact| serde_json::from_str(fact).expect("each fact is JSON"))
            .collect();
        let pull_request = "https://github.com/o/r/pull/1";
        let expected = [
            serde_json::json!({"item": "qi_01", "artifacts": "/d/artifacts/qi_01", "branch": "archon/first-01", "pull_request": pull_request}),
            serde_json::json!({"item": "qi_02", "branch": null, "pull_request": null}),
        ];
        assert_eq!(facts, expected);
        assert!(rules.contains("prepared this branch from `main`"));
        assert!(rules.contains("pull request is for `main`"));
        assert!(
            rules.contains(&format!("`git diff {FORK}...HEAD`")),
            "{rules}"
        );
        assert!(
            !rules.contains("against `main`"),
            "a stale local base adds commits"
        );
        assert!(rules.contains("pass this base to reviewers"));
    }

    /// Aether gives a child only the instructions `delegate` was passed, so
    /// a rule the delegating agent does not repeat does not reach the child.
    #[test]
    fn a_delegating_agent_must_pass_its_rules_on_in_each_childs_instructions() {
        for mode in [WorkspaceMode::Worktree, WorkspaceMode::RepoReadonly] {
            let rules = delegation_rules(mode, "a/one", 2, None);
            for said in [
                "A child inherits none of this contract or of the stage prompt",
                "only the `objective` and `instructions` you pass to `delegate`",
                "In every child's `instructions`, state each rule below that applies to its role",
                "\n  - work only in the workspace it is given, and never touch other worktrees, \
                 other branches or the operator's own checkout;",
                "\n  - do not push;",
                "\n  - never override the git identity:",
                "`--author`, no `GIT_AUTHOR_*` or `GIT_COMMITTER_*`",
                "\n  - do not ask the operator questions or wait for replies;",
                "\n  - stop only processes it started, by recorded PID, process group or \
                 Compose project, and never kill by name or pattern;",
                "\n  - read only, with no edits, commits, stashes or branch switches: for a \
                 reviewer always, and for every child when your own workspace is READ-ONLY.",
            ] {
                assert!(
                    rules.contains(said),
                    "{mode:?}: missing {said:?} in {rules}"
                );
            }
        }
    }

    fn owing(reviewers: u32, may_skip: bool) -> Owed {
        Owed {
            review: Some(ReviewDuty {
                reviewers,
                may_skip,
            }),
            ship_record: false,
        }
    }

    fn work_contract(owed: Owed) -> String {
        let work = stage(StageKind::Work, Gate::None, 1);
        instructions(
            &work,
            WorkspaceMode::Worktree,
            "/w",
            TeamPart::Solo,
            owed,
            None,
        )
    }

    fn model(name: &str) -> ModelRef {
        name.parse().unwrap()
    }

    #[test]
    fn reviewers_get_the_plan_s_models_whatever_the_policy() {
        let choice = |model: &str, effort| ModelChoice {
            model: model.parse().unwrap(),
            effort,
            tier: None,
        };
        let named = [
            choice("claude-code/opus", archon_core::model::Effort::Medium),
            choice("claude-code/haiku", archon_core::model::Effort::Low),
        ];
        let own = model("claude-code/opus");
        let rules = reviewer_model_rules(Some(&named), &own, &ModelPolicy::default());
        assert!(rules.contains("role `reviewer`"), "{rules}");
        assert!(
            rules.contains("claude-code/opus at effort medium; claude-code/haiku at effort low")
        );
        assert!(rules.contains("Archon cancels a child labelled `reviewer` on any other"));
    }

    /// The bench's smoke preset allows one model: a reviewer must run on it,
    /// not be sent to one the daemon cancels.
    #[test]
    fn a_single_model_allowlist_keeps_reviewers_on_the_lead_s_model() {
        let own = model("claude-code/claude-haiku-5-5");
        let only = ModelPolicy::from_lists(
            std::slice::from_ref(&own),
            &[],
            Some(archon_core::model::Effort::Medium),
        );
        for unnamed in [None, Some(&[][..])] {
            let rules = reviewer_model_rules(unnamed, &own, &only);
            assert!(
                rules.contains(
                    "on your own model claude-code/claude-haiku-5-5, at effort at most medium"
                ),
                "{rules}"
            );
            assert!(!rules.contains("other than yours"), "{rules}");
            assert!(!rules.contains("Codex"), "{rules}");
        }
        let denied = ModelPolicy::from_lists(
            &[own.clone(), model("codex/luna")],
            &[model("codex/luna")],
            None,
        );
        let rules = reviewer_model_rules(None, &own, &denied);
        assert!(rules.contains("on your own model"), "{rules}");
        assert!(!rules.contains("codex/luna"), "{rules}");
    }

    #[test]
    fn a_wider_allowlist_prefers_another_model_and_an_open_one_states_the_policy() {
        let own = model("claude-code/opus");
        let allow = [own.clone(), model("claude-code/haiku"), model("codex/luna")];
        let wide = ModelPolicy::from_lists(&allow, &[model("codex/luna")], None);
        let rules = reviewer_model_rules(None, &own, &wide);
        assert!(rules.contains("on one of claude-code/haiku: "), "{rules}");
        assert!(!rules.contains("claude-code/opus"), "{rules}");
        assert!(!rules.contains("codex/luna"), "{rules}");
        let open = ModelPolicy::from_lists(&[], &[model("codex/luna")], None);
        let rules = reviewer_model_rules(None, &own, &open);
        assert!(rules.contains("on a model other than yours"), "{rules}");
        assert!(
            rules.contains("any model Aether lists except codex/luna"),
            "{rules}"
        );
    }

    #[test]
    fn only_the_stage_that_holds_the_review_is_told_to_record_it() {
        let plain = work_contract(Owed::default());
        assert_eq!(plain.matches("## Review on record").count(), 0);
        let holder = work_contract(owing(1, true));
        assert!(holder.starts_with(&plain), "the review rules are appended");
        let rules = &holder[plain.len()..];
        for said in [
            "## Review on record",
            "reviewed by an agent you delegate to that wrote none of it",
            "{\"review\": {\"verdict\": \"pass\", \"reviewers\": [\"<agent id>\"]}}",
            "{\"review\": {\"verdict\": \"skipped\", \"reason\": \"<why this change needed none>\"}}",
            "Paste this JSON into each reviewer's instructions",
            "never prose alone",
            "{\"verdict\": \"pass\", \"head\": \"<commit>\", \"base\": \"<commit>\"",
            "Use `\"fail\"` with findings for failures",
            "Each agent named must be a different one you delegated to yourself",
            "a passing reviewer read with `base` at or before it",
            "Name every reviewer that coverage rests on, earlier whole-change passes included",
            "write no `REVIEW:` line yourself",
        ] {
            assert!(rules.contains(said), "missing {said:?} in {rules}");
        }
    }

    /// The item's review level sets how many reviewers the holder delegates
    /// to: none, one, or several working alone. Only a ship agent judges a skip.
    #[test]
    fn the_review_rules_follow_the_run_s_review_level() {
        let leaf = work_contract(owing(0, false));
        assert_eq!(
            leaf,
            work_contract(Owed::default()),
            "it cannot delegate at all"
        );
        let mut lead = stage(StageKind::Work, Gate::None, 1);
        lead.allow_delegation = true;
        let owed = owing(0, false);
        let none = instructions(
            &lead,
            WorkspaceMode::Worktree,
            "/w",
            TeamPart::Solo,
            owed,
            None,
        );
        assert!(none.contains("review level is `none`: delegate no reviewer"));
        assert_eq!(none.matches("\"verdict\"").count(), 0, "{none}");
        let one = work_contract(owing(1, false));
        assert!(one.contains("\"reviewers\": [\"<agent id>\"]"));
        assert_eq!(
            one.matches("skipped").count(),
            0,
            "no ship agent judges a skip"
        );
        let strict = work_contract(owing(3, false));
        for said in [
            "reviewed by 3 agents you delegate to, each working alone and none of which wrote any of it",
            "\"reviewers\": [\"<agent id>\", … 3 or more]",
        ] {
            assert!(strict.contains(said), "missing {said:?} in {strict}");
        }
        assert_eq!(strict.matches("skipped").count(), 0);
    }

    /// archond ships what the record names, so the stage that writes it is
    /// told to commit, to leave the tree clean and not to deliver anything.
    #[test]
    fn only_the_record_stage_is_told_to_write_the_ship_record() {
        let plain = work_contract(Owed::default());
        assert_eq!(plain.matches("## Ship record").count(), 0);
        let owed = Owed {
            review: None,
            ship_record: true,
        };
        let recording = work_contract(owed);
        assert!(
            recording.starts_with(&plain),
            "the record rules are appended"
        );
        for said in [
            "do not push and do not open a pull request",
            "and no build cache such as `__pycache__`: Archon refuses",
            "delivers no untracked file.",
            "verifies the recorded tree against the current branch head and delivers that head",
            "\"commit\": \"<git rev-parse HEAD>\", \"tree\": \"<git rev-parse HEAD^{tree}>\"",
            "\"pr\": {\"title\":",
            "Read both hashes after the last commit",
        ] {
            assert!(recording.contains(said), "missing {said:?} in {recording}");
        }
        let review = stage(StageKind::Review, Gate::AllPass, 1);
        let judged = instructions(
            &review,
            WorkspaceMode::Worktree,
            "/w",
            TeamPart::Solo,
            owed,
            None,
        );
        assert!(judged.contains("You commit nothing: the record names the commit you judged"));
        assert_eq!(judged.matches("untracked").count(), 0, "{judged}");
    }

    #[test]
    fn the_record_stage_proposes_a_risk_level_and_a_later_review_judges_it() {
        let plain = work_contract(Owed::default());
        assert_eq!(plain.matches("## Risk level").count(), 0, "{plain}");
        let alone = work_contract(Owed {
            review: None,
            ship_record: true,
        });
        for said in [
            "## Risk level",
            "\"risk\": {\"level\": \"<low, medium or high>\"",
            "nothing merges or gates by it, so never merge on its account",
            "high two who read the code",
            "  - `high`: security, auth, permissions",
            "  - When in doubt, pick the higher level.",
            "never a local path",
            "Without a valid `risk` key Archon labels it `high`.",
        ] {
            assert!(alone.contains(said), "missing {said:?} in {alone}");
        }
        assert!(!alone.contains("risk.reviewer"), "no reviewer to relay");
        let held = work_contract(Owed {
            ship_record: true,
            ..owing(1, false)
        });
        assert!(held.contains("Relay the call your pass rests on as `risk.reviewer`"));
        let review = stage(StageKind::Review, Gate::AllPass, 1);
        let mode = WorkspaceMode::Worktree;
        let later = instructions(&review, mode, "/w", TeamPart::Solo, Owed::default(), None);
        assert_eq!(later.matches("## Risk level").count(), 1, "{later}");
        assert!(later.contains("confirm it, or change it with a reason"));
        assert!(later.contains("\"risk\": {\"level\": \"<low, medium or high>\", \"why\": \"…\"}"));
        let ship = stage(StageKind::Ship, Gate::None, 1);
        let agent = instructions(&ship, mode, "/w", TeamPart::Solo, Owed::default(), None);
        assert_eq!(agent.matches("## Risk level").count(), 1, "{agent}");
        assert!(agent.contains("Add it to your `structured` data as `\"risk\": {"));
    }

    /// A task that does not fit its pipeline goes back to the orchestrator
    /// with a recommendation instead of failing as if it were broken.
    #[test]
    fn every_contract_says_how_to_escalate_a_task_that_does_not_fit() {
        for kind in [StageKind::Work, StageKind::Review, StageKind::Test] {
            let gate = match kind {
                StageKind::Work => Gate::None,
                _ => Gate::AllPass,
            };
            let stage = stage(kind, gate, 1);
            let text = instructions(
                &stage,
                WorkspaceMode::Worktree,
                "/w",
                TeamPart::Solo,
                Owed::default(),
                None,
            );
            assert!(
                text.contains("- `ESCALATE:` when the task does not fit this pipeline"),
                "{kind:?}"
            );
            assert!(
                text.contains("the pipeline or split you recommend"),
                "{kind:?}"
            );
        }
    }

    #[test]
    fn readonly_mode_forbids_edits() {
        let text = instructions(
            &stage(StageKind::Work, Gate::None, 1),
            WorkspaceMode::RepoReadonly,
            "/r",
            TeamPart::Solo,
            Owed::default(),
            None,
        );
        assert!(text.contains("READ-ONLY"));
        assert!(
            text.contains("snapshot"),
            "it is not the operator's checkout"
        );
    }
}
