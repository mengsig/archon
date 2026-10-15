//! The operator's `/merge` chat command (ARCHITECTURE §18.6). A message the
//! operator sends the orchestrator in Aether that is exactly
//! `/merge <item> [--ignore-checks]` merges the item's pull request through
//! the path `archon merge` uses; how it ended reaches the orchestrator as
//! `merge_finished` news, or `chat_merge_refused` when nothing began.
//!
//! As with `/answer`, the pump calls [`handle`] only for a message Aether
//! authenticated as the `user` actor, and only dedicated mode applies it.

use archon_core::config::AetherMode;
use archon_core::error::ArchonError;
use archon_core::merge::MergeAuthority;

use crate::context::DaemonContext;
use crate::executor::{clip, operator_merge};
use crate::triggers::{self, TriggerEvent};
use crate::{feed, handlers};

const COMMAND: &str = "/merge";
const IGNORE_CHECKS: &str = "--ignore-checks";
const USAGE: &str =
    "usage: `/merge <item id or unique prefix> [--ignore-checks]`, alone in the message";
const SHARED_REFUSAL: &str = "chat merges are off while Archon shares the operator's Aether instance (`[aether] mode = \"shared\"`), where a `[listen]` token client is the same `user` actor as the operator; merge with `archon merge <item>`";
/// Longest reason one `chat_merge_refused` event carries.
const MAX_NEWS_CHARS: usize = 500;

/// A well-formed `/merge` command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Command {
    item: String,
    ignore_checks: bool,
}

/// `None` unless the whole message is a `/merge` command, so one quoted or
/// embedded in other text is never applied. `Some(Err)` says what is wrong.
pub(crate) fn parse(message: &str) -> Option<Result<Command, String>> {
    let message = message.trim();
    let name = message.get(..COMMAND.len())?;
    if !name.eq_ignore_ascii_case(COMMAND) {
        return None;
    }
    let rest = &message[COMMAND.len()..];
    if !rest.is_empty() && !rest.starts_with(char::is_whitespace) {
        return None;
    }
    let words: Vec<&str> = rest.split_whitespace().collect();
    let parsed = match words.as_slice() {
        [item] if !item.starts_with('-') => (item, false),
        [item, flag] | [flag, item] if *flag == IGNORE_CHECKS && !item.starts_with('-') => {
            (item, true)
        }
        [] => return Some(Err(format!("no item was named; {USAGE}"))),
        _ => {
            return Some(Err(format!(
                "`{rest}` is not one item; {USAGE}",
                rest = rest.trim()
            )));
        }
    };
    let (item, ignore_checks) = parsed;
    assert!(!item.is_empty(), "split_whitespace yields no empty word");
    Some(Ok(Command {
        item: (*item).to_owned(),
        ignore_checks,
    }))
}

/// Apply `message` if it is a `/merge` command. Only a store failure is
/// returned: the pump shows a message here once, so a refusal is reported,
/// never retried.
pub(crate) async fn handle(
    ctx: &DaemonContext,
    message: &str,
    authority: MergeAuthority,
) -> Result<(), ArchonError> {
    let Some(parsed) = parse(message) else {
        return Ok(());
    };
    let refusal = match parsed {
        Ok(command) => apply(ctx, &command, authority).await?,
        Err(usage) => Some(usage),
    };
    match refusal {
        Some(why) => refused(ctx, &why).await,
        None => Ok(()),
    }
}

/// `Some(reason)` when no merge began; a merge that did is reported by the
/// merge path itself.
async fn apply(
    ctx: &DaemonContext,
    command: &Command,
    authority: MergeAuthority,
) -> Result<Option<String>, ArchonError> {
    if ctx.config.aether.mode != AetherMode::Dedicated
        && !matches!(authority, MergeAuthority::Github { .. })
    {
        return Ok(Some(SHARED_REFUSAL.to_owned()));
    }
    let item = match handlers::resolve_item(ctx, &command.item).await {
        Ok(item) => item,
        Err(error) if error.is_store() => return Err(error),
        Err(error) => return Ok(Some(error.to_string())),
    };
    match operator_merge::merge_now(ctx, &item, authority, command.ignore_checks).await {
        Ok(merge) => {
            assert_eq!(merge.item, item, "the item named was merged");
            Ok(None)
        }
        Err(error) if error.is_store() => Err(error),
        Err(error) => Ok(Some(error.to_string())),
    }
}

/// Note the refusal in Archon's feed and tell the orchestrator, which
/// restates it in the chat the command came from.
async fn refused(ctx: &DaemonContext, why: &str) -> Result<(), ArchonError> {
    assert!(!why.trim().is_empty(), "a refusal gives its reason");
    let note = format!("A `/merge` sent in the orchestrator's Aether chat was not applied: {why}");
    feed::system(ctx, clip(&note, 2 * MAX_NEWS_CHARS)).await?;
    let news = TriggerEvent::ChatMergeRefused {
        text: clip(why, MAX_NEWS_CHARS),
    };
    triggers::push(ctx, news).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn command(item: &str, ignore_checks: bool) -> Option<Result<Command, String>> {
        Some(Ok(Command {
            item: item.to_owned(),
            ignore_checks,
        }))
    }

    #[test]
    fn a_whole_message_command_names_its_item_and_flag() {
        assert_eq!(parse("/merge qi_01ABC"), command("qi_01ABC", false));
        assert_eq!(parse("  /MERGE  01abc \n"), command("01abc", false));
        assert_eq!(
            parse("/merge qi_01ABC --ignore-checks"),
            command("qi_01ABC", true)
        );
        assert_eq!(
            parse("/merge --ignore-checks qi_01ABC"),
            command("qi_01ABC", true)
        );
    }

    #[test]
    fn text_that_is_not_exactly_a_command_is_left_alone() {
        for message in [
            "",
            "merge qi_01ABC",
            "please /merge qi_01ABC",
            "> /merge qi_01ABC",
            "`/merge qi_01ABC`",
            "/merged qi_01ABC",
            "/mergeqi_01ABC",
            "//merge qi_01ABC",
            "/answer 3 yes",
        ] {
            assert_eq!(parse(message), None, "{message:?}");
        }
    }

    #[test]
    fn a_malformed_command_says_what_is_wrong() {
        let refusal = |message: &str| parse(message).expect("a command").expect_err("malformed");
        assert!(refusal("/merge").starts_with("no item was named"));
        assert!(refusal("/merge --ignore-checks").contains("is not one item"));
        assert!(refusal("/merge qi_1 qi_2").contains("is not one item"));
        assert!(refusal("/merge qi_1 --admin").contains("is not one item"));
        assert!(refusal("/merge qi_1 please, it is reviewed").contains(USAGE));
    }
}
