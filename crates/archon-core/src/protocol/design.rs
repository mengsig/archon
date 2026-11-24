//! The wire types of live design sessions (protocol 25, feature
//! `design_sessions`; iterations from 26, `design_iterations`): input events, the views replies carry and the events
//! of a `subscribe_design` stream.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::design::{DesignAssetId, DesignNote, DesignSession, Rect, SessionState, Target};
use crate::id::QueueItemId;

/// The status an agent reports for a note.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AgentNoteStatus {
    /// Started on it.
    Working,
    /// Finished.
    Done,
    /// Waiting for the operator's answer.
    NeedsReply,
    /// Could not do it.
    Failed,
    /// Stopped at the operator's request.
    Cancelled,
}

/// What a mouse event does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum MouseAction {
    /// The pointer moved.
    Move,
    /// A button went down.
    Down,
    /// A button came up.
    Up,
}

/// A mouse button.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum MouseButton {
    /// No button.
    None,
    /// The primary button.
    Left,
    /// The wheel button.
    Middle,
    /// The secondary button.
    Right,
}

/// A key going down or up.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum KeyAction {
    /// Pressed.
    Down,
    /// Released.
    Up,
}

/// A touch phase.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TouchAction {
    /// Fingers down.
    Start,
    /// Fingers moved.
    Move,
    /// Fingers up.
    End,
    /// The gesture was interrupted.
    Cancel,
}

/// One finger.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TouchPoint {
    /// CSS pixels from the left.
    pub x: f64,
    /// CSS pixels from the top.
    pub y: f64,
    /// The finger's id.
    pub id: u32,
}

/// One input event, in CSS pixels.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum InputEvent {
    /// A pointer event.
    Mouse {
        /// What happened.
        action: MouseAction,
        /// CSS pixels from the left.
        x: f64,
        /// CSS pixels from the top.
        y: f64,
        /// The button.
        button: MouseButton,
        /// Consecutive clicks.
        click_count: u32,
        /// Modifier bit mask.
        modifiers: u8,
    },
    /// A wheel event.
    Wheel {
        /// CSS pixels from the left.
        x: f64,
        /// CSS pixels from the top.
        y: f64,
        /// Horizontal delta.
        dx: f64,
        /// Vertical delta.
        dy: f64,
        /// Modifier bit mask.
        modifiers: u8,
    },
    /// A key event.
    Key {
        /// Down or up.
        action: KeyAction,
        /// The key value.
        key: String,
        /// The physical key code.
        code: String,
        /// The text it types, if any.
        text: Option<String>,
        /// Modifier bit mask.
        modifiers: u8,
    },
    /// Text typed or pasted.
    Text {
        /// The text.
        text: String,
    },
    /// A touch event.
    Touch {
        /// The phase.
        action: TouchAction,
        /// The fingers.
        points: Vec<TouchPoint>,
    },
}

/// The preview browser's state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum BrowserStatus {
    /// No browser is wanted.
    Idle,
    /// Starting.
    Starting,
    /// Running.
    Running,
    /// Waiting for a free preview slot.
    QueuedForSlot,
    /// No browser found.
    Missing {
        /// What to do about it.
        hint: String,
    },
    /// The browser's sandbox cannot start.
    SandboxUnavailable {
        /// What to do about it.
        hint: String,
    },
    /// It crashed too often to be restarted again.
    CrashedTooOften,
    /// Running without its sandbox, by the operator's choice.
    Unsandboxed,
}

/// A note as the operator's screen shows it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct DesignNoteView {
    /// The note itself.
    #[serde(flatten)]
    pub note: DesignNote,
    /// The revert note that undid it.
    pub undone_by: Option<String>,
    /// Made in an earlier session: no longer editable.
    pub read_only: bool,
    /// In the item's current approved set.
    pub active: bool,
    /// Bound by an approved intent: no Undo or supersession touches it.
    #[serde(default)]
    pub approved: bool,
    /// The first note of its chain of follow-ups; none for a revert note.
    #[serde(default)]
    pub chain: Option<String>,
    /// Its 1-based place in the chain; 0 for a revert note.
    #[serde(default)]
    pub version: u32,
    /// How many versions the chain holds.
    #[serde(default)]
    pub versions: u32,
    /// Its change was reverted (undone, or set aside by a continue).
    #[serde(default)]
    pub set_aside: bool,
}

/// One earlier version of a chain on its Finish card.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct FinishHistoryEntry {
    /// The version's note id.
    pub note: String,
    /// Its words.
    pub text: String,
    /// Its change was reverted.
    pub set_aside: bool,
}

/// An item's session and notes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct DesignStateView {
    /// The queue item.
    pub item: QueueItemId,
    /// The current session, if any.
    pub session: Option<DesignSession>,
    /// The notes of every session.
    pub notes: Vec<DesignNoteView>,
    /// The browser's state.
    pub browser: BrowserStatus,
    /// Why Finish is refused right now.
    pub finish_blocker: Option<String>,
    /// Pictures stored.
    pub images_used: u32,
    /// Pictures allowed.
    pub images_max: u32,
    /// When the stage times out.
    pub stage_deadline: Option<DateTime<Utc>>,
}

/// What a click or region resolved to.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct DesignResolvedView {
    /// The element or region.
    pub target: Target,
    /// A short description for the composer.
    pub label: String,
    /// The pending crop.
    pub asset: Option<DesignAssetId>,
    /// The crop as a `data:` URL.
    pub data_url: Option<String>,
}

/// One note's card in the Finish review.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct FinishCard {
    /// The note id.
    pub note: String,
    /// The note's text.
    pub text: String,
    /// The final picture.
    pub asset: Option<DesignAssetId>,
    /// The picture as a `data:` URL.
    pub data_url: Option<String>,
    /// A caption for the picture.
    pub caption: Option<String>,
    /// Whether the picture is included.
    pub include: bool,
    /// The note that supersedes this one.
    pub superseded_by: Option<String>,
    /// The element was not found at the final capture.
    pub not_found: bool,
    /// The note's commit is no longer in the history.
    pub left_history: bool,
    /// The note belongs to an earlier session of the item (before a
    /// Revise): its picture was reviewed then and is carried as it stands,
    /// never captured or confirmed again.
    #[serde(default)]
    pub earlier: bool,
    /// The earlier versions of the note's chain, oldest first.
    #[serde(default)]
    pub history: Vec<FinishHistoryEntry>,
}

/// The final capture the operator reviews.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct DesignFinishView {
    /// The capture id.
    pub capture: String,
    /// HEAD at the capture.
    pub head: String,
    /// The note version at the capture.
    pub note_version: u64,
    /// One card per note.
    pub cards: Vec<FinishCard>,
    /// Every note was done with no change.
    pub no_change_only: bool,
}

/// One stored picture.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct DesignAssetView {
    /// Its id.
    pub id: DesignAssetId,
    /// The picture as a `data:` URL.
    pub data_url: String,
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// A caption.
    pub caption: Option<String>,
}

/// One event of a `subscribe_design` stream.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "event", rename_all = "snake_case")]
#[expect(
    clippy::large_enum_variant,
    reason = "a note view is the common large event; boxing it would allocate per event"
)]
pub enum DesignEvent {
    /// One frame of the preview.
    Frame {
        /// Increases with every frame, for acknowledgements.
        seq: u64,
        /// The JPEG, base64.
        jpeg_b64: String,
        /// Width in pixels.
        w: u32,
        /// Height in pixels.
        h: u32,
        /// When it was taken.
        at: DateTime<Utc>,
    },
    /// The element under the pointer or selected.
    Selection {
        /// Its box.
        #[serde(rename = "box")]
        rect: Rect,
        /// A short description.
        label: String,
    },
    /// The session changed.
    Session {
        /// Its state.
        state: SessionState,
        /// The preview is degraded.
        degraded: bool,
        /// The page path being shown.
        preview_path: Option<String>,
        /// HEAD.
        commit: Option<String>,
        /// The browser's state.
        browser: BrowserStatus,
        /// The note version.
        note_version: u64,
    },
    /// A note changed.
    Note(DesignNoteView),
    /// Hosts the page tried to load and was refused.
    BlockedHosts {
        /// The hosts.
        hosts: Vec<String>,
    },
    /// A line of the agent's progress.
    AgentProgress {
        /// The line.
        line: String,
    },
    /// The page opened a JavaScript dialog.
    Dialog {
        /// `alert`, `confirm`, `prompt` or `beforeunload`.
        kind: String,
        /// The dialog's message.
        message: String,
        /// A prompt's default text.
        default_prompt: Option<String>,
    },
    /// A short message for the operator.
    Toast {
        /// The message.
        message: String,
    },
    /// The session ended.
    Ended {
        /// Why.
        reason: String,
    },
}

/// The `include` map of a `design_finish_confirm`.
pub type IncludeChoices = BTreeMap<String, bool>;
