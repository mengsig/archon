//! How conversation entries look in the chat and feed items in `archon log`
//! (ARCHITECTURE §12). Pure: an entry or item in, the text to print (or
//! `None` when the progress filter hides an item) out. Every line is `HH:MM  <label>  <text>`, with the text
//! wrapped under itself and the label telling the kinds apart without colour.

use archon_core::Timestamp;
use archon_core::attachment::with_image_labels;
use archon_core::authority::WEB_RELAY_MARK;
use archon_core::conversation::{ConversationEntry, EntryKind};
use archon_core::notify::{FeedEntry, FeedItem, NotificationKind, Question};
use chrono::{Local, NaiveDate};

use crate::style::{DEFAULT_WIDTH, Palette, Style};
use crate::wrap::{report_link, sanitize, wrap};

/// Right-aligned label column; the longest label is `orchestrator`.
const LABEL_WIDTH: usize = 12;
/// Text never gets fewer columns than this, however narrow the terminal.
const MIN_TEXT_WIDTH: usize = 24;

#[derive(Debug, Clone, Copy)]
pub struct Renderer {
    pub palette: Palette,
    pub width: usize,
    /// Items from this local day show only the time.
    pub today: NaiveDate,
}

/// The chat's rendering of one conversation entry without colour, at `width`.
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "the binary renders with a detected palette")
)]
pub fn render_entry_plain(entry: &ConversationEntry, width: usize) -> String {
    Renderer::new(Palette::new(false), width).render_entry(entry)
}

/// The `archon log` rendering of one item without colour, at the default
/// width; `None` when `verbose` is off and the item is progress.
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "the binary renders with a detected palette")
)]
pub fn render_feed_item(item: &FeedItem, verbose: bool) -> Option<String> {
    Renderer::new(Palette::new(false), DEFAULT_WIDTH).render(item, verbose)
}

/// What one entry says, before layout.
struct Parts {
    label: &'static str,
    tone: Style,
    text: String,
    /// Dimmed lines under the text: options, answering hints.
    notes: Vec<String>,
    /// Dimmed reference after the text, e.g. the queue item.
    reference: Option<String>,
}

impl Renderer {
    pub fn new(palette: Palette, width: usize) -> Self {
        Self {
            palette,
            width,
            today: Local::now().date_naive(),
        }
    }

    pub fn render(&self, item: &FeedItem, verbose: bool) -> Option<String> {
        if !item.visible(verbose) {
            return None;
        }
        let mut parts = describe(&item.entry);
        if let FeedEntry::Report {
            title, location, ..
        } = &item.entry
        {
            parts.text = title.clone();
            let mut rendered = self.layout(item.at, &parts);
            let indent = self.time(item.at).chars().count() + 2 + LABEL_WIDTH + 2;
            let link = self
                .palette
                .hyperlinks
                .then(|| report_link(location))
                .flatten();
            let location = link.unwrap_or_else(|| crate::wrap::sanitize_line(location));
            // Add trusted escapes after layout; plain paths stay intact even
            // when they contain spaces, so the full location can be copied.
            rendered.push_str(&format!("\n{}{location}", " ".repeat(indent)));
            return Some(rendered);
        }
        Some(self.layout(item.at, &parts))
    }

    /// One entry of the orchestrator's conversation: its text as said (a
    /// phone line without its relay mark), under who said it, links below.
    pub fn render_entry(&self, entry: &ConversationEntry) -> String {
        assert!(entry.important, "the chat shows only important entries");
        let (label, tone) = match entry.kind {
            EntryKind::Operator => ("you", Style::Bold),
            EntryKind::Assistant => ("orchestrator", Style::Cyan),
            EntryKind::Post => ("archon", Style::Green),
            EntryKind::Question => ("question", Style::Yellow),
            EntryKind::Other => ("aether", Style::Dim),
        };
        let text = match entry.kind {
            EntryKind::Operator => entry.text.strip_prefix(WEB_RELAY_MARK),
            _ => None,
        };
        let parts = Parts {
            label,
            tone,
            text: crate::bold::without_bold_marks(text.unwrap_or(&entry.text)),
            notes: entry.links.clone(),
            reference: None,
        };
        let rendered = self.layout(entry.at, &parts);
        assert!(rendered.contains(label), "every entry names who said it");
        rendered
    }

    fn layout(&self, at: Timestamp, parts: &Parts) -> String {
        let time = self.time(at);
        let indent = time.chars().count() + 2 + LABEL_WIDTH + 2;
        let text_width = self.width.saturating_sub(indent).max(MIN_TEXT_WIDTH);
        let mut body = sanitize(&parts.text);
        if let Some(reference) = &parts.reference {
            body = format!("{body}  {reference}");
        }
        let lines = wrap(&body, text_width);
        assert!(!lines.is_empty(), "wrap always yields a line");
        let pad = " ".repeat(indent);
        let label = format!("{:>LABEL_WIDTH$}", parts.label);
        let mut out = format!(
            "{}  {}  {}",
            self.palette.paint(Style::Dim, &time),
            self.palette.paint(parts.tone, &label),
            self.body_line(parts, &lines[0])
        );
        for line in &lines[1..] {
            out.push('\n');
            out.push_str(&pad);
            out.push_str(&self.body_line(parts, line));
        }
        for note in &parts.notes {
            for line in wrap(&sanitize(note), text_width.saturating_sub(2).max(1)) {
                out.push_str(&format!(
                    "\n{pad}  {}",
                    self.palette.paint(Style::Dim, &line)
                ));
            }
        }
        out
    }

    /// Progress and system notes are dimmed; everything else is plain so the
    /// label carries the colour and the text stays readable.
    fn body_line(&self, parts: &Parts, line: &str) -> String {
        match parts.label {
            "progress" | "system" => self.palette.paint(Style::Dim, line),
            _ => line.to_owned(),
        }
    }

    fn time(&self, at: Timestamp) -> String {
        let local = at.with_timezone(&Local);
        if local.date_naive() == self.today {
            local.format("%H:%M %z").to_string()
        } else {
            local.format("%b %d %H:%M %z").to_string()
        }
    }
}

fn describe(entry: &FeedEntry) -> Parts {
    let simple = |label, tone, text: &str| Parts {
        label,
        tone,
        text: text.to_owned(),
        notes: Vec::new(),
        reference: None,
    };
    match entry {
        FeedEntry::Authority {
            evidence,
            action,
            result,
            ..
        } => simple(
            "authority",
            Style::Dim,
            &format!(
                "{:?} inbox {}: {action}: {result}",
                evidence.channel, evidence.seq
            ),
        ),
        FeedEntry::Operator { text, images } => {
            simple("you", Style::Bold, &with_image_labels(text, images))
        }
        FeedEntry::Orchestrator { text } => simple("orchestrator", Style::Cyan, text),
        FeedEntry::System { text } => simple("system", Style::Dim, text),
        FeedEntry::Notification(n) => {
            let (label, tone) = notification_style(n.kind);
            let mut parts = simple(label, tone, &n.text);
            parts.reference = n.item.as_ref().map(|item| format!("[{item}]"));
            parts
        }
        FeedEntry::Question(question) => describe_question(question),
        FeedEntry::Answer {
            question,
            text,
            via,
        } => {
            let mut parts = simple("answer", Style::Green, text);
            parts.notes = via.note().map(str::to_owned).into_iter().collect();
            parts.reference = Some(format!("[{question}]"));
            parts
        }
        FeedEntry::Report {
            title,
            location,
            item,
            ..
        } => {
            let mut parts = simple("report", Style::Magenta, &format!("{title}\n{location}"));
            parts.reference = item.as_ref().map(|item| format!("[{item}]"));
            parts
        }
    }
}

fn notification_style(kind: NotificationKind) -> (&'static str, Style) {
    match kind {
        NotificationKind::Question => ("question", Style::Yellow),
        NotificationKind::Decision => ("decision", Style::Blue),
        NotificationKind::Blocker => ("blocker", Style::Red),
        NotificationKind::Outcome => ("outcome", Style::Green),
        NotificationKind::Progress => ("progress", Style::Dim),
    }
}

/// A question with its options and id; `archon questions` shows how to
/// answer the open ones.
fn describe_question(question: &Question) -> Parts {
    let notes = question
        .options
        .iter()
        .enumerate()
        .map(|(i, option)| format!("{}) {option}", i + 1))
        .collect();
    Parts {
        label: "question",
        tone: Style::Yellow,
        text: question.text.clone(),
        notes,
        reference: Some(format!("[{}]", question.id)),
    }
}

#[cfg(test)]
mod tests {
    use archon_core::id::{FeedItemId, NotificationId, QuestionId, QueueItemId, ReportId};
    use archon_core::notify::{AnswerVia, Notification};

    use super::*;

    fn item(entry: FeedEntry) -> FeedItem {
        FeedItem {
            id: FeedItemId::new(),
            seq: 7,
            at: archon_core::now(),
            entry,
        }
    }

    fn notification(kind: NotificationKind, text: &str) -> FeedItem {
        item(FeedEntry::Notification(Notification {
            id: NotificationId::new(),
            kind,
            text: text.into(),
            item: None,
            run: None,
            at: archon_core::now(),
        }))
    }

    fn question(options: &[&str]) -> Question {
        Question {
            id: QuestionId::new(),
            number: 2,
            text: "Ship it?".into(),
            options: options.iter().map(|o| (*o).to_owned()).collect(),
            item: None,
            aether: None,
            asked_by: None,
            asked_at: archon_core::now(),
            answer: None,
            answered_at: None,
        }
    }

    /// `HH:MM +0400`, the width of the time column on the current day.
    const HH_MM_ZONE: usize = 11;

    /// The label and first line of text, without the time column.
    fn head(rendered: &str) -> String {
        rendered.lines().next().unwrap()[HH_MM_ZONE..]
            .trim()
            .to_owned()
    }

    #[test]
    fn operator_and_orchestrator_lines_are_labelled() {
        let you = render_feed_item(
            &item(FeedEntry::Operator {
                text: "add caching".into(),
                images: Vec::new(),
            }),
            false,
        );
        assert_eq!(head(&you.unwrap()), "you  add caching");
        let reply = item(FeedEntry::Orchestrator {
            text: "Queued on api.".into(),
        });
        assert_eq!(
            head(&render_feed_item(&reply, false).unwrap()),
            "orchestrator  Queued on api."
        );
    }

    #[test]
    fn a_line_sent_with_images_names_each_image() {
        let shot = archon_core::attachment::ImageRef {
            media_type: "image/png".into(),
            size_bytes: 1536,
            name: Some("shot.png".into()),
        };
        let line = item(FeedEntry::Operator {
            text: "what is this?".into(),
            images: vec![shot.clone()],
        });
        let rendered = render_feed_item(&line, false).unwrap();
        assert_eq!(head(&rendered), "you  what is this?");
        assert!(rendered.contains("[image: shot.png, 1.5 KB]"), "{rendered}");
        let only = item(FeedEntry::Operator {
            text: String::new(),
            images: vec![shot],
        });
        let rendered = render_feed_item(&only, false).unwrap();
        assert_eq!(head(&rendered), "you  [image: shot.png, 1.5 KB]");
    }

    #[test]
    fn each_notification_kind_has_its_own_label() {
        for (kind, label) in [
            (NotificationKind::Decision, "decision"),
            (NotificationKind::Blocker, "blocker"),
            (NotificationKind::Outcome, "outcome"),
            (NotificationKind::Question, "question"),
        ] {
            let rendered = render_feed_item(&notification(kind, "text"), false).unwrap();
            assert_eq!(head(&rendered), format!("{label}  text"));
        }
    }

    #[test]
    fn progress_is_hidden_unless_verbose() {
        let progress = notification(NotificationKind::Progress, "50% done");
        assert_eq!(render_feed_item(&progress, false), None);
        let shown = render_feed_item(&progress, true).unwrap();
        assert_eq!(head(&shown), "progress  50% done");
    }

    #[test]
    fn notifications_reference_their_item() {
        let id = QueueItemId::new();
        let mut entry = notification(NotificationKind::Outcome, "PR #4 opened");
        if let FeedEntry::Notification(n) = &mut entry.entry {
            n.item = Some(id.clone());
        }
        let rendered = render_feed_item(&entry, false).unwrap();
        assert!(
            rendered.contains(&format!("PR #4 opened  [{id}]")),
            "{rendered}"
        );
    }

    #[test]
    fn questions_list_options_and_name_their_id() {
        let q = question(&["yes", "no"]);
        let rendered = render_feed_item(&item(FeedEntry::Question(q.clone())), false).unwrap();
        let lines: Vec<&str> = rendered.lines().collect();
        assert_eq!(head(&rendered), format!("question  Ship it?  [{}]", q.id));
        assert_eq!(lines[1].trim(), "1) yes");
        assert_eq!(lines[2].trim(), "2) no");
    }

    #[test]
    fn answers_name_their_question() {
        let q = question(&[]);
        let answer = item(FeedEntry::Answer {
            question: q.id.clone(),
            text: "yes".into(),
            via: AnswerVia::Archon,
        });
        let bare = render_feed_item(&answer, false).unwrap();
        assert_eq!(head(&bare), format!("answer  yes  [{}]", q.id));
        assert_eq!(bare.lines().count(), 1, "Archon's own surface goes unnamed");
    }

    #[test]
    fn an_answer_given_in_chat_says_so() {
        let q = question(&[]);
        for (via, note) in [
            (AnswerVia::ChatCommand, "answered from operator chat"),
            (
                AnswerVia::ChatReply,
                "answered in chat, recorded by the orchestrator",
            ),
            (
                AnswerVia::NotificationCenter,
                "answered in the notification center",
            ),
        ] {
            let answer = item(FeedEntry::Answer {
                question: q.id.clone(),
                text: "approve".into(),
                via,
            });
            let rendered = render_feed_item(&answer, false).unwrap();
            let lines: Vec<&str> = rendered.lines().collect();
            assert_eq!(lines.len(), 2, "{rendered}");
            assert!(lines[1].contains(note), "{rendered}");
        }
    }

    #[test]
    fn reports_and_system_notes_render() {
        let report = item(FeedEntry::Report {
            report: ReportId::new(),
            title: "E2E results".into(),
            location: "http://127.0.0.1:4000/s/abc".into(),
            item: None,
        });
        let rendered = render_feed_item(&report, false).unwrap();
        assert_eq!(head(&rendered), "report  E2E results");
        assert!(rendered.lines().nth(1).unwrap().trim() == "http://127.0.0.1:4000/s/abc");
        let system = item(FeedEntry::System {
            text: "Aether reconnected".into(),
        });
        assert_eq!(
            head(&render_feed_item(&system, false).unwrap()),
            "system  Aether reconnected"
        );
    }

    #[test]
    fn report_links_are_short_clickable_and_preserve_the_target() {
        let location = format!("/a very long workspace/{}/résults.html", "path/".repeat(30));
        let report = item(FeedEntry::Report {
            report: ReportId::new(),
            title: "E2E results".into(),
            location: location.clone(),
            item: None,
        });
        let mut palette = Palette::new(false);
        palette.hyperlinks = true;
        let renderer = Renderer::new(palette, 40);
        let linked = renderer.render(&report, false).unwrap();
        assert!(linked.contains("\x1b]8;;file:///a%20very%20long%20workspace/"));
        assert!(linked.contains("r%C3%A9sults.html"));
        assert!(linked.ends_with("open report\x1b]8;;\x1b\\"));
        assert_eq!(linked.lines().count(), 2, "the long target never wraps");
        let plain = render_feed_item(&report, false).unwrap();
        assert!(plain.contains(&location));
        assert!(!plain.contains('\x1b'));
        let mut unsafe_report = report;
        if let FeedEntry::Report { location, .. } = &mut unsafe_report.entry {
            *location = "https://example.test/\x1b]52;;evil\x07".into();
        }
        let inert = renderer.render(&unsafe_report, false).unwrap();
        assert!(!inert.contains('\x1b'));
    }

    fn said(kind: EntryKind, text: &str, links: &[&str]) -> ConversationEntry {
        ConversationEntry {
            seq: 9,
            part: 0,
            at: archon_core::now(),
            kind,
            important: true,
            text: text.into(),
            links: links.iter().map(|l| (*l).to_owned()).collect(),
        }
    }

    #[test]
    fn a_conversation_entry_shows_its_text_as_said_under_who_said_it() {
        let cases = [
            (EntryKind::Operator, "you"),
            (EntryKind::Assistant, "orchestrator"),
            (EntryKind::Post, "archon"),
            (EntryKind::Question, "question"),
            (EntryKind::Other, "aether"),
        ];
        for (kind, label) in cases {
            let rendered = render_entry_plain(&said(kind, "**Outcome**: merged", &[]), 100);
            assert_eq!(head(&rendered), format!("{label}  Outcome: merged"));
        }
        let phone = format!("{WEB_RELAY_MARK}thanks");
        let rendered = render_entry_plain(&said(EntryKind::Operator, &phone, &[]), 100);
        assert_eq!(rendered[HH_MM_ZONE..].trim(), "you  thanks");
        let text = "Looking.\n\n- one\n- two";
        let rendered = render_entry_plain(&said(EntryKind::Assistant, text, &[]), 100);
        let body: Vec<&str> = rendered.lines().map(|l| l[HH_MM_ZONE..].trim()).collect();
        assert_eq!(body, ["orchestrator  Looking.", "", "- one", "- two"]);
    }

    #[test]
    fn bold_marks_show_plain_but_literal_asterisks_stay() {
        let text = "**Archon:** aetherd had stopped\n**Decision** (qi_7): `src/**/*.rs` 2 ** 10";
        let rendered = render_entry_plain(&said(EntryKind::Post, text, &[]), 100);
        let body: Vec<&str> = rendered.lines().map(|l| l[HH_MM_ZONE..].trim()).collect();
        assert_eq!(
            body,
            [
                "archon  Archon: aetherd had stopped",
                "Decision (qi_7): `src/**/*.rs` 2 ** 10",
            ]
        );
    }

    #[test]
    fn a_conversation_entry_lists_its_links_below_and_drops_escapes() {
        let entry = said(
            EntryKind::Post,
            "Report\u{1b}[31m",
            &["https://r.example/1"],
        );
        let rendered = render_entry_plain(&entry, 100);
        let lines: Vec<&str> = rendered.lines().collect();
        assert_eq!(lines.len(), 2, "{rendered}");
        assert_eq!(lines[1].trim(), "https://r.example/1");
        assert!(!rendered.contains('\u{1b}'), "{rendered:?}");
    }

    #[test]
    fn long_text_wraps_under_the_text_column() {
        let text = "word ".repeat(60);
        let rendered = render_feed_item(&item(FeedEntry::Orchestrator { text }), false).unwrap();
        let lines: Vec<&str> = rendered.lines().collect();
        assert!(lines.len() > 2);
        let column = lines[0].find("word").unwrap();
        assert!(lines[1..].iter().all(|l| l.find("word") == Some(column)));
        assert!(lines.iter().all(|l| l.chars().count() <= DEFAULT_WIDTH));
    }

    #[test]
    fn escape_sequences_from_agents_are_stripped() {
        let entry = item(FeedEntry::Orchestrator {
            text: "hi\x1b[2J there".into(),
        });
        let rendered = render_feed_item(&entry, false).unwrap();
        assert!(!rendered.contains('\x1b'));
        let colored = Renderer::new(Palette::new(true), 80)
            .render(&entry, false)
            .unwrap();
        assert!(
            colored.contains("\x1b[1;36m"),
            "labels are coloured on a terminal"
        );
    }
}
