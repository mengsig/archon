//! The stage contract (ARCHITECTURE §8.6): how every stage agent must work
//! and report, passed as `config.instructions` so pipeline authors cannot
//! omit it. The stage prompt says *what* to do; this says *how to finish*.

use archon_core::catalog::{Gate, StageKind, WorkspaceMode};
use archon_core::run::StageOutcome;
use serde::Serialize;

use crate::spec::StageSpec;
use crate::team::TeamSlot;

mod initiative;
mod lessons;

pub use initiative::initiative_rules;
pub use lessons::lesson_rules;

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

/// The contract for one stage agent working in `workspace`; `holds_review` when
/// its stage is the pipeline's review. See [`delegation_rules`] for delegators.
pub fn instructions(
    stage: &StageSpec,
    mode: WorkspaceMode,
    workspace: &str,
    team: TeamPart<'_>,
    holds_review: bool,
) -> String {
    assert!(!workspace.is_empty(), "a stage always has a workspace");
    assert!(stage.fan_out >= 1, "validated specs have fan_out >= 1");
    let mut out = contract_header(stage, mode, workspace, team);
    out.push_str(kind_rules(stage.kind));
    if let TeamPart::RunTree { branch } = team
        && stage.kind == StageKind::Work
    {
        out.push_str(&run_tree_rule(branch));
    }
    if needs_verdict(stage) {
        out.push_str(VERDICT_RULES);
    }
    if holds_review {
        out.push_str(REVIEW_EVIDENCE_RULES);
    }
    out
}

/// The contract's section for a stage agent that can delegate. `models` is the
/// policy its children are held to, in words (`ModelPolicy::describe`).
pub fn delegation_rules(mode: WorkspaceMode, models: &str) -> String {
    assert!(!models.trim().is_empty(), "the policy is always stated");
    let workspaces = match mode {
        WorkspaceMode::Worktree | WorkspaceMode::Scratch => DELEGATE_WRITABLE,
        WorkspaceMode::RepoReadonly => DELEGATE_READ_ONLY,
    };
    format!(
        "\n## Delegation\n\n\
         - You can hand scoped parts of this stage to child agents with Aether's \
         `delegate`. Pass `leaf: true` for every child: a child does not delegate further.\n\
         {workspaces}\
         - Choose each child's model and effort within Archon's policy for this item: \
         {models}. Archon cancels a child outside it. Aether's `list_models` lists what \
         is connected, not what is allowed.\n\
         - Wait for every child (`wait_for`) or cancel it (`cancel_agent`) before you \
         finish: Archon cancels whatever still runs when your stage ends, and that work \
         is lost.\n\
         - A child that finished cannot be asked again. For a second look after a change, \
         delegate a new child.\n\
         {DELEGATE_HAND_OFF}\
         {DELEGATE_CHILD_RULES}"
    )
}

/// An item the stage's item was queued after, as Archon's store has it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Dependency {
    pub item: String,
    /// Its branch in the repository of the stage's item, if it has one there.
    pub branch: Option<String>,
    pub pull_request: Option<String>,
}

/// The contract's section for an item queued after `dependencies`, whose own
/// branch was forked from `base`: where their work is. Empty when there are none.
pub fn dependency_rules(base: &str, dependencies: &[Dependency]) -> String {
    if dependencies.is_empty() {
        return String::new();
    }
    assert!(!base.trim().is_empty(), "a worktree is forked from a base");
    let facts: String = dependencies.iter().map(dependency_fact).collect();
    assert_eq!(
        facts.lines().count(),
        dependencies.len(),
        "one line of facts per dependency"
    );
    format!(
        "\n## Dependencies\n\n\
         Archon started this item after the items below succeeded. Its branch was forked \
         from `{base}`, not from theirs: your workspace holds their work only where that \
         was merged into `{base}` or you bring it in. One JSON object per item; a `branch` \
         is a branch of this repository.\n\n\
         {facts}\n\
         - A change built on one of these branches is stacked on it. For each `branch`, \
         take `git merge-base <branch> HEAD`: when `git merge-base --is-ancestor <that \
         commit> {base}` fails, that commit is the base of this change, not `{base}`. With \
         several such commits, the base is the one that contains the others.\n\
         - Diff, review and report a stacked change from its base. Name that commit \
         wherever a base commit is asked for, and give it to every reviewer you delegate \
         to: a diff from `{base}` would include the dependency's change.\n"
    )
}

fn dependency_fact(dependency: &Dependency) -> String {
    assert!(!dependency.item.is_empty(), "a dependency is a queued item");
    let fact = serde_json::to_string(dependency).expect("a record of strings serializes");
    assert!(!fact.contains('\n'), "compact JSON is one line");
    format!("- {fact}\n")
}

const DELEGATE_WRITABLE: &str = "\
- A child that edits files gets its own copy: pass `workspace_mode: \"isolated\"`. \
Read its result, then bring only the changes you accept into your workspace with \
`integrate_workspace`; nothing integrates by itself. A child that only reads (a \
reviewer, a researcher) shares your workspace, so it sees the change as it stands.\n";

const DELEGATE_HAND_OFF: &str = "\
- A child starts cold and pays to read whatever you do not tell it. Hand it what you \
already hold, in a few lines: what done means, the paths that matter and what you \
found there. Never your conversation or the project's history. Read a bounded local \
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
    let blocked = StageOutcome::BLOCKED_PREFIX;
    format!(
        "# Archon stage contract\n\n\
         You are one stage (`{id}`, kind `{kind}`) of an Archon pipeline, running \
         unattended. Archon reads your result mechanically, so follow this contract \
         exactly. It overrides any conflicting habit or instruction.\n\n\
         ## Workspace\n\n\
         - Work only in `{workspace}`. {mode_rule}\n\
         - Do not ask the operator questions and do not wait for replies: nobody is \
         watching this agent. Make the conservative choice, and state every assumption \
         in your summary.\n\
         - Use `mcp__archon__message_orchestrator` to relay operator messages meant for \
         the orchestrator (`kind=relay`, quote their text exactly), escalate blockers \
         outside your stage (`kind=escalation`), or send low-priority `info`. It returns \
         a receipt immediately. Keep working on your stage afterwards: never wait for \
         a reply, end your turn, or complete the stage because you sent a message. \
         Your passive run-node parent cannot answer; do not use raw `send_message` \
         to reach the orchestrator. Other Archon mutation tools are reserved for it.\n\
         {fan_rule}\n\
         ## Finishing\n\n\
         - When done, call Aether's `complete_task` exactly once with a concise \
         `summary`: plain prose, what you did or found, how you verified it, and what \
         is left undone. Later stages and the orchestrator read only this.\n\
         - If you cannot proceed without a human (missing credentials or access, a \
         contradictory or ambiguous brief with no safe default, an irreversible action \
         the brief does not authorise), call `fail_task` with an error that starts \
         with `{blocked}` followed by exactly what the operator must provide or decide. \
         This escalates to the operator and is never retried automatically.\n\
         - For any other failure, call `fail_task` with a precise error: what failed, \
         what you tried, and the evidence. Never report a failure as completed.\n",
        id = stage.id,
        kind = kind_name(stage.kind),
        mode_rule = mode_rule(mode),
        fan_rule = match team {
            TeamPart::Role(slot) => team_rule(stage, slot),
            TeamPart::Solo | TeamPart::RunTree { .. } => fan_rule(stage),
        },
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
            "It is a git worktree dedicated to this pipeline run; earlier and later \
             stages of the same run work in it too. Do not touch other worktrees, other \
             branches, or the operator's own checkout."
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

const DELEGATE_CHILD_RULES: &str = "\
- A child inherits none of this contract or of the stage prompt: Aether gives it \
only the `objective` and `instructions` you pass to `delegate`. In every child's \
`instructions`, state each rule below that applies to its role, in your own words \
for its task:\n  \
- work only in the workspace it is given, and never touch other worktrees, other \
branches or the operator's own checkout;\n  \
- do not push;\n  \
- never override the git identity: no `git -c user.name` or `user.email`, no \
`--author`, no `GIT_AUTHOR_*` or `GIT_COMMITTER_*`, no identity taken from injected \
context;\n  \
- do not ask the operator questions or wait for replies;\n  \
- read only, with no edits, commits, stashes or branch switches: for a reviewer \
always, and for every child when your own workspace is READ-ONLY.\n";

const REVIEW_EVIDENCE_RULES: &str = "\n## Review on record\n\n\
- This pipeline counts your stage as its review, so the change is reviewed before \
you finish: by an agent you delegate to that wrote none of it, on the change as it \
finally stands. Only a later change the stage prompt says needs no second look may \
follow the pass.\n\
- `complete_task` must include `structured` data saying how that went (other keys \
may sit beside it):\n\n\
```json\n\
{\"review\": {\"verdict\": \"pass\", \"reviewer_model\": \"<provider>/<model>\"}}\n\
```\n\n\
or, only where the stage prompt lets a change go without review:\n\n\
```json\n\
{\"review\": {\"verdict\": \"skipped\", \"reason\": \"<why this change needed none>\"}}\n\
```\n\n\
- Archon fails the run here, and nothing is shipped, when the object is missing, the \
verdict is anything else, or a pass names no reviewer. It checks a pass against \
Aether: `reviewer_model` must be the model of an agent you delegated to yourself that \
completed, not one you cancelled or that failed. Never report `pass` for a review \
that did not run or did not pass: if a blocking finding stays open, call `fail_task` \
and say which.\n\
- Archon adds the review as the last line of your summary for the later stages. Do \
not write a `REVIEW:` line yourself.\n";

const WORK_RULES: &str = "\n## Work stage\n\n\
- Your summary is the hand-off to later stages and the orchestrator. Use the report \
format the stage prompt gives. If it gives none, list what changed and why, the files \
touched, the exact verification commands you ran with their results, and any known \
gaps.\n";

const TEAM_RULES: &str = "\n## Team stage\n\n\
- Your summary is the hand-off to later stages and the orchestrator. Use the report \
format the stage prompt gives. If it gives none, list what your role changed and why, \
the files touched, the commits you made, the verification commands you ran with their \
results, any assumption you made about a peer's work, and any known gaps.\n\
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
- `complete_task` must include `structured` data of this shape (other keys may sit \
beside it):\n\n\
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
            allow_delegation: false,
            nudge: None,
            on_fail: None,
            roles: Vec::new(),
            role_prompts: Default::default(),
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
                false,
            );
            assert!(text.contains("Work only in `/w`"), "{kind:?}");
            assert!(text.contains("`BLOCKED:`"), "{kind:?}");
            assert!(text.contains("complete_task"), "{kind:?}");
            assert!(
                text.contains("mcp__archon__message_orchestrator"),
                "{kind:?}"
            );
            assert!(
                text.contains("Keep working on your stage afterwards"),
                "{kind:?}"
            );
            assert!(
                text.contains("never wait for a reply, end your turn"),
                "{kind:?}"
            );
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
                false,
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
            false,
        );
        assert!(!work.contains("VERDICT: PASS"));
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
            false,
        );
        assert!(text.contains("\"reports\""));
        assert!(text.contains("one of 2 agents"));
        assert!(!text.contains("VERDICT: PASS"));
        assert!(
            instructions(&gated, WorkspaceMode::Worktree, "/w", TeamPart::Solo, false)
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
            false,
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
            instructions(&stage, WorkspaceMode::Worktree, "/w", TeamPart::Solo, false)
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
        assert!(!of(StageKind::Work).contains("HEAD^{tree}"));
    }

    #[test]
    fn propose_contract_forbids_edits_and_names_the_page() {
        let text = instructions(
            &stage(StageKind::Propose, Gate::None, 1),
            WorkspaceMode::Worktree,
            "/w",
            TeamPart::Solo,
            false,
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
        let text = instructions(&team, WorkspaceMode::Worktree, "/t/s-ui", part, false);
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
            false,
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
            false,
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
                false,
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
            false,
        );
        assert!(!solo.contains("team stage"));
    }

    /// The engine's contract holds no delegation rules: who can delegate is
    /// decided where the agent is created, which adds [`delegation_rules`].
    #[test]
    fn delegation_rules_fit_the_workspace_and_state_the_model_policy() {
        let mut work = stage(StageKind::Work, Gate::None, 1);
        work.allow_delegation = true;
        let contract = instructions(&work, WorkspaceMode::Worktree, "/w", TeamPart::Solo, false);
        assert_eq!(contract.matches("delegate").count(), 0, "{contract}");

        let writable = delegation_rules(WorkspaceMode::Worktree, "a/one, b/two");
        assert!(writable.starts_with("\n## Delegation\n"), "{writable}");
        for said in [
            "Pass `leaf: true` for every child",
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
            delegation_rules(WorkspaceMode::Scratch, "a/one, b/two")
        );

        let read_only = delegation_rules(WorkspaceMode::RepoReadonly, "a/one");
        assert!(read_only.contains("Your workspace is READ-ONLY, and so is every child's"));
        assert!(read_only.contains("Do not pass `workspace_mode: \"isolated\"`"));
        assert_eq!(
            read_only.matches("integrate_workspace").count(),
            0,
            "{read_only}"
        );
        assert!(read_only.contains("policy for this item: a/one. Archon cancels"));
    }

    /// A child rereads what it is not told, and a reviewer without the exact
    /// commits reviews the wrong diff: every delegating agent is told both.
    #[test]
    fn delegation_rules_say_what_a_child_and_a_reviewer_are_handed() {
        for mode in [WorkspaceMode::Worktree, WorkspaceMode::RepoReadonly] {
            let rules = delegation_rules(mode, "a/one");
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

    /// A dependent item's branch still forks from the base, so a diff from
    /// the base holds the dependency's change once the lead builds on it.
    #[test]
    fn dependency_rules_name_each_branch_and_the_base_of_a_stacked_change() {
        assert_eq!(dependency_rules("main", &[]), "");
        let stacked = Dependency {
            item: "qi_01".into(),
            branch: Some("archon/first-01".into()),
            pull_request: Some("https://github.com/o/r/pull/1".into()),
        };
        let elsewhere = Dependency {
            item: "qi_02".into(),
            branch: None,
            pull_request: None,
        };
        let rules = dependency_rules("main", &[stacked, elsewhere]);
        assert!(rules.starts_with("\n## Dependencies\n\n"), "{rules}");
        let facts: Vec<serde_json::Value> = rules
            .lines()
            .filter_map(|line| line.strip_prefix("- ").filter(|fact| fact.starts_with('{')))
            .map(|fact| serde_json::from_str(fact).expect("each fact is JSON"))
            .collect();
        let pull_request = "https://github.com/o/r/pull/1";
        let expected = [
            serde_json::json!({"item": "qi_01", "branch": "archon/first-01", "pull_request": pull_request}),
            serde_json::json!({"item": "qi_02", "branch": null, "pull_request": null}),
        ];
        assert_eq!(facts, expected);
        for said in [
            "Its branch was forked from `main`, not from theirs",
            "take `git merge-base <branch> HEAD`: when `git merge-base --is-ancestor <that \
             commit> main` fails, that commit is the base of this change, not `main`",
            "give it to every reviewer you delegate to",
        ] {
            assert!(rules.contains(said), "missing {said:?} in {rules}");
        }
    }

    /// Aether gives a child only the instructions `delegate` was passed, so
    /// a rule the delegating agent does not repeat does not reach the child.
    #[test]
    fn a_delegating_agent_must_pass_its_rules_on_in_each_childs_instructions() {
        for mode in [WorkspaceMode::Worktree, WorkspaceMode::RepoReadonly] {
            let rules = delegation_rules(mode, "a/one");
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

    #[test]
    fn only_the_stage_that_holds_the_review_is_told_to_record_it() {
        let work = stage(StageKind::Work, Gate::None, 1);
        let plain = instructions(&work, WorkspaceMode::Worktree, "/w", TeamPart::Solo, false);
        assert_eq!(plain.matches("## Review on record").count(), 0);
        let holder = instructions(&work, WorkspaceMode::Worktree, "/w", TeamPart::Solo, true);
        assert!(holder.starts_with(&plain), "the review rules are appended");
        let rules = &holder[plain.len()..];
        for said in [
            "## Review on record",
            "{\"review\": {\"verdict\": \"pass\", \"reviewer_model\": \"<provider>/<model>\"}}",
            "{\"review\": {\"verdict\": \"skipped\", \"reason\": \"<why this change needed none>\"}}",
            "Do not write a `REVIEW:` line yourself",
        ] {
            assert!(rules.contains(said), "missing {said:?} in {rules}");
        }
    }

    #[test]
    fn readonly_mode_forbids_edits() {
        let text = instructions(
            &stage(StageKind::Work, Gate::None, 1),
            WorkspaceMode::RepoReadonly,
            "/r",
            TeamPart::Solo,
            false,
        );
        assert!(text.contains("READ-ONLY"));
        assert!(
            text.contains("snapshot"),
            "it is not the operator's checkout"
        );
    }
}
