//! The SQLite store (ARCHITECTURE §3, §16). Only `archond` opens it. WAL
//! mode, `busy_timeout = 5s`, foreign keys on, forward-only migrations
//! tracked in `schema_migrations`. Each record is kept whole as JSON in a
//! `data` column; the columns that are queried and filtered are derived
//! from it by the same write, so the two never disagree.
//!
//! Every write runs in one `BEGIN IMMEDIATE` transaction and checks its
//! preconditions (legal state edges via `QueueState::can_transition_to`,
//! finished runs staying finished, stage runs only moving forward) before
//! the first statement, so a refused change writes nothing.

mod codec;
mod cost;
mod delegated;
mod directives;
mod feed;
mod gates;
pub mod initiatives;
mod items;
mod model_evidence;
mod repos;
mod run_edits;
mod runs;
mod schema;
mod settings;
mod snapshot;
mod spend;
pub mod stacks;

use std::path::Path;

use archon_core::Timestamp;
use archon_core::aether_ids::{AetherAgentId, AetherQuestionRef};
use archon_core::autonomy::{AutonomyPatch, Directive};
use archon_core::cost::{CostGroup, CostReport, DayRange};
use archon_core::error::ArchonError;
use archon_core::id::{
    DirectiveId, PipelineRunId, QuestionId, QueueItemId, RepoId, ReportId, StageRunId,
};
use archon_core::notify::{FeedEntry, FeedItem, Question};
use archon_core::protocol::{ItemQuery, RepoRef};
use archon_core::queue::{PauseState, QueueItem, QueueState};
use archon_core::repo::Repo;
use archon_core::run::{OperatorGate, PipelineRun, StageRun};
use archon_core::steer::RunEdit;

use crate::dispatch::DispatchSnapshot;

pub use model_evidence::ModelEvidenceSnapshot;
pub use run_edits::RecordedEdit;

/// Well-known `kv` keys.
pub mod keys {
    pub const ORCHESTRATOR_AGENT: &str = "orchestrator_agent";
    pub const ORCHESTRATOR_GENERATION: &str = "orchestrator_generation";
    pub const CURSOR_ORCHESTRATOR: &str = "aether_cursor_orchestrator";
    pub const CURSOR_FLEET: &str = "aether_cursor_fleet";
    pub const AETHER_INSTANCE: &str = "aether_instance_id";
}

/// Latest absolute usage for one Aether agent (`agent.usage_updated`).
#[derive(Debug, Clone, PartialEq)]
pub struct SpendRow {
    pub agent: AetherAgentId,
    pub item: Option<QueueItemId>,
    /// Raw Aether amounts retained for budget accounting, independent of basis.
    pub cost_usd: f64,
    pub cost_basis: archon_core::cost::CostBasis,
    /// Aether's word that no invocation is missing from `cost_usd`; `None`
    /// when it did not say.
    pub cost_complete: Option<bool>,
    pub estimated_cost_usd: f64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub at: Timestamp,
}

/// What [`Store::finish_run`] committed: the settled item, and the feed
/// answers that closed the questions of the gates it withdrew.
#[derive(Debug, Clone, PartialEq)]
pub struct FinishedRun {
    pub item: QueueItem,
    pub answers: Vec<FeedItem>,
}

#[derive(Debug)]
pub struct Store {
    conn: rusqlite::Connection,
}

impl Store {
    /// Opens (creating if needed) the on-disk store and brings its schema up
    /// to date. Refuses a store written by a newer Archon.
    pub fn open(path: &Path) -> Result<Self, ArchonError> {
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir)
                .map_err(|e| ArchonError::io(format!("create {}", dir.display()), e))?;
        }
        let conn = rusqlite::Connection::open(path)
            .map_err(|e| codec::store_error(&format!("open {}", path.display()), &e))?;
        Self::init(conn, true)
    }

    /// Opens the on-disk store read-only, as `archon gc --offline` reads it
    /// with archond stopped: nothing is created or migrated, and a store
    /// of another schema version is refused. Without a `-wal` file, as a
    /// writer that closed cleanly leaves it, the store is opened immutable,
    /// creating no `-wal` or `-shm` on a disk that may be full: the caller
    /// must keep writers out meanwhile (the daemon lock does).
    pub fn open_read_only(path: &Path) -> Result<Self, ArchonError> {
        use rusqlite::OpenFlags;
        let opening = |e: &rusqlite::Error| {
            codec::store_error(&format!("open {} read-only", path.display()), e)
        };
        let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX;
        let mut wal = path.as_os_str().to_owned();
        wal.push("-wal");
        let conn = if Path::new(&wal).exists() {
            rusqlite::Connection::open_with_flags(path, flags)
        } else {
            let uri = schema::immutable_uri(path)
                .map_err(|e| ArchonError::io(format!("resolving {}", path.display()), e))?;
            rusqlite::Connection::open_with_flags(uri, flags | OpenFlags::SQLITE_OPEN_URI)
        };
        let conn = conn.map_err(|e| opening(&e))?;
        schema::check_read_only(&conn)?;
        Ok(Self { conn })
    }

    pub fn open_in_memory() -> Result<Self, ArchonError> {
        let conn = rusqlite::Connection::open_in_memory()
            .map_err(|e| ArchonError::Store(format!("open in-memory store: {e}")))?;
        Self::init(conn, false)
    }

    fn init(mut conn: rusqlite::Connection, on_disk: bool) -> Result<Self, ArchonError> {
        schema::configure(&conn, on_disk)?;
        let version = schema::migrate(&mut conn)?;
        debug_assert_eq!(version, schema::MIGRATIONS.len());
        Ok(Self { conn })
    }

    // ----- repositories ----------------------------------------------------
    /// Names and paths are unique; the name must pass `Repo::valid_name`.
    pub fn insert_repo(&mut self, repo: &Repo) -> Result<(), ArchonError> {
        repos::insert(&mut self.conn, repo)
    }
    /// By `repo_…` id, then by name; `NotFound` otherwise.
    pub fn repo(&self, reference: &RepoRef) -> Result<Repo, ArchonError> {
        repos::find(&self.conn, reference)
    }
    /// Ordered by name.
    pub fn repos(&self) -> Result<Vec<Repo>, ArchonError> {
        repos::all(&self.conn)
    }
    pub fn update_repo(&mut self, repo: &Repo) -> Result<(), ArchonError> {
        repos::update(&mut self.conn, repo)
    }
    /// Refused while the repository has non-terminal items. It also leaves
    /// the pause state.
    pub fn remove_repo(&mut self, id: &RepoId) -> Result<(), ArchonError> {
        repos::remove(&mut self.conn, id)
    }

    // ----- queue items -----------------------------------------------------
    /// Returns the existing item when `idempotency_key` was used before with
    /// the same request, and refuses a key reused for a different request.
    /// A new item must be `queued`; its repository and dependencies must
    /// exist, and the dependencies must not form a cycle.
    ///
    /// Inserts as if every knob of `item` was asked for, so a key from before
    /// autonomy existed never matches; enqueues use [`Self::insert_enqueued`].
    pub fn insert_item(
        &mut self,
        item: &QueueItem,
        idempotency_key: Option<&str>,
    ) -> Result<QueueItem, ArchonError> {
        let asked = AutonomyPatch::from(item.autonomy);
        items::insert(&mut self.conn, item, idempotency_key, &asked)
    }
    /// [`Self::insert_item`] for an enqueue that asked for the knobs `asked`.
    /// A key used before autonomy existed matches a retry that asks for none.
    pub fn insert_enqueued(
        &mut self,
        item: &QueueItem,
        idempotency_key: Option<&str>,
        asked: &AutonomyPatch,
    ) -> Result<QueueItem, ArchonError> {
        items::insert(&mut self.conn, item, idempotency_key, asked)
    }
    pub fn item(&self, id: &QueueItemId) -> Result<QueueItem, ArchonError> {
        items::load(&self.conn, id)
    }
    /// Newest first, at most `limit` (default 50) items.
    pub fn items(&self, query: &ItemQuery) -> Result<Vec<QueueItem>, ArchonError> {
        items::list(&self.conn, query)
    }
    /// Items whose id starts with a checked `qi_…` prefix, oldest first, at
    /// most `limit`.
    pub fn items_with_id_prefix(
        &self,
        prefix: &str,
        limit: usize,
    ) -> Result<Vec<QueueItem>, ArchonError> {
        items::with_id_prefix(&self.conn, prefix, limit)
    }
    /// Apply a checked transition; returns the updated item. Runs named by
    /// the next state must belong to the item, and an item with a live run
    /// can be neither re-queued nor settled (use [`Store::finish_run`]).
    pub fn transition_item(
        &mut self,
        id: &QueueItemId,
        next: QueueState,
        at: Timestamp,
    ) -> Result<QueueItem, ArchonError> {
        items::transition(&mut self.conn, id, next, at)
    }
    /// Non-state fields only; repository, pipeline and `created_at` are
    /// fixed. Changed dependencies are checked like on insert.
    pub fn update_item(&mut self, item: &QueueItem) -> Result<(), ArchonError> {
        items::update(&mut self.conn, item)
    }
    /// The repositories of items in state `kind` (a `QueueState::kind()`
    /// name), each once; `None` for items without a repository.
    pub fn item_repos(&self, kind: &str) -> Result<Vec<Option<RepoId>>, ArchonError> {
        items::repos_in_state(&self.conn, kind)
    }
    /// Items that depend on `id` (for blocking and unblocking), oldest first.
    pub fn dependents(&self, id: &QueueItemId) -> Result<Vec<QueueItemId>, ArchonError> {
        items::dependents(&self.conn, id)
    }
    /// Crash recovery: `dispatching` items without a live run go back to
    /// `queued` (ARCHITECTURE §16). Returns them oldest first.
    pub fn requeue_orphaned_dispatching(
        &mut self,
        at: Timestamp,
    ) -> Result<Vec<QueueItemId>, ArchonError> {
        items::requeue_orphaned_dispatching(&mut self.conn, at)
    }

    // ----- pipeline and stage runs ------------------------------------------
    /// At most one non-terminal run per item, and only for a `dispatching`
    /// item of the same pipeline.
    pub fn insert_run(&mut self, run: &PipelineRun) -> Result<(), ArchonError> {
        runs::insert(&mut self.conn, run)
    }
    /// A finished run cannot change state again, and a live run finishes
    /// only through [`Store::finish_run`].
    pub fn update_run(&mut self, run: &PipelineRun) -> Result<(), ArchonError> {
        runs::update(&mut self.conn, run)
    }
    /// Finish a live run, withdraw its open gates, close their questions with
    /// feed answers, and settle its item to the terminal `next` in one
    /// transaction. An item with a live run cannot be settled any other way.
    pub fn finish_run(
        &mut self,
        run: &PipelineRun,
        next: QueueState,
        at: Timestamp,
    ) -> Result<FinishedRun, ArchonError> {
        runs::finish(&mut self.conn, run, next, at)
    }
    pub fn run(&self, id: &PipelineRunId) -> Result<PipelineRun, ArchonError> {
        runs::load(&self.conn, id)
    }
    /// Oldest first.
    pub fn runs_for_item(&self, item: &QueueItemId) -> Result<Vec<PipelineRun>, ArchonError> {
        runs::for_item(&self.conn, item)
    }
    /// Non-terminal runs, oldest first.
    pub fn active_runs(&self) -> Result<Vec<PipelineRun>, ArchonError> {
        runs::active(&self.conn)
    }
    /// New stage runs join a live run. Existing ones only move forward
    /// (`spawning` → `running` → terminal) and keep their identity and
    /// Aether ids once set.
    pub fn upsert_stage_run(&mut self, stage: &StageRun) -> Result<(), ArchonError> {
        runs::upsert_stage(&mut self.conn, stage)
    }
    /// Ordered by started_at, attempt, fan_index.
    pub fn stage_runs(&self, run: &PipelineRunId) -> Result<Vec<StageRun>, ArchonError> {
        runs::stages_of(&self.conn, run)
    }
    /// All of an item's stage runs in one read: its runs oldest first, and
    /// each run's stage runs ordered as [`Store::stage_runs`] orders them.
    pub fn stage_runs_for_item(&self, item: &QueueItemId) -> Result<Vec<StageRun>, ArchonError> {
        runs::stages_of_item(&self.conn, item)
    }
    pub fn stage_run_by_agent(
        &self,
        agent: &AetherAgentId,
    ) -> Result<Option<StageRun>, ArchonError> {
        runs::stage_by_agent(&self.conn, agent)
    }
    pub fn stage_run(&self, id: &StageRunId) -> Result<StageRun, ArchonError> {
        runs::stage(&self.conn, id)
    }
    /// Every `spawning` or `running` stage run, including those of finished
    /// runs, for crash recovery. Oldest first.
    pub fn live_stage_runs(&self) -> Result<Vec<StageRun>, ArchonError> {
        runs::live_stages(&self.conn)
    }

    // ----- run edits ---------------------------------------------------------
    /// Record a steer's change to a live run's shape; returns its 1-based
    /// place in the run's order.
    pub fn append_run_edit(
        &mut self,
        run: &PipelineRunId,
        edit: &RunEdit,
        at: Timestamp,
    ) -> Result<u32, ArchonError> {
        run_edits::append(&mut self.conn, run, edit, at)
    }
    /// A run's edits in the order they were made.
    pub fn run_edits(&self, run: &PipelineRunId) -> Result<Vec<RecordedEdit>, ArchonError> {
        run_edits::for_run(&self.conn, run)
    }

    // ----- dispatch ----------------------------------------------------------
    /// Caps, repo caps, pending spawns and first-stage fan-out are filled in
    /// by the daemon. Spend counts estimates; a daemon configured with
    /// `count_estimated_cost = false` replaces it from [`Store::spend_totals`].
    pub fn dispatch_snapshot(&self, now: Timestamp) -> Result<DispatchSnapshot, ArchonError> {
        snapshot::build(&self.conn, &now)
    }
    pub fn pause_state(&self) -> Result<PauseState, ArchonError> {
        settings::pause(&self.conn)
    }
    /// Paused repositories must exist; lists must not repeat an entry.
    pub fn set_pause_state(&mut self, pause: &PauseState) -> Result<(), ArchonError> {
        settings::set_pause(&mut self.conn, pause)
    }

    // ----- feed and questions -------------------------------------------------
    /// Appends with the next gapless `seq`.
    pub fn append_feed(
        &mut self,
        entry: FeedEntry,
        at: Timestamp,
    ) -> Result<FeedItem, ArchonError> {
        feed::append(&mut self.conn, entry, at)
    }

    /// Atomically record a feed entry and the durable state delivering it.
    pub fn append_feed_with_kv(
        &mut self,
        entry: FeedEntry,
        at: Timestamp,
        updates: &[(&str, &str)],
    ) -> Result<FeedItem, ArchonError> {
        feed::append_with_kv(&mut self.conn, entry, at, updates)
    }
    /// Every item after `after`, oldest first, hidden ones included.
    pub fn feed_after(&self, after: u64, limit: usize) -> Result<Vec<FeedItem>, ArchonError> {
        feed::after(&self.conn, after, limit)
    }
    /// The newest `limit` items before `before`: visible items only unless
    /// verbose, oldest first.
    pub fn feed_before(
        &self,
        before: Option<u64>,
        limit: usize,
        verbose: bool,
    ) -> Result<Vec<FeedItem>, ArchonError> {
        feed::before(&self.conn, before, limit, verbose)
    }
    /// The newest feed `seq`, 0 when the feed is empty.
    pub fn last_feed_seq(&self) -> Result<u64, ArchonError> {
        feed::last_seq(&self.conn)
    }
    /// Refuses a duplicate id and a second record of one Aether question.
    /// Returns the question with the permanent number it was given.
    pub fn insert_question(&mut self, question: &Question) -> Result<Question, ArchonError> {
        feed::insert_question(&mut self.conn, question)
    }
    /// Refuses an already answered question.
    pub fn answer_question(
        &mut self,
        id: &QuestionId,
        answer: &str,
        at: Timestamp,
    ) -> Result<Question, ArchonError> {
        feed::answer(&mut self.conn, id, answer, at)
    }
    /// `NotFound` for an unknown id.
    pub fn question(&self, id: &QuestionId) -> Result<Question, ArchonError> {
        feed::load_question(&self.conn, id)
    }
    /// Oldest first.
    pub fn questions(&self, open_only: bool) -> Result<Vec<Question>, ArchonError> {
        feed::list(&self.conn, open_only)
    }
    /// The question recorded for an Aether `ask_user`, for idempotent
    /// `question.asked` handling.
    pub fn question_by_aether(
        &self,
        aether: &AetherQuestionRef,
    ) -> Result<Option<Question>, ArchonError> {
        feed::by_aether(&self.conn, aether)
    }

    // ----- operator gates and directives --------------------------------------
    /// Opens a gate on its live run; a gate already recorded for the same
    /// `(run, stage_id, attempt)` is returned unchanged instead.
    pub fn open_gate(&mut self, gate: &OperatorGate) -> Result<OperatorGate, ArchonError> {
        gates::open(&mut self.conn, gate)
    }
    /// Identity and subject are fixed; a decision or withdrawal is final.
    pub fn update_gate(&mut self, gate: &OperatorGate) -> Result<(), ArchonError> {
        gates::update(&mut self.conn, gate)
    }
    pub fn gate(
        &self,
        run: &PipelineRunId,
        stage_id: &str,
        attempt: u32,
    ) -> Result<Option<OperatorGate>, ArchonError> {
        gates::find(&self.conn, run, stage_id, attempt)
    }
    /// Oldest first, like every gate list.
    pub fn gates_for_run(&self, run: &PipelineRunId) -> Result<Vec<OperatorGate>, ArchonError> {
        gates::for_run(&self.conn, run)
    }
    pub fn gates_for_item(&self, item: &QueueItemId) -> Result<Vec<OperatorGate>, ArchonError> {
        gates::for_item(&self.conn, item)
    }
    pub fn open_gates(&self) -> Result<Vec<OperatorGate>, ArchonError> {
        gates::open_all(&self.conn)
    }
    pub fn gate_by_question(
        &self,
        question: &QuestionId,
    ) -> Result<Option<OperatorGate>, ArchonError> {
        gates::by_question(&self.conn, question)
    }
    /// Crash recovery: close the questions still open on withdrawn gates,
    /// each with a feed answer, which is returned for broadcasting.
    pub fn close_withdrawn_questions(
        &mut self,
        at: Timestamp,
    ) -> Result<Vec<FeedItem>, ArchonError> {
        gates::close_withdrawn_questions(&mut self.conn, at)
    }
    /// The open gate showing `report`, if any.
    pub fn open_gate_by_report(
        &self,
        report: &ReportId,
    ) -> Result<Option<OperatorGate>, ArchonError> {
        gates::open_by_report(&self.conn, report)
    }
    /// Refused once `MAX_DIRECTIVES` are in force.
    pub fn insert_directive(&mut self, directive: &Directive) -> Result<(), ArchonError> {
        directives::insert(&mut self.conn, directive)
    }
    /// In force at `now`, oldest first.
    pub fn directives(&self, now: Timestamp) -> Result<Vec<Directive>, ArchonError> {
        directives::active(&self.conn, &now)
    }
    /// One directive, or all with `None`; returns how many were cleared.
    pub fn clear_directives(&mut self, id: Option<&DirectiveId>) -> Result<u32, ArchonError> {
        directives::clear(&mut self.conn, id)
    }

    // ----- spend ---------------------------------------------------------------
    /// Charge the agent's growth: usage events carry absolute totals. A replayed
    /// (older) row is ignored, and only a provider-settled total lowers a charge.
    pub fn record_spend(&mut self, row: &SpendRow) -> Result<(), ArchonError> {
        spend::record(&mut self.conn, row)
    }
    pub fn spend_for_item(&self, item: &QueueItemId, estimates: bool) -> Result<f64, ArchonError> {
        spend::for_item(&self.conn, item, estimates)
    }
    /// Run ownership persists with the spend ledger, beyond watcher retention.
    pub fn link_agent_run(
        &mut self,
        agent: &AetherAgentId,
        run: &PipelineRunId,
    ) -> Result<(), ArchonError> {
        spend::link_run(&self.conn, agent, run)
    }
    pub fn agent_run(&self, agent: &AetherAgentId) -> Result<Option<PipelineRunId>, ArchonError> {
        spend::agent_run(&self.conn, agent)
    }
    pub fn spend_for_run(&self, run: &PipelineRunId, estimates: bool) -> Result<f64, ArchonError> {
        spend::for_run(&self.conn, run, estimates)
    }
    /// What one agent has been charged (its stage's cost).
    pub fn spend_for_agent(
        &self,
        agent: &AetherAgentId,
        estimates: bool,
    ) -> Result<f64, ArchonError> {
        spend::for_agent(&self.conn, agent, estimates)
    }
    /// `parent` delegated to `agent`, on `model`; `effort` once it is known.
    pub fn record_delegated(
        &mut self,
        agent: &AetherAgentId,
        parent: &AetherAgentId,
        model: &archon_core::model::ModelRef,
        effort: Option<archon_core::model::Effort>,
    ) -> Result<(), ArchonError> {
        delegated::record(&self.conn, agent, parent, model, effort)
    }
    /// Every agent beneath `agent`, at any depth, with what each was charged.
    pub fn delegated_beneath(
        &self,
        agent: &AetherAgentId,
        estimates: bool,
    ) -> Result<Vec<archon_core::protocol::DelegatedAgent>, ArchonError> {
        delegated::beneath(&self.conn, agent, estimates)
    }
    /// `(today, total)`; "today" is the local calendar day of `now`.
    pub fn spend_totals(&self, now: Timestamp, estimates: bool) -> Result<(f64, f64), ArchonError> {
        spend::totals(&self.conn, &now, estimates)
    }
    /// On-demand attempt observations; the caller identifies verdict-bearing stages.
    pub fn model_evidence(
        &self,
        query: &archon_core::model_evidence::ModelEvidenceQuery,
        range: &DayRange,
        estimates: bool,
        mut is_verdict_stage: impl FnMut(&QueueItem, &str) -> Result<bool, ArchonError>,
    ) -> Result<archon_core::model_evidence::ModelEvidenceReport, ArchonError> {
        self.model_evidence_snapshot(query, range, estimates)?
            .report(|item, stage| is_verdict_stage(item, &stage.stage_id))
    }

    /// Read owned observations before releasing the store for file-based classification.
    pub fn model_evidence_snapshot(
        &self,
        query: &archon_core::model_evidence::ModelEvidenceQuery,
        range: &DayRange,
        estimates: bool,
    ) -> Result<ModelEvidenceSnapshot, ArchonError> {
        model_evidence::read(&self.conn, query, range, estimates)
    }

    /// Spend over the local days of `range` grouped `by`, counted as the
    /// budgets count it: with estimates when `estimates`. Refuses rows that
    /// do not add up to the total.
    pub fn cost_report(
        &self,
        range: &DayRange,
        by: CostGroup,
        estimates: bool,
    ) -> Result<CostReport, ArchonError> {
        cost::report(&self.conn, range, by, estimates)
    }

    // ----- kv ------------------------------------------------------------------
    pub fn kv_get(&self, key: &str) -> Result<Option<String>, ArchonError> {
        settings::kv_get(&self.conn, key)
    }
    pub fn kv_set(&mut self, key: &str, value: &str) -> Result<(), ArchonError> {
        settings::kv_set(&self.conn, key, value)
    }
    /// Every entry whose key starts with `prefix`, in key order.
    pub fn kv_prefix(&self, prefix: &str) -> Result<Vec<(String, String)>, ArchonError> {
        settings::kv_prefix(&self.conn, prefix)
    }
    /// Remove `key`; a missing key is fine.
    pub fn kv_delete(&mut self, key: &str) -> Result<(), ArchonError> {
        settings::kv_delete(&self.conn, key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pragma<T: rusqlite::types::FromSql>(store: &Store, name: &str) -> T {
        store
            .conn
            .pragma_query_value(None, name, |row| row.get(0))
            .unwrap()
    }

    #[test]
    fn on_disk_stores_use_wal_fks_busy_timeout_and_full_sync() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("archon.db")).unwrap();
        assert_eq!(pragma::<String>(&store, "journal_mode"), "wal");
        assert!(pragma::<bool>(&store, "foreign_keys"));
        assert_eq!(pragma::<i64>(&store, "busy_timeout"), 5000);
        assert_eq!(pragma::<i64>(&store, "synchronous"), 2, "FULL");
    }

    #[test]
    fn a_read_only_open_refuses_a_store_of_another_version() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("archon.db");
        let store = Store::open(&path).unwrap();
        Store::open_read_only(&path).unwrap();
        let latest = schema::MIGRATIONS.len() as i64;
        store
            .conn
            .execute("DELETE FROM schema_migrations WHERE version = ?1", [latest])
            .unwrap();
        let refused = Store::open_read_only(&path).unwrap_err().to_string();
        assert!(refused.contains("schema version"), "{refused}");
    }

    #[test]
    fn foreign_keys_back_up_the_store_checks() {
        let store = Store::open_in_memory().unwrap();
        assert!(pragma::<bool>(&store, "foreign_keys"));
        let orphan = store.conn.execute(
            "INSERT INTO item_deps (item, depends_on) VALUES ('qi_a', 'qi_b')",
            [],
        );
        assert!(orphan.is_err(), "dangling dependency rows must be refused");
    }
}
