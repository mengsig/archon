use std::path::Path;
use std::time::{Duration, Instant};

use super::*;
use crate::executor::fast_check::tests::ended;

fn check(tree: &Path, template: &str) -> Check {
    Check {
        tree: "tree-1".into(),
        workspace: tree.to_owned(),
        base: "main".into(),
        template: template.into(),
        target_dir: None,
    }
}

/// `run`'s status once its check finished, within ten seconds.
async fn finished(gates: &mut Gates, run: &PipelineRunId, wake: &Notify) -> Status {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match gates.status(run) {
            Status::Running => {}
            status => return status,
        }
        assert!(Instant::now() < deadline, "the check never finished");
        let _ = tokio::time::timeout(Duration::from_millis(200), wake.notified()).await;
    }
}

#[tokio::test]
async fn a_failed_check_is_a_kept_refusal_quoting_its_output() {
    let tree = tempfile::tempdir().unwrap();
    let (mut gates, run, wake) = (
        Gates::default(),
        PipelineRunId::new(),
        Arc::new(Notify::new()),
    );
    assert!(matches!(gates.status(&run), Status::Idle));
    let failing = check(tree.path(), "echo test-x-broke >&2; exit 1");
    gates.start(&run, failing, Arc::clone(&wake));
    let Status::Finished(Ok(verdict)) = finished(&mut gates, &run, &wake).await else {
        panic!("a verdict")
    };
    assert_eq!(verdict.tree, "tree-1");
    let refusal = verdict.refusal.clone().expect("refused");
    assert!(refusal.contains("test-x-broke"), "{refusal}");
    assert!(
        refusal.contains("echo test-x-broke"),
        "names the command: {refusal}"
    );
    let Status::Finished(Ok(again)) = gates.status(&run) else {
        panic!("the verdict is kept")
    };
    assert_eq!(again, verdict);
    gates.forget(&run);
    assert!(matches!(gates.status(&run), Status::Idle));
}

/// A later run of a tree the check passed on reuses the pass; another tree,
/// another selected command, or a check that failed, runs again.
#[tokio::test]
async fn a_pass_on_a_tree_holds_for_a_later_run_of_that_tree_and_command() {
    let tree = tempfile::tempdir().unwrap();
    let mut gates = Gates::default();
    let wake = Arc::new(Notify::new());
    let counted = "echo ran >> count";
    let runs = || std::fs::read_to_string(tree.path().join("count")).unwrap();
    let mut run_with = async |check: Check| {
        let run = PipelineRunId::new();
        gates.start(&run, check, Arc::clone(&wake));
        let Status::Finished(Ok(verdict)) = finished(&mut gates, &run, &wake).await else {
            panic!("a verdict")
        };
        verdict
    };
    let first = run_with(check(tree.path(), counted)).await;
    assert_eq!(first.refusal, None);
    let reused = run_with(check(tree.path(), counted)).await;
    assert_eq!(reused, first);
    assert_eq!(runs(), "ran\n", "the check ran once");

    let other_tree = Check {
        tree: "tree-2".into(),
        ..check(tree.path(), counted)
    };
    run_with(other_tree).await;
    assert_eq!(runs(), "ran\nran\n", "another tree runs the check");
    run_with(check(tree.path(), "echo ran >> count; true")).await;
    assert_eq!(runs(), "ran\nran\nran\n", "another command runs");

    let failing = "echo ran >> count; exit 1";
    for _ in 0..2 {
        let verdict = run_with(check(tree.path(), failing)).await;
        assert!(
            verdict.refusal.is_some(),
            "a failure is never reused as a pass"
        );
    }
    assert_eq!(runs().lines().count(), 5, "a failed check runs again");
}

#[tokio::test]
async fn forgetting_a_running_gate_kills_its_check() {
    let tree = tempfile::tempdir().unwrap();
    let (mut gates, run, wake) = (
        Gates::default(),
        PipelineRunId::new(),
        Arc::new(Notify::new()),
    );
    let pid_file = tree.path().join("pid");
    gates.start(
        &run,
        check(tree.path(), "sleep 30 & echo $! > pid; wait"),
        wake,
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    while std::fs::read_to_string(&pid_file).map_or(true, |p| !p.ends_with('\n')) {
        assert!(Instant::now() < deadline, "the check never started");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(matches!(gates.status(&run), Status::Running));
    gates.forget(&run);
    let pid: i32 = std::fs::read_to_string(&pid_file)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert!(ended(pid).await, "the check {pid} outlived its gate");
}

#[tokio::test]
async fn checks_run_one_at_a_time() {
    let tree = tempfile::tempdir().unwrap();
    let (mut gates, wake) = (Gates::default(), Arc::new(Notify::new()));
    let runs = [PipelineRunId::new(), PipelineRunId::new()];
    // Both pass only if neither finds the other's directory.
    let exclusive = "mkdir held || exit 9; sleep 0.3; rmdir held";
    for run in &runs {
        gates.start(run, check(tree.path(), exclusive), Arc::clone(&wake));
    }
    for run in &runs {
        let Status::Finished(Ok(verdict)) = finished(&mut gates, run, &wake).await else {
            panic!("a verdict")
        };
        assert_eq!(verdict.refusal, None, "the checks overlapped");
    }
}
