mod previous;

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use archon_core::catalog::StageKind;
use archon_core::error::ArchonError;
use archon_core::id::StageRunId;
use archon_core::ports::Descendant;
use archon_core::run::{StageRun, Verdict as StageVerdict, summary_json};
use archon_pipeline::review::{ReviewFinding, Verdict};
use archon_queue::Store;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::coverage::{Ancestry, History};
use super::rounds::{load_from, same_commit};
use crate::worktree;
use crate::worktree::stack::is_commit_id;
pub(super) use previous::prior_findings;
pub(in crate::executor) use previous::prior_handoff;
use previous::{previous, update_previous};

#[derive(Debug, Serialize, Deserialize)]
pub(super) struct Report {
    pub agent: String,
    pub model: String,
    pub pass: Option<bool>,
    pub head: Option<String>,
    pub base: Option<String>,
    pub finding_count: Option<usize>,
    pub findings: Vec<ReviewFinding>,
    pub resolved_findings: Vec<String>,
}

impl Report {
    pub fn new(child: &Descendant, verdict: &Verdict) -> Self {
        assert!(!verdict.head.is_empty(), "a verdict identifies its head");
        assert_eq!(verdict.findings.unwrap_or(0), verdict.listed_findings.len());
        Self {
            agent: child.agent.to_string(),
            model: child.model.to_string(),
            pass: Some(verdict.pass),
            head: Some(verdict.head.clone()),
            base: verdict.base.clone(),
            finding_count: verdict.findings,
            findings: verdict.listed_findings.clone(),
            resolved_findings: verdict.resolved_findings.clone(),
        }
    }

    fn standalone(stage: &StageRun) -> Option<Self> {
        let agent = stage.aether_agent.as_ref()?.to_string();
        assert!(stage.attempt > 0);
        assert!(stage.state.is_terminal());
        let pass = match stage.verdict {
            Some(StageVerdict::Pass) => Some(true),
            Some(StageVerdict::Fail) => Some(false),
            _ => None,
        };
        let summary = stage.summary.as_deref().and_then(summary_json);
        let field = |key| {
            stage
                .structured
                .as_ref()
                .and_then(|s| s.get(key))
                .or_else(|| summary.as_ref().and_then(|s| s.get(key)))
        };
        let listed = stage
            .structured
            .as_ref()
            .and_then(ReviewFinding::listed)
            .or_else(|| summary.as_ref().and_then(ReviewFinding::listed));
        let model = stage.current_choice();
        Some(Self {
            agent,
            model: model.model.to_string(),
            pass,
            head: field("head")
                .and_then(Value::as_str)
                .filter(|h| super::is_commit_id(h))
                .map(str::to_owned),
            base: field("base")
                .and_then(Value::as_str)
                .filter(|b| super::is_commit_id(b))
                .map(str::to_owned),
            finding_count: listed.as_ref().map(Vec::len),
            findings: listed.unwrap_or_default(),
            resolved_findings: ReviewFinding::resolved(field("resolved_findings")),
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct Finding {
    pub id: String,
    pub description: String,
    pub blocking: bool,
    found_in: RoundId,
    resolved_in: Option<RoundId>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct RoundId {
    stage: String,
    number: usize,
}

#[derive(Debug, Default, Serialize)]
pub(super) struct Rounds<'a> {
    rounds: Vec<Round<'a>>,
    pub findings: BTreeMap<String, Finding>,
}

#[derive(Debug, Serialize)]
struct Round<'a> {
    number: usize,
    head: Option<&'a str>,
    reviews: Vec<Review<'a>>,
    #[serde(skip)]
    open: Vec<Finding>,
    #[serde(skip)]
    resolved: BTreeSet<String>,
}

#[derive(Debug, Serialize)]
struct Review<'a> {
    agent: &'a str,
    model: &'a str,
    verdict: Option<&'static str>,
    base: Option<&'a str>,
    finding_count: Option<usize>,
    finding_ids: Vec<String>,
    next_round_resolved: Option<bool>,
    #[serde(skip)]
    reported: &'a [ReviewFinding],
    #[serde(skip)]
    resolved: &'a [String],
}

/// One read as the gate weighs a blocker: its head and the IDs it listed and
/// resolved.
struct Read<'r> {
    head: String,
    listed: &'r [String],
    resolved: &'r [String],
}

impl<'a> Rounds<'a> {
    pub fn new(stage: &StageRun, reports: &'a [Report], prior: Vec<Finding>, first: usize) -> Self {
        let mut result = Self {
            rounds: Vec::new(),
            findings: prior.into_iter().map(|f| (f.id.clone(), f)).collect(),
        };
        let stage = stage.id.to_string();
        for reads in group(reports) {
            let mut round = Round {
                number: result.rounds.len() + first,
                head: reads[0].head.as_deref(),
                reviews: Vec::new(),
                open: Vec::new(),
                resolved: BTreeSet::new(),
            };
            let at = RoundId {
                stage: stage.clone(),
                number: round.number,
            };
            let (open, resolved) = round.record(&mut result.findings, reads.into_iter(), &at);
            for id in resolved.difference(&open) {
                let finding = result
                    .findings
                    .get_mut(id)
                    .expect("only known findings resolve");
                finding.resolved_in = Some(at.clone());
                round.resolved.insert(id.clone());
            }
            round.open = result
                .findings
                .values()
                .filter(|f| f.resolved_in.is_none())
                .cloned()
                .collect();
            result.rounds.push(round);
        }
        result.record_next_round();
        result
    }

    fn record_next_round(&mut self) {
        for index in 1..self.rounds.len() {
            let (before, after) = self.rounds.split_at_mut(index);
            let round = before.last_mut().expect("there is a preceding round");
            let resolved = &after[0].resolved;
            assert_eq!(round.number + 1, after[0].number);
            for review in &mut round.reviews {
                if review.finding_ids.is_empty() {
                    continue;
                }
                review.next_round_resolved =
                    Some(review.finding_ids.iter().all(|id| resolved.contains(id)));
            }
        }
    }

    pub fn blocking_at(&self, head: &str) -> Vec<(&str, &str)> {
        self.at_head(head)
            .map_or_else(Vec::new, |round| round.open_findings(true))
    }

    pub fn reported_blockers(&self, head: &str) -> Vec<(&str, &str)> {
        let Some(round) = self.at_head(head) else {
            return Vec::new();
        };
        round
            .reviews
            .iter()
            .flat_map(Review::findings)
            .filter(|(_, f)| f.blocking)
            .map(|(id, f)| (id, f.description.as_str()))
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect()
    }

    /// The blocking findings the gate still holds open. As coverage reads it,
    /// one is resolved only by a read on the branch whose head descends from
    /// every read on the branch that lists it, whatever order they came in.
    pub fn open_blockers_by_ancestry(
        &self,
        workspace: &Path,
    ) -> Result<Vec<(&str, &str)>, ArchonError> {
        let mut history = History::new(workspace);
        let reads = self.reads_on_branch(workspace, &mut history)?;
        let mut open = Vec::new();
        for finding in self.findings.values().filter(|f| f.blocking) {
            let listed = reads.iter().any(|read| read.listed.contains(&finding.id));
            if finding.resolved_in.is_some() && !listed {
                continue;
            }
            if !resolved_past_listings(&mut history, &reads, &finding.id)? {
                open.push((finding.id.as_str(), self.description(finding)));
            }
        }
        assert!(
            open.iter()
                .all(|(id, text)| !id.is_empty() && !text.is_empty())
        );
        Ok(open)
    }

    /// Every review whose head the branch holds, as a full commit ID, as
    /// `coverage::place` sees it; an amended-away or unknown head counts for none.
    fn reads_on_branch(
        &self,
        workspace: &Path,
        history: &mut impl Ancestry,
    ) -> Result<Vec<Read<'_>>, ArchonError> {
        let Some(branch) = worktree::commit_id(workspace, "HEAD")? else {
            return Ok(Vec::new());
        };
        let mut reads = Vec::new();
        for round in &self.rounds {
            let Some(named) = round.head else { continue };
            let Some(head) = worktree::commit_id(workspace, named)? else {
                continue;
            };
            if !history.holds(&head, &branch)? {
                continue;
            }
            reads.extend(round.reviews.iter().map(|review| Read {
                head: head.clone(),
                listed: &review.finding_ids,
                resolved: review.resolved,
            }));
        }
        assert!(reads.iter().all(|read| is_commit_id(&read.head)));
        Ok(reads)
    }

    /// The newest report's wording of `finding`, else its first.
    fn description<'s>(&'s self, finding: &'s Finding) -> &'s str {
        let latest = self
            .rounds
            .iter()
            .rev()
            .flat_map(|round| &round.reviews)
            .find_map(|review| {
                review
                    .findings()
                    .find(|(id, _)| *id == finding.id)
                    .map(|(_, f)| f.description.as_str())
            });
        latest.unwrap_or(&finding.description)
    }

    fn at_head(&self, head: &str) -> Option<&Round<'_>> {
        self.rounds
            .iter()
            .find(|r| r.head.is_some_and(|r| same_commit(r, head)))
    }
}

impl Review<'_> {
    fn findings(&self) -> impl Iterator<Item = (&str, &ReviewFinding)> {
        assert_eq!(self.finding_ids.len(), self.reported.len());
        self.finding_ids
            .iter()
            .map(String::as_str)
            .zip(self.reported)
    }
}

/// Whether a read resolving `id` has every read listing it as a strict
/// ancestor, as `coverage::reads_past_open_claims` demands.
fn resolved_past_listings(
    history: &mut impl Ancestry,
    reads: &[Read],
    id: &str,
) -> Result<bool, ArchonError> {
    assert!(!id.is_empty(), "a blocker has an ID");
    for resolution in reads
        .iter()
        .filter(|read| read.resolved.iter().any(|r| r == id))
    {
        if listings_precede(history, reads, &resolution.head, id)? {
            return Ok(true);
        }
    }
    Ok(false)
}

fn listings_precede(
    history: &mut impl Ancestry,
    reads: &[Read],
    head: &str,
    id: &str,
) -> Result<bool, ArchonError> {
    assert!(!head.is_empty(), "a resolving read names its head");
    for listing in reads
        .iter()
        .filter(|read| read.listed.iter().any(|l| l == id))
    {
        if listing.head == head || !history.holds(&listing.head, head)? {
            return Ok(false);
        }
    }
    Ok(true)
}

fn group(reports: &[Report]) -> Vec<Vec<&Report>> {
    let mut rounds: Vec<Vec<&Report>> = Vec::new();
    for report in reports {
        match rounds.iter_mut().find(|r| {
            r[0].head
                .as_ref()
                .zip(report.head.as_ref())
                .is_some_and(|(a, b)| same_commit(a, b))
        }) {
            Some(round) => round.push(report),
            None => rounds.push(vec![report]),
        };
    }
    assert_eq!(rounds.iter().map(Vec::len).sum::<usize>(), reports.len());
    rounds
}

impl<'a> Round<'a> {
    fn open_findings(&self, blocking_only: bool) -> Vec<(&str, &str)> {
        let mut unlisted: BTreeMap<&str, &str> = self
            .open
            .iter()
            .filter(|f| !blocking_only || f.blocking)
            .map(|f| (f.id.as_str(), f.description.as_str()))
            .collect();
        let mut findings: BTreeSet<_> = self
            .reviews
            .iter()
            .flat_map(Review::findings)
            .filter(|(id, _)| unlisted.contains_key(id))
            .map(|(id, f)| (id, f.description.as_str()))
            .collect();
        for (id, _) in &findings {
            unlisted.remove(id);
        }
        findings.extend(unlisted);
        assert!(
            findings
                .iter()
                .all(|(id, text)| !id.is_empty() && !text.is_empty())
        );
        findings.into_iter().collect()
    }

    fn record(
        &mut self,
        findings: &mut BTreeMap<String, Finding>,
        reports: impl Iterator<Item = &'a Report>,
        at: &RoundId,
    ) -> (BTreeSet<String>, BTreeSet<String>) {
        let mut open = BTreeSet::new();
        let mut resolved = BTreeSet::new();
        for report in reports {
            assert!(
                report
                    .head
                    .as_deref()
                    .zip(self.head)
                    .is_none_or(|(a, b)| same_commit(a, b))
            );
            let known_open = |f: &&Finding| f.resolved_in.is_none() && f.found_in != *at;
            resolved.extend(
                report
                    .pass
                    .into_iter()
                    .flat_map(|_| &report.resolved_findings)
                    .filter(|id| findings.get(*id).filter(known_open).is_some())
                    .cloned(),
            );
            let mut finding_ids = Vec::with_capacity(report.findings.len());
            for (index, finding) in report.findings.iter().enumerate() {
                let id = record_finding(findings, &report.agent, index, finding, at, &finding_ids);
                assert!(
                    !finding_ids.contains(&id),
                    "each finding in a report has its own ID"
                );
                finding_ids.push(id);
            }
            open.extend(finding_ids.iter().cloned());
            self.reviews.push(Review {
                agent: &report.agent,
                model: &report.model,
                verdict: report.pass.map(|p| if p { "pass" } else { "fail" }),
                base: report.base.as_deref(),
                finding_count: report.finding_count,
                finding_ids,
                next_round_resolved: None,
                reported: &report.findings,
                resolved: &report.resolved_findings,
            });
        }
        assert!(!self.reviews.is_empty(), "a round has at least one verdict");
        (open, resolved)
    }
}

fn record_finding(
    findings: &mut BTreeMap<String, Finding>,
    agent: &str,
    index: usize,
    finding: &ReviewFinding,
    at: &RoundId,
    seen: &[String],
) -> String {
    assert!(!agent.is_empty(), "the id names a reviewer");
    if let Some(prior) = finding.id.as_ref().and_then(|id| findings.get_mut(id))
        && prior.found_in != *at
        && !seen.contains(&prior.id)
    {
        prior.blocking |= finding.blocking;
        prior.resolved_in = None;
        return prior.id.clone();
    }
    let id = format!("{agent}:{}", index + 1);
    let prior = findings.insert(
        id.clone(),
        Finding {
            id: id.clone(),
            description: finding.description.clone(),
            blocking: finding.blocking,
            found_in: at.clone(),
            resolved_in: None,
        },
    );
    assert!(prior.is_none(), "a reporter uses its own id space once");
    id
}

pub(in crate::executor) fn attach(
    store: &Store,
    stage: &mut StageRun,
) -> Result<Option<(StageRunId, Value)>, ArchonError> {
    assert!(stage.state.is_terminal(), "the stage has a settled outcome");
    if let Some(object) = stage.structured.as_mut().and_then(Value::as_object_mut) {
        object.remove("review_rounds");
    }
    let tally = load_from(store, &stage.id)?;
    let own;
    let (reports, first) = if tally.reports.is_empty() {
        if stage.kind != Some(StageKind::Review) && stage.only_reviews != Some(true) {
            return Ok(None);
        }
        let Some(report) = Report::standalone(stage) else {
            return Ok(None);
        };
        own = [report];
        (own.as_slice(), stage.attempt as usize)
    } else {
        (tally.reports.as_slice(), 1)
    };
    let prior = previous(store, stage)?;
    let findings = prior.as_ref().map_or_else(Vec::new, |p| p.findings.clone());
    let rounds = Rounds::new(stage, reports, findings, first);
    let previous = prior.and_then(|p| update_previous(p, stage, &rounds));
    let mut structured = match stage.structured.take() {
        Some(Value::Object(object)) => object,
        Some(value) => serde_json::Map::from_iter([("result".into(), value)]),
        None => serde_json::Map::new(),
    };
    structured.insert("review_rounds".into(), serde_json::to_value(rounds)?);
    stage.structured = Some(Value::Object(structured));
    Ok(previous)
}
