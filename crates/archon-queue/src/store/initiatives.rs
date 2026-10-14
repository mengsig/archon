//! Atomic initiative creation and persistent lifecycle state.

use archon_core::error::ArchonError;
use archon_core::id::InitiativeId;
use archon_core::initiative::{
    ContractChange, EnqueueInitiative, Initiative, InitiativeDetail, InitiativeLimits,
    InitiativeState,
};
use archon_core::queue::{QueueItem, QueueState};
use rusqlite::{Connection, OptionalExtension, params};

use super::Store;
use super::codec::{SqlContext, from_json, to_json, ts, write_tx};
use super::{items, settings};

impl Store {
    pub fn initiative(&self, id: &InitiativeId) -> Result<InitiativeDetail, ArchonError> {
        detail(&self.conn, id)
    }

    pub fn initiatives(&self) -> Result<Vec<Initiative>, ArchonError> {
        let mut stmt = self
            .conn
            .prepare_cached("SELECT data FROM initiatives ORDER BY created_at DESC, id")
            .ctx("list initiatives")?;
        let rows = stmt
            .query_map([], |row| row.get::<_, String>(0))
            .ctx("list initiatives")?;
        rows.map(|row| from_json(&row.ctx("read initiative")?, "initiative"))
            .collect()
    }

    pub fn replay_initiative(
        &self,
        request: &EnqueueInitiative,
    ) -> Result<Option<InitiativeDetail>, ArchonError> {
        replay(&self.conn, request)
    }

    pub fn insert_initiative(
        &mut self,
        initiative: &Initiative,
        members: &[QueueItem],
        request: &EnqueueInitiative,
    ) -> Result<InitiativeDetail, ArchonError> {
        request.check(&InitiativeLimits::CEILING)?;
        assert_eq!(initiative.state, InitiativeState::Open);
        assert_eq!(members.len(), request.members.len());
        let tx = write_tx(&mut self.conn)?;
        if let Some(existing) = replay(&tx, request)? {
            return Ok(existing);
        }
        tx.execute(
            "INSERT INTO initiatives (id,state,idempotency_key,request,created_at,updated_at,data) VALUES (?1,?2,?3,?4,?5,?6,?7)",
            params![initiative.id.as_str(), initiative.state.as_str(), request.idempotency_key,
                to_json(request, "initiative request")?, ts(&initiative.created_at),
                ts(&initiative.updated_at), to_json(initiative, "initiative")?],
        ).ctx("insert initiative")?;
        for item in members {
            assert_eq!(item.initiative.as_ref(), Some(&initiative.id));
            items::insert_in(&tx, item, None, &Default::default())?;
        }
        let stored = detail(&tx, &initiative.id)?;
        assert_eq!(stored.members.len(), members.len());
        tx.commit().ctx("commit initiative and members")?;
        Ok(stored)
    }

    /// Append to or replace the contract of an initiative that is not
    /// finished. A replacement with the same text changes nothing.
    pub fn change_initiative_contract(
        &mut self,
        id: &InitiativeId,
        change: &ContractChange,
        limits: &InitiativeLimits,
    ) -> Result<InitiativeDetail, ArchonError> {
        assert!(limits.within(&InitiativeLimits::CEILING));
        let tx = write_tx(&mut self.conn)?;
        let mut current = detail(&tx, id)?;
        if current.initiative.state.is_terminal() {
            return Err(ArchonError::InvalidTransition(
                "retry a terminal initiative before changing its contract".into(),
            ));
        }
        let contract = change.apply(&current.initiative.contract, limits)?;
        if contract == current.initiative.contract {
            return Ok(current);
        }
        current.initiative.contract = contract;
        save(&tx, &mut current.initiative)?;
        tx.commit().ctx("change initiative contract")?;
        assert_eq!(&current.initiative.id, id);
        Ok(current)
    }

    pub fn set_initiative_state(
        &mut self,
        id: &InitiativeId,
        state: InitiativeState,
    ) -> Result<InitiativeDetail, ArchonError> {
        let tx = write_tx(&mut self.conn)?;
        let mut current = detail(&tx, id)?;
        let before = current.initiative.state;
        if before == state {
            return Ok(current);
        }
        let cancelling_failure =
            before == InitiativeState::Failed && state == InitiativeState::Cancelled;
        if (before.is_terminal() && !cancelling_failure) || state == InitiativeState::Open {
            return Err(ArchonError::InvalidTransition(format!(
                "initiative cannot move from {} to {}",
                before.as_str(),
                state.as_str()
            )));
        }
        assert!(!before.is_terminal() || cancelling_failure);
        assert_ne!(before, state);
        current.initiative.state = state;
        save(&tx, &mut current.initiative)?;
        tx.commit().ctx("transition initiative")?;
        Ok(current)
    }

    pub fn retry_initiative(&mut self, id: &InitiativeId) -> Result<InitiativeDetail, ArchonError> {
        let tx = write_tx(&mut self.conn)?;
        let mut current = detail(&tx, id)?;
        if !matches!(
            current.initiative.state,
            InitiativeState::Failed | InitiativeState::Cancelled
        ) || current.members.iter().any(|m| m.state.is_active())
        {
            return Err(ArchonError::InvalidTransition(
                "retry requires a failed/cancelled initiative with no active members".into(),
            ));
        }
        assert!(!current.members.is_empty());
        for item in &mut current.members {
            if matches!(
                item.state,
                QueueState::Failed { .. }
                    | QueueState::Cancelled { .. }
                    | QueueState::Blocked { .. }
            ) {
                *item =
                    items::transition_in(&tx, &item.id, QueueState::Queued, archon_core::now())?;
            }
        }
        assert!(
            current
                .members
                .iter()
                .all(|m| matches!(m.state, QueueState::Queued | QueueState::Succeeded { .. }))
        );
        current.initiative.state = InitiativeState::Open;
        save(&tx, &mut current.initiative)?;
        settings::kv_set(
            &tx,
            &format!("initiative_cycle:{id}"),
            &ts(&current.initiative.updated_at),
        )?;
        tx.commit().ctx("retry initiative and members")?;
        Ok(current)
    }
}

fn replay(
    conn: &Connection,
    request: &EnqueueInitiative,
) -> Result<Option<InitiativeDetail>, ArchonError> {
    let Some(key) = &request.idempotency_key else {
        return Ok(None);
    };
    if key.trim().is_empty() {
        return Err(ArchonError::Invalid(
            "idempotency_key must not be blank".into(),
        ));
    }
    let found: Option<(String, String)> = conn
        .query_row(
            "SELECT id,request FROM initiatives WHERE idempotency_key=?1",
            [key],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()
        .ctx("look up initiative key")?;
    let Some((id, raw)) = found else {
        return Ok(None);
    };
    let stored: EnqueueInitiative = from_json(&raw, "initiative request")?;
    if stored != *request {
        return Err(ArchonError::Invalid(format!(
            "initiative key {key:?} was used for a different request"
        )));
    }
    assert_eq!(stored.idempotency_key.as_ref(), Some(key));
    let current = detail(conn, &InitiativeId::parse(&id)?)?;
    assert_eq!(current.members.len(), request.members.len());
    Ok(Some(current))
}

fn detail(conn: &Connection, id: &InitiativeId) -> Result<InitiativeDetail, ArchonError> {
    let raw: Option<String> = conn
        .query_row(
            "SELECT data FROM initiatives WHERE id=?1",
            [id.as_str()],
            |row| row.get(0),
        )
        .optional()
        .ctx("read initiative")?;
    let initiative: Initiative = from_json(
        &raw.ok_or_else(|| ArchonError::NotFound(format!("initiative {id}")))?,
        "initiative",
    )?;
    assert_eq!(&initiative.id, id);
    let mut stmt = conn
        .prepare_cached("SELECT data FROM items WHERE initiative_id=?1 ORDER BY seq")
        .ctx("list initiative members")?;
    let rows = stmt
        .query_map([id.as_str()], |row| row.get::<_, String>(0))
        .ctx("list initiative members")?;
    let members: Vec<QueueItem> = rows
        .map(|row| from_json(&row.ctx("read member")?, "member"))
        .collect::<Result<_, _>>()?;
    assert!(members.iter().all(|m| m.initiative.as_ref() == Some(id)));
    Ok(InitiativeDetail {
        initiative,
        members,
    })
}

fn save(conn: &Connection, initiative: &mut Initiative) -> Result<(), ArchonError> {
    assert!(!initiative.title.is_empty());
    let ceiling = InitiativeLimits::CEILING;
    assert!(archon_core::initiative::check_contract(&initiative.contract, &ceiling).is_ok());
    initiative.updated_at =
        archon_core::now().max(initiative.updated_at + chrono::Duration::nanoseconds(1));
    let changed = conn
        .execute(
            "UPDATE initiatives SET state=?2,updated_at=?3,data=?4 WHERE id=?1",
            params![
                initiative.id.as_str(),
                initiative.state.as_str(),
                ts(&initiative.updated_at),
                to_json(initiative, "initiative")?
            ],
        )
        .ctx("save initiative")?;
    assert_eq!(changed, 1);
    Ok(())
}
