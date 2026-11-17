//! The repository registry.

use archon_core::error::ArchonError;
use archon_core::id::RepoId;
use archon_core::protocol::RepoRef;
use archon_core::queue::PauseState;
use archon_core::repo::Repo;
use rusqlite::{Connection, OptionalExtension, params};

use super::codec::{SqlContext, from_json, to_json, ts, write_tx};
use super::settings;

fn path_text(repo: &Repo) -> Result<&str, ArchonError> {
    repo.path.to_str().ok_or_else(|| {
        ArchonError::Invalid(format!("repository path {:?} is not UTF-8", repo.path))
    })
}

fn validate(repo: &Repo) -> Result<(), ArchonError> {
    if !Repo::valid_name(&repo.name) {
        return Err(ArchonError::Invalid(format!(
            "repository name {:?} must be 1-64 characters of [A-Za-z0-9._-] \
             and not start with '.'",
            repo.name
        )));
    }
    if !repo.path.is_absolute() {
        return Err(ArchonError::Invalid(format!(
            "repository path {} must be absolute",
            repo.path.display()
        )));
    }
    Ok(())
}

/// Name and path are unique across the registry, excluding `repo` itself.
fn ensure_unique(conn: &Connection, repo: &Repo) -> Result<(), ArchonError> {
    let clash: Option<(String, String)> = conn
        .query_row(
            "SELECT name, path FROM repos WHERE id <> ?1 AND (name = ?2 OR path = ?3) LIMIT 1",
            params![repo.id.as_str(), repo.name, path_text(repo)?],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()
        .ctx("check repository uniqueness")?;
    match clash {
        None => Ok(()),
        Some((name, _)) if name == repo.name => Err(ArchonError::Invalid(format!(
            "a repository named {name:?} is already registered"
        ))),
        Some((name, path)) => Err(ArchonError::Invalid(format!(
            "{path} is already registered as {name:?}"
        ))),
    }
}

pub(super) fn exists(conn: &Connection, id: &RepoId) -> Result<bool, ArchonError> {
    conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM repos WHERE id = ?1)",
        [id.as_str()],
        |row| row.get(0),
    )
    .ctx("look up repository")
}

pub(super) fn insert(conn: &mut Connection, repo: &Repo) -> Result<(), ArchonError> {
    validate(repo)?;
    let tx = write_tx(conn)?;
    if exists(&tx, &repo.id)? {
        return Err(ArchonError::Invalid(format!(
            "repository {} already exists",
            repo.id
        )));
    }
    ensure_unique(&tx, repo)?;
    tx.execute(
        "INSERT INTO repos (id, name, path, added_at, data) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
            repo.id.as_str(),
            repo.name,
            path_text(repo)?,
            ts(&repo.added_at),
            to_json(repo, "repository")?
        ],
    )
    .ctx("insert repository")?;
    tx.commit().ctx("commit repository")
}

/// A stored row as a repository. A row that does not decode is named, so
/// the operator knows which registration holds the bad value.
fn decode(name: &str, data: &str) -> Result<Repo, ArchonError> {
    let repo: Repo = from_json(data, &format!("repository {name:?}"))?;
    debug_assert_eq!(repo.name, name, "the name column mirrors the record");
    Ok(repo)
}

/// By `repo_…` id first, then by registry name.
pub(super) fn find(conn: &Connection, reference: &RepoRef) -> Result<Repo, ArchonError> {
    let raw = reference.0.trim();
    if raw.is_empty() {
        return Err(ArchonError::Invalid("repository reference is empty".into()));
    }
    let by_id = RepoId::parse(raw).ok();
    let row: Option<(String, String)> = conn
        .query_row(
            "SELECT name, data FROM repos WHERE id = ?1 OR name = ?2 \
             ORDER BY id = ?1 DESC LIMIT 1",
            params![by_id.as_ref().map(RepoId::as_str), raw],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()
        .ctx("look up repository")?;
    let (name, data) = row.ok_or_else(|| ArchonError::NotFound(format!("repository {raw:?}")))?;
    decode(&name, &data)
}

pub(super) fn all(conn: &Connection) -> Result<Vec<Repo>, ArchonError> {
    let mut stmt = conn
        .prepare_cached("SELECT name, data FROM repos ORDER BY name")
        .ctx("list repositories")?;
    let rows = stmt
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .ctx("list repositories")?;
    let mut repos = Vec::new();
    for row in rows {
        let (name, data) = row.ctx("read repository")?;
        repos.push(decode(&name, &data)?);
    }
    Ok(repos)
}

pub(super) fn update(conn: &mut Connection, repo: &Repo) -> Result<(), ArchonError> {
    validate(repo)?;
    let tx = write_tx(conn)?;
    if !exists(&tx, &repo.id)? {
        return Err(ArchonError::NotFound(format!("repository {}", repo.id)));
    }
    ensure_unique(&tx, repo)?;
    let changed = tx
        .execute(
            "UPDATE repos SET name = ?2, path = ?3, added_at = ?4, data = ?5 WHERE id = ?1",
            params![
                repo.id.as_str(),
                repo.name,
                path_text(repo)?,
                ts(&repo.added_at),
                to_json(repo, "repository")?
            ],
        )
        .ctx("update repository")?;
    debug_assert_eq!(
        changed, 1,
        "repository {} vanished inside a transaction",
        repo.id
    );
    tx.commit().ctx("commit repository")
}

/// Refused while any item of the repository is not terminal. Terminal items
/// keep the id for history. The repository also leaves the pause state.
pub(super) fn remove(conn: &mut Connection, id: &RepoId) -> Result<(), ArchonError> {
    let tx = write_tx(conn)?;
    if !exists(&tx, id)? {
        return Err(ArchonError::NotFound(format!("repository {id}")));
    }
    let live: i64 = tx
        .query_row(
            "SELECT COUNT(*) FROM items WHERE repo = ?1 \
             AND state NOT IN ('succeeded', 'failed', 'cancelled')",
            [id.as_str()],
            |row| row.get(0),
        )
        .ctx("count live items")?;
    if live > 0 {
        return Err(ArchonError::Invalid(format!(
            "repository {id} still has {live} unfinished item(s); cancel them first"
        )));
    }
    let pause = settings::pause(&tx)?;
    if pause.repos.contains(id) {
        let repos = pause.repos.iter().filter(|r| *r != id).cloned().collect();
        settings::write_pause(&tx, &PauseState { repos, ..pause })?;
    }
    tx.execute("DELETE FROM repos WHERE id = ?1", [id.as_str()])
        .ctx("delete repository")?;
    tx.commit().ctx("commit repository removal")
}
