//! Talking to the orchestrator outside the chat: `say`, `answer` and
//! `questions`.

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use archon_client::ArchonClient;
use archon_core::attachment::check_images;
use archon_core::notify::Question;
use archon_core::protocol::{Reply, Request};

use super::{emit, to_json};
use crate::connect::{connect_or_start, connect_running, unexpected};
use crate::format;
use crate::images::read_image_file;
use crate::ops;
use crate::questions::{QuestionRef, find_open};

const SAID: &str = "Sent to the orchestrator. Follow its work with `archon log --follow`, \
                    or talk with it in `archon chat`.";
const SAY_ATTEMPTS: u32 = 4;
const SAY_RETRY_DELAY: Duration = Duration::from_secs(3);

/// Run `op` up to `attempts` times, waiting `delay` after each `Unavailable` (aetherd
/// restarting); any other error, or the last one, is returned with what was tried.
async fn retry_unavailable<T, F, Fut>(
    attempts: u32,
    delay: Duration,
    mut op: F,
) -> anyhow::Result<T>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<T>>,
{
    assert!(attempts > 0, "at least one attempt");
    let mut tried = 1;
    loop {
        let error = match op().await {
            Ok(done) => return Ok(done),
            Err(error) => error,
        };
        let unavailable = matches!(
            error.downcast_ref::<archon_core::ArchonError>(),
            Some(archon_core::ArchonError::Unavailable(_))
        );
        if !unavailable {
            return Err(error);
        }
        if tried == attempts {
            return Err(error.context(format!(
                "the message was not sent: tried {tried} times over {}s; aetherd may still be \
                 restarting, so retry in a few seconds with the same `archon say`, or check \
                 `archon doctor`",
                delay.as_secs() * u64::from(tried - 1)
            )));
        }
        eprintln!("archon: the orchestrator is not reachable yet; retrying ({tried}/{attempts})");
        tokio::time::sleep(delay).await;
        tried += 1;
    }
}

pub async fn say(words: &[String], paths: &[PathBuf], json: bool) -> anyhow::Result<ExitCode> {
    let text = words.join(" ");
    if text.trim().is_empty() && paths.is_empty() {
        return Err(archon_core::ArchonError::Invalid(
            "nothing to say; usage: archon say [--image <path>]... <text>".into(),
        )
        .into());
    }
    let images = paths
        .iter()
        .map(|path| read_image_file(path).map_err(archon_core::ArchonError::Invalid))
        .collect::<Result<Vec<_>, _>>()?;
    check_images(&images).map_err(|e| archon_core::ArchonError::Invalid(e.to_string()))?;
    assert_eq!(images.len(), paths.len(), "one attachment per --image");
    let client = connect_or_start().await?;
    retry_unavailable(SAY_ATTEMPTS, SAY_RETRY_DELAY, || {
        ops::say(&client, text.clone(), images.clone())
    })
    .await?;
    emit(json, || Ok(serde_json::json!({"ok": true})), SAID)?;
    Ok(ExitCode::SUCCESS)
}

/// `question` is a question's permanent number from `archon questions` (`2`
/// or `Q2`) or its q_ id. A bare number as the answer picks that option.
pub async fn answer(question: &str, words: &[String], json: bool) -> anyhow::Result<ExitCode> {
    let reference = QuestionRef::parse(question).map_err(archon_core::ArchonError::Invalid)?;
    let text = words.join(" ");
    if text.trim().is_empty() {
        return Err(archon_core::ArchonError::Invalid(
            "no answer given; usage: archon answer <n|id> <text>".into(),
        )
        .into());
    }
    let client = connect_or_start().await?;
    let open = open_questions(&client).await?;
    let Some(found) = find_open(&open, &reference) else {
        return Err(archon_core::ArchonError::InvalidTransition(format!(
            "{question} is not an open question (it was answered, or never asked); \
             `archon questions` lists the open ones"
        ))
        .into());
    };
    let text = ops::answer(&client, found, &text, false).await?;
    let done = format::answered(found, &text);
    emit(
        json,
        || {
            Ok(serde_json::json!({
                "question": found.id,
                "number": found.number,
                "answer": text,
            }))
        },
        &done,
    )?;
    Ok(ExitCode::SUCCESS)
}

pub async fn questions(json: bool) -> anyhow::Result<ExitCode> {
    let client = connect_running().await?;
    let open = open_questions(&client).await?;
    let text = format::questions(&open);
    emit(json, || to_json(&open), &text)?;
    Ok(ExitCode::SUCCESS)
}

pub async fn clear_notifications(
    item: Option<&str>,
    all: bool,
    json: bool,
) -> anyhow::Result<ExitCode> {
    assert!(
        item.is_some() != all,
        "clap takes an item or --all, not both"
    );
    let client = connect_or_start().await?;
    let item = match item {
        Some(item) => Some(crate::views::resolve_item(&client, item).await?),
        None => None,
    };
    let text = match &item {
        Some(id) => format!("Asked to resolve the open blockers of {id}."),
        None => "Asked to resolve every open blocker and outcome.".to_owned(),
    };
    match client.request(Request::ClearNotifications { item }).await? {
        Reply::Ok => {}
        other => return Err(unexpected(&other, "ok")),
    }
    emit(json, || Ok(serde_json::json!({"requested": true})), &text)?;
    Ok(ExitCode::SUCCESS)
}

/// Open questions by number, as `archon questions` shows them.
async fn open_questions(client: &ArchonClient) -> anyhow::Result<Vec<Question>> {
    let mut open = match client
        .request(Request::ListQuestions { open_only: true })
        .await?
    {
        Reply::Questions(open) => open,
        other => return Err(unexpected(&other, "questions")),
    };
    open.retain(Question::is_open);
    open.sort_by_key(|q| q.number);
    Ok(open)
}

#[cfg(test)]
mod tests {
    use super::*;
    use archon_core::ArchonError;
    use std::cell::Cell;

    fn down() -> anyhow::Error {
        ArchonError::Unavailable("not connected to aetherd".into()).into()
    }

    #[tokio::test]
    async fn an_unavailable_send_is_retried_until_it_goes_through() {
        let calls = Cell::new(0);
        let result = retry_unavailable(4, Duration::ZERO, || async {
            calls.set(calls.get() + 1);
            if calls.get() < 3 { Err(down()) } else { Ok(()) }
        })
        .await;
        assert!(result.is_ok());
        assert_eq!(calls.get(), 3);
    }

    #[tokio::test]
    async fn a_send_that_stays_unavailable_ends_with_the_next_step() {
        let calls = Cell::new(0);
        let error = retry_unavailable(3, Duration::ZERO, || async {
            calls.set(calls.get() + 1);
            Err::<(), _>(down())
        })
        .await
        .unwrap_err();
        assert_eq!(calls.get(), 3);
        let said = format!("{error:#}");
        assert!(
            said.contains("not sent") && said.contains("retry in a few seconds"),
            "{said}"
        );
        assert!(said.contains("not connected to aetherd"), "{said}");
    }

    #[tokio::test]
    async fn another_error_is_not_retried() {
        let calls = Cell::new(0);
        let error = retry_unavailable(4, Duration::ZERO, || async {
            calls.set(calls.get() + 1);
            Err::<(), _>(ArchonError::Invalid("bad".into()).into())
        })
        .await
        .unwrap_err();
        assert_eq!(calls.get(), 1);
        assert!(error.to_string().contains("bad"));
    }

    #[test]
    fn the_reply_names_the_next_commands() {
        assert!(SAID.contains("`archon log --follow`") && SAID.contains("`archon chat`"));
    }
}
