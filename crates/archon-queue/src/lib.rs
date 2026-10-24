//! Archon's persistent backlog: the SQLite [`Store`] and the pure
//! [`plan_dispatch`] scheduler (ARCHITECTURE §6, §7). Nothing here talks to
//! Aether; the daemon composes the two.

pub mod data_lock;
pub mod dispatch;
pub mod store;

pub use data_lock::DataLock;
pub use dispatch::{DispatchPlan, DispatchSnapshot, plan_dispatch};
pub use store::{Checkpointer, RunPullRequest, Store};
