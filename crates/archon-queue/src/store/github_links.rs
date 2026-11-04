//! The links between items and GitHub issues and pull requests (V20,
//! CONTRACTS.md §1.3). A write that adds or removes a link appends its `link`
//! change in the same transaction; a link already there changes nothing.

use archon_core::Timestamp;
use archon_core::error::ArchonError;
use archon_core::github::normalized_ref;
use archon_core::id::QueueItemId;
use archon_core::protocol::github_links::LinkSource;
use archon_core::protocol::{
    ChangeSubject, GithubKind, GithubLink, GithubRef, LinkRelation, PrRef,
};
use archon_core::pull_request::PullRequestRef;
use archon_core::queue::QueueItem;
use rusqlite::{Connection, OptionalExtension, params};
use serde::de::DeserializeOwned;

use super::Store;
use super::codec::{SqlContext, to_i64, to_u64, ts, write_tx};

const COLUMNS: &str = "item, host, repo, number, kind, relation, source, created_at";

/// Links an item's summary reads its issue from, most telling first.
const ISSUE_RELATIONS: [LinkRelation; 3] = [
    LinkRelation::Origin,
    LinkRelation::Imported,
    LinkRelation::Closes,
];

fn kind_name(kind: GithubKind) -> &'static str {
    match kind {
        GithubKind::Issue => "issue",
        GithubKind::PullRequest => "pull_request",
    }
}

fn relation_name(relation: LinkRelation) -> &'static str {
    match relation {
        LinkRelation::Origin => "origin",
        LinkRelation::Imported => "imported",
        LinkRelation::Delivered => "delivered",
        LinkRelation::Closes => "closes",
        LinkRelation::Mentioned => "mentioned",
    }
}

fn source_name(source: LinkSource) -> &'static str {
    match source {
        LinkSource::Watcher => "watcher",
        LinkSource::Operator => "operator",
        LinkSource::Import => "import",
        LinkSource::Ship => "ship",
        LinkSource::Backfill => "backfill",
    }
}

fn parse_name<T: DeserializeOwned>(name: String, what: &str) -> Result<T, ArchonError> {
    serde_json::from_value(serde_json::Value::String(name.clone()))
        .map_err(|_| ArchonError::Corrupt(format!("GitHub link {what} {name:?}")))
}

/// Add `link` inside the caller's write transaction. Returns whether it is
/// new: only a new link is a change.
pub(super) fn insert(conn: &Connection, link: &GithubLink) -> Result<bool, ArchonError> {
    assert_eq!(
        normalized_ref(&link.reference).as_ref().ok(),
        Some(&link.reference),
        "links are keyed by their normalized reference"
    );
    let reference = &link.reference;
    let added = conn
        .execute(
            &format!(
                "INSERT INTO github_links ({COLUMNS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                 ON CONFLICT DO NOTHING"
            ),
            params![
                link.item.as_str(),
                reference.host,
                reference.repo,
                to_i64(reference.number, "GitHub number")?,
                kind_name(reference.kind),
                relation_name(link.relation),
                source_name(link.source),
                ts(&link.created_at),
            ],
        )
        .ctx("insert GitHub link")?;
    assert!(added <= 1, "one link per key");
    if added == 1 {
        record_change(conn, &link.item, reference, link.relation)?;
    }
    Ok(added == 1)
}

/// Remove the `relation` link of `item` to `reference`, whatever recorded it.
/// Returns whether there was one: only a removed link is a change.
pub(super) fn delete(
    conn: &Connection,
    item: &QueueItemId,
    reference: &GithubRef,
    relation: LinkRelation,
) -> Result<bool, ArchonError> {
    let removed = conn
        .execute(
            "DELETE FROM github_links
             WHERE item = ?1 AND host = ?2 AND repo = ?3 AND number = ?4 AND relation = ?5",
            params![
                item.as_str(),
                reference.host,
                reference.repo,
                to_i64(reference.number, "GitHub number")?,
                relation_name(relation),
            ],
        )
        .ctx("delete GitHub link")?;
    assert!(removed <= 1, "one link per key");
    if removed == 1 {
        record_change(conn, item, reference, relation)?;
    }
    Ok(removed == 1)
}

fn record_change(
    conn: &Connection,
    item: &QueueItemId,
    reference: &GithubRef,
    relation: LinkRelation,
) -> Result<(), ArchonError> {
    let subject = ChangeSubject::Link {
        item: item.clone(),
        host: reference.host.clone(),
        repo: reference.repo.clone(),
        number: reference.number,
        relation,
    };
    super::changes::record(conn, &subject).map(drop)
}

/// The watcher path: a new item asked for on a GitHub thread links to it.
/// A thread no link can name, such as a number past `i64::MAX`, is refused.
pub(super) fn record_origin(conn: &Connection, item: &QueueItem) -> Result<(), ArchonError> {
    let Some(origin) = &item.origin else {
        return Ok(());
    };
    let link = GithubLink {
        item: item.id.clone(),
        reference: normalized_ref(&origin.reference())?,
        relation: LinkRelation::Origin,
        source: LinkSource::Watcher,
        created_at: item.created_at,
    };
    insert(conn, &link).map(drop)
}

/// The ship path: `item`'s run now delivers to `now` instead of `before`.
/// Run after `run_pull_requests` is written, so `before` stays linked while
/// another run of the item still delivers to it.
pub(super) fn record_delivery(
    conn: &Connection,
    item: &QueueItemId,
    before: Option<&PullRequestRef>,
    now: &PullRequestRef,
) -> Result<(), ArchonError> {
    let link = GithubLink {
        item: item.clone(),
        reference: GithubRef::from(now),
        relation: LinkRelation::Delivered,
        source: LinkSource::Ship,
        created_at: archon_core::now(),
    };
    insert(conn, &link)?;
    let Some(before) = before.filter(|before| *before != now) else {
        return Ok(());
    };
    let still_delivered: bool = conn
        .query_row(
            "SELECT EXISTS (SELECT 1 FROM run_pull_requests p JOIN runs r ON r.id = p.run
                 WHERE r.item = ?1 AND p.host = ?2 AND p.repository = ?3 AND p.number = ?4)",
            params![
                item.as_str(),
                before.host,
                before.repository,
                to_i64(before.number, "pull request number")?
            ],
            |row| row.get(0),
        )
        .ctx("look for another delivery to a pull request")?;
    if !still_delivered {
        delete(
            conn,
            item,
            &GithubRef::from(before),
            LinkRelation::Delivered,
        )?;
    }
    Ok(())
}

/// Retention is about to delete the items in `temp.prune_items`, and their
/// links with them by cascade: each link's removal is a change.
pub(super) fn record_pruned(conn: &Connection) -> Result<usize, ArchonError> {
    let filter = "item IN (SELECT id FROM temp.prune_items)";
    let links = query_links(conn, filter, params![])?;
    for link in &links {
        record_change(conn, &link.item, &link.reference, link.relation)?;
    }
    Ok(links.len())
}

type LinkRow = (String, String, String, i64, String, String, String, String);

fn read_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<LinkRow> {
    Ok((
        row.get(0)?,
        row.get(1)?,
        row.get(2)?,
        row.get(3)?,
        row.get(4)?,
        row.get(5)?,
        row.get(6)?,
        row.get(7)?,
    ))
}

fn parse_row(row: LinkRow) -> Result<GithubLink, ArchonError> {
    let (item, host, repo, number, kind, relation, source, created_at) = row;
    let created_at: Timestamp = chrono::DateTime::parse_from_rfc3339(&created_at)
        .map(|at| at.with_timezone(&chrono::Utc))
        .map_err(|e| ArchonError::Corrupt(format!("GitHub link time {created_at:?}: {e}")))?;
    let link = GithubLink {
        item: QueueItemId::parse(&item)?,
        reference: GithubRef {
            host,
            repo,
            number: to_u64(number, "GitHub number")?,
            kind: parse_name(kind, "kind")?,
        },
        relation: parse_name(relation, "relation")?,
        source: parse_name(source, "source")?,
        created_at,
    };
    if normalized_ref(&link.reference).ok().as_ref() != Some(&link.reference) {
        return Err(ArchonError::Corrupt(format!(
            "item {} links to a malformed GitHub reference {:?}",
            link.item, link.reference
        )));
    }
    Ok(link)
}

fn query_links(
    conn: &Connection,
    filter: &str,
    args: impl rusqlite::Params,
) -> Result<Vec<GithubLink>, ArchonError> {
    let sql = format!(
        "SELECT {COLUMNS} FROM github_links WHERE {filter}
         ORDER BY created_at, item, relation, host, repo, number"
    );
    let mut stmt = conn.prepare_cached(&sql).ctx("prepare GitHub links")?;
    let rows = stmt
        .query_map(args, read_row)
        .ctx("read GitHub links")?
        .collect::<Result<Vec<_>, _>>()
        .ctx("read GitHub links")?;
    rows.into_iter().map(parse_row).collect()
}

fn item_exists(conn: &Connection, item: &QueueItemId) -> Result<bool, ArchonError> {
    conn.query_row("SELECT 1 FROM items WHERE id = ?1", [item.as_str()], |_| {
        Ok(())
    })
    .optional()
    .ctx("look for an item")
    .map(|found| found.is_some())
}

/// The summary's issue and pull request among `links`, one item's in their
/// stored order: the most telling issue, and the newest delivered pull request.
fn summary_refs(links: &[GithubLink]) -> (Option<GithubRef>, Option<PrRef>) {
    let issue = ISSUE_RELATIONS.iter().find_map(|relation| {
        links
            .iter()
            .find(|l| l.relation == *relation && l.reference.kind == GithubKind::Issue)
            .map(|l| l.reference.clone())
    });
    let pr = links
        .iter()
        .rev()
        .find(|l| {
            l.relation == LinkRelation::Delivered && l.reference.kind == GithubKind::PullRequest
        })
        .map(|l| PrRef {
            reference: l.reference.clone(),
            state: None,
            review_decision: None,
            checks: None,
        });
    (issue, pr)
}

impl Store {
    /// Link an operator's or an import's `link`; the same link twice is one.
    /// Returns whether it is new.
    pub fn link_github(&mut self, link: &GithubLink) -> Result<bool, ArchonError> {
        let link = GithubLink {
            reference: normalized_ref(&link.reference)?,
            ..link.clone()
        };
        let tx = write_tx(&mut self.conn)?;
        if !item_exists(&tx, &link.item)? {
            return Err(ArchonError::NotFound(format!("queue item {}", link.item)));
        }
        let added = insert(&tx, &link)?;
        tx.commit().ctx("commit GitHub link")?;
        Ok(added)
    }

    /// Remove `item`'s `relation` link to `reference`; one not there is no
    /// error. Returns whether there was one.
    pub fn unlink_github(
        &mut self,
        item: &QueueItemId,
        reference: &GithubRef,
        relation: LinkRelation,
    ) -> Result<bool, ArchonError> {
        let reference = normalized_ref(reference)?;
        let tx = write_tx(&mut self.conn)?;
        if !item_exists(&tx, item)? {
            return Err(ArchonError::NotFound(format!("queue item {item}")));
        }
        let removed = delete(&tx, item, &reference, relation)?;
        tx.commit().ctx("commit GitHub unlink")?;
        Ok(removed)
    }

    /// Every link of `item`, oldest first.
    pub fn item_links(&self, item: &QueueItemId) -> Result<Vec<GithubLink>, ArchonError> {
        if !item_exists(&self.conn, item)? {
            return Err(ArchonError::NotFound(format!("queue item {item}")));
        }
        let links = query_links(&self.conn, "item = ?1", [item.as_str()])?;
        assert!(links.iter().all(|l| &l.item == item), "the item's links");
        Ok(links)
    }

    /// Every item's links to the issue or pull request `reference` names,
    /// whichever kind it was linked as: one number names one of them.
    pub fn issue_items(&self, reference: &GithubRef) -> Result<Vec<GithubLink>, ArchonError> {
        let reference = normalized_ref(reference)?;
        let number = to_i64(reference.number, "GitHub number")?;
        let filter = "host = ?1 AND repo = ?2 AND number = ?3";
        let links = query_links(
            &self.conn,
            filter,
            params![reference.host, reference.repo, number],
        )?;
        assert!(
            links.iter().all(|l| l.reference.number == reference.number),
            "links to the one number"
        );
        Ok(links)
    }

    /// The issue an item came from or closes and the pull request it
    /// delivered, as its summary shows them.
    pub fn item_github_refs(
        &self,
        item: &QueueItemId,
    ) -> Result<(Option<GithubRef>, Option<PrRef>), ArchonError> {
        let links = query_links(&self.conn, "item = ?1", [item.as_str()])?;
        Ok(summary_refs(&links))
    }
}
