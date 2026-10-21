//! The operator's slash commands, one list for `archon chat`'s help, the
//! `aether tui --host-commands` file and the parsers' tests.

use serde::Serialize;

/// The `--host-commands` file format version Aether reads.
pub const HOST_COMMANDS_VERSION: u32 = 1;
/// The host Aether's TUI names beside each command (`· archon`).
pub const HOST: &str = "archon";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct OperatorCommand {
    /// Without the leading slash.
    pub name: &'static str,
    pub args: &'static str,
    pub about: &'static str,
    /// False where the name is also a built-in Aether TUI command, which
    /// Aether refuses as a host command: it stays an `archon chat` command.
    #[serde(skip)]
    pub in_tui: bool,
}

const PAUSE_ARGS: &str = "[hard | repo <name> | pipeline <name>]";

pub const OPERATOR_COMMANDS: &[OperatorCommand] = &[
    command(
        "answer",
        "<n|id> <text>",
        "answer question n (a number picks one of its options)",
    ),
    command("questions", "", "list open questions"),
    command("approve", "[n] [note]", "approve a proposal or report gate"),
    command("reject", "[n] [reason]", "reject a proposal or report gate"),
    command(
        "merge",
        "<id> [--ignore-checks]",
        "merge the item's pull request now",
    ),
    command(
        "archon",
        "<command>",
        "change operator settings (see guide authority)",
    ),
    command("status", "", "daemon, Aether, queue and spend"),
    command(
        "queue",
        "[state ...]",
        "the queue, e.g. /queue running failed",
    ),
    command("item", "<id>", "one queue item (its id or the id's start)"),
    command("repos", "", "the registered repositories"),
    command(
        "lessons",
        "",
        "proposed lessons; accept or retract one with `archon lessons accept|retract <id>`",
    ),
    chat_only(
        "pause",
        PAUSE_ARGS,
        "pause new work, or one repository or pipeline",
    ),
    chat_only("resume", PAUSE_ARGS, "resume what /pause stopped"),
    command(
        "update",
        "",
        "update Archon and Aether to the latest release; running agents are waited for",
    ),
    chat_only(
        "upgrade",
        "archon|aether|both <version|latest|main> [requeue] | restart [requeue] [keep-aether]",
        "upgrade or restart archond (refused in shared mode)",
    ),
    chat_only(
        "watch",
        "<repository>",
        "watch a registered repository's GitHub issues (refused in shared mode)",
    ),
];

const fn command(name: &'static str, args: &'static str, about: &'static str) -> OperatorCommand {
    OperatorCommand {
        name,
        args,
        about,
        in_tui: true,
    }
}

const fn chat_only(name: &'static str, args: &'static str, about: &'static str) -> OperatorCommand {
    OperatorCommand {
        in_tui: false,
        ..command(name, args, about)
    }
}

/// The welcome block's limits, which Aether clamps to.
pub const WELCOME_TITLE: &str = "Archon";
const WELCOME_NAMES_SHOWN: usize = 3;
const WELCOME_NAME_CHARS: usize = 30;
const WELCOME_HINT: &str = "/ for Archon commands, ? for help";

/// What the TUI's welcome block says about the daemon, at launch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WelcomeFacts {
    pub repos: Vec<String>,
    pub running: u32,
    pub queued: u32,
    pub waiting_for_you: u32,
}

/// The `welcome` lines: from the daemon's facts, or a plain fallback when it
/// did not answer in time.
pub fn welcome_lines(facts: Option<&WelcomeFacts>) -> Vec<String> {
    let Some(facts) = facts else {
        return vec![
            "Archon's status is not available yet; /status shows it once the daemon answers."
                .to_owned(),
            WELCOME_HINT.to_owned(),
        ];
    };
    let example_repo = facts.repos.first().map_or("my-repo", String::as_str);
    let lines = vec![
        repos_line(&facts.repos),
        format!(
            "Queue: {} running, {} queued, {} waiting for you",
            facts.running, facts.queued, facts.waiting_for_you
        ),
        format!("Try: \"fix the failing test in {}\"", short(example_repo)),
        WELCOME_HINT.to_owned(),
    ];
    assert!(lines.iter().all(|l| l.chars().count() <= 200));
    lines
}

fn repos_line(repos: &[String]) -> String {
    if repos.is_empty() {
        return "No repositories yet: add one with `archon repo add <path>`".to_owned();
    }
    let shown: Vec<String> = repos
        .iter()
        .take(WELCOME_NAMES_SHOWN)
        .map(|name| short(name))
        .collect();
    let more = repos.len().saturating_sub(WELCOME_NAMES_SHOWN);
    let noun = if repos.len() == 1 {
        "repository"
    } else {
        "repositories"
    };
    let tail = if more > 0 {
        format!(" and {more} more")
    } else {
        String::new()
    };
    format!("{} {noun}: {}{tail}", repos.len(), shown.join(", "))
}

fn short(name: &str) -> String {
    let clean: String = name.chars().filter(|c| !c.is_control()).collect();
    match clean.chars().count() > WELCOME_NAME_CHARS {
        true => format!(
            "{}…",
            clean
                .chars()
                .take(WELCOME_NAME_CHARS - 1)
                .collect::<String>()
        ),
        false => clean,
    }
}

/// The `aether tui --host-commands` file:
/// `{"version":1,"host":"archon","commands":[…]}`, with `welcome` when given.
pub fn host_commands_json(welcome: Option<&[String]>) -> serde_json::Value {
    let commands: Vec<&OperatorCommand> = OPERATOR_COMMANDS.iter().filter(|c| c.in_tui).collect();
    let mut file = serde_json::json!({
        "version": HOST_COMMANDS_VERSION,
        "host": HOST,
        "commands": commands,
    });
    if let Some(lines) = welcome {
        assert!(lines.len() <= 8, "Aether shows at most 8 welcome lines");
        file["welcome"] = serde_json::json!({"title": WELCOME_TITLE, "lines": lines});
    }
    file
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_unique_lowercase_words_without_a_slash() {
        let mut names: Vec<&str> = OPERATOR_COMMANDS.iter().map(|c| c.name).collect();
        for name in &names {
            let mut chars = name.chars();
            assert!(
                chars.next().is_some_and(|c| c.is_ascii_lowercase()),
                "{name}"
            );
            assert!(
                chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'),
                "{name}"
            );
        }
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), OPERATOR_COMMANDS.len(), "no name repeats");
        assert!(!names.contains(&"a"), "`/a` is no longer a command");
    }

    #[test]
    fn the_host_commands_file_lists_every_command_with_its_hint() {
        let file = host_commands_json(None);
        assert!(file.get("welcome").is_none(), "older Aether sees no field");
        assert_eq!(file["version"], 1);
        assert_eq!(file["host"], "archon");
        let commands = file["commands"].as_array().unwrap();
        let names: Vec<&str> = commands
            .iter()
            .map(|c| c["name"].as_str().unwrap())
            .collect();
        assert_eq!(
            names,
            [
                "answer",
                "questions",
                "approve",
                "reject",
                "merge",
                "archon",
                "status",
                "queue",
                "item",
                "repos",
                "lessons",
                "update"
            ],
            "pause and resume are Aether built-ins"
        );
        assert_eq!(commands[0]["name"], "answer");
        assert!(commands[0].get("in_tui").is_none());
        assert_eq!(commands[0]["args"], "<n|id> <text>");
        assert!(
            commands
                .iter()
                .all(|c| c["about"].as_str().is_some_and(|a| !a.is_empty()))
        );
    }

    fn facts(repos: &[&str]) -> WelcomeFacts {
        WelcomeFacts {
            repos: repos.iter().map(|r| (*r).to_owned()).collect(),
            running: 1,
            queued: 2,
            waiting_for_you: 3,
        }
    }

    #[test]
    fn the_welcome_names_repos_queue_an_example_and_the_menu() {
        let lines = welcome_lines(Some(&facts(&["a", "b", "c", "d"])));
        assert_eq!(lines[0], "4 repositories: a, b, c and 1 more");
        assert_eq!(lines[1], "Queue: 1 running, 2 queued, 3 waiting for you");
        assert!(lines[2].starts_with("Try: \"") && lines[2].contains(" a\""));
        assert_eq!(lines[3], "/ for Archon commands, ? for help");
        let file = host_commands_json(Some(&lines));
        assert_eq!(file["welcome"]["title"], "Archon");
        assert_eq!(file["welcome"]["lines"].as_array().unwrap().len(), 4);
        assert_eq!(file["version"], 1);
    }

    #[test]
    fn a_welcome_without_repos_says_how_to_add_one() {
        let lines = welcome_lines(Some(&facts(&[])));
        assert!(lines[0].contains("archon repo add"), "{lines:?}");
        assert!(lines[2].contains("my-repo"));
        assert!(welcome_lines(Some(&facts(&["one"])))[0].starts_with("1 repository:"));
    }

    #[test]
    fn long_repo_names_stay_within_the_line_limit() {
        let long = "x".repeat(500);
        let lines = welcome_lines(Some(&facts(&[&long, &long, &long, &long])));
        assert!(lines.iter().all(|l| l.chars().count() <= 200), "{lines:?}");
    }

    #[test]
    fn a_silent_daemon_gets_a_plain_fallback() {
        let lines = welcome_lines(None);
        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains("/status"));
        assert_eq!(lines[1], "/ for Archon commands, ? for help");
    }
}
