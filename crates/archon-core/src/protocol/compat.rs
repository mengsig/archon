//! Protect older clients from variants their closed enums cannot decode.

use super::graph::TaskEdgeKind;
use super::{
    Change, ChangeSubject, DESIGN_PROTOCOL_VERSION, ErrorCode, ErrorReply,
    FIRST_FEATURE_PROTOCOL_VERSION, ItemSummary, Reply, Request, ServerFrame, WaitingReasonCode,
};
use crate::autonomy::{AutonomyPatch, AutonomyPolicy, ProposalMode};
use crate::catalog::{StageKind, StageSummary};
use crate::repo::Repo;
use crate::settings::OperatorSettings;

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
    legacy_design_types: bool,
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
            legacy_design_types: false,
        }
    }

    /// A client of this release, which decodes every variant.
    pub fn all() -> Self {
        Self {
            onto_edges: true,
            model_catalog_wait: true,
            enqueue_advice: true,
            legacy_design_types: false,
        }
    }

    /// The variants understood at the version accepted in the welcome.
    pub fn for_protocol(version: u32, declared: &[String]) -> Self {
        let mut client = if version >= FIRST_FEATURE_PROTOCOL_VERSION {
            Self::declared(declared)
        } else {
            Self::default()
        };
        client.legacy_design_types = version < DESIGN_PROTOCOL_VERSION;
        client
    }

    /// Adapt streamed changes and replies to the features the client declared.
    pub fn adapt(self, frame: ServerFrame) -> ServerFrame {
        match frame {
            ServerFrame::Reply { id, result } => ServerFrame::Reply {
                id,
                result: result
                    .map(|reply| self.adapt_reply(reply))
                    .map_err(|error| self.adapt_error(error)),
            },
            ServerFrame::Change { sub, mut change } => {
                self.adapt_change(&mut change);
                ServerFrame::Change { sub, change }
            }
            other => other,
        }
    }

    pub fn adapt_error(self, mut error: ErrorReply) -> ErrorReply {
        if self.legacy_design_types {
            error.code = match error.code {
                ErrorCode::Gone => ErrorCode::NotFound,
                ErrorCode::ReadOnly => ErrorCode::Forbidden,
                ErrorCode::Finishing | ErrorCode::Stale | ErrorCode::Conflict => {
                    ErrorCode::InvalidArgument
                }
                other => other,
            };
        }
        error
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
    pub fn adapt_request(self, mut request: Request) -> Request {
        let mut inner = &mut request;
        while let Request::Tracked { request, .. } | Request::ForAgent { request, .. } = inner {
            inner = request;
        }
        if let Request::SetSettings {
            legacy_proposals, ..
        } = inner
        {
            *legacy_proposals = self.legacy_design_types;
        }
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
        if self.legacy_design_types {
            adapt_design_types(&mut reply);
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
            Reply::RequestStatus(status) => {
                if let Some(reply) = status.reply.take() {
                    status.reply = Some(Box::new(self.adapt_reply(*reply)));
                }
                status.error = status.error.take().map(|error| self.adapt_error(error));
            }
            _ => {}
        }
        reply
    }
}

fn adapt_design_types(reply: &mut Reply) {
    match reply {
        Reply::Item(item) | Reply::ItemBudgetsOff(item) => {
            item.autonomy.proposal = legacy_proposal(item.autonomy.proposal);
        }
        Reply::EnqueueAdvised { item, .. } => {
            item.autonomy.proposal = legacy_proposal(item.autonomy.proposal);
        }
        Reply::Items(items) => items.iter_mut().for_each(|item| {
            item.autonomy.proposal = legacy_proposal(item.autonomy.proposal);
        }),
        Reply::ItemSummaries(rows)
        | Reply::ItemSummariesAt {
            summaries: rows, ..
        } => {
            legacy_rows(rows);
        }
        Reply::ItemDetail(detail) => {
            detail.item.autonomy.proposal = legacy_proposal(detail.item.autonomy.proposal);
            for stage in &mut detail.stages {
                stage.kind = stage.kind.map(legacy_stage_kind);
            }
        }
        Reply::Status(status) => {
            legacy_rows(&mut status.waiting_items);
            legacy_policy(&mut status.autonomy);
        }
        Reply::Autonomy(view) => legacy_policy(&mut view.policy),
        Reply::Repo(repo) => legacy_repo(repo),
        Reply::Repos(repos) => repos.iter_mut().for_each(legacy_repo),
        Reply::Settings(view) => {
            legacy_settings(&mut view.scoped);
            legacy_settings(&mut view.global);
            legacy_policy(&mut view.effective.autonomy);
        }
        Reply::Initiative(detail) => {
            for item in &mut detail.members {
                item.autonomy.proposal = legacy_proposal(item.autonomy.proposal);
            }
        }
        Reply::Catalog(catalog) => {
            for pipeline in &mut catalog.pipelines {
                legacy_stages(&mut pipeline.stages);
            }
        }
        Reply::RunPlan(plan) => legacy_stages(&mut plan.stages),
        _ => {}
    }
}

fn legacy_proposal(proposal: ProposalMode) -> ProposalMode {
    match proposal {
        ProposalMode::Live => ProposalMode::Lavish,
        known => known,
    }
}

fn legacy_stage_kind(kind: StageKind) -> StageKind {
    match kind {
        StageKind::Design => StageKind::Propose,
        known => known,
    }
}

fn legacy_policy(policy: &mut AutonomyPolicy) {
    policy.default.proposal = legacy_proposal(policy.default.proposal);
    policy.ceiling.proposal = legacy_proposal(policy.ceiling.proposal);
}

fn legacy_patch(patch: &mut AutonomyPatch) {
    patch.proposal = patch.proposal.map(legacy_proposal);
}

fn legacy_settings(settings: &mut OperatorSettings) {
    legacy_patch(&mut settings.autonomy_default);
    legacy_patch(&mut settings.autonomy_max);
}

fn legacy_repo(repo: &mut Repo) {
    legacy_patch(&mut repo.autonomy_max);
    // Protocols 23 and 24 reject unknown fields in RepoTiers.
    repo.tiers.design = Default::default();
}

fn legacy_rows(rows: &mut [ItemSummary]) {
    for row in rows {
        row.item.autonomy.proposal = legacy_proposal(row.item.autonomy.proposal);
    }
}

fn legacy_stages(stages: &mut [StageSummary]) {
    for stage in stages {
        stage.kind = legacy_stage_kind(stage.kind);
    }
}

#[cfg(test)]
#[path = "compat_tests.rs"]
mod tests;
