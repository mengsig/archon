//! `archon chat`: the line chat with the orchestrator (ARCHITECTURE §12);
//! also what a bare `archon` opens where the terminal UI cannot run.
//! A line REPL, not a TUI: it shows the orchestrator's conversation as the
//! Aether TUI's `/focus` does, sends plain lines and runs slash commands.

mod actions;
pub mod commands;
mod draft;
mod input;
mod keys;
mod link;

use std::collections::VecDeque;
use std::process::ExitCode;
use std::sync::{Arc, Mutex};

use archon_client::ArchonClient;
use archon_core::authority::WEB_RELAY_MARK;
use archon_core::config::Config;
use archon_core::conversation::{ConversationEntry, ConversationPage, EntryKind};
use archon_core::protocol::{AetherLinkStatus, Reply, Request};

use crate::chat::draft::Draft;
use crate::chat::input::{Ack, Incoming, Input, InputEvent, Output, Typed};
use crate::chat::link::{Link, LinkEvent};
use crate::connect::unexpected;
use crate::questions::QuestionBook;
use crate::render::Renderer;
use crate::style::{Palette, Style, terminal_width};

/// Lines this chat sent whose echo in the conversation is still due; the
/// operator already sees them after the prompt, so they are not printed twice.
const MAX_PENDING_ECHOES: usize = 16;
const HISTORY_FILE: &str = "chat_history";
/// Events the opening read covers, as on the phone; all its entries show.
const OPENING_EVENTS: u32 = 200;
/// How long leaving the chat waits to learn whether the orchestrator failed.
const STATUS_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Flow {
    Continue,
    Quit,
}

#[derive(Debug)]
struct Chat {
    output: Output,
    palette: Palette,
    /// The last conversation event read; `None` until the opening page.
    cursor: Option<u64>,
    /// The `(seq, part)` of the last entry shown, so none shows twice.
    shown: Option<(u64, u32)>,
    book: QuestionBook,
    pending_echoes: VecDeque<String>,
    /// Consecutive Ctrl-C presses at the prompt.
    interrupts: u32,
    /// The images attached to the line being typed, shared with the editor's keys.
    draft: Arc<Mutex<Draft>>,
    /// What the next line starts with: the chips `/image` attached.
    prefill: Option<String>,
    /// The text of a message that was not sent, for the next line to start with.
    unsent: Option<String>,
}

pub async fn run(progress: bool) -> anyhow::Result<ExitCode> {
    let config = Config::load_default()?;
    let client = crate::connect::connect_or_start().await?;
    let socket = archon_client::default_socket();
    let history = archon_core::paths::data_dir().join(HISTORY_FILE);
    let history = std::path::absolute(&history)?;
    // Plain stdout until the editor runs: an editor blocked in a read
    // could not be stopped if opening fails.
    let mut chat = Chat {
        output: Output::Stdout,
        palette: Palette::detect(),
        cursor: None,
        shown: None,
        book: QuestionBook::default(),
        pending_echoes: VecDeque::new(),
        interrupts: 0,
        draft: Arc::default(),
        prefill: None,
        unsent: None,
    };
    chat.sync_questions(&client).await?;
    chat.header(&client).await?;
    if progress || config.chat.show_progress {
        chat.note(commands::PROGRESS_MOVED)?;
    }
    let (input, output) = Input::start(history, chat.draft.clone())?;
    chat.output = output;
    let link = Link::up(socket, client);
    chat.run(input, link).await
}

impl Chat {
    async fn run(&mut self, mut input: Input, mut link: Link) -> anyhow::Result<ExitCode> {
        loop {
            tokio::select! {
                incoming = input.next() => match incoming {
                    Incoming::Notice(notice) => self.warn(&notice)?,
                    Incoming::Event(event) => match self.on_input(event, &link).await? {
                        Flow::Continue => input.ack(self.next_line()),
                        Flow::Quit => {
                            let unsent = self.unsent_on_leaving();
                            input.stop();
                            if let Some(warning) = unsent {
                                self.warn(&warning)?;
                            }
                            return self.exit_code(link.client()).await;
                        }
                    },
                },
                event = link.next(self.cursor, OPENING_EVENTS) => self.on_link(event, &link).await?,
            }
        }
    }

    /// Failure while the orchestrator has failed, so a script learns that
    /// it cannot answer; success otherwise.
    async fn exit_code(&mut self, client: Option<&ArchonClient>) -> anyhow::Result<ExitCode> {
        let Some(client) = client else {
            return Ok(ExitCode::SUCCESS);
        };
        let Some(info) = crate::connect::failed_orchestrator(client, STATUS_TIMEOUT).await? else {
            return Ok(ExitCode::SUCCESS);
        };
        assert_eq!(info.status, crate::connect::FAILED);
        self.error(&format!(
            "the orchestrator {} has failed and cannot answer: `archon doctor` names the \
             fix, and `archon orchestrator restart` starts a fresh one",
            info.agent
        ))?;
        Ok(ExitCode::FAILURE)
    }

    async fn on_input(&mut self, event: InputEvent, link: &Link) -> anyhow::Result<Flow> {
        match event {
            InputEvent::Eof => Ok(Flow::Quit),
            InputEvent::Interrupted => {
                self.interrupts += 1;
                if self.interrupts >= 2 {
                    return Ok(Flow::Quit);
                }
                self.note("(press Ctrl-C again, Ctrl-D, or type /quit to leave)")?;
                Ok(Flow::Continue)
            }
            InputEvent::Line(line) => {
                self.interrupts = 0;
                actions::handle_line(self, &line, link.client()).await
            }
        }
    }

    async fn on_link(&mut self, event: LinkEvent, link: &Link) -> anyhow::Result<()> {
        match event {
            LinkEvent::Page(page) => {
                let asks = self.show_page(&page)?;
                match asks {
                    true => self.refresh_questions(link.client()).await,
                    false => Ok(()),
                }
            }
            LinkEvent::Lost(reason) => self.warn(&format!(
                "lost the connection to archond ({reason}); reconnecting…"
            )),
            LinkEvent::StillDown(reason) => self.note(&format!("still reconnecting: {reason}")),
            LinkEvent::Unreadable(reason) => self.warn(&format!(
                "cannot read the conversation ({reason}); retrying…"
            )),
            LinkEvent::Restored => {
                self.note("reconnected to archond")?;
                self.resync(link.client()).await
            }
        }
    }

    /// Print a page's new entries, the opening page's whole as the phone
    /// and the TUI's calm view show it. `true` when one may ask or answer.
    fn show_page(&mut self, page: &ConversationPage) -> anyhow::Result<bool> {
        assert!(page.is_ordered(), "archond checked the page's order");
        let opening = self.cursor.is_none();
        let mut asks = false;
        for entry in &page.entries {
            asks |= matches!(entry.kind, EntryKind::Post | EntryKind::Question);
            self.show(entry, !opening)?;
        }
        self.cursor = Some(self.cursor.unwrap_or(0).max(page.to_seq));
        Ok(asks)
    }

    /// Print one entry unless it was shown or is the echo of a line typed
    /// here, which the prompt already shows.
    fn show(&mut self, entry: &ConversationEntry, live: bool) -> anyhow::Result<()> {
        let at = (entry.seq, entry.part);
        if self.shown.is_some_and(|shown| at <= shown) {
            return Ok(());
        }
        self.shown = Some(at);
        if live && entry.kind == EntryKind::Operator {
            let text = entry
                .text
                .strip_prefix(WEB_RELAY_MARK)
                .unwrap_or(&entry.text);
            if let Some(at) = self.pending_echoes.iter().position(|sent| sent == text) {
                self.pending_echoes.remove(at);
                return Ok(());
            }
        }
        let renderer = Renderer::new(self.palette, terminal_width());
        let text = renderer.render_entry(entry);
        self.print(&text)
    }

    /// Renumber questions after a post that may ask or answer one; a failure
    /// is only reported, as the next read reports a lost connection.
    async fn refresh_questions(&mut self, client: Option<&ArchonClient>) -> anyhow::Result<()> {
        let Some(client) = client else {
            return Ok(());
        };
        match self.sync_questions(client).await {
            Ok(()) => Ok(()),
            Err(e) => self.warn(&format!("could not refresh the open questions: {e:#}")),
        }
    }

    /// Renumber questions and say where things stand after a reconnect. A
    /// failure is only reported: a dropped connection shows up as `Lost` next,
    /// and ending the chat while the editor reads would leave the terminal raw.
    async fn resync(&mut self, client: Option<&ArchonClient>) -> anyhow::Result<()> {
        let Some(client) = client else {
            return Ok(());
        };
        let result = match self.sync_questions(client).await {
            Ok(()) => self.header(client).await,
            Err(e) => Err(e),
        };
        match result {
            Ok(()) => Ok(()),
            Err(e) => self.warn(&format!("reconnected, but could not refresh: {e:#}")),
        }
    }

    /// How the editor reads the next line: with the chips still attached and
    /// any unsent text already typed.
    fn next_line(&mut self) -> Ack {
        let chips = self.prefill.take().unwrap_or_default();
        let text = self.unsent.take().unwrap_or_default();
        if chips.is_empty() && text.is_empty() {
            return Ack::Continue;
        }
        Ack::Draft(Typed { chips, text })
    }

    /// What leaving says of images still attached: they were never sent.
    fn unsent_on_leaving(&self) -> Option<String> {
        let chips = self.lock_draft().chips();
        (!chips.is_empty())
            .then(|| format!("not sent: {chips} (still attached when the chat ended)"))
    }

    /// The draft; a poisoned lock only means a paste panicked mid-way.
    fn lock_draft(&self) -> std::sync::MutexGuard<'_, Draft> {
        self.draft.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn expect_echo(&mut self, text: &str) {
        if self.pending_echoes.len() == MAX_PENDING_ECHOES {
            self.pending_echoes.pop_front();
        }
        self.pending_echoes.push_back(text.to_owned());
    }

    async fn sync_questions(&mut self, client: &ArchonClient) -> anyhow::Result<()> {
        match client
            .request(Request::ListQuestions { open_only: true })
            .await?
        {
            Reply::Questions(open) => {
                self.book.sync_open(open);
                Ok(())
            }
            other => Err(unexpected(&other, "questions")),
        }
    }

    /// One calm line on where things stand, plus a warning when Aether is
    /// down (the orchestrator cannot hear the operator then).
    async fn header(&mut self, client: &ArchonClient) -> anyhow::Result<()> {
        let status = match client.request(Request::Status(Default::default())).await? {
            Reply::Status(status) => status,
            other => return Err(unexpected(&other, "status")),
        };
        let mut facts = vec![format!("archond {}", status.daemon_version)];
        if status.open_questions > 0 {
            facts.push(format!(
                "{} open question(s): /questions",
                status.open_questions
            ));
        }
        if status.pause.global {
            facts.push("dispatch paused: /resume".to_owned());
        }
        facts.push("/help for commands".to_owned());
        self.note(&facts.join(" · "))?;
        if let AetherLinkStatus::Disconnected { error, .. } = &status.aether {
            self.warn(&format!(
                "Aether is not reachable ({error}); the orchestrator cannot hear you until it is back"
            ))?;
        }
        Ok(())
    }

    fn print(&mut self, text: &str) -> anyhow::Result<()> {
        self.output
            .print(text)
            .map_err(|e| anyhow::anyhow!("writing to the terminal: {e}"))
    }

    fn note(&mut self, text: &str) -> anyhow::Result<()> {
        let line = self.palette.paint(Style::Dim, text);
        self.print(&line)
    }

    fn warn(&mut self, text: &str) -> anyhow::Result<()> {
        let line = format!("{} {text}", self.palette.paint(Style::Yellow, "!"));
        self.print(&line)
    }

    fn error(&mut self, text: &str) -> anyhow::Result<()> {
        let line = format!("{} {text}", self.palette.paint(Style::Red, "error:"));
        self.print(&line)
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use archon_core::protocol::{
        ClientFrame, ErrorCode, ErrorReply, PROTOCOL_VERSION, ServerFrame, Welcome,
    };
    use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};
    use tokio::net::UnixListener;

    use super::*;

    /// A client of a daemon that answers `hello` and then hangs up.
    async fn client_of_a_vanishing_daemon(socket: &Path) -> ArchonClient {
        let listener = UnixListener::bind(socket).unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (read, mut write) = stream.into_split();
            let mut hello = String::new();
            BufReader::new(read).read_line(&mut hello).await.unwrap();
            let welcome = Welcome {
                protocol_version: PROTOCOL_VERSION,
                daemon_version: "test".into(),
                pid: 1,
                min_protocol_version: PROTOCOL_VERSION,
                features: Vec::new(),
                device: None,
            };
            let frame = ServerFrame::Reply {
                id: 1,
                result: Ok(Reply::Welcome(welcome)),
            };
            let line = format!("{}\n", serde_json::to_string(&frame).unwrap());
            write.write_all(line.as_bytes()).await.unwrap();
        });
        let client = ArchonClient::connect(socket, archon_core::protocol::ClientKind::ArchonCli)
            .await
            .unwrap();
        server.await.unwrap();
        client
    }

    /// A client of a daemon that answers `hello`, then answers one request
    /// with `answer`, or hangs up unanswered when it is `None`. The task
    /// returns the request.
    async fn client_of_a_daemon_answering_once(
        socket: &Path,
        answer: Option<Result<Reply, ErrorReply>>,
    ) -> (ArchonClient, tokio::task::JoinHandle<Request>) {
        let listener = UnixListener::bind(socket).unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (read, mut write) = stream.into_split();
            let mut lines = BufReader::new(read).lines();
            let hello = lines.next_line().await.unwrap().unwrap();
            let hello: ClientFrame = serde_json::from_str(&hello).unwrap();
            assert!(matches!(hello.request, Request::Hello { .. }), "{hello:?}");
            let welcome = Welcome {
                protocol_version: PROTOCOL_VERSION,
                daemon_version: "test".into(),
                pid: 1,
                min_protocol_version: PROTOCOL_VERSION,
                features: Vec::new(),
                device: None,
            };
            let frames = [(hello.id, Ok(Reply::Welcome(welcome)))];
            write_frames(&mut write, frames).await;
            let asked = lines.next_line().await.unwrap().unwrap();
            let asked: ClientFrame = serde_json::from_str(&asked).unwrap();
            if let Some(result) = answer {
                write_frames(&mut write, [(asked.id, result)]).await;
            }
            asked.request
        });
        let client = ArchonClient::connect(socket, archon_core::protocol::ClientKind::ArchonCli)
            .await
            .unwrap();
        (client, server)
    }

    async fn write_frames(
        write: &mut tokio::net::unix::OwnedWriteHalf,
        frames: impl IntoIterator<Item = (u64, Result<Reply, ErrorReply>)>,
    ) {
        for (id, result) in frames {
            let frame = ServerFrame::Reply { id, result };
            let line = format!("{}\n", serde_json::to_string(&frame).unwrap());
            write.write_all(line.as_bytes()).await.unwrap();
        }
    }

    /// A chat with `shot.png` attached by `/image`, and the chips it prefills.
    async fn chat_with_an_image(dir: &Path) -> (Chat, String) {
        let shot = dir.join("shot.png");
        std::fs::write(&shot, b"\x89PNG\r\n\x1a\npixels").unwrap();
        let mut chat = quiet_chat();
        let attach = format!("/image {}", shot.display());
        actions::handle_line(&mut chat, &attach, None)
            .await
            .unwrap();
        let chips = chat.prefill.take().expect("the chip is prefilled");
        assert_eq!(chips, "[image #1] ");
        (chat, chips)
    }

    fn said(request: Request) -> (String, usize) {
        match request {
            Request::Say { text, images } => (text, images.len()),
            other => panic!("not a say: {other:?}"),
        }
    }

    fn quiet_chat() -> Chat {
        Chat {
            output: Output::Stdout,
            palette: Palette::new(false),
            cursor: None,
            shown: None,
            book: QuestionBook::default(),
            pending_echoes: VecDeque::new(),
            interrupts: 0,
            draft: Arc::default(),
            prefill: None,
            unsent: None,
        }
    }

    /// What a chat printed, block by block.
    #[derive(Clone, Default)]
    struct Printed(Arc<Mutex<Vec<String>>>);

    impl rustyline::ExternalPrinter for Printed {
        fn print(&mut self, msg: String) -> rustyline::Result<()> {
            self.0
                .lock()
                .unwrap()
                .push(msg.trim_end_matches('\n').to_owned());
            Ok(())
        }
    }

    fn entry(seq: u64, part: u32, kind: EntryKind, text: &str) -> ConversationEntry {
        ConversationEntry {
            seq,
            part,
            at: archon_core::now(),
            kind,
            important: true,
            text: text.into(),
            links: Vec::new(),
        }
    }

    fn page(entries: Vec<ConversationEntry>, to_seq: u64) -> ConversationPage {
        ConversationPage {
            from_seq: entries.first().map_or(to_seq, |e| e.seq),
            to_seq,
            last_seq: to_seq,
            entries,
            ..ConversationPage::default()
        }
    }

    /// A mixed turn as Aether's `/focus` holds it: the operator's line, the
    /// orchestrator's text before and between tool calls, a notify and an
    /// item outcome archond posted, and the phone's marked line.
    fn mixed_turn() -> Vec<ConversationEntry> {
        vec![
            entry(10, 0, EntryKind::Operator, "add caching to api"),
            entry(
                11,
                0,
                EntryKind::Assistant,
                "Looking at the api repo first.",
            ),
            entry(
                14,
                0,
                EntryKind::Assistant,
                "Queued qi_7 on api.\n\n- cache layer",
            ),
            entry(
                15,
                0,
                EntryKind::Post,
                "**Decision** (qi_7): build runs on opus",
            ),
            entry(19, 0, EntryKind::Post, "**Outcome** (qi_7): PR #7 merged"),
            entry(
                20,
                0,
                EntryKind::Operator,
                &format!("{WEB_RELAY_MARK}thanks"),
            ),
        ]
    }

    #[test]
    fn the_chat_prints_exactly_the_focused_entries_in_order_and_once() {
        let printed = Printed::default();
        let mut chat = quiet_chat();
        chat.output = Output::AbovePrompt(Box::new(printed.clone()));
        let turn = mixed_turn();
        let asks = chat.show_page(&page(turn[..3].to_vec(), 14)).unwrap();
        assert!(!asks, "no post yet");
        assert_eq!(chat.cursor, Some(14));
        // A live page repeating the last entry, then the rest of the turn.
        let asks = chat.show_page(&page(turn[2..].to_vec(), 20)).unwrap();
        assert!(asks, "posts may ask or answer questions");
        let renderer = Renderer::new(Palette::new(false), terminal_width());
        let expected: Vec<String> = turn.iter().map(|e| renderer.render_entry(e)).collect();
        let shown = printed.0.lock().unwrap().clone();
        assert_eq!(shown, expected);
        for (line, said) in shown.iter().zip(&turn) {
            let first = said.text.strip_prefix(WEB_RELAY_MARK).unwrap_or(&said.text);
            let first = first.lines().next().unwrap().replace("**", "");
            assert!(line.contains(&first), "{line:?} shows {first:?}");
        }
        assert_eq!(chat.cursor, Some(20));
        assert_eq!(chat.shown, Some((20, 0)));
    }

    #[test]
    fn the_opening_page_shows_all_its_history_in_order() {
        let printed = Printed::default();
        let mut chat = quiet_chat();
        chat.output = Output::AbovePrompt(Box::new(printed.clone()));
        let history: Vec<ConversationEntry> = (1..=40)
            .map(|seq| entry(seq, 0, EntryKind::Assistant, &format!("reply {seq}")))
            .collect();
        chat.show_page(&page(history.clone(), 40)).unwrap();
        let shown = printed.0.lock().unwrap().clone();
        assert_eq!(shown.len(), 40, "more than 30 entries replay whole");
        for (line, said) in shown.iter().zip(&history) {
            assert!(line.ends_with(&said.text), "{line:?} shows {:?}", said.text);
        }
        assert_eq!(chat.shown, Some((40, 0)));
    }

    #[test]
    fn a_typed_line_shows_once() {
        let printed = Printed::default();
        let mut chat = quiet_chat();
        chat.output = Output::AbovePrompt(Box::new(printed.clone()));
        let turn = mixed_turn();
        chat.show_page(&page(turn[..4].to_vec(), 15)).unwrap();
        assert_eq!(printed.0.lock().unwrap().len(), 4, "the whole opening page");
        chat.expect_echo("thanks");
        chat.show_page(&page(turn[4..].to_vec(), 20)).unwrap();
        let shown = printed.0.lock().unwrap().clone();
        assert_eq!(
            shown.len(),
            5,
            "the phone-marked echo of a typed line is not repeated"
        );
        assert!(shown[4].contains("PR #7 merged"));
        assert!(chat.pending_echoes.is_empty());
    }

    #[tokio::test]
    async fn a_line_that_sends_nothing_keeps_the_images_its_chips_hold() {
        let dir = tempfile::tempdir().unwrap();
        let shot = dir.path().join("shot.png");
        std::fs::write(&shot, b"\x89PNG\r\n\x1a\npixels").unwrap();
        let mut chat = quiet_chat();
        let attach = format!("/image {}", shot.display());
        actions::handle_line(&mut chat, &attach, None)
            .await
            .unwrap();
        assert_eq!(chat.prefill.take().as_deref(), Some("[image #1] "));
        let missing = dir.path().join("missing.png");
        let failed = format!("[image #1] /image {}", missing.display());
        actions::handle_line(&mut chat, &failed, None)
            .await
            .unwrap();
        assert_eq!(chat.prefill.take().as_deref(), Some("[image #1] "));
        // Not connected, so nothing is sent and the image stays attached.
        let unsent = "[image #1] what is this?";
        actions::handle_line(&mut chat, unsent, None).await.unwrap();
        assert_eq!(chat.prefill.take().as_deref(), Some("[image #1] "));
        assert_eq!(chat.unsent.take().as_deref(), Some("what is this?"));
        actions::handle_line(&mut chat, "/image", None)
            .await
            .unwrap();
        assert_eq!(
            chat.prefill, None,
            "a line without the chip drops its image"
        );
    }

    #[tokio::test]
    async fn an_image_attached_with_slash_image_goes_with_the_next_typed_line() {
        let dir = tempfile::tempdir().unwrap();
        let (mut chat, chips) = chat_with_an_image(dir.path()).await;
        let ahead = Typed {
            chips,
            text: String::new(),
        };
        // Input that is not a terminal reads the line without the chip shown.
        let typed = "what does the image say?";
        let line = input::line_read(typed.into(), &ahead, false);
        assert_eq!(input::line_read(line.clone(), &ahead, true), line);
        let socket = dir.path().join("d.sock");
        let (client, server) =
            client_of_a_daemon_answering_once(&socket, Some(Ok(Reply::Ok))).await;
        actions::handle_line(&mut chat, &line, Some(&client))
            .await
            .unwrap();
        assert_eq!(said(server.await.unwrap()), (typed.to_owned(), 1));
        assert_eq!(chat.lock_draft().chips(), "", "a sent image is let go");
        assert_eq!(chat.unsent_on_leaving(), None);
        assert_eq!((chat.prefill.take(), chat.unsent.take()), (None, None));
    }

    #[tokio::test]
    async fn a_refused_send_keeps_the_images_and_puts_the_text_back() {
        let dir = tempfile::tempdir().unwrap();
        let (mut chat, chips) = chat_with_an_image(dir.path()).await;
        let refusal = ErrorReply {
            code: ErrorCode::InvalidArgument,
            message: "invalid argument: refused".into(),
        };
        let socket = dir.path().join("d.sock");
        let (client, server) = client_of_a_daemon_answering_once(&socket, Some(Err(refusal))).await;
        let line = format!("{chips}hello");
        actions::handle_line(&mut chat, &line, Some(&client))
            .await
            .unwrap();
        assert_eq!(said(server.await.unwrap()), ("hello".to_owned(), 1));
        assert_eq!(chat.lock_draft().chips(), "[image #1]");
        assert_eq!(chat.prefill.take(), Some(chips));
        let leaving = chat.unsent_on_leaving().expect("quitting now warns");
        assert!(leaving.starts_with("not sent: [image #1]"), "{leaving}");
        assert_eq!(chat.unsent.take().as_deref(), Some("hello"));
    }

    #[tokio::test]
    async fn a_send_lost_on_the_way_keeps_the_draft_and_is_not_replayed() {
        let dir = tempfile::tempdir().unwrap();
        let (mut chat, chips) = chat_with_an_image(dir.path()).await;
        let socket = dir.path().join("d.sock");
        let (client, server) = client_of_a_daemon_answering_once(&socket, None).await;
        let line = format!("{chips}hello");
        actions::handle_line(&mut chat, &line, Some(&client))
            .await
            .unwrap();
        assert_eq!(said(server.await.unwrap()), ("hello".to_owned(), 1));
        assert_eq!(chat.lock_draft().message(&line).images.len(), 1);
        assert_eq!(chat.prefill.take(), Some(chips));
        assert_eq!(chat.unsent.take().as_deref(), Some("hello"));
    }

    #[tokio::test]
    async fn a_failed_refresh_after_reconnecting_does_not_end_the_chat() {
        let dir = tempfile::tempdir().unwrap();
        let client = client_of_a_vanishing_daemon(&dir.path().join("d.sock")).await;
        let mut chat = quiet_chat();
        assert!(
            chat.sync_questions(&client).await.is_err(),
            "the daemon hung up"
        );
        chat.resync(Some(&client)).await.unwrap();
        chat.resync(None).await.unwrap();
    }

    fn orchestrator_in(status: &str) -> Reply {
        Reply::Orchestrator(archon_core::protocol::OrchestratorInfo {
            agent: archon_core::aether_ids::AetherAgentId::parse("agt_failed").unwrap(),
            model: archon_core::model::ModelRef::new("claude-code", "claude-opus-5-5"),
            effort: archon_core::model::Effort::High,
            status: status.into(),
            restarts: 0,
            generation: 1,
            freshness: None,
        })
    }

    #[tokio::test]
    async fn chat_ends_with_a_failure_while_the_orchestrator_has_failed() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("failed.sock");
        let failed = Some(Ok(orchestrator_in("failed")));
        let (client, server) = client_of_a_daemon_answering_once(&socket, failed).await;
        let code = quiet_chat().exit_code(Some(&client)).await.unwrap();
        assert_eq!(code, ExitCode::FAILURE);
        assert!(matches!(server.await.unwrap(), Request::OrchestratorStatus));

        let socket = dir.path().join("ready.sock");
        let ready = Some(Ok(orchestrator_in("ready")));
        let (client, _server) = client_of_a_daemon_answering_once(&socket, ready).await;
        let code = quiet_chat().exit_code(Some(&client)).await.unwrap();
        assert_eq!(code, ExitCode::SUCCESS);
        assert_eq!(
            quiet_chat().exit_code(None).await.unwrap(),
            ExitCode::SUCCESS
        );
    }
}
