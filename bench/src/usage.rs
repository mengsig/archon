//! Token and cost accounting from each arm's own output, normalized so the
//! arms compare like for like: `input_tokens` counts every prompt token
//! (fresh, cache writes and cache reads), `cached_input_tokens` the reads.

use std::collections::BTreeMap;
use std::str::FromStr;

use anyhow::{Context, bail, ensure};
use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Usage {
    pub input_tokens: u64,
    pub cached_input_tokens: u64,
    pub output_tokens: u64,
    pub cost_usd: Option<f64>,
    pub estimated_cost_usd: Option<f64>,
    /// Aether's reason `cost_usd` is incomplete, making it a lower bound.
    pub cost_gap: Option<String>,
}

impl Usage {
    fn add(&mut self, other: &Usage) {
        self.input_tokens += other.input_tokens;
        self.cached_input_tokens += other.cached_input_tokens;
        self.output_tokens += other.output_tokens;
        self.cost_usd = add_opt(self.cost_usd, other.cost_usd);
        self.estimated_cost_usd = add_opt(self.estimated_cost_usd, other.estimated_cost_usd);
        self.cost_gap = merge_gaps(self.cost_gap.take(), other.cost_gap.clone());
    }
}

/// Equal reasons keep the reason; different ones become `mixed`.
fn merge_gaps(a: Option<String>, b: Option<String>) -> Option<String> {
    match (a, b) {
        (Some(a), Some(b)) if a != b => Some("mixed".into()),
        (a, b) => a.or(b),
    }
}

fn add_opt(a: Option<f64>, b: Option<f64>) -> Option<f64> {
    match (a, b) {
        (None, None) => None,
        (a, b) => Some(a.unwrap_or(0.0) + b.unwrap_or(0.0)),
    }
}

/// List prices in USD per million tokens, for arms that report no cost.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Price {
    pub input: f64,
    pub cached_input: f64,
    pub output: f64,
}

impl Price {
    pub fn estimate(&self, usage: &Usage) -> f64 {
        assert!(
            usage.cached_input_tokens <= usage.input_tokens,
            "cached tokens are a subset of input tokens"
        );
        let fresh = (usage.input_tokens - usage.cached_input_tokens) as f64;
        let dollars = fresh * self.input
            + usage.cached_input_tokens as f64 * self.cached_input
            + usage.output_tokens as f64 * self.output;
        dollars / 1_000_000.0
    }
}

/// `--price MODEL=IN,CACHED_IN,OUT` (USD per million tokens).
#[derive(Debug, Clone, PartialEq)]
pub struct PriceArg {
    pub model: String,
    pub price: Price,
}

impl FromStr for PriceArg {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (model, rates) = s
            .split_once('=')
            .context("--price is MODEL=IN,CACHED_IN,OUT in USD per million tokens")?;
        let numbers: Vec<f64> = rates
            .split(',')
            .map(|n| n.trim().parse::<f64>())
            .collect::<Result<_, _>>()
            .with_context(|| format!("--price {s:?}: rates must be numbers"))?;
        ensure!(
            numbers.len() == 3,
            "--price {s:?}: give exactly three rates IN,CACHED_IN,OUT"
        );
        ensure!(
            numbers.iter().all(|n| n.is_finite() && *n >= 0.0),
            "--price {s:?}: rates must be non-negative"
        );
        ensure!(
            !model.trim().is_empty(),
            "--price {s:?}: the model is empty"
        );
        Ok(Self {
            model: model.trim().to_owned(),
            price: Price {
                input: numbers[0],
                cached_input: numbers[1],
                output: numbers[2],
            },
        })
    }
}

/// What `claude -p --output-format json` reported.
#[derive(Debug, Clone, PartialEq)]
pub struct ClaudeOutcome {
    pub usage: Usage,
    /// The final result text, which is all the operator reads.
    pub result: String,
    pub is_error: bool,
    pub subtype: String,
}

/// Accepts the single result object, or the array `--verbose` prints.
pub fn parse_claude_json(stdout: &str) -> anyhow::Result<ClaudeOutcome> {
    let value: Value = serde_json::from_str(stdout.trim())
        .context("claude stdout is not JSON; was --output-format json passed?")?;
    let result = match &value {
        Value::Array(items) => items
            .iter()
            .rev()
            .find(|item| item["type"] == "result")
            .context("claude JSON array has no `result` entry")?,
        Value::Object(_) => &value,
        _ => bail!("claude JSON is neither an object nor an array"),
    };
    ensure!(
        result["type"] == "result",
        "claude JSON is not a `result` message: type {}",
        result["type"]
    );
    let mut usage =
        claude_model_usage(&result["modelUsage"]).unwrap_or_else(|| claude_usage(&result["usage"]));
    usage.cost_usd = result["total_cost_usd"].as_f64();
    Ok(ClaudeOutcome {
        usage,
        result: result["result"].as_str().unwrap_or_default().to_owned(),
        is_error: result["is_error"].as_bool().unwrap_or(false),
        subtype: result["subtype"].as_str().unwrap_or_default().to_owned(),
    })
}

fn claude_usage(usage: &Value) -> Usage {
    let field = |name: &str| usage[name].as_u64().unwrap_or(0);
    let cached = field("cache_read_input_tokens");
    Usage {
        input_tokens: field("input_tokens") + field("cache_creation_input_tokens") + cached,
        cached_input_tokens: cached,
        output_tokens: field("output_tokens"),
        ..Usage::default()
    }
}

/// Per-model totals, which also cover helper models `usage` leaves out.
fn claude_model_usage(models: &Value) -> Option<Usage> {
    let models = models.as_object().filter(|m| !m.is_empty())?;
    let mut total = Usage::default();
    for usage in models.values() {
        let field = |name: &str| usage[name].as_u64().unwrap_or(0);
        let cached = field("cacheReadInputTokens");
        total.input_tokens += field("inputTokens") + field("cacheCreationInputTokens") + cached;
        total.cached_input_tokens += cached;
        total.output_tokens += field("outputTokens");
    }
    Some(total)
}

/// What `codex exec --json` reported.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct CodexOutcome {
    pub usage: Option<Usage>,
    /// The last agent message: the final answer the operator reads.
    pub last_message: Option<String>,
    pub errors: Vec<String>,
}

/// Reads both event shapes: current (`turn.completed`) and legacy
/// (`msg.type == "token_count"`). Both carry the thread's running total
/// (codex's `total_token_usage`), so the last one is the trial's usage.
pub fn parse_codex_jsonl(stdout: &str) -> CodexOutcome {
    let mut outcome = CodexOutcome::default();
    let mut turn_total: Option<Usage> = None;
    let mut legacy_total: Option<Usage> = None;
    for line in stdout.lines().map(str::trim).filter(|l| l.starts_with('{')) {
        let Ok(event) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        match event["type"].as_str() {
            Some("turn.completed") => turn_total = Some(codex_usage(&event["usage"])),
            Some("item.completed") if event["item"]["type"] == "agent_message" => {
                outcome.last_message = event["item"]["text"].as_str().map(str::to_owned);
            }
            Some("turn.failed") => outcome.errors.push(error_text(&event["error"])),
            Some("error") => outcome.errors.push(error_text(&event)),
            _ => legacy_event(&event["msg"], &mut outcome, &mut legacy_total),
        }
    }
    outcome.usage = turn_total.or(legacy_total);
    outcome
}

fn legacy_event(msg: &Value, outcome: &mut CodexOutcome, total: &mut Option<Usage>) {
    match msg["type"].as_str() {
        Some("token_count") if msg["info"].is_object() => {
            *total = Some(codex_usage(&msg["info"]["total_token_usage"]));
        }
        Some("agent_message") => {
            outcome.last_message = msg["message"].as_str().map(str::to_owned);
        }
        Some("error") => outcome.errors.push(error_text(msg)),
        _ => {}
    }
}

/// Codex counts cached tokens inside `input_tokens` already.
fn codex_usage(usage: &Value) -> Usage {
    let field = |name: &str| usage[name].as_u64().unwrap_or(0);
    let input = field("input_tokens");
    Usage {
        input_tokens: input,
        cached_input_tokens: field("cached_input_tokens").min(input),
        output_tokens: field("output_tokens"),
        ..Usage::default()
    }
}

fn error_text(value: &Value) -> String {
    value["message"]
        .as_str()
        .map(str::to_owned)
        .unwrap_or_else(|| value.to_string())
}

/// Sum of the latest `agent.usage_updated` totals per agent. Aether's totals
/// are absolute per agent, so only the highest `seq` of each agent counts.
pub fn aether_usage(events: &[Value]) -> anyhow::Result<Option<Usage>> {
    let mut latest: BTreeMap<String, (u64, &Value)> = BTreeMap::new();
    for event in events {
        if event["kind"]["type"] != "agent.usage_updated" {
            continue;
        }
        let agent = event["agent_id"]
            .as_str()
            .context("agent.usage_updated event without agent_id")?;
        let seq = event["seq"].as_u64().context("event without seq")?;
        let slot = latest
            .entry(agent.to_owned())
            .or_insert((seq, &event["kind"]["total"]));
        if seq >= slot.0 {
            *slot = (seq, &event["kind"]["total"]);
        }
    }
    if latest.is_empty() {
        return Ok(None);
    }
    let mut total = Usage::default();
    for (_, usage) in latest.values() {
        total.add(&aether_total(usage));
    }
    Ok(Some(total))
}

fn aether_total(total: &Value) -> Usage {
    let field = |name: &str| total[name].as_u64().unwrap_or(0);
    let cached = field("cache_read_tokens");
    Usage {
        input_tokens: field("input_tokens") + field("cache_write_tokens") + cached,
        cached_input_tokens: cached,
        output_tokens: field("output_tokens"),
        cost_usd: total["cost_usd"].as_f64(),
        estimated_cost_usd: total["estimated_cost_usd"].as_f64(),
        cost_gap: uncovered_gap(cost_gap(&total["cost_gap"]), &total["estimated_cost_usd"]),
    }
}

/// An agent's own estimate prices its unpriced usage, so only a gap it leaves
/// uncovered reaches the trial, where it then always means a floor.
fn uncovered_gap(gap: Option<String>, estimate: &Value) -> Option<String> {
    let estimated = estimate.as_f64().is_some_and(|e| e > 0.0);
    gap.filter(|reason| !(estimated && reason == "unpriced"))
}

/// Any present value marks the cost incomplete, so an unrecognised or oddly
/// typed one is kept as `unknown` instead of being dropped.
fn cost_gap(value: &Value) -> Option<String> {
    if value.is_null() {
        return None;
    }
    Some(value.as_str().unwrap_or("unknown").to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(name: &str) -> String {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(name);
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
    }

    #[test]
    fn claude_result_uses_model_usage_and_total_cost() {
        let outcome = parse_claude_json(&sample("claude-result.json")).unwrap();
        assert!(!outcome.is_error);
        assert_eq!(outcome.subtype, "success");
        assert!(outcome.result.contains("all 3 tests pass"));
        // opus + haiku per-model totals, cache reads and writes included.
        assert_eq!(outcome.usage.input_tokens, (9 + 21_450 + 118_902) + 1_210);
        assert_eq!(outcome.usage.cached_input_tokens, 118_902);
        assert_eq!(outcome.usage.output_tokens, 2_817 + 64);
        assert_eq!(outcome.usage.cost_usd, Some(0.4431925));
    }

    #[test]
    fn claude_error_result_and_verbose_array_are_read() {
        let outcome = parse_claude_json(&sample("claude-error.json")).unwrap();
        assert!(outcome.is_error);
        assert_eq!(outcome.subtype, "error_max_turns");
        // No modelUsage: falls back to the aggregate `usage`.
        assert_eq!(outcome.usage.input_tokens, 4 + 3_000 + 20_000);
        let array = format!("[{{\"type\":\"system\"}},{}]", sample("claude-result.json"));
        assert_eq!(
            parse_claude_json(&array).unwrap().usage.output_tokens,
            2_817 + 64
        );
        assert!(parse_claude_json("not json").is_err());
        assert!(parse_claude_json("{\"type\":\"assistant\"}").is_err());
    }

    #[test]
    fn codex_turn_totals_are_running_totals_so_the_last_counts() {
        let outcome = parse_codex_jsonl(&sample("codex-exec.jsonl"));
        let usage = outcome.usage.unwrap();
        assert_eq!(usage.input_tokens, 57_331);
        assert_eq!(usage.cached_input_tokens, 40_704);
        assert_eq!(usage.output_tokens, 3_613);
        assert_eq!(usage.cost_usd, None);
        assert_eq!(
            outcome.last_message.as_deref(),
            Some("Fixed the off-by-one in window.py; all tests pass.")
        );
        assert!(outcome.errors.is_empty());
    }

    #[test]
    fn real_cli_output_is_read() {
        // Captured from `codex exec --json` 0.155.1 and `claude -p` 2.1.280.
        let codex = parse_codex_jsonl(&sample("codex-exec-0.155.1.jsonl"));
        let usage = codex.usage.unwrap();
        assert_eq!(
            (
                usage.input_tokens,
                usage.cached_input_tokens,
                usage.output_tokens
            ),
            (15_071, 10_624, 5)
        );
        assert_eq!(codex.last_message.as_deref(), Some("hi"));
        let claude = parse_claude_json(&sample("claude-result-2.1.280.json")).unwrap();
        assert!(!claude.is_error && claude.result.starts_with("Hi!"));
        assert_eq!(claude.usage.input_tokens, 2 + 14_626 + 10_234);
        assert_eq!(claude.usage.cached_input_tokens, 10_234);
        assert_eq!(claude.usage.output_tokens, 13);
        assert_eq!(claude.usage.cost_usd, Some(0.1193228));
    }

    #[test]
    fn codex_legacy_token_count_takes_the_last_absolute_total() {
        let outcome = parse_codex_jsonl(&sample("codex-legacy.jsonl"));
        let usage = outcome.usage.unwrap();
        assert_eq!(usage.input_tokens, 25_600);
        assert_eq!(usage.cached_input_tokens, 12_288);
        assert_eq!(usage.output_tokens, 1_904);
        assert_eq!(outcome.last_message.as_deref(), Some("Done."));
    }

    #[test]
    fn codex_failures_are_collected() {
        let text = "{\"type\":\"thread.started\",\"thread_id\":\"t\"}\n\
                    {\"type\":\"error\",\"message\":\"stream disconnected\"}\n\
                    {\"type\":\"turn.failed\",\"error\":{\"message\":\"usage limit reached\"}}\n";
        let outcome = parse_codex_jsonl(text);
        assert_eq!(
            outcome.errors,
            ["stream disconnected", "usage limit reached"]
        );
        assert_eq!(outcome.usage, None);
    }

    #[test]
    fn aether_usage_keeps_the_latest_total_per_agent() {
        let events: Vec<Value> = sample("aether-usage-events.jsonl")
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        let usage = aether_usage(&events).unwrap().unwrap();
        // agt_root's latest (seq 40) plus agt_child's only total (seq 31).
        assert_eq!(usage.input_tokens, (12 + 900 + 40_000) + (5 + 100 + 2_000));
        assert_eq!(usage.cached_input_tokens, 42_000);
        assert_eq!(usage.output_tokens, 1_500 + 80);
        assert_eq!(usage.cost_usd, Some(0.0));
        assert!((usage.estimated_cost_usd.unwrap() - 0.93).abs() < 1e-9);
        assert_eq!(aether_usage(&[]).unwrap(), None);
    }

    fn usage_event(agent: &str, seq: u64, gap: Option<Value>) -> Value {
        let mut total = serde_json::json!({"input_tokens": 1, "output_tokens": 1, "cost_usd": 0.5});
        if let Some(gap) = gap {
            total["cost_gap"] = gap;
        }
        serde_json::json!({
            "agent_id": agent, "seq": seq,
            "kind": {"type": "agent.usage_updated", "total": total}
        })
    }

    fn gap_of(events: &[Value]) -> Option<String> {
        aether_usage(events).unwrap().unwrap().cost_gap
    }

    #[test]
    fn aether_cost_gap_is_absent_kept_or_unknown_never_an_error() {
        let gap = |v: Value| gap_of(&[usage_event("a", 1, Some(v))]);
        assert_eq!(gap_of(&[usage_event("a", 1, None)]), None);
        assert_eq!(gap(Value::Null), None);
        assert_eq!(gap("interrupted".into()), Some("interrupted".into()));
        assert_eq!(
            gap("a-future-reason".into()),
            Some("a-future-reason".into())
        );
        assert_eq!(gap(serde_json::json!(7)), Some("unknown".into()));
    }

    fn priced_event(agent: &str, cost: f64, estimate: Option<f64>, gap: Option<&str>) -> Value {
        let mut total =
            serde_json::json!({"input_tokens": 1, "output_tokens": 1, "cost_usd": cost});
        total["estimated_cost_usd"] = estimate.into();
        total["cost_gap"] = gap.into();
        serde_json::json!({
            "agent_id": agent, "seq": 1,
            "kind": {"type": "agent.usage_updated", "total": total}
        })
    }

    /// A Claude lead billed $2 and a Codex reviewer estimated at $5; an
    /// unpriced sibling without an estimate keeps the trial's cost a floor.
    #[test]
    fn an_estimate_covers_only_its_own_agents_unpriced_gap() {
        use crate::metrics::{CostBound, tests::result};
        let lead = priced_event("lead", 2.0, None, None);
        let reviewer = priced_event("reviewer", 0.0, Some(5.0), Some("unpriced"));
        let sibling = priced_event("sibling", 0.0, None, Some("unpriced"));
        let trial = |events: &[Value]| {
            let usage = aether_usage(events).unwrap().unwrap();
            let mut trial = result(usage.cost_usd, usage.estimated_cost_usd);
            trial.cost_gap = usage.cost_gap;
            trial
        };
        let covered = trial(&[lead.clone(), reviewer.clone()]);
        assert_eq!(covered.cost_gap, None);
        assert_eq!(covered.effective_cost_usd(), Some(7.0));
        assert!(covered.cost_is_estimate() && !covered.cost_is_floor());
        let partly = trial(&[lead, reviewer, sibling]);
        assert_eq!(partly.cost_gap.as_deref(), Some("unpriced"));
        assert_eq!(partly.effective_cost_usd(), Some(7.0));
        assert!(partly.cost_is_estimate());
        assert_eq!(CostBound::of([&partly]), CostBound::Incomplete);
        let zero = priced_event("zero", 0.0, Some(0.0), Some("unpriced"));
        assert_eq!(gap_of(&[zero]), Some("unpriced".into()));
    }

    #[test]
    fn aether_cost_gaps_combine_across_agents() {
        let same = [
            usage_event("a", 1, Some("interrupted".into())),
            usage_event("b", 2, Some("interrupted".into())),
        ];
        assert_eq!(gap_of(&same), Some("interrupted".into()));
        let different = [
            usage_event("a", 1, Some("interrupted".into())),
            usage_event("b", 2, Some("unpriced".into())),
        ];
        assert_eq!(gap_of(&different), Some("mixed".into()));
        let one = [
            usage_event("a", 1, None),
            usage_event("b", 2, Some("unreported".into())),
        ];
        assert_eq!(gap_of(&one), Some("unreported".into()));
        let superseded = [
            usage_event("a", 1, Some("interrupted".into())),
            usage_event("a", 2, None),
        ];
        assert_eq!(gap_of(&superseded), None, "only the latest total counts");
    }

    #[test]
    fn prices_parse_and_estimate() {
        let arg: PriceArg = "gpt-5.6-sol=1.25,0.125,10".parse().unwrap();
        assert_eq!(arg.model, "gpt-5.6-sol");
        let usage = Usage {
            input_tokens: 1_000_000,
            cached_input_tokens: 800_000,
            output_tokens: 100_000,
            ..Usage::default()
        };
        let dollars = arg.price.estimate(&usage);
        assert!((dollars - (0.25 + 0.1 + 1.0)).abs() < 1e-9, "{dollars}");
        assert!("m=1,2".parse::<PriceArg>().is_err());
        assert!("m=1,x,3".parse::<PriceArg>().is_err());
        assert!("m=1,-2,3".parse::<PriceArg>().is_err());
        assert!("1,2,3".parse::<PriceArg>().is_err());
    }
}
