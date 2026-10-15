//! Executes a plan: one fresh workspace per trial, the arm, then the hidden
//! grader on a separate copy. Results are appended to `results.jsonl` as
//! each trial finishes, so an interrupted run keeps everything it paid for.

use std::collections::BTreeMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context, ensure};
use archon_aether::AetherClient;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;

use crate::arms::{self, ArmOutcome, ArmSettings, TrialInput};
use crate::grade::{self, GradeOutcome, ResultSource};
use crate::metrics::{Arm, CostBound, RunManifest, TimeoutGrade, TrialResult};
use crate::plan::{Budget, RunOptions, TrialSpec, check_plan, plan};
use crate::process::{redact_home, version_of};
use crate::task::BenchTask;
use crate::workspace::{self, TrialLayout};

/// Shared by every trial of a run.
#[derive(Debug)]
struct Shared {
    tasks: Vec<BenchTask>,
    settings: ArmSettings,
    options: RunOptions,
    /// Only one private archond can own the sandbox socket at a time.
    archon_lock: tokio::sync::Mutex<()>,
    ledger: Mutex<Ledger>,
}

/// Budget and bookkeeping, updated as trials finish.
#[derive(Debug)]
struct Ledger {
    budget: Budget,
    completed: usize,
    results: std::fs::File,
}

/// Run every planned trial; returns the results directory.
pub async fn run(
    tasks: Vec<BenchTask>,
    settings: ArmSettings,
    options: RunOptions,
) -> anyhow::Result<PathBuf> {
    options.validate()?;
    let specs = plan(tasks.len(), &options.arms, options.trials);
    check_plan(&specs, &options)?;
    let dir = options.run_results_dir();
    ensure!(
        !dir.exists(),
        "{} already exists; results are never overwritten",
        dir.display()
    );
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    workspace::create_run_dir(&options.run_work_dir())?;
    let mut manifest = start_manifest(&tasks, &settings, &options, specs.len()).await;
    write_manifest(&dir, &manifest)?;
    let results = std::fs::File::create(dir.join("results.jsonl"))?;
    let shared = Arc::new(Shared {
        ledger: Mutex::new(Ledger {
            budget: Budget::new(options.max_usd),
            completed: 0,
            results,
        }),
        archon_lock: tokio::sync::Mutex::new(()),
        tasks,
        settings,
        options,
    });
    let skipped = execute(&shared, &specs).await?;
    let ledger = shared
        .ledger
        .lock()
        .expect("a trial panicked while recording");
    manifest.finished_at = Some(archon_core::now());
    manifest.completed = ledger.completed;
    manifest.skipped = skipped;
    manifest.spent_usd = ledger.budget.spent_usd;
    manifest.spent_bound = ledger.budget.spent_bound();
    manifest.unpriced_trials = ledger.budget.unpriced_trials;
    if skipped > 0 {
        manifest.stopped_reason = Some(format!(
            "--max-usd {:.2} reached after ${:.2}; {skipped} trials skipped",
            shared.options.max_usd, ledger.budget.spent_usd
        ));
    }
    drop(ledger);
    write_manifest(&dir, &manifest)?;
    Ok(dir)
}

/// Start trials in plan order, at most `--parallel` at once, until the
/// budget runs out. Returns how many trials were skipped.
async fn execute(shared: &Arc<Shared>, specs: &[TrialSpec]) -> anyhow::Result<usize> {
    let permits = Arc::new(Semaphore::new(shared.options.parallel));
    let mut running = JoinSet::new();
    let mut skipped = 0;
    for (index, &spec) in specs.iter().enumerate() {
        let permit = permits
            .clone()
            .acquire_owned()
            .await
            .context("the trial pool closed")?;
        if !shared
            .ledger
            .lock()
            .expect("ledger poisoned")
            .budget
            .allows_another()
        {
            skipped = specs.len() - index;
            eprintln!("budget reached: skipping the remaining {skipped} trials");
            break;
        }
        let shared = Arc::clone(shared);
        let label = format!("[{}/{}]", index + 1, specs.len());
        running.spawn(async move {
            let result = run_trial(&shared, spec).await;
            let recorded = record(&shared, &result);
            eprintln!("{label} {}", progress_line(&result));
            drop(permit);
            recorded
        });
    }
    while let Some(joined) = running.join_next().await {
        joined.context("a trial task panicked")??;
    }
    Ok(skipped)
}

fn record(shared: &Shared, result: &TrialResult) -> anyhow::Result<()> {
    let mut ledger = shared.ledger.lock().expect("ledger poisoned");
    let line = serde_json::to_string(result)?;
    writeln!(ledger.results, "{line}").context("appending to results.jsonl")?;
    ledger.results.flush()?;
    ledger.budget.record(result);
    ledger.completed += 1;
    Ok(())
}

fn progress_line(r: &TrialResult) -> String {
    let verdict = if r.success { "pass" } else { "FAIL" };
    let mark = CostBound::of([r]).marker();
    let cost = r
        .effective_cost_usd()
        .map_or_else(|| "$?".to_owned(), |c| format!("{mark}${c:.2}"));
    let problem = r
        .error
        .as_deref()
        .map(|e| format!(" error: {e}"))
        .unwrap_or_default();
    let late = match r.graded_on_timeout {
        Some(g) if g.success => " (graded on timeout: pass)",
        Some(_) => " (graded on timeout: fail)",
        None => "",
    };
    format!(
        "{} {} #{}: {verdict}{late} {:.0}s {cost}{problem}",
        r.arm, r.task, r.trial, r.wall_secs
    )
}

async fn run_trial(shared: &Shared, spec: TrialSpec) -> TrialResult {
    let task = &shared.tasks[spec.task];
    let started_at = archon_core::now();
    let (outcome, layout) = match prepare_trial(&shared.options, task, spec) {
        Ok(layout) => (run_arm(shared, task, &layout, spec).await, Some(layout)),
        Err(e) => (
            ArmOutcome::failed(
                format!("preparing the workspace: {e:#}"),
                ResultSource::Workspace,
            ),
            None,
        ),
    };
    let finished_at = archon_core::now();
    let graded = match &layout {
        Some(layout) => grade_trial(task, layout, spec.arm, &outcome).await,
        None => Err(anyhow::anyhow!("no workspace to grade")),
    };
    let late = match &layout {
        Some(layout) => grade_on_timeout(task, layout, spec.arm, &outcome).await,
        None => None,
    };
    let mut result = trial_result(shared, task, spec, outcome, graded);
    if let Some(late) = late {
        result.graded_on_timeout = late.grade;
        if let Some(problem) = late.note {
            result.note = Some(join_notes(result.note.take(), problem));
        }
    }
    if let Some(layout) = &layout {
        capture_routing(&mut result, &layout.logs);
    }
    if !shared.options.keep_workspaces
        && let Some(layout) = &layout
        && let Err(e) = std::fs::remove_dir_all(&layout.workspace)
    {
        let problem = format!("removing {}: {e}", layout.workspace.display());
        result.note = Some(join_notes(result.note.take(), problem));
    }
    result.started_at = Some(started_at);
    result.finished_at = Some(finished_at);
    result
}

fn capture_routing(result: &mut TrialResult, logs: &Path) {
    if result.arm != Arm::Archon {
        return;
    }
    assert!(result.archon_work.is_none(), "routing is captured once");
    assert!(logs.is_dir(), "prepared trials have a logs directory");
    match arms::archon::read_archon_work(logs) {
        Ok(work) => result.archon_work = work,
        Err(e) => {
            let problem = format!("capturing Archon routing: {e:#}");
            eprintln!("{problem}");
            result.note = Some(join_notes(result.note.take(), problem));
        }
    }
}

/// A fresh, unguessably named workspace and the trial's logs directory,
/// which records where the workspace was.
fn prepare_trial(
    options: &RunOptions,
    task: &BenchTask,
    spec: TrialSpec,
) -> anyhow::Result<TrialLayout> {
    let logs = options.trial_logs_dir(&task.file.id, spec.arm, spec.trial);
    let layout = TrialLayout::for_trial(&options.run_work_dir(), logs)?;
    workspace::prepare(task, &layout)?;
    let record = layout.logs.join("workspace.txt");
    std::fs::write(&record, format!("{}\n", layout.workspace.display()))
        .with_context(|| format!("writing {}", record.display()))?;
    Ok(layout)
}

fn join_notes(note: Option<String>, extra: String) -> String {
    match note {
        Some(note) => format!("{note}; {extra}"),
        None => extra,
    }
}

async fn run_arm(
    shared: &Shared,
    task: &BenchTask,
    layout: &TrialLayout,
    spec: TrialSpec,
) -> ArmOutcome {
    let key = format!(
        "{}-{}-{}-{}",
        shared.options.stamp, task.file.id, spec.arm, spec.trial
    );
    let input = TrialInput {
        task,
        layout,
        settings: &shared.settings,
        timeout: shared.options.timeout_for(task),
        key: &key,
    };
    match spec.arm {
        Arm::Claude => arms::cli::run_claude(&input).await,
        Arm::Codex => arms::cli::run_codex(&input).await,
        Arm::Aether => arms::aether::run_aether(&input).await,
        Arm::Archon => {
            let _exclusive = shared.archon_lock.lock().await;
            arms::archon::run_archon(&input).await
        }
    }
}

async fn grade_trial(
    task: &BenchTask,
    layout: &TrialLayout,
    arm: Arm,
    outcome: &ArmOutcome,
) -> anyhow::Result<GradeOutcome> {
    std::fs::write(layout.answer_file(), &outcome.answer).context("writing answer.txt")?;
    grade::grade(task, layout, arm.name(), &outcome.source).await
}

/// What grading a timed-out trial's committed work produced.
struct LateGrade {
    grade: Option<TimeoutGrade>,
    note: Option<String>,
}

/// Grade the last committed branches of a timed-out trial's items, apart
/// from the verdict. A failure here is a note, never an error: the verdict
/// and the trial's infrastructure status do not depend on this extra grade.
async fn grade_on_timeout(
    task: &BenchTask,
    layout: &TrialLayout,
    arm: Arm,
    outcome: &ArmOutcome,
) -> Option<LateGrade> {
    let source = outcome.timeout_source.as_ref()?;
    assert!(
        outcome.timed_out,
        "only a timed-out trial has a timeout source"
    );
    if layout.grade.exists() {
        return Some(LateGrade {
            grade: None,
            note: Some("grading on timeout skipped: the verdict's grading copy remains".into()),
        });
    }
    let graded = match layout.late_grade() {
        Ok(late) => grade::grade(task, &late, arm.name(), source).await,
        Err(e) => Err(e),
    };
    Some(match graded {
        Ok(g) => LateGrade {
            grade: Some(TimeoutGrade {
                success: g.success,
                score: g.score,
            }),
            note: g.note.map(|n| format!("graded on timeout: {n}")),
        },
        Err(e) => LateGrade {
            grade: None,
            note: Some(format!("grading on timeout failed: {e:#}")),
        },
    })
}

fn trial_result(
    shared: &Shared,
    task: &BenchTask,
    spec: TrialSpec,
    outcome: ArmOutcome,
    graded: anyhow::Result<GradeOutcome>,
) -> TrialResult {
    let (success, score, note, grade_error) = match graded {
        Ok(g) => (g.success, g.score, g.note, None),
        Err(e) => (false, None, None, Some(format!("grading: {e:#}"))),
    };
    let error = match (outcome.error, grade_error) {
        (Some(a), Some(b)) => Some(format!("{a}; {b}")),
        (a, b) => a.or(b),
    };
    let usage = outcome.usage.as_ref();
    TrialResult {
        task: task.file.id.clone(),
        arm: spec.arm,
        trial: spec.trial,
        model: shared.settings.model_label(spec.arm),
        effort: Some(shared.settings.effort.to_string()),
        success,
        score,
        wall_secs: outcome.wall_secs,
        input_tokens: usage.map(|u| u.input_tokens),
        output_tokens: usage.map(|u| u.output_tokens),
        cached_input_tokens: usage.map(|u| u.cached_input_tokens),
        cost_usd: usage.and_then(|u| u.cost_usd),
        estimated_cost_usd: usage.and_then(|u| u.estimated_cost_usd),
        cost_gap: usage.and_then(|u| u.cost_gap.clone()),
        operator_messages: outcome.operator_messages,
        error,
        timed_out: outcome.timed_out,
        setup_secs: outcome.setup_secs,
        started_at: None,
        finished_at: None,
        note,
        archon_work: None,
        graded_on_timeout: None,
    }
}

/// Versions of every program the chosen arms run.
pub async fn collect_versions(settings: &ArmSettings, arms: &[Arm]) -> BTreeMap<String, String> {
    let mut versions = BTreeMap::new();
    let wanted = |arm: Arm| arms.contains(&arm);
    if wanted(Arm::Claude) {
        versions.insert("claude".to_owned(), version_of(&settings.claude_bin));
    }
    if wanted(Arm::Codex) {
        versions.insert("codex".to_owned(), version_of(&settings.codex_bin));
    }
    if wanted(Arm::Archon) {
        versions.insert("archon".to_owned(), version_of(&settings.archon_bin));
    }
    if wanted(Arm::Archon) || wanted(Arm::Aether) {
        versions.insert("aetherd".to_owned(), aetherd_version(settings).await);
    }
    let home = std::env::var_os("HOME");
    let home = home.as_ref().map(|value| value.to_string_lossy());
    for version in versions.values_mut() {
        *version = redact_home(version, home.as_deref());
    }
    versions
}

/// Only a validated private socket is probed; a default one could be the
/// operator's daemon.
async fn aetherd_version(settings: &ArmSettings) -> String {
    let Some(sockets) = &settings.sockets else {
        return "aetherd not probed: no private sandbox".to_owned();
    };
    let endpoint = arms::aether::endpoint(&sockets.aether);
    match AetherClient::connect(&endpoint).await {
        Ok(client) => client.welcome().daemon_version.clone(),
        Err(e) => format!("unreachable: {e}"),
    }
}

async fn start_manifest(
    tasks: &[BenchTask],
    settings: &ArmSettings,
    options: &RunOptions,
    planned: usize,
) -> RunManifest {
    RunManifest {
        stamp: options.stamp.clone(),
        started_at: archon_core::now(),
        finished_at: None,
        archon_commit: archon_commit(),
        versions: collect_versions(settings, &options.arms).await,
        models: options
            .arms
            .iter()
            .map(|&a| (a.to_string(), settings.model_label(a)))
            .collect(),
        effort: settings.effort.to_string(),
        arms: options.arms.clone(),
        tasks: tasks.iter().map(|t| t.file.id.clone()).collect(),
        trials: options.trials,
        parallel: options.parallel,
        timeout_factor: options.timeout_factor,
        max_usd: options.max_usd,
        max_trials: options.max_trials,
        planned,
        completed: 0,
        skipped: 0,
        spent_usd: 0.0,
        spent_bound: CostBound::Exact,
        unpriced_trials: 0,
        stopped_reason: None,
    }
}

/// The Archon checkout's `HEAD`, when the bench runs from one.
fn archon_commit() -> Option<String> {
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let out = std::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(manifest_dir)
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

fn write_manifest(dir: &Path, manifest: &RunManifest) -> anyhow::Result<()> {
    let path = dir.join("run.json");
    std::fs::write(&path, serde_json::to_string_pretty(manifest)?)
        .with_context(|| format!("writing {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metrics::tests::result;
    use crate::workspace::tests::scratch;

    #[test]
    fn capture_routing_persists_an_observed_empty_queue() {
        let tmp = scratch();
        std::fs::write(tmp.path().join("archon-items.json"), "[]").unwrap();
        let mut trial = result(None, None);
        trial.arm = Arm::Archon;
        capture_routing(&mut trial, tmp.path());
        assert_eq!(trial.archon_work, Some(Vec::new()));
        let json = serde_json::to_string(&trial).unwrap();
        assert!(json.contains("\"archon_work\":[]"));
    }

    #[test]
    fn capture_failure_is_visible_without_changing_the_grader_verdict() {
        let tmp = scratch();
        std::fs::write(tmp.path().join("archon-items.json"), "broken").unwrap();
        let mut trial = result(None, None);
        trial.arm = Arm::Archon;
        trial.note = Some("grader remark".into());
        capture_routing(&mut trial, tmp.path());
        assert!(trial.archon_work.is_none());
        assert!(trial.success && trial.error.is_none());
        assert!(
            trial
                .note
                .unwrap()
                .starts_with("grader remark; capturing Archon routing:")
        );
    }

    #[tokio::test]
    async fn versions_never_probe_aetherd_without_a_private_sandbox() {
        let mut settings = crate::arms::tests::settings();
        settings.sockets = None;
        let versions = collect_versions(&settings, &[Arm::Aether]).await;
        assert_eq!(
            versions["aetherd"],
            "aetherd not probed: no private sandbox"
        );
        assert_eq!(versions.len(), 1);
    }

    #[test]
    fn progress_marks_a_gapped_cost_by_its_bound() {
        let mut gapped = result(Some(1.5), None);
        gapped.cost_gap = Some("interrupted".into());
        assert!(progress_line(&gapped).ends_with("1s ≥ $1.50"));
        assert!(progress_line(&result(Some(1.5), None)).ends_with("1s $1.50"));
        assert!(progress_line(&result(Some(0.0), Some(2.0))).ends_with("1s $2.00"));
        let mut estimated = result(Some(0.0), Some(2.0));
        estimated.cost_gap = Some("unpriced".into());
        assert!(progress_line(&estimated).ends_with("1s * $2.00"));
    }
}
