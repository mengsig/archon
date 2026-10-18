//! A Rust client for the Aether daemon protocol (v23, falling back to v14).
//!
//! Aether ships Python and TypeScript clients only; this crate is Archon's
//! own, covering the commands and events in ARCHITECTURE §4. `wire` holds the
//! frame shapes, `events` the narrowed event vocabulary, `client` the
//! connection, `link` the reconnecting handle, `mcp` an MCP server's health,
//! `github` the GitHub watcher's config, `memory` the curation of one named
//! memory, and `backend` the `AgentBackend` implementation.
//! With the `fake` feature, `fake` is a scripted in-process `aetherd` for tests.

pub mod backend;
pub mod client;
mod connection;
pub mod events;
#[cfg(feature = "fake")]
pub mod fake;
pub mod github;
pub mod link;
pub mod mcp;
pub mod memory;
pub mod memory_feedback;
mod resumed;
pub mod wire;

pub use backend::AetherBackend;
pub use client::{AetherClient, ClientOptions, Endpoint, Subscription};
pub use events::{AetherEvent, EventKind};
pub use link::AetherLink;
