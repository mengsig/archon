//! Shared Archon domain types.
//!
//! Everything more than one crate needs lives here: identifiers, the
//! configuration schema, the domain records (repositories, queue items,
//! pipeline runs, notifications, initiatives, steers, outcomes, lessons, the
//! pull requests runs delivered to), the model policy, the pipeline catalog
//! vocabulary, the GitHub watcher vocabulary, the `AgentBackend` port, and the
//! Archon daemon wire protocol.
//! This crate has no I/O beyond reading configuration files; with the
//! `testing` feature, `testing` holds helpers for the workspace's tests.

pub mod aether_ids;
pub mod age;
pub mod agent_message;
pub mod attachment;
pub mod authority;
pub mod autonomy;
pub mod catalog;
pub mod config;
pub mod config_files;
pub mod cost;
pub mod crash;
pub mod error;
pub mod feedback;
pub mod github;
pub mod host_load;
pub mod id;
pub mod initiative;
pub mod integration;
pub mod lesson;
pub mod mcp_tools;
pub mod merge;
pub mod model;
pub mod model_evidence;
pub mod model_tier;
pub mod notify;
pub mod operator_commands;
pub mod ops;
pub mod outcome;
pub mod paths;
pub mod policy;
pub mod ports;
pub mod protocol;
pub mod provider_route;
pub mod pull_request;
pub mod push;
pub mod quarantine;
pub mod queue;
pub mod quota;
pub mod remote;
pub mod repo;
pub mod risk;
pub mod run;
pub mod settings;
pub mod steer;
#[cfg(feature = "testing")]
pub mod testing;
pub mod tracked;

pub use error::{ArchonError, Result};

/// RFC 3339 UTC timestamps everywhere; SQLite stores them as text.
pub type Timestamp = chrono::DateTime<chrono::Utc>;

pub fn now() -> Timestamp {
    chrono::Utc::now()
}
