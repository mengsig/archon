//! "Reopen it?" questions (ARCHITECTURE §14). A closed review window asks
//! only when an open gate waits on its report, at most once per report, and
//! the question closes once no gate waits any more. Any other report stays
//! `Disconnected` quietly until `archon reports reopen` shows it again.
//!
//! An answer and a withdrawal each close the question in one step under the
//! store lock, conditional on the report still asking it, so exactly one
//! wins: a losing withdrawal does nothing, and a losing answer is refused.
//!
//! A question is tied to its report's page by its text, which names the
//! page's path, so one no record asks any more (an orphan) is found too.

use std::collections::{BTreeMap, HashSet};
use std::path::Path;

use archon_core::Timestamp;
use archon_core::error::ArchonError;
use archon_core::id::{QuestionId, ReportId};
use archon_core::notify::{AnswerVia, FeedEntry, FeedItem, Question};
use archon_core::protocol::ReportState;
use archon_core::run::OperatorGate;
use archon_queue::Store;

use super::records::{self, ReportRecord};
use crate::context::DaemonContext;
use crate::feed;

const REOPEN: &str = "reopen";
const LEAVE_CLOSED: &str = "leave it closed";
const ASK_PREFIX: &str = "The review window for \"";
const ASK_MIDDLE: &str = "\" was closed. Reopen it? (The report is at ";
/// How a reopen question closes once no gate waits on its report.
const NO_GATE: &str = "withdrawn: no gate waits on the report any more";
/// How a reopen question closes when its report is published again.
const SHOWN_AGAIN: &str = "withdrawn: the report was shown again";
/// How an orphan closes while its report's gate waits on a newer question.
const SUPERSEDED: &str = "withdrawn: the report no longer asks it";

/// Mark the report `Disconnected`, and ask whether to reopen it only when
/// an open gate waits on it and no reopen question is open yet. Checked
/// and asked under one store lock, so a gate closing meanwhile is seen.
pub(super) async fn disconnected(
    ctx: &DaemonContext,
    report: &ReportId,
) -> Result<(), ArchonError> {
    let asked = {
        let mut store = ctx.store.lock().await;
        let mut record = records::get(&mut store, report)?
            .ok_or_else(|| ArchonError::Store(format!("report {report} has no record")))?;
        record.state = ReportState::Disconnected;
        let gate = store.open_gate_by_report(report)?;
        let asked = match gate {
            Some(gate) if record.reopen_question.is_none() => {
                Some(ask(&mut store, report, &record, &gate)?)
            }
            _ => None,
        };
        if let Some((question, _)) = &asked {
            record.reopen_question = Some(question.clone());
        }
        records::put(&mut store, report, record, archon_core::now())?;
        asked.map(|(_, item)| item)
    };
    if asked.is_none() {
        tracing::info!(%report, "review window closed; no gate waits, so nothing is asked");
    }
    feed::publish(ctx, asked.into_iter().collect());
    Ok(())
}

/// Lavish says: ask, and neither reopen nor end the session uninvited.
fn ask(
    store: &mut Store,
    report: &ReportId,
    record: &ReportRecord,
    gate: &OperatorGate,
) -> Result<(QuestionId, FeedItem), ArchonError> {
    assert!(gate.is_open(), "only an open gate waits on a report");
    assert_eq!(
        gate.report.as_ref(),
        Some(report),
        "the gate shows this report"
    );
    let text = format!(
        "{ASK_PREFIX}{}{ASK_MIDDLE}{}.)",
        record.title,
        record.html.display()
    );
    let now = archon_core::now();
    let question = Question {
        id: QuestionId::new(),
        number: 0,
        text,
        options: vec![REOPEN.to_owned(), LEAVE_CLOSED.to_owned()],
        item: Some(gate.item.clone()),
        aether: None,
        asked_at: now,
        answer: None,
        answered_at: None,
    };
    let question = store.insert_question(&question)?;
    assert!(question.number > 0, "a recorded question is numbered");
    let item = store.append_feed(FeedEntry::Question(question.clone()), now)?;
    Ok((question.id, item))
}

/// Whether `question` is a report's "Reopen it?" question: one a record
/// asks, or one worded as such that no record asks any more.
pub fn asks_to_reopen(store: &mut Store, question: &Question) -> Result<bool, ArchonError> {
    if is_reopen_question(question) {
        return Ok(true);
    }
    let records = records::all(store)?;
    Ok(records
        .iter()
        .any(|(_, record)| record.reopen_question.as_ref() == Some(&question.id)))
}

/// A "Reopen it?" question as [`ask`] words it, whichever build asked it.
fn is_reopen_question(question: &Question) -> bool {
    question.aether.is_none()
        && question.options == [REOPEN, LEAVE_CLOSED]
        && question.text.starts_with(ASK_PREFIX)
        && question.text.contains(ASK_MIDDLE)
}

/// `question` asks to reopen the page at `html`, which its text ends with.
fn asks_about(question: &Question, html: &Path) -> bool {
    debug_assert!(
        is_reopen_question(question),
        "only reopen questions name a page"
    );
    let ending = format!("{ASK_MIDDLE}{}.)", html.display());
    question.text.ends_with(&ending)
}

/// The report whose page `question` asks to reopen, if it is recorded.
fn report_asked_about<'a>(
    records: &'a BTreeMap<ReportId, ReportRecord>,
    question: &Question,
) -> Option<&'a ReportId> {
    records
        .iter()
        .find(|(_, record)| asks_about(question, &record.html))
        .map(|(report, _)| report)
}

/// A reopen question the operator's answer won, recorded as answered.
enum Claimed {
    /// Its report no longer asks it; the answer decides whether it reopens.
    Asked {
        report: ReportId,
        record: ReportRecord,
        question: Question,
        echo: FeedItem,
    },
    /// An orphan no report asks: the answer only closes it. `report`
    /// is the report of the page it names, if one is recorded.
    Orphan {
        report: Option<ReportId>,
        question: Question,
        echo: FeedItem,
    },
}

/// Act on the answer to a "Reopen it?" question: record it, then reopen
/// the report in Lavish (its poll restarts from the feed entry) or leave it
/// closed. An orphan's answer only closes it. `Some` with the recorded
/// question, which the caller must not record again; `None` when
/// `question` asked something else.
pub async fn answer_reopen(
    ctx: &DaemonContext,
    question: &QuestionId,
    answer: &str,
    via: AnswerVia,
) -> Result<Option<Question>, ArchonError> {
    assert!(!answer.trim().is_empty(), "an answer says something");
    let claimed = claim(&mut *ctx.store.lock().await, question, answer, via)?;
    let answer = answer.trim();
    let wants_reopen = answer.eq_ignore_ascii_case(REOPEN) || answer.eq_ignore_ascii_case("yes");
    match claimed {
        None => Ok(None),
        Some(Claimed::Asked {
            report,
            record,
            question,
            echo,
        }) => {
            feed::publish(ctx, vec![echo]);
            if wants_reopen {
                reopen_asked(ctx, &report, record).await?;
            }
            Ok(Some(question))
        }
        Some(Claimed::Orphan {
            report,
            question,
            echo,
        }) => {
            feed::publish(ctx, vec![echo]);
            orphan_answered(ctx, report.as_ref(), &question, wants_reopen).await?;
            Ok(Some(question))
        }
    }
}

/// Answer `question` and stop its report asking it, in one step under the
/// store lock that [`close_stale_questions`] also takes, so exactly one of
/// the two closes it. `None` for an open question that is not a reopen
/// question; a closed one is refused, saying why it closed.
fn claim(
    store: &mut Store,
    question: &QuestionId,
    answer: &str,
    via: AnswerVia,
) -> Result<Option<Claimed>, ArchonError> {
    let records = records::all(store)?;
    let asking = records
        .iter()
        .find(|(_, record)| record.reopen_question.as_ref() == Some(question));
    let Some((report, record)) = asking else {
        let current = store.question(question)?;
        if !current.is_open() {
            return Err(already_closed(&current));
        }
        if !is_reopen_question(&current) {
            return Ok(None);
        }
        let report = report_asked_about(&records, &current).cloned();
        let (question, echo) = record_answer(store, question, answer, via)?;
        return Ok(Some(Claimed::Orphan {
            report,
            question,
            echo,
        }));
    };
    assert_eq!(
        record.state,
        ReportState::Disconnected,
        "only a disconnected report asks to reopen"
    );
    let (question, echo) = record_answer(store, question, answer, via)?;
    let mut record = record.clone();
    record.reopen_question = None;
    records::put(store, report, record.clone(), archon_core::now())?;
    Ok(Some(Claimed::Asked {
        report: report.clone(),
        record,
        question,
        echo,
    }))
}

/// Record the operator's answer to `question` and its feed echo, which the
/// caller broadcasts.
fn record_answer(
    store: &mut Store,
    question: &QuestionId,
    answer: &str,
    via: AnswerVia,
) -> Result<(Question, FeedItem), ArchonError> {
    let now = archon_core::now();
    let answered = store.answer_question(question, answer, now)?;
    assert!(!answered.is_open(), "an answered question is closed");
    assert_eq!(
        &answered.id, question,
        "the store answers the question asked"
    );
    let entry = FeedEntry::Answer {
        question: question.clone(),
        text: answer.to_owned(),
        via,
    };
    let echo = store.append_feed(entry, now)?;
    Ok((answered, echo))
}

/// Acknowledge an answer to an orphaned reopen question: it is closed and
/// nothing reopens, so an answer asking to reopen says how to.
async fn orphan_answered(
    ctx: &DaemonContext,
    report: Option<&ReportId>,
    question: &Question,
    wants_reopen: bool,
) -> Result<(), ArchonError> {
    assert!(!question.is_open(), "the orphan's answer is recorded");
    tracing::info!(
        question = %question.id,
        report = ?report,
        "answered a reopen question no report asks any more; it is closed and nothing is reopened"
    );
    if !wants_reopen {
        return Ok(());
    }
    let number = question.number;
    let text = match report {
        Some(report) => format!(
            "Q{number} is out of date: its report no longer asks it, so nothing was reopened. `archon reports reopen {report}` shows it again."
        ),
        None => format!(
            "Q{number} is out of date: its report is no longer recorded, so nothing was reopened."
        ),
    };
    feed::system(ctx, text).await.map(drop)
}

/// Show the report again as its answer asked. The answer is recorded
/// already, so a failure is told in the feed and the report stays
/// `Disconnected`; only a store failure is returned.
async fn reopen_asked(
    ctx: &DaemonContext,
    report: &ReportId,
    record: ReportRecord,
) -> Result<(), ArchonError> {
    assert_eq!(
        record.state,
        ReportState::Disconnected,
        "it asked to reopen"
    );
    assert!(record.reopen_question.is_none(), "its question is answered");
    let shown = match &ctx.lavish {
        Some(lavish) => super::show_again(ctx, lavish, report).await,
        None => Err(ArchonError::Unavailable(format!(
            "Lavish is not available to show \"{}\" ({report}) again. It is at {}",
            record.title,
            record.html.display()
        ))),
    };
    match shown {
        Ok(published) => assert!(published.published, "shown in Lavish"),
        Err(error) if error.is_store() => return Err(error),
        Err(error) => {
            tracing::warn!(%error, %report, "reopening an answered report failed");
            let text = format!("{error}. `archon reports reopen {report}` tries again.");
            feed::system(ctx, text).await?;
        }
    }
    Ok(())
}

/// The refusal of an answer to `question`, which closed before the answer
/// was recorded. A reopen question the reports withdrew says why.
pub fn already_closed(question: &Question) -> ArchonError {
    assert!(!question.is_open(), "an open question takes an answer");
    let why = match question.answer.as_deref() {
        Some(NO_GATE) => "its report's gate closed",
        Some(SHOWN_AGAIN) => "its report was shown again",
        Some(SUPERSEDED) => "its report no longer asks it",
        _ => {
            return ArchonError::InvalidTransition(format!(
                "question {} was already answered",
                question.id
            ));
        }
    };
    ArchonError::InvalidTransition(format!(
        "Q{} was closed because {why}, so your answer was not delivered",
        question.number
    ))
}

/// Close every reopen question whose report no open gate waits on: its
/// gate was decided, its run finished, or an older archond asked about an
/// informational page. Orphans no report asks any more close too. Run when
/// a gate closes and at startup. Returns how many questions it closed.
pub async fn close_stale_questions(ctx: &DaemonContext) -> Result<usize, ArchonError> {
    let closed = {
        let mut store = ctx.store.lock().await;
        let now = archon_core::now();
        let mut closed = release_ungated(&mut store, now)?;
        closed.extend(close_orphans(&mut store, now)?);
        closed
    };
    let count = closed.len();
    if count > 0 {
        tracing::info!(count, "closed reopen questions no gate waits on");
    }
    feed::publish(ctx, closed);
    Ok(count)
}

/// Stop every report no open gate waits on asking to reopen, closing the
/// question it asks; the caller broadcasts the returned items.
fn release_ungated(store: &mut Store, now: Timestamp) -> Result<Vec<FeedItem>, ArchonError> {
    let mut closed = Vec::new();
    for (report, mut record) in records::all(store)? {
        let Some(question) = record.reopen_question.take() else {
            continue;
        };
        if store.open_gate_by_report(&report)?.is_some() {
            continue;
        }
        closed.extend(withdraw(store, &question, NO_GATE, now)?);
        records::put(store, &report, record, now)?;
    }
    Ok(closed)
}

/// Close every open reopen question no report asks any more. Older builds
/// left one behind when a page was published again.
fn close_orphans(store: &mut Store, now: Timestamp) -> Result<Vec<FeedItem>, ArchonError> {
    let records = records::all(store)?;
    let asked: HashSet<&QuestionId> = records
        .values()
        .filter_map(|record| record.reopen_question.as_ref())
        .collect();
    let mut closed = Vec::new();
    for question in store.questions(true)? {
        assert!(question.is_open(), "only open questions are listed");
        if !is_reopen_question(&question) || asked.contains(&question.id) {
            continue;
        }
        let why = match report_asked_about(&records, &question) {
            Some(report) if store.open_gate_by_report(report)?.is_some() => SUPERSEDED,
            _ => NO_GATE,
        };
        tracing::info!(question = %question.id, why, "closing an orphaned reopen question");
        closed.extend(withdraw(store, &question.id, why, now)?);
    }
    Ok(closed)
}

/// Close the reopen questions about the page at `html`, `asked` (its
/// report's own) and any orphan, because the page is shown again; the
/// caller broadcasts the returned items.
pub(super) fn withdraw_shown_again(
    store: &mut Store,
    html: &Path,
    asked: Option<&QuestionId>,
    now: Timestamp,
) -> Result<Vec<FeedItem>, ArchonError> {
    assert!(html.is_absolute(), "report paths are validated");
    let mut ids: Vec<QuestionId> = store
        .questions(true)?
        .into_iter()
        .filter(|question| is_reopen_question(question) && asks_about(question, html))
        .map(|question| question.id)
        .collect();
    if let Some(asked) = asked.filter(|asked| !ids.contains(asked)) {
        ids.push(asked.clone());
    }
    let mut closed = Vec::new();
    for question in &ids {
        closed.extend(withdraw(store, question, SHOWN_AGAIN, now)?);
    }
    Ok(closed)
}

/// Close `question` with `text` and echo it to the feed. `None` when it was
/// answered already; the caller broadcasts the returned item.
fn withdraw(
    store: &mut Store,
    question: &QuestionId,
    text: &str,
    now: Timestamp,
) -> Result<Option<FeedItem>, ArchonError> {
    assert!(
        text.starts_with("withdrawn: "),
        "reads like a gate's withdrawal"
    );
    match store.answer_question(question, text, now) {
        Ok(closed) => assert!(!closed.is_open(), "an answered question is closed"),
        Err(ArchonError::InvalidTransition(_)) => return Ok(None),
        Err(error) => return Err(error),
    }
    let entry = FeedEntry::Answer {
        question: question.clone(),
        text: text.to_owned(),
        via: AnswerVia::Archon,
    };
    store.append_feed(entry, now).map(Some)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reopen_question(title: &str, html: &str) -> Question {
        Question {
            id: QuestionId::new(),
            number: 1,
            text: format!("{ASK_PREFIX}{title}{ASK_MIDDLE}{html}.)"),
            options: vec![REOPEN.to_owned(), LEAVE_CLOSED.to_owned()],
            item: None,
            aether: None,
            asked_at: archon_core::now(),
            answer: None,
            answered_at: None,
        }
    }

    /// A question is tied to its page by the whole path it names, and
    /// only a question worded and offered as a reopen question counts.
    #[test]
    fn a_reopen_question_names_exactly_one_page() {
        let question = reopen_question("Merged PR #7 (report stage)", "/w/a/index.html");
        assert!(is_reopen_question(&question));
        assert!(asks_about(&question, Path::new("/w/a/index.html")));
        assert!(
            !asks_about(&question, Path::new("/a/index.html")),
            "a suffix"
        );
        assert!(!asks_about(&question, Path::new("/w/a/index.htm")));

        let other_options = Question {
            options: vec!["yes".into(), "no".into()],
            ..question.clone()
        };
        assert!(!is_reopen_question(&other_options));
        let other_text = Question {
            text: "Reopen it? (The report is at /w/a/index.html.)".into(),
            ..question
        };
        assert!(!is_reopen_question(&other_text));
    }
}
