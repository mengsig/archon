use super::codec::{SqlContext, from_json, to_json};
use super::settings;
use archon_core::error::ArchonError;
use archon_core::intent::{ApprovedIntent, IntentBinding};
use rusqlite::{Connection, params};

pub(super) const PREFIX: &str = "approved_intent:";

fn key(id: &str, version: u32) -> Result<String, ArchonError> {
    if id.is_empty()
        || !id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
        || version == 0
    {
        return Err(ArchonError::Invalid(
            "invalid intent binding id or version".into(),
        ));
    }
    Ok(format!("{PREFIX}{id}:{version}"))
}

pub(super) fn save(conn: &Connection, record: &ApprovedIntent) -> Result<(), ArchonError> {
    record.validate()?;
    let key = key(&record.id, record.version)?;
    let data = to_json(record, "approved intent")?;
    let changed = conn
        .execute(
            "INSERT OR IGNORE INTO kv (key, value) VALUES (?1, ?2)",
            params![key, data],
        )
        .ctx("save approved intent")?;
    if changed != 1 {
        return Err(ArchonError::InvalidTransition(
            "approved intent version already exists".into(),
        ));
    }
    Ok(())
}

pub(super) fn get(
    conn: &Connection,
    binding: &IntentBinding,
) -> Result<Option<ApprovedIntent>, ArchonError> {
    let key = key(&binding.id, binding.version)?;
    let Some(data) = settings::kv_get(conn, &key)? else {
        return Ok(None);
    };
    let record: ApprovedIntent = from_json(&data, "approved intent")?;
    record.selected_requirements(binding)?;
    Ok(Some(record))
}

pub(super) fn check_public_key(key: &str) -> Result<(), ArchonError> {
    if key.starts_with(PREFIX) {
        return Err(ArchonError::Invalid(
            "approved intent keys are immutable".into(),
        ));
    }
    Ok(())
}
