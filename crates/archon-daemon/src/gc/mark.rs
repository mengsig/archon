//! The items whose worktrees a gc pass is collecting right now. A pass
//! marks an item holding the run table, then collects it without the
//! table, so live runs are not held up by git and the filesystem; the
//! dispatcher leaves a marked item queued, so no run starts in its
//! worktree meanwhile, even one a retry queued again. A pass whose task
//! was aborted leaves its sweep running with its mark, so a later pass can
//! find an item still marked; it leaves that item to the sweep. A mark
//! pokes dispatch when it goes, so a retry it held back starts then. A
//! Compose teardown, a restack and a fresh retry hold the same mark.

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex, MutexGuard};

use archon_core::id::QueueItemId;
use tokio::sync::Notify;

#[derive(Debug)]
pub struct Collecting {
    items: Arc<Mutex<BTreeSet<QueueItemId>>>,
    /// The dispatcher's wake-up (`DaemonContext::dispatch`).
    dispatch: Arc<Notify>,
}

impl Collecting {
    pub fn new(dispatch: Arc<Notify>) -> Self {
        Self {
            items: Arc::default(),
            dispatch,
        }
    }

    pub fn contains(&self, item: &QueueItemId) -> bool {
        self.items().contains(item)
    }

    /// Mark `item` until the returned mark is dropped, or `None` while a
    /// sweep still holds its mark. Take the run table first, and mark only
    /// an item that is still settled.
    pub fn mark(&self, item: QueueItemId) -> Option<Mark> {
        let fresh = self.items().insert(item.clone());
        if !fresh {
            return None;
        }
        let mark = Mark {
            items: self.items.clone(),
            dispatch: self.dispatch.clone(),
            item,
        };
        debug_assert!(self.contains(&mark.item), "a mark holds its item");
        Some(mark)
    }

    fn items(&self) -> MutexGuard<'_, BTreeSet<QueueItemId>> {
        self.items.lock().expect("no gc mark holder panics")
    }
}

/// Clears its item's mark when dropped, then pokes dispatch. It travels
/// with the sweep, so a pass whose caller went away still holds it until
/// the sweep ends, and a retry it held back starts without waiting for a
/// dispatch tick.
#[derive(Debug)]
pub struct Mark {
    items: Arc<Mutex<BTreeSet<QueueItemId>>>,
    dispatch: Arc<Notify>,
    item: QueueItemId,
}

impl Drop for Mark {
    fn drop(&mut self) {
        let marked = self
            .items
            .lock()
            .expect("no gc mark holder panics")
            .remove(&self.item);
        debug_assert!(marked, "only its own mark clears an item");
        self.dispatch.notify_one();
    }
}
