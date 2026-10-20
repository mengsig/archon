//! The chat's own keys. Ctrl+V and Alt+V paste the clipboard: an image is
//! attached as a chip, text is inserted. Ctrl+Shift+V is the terminal's own
//! paste (bracketed) and stays text. Backspace right after a chip deletes it.
//!
//! The line editor never turns on the kitty keyboard protocol, so a terminal
//! sends Ctrl+V as the raw byte 0x16, which the editor reads as Ctrl+V.

use std::sync::{Arc, Mutex};

use rustyline::history::DefaultHistory;
use rustyline::{
    Cmd, ConditionalEventHandler, Editor, Event, EventContext, EventHandler, KeyCode, KeyEvent,
    Modifiers, Movement, RepeatCount,
};
use tokio::sync::mpsc;

use super::draft::{Draft, chip_before, paste};
use super::input::Incoming;

/// Bind the paste and chip keys of `editor` to `draft`; what a paste has to
/// say goes to `notices`.
pub fn bind(
    editor: &mut Editor<(), DefaultHistory>,
    draft: Arc<Mutex<Draft>>,
    notices: mpsc::Sender<Incoming>,
) {
    let paste = PasteKey { draft, notices };
    let handler = || EventHandler::Conditional(Box::new(paste.clone()));
    for key in paste_keys() {
        let replaced = editor.bind_sequence(key, handler());
        debug_assert!(replaced.is_none(), "{key:?} is bound once");
    }
    let backspace = KeyEvent(KeyCode::Backspace, Modifiers::NONE);
    editor.bind_sequence(
        backspace,
        EventHandler::Conditional(Box::new(ChipBackspace)),
    );
}

/// Ctrl+V (raw 0x16 normalizes to it) and Alt+V in either case.
fn paste_keys() -> [KeyEvent; 3] {
    let raw = KeyEvent::new('\u{16}', Modifiers::NONE);
    assert_eq!(raw, KeyEvent::ctrl('V'), "0x16 is read as Ctrl+V");
    [raw, KeyEvent::alt('v'), KeyEvent::alt('V')]
}

#[derive(Clone)]
struct PasteKey {
    draft: Arc<Mutex<Draft>>,
    notices: mpsc::Sender<Incoming>,
}

impl ConditionalEventHandler for PasteKey {
    fn handle(&self, _: &Event, _: RepeatCount, _: bool, ctx: &EventContext) -> Option<Cmd> {
        let content = crate::clipboard::read();
        let home = dirs::home_dir();
        let pasted = {
            // A poisoned draft only means an earlier paste panicked mid-way.
            let mut draft = self.draft.lock().unwrap_or_else(|e| e.into_inner());
            paste(&mut draft, ctx.line(), content, home.as_deref())
        };
        if let Some(notice) = pasted.notice {
            // The chat drains notices while the editor reads, so this never waits long;
            // an error means the chat is gone and the editor ends next.
            let _chat_gone = self.notices.blocking_send(Incoming::Notice(notice));
        }
        Some(match pasted.insert {
            Some(text) => Cmd::Insert(1, text),
            None => Cmd::Noop,
        })
    }
}

struct ChipBackspace;

impl ConditionalEventHandler for ChipBackspace {
    fn handle(&self, _: &Event, _: RepeatCount, _: bool, ctx: &EventContext) -> Option<Cmd> {
        let before = ctx.line().get(..ctx.pos())?;
        let width = chip_before(before)?;
        // A chip is ASCII, so its bytes are its characters.
        let chip = &before[before.len() - width..];
        assert!(chip.is_ascii(), "a chip is ASCII: {chip:?}");
        let chars = RepeatCount::try_from(width).ok()?;
        Some(Cmd::Kill(Movement::BackwardChar(chars)))
    }
}
