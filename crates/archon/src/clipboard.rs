//! The system clipboard through its helper program (`wl-paste` on Wayland,
//! `xclip` on X11), with bounded output and time to answer. An image wins
//! over text when the clipboard offers both, so Ctrl+V attaches a copied
//! screenshot; the terminal's own paste (Ctrl+Shift+V) never comes here.

use std::io::Read as _;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use archon_core::attachment::{ImageAttachment, MAX_IMAGE_BYTES};

pub const HELPER_TIMEOUT: Duration = Duration::from_secs(3);
const NO_CONTENT: &str = "the clipboard holds no text or supported image";
const MAX_TEXT_BYTES: usize = 16 * 1024 * 1024;
const MAX_LISTING_BYTES: usize = 64 * 1024;
const IMAGE_TYPES: [&str; 4] = ["image/png", "image/jpeg", "image/gif", "image/webp"];
/// Text types in the order they are preferred, Wayland's then X11's.
const TEXT_TYPES: [&str; 5] = [
    "text/plain;charset=utf-8",
    "text/plain",
    "UTF8_STRING",
    "TEXT",
    "STRING",
];

/// What the clipboard held.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Clipboard {
    Image(ImageAttachment),
    Text(String),
}

/// The program that reads this session's clipboard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClipboardTool {
    WlPaste,
    Xclip,
}

impl ClipboardTool {
    pub fn program(self) -> &'static str {
        match self {
            Self::WlPaste => "wl-paste",
            Self::Xclip => "xclip",
        }
    }

    fn missing_hint(self) -> String {
        let package = match self {
            Self::WlPaste => "wl-clipboard",
            Self::Xclip => "xclip",
        };
        format!("install {package} to paste images, or use /image <path>")
    }

    fn list_args(self) -> &'static [&'static str] {
        match self {
            Self::WlPaste => &["--list-types"],
            Self::Xclip => &["-selection", "clipboard", "-t", "TARGETS", "-o"],
        }
    }

    fn read_args(self, kind: &str) -> Vec<&str> {
        match self {
            Self::WlPaste => vec!["--no-newline", "--type", kind],
            Self::Xclip => vec!["-selection", "clipboard", "-o", "-t", kind],
        }
    }
}

/// Which tool reads the clipboard here, or why none can.
pub fn clipboard_tool(ssh: bool, wayland: bool, display: bool) -> Result<ClipboardTool, String> {
    if ssh {
        return Err(
            "the clipboard is on the other end of this SSH session: copy the file here and use /image <path>"
                .into(),
        );
    }
    if wayland {
        return Ok(ClipboardTool::WlPaste);
    }
    if display {
        return Ok(ClipboardTool::Xclip);
    }
    Err("no Wayland or X11 clipboard here: use /image <path>".into())
}

/// Read this session's clipboard: an image if it offers one, else its text.
pub fn read() -> Result<Clipboard, String> {
    let tool = clipboard_tool(
        std::env::var_os("SSH_CONNECTION").is_some() || std::env::var_os("SSH_TTY").is_some(),
        std::env::var_os("WAYLAND_DISPLAY").is_some(),
        std::env::var_os("DISPLAY").is_some(),
    )?;
    read_with(tool, &[tool.program()], HELPER_TIMEOUT)
}

/// [`read`] through `command` (the program and any leading arguments),
/// each helper run given `timeout` to finish.
pub fn read_with(
    tool: ClipboardTool,
    command: &[&str],
    timeout: Duration,
) -> Result<Clipboard, String> {
    assert!(!command.is_empty(), "a helper command names a program");
    let run = |args: &[&str], limit| {
        let mut argv = command.to_vec();
        argv.extend_from_slice(args);
        run_helper(&argv, limit, timeout).map_err(|e| e.reason(tool))
    };
    let listing = run(tool.list_args(), MAX_LISTING_BYTES)?;
    let listing = String::from_utf8_lossy(&listing);
    let offered: Vec<&str> = listing.lines().map(str::trim).collect();
    if let Some(kind) = IMAGE_TYPES.into_iter().find(|kind| offered.contains(kind)) {
        let bytes = run(&tool.read_args(kind), MAX_IMAGE_BYTES + 1)?;
        let image = ImageAttachment::encode(&bytes, None).map_err(|e| e.to_string())?;
        return Ok(Clipboard::Image(image));
    }
    let text_kind = TEXT_TYPES.into_iter().find_map(|kind| {
        let offered_as = offered.iter().find(|o| o.eq_ignore_ascii_case(kind));
        offered_as.copied()
    });
    let Some(kind) = text_kind else {
        return Err(NO_CONTENT.into());
    };
    let bytes = run(&tool.read_args(kind), MAX_TEXT_BYTES + 1)?;
    if bytes.len() > MAX_TEXT_BYTES {
        return Err("the clipboard text is over 16 MiB".into());
    }
    let text =
        String::from_utf8(bytes).map_err(|e| format!("the clipboard text is not UTF-8: {e}"))?;
    match text.is_empty() {
        true => Err(NO_CONTENT.into()),
        false => Ok(Clipboard::Text(text)),
    }
}

/// Why a helper gave no bytes.
#[derive(Debug, PartialEq, Eq)]
enum HelperError {
    Missing,
    /// It ran and said no: an empty clipboard, or nothing of the type asked for.
    Refused,
    Broken(String),
}

impl HelperError {
    fn reason(self, tool: ClipboardTool) -> String {
        match self {
            Self::Missing => tool.missing_hint(),
            Self::Refused => NO_CONTENT.to_owned(),
            Self::Broken(why) => format!("{}: {why}", tool.program()),
        }
    }
}

/// Run `argv` and take at most `limit` bytes of what it prints. A helper
/// still printing at the limit is killed: the caller refuses that size.
fn run_helper(argv: &[&str], limit: usize, timeout: Duration) -> Result<Vec<u8>, HelperError> {
    assert!(limit > 0, "a helper may print something");
    let deadline = Instant::now() + timeout;
    let spawned = Command::new(argv[0])
        .args(&argv[1..])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn();
    let mut child = match spawned {
        Ok(child) => child,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(HelperError::Missing),
        Err(e) => return Err(HelperError::Broken(e.to_string())),
    };
    let Some(stdout) = child.stdout.take() else {
        stop(&mut child);
        return Err(HelperError::Broken("no output to read".into()));
    };
    let (sent, printed) = mpsc::channel();
    std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let read = stdout
            .take(limit as u64)
            .read_to_end(&mut bytes)
            .map(|_| bytes);
        // The receiver is gone only after a timeout, which is already reported.
        let _timed_out = sent.send(read);
    });
    let left = deadline.saturating_duration_since(Instant::now());
    let bytes = match printed.recv_timeout(left) {
        Ok(Ok(bytes)) => bytes,
        Ok(Err(e)) => {
            stop(&mut child);
            return Err(HelperError::Broken(e.to_string()));
        }
        Err(_) => {
            stop(&mut child);
            return Err(HelperError::Broken(format!("no answer within {timeout:?}")));
        }
    };
    debug_assert!(bytes.len() <= limit);
    if bytes.len() == limit {
        stop(&mut child);
        return Ok(bytes);
    }
    finish(&mut child, deadline, bytes)
}

/// Wait for a helper that closed its output; a failure with nothing printed is a refusal.
fn finish(child: &mut Child, deadline: Instant, bytes: Vec<u8>) -> Result<Vec<u8>, HelperError> {
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => return Ok(bytes),
            Ok(Some(_)) if bytes.is_empty() => return Err(HelperError::Refused),
            Ok(Some(status)) => return Err(HelperError::Broken(format!("exited with {status}"))),
            Ok(None) if Instant::now() >= deadline => {
                stop(child);
                return Err(HelperError::Broken("did not exit in time".into()));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(10)),
            Err(e) => return Err(HelperError::Broken(e.to_string())),
        }
    }
}

/// Kill and reap a helper whose answer is not wanted. Either step fails only
/// for a helper that already exited, which is the state wanted.
fn stop(child: &mut Child) {
    let _already_exited = child.kill();
    let _already_reaped = child.wait();
}

#[cfg(test)]
mod tests;
