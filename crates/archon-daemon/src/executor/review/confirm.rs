use std::collections::BTreeSet;

use archon_core::aether_ids::AetherAgentId;
use archon_core::delegation::REVIEWER_ROLE;
use archon_core::error::ArchonError;
use archon_core::ports::{Descendant, DescendantState};
use archon_core::run::{PipelineRun, StageRun};
use serde::{Deserialize, Serialize};

use super::rounds::same_commit;
use super::{handoff, record::Report};
use crate::{context::DaemonContext, worktree};

#[derive(Debug, Serialize, Deserialize)]
pub(super) struct Confirmation {
    pub base: String,
    pub head: String,
    findings: Vec<(String, String)>,
    existing: Option<BTreeSet<String>>,
}

pub(super) enum Status {
    Waiting,
    WaitingForPeers,
    Ready,
    Passed,
    Refused(&'static str),
}

pub(super) enum Eligibility {
    NoBranch,
    AwaitingFixes,
    Ready(Confirmation),
}

impl Confirmation {
    pub async fn eligible(
        run: PipelineRun,
        failed_head: &str,
        findings: &[(&str, &str)],
    ) -> Result<Eligibility, ArchonError> {
        assert!(!findings.is_empty());
        assert!(!failed_head.is_empty());
        let failed = failed_head.to_owned();
        let fixes = tokio::task::spawn_blocking(move || committed_fixes(&run, &failed))
            .await
            .map_err(|e| {
                ArchonError::io("check committed review fixes", std::io::Error::other(e))
            })??;
        let Some((base, head, fixes)) = fixes else {
            return Ok(Eligibility::NoBranch);
        };
        if findings.iter().any(|(id, _)| !fixes.contains(*id)) {
            return Ok(Eligibility::AwaitingFixes);
        }
        assert_ne!(base, head, "trailer evidence requires a later commit");
        let findings = findings
            .iter()
            .map(|(id, text)| ((*id).to_owned(), (*text).to_owned()))
            .collect();
        Ok(Eligibility::Ready(Self {
            base,
            head,
            findings,
            existing: None,
        }))
    }

    pub fn status(
        &self,
        lead: &AetherAgentId,
        beneath: &[Descendant],
        reports: &[Report],
        capped: &[(&str, &str)],
    ) -> Status {
        assert_ne!(
            self.base, self.head,
            "confirmation reads the committed fixes"
        );
        assert!(
            !self.findings.is_empty(),
            "confirmation has findings to verify"
        );
        if capped
            .iter()
            .any(|(id, _)| !self.findings.iter().any(|(known, _)| known == id))
        {
            return Status::Refused("another blocker was reported at the capped head");
        }
        let mut direct = reviewers(lead, beneath).peekable();
        if direct.peek().is_none() {
            return Status::Refused("Aether no longer lists the capped review's reviewers");
        }
        let Some(existing) = &self.existing else {
            return if direct.any(|d| d.state == DescendantState::Live) {
                Status::WaitingForPeers
            } else {
                Status::Ready
            };
        };
        let mut new = direct.filter(|d| !existing.contains(d.agent.as_str()));
        let Some(reviewer) = new.next() else {
            return Status::Waiting;
        };
        if new.next().is_some() {
            return Status::Refused("more than one reviewer followed the capped round");
        }
        match reviewer.state {
            DescendantState::Live => Status::Waiting,
            DescendantState::Stopped => Status::Refused("the confirmation reviewer stopped"),
            DescendantState::Completed => self.completed(reports, &reviewer.agent),
        }
    }

    pub fn prepare_delivery(
        &mut self,
        lead: &AetherAgentId,
        beneath: &[Descendant],
        capped: &[(&str, &str)],
    ) {
        assert!(self.existing.is_none(), "confirmation is instructed once");
        let existing: BTreeSet<_> = reviewers(lead, beneath)
            .map(|d| d.agent.to_string())
            .collect();
        assert!(
            !existing.is_empty(),
            "the capped round already had reviewers"
        );
        self.existing = Some(existing);
        self.findings.extend(
            capped
                .iter()
                .map(|(id, text)| ((*id).to_owned(), (*text).to_owned())),
        );
        self.findings.sort_unstable();
        self.findings.dedup();
    }

    fn completed(&self, reports: &[Report], agent: &AetherAgentId) -> Status {
        let Some(report) = reports.iter().find(|r| r.agent == agent.as_str()) else {
            return Status::Refused("the confirmation reviewer returned no readable verdict");
        };
        let right_head = report
            .head
            .as_deref()
            .is_some_and(|head| same_commit(head, &self.head));
        let right_base = report
            .base
            .as_deref()
            .is_some_and(|base| same_commit(base, &self.base));
        if !right_head || !right_base {
            return Status::Refused("the confirmation reviewer read a different revision or delta");
        }
        if report.pass != Some(true)
            || self.findings.iter().any(|(id, _)| {
                !report.resolved_findings.contains(id)
                    || report.findings.iter().any(|f| f.id.as_ref() == Some(id))
            })
        {
            return Status::Refused(
                "the confirmation reviewer did not pass and resolve every capped finding",
            );
        }
        Status::Passed
    }

    pub async fn tell(
        &self,
        ctx: &DaemonContext,
        stage: &StageRun,
        lead: &AetherAgentId,
    ) -> Result<bool, ArchonError> {
        assert!(
            self.existing.is_some(),
            "the confirmation scope is frozen before delivery"
        );
        let findings: Vec<_> = self
            .findings
            .iter()
            .map(|(id, text)| (id.as_str(), text.as_str()))
            .collect();
        let workspace = ctx.store.lock().await.run(&stage.run)?.workspace;
        let findings = handoff::findings(&workspace, &findings)?;
        let note = format!(
            concat!(
                "The review reached the cap with fixes committed. Have exactly one reviewer read ",
                "the fixes from base `{}` at head `{}`. Confirm each ID below as resolved in ",
                "`resolved_findings` or still open by including its `id` in `findings`. Review the ",
                "fix delta for regressions. Any real blocker you find anywhere fails. ",
                "Use the usual structured verdict and record the reviewer's result.\n\n{}",
            ),
            self.base, self.head, findings
        );
        super::rounds::tell(
            ctx,
            lead,
            &note,
            &format!("archon:review-confirm:{}", stage.id),
        )
        .await
    }
}

pub(super) fn reviewers<'a>(
    lead: &'a AetherAgentId,
    beneath: &'a [Descendant],
) -> impl Iterator<Item = &'a Descendant> {
    beneath
        .iter()
        .filter(move |d| &d.parent == lead && d.role.eq_ignore_ascii_case(REVIEWER_ROLE))
}

fn committed_fixes(
    run: &PipelineRun,
    failed: &str,
) -> Result<Option<(String, String, BTreeSet<String>)>, ArchonError> {
    let path = &run.workspace;
    if run.branch.is_empty()
        || !path.join(".git").exists()
        || !worktree::head(path)?.is_branch(&run.branch)
    {
        return Ok(None);
    }
    let Some(base) = worktree::commit_id(path, failed)? else {
        return Ok(None);
    };
    let Some(head) = worktree::commit_id(path, "HEAD")? else {
        return Ok(None);
    };
    if base != head && !worktree::is_ancestor(path, &base, &head)? {
        return Ok(None);
    }
    if base == head
        || worktree::stack::tree_of(path, &head)? == worktree::stack::tree_of(path, &base)?
    {
        return Ok(Some((base, head, BTreeSet::new())));
    }
    assert!(
        head.len() == base.len(),
        "commits use the same hash algorithm"
    );
    let range = format!("{base}..{head}");
    let trailers = worktree::git(
        path,
        &[
            "log",
            "--no-merges",
            "--ancestry-path",
            "--format=%(trailers:key=Review-Fix,valueonly,unfold,separator=%x0a)",
            &range,
            "--",
        ],
    )?;
    let fixes = trailers
        .lines()
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .map(str::to_owned)
        .collect();
    Ok(Some((base, head, fixes)))
}
