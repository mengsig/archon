//! Aggregation of `results.jsonl` into `summary.md` and the chart-ready
//! `chart.json`: mean ± sample stdev per metric, success rate and pass@k.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::path::Path;

use anyhow::{Context, ensure};
use serde::{Deserialize, Serialize};

use crate::arms::archon::read_archon_work;
use crate::metrics::{Arm, CostBound, RunManifest, TrialResult};

/// Mean and sample standard deviation of `n` values (stdev 0 when n = 1).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Stat {
    pub n: usize,
    pub mean: f64,
    pub stdev: f64,
}

impl Stat {
    pub fn of(values: &[f64]) -> Option<Self> {
        if values.is_empty() {
            return None;
        }
        assert!(
            values.iter().all(|v| v.is_finite()),
            "non-finite metric in {values:?}"
        );
        let n = values.len();
        let mean = values.iter().sum::<f64>() / n as f64;
        let variance = if n < 2 {
            0.0
        } else {
            values.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / (n - 1) as f64
        };
        Some(Self {
            n,
            mean,
            stdev: variance.sqrt(),
        })
    }
}

/// Unbiased pass@k (Chen et al. 2021): the chance that at least one of k
/// trials drawn without replacement from n, of which c passed, passes.
pub fn pass_at_k(n: u32, c: u32, k: u32) -> f64 {
    assert!(c <= n, "{c} passes out of {n} trials");
    assert!((1..=n).contains(&k), "k = {k} must be within 1..={n}");
    if n - c < k {
        return 1.0;
    }
    let all_fail: f64 = ((n - c + 1)..=n)
        .map(|i| 1.0 - f64::from(k) / f64::from(i))
        .product();
    1.0 - all_fail
}

/// One arm over a set of trials (a whole run, or one task).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Aggregate {
    pub arm: Arm,
    pub model: String,
    /// Graded trials: infrastructure failures are not among them.
    pub trials: u32,
    pub successes: u32,
    /// Infrastructure failures left out of every other field.
    pub infra: u32,
    pub timeouts: u32,
    /// Timed-out trials whose committed work was graded separately, and how
    /// many of those passed. Never part of `successes`, rates or pass@k.
    #[serde(default)]
    pub timeout_graded: u32,
    #[serde(default)]
    pub timeout_graded_passes: u32,
    pub success_rate: f64,
    /// Mean over tasks of per-task pass@1 and pass@k.
    pub pass_at_1: f64,
    pub pass_at_k: f64,
    pub score: Stat,
    pub wall_secs: Stat,
    pub input_tokens: Option<Stat>,
    pub output_tokens: Option<Stat>,
    pub cost_usd: Option<Stat>,
    /// Whether `cost_usd` is exact, a floor or incomplete; see [`CostBound`].
    #[serde(default)]
    pub cost_bound: CostBound,
    pub operator_messages: Stat,
}

impl Aggregate {
    /// `all` must be for `arm`; pass@k is per task, then averaged. `None`
    /// when no trial was graded (every one was an infrastructure failure).
    pub fn of(arm: Arm, all: &[&TrialResult], k: u32) -> Option<Self> {
        assert!(
            all.iter().all(|r| r.arm == arm),
            "mixed arms in one aggregate"
        );
        let results: Vec<&TrialResult> = all
            .iter()
            .copied()
            .filter(|r| !r.is_infra_failure())
            .collect();
        let first = results.first()?;
        let mut per_task: BTreeMap<&str, (u32, u32)> = BTreeMap::new();
        for r in &results {
            let entry = per_task.entry(r.task.as_str()).or_default();
            entry.0 += 1;
            entry.1 += u32::from(r.success);
        }
        let tasks = per_task.len() as f64;
        let pass = |k: u32| {
            per_task
                .values()
                .map(|&(n, c)| pass_at_k(n, c, k.min(n)))
                .sum::<f64>()
                / tasks
        };
        let count =
            |f: &dyn Fn(&TrialResult) -> bool| results.iter().filter(|r| f(r)).count() as u32;
        let values = |f: &dyn Fn(&TrialResult) -> Option<f64>| {
            results.iter().filter_map(|r| f(r)).collect::<Vec<_>>()
        };
        let trials = results.len() as u32;
        let successes = count(&|r| r.success);
        assert!(successes <= trials, "more passes than graded trials");
        let timeout_graded = count(&|r| r.graded_on_timeout.is_some());
        let timeout_graded_passes = count(&|r| r.graded_on_timeout.is_some_and(|g| g.success));
        assert!(
            results
                .iter()
                .all(|r| r.graded_on_timeout.is_none() || r.timed_out),
            "only a timed-out trial is graded on timeout"
        );
        assert!(timeout_graded_passes <= timeout_graded);
        Some(Self {
            arm,
            model: first.model.clone(),
            trials,
            successes,
            infra: (all.len() - results.len()) as u32,
            timeouts: count(&|r| r.timed_out),
            timeout_graded,
            timeout_graded_passes,
            success_rate: f64::from(successes) / f64::from(trials),
            pass_at_1: pass(1),
            pass_at_k: pass(k),
            score: Stat::of(&values(&|r| Some(trial_score(r))))?,
            wall_secs: Stat::of(&values(&|r| Some(r.wall_secs)))?,
            input_tokens: Stat::of(&values(&|r| r.input_tokens.map(|t| t as f64))),
            output_tokens: Stat::of(&values(&|r| r.output_tokens.map(|t| t as f64))),
            cost_usd: Stat::of(&values(&TrialResult::effective_cost_usd)),
            cost_bound: CostBound::of(results.iter().copied()),
            operator_messages: Stat::of(&values(&|r| Some(f64::from(r.operator_messages))))?,
        })
    }
}

/// The grader's partial score, else 1 or 0 from its verdict.
pub fn trial_score(result: &TrialResult) -> f64 {
    result
        .score
        .unwrap_or(if result.success { 1.0 } else { 0.0 })
}

/// Everything `summary.md`, `chart.json` and the HTML report show.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Summary {
    pub manifest: Option<RunManifest>,
    /// pass@k's k: the fewest graded trials any (task, arm) cell has.
    pub k: u32,
    pub arms: Vec<Aggregate>,
    /// Task id → one aggregate per arm with a graded trial of it (none when
    /// every trial of the task was an infrastructure failure).
    pub tasks: BTreeMap<String, Vec<Aggregate>>,
    /// Infrastructure failures, left out of every aggregate.
    pub infra: Vec<InfraFailure>,
    /// Disjoint whole-trial buckets; mixed routing remains one observation.
    #[serde(default)]
    pub pipelines: BTreeMap<String, Aggregate>,
    #[serde(default)]
    pub reviews: BTreeMap<String, Aggregate>,
}

/// One trial left out of the aggregates, and why.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InfraFailure {
    pub task: String,
    pub arm: Arm,
    pub trial: u32,
    pub error: String,
}

impl Summary {
    pub fn build(results: &[TrialResult], manifest: Option<RunManifest>) -> anyhow::Result<Self> {
        ensure!(!results.is_empty(), "no trial results to summarize");
        validate_routing(results)?;
        let mut cells: BTreeMap<(&str, Arm), Vec<&TrialResult>> = BTreeMap::new();
        for r in results {
            cells.entry((r.task.as_str(), r.arm)).or_default().push(r);
        }
        let k = cells
            .values()
            .map(|c| c.iter().filter(|r| !r.is_infra_failure()).count() as u32)
            .filter(|&graded| graded > 0)
            .min()
            .context("no trial was graded: every one is an infrastructure failure")?;
        let mut tasks: BTreeMap<String, Vec<Aggregate>> = BTreeMap::new();
        for ((task, arm), trials) in &cells {
            let row = tasks.entry((*task).to_owned()).or_default();
            row.extend(Aggregate::of(*arm, trials, k));
        }
        let infra = results
            .iter()
            .filter(|r| r.is_infra_failure())
            .map(|r| InfraFailure {
                task: r.task.clone(),
                arm: r.arm,
                trial: r.trial,
                error: r.error.clone().expect("an infra failure carries its error"),
            })
            .collect();
        let arms = Arm::ALL
            .into_iter()
            .filter_map(|arm| {
                let all: Vec<&TrialResult> = results.iter().filter(|r| r.arm == arm).collect();
                Aggregate::of(arm, &all, k)
            })
            .collect();
        Ok(Self {
            manifest,
            k,
            arms,
            tasks,
            infra,
            pipelines: routing_buckets(results, k, false),
            reviews: routing_buckets(results, k, true),
        })
    }
}

fn validate_routing(results: &[TrialResult]) -> anyhow::Result<()> {
    for r in results {
        let Some(items) = &r.archon_work else {
            continue;
        };
        ensure!(r.arm == Arm::Archon, "{} trial has Archon routing", r.arm);
        ensure!(
            items.iter().all(|item| {
                !item.pipeline.trim().is_empty()
                    && item.run_pipelines.iter().all(|p| !p.trim().is_empty())
            }),
            "{} trial {} has an empty pipeline name",
            r.task,
            r.trial
        );
    }
    Ok(())
}

fn routing_buckets(results: &[TrialResult], k: u32, review: bool) -> BTreeMap<String, Aggregate> {
    assert!(k > 0, "pass@k needs at least one graded trial");
    let mut groups: BTreeMap<String, Vec<&TrialResult>> = BTreeMap::new();
    let archon = results.iter().filter(|r| r.arm == Arm::Archon);
    for r in archon.clone() {
        groups.entry(routing_label(r, review)).or_default().push(r);
    }
    let buckets: BTreeMap<String, Aggregate> = groups
        .into_iter()
        .filter_map(|(label, trials)| Some((label, Aggregate::of(Arm::Archon, &trials, k)?)))
        .collect();
    assert_eq!(
        buckets.values().map(|a| a.trials as usize).sum::<usize>(),
        archon.filter(|r| !r.is_infra_failure()).count(),
        "each graded Archon trial belongs to exactly one bucket"
    );
    buckets
}

fn routing_label(result: &TrialResult, review: bool) -> String {
    assert_eq!(result.arm, Arm::Archon, "only Archon has pipeline routing");
    let Some(items) = &result.archon_work else {
        return "unknown (no capture)".into();
    };
    if items.is_empty() {
        return "no work (empty queue)".into();
    }
    let mut labels = BTreeSet::new();
    for item in items {
        if review {
            labels.insert(
                item.review
                    .map_or_else(|| "unknown".into(), |r| r.to_string()),
            );
        } else {
            labels.insert(item.pipeline.clone());
            labels.extend(item.run_pipelines.iter().cloned());
        }
    }
    assert!(
        !labels.is_empty(),
        "each captured item supplies a routing label"
    );
    let labels: Vec<String> = labels.into_iter().collect();
    if labels.len() == 1 {
        return labels[0].clone();
    }
    format!("mixed: {}", labels.join(" + "))
}

pub fn read_results(path: &Path) -> anyhow::Result<Vec<TrialResult>> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    text.lines()
        .enumerate()
        .filter(|(_, line)| !line.trim().is_empty())
        .map(|(i, line)| {
            let mut result: TrialResult = serde_json::from_str(line)
                .with_context(|| format!("{}:{}: bad result line", path.display(), i + 1))?;
            hydrate_routing(
                &mut result,
                path.parent().context("results path has no parent")?,
            )?;
            Ok(result)
        })
        .collect()
}

fn hydrate_routing(result: &mut TrialResult, dir: &Path) -> anyhow::Result<()> {
    if result.arm != Arm::Archon || result.archon_work.is_some() {
        return Ok(());
    }
    let mut components = Path::new(&result.task).components();
    ensure!(
        matches!(components.next(), Some(std::path::Component::Normal(_)))
            && components.next().is_none(),
        "invalid task id {:?} in results",
        result.task
    );
    ensure!(result.trial > 0, "trial numbers must be positive");
    let logs = dir
        .join("trials")
        .join(&result.task)
        .join("archon")
        .join(result.trial.to_string())
        .join("logs");
    result.archon_work = read_archon_work(&logs)?;
    Ok(())
}

pub fn read_manifest(dir: &Path) -> anyhow::Result<Option<RunManifest>> {
    let path = dir.join("run.json");
    if !path.exists() {
        return Ok(None);
    }
    let text =
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    Ok(Some(
        serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))?,
    ))
}

/// Write `summary.md` and `chart.json` next to `results.jsonl`.
pub fn write_summary(dir: &Path) -> anyhow::Result<Summary> {
    let results = read_results(&dir.join("results.jsonl"))?;
    let summary = Summary::build(&results, read_manifest(dir)?)?;
    std::fs::write(dir.join("summary.md"), render_markdown(&summary)?)?;
    std::fs::write(
        dir.join("chart.json"),
        serde_json::to_string_pretty(&summary)?,
    )?;
    Ok(summary)
}

pub fn render_markdown(summary: &Summary) -> anyhow::Result<String> {
    let mut out = String::from("# Archon benchmark\n\n");
    if let Some(m) = &summary.manifest {
        render_manifest(&mut out, m)?;
    }
    let k = summary.k;
    writeln!(out, "## Per arm\n")?;
    writeln!(
        out,
        "| Arm | Model | Trials | Success | pass@1 | pass@{k} | Score | Wall s | Input tok | Output tok | Cost $ | Operator msgs | Infra | Timeouts |"
    )?;
    writeln!(
        out,
        "|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|"
    )?;
    for a in &summary.arms {
        writeln!(
            out,
            "| {} | `{}` | {} | {}/{} ({}) | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} |",
            a.arm,
            a.model,
            a.trials,
            a.successes,
            a.trials,
            percent(a.success_rate),
            percent(a.pass_at_1),
            percent(a.pass_at_k),
            stat(&a.score, 2),
            stat(&a.wall_secs, 0),
            opt_stat(a.input_tokens.as_ref(), 0, a.trials),
            opt_stat(a.output_tokens.as_ref(), 0, a.trials),
            cost_text(a),
            stat(&a.operator_messages, 1),
            a.infra,
            a.timeouts
        )?;
    }
    render_timeout_table(&mut out, &summary.arms)?;
    render_task_table(&mut out, summary)?;
    render_routing_table(&mut out, "Per pipeline", &summary.pipelines)?;
    render_routing_table(&mut out, "Per review level", &summary.reviews)?;
    render_infra(&mut out, &summary.infra)?;
    out.push_str(METHOD_NOTES);
    Ok(out)
}

/// Timed-out trials whose committed work was graded on its own. These are
/// failures in every table above; this one only shows what the deadline cost.
fn render_timeout_table(out: &mut String, arms: &[Aggregate]) -> anyhow::Result<()> {
    if arms.iter().all(|a| a.timeout_graded == 0) {
        return Ok(());
    }
    writeln!(
        out,
        "\n## Graded on timeout\n\n{TIMEOUT_NOTES}\n\n\
         | Arm | Timeouts | Graded on timeout | Passed when graded on timeout |\n|---|---:|---:|---:|"
    )?;
    for a in arms.iter().filter(|a| a.timeout_graded > 0) {
        assert!(a.timeout_graded <= a.timeouts, "graded more than timed out");
        writeln!(
            out,
            "| {} | {} | {} | {} |",
            a.arm, a.timeouts, a.timeout_graded, a.timeout_graded_passes
        )?;
    }
    Ok(())
}

fn render_manifest(out: &mut String, m: &RunManifest) -> anyhow::Result<()> {
    let finished = m
        .finished_at
        .map_or_else(|| "unfinished".to_owned(), |t| t.to_rfc3339());
    writeln!(
        out,
        "- Run `{}`: {} → {}",
        m.stamp,
        m.started_at.to_rfc3339(),
        finished
    )?;
    writeln!(
        out,
        "- Archon commit: `{}`",
        m.archon_commit.as_deref().unwrap_or("unknown")
    )?;
    writeln!(
        out,
        "- Effort: `{}`; trials per task and arm: {}; parallel: {}",
        m.effort, m.trials, m.parallel
    )?;
    for (arm, model) in &m.models {
        writeln!(out, "- Model `{arm}`: `{model}`")?;
    }
    for (program, version) in &m.versions {
        writeln!(out, "- Version `{program}`: `{version}`")?;
    }
    writeln!(
        out,
        "- Trials: {} planned, {} completed, {} skipped; spent {}${:.2} of ${:.2}{}",
        m.planned,
        m.completed,
        m.skipped,
        m.spent_bound.marker(),
        m.spent_usd,
        m.max_usd,
        if m.unpriced_trials > 0 {
            format!(" ({} trials unpriced)", m.unpriced_trials)
        } else {
            String::new()
        }
    )?;
    if let Some(reason) = &m.stopped_reason {
        writeln!(out, "- **Stopped early:** {reason}")?;
    }
    out.push('\n');
    Ok(())
}

fn render_routing_table(
    out: &mut String,
    title: &str,
    buckets: &BTreeMap<String, Aggregate>,
) -> anyhow::Result<()> {
    if buckets.is_empty() {
        return Ok(());
    }
    writeln!(out, "\n## {title}\n\n{ROUTING_NOTES}\n")?;
    writeln!(
        out,
        "| Bucket | Trials | Pass rate | Wall s | Cost $ | Infra | Timeouts |"
    )?;
    writeln!(out, "|---|---:|---:|---:|---:|---:|---:|")?;
    for (label, a) in buckets {
        assert_eq!(a.arm, Arm::Archon, "routing buckets contain Archon only");
        assert!(a.successes <= a.trials, "passes cannot exceed trials");
        writeln!(
            out,
            "| {} | {} | {}/{} ({}) | {} | {} | {} | {} |",
            label.replace('|', "\\|").replace(['\n', '\r'], " "),
            a.trials,
            a.successes,
            a.trials,
            percent(a.success_rate),
            stat(&a.wall_secs, 0),
            cost_text(a),
            a.infra,
            a.timeouts
        )?;
    }
    Ok(())
}

fn render_task_table(out: &mut String, summary: &Summary) -> anyhow::Result<()> {
    let arms: Vec<Arm> = summary.arms.iter().map(|a| a.arm).collect();
    writeln!(
        out,
        "\n## Per task\n\nEach cell: passes/trials · wall s · cost $ · operator msgs (mean ± stdev).\n"
    )?;
    let header: Vec<String> = arms.iter().map(ToString::to_string).collect();
    writeln!(out, "| Task | {} |", header.join(" | "))?;
    writeln!(out, "|---|{}", "---|".repeat(arms.len()))?;
    for (task, cells) in &summary.tasks {
        let row: Vec<String> = arms
            .iter()
            .map(|arm| {
                cells
                    .iter()
                    .find(|c| c.arm == *arm)
                    .map_or_else(|| "—".to_owned(), cell_text)
            })
            .collect();
        writeln!(out, "| {task} | {} |", row.join(" | "))?;
    }
    Ok(())
}

fn render_infra(out: &mut String, infra: &[InfraFailure]) -> anyhow::Result<()> {
    if infra.is_empty() {
        return Ok(());
    }
    writeln!(
        out,
        "\n## Infrastructure failures\n\nLeft out of every number above.\n"
    )?;
    for f in infra {
        let error: String = f.error.chars().take(INFRA_ERROR_CHARS).collect();
        let cut = if f.error.chars().count() > INFRA_ERROR_CHARS {
            "…"
        } else {
            ""
        };
        writeln!(
            out,
            "- {} / {} #{}: {}{cut}",
            f.task,
            f.arm,
            f.trial,
            error.replace('\n', " ")
        )?;
    }
    Ok(())
}

fn cell_text(c: &Aggregate) -> String {
    format!(
        "{}/{} · {} · {} · {}",
        c.successes,
        c.trials,
        stat(&c.wall_secs, 0),
        cost_text(c),
        stat(&c.operator_messages, 1)
    )
}

/// How much of an infrastructure failure's error `summary.md` quotes.
const INFRA_ERROR_CHARS: usize = 200;

pub(crate) const ROUTING_NOTES: &str = "Archon whole-trial observations: each trial appears once per table. Mixed buckets retain all selected and attempted pipelines or selected review levels. Pass rate is the trial grader verdict; cost and wall time cover the whole trial, including orchestration. These are not per-item outcomes or causal comparisons. Review levels are captured policy, not proof of completed review; historical policy changes are unavailable. Unknown means missing metadata; no work means a captured empty queue. Infrastructure failures are excluded from metrics.";

const TIMEOUT_NOTES: &str = "A trial that hit its deadline keeps the grader verdict and score of its succeeded items only; what its cancelled items had committed counts for nothing in the success, pass@k and score columns. Here the last committed branch of each timed-out Archon item was graded as well, which shows a slow but correct solution. These results are never counted as passes.";

fn percent(x: f64) -> String {
    format!("{:.0}%", x * 100.0)
}

fn stat(s: &Stat, decimals: usize) -> String {
    format!("{:.*} ± {:.*}", decimals, s.mean, decimals, s.stdev)
}

/// The cost cell, prefixed with the mark for its [`CostBound`].
pub(crate) fn cost_text(a: &Aggregate) -> String {
    let text = opt_stat(a.cost_usd.as_ref(), 2, a.trials);
    if a.cost_usd.is_none() {
        return text;
    }
    format!("{}{text}", a.cost_bound.marker())
}

/// A metric only some trials reported (tokens, cost) says over how many.
fn opt_stat(s: Option<&Stat>, decimals: usize, trials: u32) -> String {
    let Some(s) = s else {
        return "—".to_owned();
    };
    assert!(
        s.n <= trials as usize,
        "{} values from {trials} trials",
        s.n
    );
    if s.n < trials as usize {
        return format!("{} (n={})", stat(s, decimals), s.n);
    }
    stat(s, decimals)
}

const METHOD_NOTES: &str = "
## Method

- Every arm gets the same task prompt, effort and time limit, and the same model where the harness allows; `codex` runs its own model. Every arm runs with its tool permission prompts bypassed.
- `claude` keeps Claude Code's built-in subagents (the Task tool); the `aether` arm is a single leaf agent without delegation.
- Each trial starts from a fresh copy of the fixtures in an unguessably named directory outside the Archon checkout, with a one-commit git history. Hidden tests, reference solutions and rubrics are copied in only at grade time, into a separate copy the agent never sees, after the agent and every process it started have stopped. Agent processes do not inherit environment variables pointing at the checkout. There is no OS sandbox: an agent that searched the whole home directory could still find the checkout.
- Success is the hidden grader's verdict. Test graders restore the visible tests from the task, not from the agent's repository, and require the runner's own summary to report no failures and a minimum number of passing tests. Rubric graders need related facts close together and cap the answer length.
- A failed trial with a harness error (the arm could not run cleanly: a provider limit, a crash, a grading failure) is an infrastructure failure. It is left out of pass rates and every metric, counted under Infra and listed with its error.
- Wall time runs from handing over the prompt until the arm finished; Archon's daemon start-up (recorded as `setup_secs`) and the quiet window used to detect that Archon finished are excluded.
- Input tokens include cache reads and writes. Cost is the API-equivalent total: billed dollars plus Aether's estimate of unbilled usage (such as Codex), or `--price` list prices for an arm that reports neither. `(n=…)` marks a metric only that many trials reported. `≥` marks a cost Aether reported as incomplete (`cost_gap`); the true cost is at least that. `*` marks a cost that includes one Aether reported as incomplete together with estimates; it is not a bound.
- Operator messages are what the operator had to read: one final result for `claude`/`codex`, the assistant messages of the Aether agent, and the default-visible chat lines for Archon.
- Values are mean ± sample standard deviation over graded trials. pass@k uses the unbiased estimator per task, averaged over tasks, with k the fewest graded trials any (task, arm) cell has.
";

#[cfg(test)]
mod tests;
