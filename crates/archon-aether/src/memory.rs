//! Aether's memory records, narrowed to what curating one named record
//! needs: read it by id, forget it, restore it. Its text is never decoded.

use archon_core::Timestamp;
use archon_core::aether_ids::AetherMemoryId;
use archon_core::error::ArchonError;
use archon_core::feedback::MemoryView;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::client::AetherClient;
use crate::wire::Command;

/// Aether's `MemoryQuerySpec`, as Archon fills it: one record by id.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MemoryQuery {
    pub id: AetherMemoryId,
    pub include_forgotten: bool,
    pub include_superseded: bool,
    pub limit: u32,
}

/// The parts of Aether's `MemoryRecord` Archon shows; `text` stays unread.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct MemoryRecord {
    pub id: AetherMemoryId,
    pub scope: Value,
    pub kind: String,
    pub provenance: Value,
    pub created_at: Timestamp,
    #[serde(default)]
    pub forgotten_at: Option<Timestamp>,
    #[serde(default)]
    pub superseded_by: Option<String>,
    #[serde(default)]
    pub pinned: bool,
}

impl MemoryRecord {
    pub fn view(&self) -> MemoryView {
        MemoryView {
            id: self.id.clone(),
            origin: origin(&self.provenance),
            scope: scope(&self.scope),
            kind: self.kind.clone(),
            created_at: self.created_at,
            forgotten_at: self.forgotten_at,
            superseded_by: self.superseded_by.clone(),
            pinned: self.pinned,
        }
    }
}

/// Who wrote a memory, from Aether's authenticated actor first: only a
/// person's own write reads as `operator`.
pub fn origin(provenance: &Value) -> String {
    let text = |value: &Value, key: &str| value.get(key).and_then(Value::as_str).map(str::to_owned);
    let file = provenance
        .get("source")
        .filter(|s| !s.is_null())
        .map(|source| {
            let path = text(source, "file").unwrap_or_else(|| "?".into());
            text(source, "commit").map_or(format!("file {path}"), |c| format!("file {path}@{c}"))
        });
    let actor = provenance.get("actor").filter(|a| !a.is_null()).map(actor);
    match (actor, file) {
        (Some(actor), Some(file)) => format!("{actor} from {file}"),
        (Some(actor), None) => actor,
        (None, Some(file)) => file,
        (None, None) => {
            text(provenance, "agent").map_or("unknown".into(), |a| format!("agent {a}"))
        }
    }
}

fn actor(actor: &Value) -> String {
    let field = |key: &str| actor.get(key).and_then(Value::as_str);
    match field("actor") {
        Some("user") => field("name").map_or("operator".into(), |n| format!("operator {n}")),
        Some("agent") => format!("agent {}", field("id").unwrap_or("?")),
        Some("client") => format!("client {}", field("id").unwrap_or("?")),
        Some("imported") => {
            let was = actor.get("was").map_or("unknown".into(), self::actor);
            format!("imported ({was})")
        }
        Some(kind) => kind.to_owned(),
        None => "unknown".into(),
    }
}

/// Where a memory applies, as Aether's `MemoryScope` names it.
pub fn scope(scope: &Value) -> String {
    let field = |key: &str| scope.get(key).and_then(Value::as_str).unwrap_or("?");
    match scope.get("scope").and_then(Value::as_str) {
        Some("global") => "global".into(),
        Some("repository") => format!("repository {}", field("id")),
        Some("role") => format!("role {}", field("role")),
        Some("agent") => format!("agent {}", field("id")),
        Some("repository_role") => format!("repository {} role {}", field("id"), field("role")),
        Some("subtree") => format!("subtree of {}", field("root")),
        Some(other) => format!("{other} (a scope this Archon does not know)"),
        None => "unknown".into(),
    }
}

impl AetherClient {
    /// The record `id`, forgotten or superseded; `None` when Aether has none.
    pub async fn memory(&self, id: &AetherMemoryId) -> Result<Option<MemoryRecord>, ArchonError> {
        let query = MemoryQuery {
            id: id.clone(),
            include_forgotten: true,
            include_superseded: true,
            limit: 1,
        };
        let data = self
            .request(Command::Memories { query }, "memories")
            .await?;
        let records: Vec<MemoryRecord> = decode(data, "memories")?;
        if records.len() > 1 || records.iter().any(|r| &r.id != id) {
            return Err(ArchonError::Protocol(format!(
                "aetherd answered a query for {id} with other memories"
            )));
        }
        Ok(records.into_iter().next())
    }

    /// Forgets `id`; Aether answers an already forgotten record unchanged.
    pub async fn forget_memory(
        &self,
        id: &AetherMemoryId,
        reason: &str,
    ) -> Result<MemoryRecord, ArchonError> {
        assert!(!reason.trim().is_empty(), "a forget says why");
        let command = Command::ForgetMemory {
            id: id.clone(),
            reason: Some(reason.to_owned()),
        };
        let record: MemoryRecord = decode(self.request(command, "memory").await?, "memory")?;
        if &record.id != id || record.forgotten_at.is_none() {
            return Err(ArchonError::Protocol(format!(
                "aetherd answered the forget of {id} without forgetting it"
            )));
        }
        Ok(record)
    }

    pub async fn restore_memory(&self, id: &AetherMemoryId) -> Result<MemoryRecord, ArchonError> {
        let command = Command::RestoreMemory { id: id.clone() };
        let record: MemoryRecord = decode(self.request(command, "memory").await?, "memory")?;
        if &record.id != id || record.forgotten_at.is_some() {
            return Err(ArchonError::Protocol(format!(
                "aetherd answered the restore of {id} without restoring it"
            )));
        }
        Ok(record)
    }
}

fn decode<T: serde::de::DeserializeOwned>(data: Value, kind: &str) -> Result<T, ArchonError> {
    serde_json::from_value(data)
        .map_err(|error| ArchonError::Protocol(format!("`{kind}` reply did not decode: {error}")))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn origins_follow_the_authenticated_actor() {
        let operator = json!({"actor": {"actor": "user", "name": null, "origin": null}});
        assert_eq!(origin(&operator), "operator");
        let proposal = json!({"actor": {"actor": "agent", "id": "agt_1"}, "agent": "agt_1"});
        assert_eq!(origin(&proposal), "agent agt_1");
        let note = json!({"source": {"file": "AGENTS.md", "commit": "abc"}});
        assert_eq!(origin(&note), "file AGENTS.md@abc");
        let synced =
            json!({"actor": {"actor": "system"}, "source": {"file": "a.md", "commit": null}});
        assert_eq!(origin(&synced), "system from file a.md");
        let imported =
            json!({"actor": {"actor": "imported", "was": {"actor": "user", "name": "ana"}}});
        assert_eq!(origin(&imported), "imported (operator ana)");
        assert_eq!(origin(&json!({"agent": "agt_2"})), "agent agt_2");
    }

    #[test]
    fn scopes_name_where_a_memory_applies() {
        assert_eq!(scope(&json!({"scope": "global"})), "global");
        assert_eq!(
            scope(&json!({"scope": "repository", "id": "r1"})),
            "repository r1"
        );
        let family = json!({"scope": "repository_role", "id": "r1", "role": "worker"});
        assert_eq!(scope(&family), "repository r1 role worker");
        assert_eq!(
            scope(&json!({"scope": "subtree", "root": "agt_1"})),
            "subtree of agt_1"
        );
        assert!(scope(&json!({"scope": "galaxy"})).contains("does not know"));
    }
}
