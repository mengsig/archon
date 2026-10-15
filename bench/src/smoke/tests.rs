use super::*;
use crate::metrics::tests::result;

fn evidence() -> (Baseline, RunManifest, Vec<TrialResult>) {
    let baseline = Baseline {
        suite_version: 1,
        tasks: TASKS.map(str::to_owned).to_vec(),
        model: "claude-code/claude-haiku-5-5".into(),
        effort: "medium".into(),
        max_usd: DEFAULT_MAX_USD,
        candidate: false,
        archon_successes: 4,
        archon_trials: 4,
        source_run: "unit-test-fixture".into(),
    };
    let mut manifest: RunManifest =
        serde_json::from_str(include_str!("../../results-v3/run.json")).unwrap();
    manifest.tasks = TASKS.map(str::to_owned).to_vec();
    manifest.models = ["archon", "aether"]
        .map(|arm| (arm.to_owned(), baseline.model.clone()))
        .into_iter()
        .collect();
    manifest.effort = baseline.effort.clone();
    manifest.arms = vec![Arm::Archon, Arm::Aether];
    manifest.trials = 1;
    manifest.parallel = 1;
    manifest.planned = 8;
    manifest.completed = 8;
    manifest.skipped = 0;
    manifest.spent_usd = 4.0;
    manifest.max_usd = DEFAULT_MAX_USD;
    manifest.unpriced_trials = 0;
    manifest.stopped_reason = None;
    let mut results = Vec::new();
    for task in TASKS {
        for arm in [Arm::Archon, Arm::Aether] {
            let mut trial = result(Some(0.5), None);
            trial.task = task.to_owned();
            trial.arm = arm;
            trial.model = baseline.model.clone();
            trial.effort = Some(baseline.effort.clone());
            results.push(trial);
        }
    }
    assert_eq!(results.len(), manifest.planned);
    assert!(manifest.finished_at.is_some());
    (baseline, manifest, results)
}

#[test]
fn gate_detects_lost_archon_pass_without_penalizing_aether_failures() {
    let (mut baseline, manifest, mut results) = evidence();
    baseline.archon_successes = 4;
    let first = evaluate(&results, &manifest, &baseline, 7.0).unwrap();
    assert!(first.passed);
    results[1].success = false;
    assert!(
        evaluate(&results, &manifest, &baseline, 7.0)
            .unwrap()
            .passed
    );
    results[0].success = false;
    let lost_one = evaluate(&results, &manifest, &baseline, 7.0).unwrap();
    assert_eq!(lost_one.archon_successes, 3);
    assert!(!lost_one.passed);
    baseline.archon_successes = 3;
    assert!(
        evaluate(&results, &manifest, &baseline, 7.0)
            .unwrap()
            .passed
    );
    for result in results.iter_mut().filter(|r| r.arm == Arm::Archon) {
        result.success = false;
    }
    let failed = evaluate(&results, &manifest, &baseline, 7.0).unwrap();
    assert!(!failed.passed);
    assert_eq!(failed.archon_successes, 0);
}

#[test]
fn incomplete_duplicate_and_foreign_cells_are_rejected() {
    let (baseline, manifest, results) = evidence();
    assert!(evaluate(&results[..7], &manifest, &baseline, 7.0).is_err());
    let mut duplicate = results.clone();
    duplicate[7] = duplicate[0].clone();
    assert!(evaluate(&duplicate, &manifest, &baseline, 7.0).is_err());
    let mut foreign = results;
    foreign[0].task = "not-smoke".into();
    assert!(evaluate(&foreign, &manifest, &baseline, 7.0).is_err());
}

#[test]
fn infrastructure_errors_timeouts_and_unpriced_trials_cannot_pass() {
    let (baseline, manifest, results) = evidence();
    let mut infra = results.clone();
    infra[0].error = Some("provider unavailable".into());
    assert!(evaluate(&infra, &manifest, &baseline, 7.0).is_err());
    let mut timed_out = results.clone();
    timed_out[0].timed_out = true;
    assert!(evaluate(&timed_out, &manifest, &baseline, 7.0).is_err());
    let mut unpriced = results;
    unpriced[0].cost_usd = None;
    assert!(evaluate(&unpriced, &manifest, &baseline, 7.0).is_err());
}

#[test]
fn budget_overruns_and_manifest_drift_cannot_pass() {
    let (baseline, mut manifest, results) = evidence();
    assert!(evaluate(&results, &manifest, &baseline, 3.99).is_err());
    assert!(evaluate(&results, &manifest, &baseline, f64::NAN).is_err());
    manifest.skipped = 1;
    assert!(evaluate(&results, &manifest, &baseline, 7.0).is_err());
    manifest.skipped = 0;
    manifest.spent_usd = 0.0;
    assert!(evaluate(&results, &manifest, &baseline, 7.0).is_err());
}

#[test]
fn changed_models_effort_and_baseline_shape_need_explicit_rebaselining() {
    let (mut baseline, manifest, mut results) = evidence();
    results[0].model = "another/model".into();
    assert!(evaluate(&results, &manifest, &baseline, 7.0).is_err());
    results[0].model = baseline.model.clone();
    results[0].effort = Some("low".into());
    assert!(evaluate(&results, &manifest, &baseline, 7.0).is_err());
    baseline.source_run = "pending".into();
    assert!(baseline.validate().is_err());
    baseline.source_run = "unit-test-fixture".into();
    baseline.archon_successes = 5;
    assert!(baseline.validate().is_err());
    baseline.archon_successes = 0;
    assert!(baseline.validate().is_err());
}

#[test]
fn the_smoke_total_carries_the_mark_of_its_cost_bound() {
    assert_eq!(total_cost_text(CostBound::Exact, 1.5), "$1.50");
    assert_eq!(total_cost_text(CostBound::Floor, 1.5), "≥ $1.50");
    assert_eq!(total_cost_text(CostBound::Incomplete, 1.5), "* $1.50");
}

#[test]
fn failing_gate_is_persisted_and_returns_an_error() {
    let (baseline, manifest, mut results) = evidence();
    for result in &mut results {
        result.success = false;
    }
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("run.json"),
        serde_json::to_string(&manifest).unwrap(),
    )
    .unwrap();
    let lines: Vec<_> = results
        .iter()
        .map(|r| serde_json::to_string(r).unwrap())
        .collect();
    std::fs::write(dir.path().join("results.jsonl"), lines.join("\n")).unwrap();
    assert!(check_directory(dir.path(), &baseline, 7.0).is_err());
    let gate: Gate =
        serde_json::from_str(&std::fs::read_to_string(dir.path().join("gate.json")).unwrap())
            .unwrap();
    assert!(!gate.passed);
}

#[test]
fn increased_or_invalid_caps_cannot_pass() {
    let (mut baseline, mut manifest, results) = evidence();
    assert!(evaluate(&results, &manifest, &baseline, DEFAULT_MAX_USD + 1.0).is_err());
    for cap in [DEFAULT_MAX_USD + 1.0, f64::NAN, 0.0, -1.0] {
        manifest.max_usd = cap;
        assert!(evaluate(&results, &manifest, &baseline, DEFAULT_MAX_USD).is_err());
    }
    for cap in [f64::NAN, 0.0, -1.0] {
        baseline.max_usd = cap;
        assert!(baseline.validate().is_err());
    }
    manifest.max_usd = 3.99;
    baseline.max_usd = DEFAULT_MAX_USD;
    assert!(evaluate(&results, &manifest, &baseline, DEFAULT_MAX_USD).is_err());
}

fn write_evidence(dir: &Path, manifest: &RunManifest, results: &[TrialResult]) {
    assert!(dir.is_dir());
    assert_eq!(manifest.completed, results.len());
    std::fs::write(
        dir.join("run.json"),
        serde_json::to_string(manifest).unwrap(),
    )
    .unwrap();
    let lines: Vec<_> = results
        .iter()
        .map(|r| serde_json::to_string(r).unwrap())
        .collect();
    std::fs::write(dir.join("results.jsonl"), lines.join("\n")).unwrap();
}

#[test]
fn bootstrap_writes_a_candidate_that_requires_explicit_review() {
    let (baseline, manifest, results) = evidence();
    let dir = tempfile::tempdir().unwrap();
    write_evidence(dir.path(), &manifest, &results);
    write_candidate(dir.path()).unwrap();
    let path = dir.path().join("candidate-baseline.json");
    assert!(
        Baseline::load(&path)
            .unwrap_err()
            .to_string()
            .contains("review")
    );
    let mut candidate: Baseline =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert!(candidate.candidate);
    assert_eq!(candidate.max_usd, manifest.max_usd);
    assert_eq!(candidate.archon_successes, baseline.archon_successes);
    assert_eq!(candidate.source_run, manifest.stamp);
    candidate.candidate = false;
    assert!(
        evaluate(&results, &manifest, &candidate, candidate.max_usd)
            .unwrap()
            .passed
    );
    assert!(!dir.path().join("baseline.json").exists());
}

#[test]
fn bootstrap_rejects_partial_unpriced_and_zero_pass_evidence() {
    let (_, mut manifest, mut results) = evidence();
    let dir = tempfile::tempdir().unwrap();
    manifest.finished_at = None;
    write_evidence(dir.path(), &manifest, &results);
    assert!(write_candidate(dir.path()).is_err());
    manifest.finished_at = Some(archon_core::now());
    results[0].cost_usd = None;
    write_evidence(dir.path(), &manifest, &results);
    assert!(write_candidate(dir.path()).is_err());
    results[0].cost_usd = Some(0.5);
    for result in &mut results {
        result.success = false;
    }
    write_evidence(dir.path(), &manifest, &results);
    assert!(write_candidate(dir.path()).is_err());
    assert!(!dir.path().join("candidate-baseline.json").exists());
}
