//! Variants added to protocol 24's closed enums after its first release. A
//! protocol-24 client of an older release rejects a variant it does not
//! know, so archond sends one only to a client that names it in `hello`'s
//! `features`.

use super::graph::TaskEdgeKind;
use super::{ChangeSubject, Reply, ServerFrame};

/// The `hello` feature strings a client declares to receive later variants.
pub mod client_feature {
    /// `task_graph` may carry `onto` edges.
    pub const ONTO_EDGES: &str = "onto_edges";
    /// `changes_page` and the change stream may carry `pause` subjects.
    pub const PAUSE_CHANGES: &str = "pause_changes";
    /// Every one this release knows; its own clients declare them all.
    pub const ALL: [&str; 2] = [ONTO_EDGES, PAUSE_CHANGES];
}

/// What one connection's client declared it decodes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ClientFeatures {
    onto_edges: bool,
    pause_changes: bool,
}

impl ClientFeatures {
    /// From a `hello`'s `features`; names this release does not know are
    /// ignored, since a newer client may declare more.
    pub fn declared(features: &[String]) -> Self {
        let has = |name: &str| features.iter().any(|f| f == name);
        Self {
            onto_edges: has(client_feature::ONTO_EDGES),
            pause_changes: has(client_feature::PAUSE_CHANGES),
        }
    }

    /// A client of this release, which decodes every variant.
    pub fn all() -> Self {
        Self {
            onto_edges: true,
            pause_changes: true,
        }
    }

    /// `frame` as this client can decode it, or `None` for a change frame
    /// it must not be sent.
    pub fn adapt(self, frame: ServerFrame) -> Option<ServerFrame> {
        match frame {
            ServerFrame::Change { change, .. } if !self.admits(&change.subject) => None,
            ServerFrame::Reply {
                id,
                result: Ok(reply),
            } => Some(ServerFrame::Reply {
                id,
                result: Ok(self.adapt_reply(reply)),
            }),
            other => Some(other),
        }
    }

    /// `reply` without the variants this client did not declare. A page
    /// keeps its `last_seq`, so a cursor still moves past a dropped change.
    pub fn adapt_reply(self, reply: Reply) -> Reply {
        match reply {
            Reply::TaskGraph(mut graph) if !self.onto_edges => {
                graph.edges.retain(|edge| edge.kind != TaskEdgeKind::Onto);
                Reply::TaskGraph(graph)
            }
            Reply::Changes {
                mut changes,
                last_seq,
            } if !self.pause_changes => {
                assert!(
                    changes.last().is_none_or(|c| c.seq <= last_seq),
                    "a page's last_seq covers its changes"
                );
                changes.retain(|change| self.admits(&change.subject));
                Reply::Changes { changes, last_seq }
            }
            other => other,
        }
    }

    fn admits(self, subject: &ChangeSubject) -> bool {
        self.pause_changes || !matches!(subject, ChangeSubject::Pause { .. })
    }
}

#[cfg(test)]
#[path = "compat_tests.rs"]
mod tests;
