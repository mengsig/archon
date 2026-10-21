//! A team stage against the scripted backend: two roles in their own
//! worktrees, their branches merged onto the run branch, the budget split
//! between them, a conflict looping back with its files, a restart that
//! does not merge twice, work a role left uncommitted, off its branch or
//! mid-rebase or in a repository of its own, roles forking afresh after a
//! loop or a retry, and the
//! budget of a role replaced after an infrastructure failure.

use crate::common;

use std::path::Path;

use archon_core::id::QueueItemId;
use archon_core::notify::NotificationKind;
use archon_core::protocol::Request;
use archon_core::queue::QueueState;
use archon_core::run::{PipelineRun, StageOutcome, StageRun, StageState};
use archon_daemon::executor;
use archon_queue::store::SpendRow;
use common::team::{BUDGET_USD, branch, role_of, start_plan, start_team, worker};
use common::{Harness, completed, git, verdict};

fn checked_out(tree: &Path) -> String {
    git(tree, &["rev-parse", "--abbrev-ref", "HEAD"])
}

fn head(tree: &Path) -> String {
    git(tree, &["rev-parse", "HEAD"])
}

fn commit_file(tree: &Path, file: &str, text: &str) {
    std::fs::write(tree.join(file), text).expect("write");
    git(tree, &["add", file]);
    git(tree, &["commit", "--quiet", "-m", &format!("write {file}")]);
}

/// What the planner, kept through the loop, was told to plan again with.
async fn replan_brief(h: &Harness) -> String {
    let running = h.running().await;
    assert_eq!(running.len(), 1, "only `plan` runs: {running:?}");
    assert_eq!(running[0].stage_id, "plan");
    h.backend.resumed_with(&running[0])
}

fn merge_subjects(tree: &Path) -> Vec<String> {
    let log = git(tree, &["log", "--merges", "--format=%s"]);
    log.lines().map(str::to_owned).collect()
}

#[tokio::test]
async fn two_roles_work_apart_and_land_on_the_run_branch() {
    let h = Harness::new().await;
    let (item, run) = start_team(&h, "split work").await;
    let mut roles = h.backend.requests().split_off(1);
    assert_eq!(roles.len(), 2, "one agent per role");
    roles.sort_by(|a, b| a.labels["archon.fan"].cmp(&b.labels["archon.fan"]));
    for (request, role) in roles.iter().zip(["api", "ui"]) {
        let tree = worker(&run, role);
        assert_eq!(request.workspace, tree);
        assert_eq!(checked_out(&tree), branch(&run, role, 1));
        assert!(request.instructions.contains(&format!("role `{role}`")));
        assert_eq!(request.cost_budget_usd, Some(BUDGET_USD / 2.0));
    }

    commit_file(&worker(&run, "api"), "api.txt", "api\n");
    commit_file(&worker(&run, "ui"), "ui.txt", "ui\n");
    let settled = h.settle_running(|s| completed(&format!("{} done", role_of(s))));
    assert_eq!(settled.await.len(), 2);
    assert!(run.workspace.join("api.txt").exists());
    assert!(run.workspace.join("ui.txt").exists());
    assert_eq!(merge_subjects(&run.workspace).len(), 2);
    let key = format!("team_merge:{}:implement:1", run.id);
    assert_eq!(h.kv(&key).await.as_deref(), Some(r#"{"outcome":"merged"}"#));

    h.dispatch().await;
    let review = h.running().await;
    assert_eq!(review.len(), 1);
    assert_eq!(review[0].stage_id, "review");
    h.settle_running(|_| verdict(true)).await;
    h.dispatch().await;
    h.settle_running(|_| completed("opened a pull request"))
        .await;
    assert!(matches!(
        h.state(&item.id).await,
        QueueState::Succeeded { .. }
    ));
}

/// A crash after the first role merged: the daemon skips it and merges
/// the second, and a restart replays the outcome instead of merging again.
#[tokio::test]
async fn a_merge_interrupted_by_a_crash_is_not_repeated() {
    let mut h = Harness::new().await;
    let (_, run) = start_team(&h, "crash mid-merge").await;
    commit_file(&worker(&run, "api"), "api.txt", "api\n");
    commit_file(&worker(&run, "ui"), "ui.txt", "ui\n");
    let api_branch = branch(&run, "api", 1);
    git(
        &run.workspace,
        &["merge", "--no-ff", "--no-edit", &api_branch],
    );

    h.settle_running(|s| completed(role_of(s))).await;
    assert_eq!(
        merge_subjects(&run.workspace).len(),
        2,
        "ui merged, api kept"
    );
    h.dispatch().await;
    let spawned = h.backend.requests().len();

    h.restart();
    executor::recover(&h.ctx).await.expect("recover");
    h.dispatch().await;
    assert_eq!(merge_subjects(&run.workspace).len(), 2, "no second merge");
    let review = h.running().await;
    assert_eq!(review.len(), 1, "{review:?}");
    assert_eq!(review[0].stage_id, "review");
    assert_eq!(
        h.backend.requests().len(),
        spawned,
        "review is not respawned"
    );
}

#[tokio::test]
async fn a_conflict_loops_back_with_its_files() {
    let h = Harness::new().await;
    let (_, run) = start_team(&h, "clashing roles").await;
    commit_file(&worker(&run, "api"), "README.md", "api's readme\n");
    commit_file(&worker(&run, "ui"), "README.md", "ui's readme\n");
    h.settle_running(|s| completed(role_of(s))).await;
    let status = git(&run.workspace, &["status", "--porcelain"]);
    assert!(
        status.is_empty(),
        "the conflicted merge was aborted: {status}"
    );
    let key = format!("team_merge:{}:implement:1", run.id);
    let recorded = h.kv(&key).await.expect("the outcome is recorded");
    assert!(recorded.contains(r#""role":"ui""#), "{recorded}");

    h.dispatch().await;
    let running = h.running().await;
    assert_eq!(running.len(), 1);
    assert_eq!(
        (running[0].stage_id.as_str(), running[0].attempt),
        ("plan", 2)
    );
    let replan = replan_brief(&h).await;
    assert!(replan.contains("merging role `ui`"), "{replan}");
    assert!(replan.contains("`README.md`"), "{replan}");

    h.settle_running(|_| completed("split differently")).await;
    h.dispatch().await;
    for role in ["api", "ui"] {
        let tree = worker(&run, role);
        assert_eq!(checked_out(&tree), branch(&run, role, 2));
    }
    assert_eq!(h.running().await.len(), 2, "attempt 2 of both roles");
}

#[tokio::test]
async fn a_second_conflict_fails_the_run() {
    let h = Harness::new().await;
    let (item, run) = start_team(&h, "stubborn clash").await;
    for attempt in 1..=2 {
        commit_file(
            &worker(&run, "api"),
            "README.md",
            &format!("api {attempt}\n"),
        );
        commit_file(&worker(&run, "ui"), "README.md", &format!("ui {attempt}\n"));
        h.settle_running(|s| completed(role_of(s))).await;
        h.dispatch().await;
        if attempt == 1 {
            h.settle_running(|_| completed("split again")).await;
            h.dispatch().await;
        }
    }
    let QueueState::Failed { error, .. } = h.state(&item.id).await else {
        panic!("expected a failed run: {:?}", h.state(&item.id).await);
    };
    assert!(error.contains("conflicted"), "{error}");
    assert!(error.contains("`README.md`"), "{error}");
}

/// A restart after a conflict was recorded replays it: the run stays in
/// the loop it took, even though merging again would now be clean.
#[tokio::test]
async fn a_restart_replays_a_recorded_conflict() {
    let mut h = Harness::new().await;
    let (_, run) = start_team(&h, "conflict then crash").await;
    commit_file(&worker(&run, "api"), "README.md", "api's readme\n");
    commit_file(&worker(&run, "ui"), "README.md", "ui's readme\n");
    h.settle_running(|s| completed(role_of(s))).await;
    h.dispatch().await;
    assert_eq!(merge_subjects(&run.workspace).len(), 1, "api merged");
    let ui = worker(&run, "ui");
    git(&ui, &["reset", "--quiet", "--hard", &head(&run.workspace)]);
    commit_file(&ui, "ui.txt", "ui\n");
    let spawned = h.backend.requests().len();

    h.restart();
    executor::recover(&h.ctx).await.expect("recover");
    h.dispatch().await;
    assert_eq!(merge_subjects(&run.workspace).len(), 1, "ui is not merged");
    let running = h.running().await;
    assert_eq!(running.len(), 1, "{running:?}");
    assert_eq!(
        (running[0].stage_id.as_str(), running[0].attempt),
        ("plan", 2)
    );
    assert_eq!(h.backend.requests().len(), spawned, "nothing respawned");
}

/// A role that forgot to commit before completing: the daemon commits its
/// work on its branch, so it lands on the run branch and the PR.
#[tokio::test]
async fn a_role_that_forgot_to_commit_still_lands() {
    let h = Harness::new().await;
    let (_, run) = start_team(&h, "forgetful role").await;
    commit_file(&worker(&run, "api"), "api.txt", "api\n");
    std::fs::write(worker(&run, "ui").join("ui.txt"), "ui\n").expect("write");
    h.settle_running(|s| completed(role_of(s))).await;
    assert!(run.workspace.join("ui.txt").exists(), "ui's work merged");
    assert!(run.workspace.join("api.txt").exists());
    let listed = git(&run.workspace, &["ls-files", "ui.txt"]);
    assert_eq!(listed, "ui.txt", "committed, not copied");
    let notices = h.notices(NotificationKind::Progress).await;
    let saved: Vec<&String> = notices
        .iter()
        .filter(|n| n.contains("left uncommitted"))
        .collect();
    assert_eq!(saved.len(), 1, "{notices:?}");
    assert!(saved[0].ends_with(": ui.txt"), "{}", saved[0]);
    assert!(saved[0].contains(&branch(&run, "ui", 1)), "{}", saved[0]);
    h.dispatch().await;
    let review = h.running().await;
    assert_eq!(review.len(), 1);
    assert_eq!(review[0].stage_id, "review");
}

/// A role that committed on a branch of its own: the merge is refused and
/// loops back naming the branch, instead of shipping without its work.
#[tokio::test]
async fn a_role_off_its_branch_loops_back() {
    let h = Harness::new().await;
    let (_, run) = start_team(&h, "wandering role").await;
    let ui = worker(&run, "ui");
    git(&ui, &["switch", "--quiet", "-c", "feature"]);
    commit_file(&ui, "ui.txt", "ui\n");
    h.settle_running(|s| completed(role_of(s))).await;
    assert!(merge_subjects(&run.workspace).is_empty(), "nothing merged");
    h.dispatch().await;

    let running = h.running().await;
    assert_eq!(running.len(), 1, "{running:?}");
    assert_eq!(
        (running[0].stage_id.as_str(), running[0].attempt),
        ("plan", 2)
    );
    let replan = replan_brief(&h).await;
    assert!(replan.contains("`feature` checked out"), "{replan}");
    assert!(replan.contains(&branch(&run, "ui", 1)), "{replan}");
}

/// A role that failed mid-edit leaves its worktree dirty and is never
/// merged; the loop's next attempt of that role still starts, from the
/// run branch, with the leftovers kept on the earlier branch.
#[tokio::test]
async fn a_role_that_left_uncommitted_work_gets_its_next_attempt() {
    let h = Harness::new().await;
    let (_, run) = start_team(&h, "half-done role").await;
    let ui = worker(&run, "ui");
    commit_file(&ui, "README.md", "ui's readme\n");
    std::fs::write(ui.join("README.md"), "ui's unsaved edit\n").expect("write");
    std::fs::write(ui.join("scratch.txt"), "notes\n").expect("write");
    h.settle_running(|s| match role_of(s) {
        "ui" => StageOutcome::Failed {
            error: "ran out of ideas".into(),
            infra: false,
        },
        role => completed(role),
    })
    .await;
    assert!(
        merge_subjects(&run.workspace).is_empty(),
        "a failed team is not merged"
    );
    h.dispatch().await;
    h.settle_running(|_| completed("split differently")).await;
    h.dispatch().await;

    let running = h.running().await;
    assert_eq!(running.len(), 2, "both roles' attempt 2: {running:?}");
    assert!(running.iter().all(|s| s.attempt == 2));
    assert_eq!(checked_out(&ui), branch(&run, "ui", 2));
    assert_eq!(head(&ui), head(&run.workspace));
    let first = branch(&run, "ui", 1);
    let kept = git(&ui, &["show", &format!("{first}:scratch.txt")]);
    assert_eq!(
        kept, "notes",
        "the leftovers are committed on attempt 1's branch"
    );
    let notices = h.notices(NotificationKind::Progress).await;
    let saved = notices.iter().find(|n| n.contains("left uncommitted"));
    let saved = saved.expect("the operator is told what was committed");
    assert!(saved.ends_with(": README.md, scratch.txt"), "{saved}");
}

/// `plan` left its edit uncommitted: the roles would not see it, so they
/// are not started, and `plan` runs again with the files in its feedback.
#[tokio::test]
async fn an_uncommitted_run_worktree_loops_back_before_the_roles_start() {
    let h = Harness::new().await;
    let (_, run) = start_plan(&h, "forgot to commit").await;
    std::fs::write(run.workspace.join("README.md"), "planned\n").expect("write");
    h.settle_running(|_| completed("planned")).await;
    h.dispatch().await;
    h.dispatch().await;

    let running = h.running().await;
    assert_eq!(running.len(), 1, "{running:?}");
    assert_eq!(
        (running[0].stage_id.as_str(), running[0].attempt),
        ("plan", 2)
    );
    let requests = h.backend.requests();
    assert_eq!(requests.len(), 1, "no role agent was created");
    let replan = replan_brief(&h).await;
    assert!(
        replan.contains("uncommitted changes to README.md"),
        "{replan}"
    );
    assert!(requests[0].instructions.contains("Commit your changes on"));

    git(&run.workspace, &["commit", "--quiet", "-am", "plan"]);
    h.settle_running(|_| completed("planned and committed"))
        .await;
    h.dispatch().await;
    assert_eq!(h.running().await.len(), 2, "both roles start");
    for role in ["api", "ui"] {
        assert_eq!(head(&worker(&run, role)), head(&run.workspace));
    }
}

/// A retried item runs again on the same run branch and directories: its
/// roles fork new branches from the run branch as it is now, not the
/// earlier run's branches.
#[tokio::test]
async fn a_retried_item_forks_its_roles_afresh() {
    let h = Harness::new().await;
    let (item, first) = start_team(&h, "retry a team").await;
    commit_file(&worker(&first, "api"), "api.txt", "api\n");
    commit_file(&worker(&first, "ui"), "ui.txt", "ui\n");
    h.settle_running(|s| completed(role_of(s))).await;
    h.dispatch().await;
    h.settle_running(|_| verdict(false)).await;
    assert!(matches!(h.state(&item.id).await, QueueState::Failed { .. }));

    let retry = Request::RetryItem {
        item: item.id.clone(),
        model_plan: None,
        model_reasons: None,
        fresh: false,
        brief_append: None,
        stage_timeouts: Default::default(),
    };
    h.cli(retry).await.expect("retry");
    h.dispatch().await;
    let second = latest_run(&h, &item.id).await;
    assert_ne!(second.id, first.id);
    assert_eq!(
        (&second.workspace, &second.branch),
        (&first.workspace, &first.branch)
    );
    commit_file(&second.workspace, "plan.txt", "plan 2\n");
    h.settle_running(|_| completed("planned again")).await;
    h.dispatch().await;

    assert_eq!(h.running().await.len(), 2);
    for role in ["api", "ui"] {
        let tree = worker(&second, role);
        assert_eq!(checked_out(&tree), branch(&second, role, 1));
        assert_ne!(branch(&second, role, 1), branch(&first, role, 1));
        assert_eq!(
            head(&tree),
            head(&second.workspace),
            "{role} forks from the tip"
        );
    }
}

/// A fresh retry of a team item moves the run worktree `main` aside under
/// the item's `.set-aside`, and the new run's `main` and roles start from
/// the base branch on a new run branch.
#[tokio::test]
async fn a_fresh_retried_team_item_starts_from_the_base_branch() {
    let h = Harness::new().await;
    let (item, first) = start_team(&h, "fresh team").await;
    h.settle_running(|s| completed(role_of(s))).await;
    h.dispatch().await;
    h.settle_running(|_| verdict(false)).await;
    assert!(matches!(h.state(&item.id).await, QueueState::Failed { .. }));
    let planned = head(&first.workspace);
    std::fs::write(first.workspace.join("notes.txt"), "left\n").expect("notes");

    let retry = Request::RetryItem {
        item: item.id.clone(),
        model_plan: None,
        model_reasons: None,
        fresh: true,
        brief_append: None,
        stage_timeouts: Default::default(),
    };
    h.cli(retry).await.expect("fresh retry");
    let root = first.team_root.clone().expect("a team run");
    let kept = root.join(".set-aside/main-1");
    assert_eq!(
        std::fs::read_to_string(kept.join("notes.txt")).expect("kept"),
        "left\n"
    );
    assert_eq!(
        head_of(&h, &first.branch),
        planned,
        "the old run branch stays"
    );
    h.dispatch().await;
    let second = latest_run(&h, &item.id).await;
    assert_eq!(second.workspace, first.workspace);
    assert_eq!(second.branch, format!("{}-fresh1", first.branch));
    assert_eq!(head(&second.workspace), head_of(&h, "main"));
    commit_file(&second.workspace, "plan.txt", "plan 2\n");
    h.settle_running(|_| completed("planned again")).await;
    h.dispatch().await;
    for role in ["api", "ui"] {
        let tree = worker(&second, role);
        assert_eq!(checked_out(&tree), branch(&second, role, 1));
        assert_eq!(
            head(&tree),
            head(&second.workspace),
            "{role} forks from the new run"
        );
    }
}

fn head_of(h: &Harness, branch: &str) -> String {
    git(&h.repo, &["rev-parse", &format!("refs/heads/{branch}")])
}

/// A role left a rebase stopped on a conflict, on a detached `HEAD`: the
/// merge is refused, and the loop's next attempt of the role still starts
/// on its new branch, with the rebased commit kept.
#[tokio::test]
async fn a_role_left_mid_rebase_gets_its_next_attempt() {
    let h = Harness::new().await;
    let (_, run) = start_team(&h, "rebasing role").await;
    let ui = worker(&run, "ui");
    git(&ui, &["switch", "--quiet", "-c", "side"]);
    commit_file(&ui, "README.md", "side\n");
    git(&ui, &["switch", "--quiet", &branch(&run, "ui", 1)]);
    commit_file(&ui, "README.md", "ui's readme\n");
    let rebase = std::process::Command::new("git")
        .arg("-C")
        .arg(&ui)
        .args(["rebase", "side"])
        .output()
        .expect("git runs");
    assert!(!rebase.status.success(), "the rebase stops on a conflict");
    h.settle_running(|s| completed(role_of(s))).await;
    assert!(merge_subjects(&run.workspace).is_empty(), "nothing merged");
    h.dispatch().await;
    let replan = replan_brief(&h).await;
    assert!(replan.contains("`HEAD` checked out"), "{replan}");

    h.settle_running(|_| completed("split differently")).await;
    h.dispatch().await;
    let running = h.running().await;
    assert_eq!(running.len(), 2, "both roles' attempt 2: {running:?}");
    assert_eq!(checked_out(&ui), branch(&run, "ui", 2));
    assert_eq!(head(&ui), head(&run.workspace));
    let saved = git(
        &ui,
        &["for-each-ref", "--format=%(refname)", "refs/archon/saved/"],
    );
    assert_eq!(
        saved.lines().count(),
        1,
        "the detached HEAD is kept: {saved}"
    );
}

/// A role cloned a repository into its worktree and committed it with
/// `git add -A`, as a gitlink whose git directory the repository does not
/// keep: the merge is refused, and the loop's next attempt of the role
/// still starts, in a fresh worktree, the old one moved aside and reported.
#[tokio::test]
async fn a_role_that_committed_its_own_repository_gets_its_next_attempt() {
    let h = Harness::new().await;
    let (_, run) = start_team(&h, "vendoring role").await;
    let ui = worker(&run, "ui");
    let lib = ui.join("vendor/lib");
    std::fs::create_dir_all(&lib).expect("mkdir");
    git(&lib, &["init", "--quiet"]);
    commit_file(&lib, "lib.rs", "lib\n");
    git(&ui, &["add", "-A"]);
    git(&ui, &["commit", "--quiet", "-m", "vendor lib"]);
    h.settle_running(|s| completed(role_of(s))).await;
    assert!(merge_subjects(&run.workspace).is_empty(), "nothing merged");
    h.dispatch().await;
    let replan = replan_brief(&h).await;
    assert!(
        replan.contains(": vendor/lib. The next attempt"),
        "{replan}"
    );

    h.settle_running(|_| completed("split differently")).await;
    h.dispatch().await;
    let running = h.running().await;
    assert_eq!(running.len(), 2, "both roles' attempt 2: {running:?}");
    assert_eq!(checked_out(&ui), branch(&run, "ui", 2));
    assert_eq!(
        head(&ui),
        head(&run.workspace),
        "forked from the run branch"
    );
    assert!(!ui.join("vendor").exists(), "a fresh worktree");
    let root = run.team_root.as_ref().expect("a team run");
    let kept = root.join(".set-aside/implement-ui-1");
    assert!(
        kept.join("vendor/lib/lib.rs").exists(),
        "every file is kept"
    );
    assert!(!kept.join(".git").exists(), "no longer a worktree");
    let notices = h.notices(NotificationKind::Progress).await;
    let aside: Vec<&String> = notices.iter().filter(|n| n.contains("aside")).collect();
    assert_eq!(aside.len(), 1, "{notices:?}");
    assert!(
        aside[0].contains(&kept.display().to_string()),
        "{}",
        aside[0]
    );
    assert!(aside[0].contains(&branch(&run, "ui", 1)), "{}", aside[0]);
}

/// An infrastructure failure replaces a role's agent: the replacement gets
/// what the failed agent left of its share, not a share of what is left.
#[tokio::test]
async fn a_replaced_role_keeps_what_is_left_of_its_share() {
    let h = Harness::new().await;
    let (item, _) = start_team(&h, "flaky role").await;
    let running = h.running().await;
    assert_eq!(running.len(), 2);
    for (stage, cost_usd) in running.iter().zip([2.0, 1.0]) {
        let row = SpendRow {
            agent: stage.aether_agent.clone().expect("running"),
            item: Some(item.id.clone()),
            cost_usd,
            cost_basis: archon_core::cost::CostBasis::ProviderReported,
            cost_complete: None,
            estimated_cost_usd: 0.25,
            input_tokens: 10,
            output_tokens: 5,
            at: archon_core::now(),
        };
        h.ctx.store.lock().await.record_spend(&row).expect("spend");
    }
    let ui = running.iter().find(|s| role_of(s) == "ui").expect("ui");
    let failed = StageOutcome::Failed {
        error: "engine died".into(),
        infra: true,
    };
    let agent = ui.aether_agent.clone().expect("running");
    executor::stage_settled(&h.ctx, &agent, failed)
        .await
        .expect("settles");
    h.dispatch().await;

    let replacement = h.backend.requests().last().cloned().expect("a replacement");
    assert_eq!(replacement.labels["archon.fan"], ui.fan_index.to_string());
    assert!(
        !replacement.name.contains("retry"),
        "the first replacement keeps a plain name: {}",
        replacement.name
    );
    // ui's share was 6 / 2 = 3, of which its agent spent 1.25.
    assert_eq!(replacement.cost_budget_usd, Some(1.75));
}

async fn latest_run(h: &Harness, item: &QueueItemId) -> PipelineRun {
    let runs = h.ctx.store.lock().await.runs_for_item(item).expect("runs");
    let latest = runs.into_iter().max_by_key(|r| r.started_at);
    latest.expect("the item has a run")
}

/// A role's create was sent but its reply lost: its agent may be live in
/// the worktree. Issuing it again never moves the worktree nor commits
/// what the agent is doing there, and finds the same agent.
#[tokio::test]
async fn a_role_issued_again_after_a_lost_reply_keeps_its_worktree() {
    let h = Harness::new().await;
    let (_, run) = start_plan(&h, "lost role reply").await;
    h.settle_running(|_| completed("split into api and ui"))
        .await;
    h.backend.lose_next_reply();
    h.dispatch().await;
    let stages = h.live_stages().await;
    let lost: Vec<&StageRun> = stages
        .iter()
        .filter(|s| s.state == StageState::Spawning)
        .collect();
    assert_eq!(lost.len(), 1, "one role's reply was lost: {stages:?}");
    let tree = worker(&run, role_of(lost[0]));
    git(&tree, &["switch", "--quiet", "-c", "agent-branch"]);
    std::fs::write(tree.join("draft.txt"), "in progress\n").expect("write");
    let head_before = head(&tree);

    h.dispatch().await;
    assert_eq!(h.running().await.len(), 2, "both roles run");
    assert_eq!(
        h.backend.created(),
        3,
        "plan and two roles, no second agent"
    );
    assert_eq!(
        checked_out(&tree),
        "agent-branch",
        "the worktree is not switched"
    );
    assert_eq!(head(&tree), head_before, "nothing is committed");
    let status = git(&tree, &["status", "--porcelain"]);
    assert_eq!(status, "?? draft.txt", "the agent's work is untouched");
}
