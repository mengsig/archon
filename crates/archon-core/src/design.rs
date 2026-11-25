//! Live design sessions: the records, limits and pure rules the daemon, the
//! stores and the web app share. Nothing here does I/O.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::aether_ids::AetherAgentId;
use crate::error::ArchonError;
use crate::id::{PipelineRunId as RunId, QueueItemId};

pub use crate::id::{DesignAssetId, DesignSessionId};

/// Where a design session is in its life.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SessionState {
    /// The preview is being set up.
    Starting,
    /// The operator may point and comment.
    Live,
    /// The operator is reviewing the final capture.
    Finishing,
    /// The operator confirmed; the finish message is sent.
    Confirmed,
    /// Over.
    Ended,
}

/// What a note asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum NoteKind {
    /// A change to one element or region.
    Change,
    /// Archond's own undo of an earlier note; never sent to the agent.
    Revert,
    /// A change to the page as a whole.
    Page,
}

/// Where a note is. There is deliberately no "undone": an undone note stays
/// `Done` and is derived undone from the revert note naming it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum NoteStatus {
    /// Waiting its turn.
    Queued,
    /// Delivered to the agent.
    Sent,
    /// The agent is on it.
    Working,
    /// The agent asked the operator a question.
    NeedsReply,
    /// Finished.
    Done,
    /// Did not finish; see [`FailReason`].
    Failed,
    /// Withdrawn.
    Cancelled,
}

/// Why a note failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum FailReason {
    /// The branch moved under the note.
    HistoryChanged,
    /// A later change touches the same lines.
    UndoConflict,
    /// The daemon stopped in the middle of an undo.
    UndoInterrupted,
    /// An undo that git refused.
    UndoRefused,
    /// The agent reported failure.
    Agent,
    /// The agent could not be reached.
    NotDelivered,
}

/// The size the preview is drawn at.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ViewportPreset {
    /// 1440x900.
    Desktop,
    /// 834x1112, touch.
    Tablet,
    /// 390x844, touch.
    Phone,
}

impl ViewportPreset {
    /// The CSS pixel size `(width, height)`.
    pub fn size(self) -> (u32, u32) {
        match self {
            Self::Desktop => (1440, 900),
            Self::Tablet => (834, 1112),
            Self::Phone => (390, 844),
        }
    }

    /// Whether the preset emulates touch.
    pub fn touch(self) -> bool {
        matches!(self, Self::Tablet | Self::Phone)
    }
}

/// Where a stored picture came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AssetSource {
    /// A crop of the live page.
    Capture,
    /// A drawing over a capture.
    Sketch,
    /// A file the operator supplied.
    Upload,
    /// The page after the agent's change; never leaves the operator.
    After,
    /// The final picture taken at Finish.
    Final,
}

/// Who wrote a thread entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ReplyAuthor {
    /// The operator.
    Operator,
    /// The stage agent.
    Agent,
    /// Archon itself.
    Archon,
}

/// A rectangle in CSS pixels.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Rect {
    /// Left edge.
    pub x: f64,
    /// Top edge.
    pub y: f64,
    /// Width.
    pub w: f64,
    /// Height.
    pub h: f64,
}

/// The viewport a note was made in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Viewport {
    /// Width in CSS pixels.
    pub w: u32,
    /// Height in CSS pixels.
    pub h: u32,
    /// The preset it came from.
    pub preset: ViewportPreset,
}

/// A UI component the page names for an element.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ComponentRef {
    /// The component's name.
    pub name: String,
    /// The framework it came from.
    pub framework: String,
}

/// What a click became: the element or region the operator pointed at.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Target {
    /// The page path, without query or fragment.
    pub path: String,
    /// A CSS selector, generated in the page.
    pub selector: Option<String>,
    /// The element's role and name.
    pub role_name: Option<String>,
    /// The framework component, best effort.
    pub component: Option<ComponentRef>,
    /// The element's box.
    #[serde(rename = "box")]
    pub rect: Rect,
    /// The viewport at the time.
    pub viewport: Viewport,
    /// A few computed styles.
    pub styles: BTreeMap<String, String>,
    /// Whether the operator drew a region rather than picking an element.
    pub region: bool,
    /// The steps that put the preview in the state this picture shows (a
    /// dialog the button opens, a hovered menu); empty for the page as loaded.
    #[serde(default, skip_serializing_if = "no_steps")]
    pub steps: Box<[UiStep]>,
}

/// Whether a picture's state is the page as loaded. (A boxed slice keeps a
/// note, and so every reply that carries one, from outgrowing the others.)
fn no_steps(steps: &[UiStep]) -> bool {
    steps.is_empty()
}

/// One step that drives the preview into the state that shows a change. The
/// agent names the steps; the before and after pictures replay them on a
/// fresh load of the note's page.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "do", rename_all = "snake_case", deny_unknown_fields)]
pub enum UiStep {
    /// Go to an app path.
    Open {
        /// The path, like `/settings`.
        path: String,
    },
    /// Click the element a CSS selector matches.
    Click {
        /// The selector.
        selector: String,
    },
    /// Move the pointer over the element a CSS selector matches.
    Hover {
        /// The selector.
        selector: String,
    },
    /// Wait for an animation or a request.
    Wait {
        /// Milliseconds.
        ms: u32,
    },
}

/// The most steps a state may take.
pub const MAX_UI_STEPS: usize = 12;
/// The longest single wait of a state, in milliseconds.
pub const MAX_STEP_WAIT_MS: u32 = 5000;
/// The longest selector of a step, in characters.
pub const MAX_STEP_SELECTOR_CHARS: usize = 300;

/// Why `steps` cannot be replayed, in plain words.
pub fn check_steps(steps: &[UiStep]) -> Result<(), String> {
    if steps.len() > MAX_UI_STEPS {
        return Err(format!("take at most {MAX_UI_STEPS} steps"));
    }
    for (n, step) in steps.iter().enumerate() {
        let at = n + 1;
        match step {
            UiStep::Open { path } => {
                check_app_path(path).map_err(|why| format!("step {at}: path {why}"))?
            }
            UiStep::Click { selector } | UiStep::Hover { selector } => {
                if selector.trim().is_empty() {
                    return Err(format!("step {at}: the selector is empty"));
                }
                if selector.chars().count() > MAX_STEP_SELECTOR_CHARS
                    || selector.chars().any(char::is_control)
                {
                    return Err(format!(
                        "step {at}: the selector is too long or has control characters"
                    ));
                }
            }
            UiStep::Wait { ms } if *ms > MAX_STEP_WAIT_MS => {
                return Err(format!("step {at}: wait at most {MAX_STEP_WAIT_MS} ms"));
            }
            UiStep::Wait { .. } => {}
        }
    }
    Ok(())
}

/// What Archon says on a note whose after picture matched its before picture
/// even after the agent's retake; the agent's own words follow it.
pub const NO_VISIBLE_CHANGE: &str = "No visible change was captured";

/// What Archon says on a done note when its after picture first matched its
/// before picture and the agent was asked to retake it in the right state.
pub const RETAKE_ASKED: &str = "The after picture matched the before picture";

/// Whether the agent was already asked to retake `note`'s after picture.
pub fn retake_asked(note: &DesignNote) -> bool {
    note.thread
        .iter()
        .any(|e| e.from == ReplyAuthor::Archon && e.text.starts_with(RETAKE_ASKED))
}

/// The notice of a done note that has no after picture to compare because
/// the one taken matched the before picture: Archon's newest thread entry
/// that begins with [`NO_VISIBLE_CHANGE`]. A note with a real after picture
/// has none.
pub fn no_visible_change(note: &DesignNote) -> Option<&str> {
    if note.after_asset.is_some() {
        return None;
    }
    note.thread
        .iter()
        .rev()
        .find(|e| e.from == ReplyAuthor::Archon && e.text.starts_with(NO_VISIBLE_CHANGE))
        .map(|e| e.text.as_str())
}

/// One message in a note's conversation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ThreadEntry {
    /// When it was written.
    pub at: DateTime<Utc>,
    /// Who wrote it.
    pub from: ReplyAuthor,
    /// What it says.
    pub text: String,
}

/// What the operator chose at Finish.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct FinishChoices {
    /// Per note id, whether its final picture is included.
    pub include: BTreeMap<String, bool>,
    /// Per note id, the note that supersedes it, if any.
    pub superseded: BTreeMap<String, Option<String>>,
}

/// One design session of a stage run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct DesignSession {
    /// Its id.
    pub id: DesignSessionId,
    /// The queue item.
    pub item: QueueItemId,
    /// The pipeline run.
    pub run: RunId,
    /// The stage's id in the run.
    pub stage_id: String,
    /// The stage's attempt.
    pub attempt: u32,
    /// Where it is.
    pub state: SessionState,
    /// The preview could not be started or kept; notes still reach the agent.
    pub degraded: bool,
    /// Bumped on every change to the notes, so Finish can detect a stale capture.
    pub note_version: u64,
    /// The finish capture's id.
    pub finish_capture: Option<String>,
    /// HEAD at the finish capture.
    pub finish_head: Option<String>,
    /// The frozen Finish choices.
    pub finish_choices: Option<FinishChoices>,
    /// The URL the agent gave the preview.
    pub preview_url: Option<String>,
    /// The browser's process id.
    pub browser_pid: Option<u32>,
    /// The browser process's start time, to tell a reused pid.
    pub browser_started_at: Option<u64>,
    /// The size the preview is drawn at.
    pub viewport_preset: ViewportPreset,
    /// The browser runs without its sandbox.
    pub sandbox_off: bool,
    /// The stage agent.
    pub agent: Option<AetherAgentId>,
    /// When it began.
    pub created_at: DateTime<Utc>,
    /// When it ended.
    pub ended_at: Option<DateTime<Utc>>,
    /// Why it ended.
    pub end_reason: Option<String>,
}

/// One note of an item.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct DesignNote {
    /// The queue item.
    pub item: QueueItemId,
    /// `D<n>`, or `D<n>-final` for a final picture.
    pub id: String,
    /// The `n` of `D<n>`.
    pub number: u64,
    /// The session it was made in.
    pub session: DesignSessionId,
    /// What it asks for.
    pub kind: NoteKind,
    /// The operator's words.
    pub text: String,
    /// The details block as the operator left it.
    pub details_text: Option<String>,
    /// Whether the picture goes to the agent.
    pub send_picture: bool,
    /// What it points at.
    pub target: Option<Target>,
    /// The viewport it was made in.
    pub viewport: Viewport,
    /// Where it is.
    pub status: NoteStatus,
    /// Why it failed.
    pub fail_reason: Option<FailReason>,
    /// HEAD when it was sent.
    pub base_commit: Option<String>,
    /// The commit that made the change.
    pub commit: Option<String>,
    /// Done with nothing to change.
    pub no_change: bool,
    /// For a revert note, the note it undoes.
    pub reverts: Option<String>,
    /// The note this one was resent from.
    pub resent_from: Option<String>,
    /// For a follow-up, the version of its chain it continues from.
    #[serde(default)]
    pub follows: Option<String>,
    /// Earlier notes the operator referenced, at most [`MAX_REFS`].
    #[serde(default)]
    pub refs: Vec<String>,
    /// The note that supersedes this one.
    pub superseded_by: Option<String>,
    /// The conversation about it.
    pub thread: Vec<ThreadEntry>,
    /// The client's idempotency key.
    pub client_key: String,
    /// How many times it was delivered.
    pub delivery: u32,
    /// The picture sent with it.
    pub before_asset: Option<DesignAssetId>,
    /// The picture after the change; operator only.
    pub after_asset: Option<DesignAssetId>,
    /// The final picture.
    pub final_asset: Option<DesignAssetId>,
    /// Whether the final picture was included at Finish.
    pub final_included: Option<bool>,
    /// The commit the final picture was captured at.
    pub captured_commit: Option<String>,
    /// The device that made it.
    pub created_by_device: Option<String>,
    /// When it was made.
    pub created_at: DateTime<Utc>,
    /// When it last changed.
    pub updated_at: DateTime<Utc>,
}

/// A stored picture.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct DesignAsset {
    /// Its id.
    pub id: DesignAssetId,
    /// The queue item.
    pub item: QueueItemId,
    /// Hex SHA-256 of the bytes.
    pub sha256: String,
    /// The media type.
    pub mime: String,
    /// The size in bytes.
    pub bytes: u64,
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Where it is stored, relative to the data directory.
    pub path: String,
    /// Where it came from.
    pub source: AssetSource,
    /// What it shows.
    pub target: Option<Target>,
    /// The note using it.
    pub note: Option<String>,
    /// The commit it was taken at.
    pub commit: Option<String>,
    /// When it was stored.
    pub created_at: DateTime<Utc>,
}

/// The longest note, in characters.
pub const MAX_NOTE_CHARS: usize = 4000;
/// The longest reply, in characters.
pub const MAX_REPLY_CHARS: usize = 2000;
/// The longest details block, in characters.
pub const MAX_DETAILS_CHARS: usize = 2000;
/// The largest encoded picture sent in a frame or message.
pub const MAX_ENCODED_IMAGE_BYTES: usize = 560 * 1024;
/// The largest serialized server frame.
pub const MAX_SERIALIZED_FRAME_BYTES: usize = 768 * 1024;
/// Input events in one `design_input`.
pub const MAX_INPUT_EVENTS: usize = 64;
/// Notes in one session.
pub const MAX_NOTES_PER_SESSION: usize = 200;
/// Asset bytes of one item.
pub const MAX_ITEM_ASSET_BYTES: u64 = 200 * 1024 * 1024;
/// The longest side of a stored picture.
pub const MAX_IMAGE_SIDE: u32 = 4096;
/// Pictures left before the operator is warned.
pub const IMAGE_CAP_WARN_MARGIN: u32 = 8;
/// Minutes after which an offered, unused design stage is dropped.
pub const DROP_OFFERED_AFTER_MINUTES: i64 = 15;
/// A design stage's timeout in minutes.
pub const STAGE_TIMEOUT_MINUTES: u32 = 240;
/// Versions one chain of follow-ups may hold, its first note included.
pub const MAX_CHAIN_VERSIONS: usize = 20;
/// Earlier notes one note may reference.
pub const MAX_REFS: usize = 5;
/// After pictures of referenced notes attached to one message.
pub const MAX_REF_PICTURES: usize = 3;

/// Whether a note of `kind` may move from `from` to `to`.
pub fn transition_allowed(kind: NoteKind, from: NoteStatus, to: NoteStatus) -> bool {
    use NoteStatus::{Cancelled, Done, Failed, NeedsReply, Queued, Sent, Working};
    match kind {
        NoteKind::Revert => matches!(
            (from, to),
            (Queued, Working | Failed | Cancelled) | (Working, Done | Failed)
        ),
        NoteKind::Change | NoteKind::Page => matches!(
            (from, to),
            (Queued, Sent | Cancelled)
                | (Sent, Working | NeedsReply | Done | Failed | Cancelled)
                | (Working, NeedsReply | Done | Failed | Cancelled)
                | (NeedsReply, Working | Done | Failed | Cancelled)
        ),
    }
}

/// Whether the agent holds the note: `Sent`, `Working` or `NeedsReply`.
pub fn is_in_flight(status: NoteStatus) -> bool {
    matches!(
        status,
        NoteStatus::Sent | NoteStatus::Working | NoteStatus::NeedsReply
    )
}

/// The id of note number `number`: `D<n>`.
pub fn note_id(number: u64) -> String {
    format!("D{number}")
}

/// A note allocated by the store, spelled exactly as it appears in the ledger.
pub fn valid_note_id(id: &str) -> bool {
    id.strip_prefix('D')
        .filter(|digits| !digits.starts_with('0') && digits.bytes().all(|c| c.is_ascii_digit()))
        .is_some_and(|digits| digits.parse::<u64>().is_ok())
}

/// The number of a `D<n>` or `D<n>-<suffix>` id.
pub fn note_number(id: &str) -> Option<u64> {
    let rest = id.strip_prefix('D')?;
    let digits = match rest.split_once('-') {
        Some((digits, suffix)) if !suffix.is_empty() => digits,
        Some(_) => return None,
        None => rest,
    };
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

/// The number the next note takes: the stored sequence (1 when none), or one
/// more than any `D<k>` or `D<k>-…` id already in use, whichever is larger.
pub fn next_note_number(seq: Option<u64>, existing_ids: &[&str]) -> u64 {
    let from_ids = existing_ids
        .iter()
        .filter_map(|id| note_number(id))
        .map(|k| k.saturating_add(1))
        .max()
        .unwrap_or(1);
    seq.unwrap_or(1).max(from_ids).max(1)
}

/// Check a preview URL and return it normalized: `http` only, the host
/// exactly `127.0.0.1`, `localhost` or `[::1]`, any port, no credentials.
pub fn validate_preview_url(url: &str) -> Result<String, ArchonError> {
    if url.len() > 4096 {
        return Err(ArchonError::Invalid(
            "preview URL exceeds 4096 bytes".into(),
        ));
    }
    let refuse = |why: &str| {
        ArchonError::Invalid(format!(
            "preview URL {url:?}: {why}; use http://127.0.0.1:<port>/, http://localhost:<port>/ or http://[::1]:<port>/"
        ))
    };
    if url
        .chars()
        .any(|c| c.is_control() || c.is_whitespace() || c == '\\')
    {
        return Err(refuse(
            "contains whitespace, control characters or backslashes",
        ));
    }
    let rest = url
        .strip_prefix("http://")
        .ok_or_else(|| refuse("only http:// is allowed"))?;
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = &rest[..end];
    if authority.contains('@') {
        return Err(refuse("credentials are not allowed"));
    }
    let (host, port) = if let Some(after) = authority.strip_prefix('[') {
        let (host, tail) = after
            .split_once(']')
            .ok_or_else(|| refuse("malformed host"))?;
        let port = match tail {
            "" => None,
            t => Some(
                t.strip_prefix(':')
                    .ok_or_else(|| refuse("malformed host"))?,
            ),
        };
        (format!("[{host}]"), port)
    } else {
        match authority.split_once(':') {
            Some((host, port)) => (host.to_owned(), Some(port)),
            None => (authority.to_owned(), None),
        }
    };
    if !matches!(host.as_str(), "127.0.0.1" | "localhost" | "[::1]") {
        return Err(refuse("the host must be 127.0.0.1, localhost or [::1]"));
    }
    if let Some(port) = port
        && (!port.bytes().all(|b| b.is_ascii_digit()) || !matches!(port.parse::<u16>(), Ok(1..)))
    {
        return Err(refuse("the port must be between 1 and 65535"));
    }
    Ok(url.to_owned())
}

/// The longest app path a note may name.
pub const MAX_PATH_CHARS: usize = 512;

/// An app path a page note names: `/` then no whitespace, control
/// characters, scheme or host. Returns why not.
pub fn check_app_path(path: &str) -> Result<(), &'static str> {
    if !path.starts_with('/') || path.starts_with("//") {
        return Err("must start with a single /, like /settings");
    }
    if path.chars().count() > MAX_PATH_CHARS {
        return Err("is too long");
    }
    if path
        .chars()
        .any(|c| c.is_whitespace() || c.is_control() || c == '\\')
    {
        return Err("must not contain spaces, control characters or backslashes");
    }
    if path.contains("://") {
        return Err("is a path in the app, not a URL");
    }
    Ok(())
}

/// `text` without control characters other than newline and tab, trimmed and
/// cut to `max_chars` characters.
pub fn clean_text(text: &str, max_chars: usize) -> String {
    let kept: String = text
        .chars()
        .filter(|c| !c.is_control() || matches!(c, '\n' | '\t'))
        .collect();
    kept.trim().chars().take(max_chars).collect()
}

/// For each undone note, the id of the `Done` revert note naming it.
pub fn undone_by(notes: &[DesignNote]) -> BTreeMap<String, String> {
    notes
        .iter()
        .filter(|n| n.kind == NoteKind::Revert && n.status == NoteStatus::Done)
        .filter_map(|n| Some((n.reverts.clone()?, n.id.clone())))
        .collect()
}

/// Intersection over union of two rectangles; 0 when either is empty.
pub fn iou(a: &Rect, b: &Rect) -> f64 {
    let w = (a.x + a.w).min(b.x + b.w) - a.x.max(b.x);
    let h = (a.y + a.h).min(b.y + b.h) - a.y.max(b.y);
    if w <= 0.0 || h <= 0.0 {
        return 0.0;
    }
    let inter = w * h;
    let union = a.w * a.h + b.w * b.h - inter;
    if union <= 0.0 { 0.0 } else { inter / union }
}

/// Whether two notes point at the same thing: the same path and either the
/// same selector (neither a region) or both regions overlapping by 0.5 IoU.
pub fn same_target(a: &Target, b: &Target) -> bool {
    if a.path != b.path {
        return false;
    }
    match (a.region, b.region) {
        (false, false) => a.selector.is_some() && a.selector == b.selector,
        (true, true) => iou(&a.rect, &b.rect) >= 0.5,
        _ => false,
    }
}

/// Which earlier note a later one supersedes, by note id: each earlier note
/// maps to the latest later note with the same target. Notes without a
/// target never supersede or are superseded.
pub fn auto_supersession(candidates: &[&DesignNote]) -> BTreeMap<String, String> {
    let mut sorted: Vec<&DesignNote> = candidates
        .iter()
        .copied()
        .filter(|n| n.target.is_some())
        .collect();
    sorted.sort_by_key(|n| n.number);
    let mut out = BTreeMap::new();
    for (i, earlier) in sorted.iter().enumerate() {
        let target = earlier.target.as_ref().expect("filtered on a target");
        let latest = sorted[i + 1..].iter().rev().find(|later| {
            later.number > earlier.number
                && later
                    .target
                    .as_ref()
                    .is_some_and(|t| same_target(target, t))
        });
        if let Some(later) = latest {
            out.insert(earlier.id.clone(), later.id.clone());
        }
    }
    out
}

/// Where a note sits in its chain of follow-ups.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChainPlace {
    /// The chain's first note.
    pub root: String,
    /// 1-based position in the chain, by note number.
    pub version: u32,
    /// How many versions the chain holds.
    pub versions: u32,
}

/// Each note's id, with the id its `follows` names when that note exists.
fn parents(notes: &[DesignNote]) -> BTreeMap<&str, Option<&str>> {
    let ids: std::collections::BTreeSet<&str> = notes.iter().map(|n| n.id.as_str()).collect();
    notes
        .iter()
        .map(|n| {
            let parent = n.follows.as_deref().filter(|p| ids.contains(p));
            (n.id.as_str(), parent)
        })
        .collect()
}

/// The root `id` reaches through `parents`; a loop ends the walk.
fn root_in(parents: &BTreeMap<&str, Option<&str>>, id: &str) -> String {
    let mut at = id;
    for _ in 0..=parents.len() {
        match parents.get(at).copied().flatten() {
            Some(parent) => at = parent,
            None => break,
        }
    }
    at.to_owned()
}

/// The first note of `id`'s chain: `follows` walked back as far as it goes
/// among `notes`. A loop or a missing link ends the walk where it is.
pub fn chain_root(notes: &[DesignNote], id: &str) -> String {
    root_in(&parents(notes), id)
}

/// Every note of a chain, by note id: its place. Revert notes belong to none.
pub fn chains(notes: &[DesignNote]) -> BTreeMap<String, ChainPlace> {
    let parents = parents(notes);
    let mut members: BTreeMap<String, Vec<(u64, String)>> = BTreeMap::new();
    for note in notes.iter().filter(|n| n.kind != NoteKind::Revert) {
        members
            .entry(root_in(&parents, &note.id))
            .or_default()
            .push((note.number, note.id.clone()));
    }
    let mut out = BTreeMap::new();
    for (root, mut ids) in members {
        ids.sort();
        let versions = u32::try_from(ids.len()).unwrap_or(u32::MAX);
        for (n, (_, id)) in ids.into_iter().enumerate() {
            let version = u32::try_from(n + 1).unwrap_or(u32::MAX);
            out.insert(
                id,
                ChainPlace {
                    root: root.clone(),
                    version,
                    versions,
                },
            );
        }
    }
    out
}

/// Which earlier version of a chain the chain's newest active version
/// supersedes, by note id: among `candidates` (the active notes), the one
/// with the highest number in each chain stands for it. `all` resolves the
/// chains, so a link through a set-aside version still counts.
pub fn chain_supersession(
    candidates: &[&DesignNote],
    all: &[DesignNote],
) -> BTreeMap<String, String> {
    let parents = parents(all);
    let mut newest: BTreeMap<String, (u64, String)> = BTreeMap::new();
    for note in candidates {
        let root = root_in(&parents, &note.id);
        let slot = newest.entry(root).or_insert((note.number, note.id.clone()));
        if note.number > slot.0 {
            *slot = (note.number, note.id.clone());
        }
    }
    candidates
        .iter()
        .filter_map(|note| {
            let (_, by) = newest.get(&root_in(&parents, &note.id))?;
            (by != &note.id).then(|| (note.id.clone(), by.clone()))
        })
        .collect()
}

/// The earlier versions of `id`'s chain that are ancestors of it, oldest
/// first: the line of `follows` from the chain's first note to `id`.
pub fn ancestors<'a>(notes: &'a [DesignNote], id: &str) -> Vec<&'a DesignNote> {
    let mut line = Vec::new();
    let mut at = notes
        .iter()
        .find(|n| n.id == id)
        .and_then(|n| n.follows.clone());
    while let Some(parent) = at {
        if line.len() > notes.len() {
            break;
        }
        let Some(note) = notes.iter().find(|n| n.id == parent) else {
            break;
        };
        line.push(note);
        at = note.follows.clone();
    }
    line.reverse();
    line
}

#[cfg(test)]
mod tests;
