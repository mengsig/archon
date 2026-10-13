//! Arguments of `record_lesson`.

use archon_core::lesson::{LessonDraft, checked_text};
use archon_core::protocol::Request;

use super::{Args, ToolError};

pub(super) fn record_lesson(args: &mut Args) -> Result<Request, ToolError> {
    let text = args.required_text("text")?;
    let text = checked_text(&text).map_err(|e| args.error("text", e))?;
    Ok(Request::RecordLesson(LessonDraft {
        kind: args.required("kind")?,
        text,
        repo: args.optional("repo")?,
        pipeline: args.optional_text("pipeline")?,
        item: args.optional("item")?,
    }))
}
