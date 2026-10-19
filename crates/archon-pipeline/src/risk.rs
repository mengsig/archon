//! The risk level a delivery labels its pull request with (ARCHITECTURE
//! §8.7): the record stage proposes one with its evidence, a reviewer
//! confirms or changes it, and the delivery applies what they settled on.
//! Strictly advisory; nothing merges, gates or reviews by it.

mod section;

use archon_core::risk::RiskLevel;
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub use section::{DiffStat, Evidence, FileStat, section, with_section};

/// Screenshot links kept from one result.
const MAX_CAPTURES: usize = 10;
/// The longest evidence kept from one agent, in characters.
const MAX_WHY_CHARS: usize = 3_000;

/// One agent's call on a change's risk level, and why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RiskCall {
    pub level: RiskLevel,
    pub why: String,
}

impl RiskCall {
    /// The call `object` holds, `{"level": "low|medium|high", "why": "…"}`:
    /// `Ok(None)` without one, `Err` saying what is wrong with a bad one.
    pub fn read(object: Option<&Value>) -> Result<Option<Self>, String> {
        let Some(object) = object else {
            return Ok(None);
        };
        let Some(level) = object.get("level").and_then(Value::as_str) else {
            return Err("`risk.level` is missing".into());
        };
        let Some(level) = RiskLevel::parse(level) else {
            return Err(format!(
                "`risk.level` is {level:?}, not `low`, `medium` or `high`"
            ));
        };
        let why = object.get("why").and_then(Value::as_str).unwrap_or("");
        let why = crate::gate::clip(why.trim(), MAX_WHY_CHARS);
        if why.is_empty() {
            return Err(format!("`risk.why` is empty for level `{level}`"));
        }
        Ok(Some(Self { level, why }))
    }

    /// The call of a stage result's `risk` key, if it holds a valid one.
    pub fn of_result(structured: Option<&Value>) -> Option<Self> {
        Self::read(structured.and_then(|data| data.get("risk")))
            .ok()
            .flatten()
    }
}

/// What the record stage reported of its change's risk.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Proposal {
    /// Its own call; `Err` says why there is none.
    pub call: Result<RiskCall, String>,
    /// Its reviewer's call, which it relays as `risk.reviewer`.
    pub reviewer: Option<RiskCall>,
    /// `https://` links to screenshots of the change; no local path.
    pub captures: Vec<String>,
}

impl Proposal {
    /// The proposal on a record stage's `structured` result. A missing or bad
    /// call is kept as why it is missing: the delivery then labels high.
    pub fn read(structured: Option<&Value>) -> Self {
        let risk = structured.and_then(|data| data.get("risk"));
        let call = match RiskCall::read(risk) {
            Ok(Some(call)) => Ok(call),
            Ok(None) => Err("no level was proposed".to_owned()),
            Err(why) => Err(why),
        };
        let reviewer = risk.and_then(|risk| RiskCall::read(risk.get("reviewer")).ok().flatten());
        let captures = risk
            .and_then(|risk| risk.get("captures"))
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(str::trim)
            .filter(|url| is_web_link(url))
            .take(MAX_CAPTURES)
            .map(str::to_owned)
            .collect();
        Self {
            call,
            reviewer,
            captures,
        }
    }
}

/// Whether `url` is an `https://` link with nothing in it that could break
/// out of a Markdown image.
fn is_web_link(url: &str) -> bool {
    let unsafe_char = |c: char| c.is_whitespace() || matches!(c, '(' | ')' | '<' | '>' | '"');
    url.len() > "https://".len() && url.starts_with("https://") && !url.contains(unsafe_char)
}

/// The level a delivery applies, and how it was settled.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Assessment {
    pub level: RiskLevel,
    /// One line: who proposed the level, and whether a reviewer kept it.
    pub decided: String,
    /// The proposing agent's evidence, then a reviewer's reason for a change.
    pub why: String,
    pub captures: Vec<String>,
}

impl Assessment {
    /// Settle `proposal`, made by `stage`, with the latest reviewer's call:
    /// `reviewed` from a later review stage, else the one the proposal relays.
    /// A reviewer's call on a proposal stands; without a valid proposal, the
    /// level is high.
    pub fn settle(stage: &str, proposal: &Proposal, reviewed: Option<(&str, &RiskCall)>) -> Self {
        assert!(!stage.is_empty(), "a stage proposed it");
        let relayed = proposal
            .reviewer
            .as_ref()
            .map(|call| ("its reviewer", call));
        let reviewer = reviewed.or(relayed);
        let (level, decided, why) = match (&proposal.call, reviewer) {
            (Ok(own), None) => (
                own.level,
                format!(
                    "`{stage}` proposed `{}`; no reviewer judged the level.",
                    own.level
                ),
                own.why.clone(),
            ),
            (Ok(own), Some((by, call))) if call.level == own.level => (
                own.level,
                format!("`{stage}` proposed `{}`, and {by} confirmed it.", own.level),
                format!("{}\n\n{by}: {}", own.why, call.why),
            ),
            (Ok(own), Some((by, call))) => (
                call.level,
                format!(
                    "`{stage}` proposed `{}`; {by} changed it to `{}`.",
                    own.level, call.level
                ),
                format!("{}\n\n{by}, changing it: {}", own.why, call.why),
            ),
            // A reviewer only judges a proposal; without one the level stays high.
            (Err(missing), Some((by, call))) => (
                RiskLevel::High,
                format!(
                    "`{stage}` proposed no valid level ({missing}), so Archon applies `high`; \
                     {by} called it `{}`.",
                    call.level
                ),
                format!("{by}: {}", call.why),
            ),
            (Err(missing), None) => (
                RiskLevel::High,
                format!("`{stage}` proposed no valid level ({missing}), so Archon applies `high`."),
                "No agent gave evidence for a level; when in doubt, the higher level applies."
                    .to_owned(),
            ),
        };
        assert!(!why.trim().is_empty(), "every level says why");
        Self {
            level,
            decided,
            why,
            captures: proposal.captures.clone(),
        }
    }

    /// The level applied to a pull request whose risk label was `before`:
    /// `self` judged only the commits it added, so the higher one stands.
    pub fn onto(mut self, before: Option<RiskLevel>) -> Self {
        let Some(before) = before.filter(|before| *before > self.level) else {
            return self;
        };
        self.decided = format!(
            "{} The pull request was already labelled `{before}`, and these commits add to it, \
             so `{before}` stays.",
            self.decided
        );
        self.level = before;
        self
    }
}

/// The highest valid call among the `structured` results of one review
/// stage's members.
pub fn highest_call<'a>(results: impl IntoIterator<Item = Option<&'a Value>>) -> Option<RiskCall> {
    results
        .into_iter()
        .filter_map(RiskCall::of_result)
        .max_by_key(|call| call.level)
}

#[cfg(test)]
mod tests;
