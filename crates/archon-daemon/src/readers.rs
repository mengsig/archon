//! archond's reader connections (ARCHITECTURE §16, "SQLite busy"). The
//! reads it serves run on read-only WAL connections of their own, so they
//! never queue behind the writer's lock or its synced commits. A reader
//! holds one snapshot while it is borrowed, so its reads agree with each
//! other as reads under the writer's lock do. A context without readers (an
//! in-memory store) reads on the writer.

use std::ops::Deref;
use std::path::Path;
use std::time::Duration;

use archon_core::error::ArchonError;
use archon_queue::{Checkpointer, Store};
use tokio::sync::{MutexGuard, Semaphore, SemaphorePermit};

use crate::context::DaemonContext;

/// Reader connections archond opens beside its writer.
pub const READERS: usize = 4;
/// How often the WAL is checkpointed off the writer.
pub const CHECKPOINT_EVERY: Duration = Duration::from_secs(2);

/// A pool of reader connections, or none.
#[derive(Debug, Default)]
pub struct Readers {
    pool: Option<Pool>,
}

#[derive(Debug)]
struct Pool {
    idle: std::sync::Mutex<Vec<Store>>,
    permits: Semaphore,
}

impl Readers {
    /// `count` reader connections beside `writer`, the on-disk store at `path`.
    pub fn open(writer: &Store, path: &Path, count: usize) -> Result<Self, ArchonError> {
        assert!(count > 0, "a pool holds a reader");
        let idle = (0..count)
            .map(|_| writer.open_reader(path))
            .collect::<Result<Vec<_>, _>>()?;
        assert_eq!(idle.len(), count, "every reader opened");
        let pool = Pool {
            idle: std::sync::Mutex::new(idle),
            permits: Semaphore::new(count),
        };
        Ok(Self { pool: Some(pool) })
    }
}

/// Where a read runs: the dispatcher reads on the writer, so it sees a
/// change another task is committing only once that commit is done.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadOn {
    Reader,
    Writer,
}

/// A store to read on, held for one read; derefs to `&Store` only.
#[derive(Debug)]
pub enum ReadStore<'a> {
    Reader(Box<Pooled<'a>>),
    Writer(MutexGuard<'a, Store>),
}

/// A reader connection taken from the pool, returned to it on drop.
#[derive(Debug)]
pub struct Pooled<'a> {
    store: Option<Store>,
    pool: &'a Pool,
    _permit: SemaphorePermit<'a>,
}

impl Drop for Pooled<'_> {
    fn drop(&mut self) {
        let store = self
            .store
            .take()
            .expect("a pooled reader is held until dropped");
        // Drop cannot fail; the reader's next borrow rolls back what is left open.
        if let Err(error) = store.end_snapshot() {
            tracing::warn!(%error, "a reader's snapshot did not end; its next borrow rolls it back");
        }
        let mut idle = self.pool.idle.lock().unwrap_or_else(|e| e.into_inner());
        idle.push(store);
    }
}

impl Deref for ReadStore<'_> {
    type Target = Store;

    fn deref(&self) -> &Store {
        match self {
            Self::Reader(pooled) => pooled.store.as_ref().expect("a pooled reader is held"),
            Self::Writer(guard) => guard,
        }
    }
}

impl DaemonContext {
    /// A reader connection when archond has them, else the writer.
    pub async fn read_store(&self) -> Result<ReadStore<'_>, ArchonError> {
        self.read_on(ReadOn::Reader).await
    }

    /// A reader holding one snapshot until it is dropped, or the writer.
    pub async fn read_on(&self, on: ReadOn) -> Result<ReadStore<'_>, ArchonError> {
        let Some(pool) = self.readers.pool.as_ref().filter(|_| on == ReadOn::Reader) else {
            return Ok(ReadStore::Writer(self.store.lock().await));
        };
        let permit = pool
            .permits
            .acquire()
            .await
            .expect("the reader pool is never closed");
        let store = pool.idle.lock().unwrap_or_else(|e| e.into_inner()).pop();
        assert!(store.is_some(), "a permit always finds an idle reader");
        // Pooled first: a failed begin still returns the connection to the pool.
        let pooled = Pooled {
            store,
            pool,
            _permit: permit,
        };
        let reader = ReadStore::Reader(Box::new(pooled));
        reader.begin_snapshot()?;
        Ok(reader)
    }
}

/// Checkpoints the WAL every [`CHECKPOINT_EVERY`] on its own connection, so
/// the writer's commits never do it with the store locked.
pub async fn checkpoint(ctx: std::sync::Arc<DaemonContext>) -> Result<(), ArchonError> {
    let path = ctx.paths.store.clone();
    let mut checkpointer = blocking(move || Checkpointer::open(&path)).await?;
    let mut tick = tokio::time::interval(CHECKPOINT_EVERY);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tick.tick().await;
        // The connection moves to the blocking thread and back, never shared.
        let (back, (log, done)) = blocking(move || {
            let frames = checkpointer.checkpoint()?;
            Ok((checkpointer, frames))
        })
        .await?;
        checkpointer = back;
        if done < log {
            tracing::debug!(
                log,
                done,
                "a reader holds part of the WAL; checkpointing it later"
            );
        }
    }
}

async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, ArchonError> + Send + 'static,
) -> Result<T, ArchonError> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|e| ArchonError::io("the checkpoint task", std::io::Error::other(e)))?
}
