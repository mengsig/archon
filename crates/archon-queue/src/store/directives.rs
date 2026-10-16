//! Operator directives (ARCHITECTURE §18.5): standing instructions to the
//! orchestrator. Expired ones are deleted, never shown.

use archon_core::Timestamp;
use archon_core::autonomy::{Directive, MAX_DIRECTIVES};
use archon_core::error::ArchonError;
use archon_core::id::DirectiveId;
use rusqlite::{Connection, params};

use super::codec::{SqlContext, from_json, to_json, ts, write_tx};

fn prune(conn: &Connection, now: &Timestamp) -> Result<usize, ArchonError> {
    conn.execute(
        "DELETE FROM directives WHERE expires_at IS NOT NULL AND expires_at <= ?1",
        [ts(now)],
    )
    .ctx("prune expired directives")
}

fn count(conn: &Connection) -> Result<usize, ArchonError> {
    let n: i64 = conn
        .query_row("SELECT COUNT(*) FROM directives", [], |row| row.get(0))
        .ctx("count directives")?;
    usize::try_from(n).map_err(|_| ArchonError::Store(format!("directive count {n} is negative")))
}

/// Refused once [`MAX_DIRECTIVES`] are in force at the directive's `set_at`.
pub(super) fn insert(conn: &mut Connection, directive: &Directive) -> Result<(), ArchonError> {
    if !directive.is_active(directive.set_at) {
        return Err(ArchonError::Invalid(
            "a directive cannot start expired".into(),
        ));
    }
    let tx = write_tx(conn)?;
    prune(&tx, &directive.set_at)?;
    if count(&tx)? >= MAX_DIRECTIVES {
        let in_force = active(&tx, &directive.set_at)?;
        return Err(ArchonError::Invalid(refusal(&in_force)));
    }
    let inserted = tx
        .execute(
            "INSERT INTO directives (id, set_at, expires_at, data) VALUES (?1, ?2, ?3, ?4)",
            params![
                directive.id.as_str(),
                ts(&directive.set_at),
                directive.expires_at.as_ref().map(ts),
                to_json(directive, "directive")?,
            ],
        )
        .ctx("insert directive")?;
    assert_eq!(inserted, 1, "one row per directive");
    debug_assert!(
        count(&tx)? <= MAX_DIRECTIVES,
        "the cap holds after inserting"
    );
    tx.commit().ctx("commit directive")
}

/// Directives listed per group in the refusal.
const NAMED: usize = 3;

/// Why a directive was refused at the cap, naming which ones to clear:
/// the oldest and the soonest to lapse. `in_force` is oldest first.
fn refusal(in_force: &[Directive]) -> String {
    assert!(in_force.len() >= MAX_DIRECTIVES, "refused only at the cap");
    debug_assert!(in_force.is_sorted_by_key(|d| (d.set_at, d.id.as_str().to_owned())));
    let oldest: Vec<String> = in_force
        .iter()
        .take(NAMED)
        .map(|d| format!("{} \"{}\" (set {})", d.id, snippet(&d.text), utc(&d.set_at)))
        .collect();
    let mut lapsing: Vec<&Directive> = in_force.iter().filter(|d| d.expires_at.is_some()).collect();
    lapsing.sort_by_key(|d| d.expires_at);
    let lapsing: Vec<String> = lapsing
        .iter()
        .take(NAMED)
        .filter_map(|d| Some(format!("{} (until {})", d.id, utc(d.expires_at.as_ref()?))))
        .collect();
    let lapsing = if lapsing.is_empty() {
        "none lapses on its own".to_owned()
    } else {
        format!("soonest to lapse: {}", lapsing.join(", "))
    };
    format!(
        "{} directives are in force, the most at once (MAX_DIRECTIVES = {MAX_DIRECTIVES}); \
         clear one first with `archon autonomy clear <id>` or clear_directive. \
         Oldest: {}; {lapsing}",
        in_force.len(),
        oldest.join(", ")
    )
}

fn utc(at: &Timestamp) -> String {
    at.format("%Y-%m-%d %H:%M UTC").to_string()
}

/// The first words of a directive, enough to recognise it.
fn snippet(text: &str) -> String {
    const CHARS: usize = 40;
    match text.char_indices().nth(CHARS) {
        Some((end, _)) => format!("{}…", &text[..end]),
        None => text.to_owned(),
    }
}

/// The directives in force at `now`, oldest first.
pub(super) fn active(conn: &Connection, now: &Timestamp) -> Result<Vec<Directive>, ArchonError> {
    let mut stmt = conn
        .prepare_cached(
            "SELECT data FROM directives WHERE expires_at IS NULL OR expires_at > ?1
             ORDER BY set_at, id",
        )
        .ctx("list directives")?;
    let rows = stmt
        .query_map([ts(now)], |row| row.get::<_, String>(0))
        .ctx("list directives")?;
    let mut out: Vec<Directive> = Vec::new();
    for data in rows {
        out.push(from_json(&data.ctx("read directive")?, "directive")?);
    }
    debug_assert!(out.iter().all(|d| d.is_active(*now)));
    Ok(out)
}

/// Delete one directive (`NotFound` if absent) or, with `None`, all of them.
/// Returns how many were deleted.
pub(super) fn clear(conn: &mut Connection, id: Option<&DirectiveId>) -> Result<u32, ArchonError> {
    let tx = write_tx(conn)?;
    let deleted = match id {
        Some(id) => tx
            .execute("DELETE FROM directives WHERE id = ?1", [id.as_str()])
            .ctx("clear directive")?,
        None => tx
            .execute("DELETE FROM directives", [])
            .ctx("clear directives")?,
    };
    if let Some(id) = id
        && deleted == 0
    {
        return Err(ArchonError::NotFound(format!("directive {id}")));
    }
    assert!(id.is_none() || deleted == 1, "directive ids are unique");
    debug_assert!(id.is_some() || count(&tx)? == 0, "clearing all leaves none");
    tx.commit().ctx("commit cleared directives")?;
    u32::try_from(deleted).map_err(|_| ArchonError::Store(format!("{deleted} directives cleared")))
}
