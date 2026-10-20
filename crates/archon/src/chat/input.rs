//! The chat's line editor (rustyline) on its own thread, and the output
//! path that prints above the prompt without garbling what is being typed.
//!
//! The editor thread hands over one event, then waits for an [`Ack`] before
//! reading again. So the terminal is never left in raw mode: the chat
//! decides to stop while the editor is idle, and the thread then exits.

use std::io::Write as _;
use std::path::PathBuf;
use std::sync::mpsc as std_mpsc;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use anyhow::Context as _;
use rustyline::error::ReadlineError;
use rustyline::history::DefaultHistory;
use rustyline::{Config, Editor, ExternalPrinter};
use tokio::sync::mpsc;

use super::draft::Draft;

const PROMPT: &str = "› ";
const HISTORY_ENTRIES: usize = 1_000;

/// What the editor read. Each one must be answered with an [`Ack`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputEvent {
    Line(String),
    /// Ctrl-C at the prompt.
    Interrupted,
    /// Ctrl-D on an empty line, or the end of piped input.
    Eof,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Incoming {
    Event(InputEvent),
    /// Something the operator should know that did not stop the editor.
    /// Never acknowledged: the editor does not wait after a notice.
    Notice(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ack {
    Continue,
    /// Read the next line with this already typed (the chips `/image` attached).
    Draft(String),
    Stop,
}

/// Where chat output goes: above the live prompt when stdin and stdout are
/// terminals, else straight to stdout.
pub enum Output {
    AbovePrompt(Box<dyn ExternalPrinter + Send>),
    Stdout,
}

impl std::fmt::Debug for Output {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AbovePrompt(_) => f.write_str("Output::AbovePrompt"),
            Self::Stdout => f.write_str("Output::Stdout"),
        }
    }
}

impl Output {
    /// Print `text` as whole lines.
    pub fn print(&mut self, text: &str) -> std::io::Result<()> {
        let mut line = text.trim_end_matches('\n').to_owned();
        line.push('\n');
        if let Self::AbovePrompt(printer) = self {
            match printer.print(line.clone()) {
                Ok(()) => return Ok(()),
                // The editor is gone (it is shutting down); the terminal is
                // still there, so write to it directly from now on.
                Err(_) => *self = Self::Stdout,
            }
        }
        let mut stdout = std::io::stdout().lock();
        stdout.write_all(line.as_bytes())?;
        stdout.flush()
    }
}

#[derive(Debug)]
pub struct Input {
    events: mpsc::Receiver<Incoming>,
    acks: std_mpsc::Sender<Ack>,
    thread: Option<JoinHandle<()>>,
}

impl Input {
    /// Start the editor thread; history is loaded from and appended to
    /// `history` (a missing file is an empty history). Pasted images go to `draft`.
    pub fn start(history: PathBuf, draft: Arc<Mutex<Draft>>) -> anyhow::Result<(Self, Output)> {
        assert!(history.is_absolute(), "the history file path is absolute");
        let config = Config::builder()
            .max_history_size(HISTORY_ENTRIES)
            .context("configuring the line editor")?
            .history_ignore_dups(true)
            .context("configuring the line editor")?
            .history_ignore_space(true)
            .auto_add_history(false)
            .build();
        let mut editor: Editor<(), DefaultHistory> =
            Editor::with_config(config).context("opening the terminal for the chat")?;
        let mut notices = Vec::new();
        match editor.load_history(&history) {
            Ok(()) => {}
            Err(ReadlineError::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => notices.push(format!(
                "chat history {} not loaded: {e}",
                history.display()
            )),
        }
        // ENOTTY when stdin or stdout is not a terminal: plain output is right then.
        let output = match editor.create_external_printer() {
            Ok(printer) => Output::AbovePrompt(Box::new(printer)),
            Err(_) => Output::Stdout,
        };
        let (events_tx, events) = mpsc::channel(4);
        super::keys::bind(&mut editor, draft, events_tx.clone());
        let (acks, acks_rx) = std_mpsc::channel();
        let thread = std::thread::Builder::new()
            .name("archon-chat-input".into())
            .spawn(move || run_editor(editor, history, notices, events_tx, acks_rx))
            .context("starting the chat input thread")?;
        let input = Self {
            events,
            acks,
            thread: Some(thread),
        };
        Ok((input, output))
    }

    /// The next event; `Eof` if the editor thread is gone.
    pub async fn next(&mut self) -> Incoming {
        let eof = Incoming::Event(InputEvent::Eof);
        self.events.recv().await.unwrap_or(eof)
    }

    /// Let the editor read the next line (`Continue`) or finish (`Stop`).
    pub fn ack(&self, ack: Ack) {
        // A send error means the thread already ended; there is nothing to resume.
        let _thread_gone = self.acks.send(ack);
    }

    /// Stop the editor and wait for it, so the terminal is restored before
    /// the process exits. Call only while the editor waits for an ack.
    pub fn stop(mut self) {
        self.ack(Ack::Stop);
        if let Some(thread) = self.thread.take()
            && thread.join().is_err()
        {
            eprintln!("archon: the chat input thread panicked");
        }
    }
}

fn run_editor(
    mut editor: Editor<(), DefaultHistory>,
    history: PathBuf,
    notices: Vec<String>,
    events: mpsc::Sender<Incoming>,
    acks: std_mpsc::Receiver<Ack>,
) {
    for notice in notices {
        if events.blocking_send(Incoming::Notice(notice)).is_err() {
            return;
        }
    }
    let mut history_warned = false;
    let mut typed = String::new();
    loop {
        let read = match std::mem::take(&mut typed) {
            typed if typed.is_empty() => editor.readline(PROMPT),
            typed => editor.readline_with_initial(PROMPT, (&typed, "")),
        };
        let event = match read {
            Ok(line) => {
                let remembered = super::draft::without_chips(&line);
                if let Err(e) = remember(&mut editor, &history, &remembered)
                    && !history_warned
                {
                    history_warned = true;
                    let notice = format!("chat history not saved to {}: {e:#}", history.display());
                    if events.blocking_send(Incoming::Notice(notice)).is_err() {
                        return;
                    }
                }
                InputEvent::Line(line)
            }
            Err(ReadlineError::Interrupted) => InputEvent::Interrupted,
            Err(ReadlineError::Eof) => InputEvent::Eof,
            Err(e) => {
                let notice = format!("reading input failed: {e}");
                let _receiver_gone = events.blocking_send(Incoming::Notice(notice));
                InputEvent::Eof
            }
        };
        if events.blocking_send(Incoming::Event(event)).is_err() {
            return;
        }
        match acks.recv() {
            Ok(Ack::Continue) => continue,
            Ok(Ack::Draft(chips)) => typed = chips,
            Ok(Ack::Stop) | Err(_) => return,
        }
    }
}

/// Add a non-blank line to the history and append it to the file right away,
/// so a crash or a closed terminal loses nothing.
fn remember(
    editor: &mut Editor<(), DefaultHistory>,
    path: &std::path::Path,
    line: &str,
) -> anyhow::Result<()> {
    if line.trim().is_empty() || !editor.add_history_entry(line)? {
        return Ok(());
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    editor.append_history(path)?;
    Ok(())
}
