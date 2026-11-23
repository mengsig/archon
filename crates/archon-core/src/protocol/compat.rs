//! Variants added to protocol 24's closed enums after its first release. A
//! protocol-24 client of an older release rejects a variant it does not
//! know, so archond sends one only to a client that names it in `hello`'s
//! `features`.

use super::graph::TaskEdgeKind;
use super::{Change, ChangeSubject, ItemSummary, Reply, Request, ServerFrame, WaitingReasonCode};

/// The `hello` feature strings a client declares to receive later variants.
pub mod client_feature {
    /// `task_graph` may carry `onto` edges.
    pub const ONTO_EDGES: &str = "onto_edges";
    /// Waiting reasons may name an unavailable catalog directly.
    pub const MODEL_CATALOG_WAIT: &str = "model_catalog_wait";
    /// A successful enqueue may return advice alongside its item.
    pub use crate::protocol::feature::ENQUEUE_ADVICE;
    /// Every feature this release's clients decode.
    pub const ALL: [&str; 3] = [ONTO_EDGES, MODEL_CATALOG_WAIT, ENQUEUE_ADVICE];
}

/// What one connection's client declared it decodes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ClientFeatures {
    onto_edges: bool,
    model_catalog_wait: bool,
    enqueue_advice: bool,
}

impl ClientFeatures {
    /// From a `hello`'s `features`; names this release does not know are
    /// ignored, since a newer client may declare more.
    pub fn declared(features: &[String]) -> Self {
        Self {
            model_catalog_wait: features
                .iter()
                .any(|f| f == client_feature::MODEL_CATALOG_WAIT),
            onto_edges: features.iter().any(|f| f == client_feature::ONTO_EDGES),
            enqueue_advice: features.iter().any(|f| f == client_feature::ENQUEUE_ADVICE),
        }
    }

    /// A client of this release, which decodes every variant.
    pub fn all() -> Self {
        Self {
            onto_edges: true,
            model_catalog_wait: true,
            enqueue_advice: true,
        }
    }

    /// Adapt streamed changes and replies to the features the client declared.
    pub fn adapt(self, frame: ServerFrame) -> ServerFrame {
        match frame {
            ServerFrame::Reply {
                id,
                result: Ok(reply),
            } => ServerFrame::Reply {
                id,
                result: Ok(self.adapt_reply(reply)),
            },
            ServerFrame::Change { sub, mut change } => {
                self.adapt_change(&mut change);
                ServerFrame::Change { sub, change }
            }
            other => other,
        }
    }

    fn adapt_code(self, code: &mut WaitingReasonCode) {
        if !self.model_catalog_wait && *code == WaitingReasonCode::ModelCatalogUnavailable {
            *code = WaitingReasonCode::AetherDisconnected;
        }
    }

    fn adapt_change(self, change: &mut Change) {
        if let ChangeSubject::Item {
            waiting_reason: Some(code),
            ..
        } = &mut change.subject
        {
            self.adapt_code(code);
        }
    }

    fn adapt_rows(self, rows: &mut [ItemSummary]) {
        for reason in rows
            .iter_mut()
            .filter_map(|row| row.waiting_reason.as_mut())
        {
            self.adapt_code(&mut reason.code);
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
    pub fn adapt_reply(self, mut reply: Reply) -> Reply {
        if self == Self::all() {
            return reply;
        }
        if !self.enqueue_advice
            && let Reply::EnqueueAdvised {
                item, budgets_off, ..
            } = reply
        {
            return if budgets_off {
                Reply::ItemBudgetsOff(*item)
            } else {
                Reply::Item(*item)
            };
        }
        match &mut reply {
            Reply::TaskGraph(graph) => {
                if !self.onto_edges {
                    graph.edges.retain(|edge| edge.kind != TaskEdgeKind::Onto);
                }
                for code in graph
                    .nodes
                    .iter_mut()
                    .filter_map(|node| node.waiting_reason.as_mut())
                {
                    self.adapt_code(code);
                }
            }
            Reply::Status(status) => self.adapt_rows(&mut status.waiting_items),
            Reply::ItemSummaries(rows)
            | Reply::ItemSummariesAt {
                summaries: rows, ..
            } => self.adapt_rows(rows),
            Reply::ItemDetail(detail) => {
                if let Some(reason) = &mut detail.waiting_reason {
                    self.adapt_code(&mut reason.code);
                }
            }
            Reply::Changes { changes, .. } => {
                for change in changes {
                    self.adapt_change(change);
                }
            }
            _ => {}
        }
        reply
    }
}

#[cfg(test)]
#[path = "compat_tests.rs"]
mod tests;
