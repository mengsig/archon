//! `archon chat`: the quiet operator chat (ARCHITECTURE §12); also what a bare
//! `archon` opens where the terminal UI cannot run.
//! A line REPL, not a TUI: it replays recent history, follows the feed,
//! sends plain lines to the orchestrator and runs slash commands.

mod actions;
pub mod commands;
mod draft;
mod input;
mod keys;
mod link;

use std::collections::VecDeque;
use std::process::ExitCode;
use std::sync::{Arc, Mutex};

use archon_client::{ArchonClient, FeedSubscription};
use archon_core::config::Config;
use archon_core::notify::{FeedEntry, FeedItem};
use archon_core::protocol::{AetherLinkStatus, Reply, Request};

use crate::chat::draft::Draft;
use crate::chat::input::{Ack, Incoming, Input, InputEvent, Output, Typed};
use crate::chat::link::{Link, LinkEvent};
use crate::connect::unexpected;
use crate::questions::QuestionBook;
use crate::render::Renderer;
use crate::style::{Palette, Style, terminal_width};

/// Lines this chat sent whose feed echo is still due; the operator already
/// sees them after the prompt, so the echo is not printed twice.
const MAX_PENDING_ECHOES: usize = 16;
const HISTORY_FILE: &str = "chat_history";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Flow {
    Continue,
    Quit,
}

#[derive(Debug)]
struct Chat {
    output: Output,
    palette: Palette,
    verbose: bool,
    /// Highest feed seq handled; the resume cursor after a reconnect.
    last_seq: u64,
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
        verbose: progress || config.chat.show_progress,
        last_seq: 0,
        book: QuestionBook::default(),
        pending_echoes: VecDeque::new(),
        interrupts: 0,
        draft: Arc::default(),
        prefill: None,
        unsent: None,
    };
    let feed = chat.open(&client, config.chat.history).await?;
    let (input, output) = Input::start(history, chat.draft.clone())?;
    chat.output = output;
    let link = Link::up(socket, client, feed);
    chat.run(input, link).await
}

impl Chat {
    /// Replay recent history, number open questions, say where things
    /// stand, and subscribe after the last replayed item.
    async fn open(
        &mut self,
        client: &ArchonClient,
        history: usize,
    ) -> anyhow::Result<FeedSubscription> {
        let replay = match history {
            0 => Vec::new(),
            limit => {
                let request = Request::FeedHistory {
                    before: None,
                    limit,
                    verbose: self.verbose,
                };
                match client.request(request).await? {
                    Reply::Feed(items) => items,
                    other => return Err(unexpected(&other, "feed")),
                }
            }
        };
        assert!(
            replay.windows(2).all(|w| w[0].seq < w[1].seq),
            "history is in seq order"
        );
        self.sync_questions(client).await?;
        for item in &replay {
            self.show(item, false)?;
        }
        // Always verbose: /progress then filters locally without resubscribing.
        let after = replay.last().map(|item| item.seq);
        let feed = client.subscribe_feed(after, true).await?;
        if after.is_none() {
            self.last_seq = feed.last_seq;
        }
        debug_assert_eq!(after.unwrap_or(self.last_seq), self.last_seq);
        self.header(client).await?;
        Ok(feed)
    }

    async fn run(&mut self, mut input: Input, mut link: Link) -> anyhow::Result<ExitCode> {
        loop {
            tokio::select! {
                incoming = input.next() => match incoming {
                    Incoming::Notice(notice) => self.warn(&notice)?,
                    Incoming::Event(event) => match self.on_input(event, &link).await? {
                        Flow::Continue => input.ack(self.next_line()),
                        Flow::Quit => {
                            input.stop();
                            return Ok(ExitCode::SUCCESS);
                        }
                    },
                },
                event = link.next(self.last_seq) => self.on_link(event, &link).await?,
            }
        }
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
            LinkEvent::Item(item) => self.show(&item, true),
            LinkEvent::Lost(reason) => self.warn(&format!(
                "lost the connection to archond ({reason}); reconnecting…"
            )),
            LinkEvent::StillDown(reason) => self.note(&format!("still reconnecting: {reason}")),
            LinkEvent::Restored { reset_to } => {
                if let Some(head) = reset_to {
                    self.last_seq = head;
                    self.warn("archond's feed is behind this chat (a different store?); showing new items from here")?;
                }
                self.note("reconnected to archond")?;
                self.resync(link.client()).await
            }
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

    /// Print one feed item unless it is a duplicate, hidden, or the echo of
    /// a line typed here. `live` items also update the question numbers.
    fn show(&mut self, item: &FeedItem, live: bool) -> anyhow::Result<()> {
        if item.seq <= self.last_seq {
            return Ok(());
        }
        self.last_seq = item.seq;
        if live {
            self.book.observe(&item.entry);
            if let FeedEntry::Operator { text, .. } = &item.entry
                && let Some(at) = self.pending_echoes.iter().position(|sent| sent == text)
            {
                self.pending_echoes.remove(at);
                return Ok(());
            }
        }
        let renderer = Renderer::new(self.palette, terminal_width());
        match renderer.render(item, self.verbose, self.book.labels()) {
            Some(text) => self.print(&text),
            None => Ok(()),
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
            verbose: false,
            last_seq: 0,
            book: QuestionBook::default(),
            pending_echoes: VecDeque::new(),
            interrupts: 0,
            draft: Arc::default(),
            prefill: None,
            unsent: None,
        }
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
}
