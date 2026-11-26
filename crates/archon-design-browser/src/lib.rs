//! Headless Chromium driver for live design sessions (design.md §8.2, §8.4, §8.8).
//!
//! Chromium is a child of the daemon with a cleared environment, a private profile and CDP over
//! `--remote-debugging-pipe` only. This crate launches it, speaks CDP, relays screencast frames
//! with backpressure, captures size-budgeted pictures and describes the element under a point.

mod bridge;
mod browser;
mod cdp;
mod containment;
mod error;
mod features;
mod input;
mod keys;
mod launch;
mod origin;
mod proxy;
mod relay;
mod resolve;
mod sanitize;
mod settle;
#[cfg(test)]
mod testutil;

use std::collections::BTreeMap;

pub use bridge::{PickerOption, PickerRequest};
pub use browser::Browser;
pub use error::BrowserError;
pub use features::{KEPT_DOWNLOADS, MAX_DOWNLOAD_BYTES};
pub use launch::{
    LaunchSpec, doctor_line, find_browser, kill_if_matches, launch_env, process_start_time,
    sweep_profiles,
};
pub use proxy::UpstreamGuard;
pub use relay::{FrameRelay, FrameSubscriber};
pub use sanitize::{MAX_FILE_NAME_BYTES, file_name};

/// Largest encoded picture or frame (a base64 JSON frame then stays under 768 KiB).
pub const MAX_IMAGE_BYTES: usize = 560 * 1024;

/// The page's viewport.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Viewport {
    /// CSS px.
    pub width: u32,
    /// CSS px.
    pub height: u32,
    /// Emulate touch.
    pub touch: bool,
    /// Device pixel ratio.
    pub dpr: f64,
}

/// One input event, in CSS px.
#[derive(Debug, Clone, PartialEq)]
pub enum Input {
    /// Pointer event.
    Mouse {
        /// Move, down or up.
        action: MouseAction,
        /// X.
        x: f64,
        /// Y.
        y: f64,
        /// Button.
        button: MouseButton,
        /// Click count.
        click_count: u32,
        /// CDP modifier bitmask.
        modifiers: u8,
    },
    /// Wheel event.
    Wheel {
        /// X.
        x: f64,
        /// Y.
        y: f64,
        /// Horizontal delta.
        dx: f64,
        /// Vertical delta.
        dy: f64,
        /// CDP modifier bitmask.
        modifiers: u8,
    },
    /// Key event.
    Key {
        /// Down or up.
        action: KeyAction,
        /// `KeyboardEvent.key`.
        key: String,
        /// `KeyboardEvent.code`.
        code: String,
        /// Text the key produces.
        text: Option<String>,
        /// CDP modifier bitmask.
        modifiers: u8,
    },
    /// Insert text.
    Text {
        /// The text.
        text: String,
    },
    /// Touch event.
    Touch {
        /// Phase.
        action: TouchAction,
        /// Touch points.
        points: Vec<TouchPoint>,
    },
}

/// Pointer phase.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseAction {
    /// Move.
    Move,
    /// Button down.
    Down,
    /// Button up.
    Up,
}

/// Pointer button.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseButton {
    /// None.
    None,
    /// Left.
    Left,
    /// Middle.
    Middle,
    /// Right.
    Right,
}

/// Key phase.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyAction {
    /// Down.
    Down,
    /// Up.
    Up,
}

/// Touch phase.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TouchAction {
    /// Start.
    Start,
    /// Move.
    Move,
    /// End.
    End,
    /// Cancel.
    Cancel,
}

/// One touch point.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TouchPoint {
    /// X.
    pub x: f64,
    /// Y.
    pub y: f64,
    /// Identifier.
    pub id: u32,
}

/// A rectangle in CSS px.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ClipRect {
    /// Left.
    pub x: f64,
    /// Top.
    pub y: f64,
    /// Width.
    pub w: f64,
    /// Height.
    pub h: f64,
}

/// A captured picture, at most [`MAX_IMAGE_BYTES`].
#[derive(Debug, Clone, PartialEq)]
pub struct Picture {
    /// MIME type.
    pub mime: &'static str,
    /// Encoded bytes.
    pub bytes: Vec<u8>,
    /// Pixel width.
    pub width: u32,
    /// Pixel height.
    pub height: u32,
    /// How it was taken, so the same picture can be taken of a build.
    pub capture: PictureCapture,
}

/// How a picture was taken: the page's emulation, the area it shows in whole
/// CSS px, and the screenshot's own output scale, which is not the page's.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PictureCapture {
    /// Viewport width.
    pub viewport_w: u32,
    /// Viewport height.
    pub viewport_h: u32,
    /// The page's emulated device pixel ratio (`deviceScaleFactor`).
    pub device_scale_factor: f64,
    /// Whether the page was emulated as a mobile device with touch input.
    pub mobile: bool,
    /// Left, from the page's left edge.
    pub x: u32,
    /// Top, from the page's top edge.
    pub y: u32,
    /// Width.
    pub w: u32,
    /// Height.
    pub h: u32,
    /// `Page.captureScreenshot`'s `clip.scale`, after the byte budget.
    pub screenshot_scale: f64,
}

/// One screencast frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    /// Sequence number, from 1.
    pub seq: u64,
    /// JPEG bytes.
    pub jpeg: Vec<u8>,
    /// Pixel width.
    pub width: u32,
    /// Pixel height.
    pub height: u32,
}

/// What a point on the page is. Every string is capped and free of control characters.
#[derive(Debug, Clone, PartialEq)]
pub struct Resolved {
    /// `location.pathname`.
    pub path: String,
    /// Test-id, id or tag/class chain.
    pub selector: Option<String>,
    /// Role and accessible name, e.g. `button "Save changes"`.
    pub role_name: Option<String>,
    /// Component `(name, framework)`.
    pub component: Option<(String, String)>,
    /// Box left, CSS px.
    pub x: f64,
    /// Box top.
    pub y: f64,
    /// Box width.
    pub w: f64,
    /// Box height.
    pub h: f64,
    /// Computed styles (fixed property list).
    pub styles: BTreeMap<String, String>,
    /// One-line label, e.g. `button "Save changes" - SaveButton`.
    pub label: String,
}

/// Something that happened in the browser.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BrowserEvent {
    /// The page crashed.
    Crashed,
    /// The browser process or pipe is gone.
    Exited,
    /// A document policy failure stopped the browser.
    ContainmentStopped {
        /// Trusted explanation of the stop.
        reason: String,
    },
    /// A JavaScript dialog opened and waits for `answer_dialog`.
    Dialog {
        /// alert, confirm, prompt or beforeunload.
        kind: String,
        /// Message text.
        message: String,
        /// Default prompt text.
        default_prompt: Option<String>,
    },
    /// A request to a host outside the allowlist failed at the proxy.
    BlockedHost {
        /// The host.
        host: String,
    },
    /// A pop-up was closed.
    PopupClosed {
        /// Its URL.
        url: String,
    },
    /// A file input opened its chooser and waits for `set_files`.
    FileChooser {
        /// The input's backend node.
        node: i64,
        /// More than one file may be chosen.
        multiple: bool,
        /// The input's `accept` attribute.
        accept: String,
    },
    /// A file input opened its chooser, but it could not be offered.
    FileChooserFailed {
        /// Why, for the operator.
        reason: String,
    },
    /// A download finished in the private downloads directory.
    Downloaded {
        /// Sanitised suggested file name.
        name: String,
        /// Where Chromium saved it.
        path: std::path::PathBuf,
        /// Its size.
        bytes: u64,
    },
    /// A download was cancelled, failed or was too big.
    DownloadFailed {
        /// Sanitised suggested file name.
        name: String,
        /// Why, for the operator.
        reason: String,
    },
    /// The page opened a native picker a headless browser cannot draw.
    Picker(PickerRequest),
    /// The page copied text.
    Copied {
        /// The text.
        text: String,
    },
    /// A link or `window.open` meant for a new tab opened in the preview instead.
    NewTab {
        /// Its URL.
        url: String,
    },
    /// The main document failed to load.
    NavigationFailed {
        /// URL.
        url: String,
        /// Error text.
        error: String,
    },
    /// The page finished loading.
    Loaded {
        /// URL.
        url: String,
    },
}

/// Dimensions of a JPEG from its first start-of-frame marker.
pub fn jpeg_dims(b: &[u8]) -> Option<(u32, u32)> {
    let mut i = 2;
    if b.get(..2)? != [0xFF, 0xD8] {
        return None;
    }
    while i + 9 < b.len() {
        if b[i] != 0xFF {
            i += 1;
            continue;
        }
        let m = b[i + 1];
        if m == 0xFF {
            i += 1;
            continue;
        }
        if (0xC0..=0xCF).contains(&m) && !matches!(m, 0xC4 | 0xC8 | 0xCC) {
            let h = u32::from(b[i + 5]) << 8 | u32::from(b[i + 6]);
            let w = u32::from(b[i + 7]) << 8 | u32::from(b[i + 8]);
            return Some((w, h));
        }
        if m == 0xD8 || m == 0x01 || (0xD0..=0xD7).contains(&m) {
            i += 2;
            continue;
        }
        let len = usize::from(b[i + 2]) << 8 | usize::from(b[i + 3]);
        i += 2 + len;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jpeg_dimensions() {
        let j = [
            0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x04, 0x00, 0x00, 0xFF, 0xC0, 0x00, 0x11, 0x08, 0x01,
            0x2C, 0x02, 0x58, 0x03, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        ];
        assert_eq!(jpeg_dims(&j), Some((600, 300)));
        assert_eq!(jpeg_dims(b"nope"), None);
    }
}
