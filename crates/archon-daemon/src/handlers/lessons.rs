//! Lessons: recording them, the operator's edits, and what they did.

use archon_core::error::ArchonError;
use archon_core::id::{LessonId, QueueItemId};
use archon_core::lesson::{LessonDraft, LessonPatch, LessonQuery};
use archon_core::protocol::{ClientKind, Reply, RepoRef};

use super::not_implemented;
use crate::context::DaemonContext;

/// Replies `Reply::Lesson`. `client` tells the operator's CLI from an agent.
pub(super) async fn record(
    _ctx: &DaemonContext,
    _client: ClientKind,
    _draft: LessonDraft,
) -> Result<Reply, ArchonError> {
    Err(not_implemented("record_lesson"))
}

/// Replies `Reply::Lessons`.
pub(super) async fn list(_ctx: &DaemonContext, _query: &LessonQuery) -> Result<Reply, ArchonError> {
    Err(not_implemented("lessons"))
}

/// Replies `Reply::Lesson` with the lesson as the operator left it.
pub(super) async fn update(
    _ctx: &DaemonContext,
    _lesson: &LessonId,
    _patch: LessonPatch,
) -> Result<Reply, ArchonError> {
    Err(not_implemented("update_lesson"))
}

/// Replies `Reply::LessonsWhy`.
pub(super) async fn why(_ctx: &DaemonContext, _item: &QueueItemId) -> Result<Reply, ArchonError> {
    Err(not_implemented("lessons_why"))
}

/// Replies `Reply::LessonStats`.
pub(super) async fn stats(
    _ctx: &DaemonContext,
    _repo: Option<&RepoRef>,
) -> Result<Reply, ArchonError> {
    Err(not_implemented("lesson_stats"))
}
