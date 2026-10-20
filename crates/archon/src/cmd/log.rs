//! `archon log`: the feed with progress, optionally narrowed to one item's
//! entries or some kinds of entry, and optionally followed as it grows.

use std::collections::HashSet;
use std::io::{ErrorKind, Write as _};
use std::process::ExitCode;

use anyhow::Context as _;
use archon_client::ArchonClient;
use archon_core::id::{QuestionId, QueueItemId};
use archon_core::notify::{FeedEntry, FeedItem, NotificationKind};
use archon_core::protocol::{Reply, Request};

use super::to_json;
use crate::cli::LogKind;
use crate::connect::{connect_running, unexpected};
use crate::render::{QuestionLabels, Renderer};
use crate::style::{Palette, terminal_width};
use crate::views;

/// Most entries one `FeedHistory` returns (archond's own bound).
const PAGE: usize = 1_000;

/// Which feed entries `archon log` prints.
#[derive(Debug)]
struct LogFilter {
    item: Option<QueueItemId>,
    /// The item's questions, so their answers (which name only the
    /// question) count as the item's too.
    questions: HashSet<QuestionId>,
    kinds: Vec<LogKind>,
}

impl LogFilter {
    fn new(item: Option<QueueItemId>, questions: HashSet<QuestionId>, kinds: Vec<LogKind>) -> Self {
        assert!(
            item.is_some() || questions.is_empty(),
            "questions narrow an item filter only"
        );
        Self {
            item,
            questions,
            kinds,
        }
    }

    fn is_open(&self) -> bool {
        self.item.is_none() && self.kinds.is_empty()
    }

    /// Whether `entry` passes; a question about the item is remembered so a
    /// later answer to it passes too.
    fn admits(&mut self, entry: &FeedItem) -> bool {
        let kind_ok = self.kinds.is_empty() || self.kinds.contains(&kind_of(&entry.entry));
        let Some(item) = &self.item else {
            return kind_ok;
        };
        let about = match &entry.entry {
            FeedEntry::Notification(n) => n.item.as_ref() == Some(item),
            FeedEntry::Question(q) if q.item.as_ref() == Some(item) => {
                self.questions.insert(q.id.clone());
                true
            }
            FeedEntry::Report { item: of, .. } => of.as_ref() == Some(item),
            FeedEntry::Answer { question, .. } => self.questions.contains(question),
            FeedEntry::Question(_)
            | FeedEntry::Operator { .. }
            | FeedEntry::Orchestrator { .. }
            | FeedEntry::Authority { .. }
            | FeedEntry::System { .. } => false,
        };
        kind_ok && about
    }
}

fn kind_of(entry: &FeedEntry) -> LogKind {
    match entry {
        FeedEntry::Operator { .. } => LogKind::Operator,
        FeedEntry::Orchestrator { .. } => LogKind::Orchestrator,
        FeedEntry::Question(_) => LogKind::Question,
        FeedEntry::Answer { .. } => LogKind::Answer,
        FeedEntry::Report { .. } => LogKind::Report,
        FeedEntry::System { .. } | FeedEntry::Authority { .. } => LogKind::System,
        FeedEntry::Notification(n) => match n.kind {
            NotificationKind::Question => LogKind::Question,
            NotificationKind::Decision => LogKind::Decision,
            NotificationKind::Blocker => LogKind::Blocker,
            NotificationKind::Outcome => LogKind::Outcome,
            NotificationKind::Progress => LogKind::Progress,
        },
    }
}

/// Prints entries as the chat does, or as one JSON object per line.
enum Output {
    Text(Renderer),
    JsonLines,
}

/// Whether stdout still has a reader after a print.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Reader {
    Present,
    /// It closed the pipe (`archon log --follow | head`): a clean end.
    Gone,
}

impl Output {
    fn print(&self, entry: &FeedItem) -> anyhow::Result<Reader> {
        let text = match self {
            Self::Text(renderer) => {
                match renderer.render(entry, true, &QuestionLabels::default()) {
                    Some(text) => text,
                    None => return Ok(Reader::Present),
                }
            }
            Self::JsonLines => to_json(entry)?.to_string(),
        };
        write_line(&text)
    }
}

/// Print `text` and a newline; a reader that closed the pipe is not an error.
fn write_line(text: &str) -> anyhow::Result<Reader> {
    let mut stdout = std::io::stdout().lock();
    match writeln!(stdout, "{text}").and_then(|()| stdout.flush()) {
        Ok(()) => Ok(Reader::Present),
        Err(e) if e.kind() == ErrorKind::BrokenPipe => Ok(Reader::Gone),
        Err(e) => Err(anyhow::Error::new(e).context("writing the feed to stdout")),
    }
}

/// `limit` newest matching entries, oldest first; with `follow`, then every
/// new matching one until the feed ends.
pub async fn log(
    limit: usize,
    item: Option<&str>,
    kinds: Vec<LogKind>,
    follow: bool,
    json: bool,
) -> anyhow::Result<ExitCode> {
    if limit == 0 {
        return Err(archon_core::ArchonError::Invalid("--limit must be at least 1".into()).into());
    }
    let client = connect_running().await?;
    let mut filter = match item {
        Some(text) => {
            let item = views::resolve_item(&client, text).await?;
            let questions = item_questions(&client, &item).await?;
            LogFilter::new(Some(item), questions, kinds)
        }
        None => LogFilter::new(None, HashSet::new(), kinds),
    };
    let (entries, newest) = history(&client, &mut filter, limit).await?;
    if json && !follow {
        let text =
            serde_json::to_string_pretty(&to_json(&entries)?).context("encoding JSON output")?;
        // Printed last, so a reader gone already ends it just as cleanly.
        write_line(&text)?;
        return Ok(ExitCode::SUCCESS);
    }
    let output = match json {
        true => Output::JsonLines,
        false => Output::Text(Renderer::new(Palette::detect(), terminal_width())),
    };
    for entry in &entries {
        if output.print(entry)? == Reader::Gone {
            return Ok(ExitCode::SUCCESS);
        }
    }
    if !follow {
        return Ok(ExitCode::SUCCESS);
    }
    follow_feed(&client, &mut filter, newest, &output).await
}

/// Every question asked about `item`, answered or not.
async fn item_questions(
    client: &ArchonClient,
    item: &QueueItemId,
) -> anyhow::Result<HashSet<QuestionId>> {
    let request = Request::ListQuestions { open_only: false };
    let questions = match client.request(request).await? {
        Reply::Questions(questions) => questions,
        other => return Err(unexpected(&other, "questions")),
    };
    Ok(questions
        .into_iter()
        .filter(|q| q.item.as_ref() == Some(item))
        .map(|q| q.id)
        .collect())
}

/// The newest `limit` entries `filter` admits, oldest first, paging back
/// through the feed as far as needed, and the newest seq archond had (0
/// for an empty feed), which following continues after.
async fn history(
    client: &ArchonClient,
    filter: &mut LogFilter,
    limit: usize,
) -> anyhow::Result<(Vec<FeedItem>, u64)> {
    assert!(limit > 0, "the limit is checked first");
    let page_size = if filter.is_open() {
        limit.min(PAGE)
    } else {
        PAGE
    };
    let (mut kept, mut newest, mut before) = (Vec::new(), None, None);
    loop {
        let page = feed_page(client, before, page_size).await?;
        newest = newest.or(page.last().map(|entry| entry.seq));
        for entry in page.iter().rev() {
            if kept.len() < limit && filter.admits(entry) {
                kept.push(entry.clone());
            }
        }
        before = match page.first() {
            Some(oldest) if kept.len() < limit && page.len() == page_size => Some(oldest.seq),
            _ => break,
        };
    }
    kept.reverse();
    assert!(kept.len() <= limit, "at most the entries asked for");
    Ok((kept, newest.unwrap_or(0)))
}

/// Entries before `before` (the newest when `None`), oldest first.
async fn feed_page(
    client: &ArchonClient,
    before: Option<u64>,
    limit: usize,
) -> anyhow::Result<Vec<FeedItem>> {
    let request = Request::FeedHistory {
        before,
        limit,
        verbose: true,
    };
    let page = match client.request(request).await? {
        Reply::Feed(page) => page,
        other => return Err(unexpected(&other, "feed")),
    };
    assert!(page.len() <= limit, "archond honours the history limit");
    assert!(
        page.windows(2).all(|w| w[0].seq < w[1].seq)
            && before.is_none_or(|b| page.iter().all(|e| e.seq < b)),
        "a history page is in seq order and before its cursor"
    );
    Ok(page)
}

async fn follow_feed(
    client: &ArchonClient,
    filter: &mut LogFilter,
    after: u64,
    output: &Output,
) -> anyhow::Result<ExitCode> {
    let mut feed = client.subscribe_feed(Some(after), true).await?;
    while let Some(entry) = feed.items.recv().await {
        if filter.admits(&entry) && output.print(&entry)? == Reader::Gone {
            return Ok(ExitCode::SUCCESS);
        }
    }
    anyhow::bail!("the feed stopped: {}", feed.close_reason())
}

#[cfg(test)]
mod tests {
    use archon_core::id::{FeedItemId, NotificationId, ReportId};
    use archon_core::notify::{AnswerVia, Notification, Question};

    use super::*;

    fn feed(seq: u64, entry: FeedEntry) -> FeedItem {
        FeedItem {
            id: FeedItemId::new(),
            seq,
            at: archon_core::now(),
            entry,
        }
    }

    fn notice(kind: NotificationKind, item: Option<&QueueItemId>) -> FeedEntry {
        FeedEntry::Notification(Notification {
            id: NotificationId::new(),
            kind,
            text: "t".into(),
            item: item.cloned(),
            run: None,
            at: archon_core::now(),
        })
    }

    fn question(item: Option<&QueueItemId>) -> Question {
        Question {
            id: QuestionId::new(),
            number: 1,
            text: "ship?".into(),
            options: vec![],
            item: item.cloned(),
            aether: None,
            asked_by: None,
            asked_at: archon_core::now(),
            answer: None,
            answered_at: None,
        }
    }

    fn answer(question: &Question) -> FeedEntry {
        FeedEntry::Answer {
            question: question.id.clone(),
            text: "yes".into(),
            via: AnswerVia::Archon,
        }
    }

    /// The seqs of `entries` that `filter` admits, in order.
    fn admitted(filter: &mut LogFilter, entries: &[FeedItem]) -> Vec<u64> {
        entries
            .iter()
            .filter(|e| filter.admits(e))
            .map(|e| e.seq)
            .collect()
    }

    #[test]
    fn an_item_filter_keeps_its_notices_questions_answers_and_reports() {
        let (mine, other) = (QueueItemId::new(), QueueItemId::new());
        let (asked_before, asked_now) = (question(Some(&mine)), question(Some(&mine)));
        let theirs = question(Some(&other));
        let report = FeedEntry::Report {
            report: ReportId::new(),
            title: "r".into(),
            location: "/r.html".into(),
            item: Some(mine.clone()),
        };
        let entries = [
            feed(1, notice(NotificationKind::Progress, Some(&mine))),
            feed(2, notice(NotificationKind::Outcome, Some(&other))),
            feed(
                3,
                FeedEntry::Operator {
                    text: "hi".into(),
                    images: Vec::new(),
                },
            ),
            feed(4, answer(&asked_before)),
            feed(5, FeedEntry::Question(asked_now.clone())),
            feed(6, answer(&asked_now)),
            feed(7, FeedEntry::Question(theirs.clone())),
            feed(8, answer(&theirs)),
            feed(9, report),
            feed(10, FeedEntry::System { text: "up".into() }),
        ];
        let known = HashSet::from([asked_before.id.clone()]);
        let mut filter = LogFilter::new(Some(mine), known, vec![]);
        assert!(!filter.is_open());
        assert_eq!(admitted(&mut filter, &entries), [1, 4, 5, 6, 9]);
    }

    #[test]
    fn kinds_filter_alone_or_with_an_item() {
        let item = QueueItemId::new();
        let asked = question(Some(&item));
        let entries = [
            feed(1, notice(NotificationKind::Outcome, Some(&item))),
            feed(2, notice(NotificationKind::Outcome, None)),
            feed(3, notice(NotificationKind::Question, Some(&item))),
            feed(4, FeedEntry::Question(asked.clone())),
            feed(5, notice(NotificationKind::Progress, Some(&item))),
            feed(6, FeedEntry::Orchestrator { text: "ok".into() }),
        ];
        let kinds = vec![LogKind::Outcome, LogKind::Question];
        let mut any_item = LogFilter::new(None, HashSet::new(), kinds.clone());
        assert_eq!(admitted(&mut any_item, &entries), [1, 2, 3, 4]);
        let mut one_item = LogFilter::new(Some(item), HashSet::new(), kinds);
        assert_eq!(admitted(&mut one_item, &entries), [1, 3, 4]);
        let mut open = LogFilter::new(None, HashSet::new(), vec![]);
        assert!(open.is_open());
        assert_eq!(admitted(&mut open, &entries), [1, 2, 3, 4, 5, 6]);
    }
}
