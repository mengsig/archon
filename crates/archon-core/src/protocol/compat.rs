//! Variants added to protocol 24's closed enums after its first release. A
//! protocol-24 client of an older release rejects a variant it does not
//! know, so archond sends one only to a client that names it in `hello`'s
//! `features`.

use super::graph::TaskEdgeKind;
use super::{Reply, Request, ServerFrame};

/// The `hello` feature strings a client declares to receive later variants.
pub mod client_feature {
    /// `task_graph` may carry `onto` edges.
    pub const ONTO_EDGES: &str = "onto_edges";
    /// Every one this release knows; its own clients declare them all.
    pub const ALL: [&str; 1] = [ONTO_EDGES];
}

/// What one connection's client declared it decodes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ClientFeatures {
    onto_edges: bool,
}

impl ClientFeatures {
    /// From a `hello`'s `features`; names this release does not know are
    /// ignored, since a newer client may declare more.
    pub fn declared(features: &[String]) -> Self {
        Self {
            onto_edges: features.iter().any(|f| f == client_feature::ONTO_EDGES),
        }
    }

    /// A client of this release, which decodes every variant.
    pub fn all() -> Self {
        Self { onto_edges: true }
    }

    /// `frame` as this client can decode it. Change frames and pages pass
    /// unchanged: no change subject is gated.
    pub fn adapt(self, frame: ServerFrame) -> ServerFrame {
        match frame {
            ServerFrame::Reply {
                id,
                result: Ok(reply),
            } => ServerFrame::Reply {
                id,
                result: Ok(self.adapt_reply(reply)),
            },
            other => other,
        }
    }

    /// `request` as this client means it: a client that never sees `onto`
    /// edges gets a component joined without them too.
    pub fn adapt_request(self, request: Request) -> Request {
        match request {
            Request::TaskGraph { mut filter, limit } if !self.onto_edges => {
                filter.without_onto = true;
                Request::TaskGraph { filter, limit }
            }
            other => other,
        }
    }

    /// `reply` without the variants this client did not declare. A task
    /// graph is one whole reply, so dropping an edge breaks no cursor.
    pub fn adapt_reply(self, reply: Reply) -> Reply {
        match reply {
            Reply::TaskGraph(mut graph) if !self.onto_edges => {
                graph.edges.retain(|edge| edge.kind != TaskEdgeKind::Onto);
                Reply::TaskGraph(graph)
            }
            other => other,
        }
    }
}

#[cfg(test)]
#[path = "compat_tests.rs"]
mod tests;
