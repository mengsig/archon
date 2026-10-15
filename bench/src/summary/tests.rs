use super::*;
use crate::metrics::tests::result;
use crate::metrics::{ArchonItem, TimeoutGrade};
use crate::workspace::tests::scratch;
use archon_core::autonomy::ReviewLevel;

fn trial(task: &str, arm: Arm, success: bool, wall: f64, cost: f64) -> TrialResult {
    TrialResult {
        task: task.into(),
        arm,
        success,
        wall_secs: wall,
        input_tokens: Some(1000),
        output_tokens: Some(100),
        ..result(Some(cost), None)
    }
}

#[test]
fn stats_use_the_sample_standard_deviation() {
    let s = Stat::of(&[2.0, 4.0, 4.0, 4.0, 5.0, 5.0, 7.0, 9.0]).unwrap();
    assert_eq!(s.n, 8);
    assert!((s.mean - 5.0).abs() < 1e-12);
    assert!((s.stdev - (32.0f64 / 7.0).sqrt()).abs() < 1e-12);
    assert_eq!(Stat::of(&[3.0]).unwrap().stdev, 0.0);
    assert!(Stat::of(&[]).is_none());
}

#[test]
fn pass_at_k_matches_the_closed_form() {
    assert!((pass_at_k(4, 1, 1) - 0.25).abs() < 1e-12);
    // 1 - C(3,2)/C(4,2) = 1 - 3/6.
    assert!((pass_at_k(4, 1, 2) - 0.5).abs() < 1e-12);
    assert_eq!(pass_at_k(4, 0, 4), 0.0);
    assert_eq!(pass_at_k(4, 1, 4), 1.0);
    assert_eq!(pass_at_k(3, 3, 1), 1.0);
}

#[test]
fn summary_aggregates_per_arm_and_per_task() {
    let results = vec![
        trial("a", Arm::Claude, true, 10.0, 1.0),
        trial("a", Arm::Claude, false, 20.0, 3.0),
        trial("b", Arm::Claude, true, 30.0, 2.0),
        trial("b", Arm::Claude, true, 40.0, 2.0),
        trial("a", Arm::Archon, true, 50.0, 4.0),
        trial("a", Arm::Archon, true, 70.0, 4.0),
    ];
    let summary = Summary::build(&results, None).unwrap();
    assert_eq!(summary.k, 2);
    let arms: Vec<Arm> = summary.arms.iter().map(|a| a.arm).collect();
    assert_eq!(
        arms,
        [Arm::Archon, Arm::Claude],
        "arms keep their canonical order"
    );
    let claude = &summary.arms[1];
    assert_eq!((claude.trials, claude.successes), (4, 3));
    assert!((claude.success_rate - 0.75).abs() < 1e-12);
    assert!(
        (claude.pass_at_1 - 0.75).abs() < 1e-12,
        "mean of 0.5 and 1.0"
    );
    assert!((claude.pass_at_k - 1.0).abs() < 1e-12);
    assert!((claude.wall_secs.mean - 25.0).abs() < 1e-12);
    assert!((claude.cost_usd.unwrap().mean - 2.0).abs() < 1e-12);
    assert_eq!(summary.tasks["a"].len(), 2);
    assert_eq!(summary.tasks["b"].len(), 1);
}

#[test]
fn partial_scores_and_timeouts_are_reported() {
    let mut scored = trial("a", Arm::Codex, false, 5.0, 0.5);
    scored.score = Some(0.75);
    let mut slow = trial("a", Arm::Codex, false, 900.0, 1.0);
    slow.timed_out = true;
    let summary = Summary::build(&[scored, slow], None).unwrap();
    let codex = &summary.arms[0];
    assert_eq!((codex.trials, codex.infra, codex.timeouts), (2, 0, 1));
    assert!((codex.score.mean - 0.375).abs() < 1e-12);
    assert!(Summary::build(&[], None).is_err());
}

#[test]
fn infra_failures_are_left_out_of_every_aggregate() {
    let mut limited = trial("a", Arm::Claude, false, 10.0, 0.11);
    limited.trial = 2;
    limited.error = Some("claude reported success: You've hit your session limit".into());
    let mut misgraded = trial("b", Arm::Claude, false, 120.0, 0.72);
    misgraded.error = Some("grading: merge failed".into());
    let mut noisy_pass = trial("a", Arm::Claude, true, 40.0, 1.0);
    noisy_pass.error = Some("stopping archond: killed".into());
    let results = vec![
        trial("a", Arm::Claude, true, 30.0, 0.5),
        limited,
        misgraded,
        noisy_pass,
        trial("a", Arm::Codex, true, 50.0, 0.5),
    ];
    let summary = Summary::build(&results, None).unwrap();

    let claude = summary.arms.iter().find(|a| a.arm == Arm::Claude).unwrap();
    assert_eq!((claude.trials, claude.successes, claude.infra), (2, 2, 2));
    assert!((claude.success_rate - 1.0).abs() < 1e-12);
    assert!((claude.wall_secs.mean - 35.0).abs() < 1e-12);
    assert_eq!(summary.k, 1, "codex has one graded trial of `a`");
    assert!(
        summary.tasks["b"].is_empty(),
        "a cell of infra failures only has no aggregate"
    );
    let listed: Vec<(&str, u32)> = summary
        .infra
        .iter()
        .map(|f| (f.task.as_str(), f.trial))
        .collect();
    assert_eq!(listed, [("a", 2), ("b", 1)]);
    let md = render_markdown(&summary).unwrap();
    assert!(md.contains("## Infrastructure failures"), "{md}");
    assert!(
        md.contains("- a / claude #2: claude reported success"),
        "{md}"
    );
    assert!(md.contains("| b | — | — |"), "{md}");
}

#[test]
fn a_run_of_infra_failures_only_has_no_summary() {
    let mut broken = trial("a", Arm::Codex, false, 1.0, 0.0);
    broken.error = Some("codex exited 1".into());
    let error = Summary::build(&[broken], None).unwrap_err().to_string();
    assert!(error.contains("infrastructure"), "{error}");
}

#[test]
fn summary_files_are_written_from_results_jsonl() {
    let tmp = scratch();
    let lines: Vec<String> = [
        trial("a", Arm::Claude, true, 10.0, 1.0),
        trial("a", Arm::Codex, false, 12.0, 0.0),
    ]
    .iter()
    .map(|r| serde_json::to_string(r).unwrap())
    .collect();
    std::fs::write(tmp.path().join("results.jsonl"), lines.join("\n") + "\n").unwrap();
    let summary = write_summary(tmp.path()).unwrap();
    assert_eq!(summary.arms.len(), 2);
    let md = std::fs::read_to_string(tmp.path().join("summary.md")).unwrap();
    assert!(md.contains("| claude | `m` | 1 | 1/1 (100%)"), "{md}");
    assert!(
        md.contains("| a | 1/1 · 10 ± 0 · 1.00 ± 0.00 · 1.0 ± 0.0 | 0/1 · 12 ± 0 · 0.00 ± 0.00 · 1.0 ± 0.0 |"),
        "{md}"
    );
    let chart: Summary =
        serde_json::from_str(&std::fs::read_to_string(tmp.path().join("chart.json")).unwrap())
            .unwrap();
    assert_eq!(chart, summary);
}

#[test]
fn metrics_only_some_trials_reported_show_their_n() {
    let mut unpriced = trial("a", Arm::Codex, true, 8.0, 0.0);
    unpriced.cost_usd = None;
    unpriced.input_tokens = None;
    let results = vec![
        trial("a", Arm::Codex, true, 10.0, 1.0),
        trial("a", Arm::Codex, false, 20.0, 3.0),
        unpriced,
    ];
    let md = render_markdown(&Summary::build(&results, None).unwrap()).unwrap();
    assert!(md.contains("| 1000 ± 0 (n=2) |"), "{md}");
    assert!(md.contains("| 2.00 ± 1.41 (n=2) |"), "{md}");
    assert!(
        md.contains("| 100 ± 0 |"),
        "output tokens came from all three: {md}"
    );
    assert!(md.contains("2/3 · 13 ± 6 · 2.00 ± 1.41 (n=2) ·"), "{md}");
}

fn routed(pipeline: &str, review: Option<ReviewLevel>) -> ArchonItem {
    ArchonItem {
        pipeline: pipeline.into(),
        run_pipelines: Vec::new(),
        review,
    }
}

#[test]
fn routing_buckets_measure_whole_trials_without_duplicating_items() {
    let mut pass = trial("a", Arm::Archon, true, 10.0, 1.0);
    pass.archon_work = Some(vec![routed("direct", Some(ReviewLevel::Standard)); 2]);
    let mut fail = pass.clone();
    fail.success = false;
    fail.wall_secs = 30.0;
    fail.cost_usd = None;
    fail.timed_out = true;
    let mut infra = pass.clone();
    infra.success = false;
    infra.error = Some("provider unavailable".into());
    infra.wall_secs = 999.0;
    let summary = Summary::build(&[pass, fail, infra], None).unwrap();
    let pipeline = &summary.pipelines["direct"];
    assert_eq!(
        (pipeline.trials, pipeline.successes, pipeline.infra),
        (2, 1, 1)
    );
    assert_eq!(
        (pipeline.success_rate, pipeline.wall_secs.mean),
        (0.5, 20.0)
    );
    assert_eq!(pipeline.cost_usd.unwrap().n, 1);
    assert_eq!(pipeline.cost_usd.unwrap().mean, 1.0);
    assert_eq!(pipeline.timeouts, 1);
    assert_eq!(summary.reviews["standard"], *pipeline);
    let md = render_markdown(&summary).unwrap();
    assert!(md.contains("## Per pipeline") && md.contains("## Per review level"));
    assert!(md.contains("| direct | 2 | 1/2 (50%) | 20 ± 14 | 1.00 ± 0.00 (n=1)"));
}

#[test]
fn routing_distinguishes_unknown_empty_mixed_and_non_archon_trials() {
    let unknown = trial("a", Arm::Archon, true, 1.0, 1.0);
    let mut empty = unknown.clone();
    empty.archon_work = Some(Vec::new());
    let mut mixed = unknown.clone();
    let mut item = routed("adaptive", Some(ReviewLevel::Strict));
    item.run_pipelines = vec!["direct".into(), "adaptive".into()];
    mixed.archon_work = Some(vec![item, routed("adaptive", None)]);
    let other = trial("a", Arm::Codex, false, 999.0, 999.0);
    let summary = Summary::build(&[unknown, empty, mixed, other], None).unwrap();
    assert_eq!(summary.pipelines.len(), 3);
    assert_eq!(summary.pipelines["unknown (no capture)"].trials, 1);
    assert_eq!(summary.pipelines["no work (empty queue)"].trials, 1);
    assert_eq!(summary.pipelines["mixed: adaptive + direct"].trials, 1);
    assert_eq!(summary.reviews["mixed: strict + unknown"].trials, 1);
    assert_eq!(summary.pipelines.values().map(|a| a.trials).sum::<u32>(), 3);
    assert_eq!(summary.reviews.values().map(|a| a.trials).sum::<u32>(), 3);
}

#[test]
fn no_review_is_distinct_from_missing_review_and_metadata_round_trips() {
    let mut none = trial("a", Arm::Archon, true, 10.0, 1.0);
    none.archon_work = Some(vec![routed("direct", Some(ReviewLevel::None))]);
    let mut unknown = none.clone();
    unknown.archon_work.as_mut().unwrap()[0].review = None;
    let encoded = serde_json::to_string(&none).unwrap();
    assert_eq!(serde_json::from_str::<TrialResult>(&encoded).unwrap(), none);
    let summary = Summary::build(&[none, unknown], None).unwrap();
    assert_eq!(summary.reviews["none"].trials, 1);
    assert_eq!(summary.reviews["unknown"].trials, 1);
    let mut legacy = serde_json::to_value(&summary).unwrap();
    legacy.as_object_mut().unwrap().remove("pipelines");
    legacy.as_object_mut().unwrap().remove("reviews");
    let parsed: Summary = serde_json::from_value(legacy).unwrap();
    assert!(parsed.pipelines.is_empty() && parsed.reviews.is_empty());
}

#[test]
fn old_result_files_hydrate_captures_but_persisted_metadata_wins() {
    let tmp = scratch();
    let result = trial("a", Arm::Archon, true, 1.0, 1.0);
    let path = tmp.path().join("results.jsonl");
    std::fs::write(&path, serde_json::to_string(&result).unwrap()).unwrap();
    assert!(read_results(&path).unwrap()[0].archon_work.is_none());
    let logs = tmp.path().join("trials/a/archon/1/logs");
    std::fs::create_dir_all(&logs).unwrap();
    let capture = logs.join("archon-items.json");
    std::fs::write(&capture, r#"[{"item":{"pipeline":"direct"},"runs":[]}]"#).unwrap();
    let loaded = read_results(&path).unwrap();
    assert_eq!(loaded[0].archon_work, Some(vec![routed("direct", None)]));
    let summary = write_summary(tmp.path()).unwrap();
    assert_eq!(summary.reviews["unknown"].trials, 1);
    std::fs::write(&path, serde_json::to_string(&loaded[0]).unwrap()).unwrap();
    std::fs::write(&capture, "broken capture").unwrap();
    assert_eq!(read_results(&path).unwrap(), loaded);
    std::fs::write(&path, serde_json::to_string(&result).unwrap()).unwrap();
    let error = read_results(&path).unwrap_err();
    assert!(format!("{error:#}").contains("archon-items.json"));
}

#[test]
fn invalid_routing_and_capture_paths_are_rejected() {
    let mut invalid = trial("a", Arm::Codex, true, 1.0, 1.0);
    invalid.archon_work = Some(Vec::new());
    assert!(Summary::build(&[invalid.clone()], None).is_err());
    invalid.arm = Arm::Archon;
    invalid.archon_work = Some(vec![routed(" ", None)]);
    assert!(Summary::build(&[invalid.clone()], None).is_err());
    invalid.archon_work = None;
    invalid.task = "../outside".into();
    let tmp = scratch();
    let path = tmp.path().join("results.jsonl");
    std::fs::write(&path, serde_json::to_string(&invalid).unwrap()).unwrap();
    assert!(
        read_results(&path)
            .unwrap_err()
            .to_string()
            .contains("invalid task id")
    );
}

#[test]
fn graded_on_timeout_is_reported_apart_and_never_counted_as_a_pass() {
    let late_pass = TimeoutGrade {
        success: true,
        score: Some(1.0),
    };
    let mut slow = trial("a", Arm::Archon, false, 1200.0, 1.0);
    slow.timed_out = true;
    slow.graded_on_timeout = Some(late_pass);
    let mut stuck = trial("a", Arm::Archon, false, 1200.0, 1.0);
    stuck.timed_out = true;
    stuck.graded_on_timeout = Some(TimeoutGrade {
        success: false,
        score: None,
    });
    let mut unreached = trial("b", Arm::Archon, false, 1200.0, 1.0);
    unreached.timed_out = true;
    let summary = Summary::build(&[slow, stuck, unreached], None).unwrap();
    let archon = &summary.arms[0];
    assert_eq!(
        (archon.trials, archon.successes, archon.timeouts),
        (3, 0, 3)
    );
    assert_eq!(
        (archon.timeout_graded, archon.timeout_graded_passes),
        (2, 1)
    );
    assert_eq!(
        (archon.success_rate, archon.pass_at_1, archon.score.mean),
        (0.0, 0.0, 0.0)
    );
    let md = render_markdown(&summary).unwrap();
    assert!(md.contains("## Graded on timeout"), "{md}");
    assert!(md.contains("| archon | 3 | 2 | 1 |"), "{md}");
    assert!(md.contains("| archon | `m` | 3 | 0/3 (0%)"), "{md}");
}

#[test]
fn summaries_without_timeout_grades_keep_their_layout() {
    let summary = Summary::build(&[trial("a", Arm::Claude, true, 1.0, 0.1)], None).unwrap();
    assert!(
        !render_markdown(&summary)
            .unwrap()
            .contains("Graded on timeout")
    );
    let old = r#"{"arm":"claude","model":"m","trials":1,"successes":1,"infra":0,"timeouts":0,
        "success_rate":1.0,"pass_at_1":1.0,"pass_at_k":1.0,
        "score":{"n":1,"mean":1.0,"stdev":0.0},"wall_secs":{"n":1,"mean":1.0,"stdev":0.0},
        "input_tokens":null,"output_tokens":null,"cost_usd":null,
        "operator_messages":{"n":1,"mean":1.0,"stdev":0.0}}"#;
    let parsed: Aggregate = serde_json::from_str(old).unwrap();
    assert_eq!(
        (parsed.timeout_graded, parsed.timeout_graded_passes),
        (0, 0)
    );
}

#[test]
fn only_priced_trials_with_an_uncovered_gap_mark_the_arm_cost() {
    let mut substituted = result(Some(0.0), Some(5.0));
    substituted.cost_gap = Some("unpriced".into());
    let mut unpriced = result(None, None);
    unpriced.cost_gap = Some("unreported".into());
    let exact = trial("t", Arm::Claude, true, 1.0, 2.0);

    let estimated = Summary::build(&[substituted], None).unwrap();
    assert_eq!(estimated.arms[0].cost_usd.as_ref().unwrap().mean, 5.0);
    assert_eq!(estimated.arms[0].cost_bound, CostBound::Incomplete);
    let covered = Summary::build(&[result(Some(0.0), Some(5.0))], None).unwrap();
    assert_eq!(covered.arms[0].cost_bound, CostBound::Exact);
    let with_unpriced = Summary::build(&[exact, unpriced], None).unwrap();
    assert_eq!(with_unpriced.arms[0].cost_usd.as_ref().unwrap().n, 1);
    assert_eq!(with_unpriced.arms[0].cost_bound, CostBound::Exact);
}

#[test]
fn a_gapped_cost_beside_an_estimate_marks_the_mean_incomplete_not_a_floor() {
    let mut gapped = trial("a", Arm::Archon, true, 1.0, 1.0);
    gapped.cost_gap = Some("interrupted".into());
    let mut estimate = trial("b", Arm::Archon, true, 1.0, 0.0);
    estimate.cost_usd = None;
    estimate.estimated_cost_usd = Some(5.0);
    let summary = Summary::build(&[estimate, gapped], None).unwrap();
    let archon = &summary.arms[0];
    assert_eq!(archon.cost_usd.as_ref().unwrap().mean, 3.0);
    assert_eq!(archon.cost_bound, CostBound::Incomplete);
    let md = render_markdown(&summary).unwrap();
    assert!(md.contains("| * 3.00 ± "), "{md}");
    assert!(!md.contains("≥ 3.00"), "{md}");
    assert!(md.contains("not a bound"));
}

#[test]
fn a_cost_gap_marks_the_cost_as_a_floor_only_where_it_occurs() {
    let mut gapped = trial("a", Arm::Archon, true, 1.0, 2.0);
    gapped.cost_gap = Some("interrupted".into());
    let exact = trial("a", Arm::Claude, true, 1.0, 1.0);
    let summary = Summary::build(&[gapped, exact], None).unwrap();
    let (archon, claude) = (&summary.arms[0], &summary.arms[1]);
    assert_eq!(archon.cost_bound, CostBound::Floor);
    assert_eq!(claude.cost_bound, CostBound::Exact);
    assert_eq!(summary.tasks["a"][0].cost_bound, CostBound::Floor);
    assert_eq!(summary.tasks["a"][1].cost_bound, CostBound::Exact);
    let md = render_markdown(&summary).unwrap();
    assert!(md.contains("| ≥ 2.00 ± 0.00 |"), "{md}");
    assert!(
        md.contains("| 1.00 ± 0.00 |") && !md.contains("≥ 1.00"),
        "{md}"
    );
    assert!(md.contains("true cost is at least that"));

    let plain = Summary::build(&[trial("a", Arm::Claude, true, 1.0, 1.0)], None).unwrap();
    assert!(!render_markdown(&plain).unwrap().contains("≥ 1.00"));
}

#[test]
fn the_manifest_spend_is_marked_by_its_bound_and_old_manifests_load_exact() {
    let mut manifest: RunManifest =
        serde_json::from_str(include_str!("../../results-v3/run.json")).unwrap();
    assert_eq!(manifest.spent_bound, CostBound::Exact);
    let mut out = String::new();
    render_manifest(&mut out, &manifest).unwrap();
    assert!(out.contains("spent $") && !out.contains("spent ≥"), "{out}");
    manifest.spent_bound = CostBound::Floor;
    let mut floor = String::new();
    render_manifest(&mut floor, &manifest).unwrap();
    assert!(floor.contains("spent ≥ $"), "{floor}");
    manifest.spent_bound = CostBound::Incomplete;
    let mut incomplete = String::new();
    render_manifest(&mut incomplete, &manifest).unwrap();
    assert!(incomplete.contains("spent * $"), "{incomplete}");
}
