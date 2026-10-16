use std::path::Path;

use super::*;
use crate::arms::tests::settings;
use crate::metrics::tests::result;

fn options(arms: Vec<Arm>, trials: u32) -> RunOptions {
    RunOptions {
        arms,
        trials,
        parallel: 2,
        max_usd: 10.0,
        max_trials: 100,
        timeout_factor: 1.0,
        work_dir: "/data/work".into(),
        results_dir: "/w/results".into(),
        stamp: "20260926T101500Z".into(),
        keep_workspaces: false,
    }
}

fn bundled() -> Vec<BenchTask> {
    BenchTask::load_all(&Path::new(env!("CARGO_MANIFEST_DIR")).join("tasks")).unwrap()
}

#[test]
fn trials_interleave_arms_within_each_round() {
    let specs = plan(2, &[Arm::Claude, Arm::Codex], 2);
    let order: Vec<(u32, usize, Arm)> = specs.iter().map(|s| (s.trial, s.task, s.arm)).collect();
    assert_eq!(
        order,
        [
            (1, 0, Arm::Claude),
            (1, 0, Arm::Codex),
            (1, 1, Arm::Claude),
            (1, 1, Arm::Codex),
            (2, 0, Arm::Claude),
            (2, 0, Arm::Codex),
            (2, 1, Arm::Claude),
            (2, 1, Arm::Codex),
        ]
    );
}

#[test]
fn max_trials_refuses_an_oversized_plan() {
    let mut opts = options(Arm::ALL.to_vec(), 3);
    let specs = plan(9, &opts.arms, opts.trials);
    opts.max_trials = 108;
    assert!(check_plan(&specs, &opts).is_ok());
    opts.max_trials = 107;
    let error = check_plan(&specs, &opts).unwrap_err().to_string();
    assert!(error.contains("108 trials"), "{error}");
    assert!(check_plan(&[], &opts).is_err());
}

#[test]
fn options_reject_nonsense() {
    assert!(options(vec![Arm::Claude], 1).validate().is_ok());
    assert!(options(Vec::new(), 1).validate().is_err());
    assert!(
        options(vec![Arm::Claude, Arm::Claude], 1)
            .validate()
            .is_err()
    );
    assert!(options(vec![Arm::Claude], 0).validate().is_err());
    let mut free = options(vec![Arm::Claude], 1);
    free.max_usd = 0.0;
    assert!(free.validate().is_err());
}

#[test]
fn budget_stops_new_trials_once_spent() {
    let mut budget = Budget::new(1.0);
    assert!(budget.allows_another());
    budget.record(&result(Some(0.6), None));
    budget.record(&result(None, None));
    assert!(budget.allows_another());
    budget.record(&result(None, Some(0.5)));
    assert!(!budget.allows_another());
    assert!((budget.spent_usd - 1.1).abs() < 1e-9);
    assert_eq!(budget.unpriced_trials, 1);
}

#[test]
fn budget_spend_is_a_floor_only_when_every_cost_is_billed_and_one_has_a_gap() {
    let mut budget = Budget::new(10.0);
    budget.record(&result(Some(1.0), None));
    assert_eq!(budget.spent_bound(), CostBound::Exact);
    let mut gapped = result(Some(1.0), None);
    gapped.cost_gap = Some("interrupted".into());
    budget.record(&gapped);
    assert_eq!(budget.spent_bound(), CostBound::Floor);
    assert!((budget.spent_usd - 2.0).abs() < 1e-9);
}

#[test]
fn budget_spend_mixing_an_estimate_and_a_gapped_cost_is_incomplete_in_any_order() {
    let estimate = result(None, Some(5.0));
    let mut gapped = result(Some(1.0), None);
    gapped.cost_gap = Some("interrupted".into());
    for order in [[&estimate, &gapped], [&gapped, &estimate]] {
        let mut budget = Budget::new(10.0);
        budget.record(order[0]);
        budget.record(order[1]);
        assert_eq!(budget.spent_bound(), CostBound::Incomplete);
        assert!((budget.spent_usd - 6.0).abs() < 1e-9);
    }
    let mut only_estimates = Budget::new(10.0);
    only_estimates.record(&estimate);
    assert_eq!(only_estimates.spent_bound(), CostBound::Exact);
}

#[test]
fn trial_logs_nest_by_stamp_task_arm_and_trial_away_from_workspaces() {
    let opts = options(vec![Arm::Codex], 1);
    assert_eq!(
        opts.trial_logs_dir("t", Arm::Codex, 3),
        Path::new("/w/results/20260926T101500Z/trials/t/codex/3/logs")
    );
    assert_eq!(
        opts.run_work_dir(),
        Path::new("/data/work/20260926T101500Z")
    );
    assert_eq!(
        opts.run_results_dir(),
        Path::new("/w/results/20260926T101500Z")
    );
}

#[test]
fn dry_run_prints_every_arm_command_without_touching_disk() {
    let tasks = bundled();
    let opts = options(Arm::ALL.to_vec(), 2);
    let specs = plan(tasks.len(), &opts.arms, opts.trials);
    let versions = vec![(Arm::Claude, "2.1.280 (Claude Code)".to_owned())];
    let text = render_dry_run(&tasks, &specs, &settings(), &opts, &versions).unwrap();
    assert!(text.starts_with(&format!("plan: {} tasks x 4 arms x 2 trials", tasks.len())));
    assert!(text.contains("2.1.280 (Claude Code)"));
    assert!(text.contains("--dangerously-skip-permissions"));
    assert!(text.contains("exec --json"));
    assert!(text.contains("aether create_agent"));
    assert!(text.contains("daemon run"));
    assert!(text.contains("repo_add"));
    for task in &tasks {
        assert!(text.contains(&format!("== {} (", task.file.id)));
    }
    assert!(text.contains("/data/work/20260926T101500Z/<random>/fix-off-by-one"));
    assert!(!Path::new("/data/work").exists());
}

/// Archon and plain Aether run under one `timeout_for` value per task.
#[test]
fn ledger_transfer_has_one_twenty_minute_deadline_for_both_arms() {
    let task = bundled()
        .into_iter()
        .find(|t| t.file.id == "quality-ledger-transfer")
        .unwrap();
    let mut opts = options(vec![Arm::Archon, Arm::Aether], 1);
    assert_eq!(opts.timeout_for(&task), Duration::from_secs(20 * 60));
    opts.timeout_factor = 0.5;
    assert_eq!(opts.timeout_for(&task), Duration::from_secs(10 * 60));
    let specs = plan(1, &opts.arms, 1);
    let text = render_dry_run(&[task], &specs, &settings(), &opts, &[]).unwrap();
    let section = text.split("\n== ").nth(1).unwrap();
    assert!(section.contains("timeout 600s"), "{section}");
    let (archon, aether) = (section.find("[archon]"), section.find("[aether]"));
    assert!(archon.is_some() && aether.is_some(), "{section}");
    assert_eq!(task_deadlines(&text), 1, "one shared deadline: {text}");
}

/// Counts per-task deadlines (`timeout <N>s`), not config keys such as `full_timeout`.
fn task_deadlines(text: &str) -> usize {
    text.match_indices("timeout ")
        .filter(|(at, word)| {
            let rest = &text[at + word.len()..];
            let digits = rest.chars().take_while(char::is_ascii_digit).count();
            digits > 0 && rest[digits..].starts_with('s')
        })
        .count()
}

#[test]
fn the_default_work_dir_is_outside_the_checkout() {
    let dir = |xdg: Option<&str>, home: Option<&str>| {
        work_dir_from(xdg.map(Into::into), home.map(Into::into))
    };
    assert_eq!(
        dir(Some("/x/data"), Some("/users/u")).unwrap(),
        Path::new("/x/data/archon-bench/work")
    );
    assert_eq!(
        dir(Some("relative"), Some("/users/u")).unwrap(),
        Path::new("/users/u/.local/share/archon-bench/work")
    );
    assert!(dir(None, None).is_err());
    let tasks = Path::new(env!("CARGO_MANIFEST_DIR")).join("tasks");
    let protected = crate::isolation::Protected::for_run(&tasks, &default_results_dir()).unwrap();
    let old_default = Path::new(env!("CARGO_MANIFEST_DIR")).join(".work");
    assert!(protected.check_work_dir(&old_default).is_err());
}
