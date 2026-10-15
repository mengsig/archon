//! One JSON line per trial in `results.jsonl` (ARCHITECTURE §15).

use std::collections::BTreeMap;
use std::fmt;
use std::str::FromStr;

use archon_core::Timestamp;
use archon_core::autonomy::ReviewLevel;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Arm {
    /// The full Archon stack: orchestrator + pipelines.
    Archon,
    /// One plain Aether root agent.
    Aether,
    /// `claude -p`.
    Claude,
    /// `codex exec`.
    Codex,
}

impl Arm {
    pub const ALL: [Arm; 4] = [Arm::Archon, Arm::Aether, Arm::Claude, Arm::Codex];

    pub fn name(self) -> &'static str {
        match self {
            Self::Archon => "archon",
            Self::Aether => "aether",
            Self::Claude => "claude",
            Self::Codex => "codex",
        }
    }
}

impl fmt::Display for Arm {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

impl FromStr for Arm {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|arm| arm.name() == s.trim())
            .ok_or_else(|| {
                anyhow::anyhow!("unknown arm {s:?}; expected archon, aether, claude or codex")
            })
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TrialResult {
    pub task: String,
    pub arm: Arm,
    pub trial: u32,
    pub model: String,
    pub effort: Option<String>,
    /// Grader exit status 0.
    pub success: bool,
    /// Optional grader JSON `{"score": 0..1}`.
    pub score: Option<f64>,
    /// From handing over the prompt to the arm finishing; excludes fixture
    /// setup, daemon startup (`setup_secs`) and grading.
    pub wall_secs: f64,
    /// Every prompt token, cached or not, so arms compare like for like.
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    /// The part of `input_tokens` served from a prompt cache.
    #[serde(default)]
    pub cached_input_tokens: Option<u64>,
    /// Dollars the harness reported as billed (or list price, for `claude`).
    pub cost_usd: Option<f64>,
    /// Subscription estimates, or `--price` applied to the token counts.
    pub estimated_cost_usd: Option<f64>,
    /// Why Aether called `cost_usd` incomplete; present means it is a floor.
    /// An unpriced gap that the same agent's estimate covers is left out.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_gap: Option<String>,
    /// Messages the operator would have had to read.
    pub operator_messages: u32,
    /// Harness failure (not a task failure): the trial did not run cleanly.
    pub error: Option<String>,
    #[serde(default)]
    pub timed_out: bool,
    /// Archon only: daemon start until the orchestrator was ready.
    #[serde(default)]
    pub setup_secs: Option<f64>,
    #[serde(default)]
    pub started_at: Option<Timestamp>,
    #[serde(default)]
    pub finished_at: Option<Timestamp>,
    /// Grading remarks, such as a result branch left out on a merge conflict.
    #[serde(default)]
    pub note: Option<String>,
    /// Captured queue work; absent means unknown, an empty list means no work.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub archon_work: Option<Vec<ArchonItem>>,
    /// Timed-out trials only: the grader's result with the last committed
    /// branch of each timed-out item added. It never changes `success`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub graded_on_timeout: Option<TimeoutGrade>,
}

/// The grader's verdict on a timed-out trial's committed work, kept apart
/// from the trial's own verdict so it is never counted as a pass.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct TimeoutGrade {
    pub success: bool,
    pub score: Option<f64>,
}

/// Routing observed at capture time, without attributing the trial verdict to items.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ArchonItem {
    pub pipeline: String,
    #[serde(default)]
    pub run_pipelines: Vec<String>,
    /// Selected review policy, not proof that reviewers ran or passed.
    pub review: Option<ReviewLevel>,
}

impl TrialResult {
    /// A failed trial the arm never got a fair run at: the harness or the
    /// provider broke (`error` set). Kept out of pass rates and metrics.
    pub fn is_infra_failure(&self) -> bool {
        !self.success && self.error.is_some()
    }

    /// The cost to compare arms by: the API-equivalent total, billed dollars
    /// plus the estimate of unbilled usage. `None` means nobody priced the trial.
    pub fn effective_cost_usd(&self) -> Option<f64> {
        if self.cost_usd.is_none() && self.estimated_cost_usd.is_none() {
            return None;
        }
        let (billed, estimate) = (
            self.cost_usd.unwrap_or(0.0),
            self.estimated_cost_usd.unwrap_or(0.0),
        );
        assert!(billed >= 0.0 && estimate >= 0.0, "costs are never negative");
        Some(billed + estimate)
    }

    /// Aether reported work this trial's cost leaves out, so it is a lower bound.
    /// Gaps covered by an agent's own estimate never get here (`usage::aether_usage`).
    pub fn cost_is_floor(&self) -> bool {
        self.cost_gap.is_some() && self.effective_cost_usd().is_some()
    }

    /// Some of the effective cost is an estimate of unbilled usage (Codex,
    /// subscriptions or `--price`), so it bounds nothing.
    pub fn cost_is_estimate(&self) -> bool {
        self.estimated_cost_usd
            .is_some_and(|estimate| estimate > 0.0)
    }
}

/// What a sum or mean of trial costs can claim. Floors are lower bounds and
/// estimates are not, so the two together support neither claim.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CostBound {
    /// Billed or estimated figures only: no mark.
    #[default]
    Exact,
    /// Billed figures where some are reported incomplete: at least this much.
    Floor,
    /// Reported-incomplete costs mixed with estimates: not a bound.
    Incomplete,
}

impl CostBound {
    pub fn of<'a>(results: impl IntoIterator<Item = &'a TrialResult>) -> Self {
        let (mut floor, mut estimate) = (false, false);
        for result in results {
            floor |= result.cost_is_floor();
            estimate |= result.cost_is_estimate();
        }
        Self::from_flags(floor, estimate)
    }

    /// `floor`: some cost is a reported-incomplete figure; `estimate`: some
    /// cost is a substituted estimate.
    pub fn from_flags(floor: bool, estimate: bool) -> Self {
        match (floor, estimate) {
            (true, true) => Self::Incomplete,
            (true, false) => Self::Floor,
            (false, _) => Self::Exact,
        }
    }

    /// The prefix drawn before the figure.
    pub fn marker(self) -> &'static str {
        match self {
            Self::Exact => "",
            Self::Floor => "≥ ",
            Self::Incomplete => "* ",
        }
    }
}

/// `run.json`: everything needed to reproduce and judge a run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunManifest {
    pub stamp: String,
    pub started_at: Timestamp,
    pub finished_at: Option<Timestamp>,
    /// `git rev-parse HEAD` of the Archon checkout the bench ran from.
    pub archon_commit: Option<String>,
    /// Program → first line of `--version` (and aetherd's daemon version).
    pub versions: BTreeMap<String, String>,
    /// Arm → the model label recorded in its results.
    pub models: BTreeMap<String, String>,
    pub effort: String,
    pub arms: Vec<Arm>,
    pub tasks: Vec<String>,
    pub trials: u32,
    pub parallel: usize,
    pub timeout_factor: f64,
    pub max_usd: f64,
    pub max_trials: usize,
    pub planned: usize,
    pub completed: usize,
    pub skipped: usize,
    pub spent_usd: f64,
    /// Whether `spent_usd` is exact, a floor or incomplete; see [`CostBound`].
    #[serde(default)]
    pub spent_bound: CostBound,
    pub unpriced_trials: u32,
    /// Why the run ended early (budget exhausted), when it did.
    pub stopped_reason: Option<String>,
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn result(cost: Option<f64>, estimate: Option<f64>) -> TrialResult {
        TrialResult {
            task: "t".into(),
            arm: Arm::Claude,
            trial: 1,
            model: "m".into(),
            effort: None,
            success: true,
            score: None,
            wall_secs: 1.0,
            input_tokens: None,
            output_tokens: None,
            cached_input_tokens: None,
            cost_usd: cost,
            estimated_cost_usd: estimate,
            cost_gap: None,
            operator_messages: 1,
            error: None,
            timed_out: false,
            setup_secs: None,
            started_at: None,
            finished_at: None,
            note: None,
            archon_work: None,
            graded_on_timeout: None,
        }
    }

    #[test]
    fn any_estimated_dollar_makes_the_cost_an_estimate() {
        assert!(result(None, Some(2.0)).cost_is_estimate());
        assert!(result(Some(0.0), Some(2.0)).cost_is_estimate());
        assert!(result(Some(1.0), Some(2.0)).cost_is_estimate());
        assert!(!result(Some(1.0), Some(0.0)).cost_is_estimate());
        assert!(!result(Some(2.0), None).cost_is_estimate());
        assert!(!result(None, None).cost_is_estimate());
    }

    #[test]
    fn a_floor_mixed_with_an_estimate_is_incomplete() {
        let estimate = result(None, Some(5.0));
        let mut gapped = result(Some(1.0), None);
        gapped.cost_gap = Some("interrupted".into());
        assert_eq!(CostBound::of([&estimate]), CostBound::Exact);
        assert_eq!(
            CostBound::of([&gapped, &result(Some(1.0), None)]),
            CostBound::Floor
        );
        assert_eq!(CostBound::of([&estimate, &gapped]), CostBound::Incomplete);
        assert_eq!(CostBound::of([]), CostBound::Exact);
    }

    #[test]
    fn cost_bounds_serialize_lowercase_and_default_to_exact() {
        assert_eq!(
            serde_json::to_string(&CostBound::Incomplete).unwrap(),
            "\"incomplete\""
        );
        assert_eq!(
            serde_json::from_str::<CostBound>("\"floor\"").unwrap(),
            CostBound::Floor
        );
        assert_eq!(CostBound::default(), CostBound::Exact);
    }

    #[test]
    fn arms_round_trip_through_their_names() {
        for arm in Arm::ALL {
            assert_eq!(arm.name().parse::<Arm>().unwrap(), arm);
            let json = serde_json::to_string(&arm).unwrap();
            assert_eq!(json, format!("\"{arm}\""));
        }
        assert!("gpt".parse::<Arm>().is_err());
    }

    #[test]
    fn effective_cost_is_billed_plus_estimated_dollars() {
        // A Claude lead billed $2 and its Codex reviewer was estimated at $5.
        assert_eq!(result(Some(2.0), Some(5.0)).effective_cost_usd(), Some(7.0));
        assert_eq!(result(Some(0.0), Some(5.0)).effective_cost_usd(), Some(5.0));
        assert_eq!(result(None, Some(1.5)).effective_cost_usd(), Some(1.5));
        assert_eq!(result(Some(0.0), None).effective_cost_usd(), Some(0.0));
        assert_eq!(result(None, None).effective_cost_usd(), None);
    }

    #[test]
    fn a_gap_left_at_the_trial_is_a_floor_whatever_the_estimate() {
        // Covered gaps were dropped per agent, so any left mark unpriced work.
        let gapped = |cost, estimate| {
            let mut r = result(cost, estimate);
            r.cost_gap = Some("unpriced".into());
            r
        };
        let mut interrupted = result(Some(2.0), Some(5.0));
        interrupted.cost_gap = Some("interrupted".into());
        assert!(interrupted.cost_is_floor() && interrupted.cost_is_estimate());
        assert_eq!(CostBound::of([&interrupted]), CostBound::Incomplete);
        assert!(gapped(Some(2.0), Some(5.0)).cost_is_floor());
        assert_eq!(
            CostBound::of([&gapped(None, Some(5.0))]),
            CostBound::Incomplete
        );
        assert_eq!(CostBound::of([&gapped(Some(0.0), None)]), CostBound::Floor);
        assert!(!gapped(None, None).cost_is_floor());
        assert!(!result(Some(2.0), None).cost_is_floor());
    }

    #[test]
    fn old_result_lines_without_new_fields_still_parse() {
        let line = r#"{"task":"t","arm":"codex","trial":2,"model":"m","effort":null,
            "success":false,"score":null,"wall_secs":3.5,"input_tokens":10,
            "output_tokens":2,"cost_usd":null,"estimated_cost_usd":null,
            "operator_messages":1,"error":null}"#;
        let parsed: TrialResult = serde_json::from_str(line).unwrap();
        assert_eq!(parsed.arm, Arm::Codex);
        assert!(!parsed.timed_out && parsed.cached_input_tokens.is_none());
        assert!(parsed.archon_work.is_none() && parsed.graded_on_timeout.is_none());
        assert_eq!(parsed.cost_gap, None);
    }

    #[test]
    fn cost_gap_round_trips_and_stays_absent_when_unused() {
        let plain = serde_json::to_string(&result(Some(1.0), None)).unwrap();
        assert!(!plain.contains("cost_gap"), "{plain}");
        let mut floor = result(Some(1.0), None);
        floor.cost_gap = Some("interrupted".into());
        let parsed: TrialResult =
            serde_json::from_str(&serde_json::to_string(&floor).unwrap()).unwrap();
        assert_eq!(parsed.cost_gap.as_deref(), Some("interrupted"));
    }

    #[test]
    fn graded_on_timeout_round_trips_and_stays_absent_when_unused() {
        let plain = serde_json::to_string(&result(None, None)).unwrap();
        assert!(!plain.contains("graded_on_timeout"), "{plain}");
        let mut late = result(None, None);
        late.success = false;
        late.timed_out = true;
        late.graded_on_timeout = Some(TimeoutGrade {
            success: true,
            score: Some(1.0),
        });
        let parsed: TrialResult =
            serde_json::from_str(&serde_json::to_string(&late).unwrap()).unwrap();
        assert_eq!(parsed, late);
        assert!(!parsed.success, "the verdict is untouched");
    }
}
