//! A reconnecting handle to Aether (ARCHITECTURE §16). The daemon's event
//! pump owns reconnection and calls [`AetherLink::replace`]; everyone else
//! asks for the current client and gets `Unavailable` while disconnected,
//! never a stale connection.

use std::sync::{Arc, RwLock};

use archon_core::error::ArchonError;

use crate::client::AetherClient;

#[derive(Debug, Default)]
pub struct AetherLink {
    current: RwLock<Slot>,
}

#[derive(Debug, Default)]
struct Slot {
    client: Option<Arc<AetherClient>>,
    /// Why the link is down, as callers are told; `None` says only that.
    down: Option<String>,
}

impl AetherLink {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn client(&self) -> Result<Arc<AetherClient>, ArchonError> {
        let guard = self
            .current
            .read()
            .map_err(|_| ArchonError::Unavailable("aether link lock poisoned".into()))?;
        let Some(client) = guard.client.clone() else {
            let why = guard.down.as_deref().unwrap_or("not connected to aetherd");
            return Err(ArchonError::Unavailable(why.to_owned()));
        };
        // Between the reader seeing the connection end and the pump replacing
        // it, the installed client is dead; say so rather than hand it out.
        match client.disconnect_reason() {
            Some(reason) => Err(ArchonError::Unavailable(reason)),
            None => Ok(client),
        }
    }

    /// `false` while disconnected, including before the pump notices a drop.
    pub fn is_connected(&self) -> bool {
        self.current
            .read()
            .is_ok_and(|guard| guard.client.as_ref().is_some_and(|c| c.is_connected()))
    }

    /// Install a fresh connection (`Some`) or mark the link down (`None`).
    pub fn replace(&self, client: Option<Arc<AetherClient>>) -> Result<(), ArchonError> {
        self.set(Slot { client, down: None })
    }

    /// Mark the link down and say why, with what brings it back.
    pub fn mark_down(&self, why: impl Into<String>) -> Result<(), ArchonError> {
        let why = why.into();
        assert!(!why.is_empty(), "a down link says why");
        self.set(Slot {
            client: None,
            down: Some(why),
        })
    }

    fn set(&self, slot: Slot) -> Result<(), ArchonError> {
        let mut guard = self
            .current
            .write()
            .map_err(|_| ArchonError::Unavailable("aether link lock poisoned".into()))?;
        *guard = slot;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fresh_link_is_disconnected_and_says_so() {
        let link = AetherLink::new();
        assert!(!link.is_connected());
        assert!(matches!(link.client(), Err(ArchonError::Unavailable(_))));
        link.replace(None).unwrap();
        assert!(!link.is_connected());
    }

    #[test]
    fn a_link_marked_down_says_why_until_it_is_replaced() {
        let link = AetherLink::new();
        link.mark_down("aetherd is starting").unwrap();
        let error = link.client().unwrap_err();
        assert!(error.to_string().contains("aetherd is starting"), "{error}");
        link.replace(None).unwrap();
        let error = link.client().unwrap_err();
        assert!(error.to_string().contains("not connected"), "{error}");
    }
}
