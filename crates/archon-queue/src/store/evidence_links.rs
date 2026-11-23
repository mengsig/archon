//! Bounded candidate items for downstream evidence.

use std::collections::{HashMap, HashSet};

use archon_core::{
    ArchonError, Timestamp,
    cost::DayRange,
    id::QueueItemId,
    pull_request::{OntoTarget, PullRequestRef},
    queue::QueueItem,
    run::{StageRun, StageState, Verdict},
};
use chrono::Days;
use rusqlite::{Connection, params};

use super::codec::SqlContext;

pub(super) struct Links<'a> {
    by_item: HashMap<&'a QueueItemId, Vec<&'a QueueItem>>,
    by_pr: HashMap<&'a PullRequestRef, Vec<&'a QueueItem>>,
    by_branch: HashMap<&'a str, Vec<&'a QueueItem>>,
    failed_reviews: HashMap<&'a QueueItemId, Vec<Timestamp>>,
}

impl<'a> Links<'a> {
    pub(super) fn new(items: &'a [QueueItem], reviews: &'a [(QueueItemId, StageRun)]) -> Self {
        let mut links = Self {
            by_item: HashMap::new(),
            by_pr: HashMap::new(),
            by_branch: HashMap::new(),
            failed_reviews: HashMap::new(),
        };
        for item in items {
            for dependency in &item.dependencies {
                links.by_item.entry(dependency).or_default().push(item);
            }
            if let Some(onto) = &item.onto {
                if let OntoTarget::Item(target) = &onto.target {
                    links.by_item.entry(target).or_default().push(item);
                }
                links.by_pr.entry(&onto.pr).or_default().push(item);
                links
                    .by_branch
                    .entry(&onto.head_ref)
                    .or_default()
                    .push(item);
            }
        }
        for (item, stage) in reviews {
            if stage.state == StageState::Completed
                && (stage.kind == Some(archon_core::catalog::StageKind::Review)
                    || stage.only_reviews == Some(true))
                && stage
                    .verdict_reading()
                    .is_some_and(|(verdict, _)| verdict == Verdict::Fail)
                && let Some(finished) = stage.finished_at
            {
                links.failed_reviews.entry(item).or_default().push(finished);
            }
        }
        links
    }

    pub(super) fn related<'b>(
        &'b self,
        item: &QueueItemId,
        pr: Option<&PullRequestRef>,
        branch: &str,
    ) -> impl Iterator<Item = &'a QueueItem> + 'b {
        self.by_item
            .get(item)
            .into_iter()
            .flatten()
            .copied()
            .chain(
                pr.and_then(|pr| self.by_pr.get(pr))
                    .into_iter()
                    .flatten()
                    .copied(),
            )
            .chain(
                (!branch.is_empty())
                    .then(|| self.by_branch.get(branch))
                    .flatten()
                    .into_iter()
                    .flatten()
                    .copied(),
            )
    }

    pub(super) fn add(
        &self,
        owner: &QueueItem,
        branch: &str,
        pr: Option<&PullRequestRef>,
        started: Timestamp,
        fixups: &mut HashSet<QueueItemId>,
        reverts: &mut HashSet<QueueItemId>,
    ) {
        let branch = if owner.repo.is_some() { branch } else { "" };
        for item in self.related(&owner.id, pr, branch) {
            if item.id == owner.id || item.repo != owner.repo || item.created_at <= started {
                continue;
            }
            if item.onto.as_ref().is_some_and(|onto| onto.review_only) {
                continue;
            }
            if is_revert(item) {
                reverts.insert(item.id.clone());
            } else if self.is_fixup(item, owner, branch, pr, started) {
                fixups.insert(item.id.clone());
            }
        }
    }

    fn is_fixup(
        &self,
        item: &QueueItem,
        owner: &QueueItem,
        branch: &str,
        pr: Option<&PullRequestRef>,
        started: Timestamp,
    ) -> bool {
        let same_target = item.onto.as_ref().is_some_and(|onto| {
            matches!(&onto.target, OntoTarget::Item(id) if id == &owner.id)
                || pr == Some(&onto.pr)
                || (owner.repo.is_some() && !branch.is_empty() && onto.head_ref == branch)
        });
        let after_failed_review = item.dependencies.contains(&owner.id)
            && self.failed_reviews.get(&owner.id).is_some_and(|times| {
                times
                    .iter()
                    .any(|time| *time >= started && *time < item.created_at)
            });
        same_target || after_failed_review
    }
}

fn is_revert(item: &QueueItem) -> bool {
    item.title
        .get(..6)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("revert"))
        || item
            .tags
            .iter()
            .any(|tag| tag.eq_ignore_ascii_case("revert"))
}

pub(super) fn read_reviews(
    conn: &Connection,
    range: &DayRange,
) -> Result<Vec<(String, String)>, ArchonError> {
    let (start, end) = utc_bounds(range)?;
    let mut statement = conn.prepare("SELECT r.item, st.data FROM stage_runs st INDEXED BY stage_runs_by_started_at JOIN runs r ON r.id = st.run WHERE st.started_at >= ?1 AND st.started_at < ?2 AND date(st.started_at, 'localtime') BETWEEN ?3 AND ?4 AND (json_extract(st.data, '$.kind') = 'review' OR json_extract(st.data, '$.only_reviews') = 1)")
        .ctx("prepare evidence review snapshot")?;
    let mut rows = statement
        .query(params![
            start,
            end,
            range.since.to_string(),
            range.until.to_string()
        ])
        .ctx("query evidence review snapshot")?;
    let mut reviews = Vec::new();
    while let Some(row) = rows.next().ctx("read evidence review snapshot")? {
        let item: String = row.get(0).ctx("read evidence review item")?;
        let data: String = row.get(1).ctx("read evidence review stage")?;
        reviews.push((item, data));
    }
    Ok(reviews)
}

pub(super) fn read(conn: &Connection, range: &DayRange) -> Result<Vec<String>, ArchonError> {
    let (start, end) = utc_bounds(range)?;
    let mut statement = conn
        .prepare("SELECT data FROM items WHERE created_at >= ?1 AND created_at < ?2 AND date(created_at, 'localtime') BETWEEN ?3 AND ?4")
        .ctx("prepare evidence candidate items")?;
    let mut rows = statement
        .query(params![
            start,
            end,
            range.since.to_string(),
            range.until.to_string()
        ])
        .ctx("query evidence candidate items")?;
    let mut items = Vec::new();
    while let Some(row) = rows.next().ctx("read evidence candidate item")? {
        items.push(row.get(0).ctx("read evidence candidate data")?);
    }
    Ok(items)
}

pub(super) fn utc_bounds(range: &DayRange) -> Result<(String, String), ArchonError> {
    if range.since > range.until {
        return Err(ArchonError::Invalid(
            "evidence range starts after it ends".into(),
        ));
    }
    let start = range
        .since
        .checked_sub_days(Days::new(1))
        .ok_or_else(|| ArchonError::Invalid("evidence range is too early".into()))?;
    let end = range
        .until
        .checked_add_days(Days::new(2))
        .ok_or_else(|| ArchonError::Invalid("evidence range is too late".into()))?;
    Ok((format!("{start}T00:00:00"), format!("{end}T00:00:00")))
}
