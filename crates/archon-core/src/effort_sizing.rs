//! The deterministic effort sizer (design 7.6, A11). Pure functions over
//! what an item already records: no clock, no store, no learned evidence.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::autonomy::ReviewLevel;
use crate::catalog::StageKind;
use crate::model::{Effort, ModelRef};
use crate::outcome::DiffSize;

/// How risky an item's work is, settled before any effort is chosen.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Serialize,
    Deserialize,
    schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum RiskClass {
    Mechanical,
    Ordinary,
    Elevated,
    Critical,
}

impl RiskClass {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Mechanical => "mechanical",
            Self::Ordinary => "ordinary",
            Self::Elevated => "elevated",
            Self::Critical => "critical",
        }
    }
}

impl fmt::Display for RiskClass {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What an item records about its own work: the classifier's only input.
#[derive(Debug, Clone, Copy)]
pub struct RiskInput<'a> {
    pub title: &'a str,
    pub brief: &'a str,
    pub tags: &'a [String],
    pub review: ReviewLevel,
    pub size: Option<DiffSize>,
}

/// An item's class and the signals that produced it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ItemRisk {
    pub class: RiskClass,
    pub signals: Vec<String>,
}

/// The tag that makes work critical whatever else it says.
pub const CRITICAL_TAG: &str = "risk:critical";

/// The signal an ordinary item of a small diff carries.
const SMALL_SIGNAL: &str = "size:small";

/// Word stems naming a risk domain: security, concurrency or locking, data
/// migration. A stem matches the start of a whole word of the title or brief.
const DOMAIN_STEMS: [(&str, &str); 13] = [
    ("secur", "security"),
    ("vulnerab", "security"),
    ("authenticat", "security"),
    ("authoriz", "security"),
    ("credential", "security"),
    ("encrypt", "security"),
    ("permission", "security"),
    ("secret", "security"),
    ("concurren", "concurrency"),
    ("deadlock", "concurrency"),
    ("mutex", "concurrency"),
    ("locking", "concurrency"),
    ("migrat", "migration"),
];

/// Tags and title words that make work mechanical when nothing else is risky.
const MECHANICAL_WORDS: [&str; 9] = [
    "docs",
    "documentation",
    "changelog",
    "upmerge",
    "lint",
    "typo",
    "formatting",
    "mechanical",
    "version-bump",
];

/// Title prefixes of mechanical work, as Conventional Commits writes them.
const MECHANICAL_PREFIXES: [&str; 5] = ["docs:", "docs(", "style:", "chore(release)", "chore(deps)"];

/// Tags and title words naming release, CI or public-API work.
const ELEVATED_WORDS: [&str; 5] = ["release", "ci", "public-api", "api", "workflow"];

fn words(text: &str) -> impl Iterator<Item = String> + '_ {
    let is_word = |c: char| c.is_alphanumeric() || c == '-';
    text.split(move |c: char| !is_word(c))
        .filter(|w| !w.is_empty())
        .map(str::to_lowercase)
}

/// The risk domain a lowercase `word` names. "race" counts only as the word
/// itself, so "trace" and "raced" name none.
fn domain_of(word: &str) -> Option<&'static str> {
    if matches!(word, "race" | "races") {
        return Some("concurrency");
    }
    let hit = DOMAIN_STEMS.iter().find(|(stem, _)| word.starts_with(stem));
    hit.map(|(_, domain)| *domain)
}

/// The domains the title and the brief name, each once, in a fixed order.
fn domains(input: &RiskInput<'_>) -> Vec<&'static str> {
    let mut found: Vec<&'static str> = Vec::new();
    for word in words(input.title).chain(words(input.brief)) {
        if let Some(domain) = domain_of(&word)
            && !found.contains(&domain)
        {
            found.push(domain);
        }
    }
    found.sort_unstable();
    found
}

/// Whether the title says the work is mechanical: a `docs:` or `chore(deps)`
/// style prefix, a version bump, or a mechanical word.
fn mechanical_title(title: &str) -> bool {
    let lower = title.to_lowercase();
    MECHANICAL_PREFIXES.iter().any(|p| lower.starts_with(p))
        || lower.contains("version bump")
        || lower.contains("bump version")
        || words(title).any(|w| MECHANICAL_WORDS.contains(&w.as_str()))
}

/// The diff size an item's tags name: `diff:small`, `diff:medium` or `diff:large`.
pub fn tagged_size(tags: &[String]) -> Option<DiffSize> {
    tags.iter().find_map(|tag| match tag.as_str() {
        "diff:small" => Some(DiffSize::Small),
        "diff:medium" => Some(DiffSize::Medium),
        "diff:large" => Some(DiffSize::Large),
        _ => None,
    })
}

/// The item's risk class from its recorded signals (7.6).
pub fn classify(input: RiskInput<'_>) -> ItemRisk {
    let tags: Vec<String> = input.tags.iter().map(|t| t.to_lowercase()).collect();
    let tagged = |set: &[&str]| tags.iter().find(|t| set.contains(&t.as_str())).cloned();
    let found = domains(&input);
    let mut signals: Vec<String> = found.iter().map(|d| format!("domain:{d}")).collect();
    let strict = input.review == ReviewLevel::Strict;
    let large = input.size == Some(DiffSize::Large);
    if strict {
        signals.push("review:strict".to_owned());
    }
    if large {
        signals.push("size:large".to_owned());
    }
    // A version bump's title names its release; only a tag makes it release work.
    let mechanical_named = mechanical_title(input.title);
    let titled = || words(input.title).find(|w| ELEVATED_WORDS.contains(&w.as_str()));
    let scope = tagged(&ELEVATED_WORDS).or_else(|| titled().filter(|_| !mechanical_named));
    signals.extend(scope.iter().map(|word| format!("scope:{word}")));
    let tagged_critical = tags.iter().any(|t| t == CRITICAL_TAG);
    if tagged_critical {
        signals.push(CRITICAL_TAG.to_owned());
    }
    let domain = !found.is_empty();
    let class = if tagged_critical || (domain && (strict || large)) {
        RiskClass::Critical
    } else if domain || strict || large || scope.is_some() {
        RiskClass::Elevated
    } else if mechanical_named || tagged(&MECHANICAL_WORDS).is_some() {
        signals.push("mechanical".to_owned());
        RiskClass::Mechanical
    } else {
        RiskClass::Ordinary
    };
    if class == RiskClass::Ordinary && input.size == Some(DiffSize::Small) {
        signals.push(SMALL_SIGNAL.to_owned());
    }
    assert!(
        class == RiskClass::Ordinary || !signals.is_empty(),
        "every class but ordinary names the signal that made it"
    );
    ItemRisk { class, signals }
}

/// The stage facts one plan key is sized from: its own, never another key's.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct StageFacts {
    /// The stage's own declared default effort for this key.
    pub base: Effort,
    /// Whether the key's agents check work: a review or test stage, or reviewers.
    #[serde(default)]
    pub checks: bool,
}

impl StageFacts {
    pub fn new(base: Effort, kind: StageKind, reviewers: bool) -> Self {
        let checks = reviewers || matches!(kind, StageKind::Review | StageKind::Test);
        Self { base, checks }
    }
}

/// The highest effort the sizer may set: the lower of the policy's cap and
/// `[models] sizer_max_effort`.
pub fn sizer_cap(policy_cap: Option<Effort>, sizer_max: Effort) -> Effort {
    let cap = policy_cap.map_or(sizer_max, |policy| policy.min(sizer_max));
    assert!(cap <= sizer_max, "the sizer never exceeds its own setting");
    cap
}

/// Whether `model`'s engine honours `x_high`: Claude Code and Codex do; the
/// API-dialect adapters run `high` instead.
pub fn supports_x_high(model: &ModelRef) -> bool {
    matches!(model.provider.as_str(), "claude-code" | "codex")
}

/// The effort one step below `effort`; `low` stays `low`.
fn step_down(effort: Effort) -> Effort {
    match effort {
        Effort::Low | Effort::Medium => Effort::Low,
        Effort::High => Effort::Medium,
        Effort::XHigh => Effort::High,
        Effort::Max => Effort::XHigh,
    }
}

/// What the class asks of a stage before any cap: the class floor, then the
/// size step and the mechanical class, then the stage default.
fn class_target(facts: StageFacts, risk: &ItemRisk, review: ReviewLevel) -> Effort {
    let small = risk.signals.iter().any(|s| s == SMALL_SIGNAL);
    match risk.class {
        RiskClass::Mechanical if facts.checks && review == ReviewLevel::Strict => Effort::Medium,
        RiskClass::Mechanical => Effort::Low,
        RiskClass::Ordinary if small => step_down(facts.base),
        RiskClass::Ordinary => facts.base,
        RiskClass::Elevated => facts.base.max(Effort::High),
        RiskClass::Critical => Effort::XHigh,
    }
}

/// An effort the sizer chose, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SizedEffort {
    pub effort: Effort,
    /// `sizer: critical -> x_high`, with what lowered it when something did.
    pub reason: String,
}

/// The effort a stage of an item of class `risk` gets on `model`, within
/// `cap`: `x_high` only where the engine honours it.
pub fn size_effort(
    facts: StageFacts,
    (risk, review): (&ItemRisk, ReviewLevel),
    cap: Effort,
    model: &ModelRef,
) -> SizedEffort {
    let target = class_target(facts, risk, review);
    let small = risk.class == RiskClass::Ordinary && target < facts.base;
    let mut reason = format!("sizer: {} -> {target}", risk.class);
    if small {
        reason.push_str(" (small)");
    }
    let mut effort = target;
    if effort > cap {
        effort = cap;
        reason.push_str(&format!("; capped at {cap}; consider a stronger tier"));
    }
    if effort == Effort::XHigh && !supports_x_high(model) {
        effort = Effort::High;
        let engine = &model.provider;
        reason.push_str(&format!("; {engine} runs no more than high, so high"));
    }
    assert!(effort <= cap, "no sized effort exceeds the cap");
    assert!(effort <= target, "a cap or an engine only lowers the target");
    SizedEffort { effort, reason }
}

#[cfg(test)]
#[path = "effort_sizing/tests.rs"]
mod tests;
