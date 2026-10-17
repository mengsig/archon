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
    chat_only(
        "pause",
        PAUSE_ARGS,
        "pause new work, or one repository or pipeline",
    ),
    chat_only("resume", PAUSE_ARGS, "resume what /pause stopped"),
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

/// The `aether tui --host-commands` file:
/// `{"version":1,"host":"archon","commands":[…]}`.
pub fn host_commands_json() -> serde_json::Value {
    let commands: Vec<&OperatorCommand> = OPERATOR_COMMANDS.iter().filter(|c| c.in_tui).collect();
    serde_json::json!({
        "version": HOST_COMMANDS_VERSION,
        "host": HOST,
        "commands": commands,
    })
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
        let file = host_commands_json();
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
                "item"
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
}
