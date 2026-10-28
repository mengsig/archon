//! Open WebSocket sessions, so a revoke or a disable closes them at once.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};

use tokio::sync::{OwnedSemaphorePermit, Semaphore, watch};

/// Why archond closes a session, each with its close code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Close {
    Revoked,
    Disabled,
    /// An allowed origin was removed; sessions reconnect and are checked again.
    OriginWithdrawn,
    ShuttingDown,
}

impl Close {
    pub fn code(self) -> u16 {
        match self {
            Self::Revoked => 4401,
            Self::Disabled => 4403,
            Self::OriginWithdrawn => 4002,
            Self::ShuttingDown => 1001,
        }
    }

    pub fn reason(self) -> &'static str {
        match self {
            Self::Revoked => "this device was unpaired",
            Self::Disabled => "remote access was turned off",
            Self::OriginWithdrawn => "an allowed origin was withdrawn",
            Self::ShuttingDown => "archond is stopping",
        }
    }
}

#[derive(Debug)]
pub struct Sessions {
    slots: Arc<Semaphore>,
    open: Mutex<Open>,
}

#[derive(Debug, Default)]
struct Open {
    next: u64,
    by_id: HashMap<u64, Live>,
}

#[derive(Debug)]
struct Live {
    device: String,
    close: watch::Sender<Option<Close>>,
}

/// A registered session: dropping it unregisters it and frees its slot.
#[derive(Debug)]
pub struct Registration {
    sessions: Arc<Sessions>,
    id: u64,
    pub closed: watch::Receiver<Option<Close>>,
    _slot: OwnedSemaphorePermit,
}

impl Drop for Registration {
    fn drop(&mut self) {
        self.sessions.lock().by_id.remove(&self.id);
    }
}

impl Sessions {
    pub fn new(max: usize) -> Self {
        assert!(max > 0, "at least one session may open");
        Self {
            slots: Arc::new(Semaphore::new(max)),
            open: Mutex::default(),
        }
    }

    fn lock(&self) -> MutexGuard<'_, Open> {
        self.open
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// A slot for a new session, or `None` when every one is taken.
    pub fn slot(&self) -> Option<OwnedSemaphorePermit> {
        self.slots.clone().try_acquire_owned().ok()
    }

    pub fn register(self: &Arc<Self>, device: &str, slot: OwnedSemaphorePermit) -> Registration {
        let (close, closed) = watch::channel(None);
        let mut open = self.lock();
        open.next += 1;
        let id = open.next;
        let live = Live {
            device: device.to_owned(),
            close,
        };
        assert!(
            open.by_id.insert(id, live).is_none(),
            "session ids are not reused"
        );
        Registration {
            sessions: self.clone(),
            id,
            closed,
            _slot: slot,
        }
    }

    pub fn count(&self, device: &str) -> u32 {
        let open = self.lock();
        let count = open.by_id.values().filter(|l| l.device == device).count();
        u32::try_from(count).unwrap_or(u32::MAX)
    }

    pub fn close_device(&self, device: &str, why: Close) {
        for live in self.lock().by_id.values().filter(|l| l.device == device) {
            live.close.send_replace(Some(why));
        }
    }

    pub fn close_all(&self, why: Close) {
        for live in self.lock().by_id.values() {
            live.close.send_replace(Some(why));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn revoking_closes_only_that_devices_sessions() {
        let sessions = Arc::new(Sessions::new(4));
        let a = sessions.register("dev_a", sessions.slot().unwrap());
        let b = sessions.register("dev_b", sessions.slot().unwrap());
        assert_eq!(sessions.count("dev_a"), 1);
        sessions.close_device("dev_a", Close::Revoked);
        assert_eq!(*a.closed.borrow(), Some(Close::Revoked));
        assert_eq!(*b.closed.borrow(), None);
        drop(a);
        assert_eq!(sessions.count("dev_a"), 0);
        sessions.close_all(Close::Disabled);
        assert_eq!(*b.closed.borrow(), Some(Close::Disabled));
    }

    #[test]
    fn sessions_beyond_the_cap_get_no_slot() {
        let sessions = Arc::new(Sessions::new(1));
        let held = sessions.slot().unwrap();
        assert!(sessions.slot().is_none());
        drop(held);
        assert!(sessions.slot().is_some());
        assert_eq!(Close::Revoked.code(), 4401);
    }
}
