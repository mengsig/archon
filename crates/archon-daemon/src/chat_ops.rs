//! The operator's `/upgrade` and `/watch` chat commands, taken as `/answer`
//! is (see `chat_answer`): only a whole message Aether authenticated as the
//! `user` actor, and only in dedicated mode, as a shared instance's `user`
//! includes `[listen]` token clients. The outcome is `chat_command` news
//! for the orchestrator to restate.

use archon_core::config::AetherMode;
use archon_core::error::ArchonError;
use archon_core::ops::{DEFAULT_DRAIN_SECS, UpgradeRequest, UpgradeTarget};
use archon_core::protocol::RepoRef;

use crate::context::DaemonContext;
use crate::executor::clip;
use crate::triggers::{self, TriggerEvent};
use crate::{feed, handlers, ops};

const UPGRADE: &str = "/upgrade";
const WATCH: &str = "/watch";
const UPGRADE_USAGE: &str = "usage: `/upgrade archon|aether|both <version|latest|main> [requeue]` \
     or `/upgrade restart [requeue] [keep-aether]`, alone in the message";
const WATCH_USAGE: &str = "usage: `/watch <registered repository>`, alone in the message";
const SHARED_REFUSAL: &str = "chat commands are off while Archon shares the operator's Aether \
     instance (`[aether] mode = \"shared\"`), where a `[listen]` token client is the same `user` \
     actor as the operator; use `archon daemon upgrade` or `archon github-watcher add`";
/// Longest outcome one `chat_command` event carries.
const MAX_NEWS_CHARS: usize = 500;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Command {
    Upgrade(UpgradeRequest),
    Watch(RepoRef),
}

/// `None` unless the whole message is one of these commands; else the
/// command's name and the command, or what a malformed one lacks.
pub(crate) fn parse(message: &str) -> Option<(&'static str, Result<Command, String>)> {
    let message = message.trim();
    if let Some(args) = after_name(message, UPGRADE) {
        return Some((UPGRADE, parse_upgrade(args).map(Command::Upgrade)));
    }
    let args = after_name(message, WATCH)?;
    let watch = match args.split_whitespace().collect::<Vec<_>>().as_slice() {
        [repo] => Ok(Command::Watch(RepoRef((*repo).to_owned()))),
        [] => Err(format!("no repository was named; {WATCH_USAGE}")),
        _ => Err(format!("name one repository; {WATCH_USAGE}")),
    };
    Some((WATCH, watch))
}

/// What follows `name` when `message` starts with that command word.
fn after_name<'a>(message: &'a str, name: &str) -> Option<&'a str> {
    let head = message.get(..name.len())?;
    let rest = &message[name.len()..];
    let whole_word = rest.is_empty() || rest.starts_with(char::is_whitespace);
    (head.eq_ignore_ascii_case(name) && whole_word).then_some(rest)
}

fn parse_upgrade(args: &str) -> Result<UpgradeRequest, String> {
    let usage = |why: String| format!("{why}; {UPGRADE_USAGE}");
    let words: Vec<&str> = args.split_whitespace().collect();
    let mut request = UpgradeRequest {
        archon: None,
        aether: None,
        drain_secs: DEFAULT_DRAIN_SECS,
        requeue: false,
        keep_aether: false,
    };
    let options = match words.as_slice() {
        [first, options @ ..] if first.eq_ignore_ascii_case("restart") => options,
        [which, target, options @ ..] => {
            let target = UpgradeTarget::parse(target).map_err(|e| usage(e.to_string()))?;
            match which.to_ascii_lowercase().as_str() {
                "archon" => request.archon = Some(target),
                "aether" => request.aether = Some(target),
                "both" => (request.archon, request.aether) = (Some(target.clone()), Some(target)),
                _ => {
                    return Err(usage(format!(
                        "{which:?} is not archon, aether, both or restart"
                    )));
                }
            }
            options
        }
        _ => return Err(usage("nothing to upgrade was named".into())),
    };
    for option in options {
        match option.to_ascii_lowercase().as_str() {
            "requeue" => request.requeue = true,
            "keep-aether" => request.keep_aether = true,
            _ => return Err(usage(format!("{option:?} is not requeue or keep-aether"))),
        }
    }
    request.check().map_err(|e| usage(e.to_string()))?;
    Ok(request)
}

/// Apply `message` if it is one of these commands and tell the operator what
/// happened. Only a store failure is returned: any other refusal is
/// reported, never retried, as the pump shows a message here once.
pub(crate) async fn handle(ctx: &DaemonContext, message: &str) -> Result<(), ArchonError> {
    let Some((name, parsed)) = parse(message) else {
        return Ok(());
    };
    let outcome = match parsed {
        Err(usage) => Err(usage),
        Ok(_) if ctx.config.aether.mode != AetherMode::Dedicated => Err(SHARED_REFUSAL.to_owned()),
        Ok(command) => apply(ctx, command).await?,
    };
    report(ctx, name, outcome).await
}

async fn apply(
    ctx: &DaemonContext,
    command: Command,
) -> Result<Result<String, String>, ArchonError> {
    let done = match command {
        Command::Upgrade(request) => ops::start(ctx, request).await,
        Command::Watch(repo) => handlers::watch_repo(ctx, &repo).await,
    };
    match done {
        Ok(text) => Ok(Ok(text)),
        Err(error) if error.is_store() => Err(error),
        Err(error) => Ok(Err(error.to_string())),
    }
}

/// Tell the orchestrator, which restates it in the chat the command came
/// from; a refusal is also noted in Archon's own feed.
async fn report(
    ctx: &DaemonContext,
    name: &str,
    outcome: Result<String, String>,
) -> Result<(), ArchonError> {
    let applied = outcome.is_ok();
    let text = outcome.unwrap_or_else(|why| why);
    assert!(!text.trim().is_empty(), "an outcome says something");
    if !applied {
        let note = format!("A `{name}` sent in the orchestrator's Aether chat was refused: {text}");
        feed::system(ctx, clip(&note, 2 * MAX_NEWS_CHARS)).await?;
    }
    crate::command_notice::show(ctx, name, applied, &text).await;
    let news = TriggerEvent::ChatCommand {
        command: name.to_owned(),
        applied,
        text: clip(&text, MAX_NEWS_CHARS),
    };
    triggers::push(ctx, news).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn upgrade(message: &str) -> UpgradeRequest {
        match parse(message) {
            Some((UPGRADE, Ok(Command::Upgrade(request)))) => request,
            other => panic!("{message:?} parsed as {other:?}"),
        }
    }

    fn refusal(message: &str) -> String {
        match parse(message) {
            Some((_, Err(why))) => why,
            other => panic!("{message:?} parsed as {other:?}"),
        }
    }

    #[test]
    fn an_upgrade_names_programs_targets_and_options() {
        let main = upgrade("/upgrade archon main");
        assert_eq!(main.archon, Some(UpgradeTarget::Main));
        assert_eq!((main.aether, main.requeue), (None, false));
        let both = upgrade("  /UPGRADE both latest requeue ");
        assert_eq!(both.archon, Some(UpgradeTarget::Latest));
        assert_eq!(both.aether, Some(UpgradeTarget::Latest));
        assert!(both.requeue);
        let restart = upgrade("/upgrade restart keep-aether");
        assert_eq!((restart.archon, restart.aether), (None, None));
        assert!(restart.keep_aether && !restart.requeue);
        assert_eq!(restart.drain_secs, DEFAULT_DRAIN_SECS);
    }

    #[test]
    fn a_malformed_upgrade_says_what_is_wrong() {
        assert!(refusal("/upgrade").starts_with("nothing to upgrade"));
        assert!(refusal("/upgrade archon").starts_with("nothing to upgrade"));
        assert!(refusal("/upgrade archon --locked").contains("is not `latest`"));
        assert!(refusal("/upgrade lavish main").contains("is not archon, aether"));
        assert!(refusal("/upgrade restart now").contains("is not requeue"));
        assert!(refusal("/upgrade aether 1.0.7 keep-aether").contains("drop keep_aether"));
        assert!(refusal("/upgrade archon main later").ends_with(UPGRADE_USAGE));
    }

    #[test]
    fn a_watch_names_one_registered_repository() {
        match parse("/watch archon") {
            Some((WATCH, Ok(Command::Watch(repo)))) => assert_eq!(repo.0, "archon"),
            other => panic!("parsed as {other:?}"),
        }
        assert!(refusal("/watch").starts_with("no repository"));
        assert!(refusal("/watch a b").starts_with("name one repository"));
    }

    #[test]
    fn text_that_is_not_exactly_a_command_is_left_alone() {
        for message in [
            "",
            "upgrade archon main",
            "please /upgrade archon main",
            "> /upgrade restart",
            "`/watch archon`",
            "/upgraded archon main",
            "/watching archon",
            "/answer 3 yes",
        ] {
            assert_eq!(parse(message), None, "{message:?}");
        }
    }
}
