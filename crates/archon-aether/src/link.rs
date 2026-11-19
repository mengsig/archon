//! A reconnecting handle to Aether (ARCHITECTURE §16). The daemon's event
//! pump owns reconnection and calls [`AetherLink::replace`]; everyone else
//! asks for the current client and gets `Unavailable` while disconnected,
//! never a stale connection.

use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};

use archon_core::error::ArchonError;
use archon_core::model::ModelRef;

use crate::client::AetherClient;

#[derive(Debug, Default)]
pub struct AetherLink {
    current: RwLock<Slot>,
    catalog_refresh: tokio::sync::Mutex<()>,
}

#[derive(Debug, Default)]
struct Slot {
    client: Option<Arc<AetherClient>>,
    /// Why the link is down, as callers are told; `None` says only that.
    down: Option<String>,
    generation: u64,
    /// Catalog aliases read on this connection; dropped with it.
    aliases: Arc<BTreeMap<ModelRef, ModelRef>>,
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
        Self::installed(&guard)
    }

    /// The client of connection `generation` only: a reconnect since then is
    /// refused, so a request checked on one connection is not sent on another.
    pub fn client_on(&self, generation: u64) -> Result<Arc<AetherClient>, ArchonError> {
        let guard = self
            .current
            .read()
            .map_err(|_| ArchonError::Unavailable("aether link lock poisoned".into()))?;
        if guard.generation != generation {
            return Err(ArchonError::Unavailable(format!(
                "aether connection {} replaced connection {generation} since the check",
                guard.generation
            )));
        }
        Self::installed(&guard)
    }

    fn installed(guard: &Slot) -> Result<Arc<AetherClient>, ArchonError> {
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
        self.set(Slot {
            client,
            down: None,
            generation: 0,
            aliases: Arc::default(),
        })
    }

    /// Mark the link down and say why, with what brings it back.
    pub fn mark_down(&self, why: impl Into<String>) -> Result<(), ArchonError> {
        let why = why.into();
        assert!(!why.is_empty(), "a down link says why");
        self.set(Slot {
            client: None,
            down: Some(why),
            generation: 0,
            aliases: Arc::default(),
        })
    }

    pub async fn lock_catalog(&self) -> tokio::sync::MutexGuard<'_, ()> {
        self.catalog_refresh.lock().await
    }

    pub fn generation(&self) -> Result<u64, ArchonError> {
        self.current
            .read()
            .map(|slot| slot.generation)
            .map_err(|_| ArchonError::Unavailable("aether link lock poisoned".into()))
    }

    /// The aliases of the current connection; empty until a catalog read.
    pub fn aliases(&self) -> Arc<BTreeMap<ModelRef, ModelRef>> {
        self.current
            .read()
            .map(|slot| Arc::clone(&slot.aliases))
            .unwrap_or_default()
    }

    /// Keep a catalog read's aliases only if the connection it was read on
    /// is still the installed one.
    pub fn install_aliases(
        &self,
        generation: u64,
        aliases: BTreeMap<ModelRef, ModelRef>,
    ) -> Result<(), ArchonError> {
        let mut guard = self
            .current
            .write()
            .map_err(|_| ArchonError::Unavailable("aether link lock poisoned".into()))?;
        if guard.client.is_none() {
            return Err(ArchonError::Unavailable(
                "aether link is disconnected".into(),
            ));
        }
        if guard.generation != generation {
            return Err(ArchonError::Unavailable(
                "aether connection changed during the catalog read".into(),
            ));
        }
        guard.aliases = Arc::new(aliases);
        Ok(())
    }

    /// Forget the aliases after a failed catalog read on `generation`.
    pub fn clear_aliases(&self, generation: u64) {
        if let Ok(mut guard) = self.current.write()
            && guard.generation == generation
        {
            guard.aliases = Arc::default();
        }
    }

    fn set(&self, mut slot: Slot) -> Result<(), ArchonError> {
        let mut guard = self
            .current
            .write()
            .map_err(|_| ArchonError::Unavailable("aether link lock poisoned".into()))?;
        slot.generation = guard.generation.wrapping_add(1);
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

    #[test]
    fn aliases_are_refused_while_disconnected() {
        let link = AetherLink::new();
        let generation = link.generation().unwrap();
        assert!(link.install_aliases(generation, BTreeMap::new()).is_err());
        assert!(link.aliases().is_empty());
    }
}
