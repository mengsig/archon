//! Parsing chat input lines: plain text goes to the orchestrator, `/…`
//! lines are slash commands (ARCHITECTURE §12). Pure.

use std::fmt::Write as _;

use archon_core::operator_commands::OPERATOR_COMMANDS;
use archon_core::protocol::{PauseTarget, RepoRef};

use crate::questions::QuestionRef;

/// The chat's own commands, after the operator commands every surface takes.
const CHAT_HELP: &str = "\
  /archon answer|approve|reject|merge   the same as /answer, /approve, /reject, /merge
  /archon undo N          undo operator change N from the Authority feed
  /archon reset [--repo NAME]   forget the saved settings of a scope
  /image <path>           attach an image file to the next message
  /progress               show or hide progress notes (also /verbose)
  /help                   this list
  /quit                   leave (also Ctrl-D, or Ctrl-C twice)
Start a line with // to send literal slash text; the escape stays in the message.
Ctrl+V (or Alt+V) attaches a copied image as [image #N], else pastes the text;
Ctrl+Shift+V always pastes text. Backspace after a chip removes its image.";

pub fn help() -> String {
    let mut text = String::from("Type a message and press Enter to talk to the orchestrator.\n");
    for command in OPERATOR_COMMANDS {
        let usage = format!("/{} {}", command.name, command.args);
        let _ = writeln!(text, "  {:<23} {}", usage.trim_end(), command.about);
    }
    text.push_str(CHAT_HELP);
    text
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChatInput {
    Empty,
    Say(String),
    Command(SlashCommand),
    /// A malformed command, with what to tell the operator.
    Invalid(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SlashCommand {
    Help,
    Authority(String),
    Quit,
    ToggleProgress,
    Answer {
        question: QuestionRef,
        text: String,
    },
    Questions,
    Status,
    Queue {
        states: Vec<String>,
    },
    Item(String),
    /// Attach the image file at this path to the next message.
    Image(String),
    Merge {
        item: String,
        ignore_checks: bool,
    },
    Pause(PauseTarget),
    Resume(PauseTarget),
}

pub fn parse_line(line: &str) -> ChatInput {
    let line = line.trim();
    if line.is_empty() {
        return ChatInput::Empty;
    }
    if line.starts_with("//") {
        // Preserve the escape across Aether delivery, whose user messages
        // also pass through the daemon's operator-command parser.
        return ChatInput::Say(line.to_owned());
    }
    let Some(command) = line.strip_prefix('/') else {
        return ChatInput::Say(line.to_owned());
    };
    let (name, rest) = split_word(command);
    match parse_command(&name.to_ascii_lowercase(), rest) {
        Ok(command) => ChatInput::Command(command),
        Err(message) => ChatInput::Invalid(message),
    }
}

fn parse_command(name: &str, rest: &str) -> Result<SlashCommand, String> {
    let no_args = |command: SlashCommand| match rest.is_empty() {
        true => Ok(command),
        false => Err(format!("/{name} takes no arguments")),
    };
    match name {
        "help" | "h" | "?" => no_args(SlashCommand::Help),
        "quit" | "exit" | "q" => no_args(SlashCommand::Quit),
        "progress" | "verbose" => no_args(SlashCommand::ToggleProgress),
        "questions" => no_args(SlashCommand::Questions),
        "status" => no_args(SlashCommand::Status),
        "queue" => Ok(SlashCommand::Queue {
            states: rest.split_whitespace().map(str::to_owned).collect(),
        }),
        "item" => match split_word(rest) {
            (id, "") if !id.is_empty() => Ok(SlashCommand::Item(id.to_owned())),
            _ => Err("usage: /item <id>".to_owned()),
        },
        "image" | "img" => match rest.is_empty() {
            true => Err("usage: /image <path>".to_owned()),
            false => Ok(SlashCommand::Image(rest.to_owned())),
        },
        "archon" => parse_archon(rest),
        "approve" | "reject" | "upgrade" | "update" | "watch" => {
            Ok(SlashCommand::Authority(format!("/{name} {rest}")))
        }
        "merge" => parse_merge(rest),
        "a" => Err("use /answer <n> <text>".to_owned()),
        "answer" => parse_answer(rest),
        "pause" => parse_pause_target(rest, "pause").map(SlashCommand::Pause),
        "resume" => parse_pause_target(rest, "resume").map(SlashCommand::Resume),
        other => Err(format!("unknown command /{other}; /help lists them")),
    }
}

/// `/archon answer|approve|reject|merge …` is the same command as `/answer …`.
fn parse_archon(rest: &str) -> Result<SlashCommand, String> {
    let (verb, args) = split_word(rest);
    let verb = verb.to_ascii_lowercase();
    match verb.as_str() {
        "answer" | "approve" | "reject" | "merge" => parse_command(&verb, args),
        _ => Ok(SlashCommand::Authority(format!("/archon {rest}"))),
    }
}

fn parse_answer(rest: &str) -> Result<SlashCommand, String> {
    const USAGE: &str = "usage: /answer <n|id> <text>";
    let (question, text) = split_word(rest);
    if question.is_empty() || text.is_empty() {
        return Err(USAGE.to_owned());
    }
    let question = QuestionRef::parse(question).map_err(|e| format!("{e}; {USAGE}"))?;
    Ok(SlashCommand::Answer {
        question,
        text: text.to_owned(),
    })
}

fn parse_merge(rest: &str) -> Result<SlashCommand, String> {
    const FLAG: &str = "--ignore-checks";
    let words: Vec<&str> = rest.split_whitespace().collect();
    let (item, ignore_checks) = match words.as_slice() {
        [item] => (*item, false),
        [item, FLAG] | [FLAG, item] => (*item, true),
        _ => ("", false),
    };
    if item.is_empty() || item.starts_with('-') {
        return Err(format!("usage: /merge <id> [{FLAG}]"));
    }
    Ok(SlashCommand::Merge {
        item: item.to_owned(),
        ignore_checks,
    })
}

fn parse_pause_target(rest: &str, verb: &str) -> Result<PauseTarget, String> {
    let words: Vec<&str> = rest.split_whitespace().collect();
    match words.as_slice() {
        [] => Ok(PauseTarget::Global { hard: false }),
        ["hard"] => Ok(PauseTarget::Global { hard: true }),
        ["repo", name] => Ok(PauseTarget::Repo {
            name: RepoRef((*name).to_owned()),
        }),
        ["pipeline", name] => Ok(PauseTarget::Pipeline {
            name: (*name).to_owned(),
        }),
        _ => Err(format!(
            "usage: /{verb} [hard | repo <name> | pipeline <name>]"
        )),
    }
}

/// The first whitespace-separated word and the trimmed remainder.
fn split_word(text: &str) -> (&str, &str) {
    let text = text.trim();
    match text.split_once(char::is_whitespace) {
        Some((word, rest)) => (word, rest.trim()),
        None => (text, ""),
    }
}

#[cfg(test)]
mod tests {
    use archon_core::id::QuestionId;

    use super::*;

    #[test]
    fn plain_lines_are_messages_and_blank_lines_nothing() {
        assert_eq!(
            parse_line("  hello there "),
            ChatInput::Say("hello there".into())
        );
        assert_eq!(parse_line("   "), ChatInput::Empty);
        assert_eq!(
            parse_line("//etc/hosts is odd"),
            ChatInput::Say("//etc/hosts is odd".into())
        );
        assert_eq!(
            parse_line("//archon resume --hard"),
            ChatInput::Say("//archon resume --hard".into())
        );
    }

    #[test]
    fn image_names_a_path_to_attach() {
        assert_eq!(
            parse_line("/image ~/shots/a b.png "),
            ChatInput::Command(SlashCommand::Image("~/shots/a b.png".into()))
        );
        assert_eq!(
            parse_line("/image"),
            ChatInput::Invalid("usage: /image <path>".into())
        );
        assert!(help().contains("Ctrl+Shift+V always pastes text"));
    }

    #[test]
    fn answers_take_a_number_or_an_id() {
        assert_eq!(
            parse_line("/answer 2 go with option b"),
            ChatInput::Command(SlashCommand::Answer {
                question: QuestionRef::Number(2),
                text: "go with option b".into()
            })
        );
        let id = QuestionId::new();
        let parsed = parse_line(&format!("/answer {id} yes"));
        assert_eq!(
            parsed,
            ChatInput::Command(SlashCommand::Answer {
                question: QuestionRef::Id(id),
                text: "yes".into()
            })
        );
        assert!(matches!(parse_line("/answer 2"), ChatInput::Invalid(_)));
        assert!(matches!(
            parse_line("/answer zero yes"),
            ChatInput::Invalid(_)
        ));
        for retired in ["/a", "/a 3 yes", "/A sounds good"] {
            assert_eq!(
                parse_line(retired),
                ChatInput::Invalid("use /answer <n> <text>".into())
            );
        }
    }

    #[test]
    fn archon_verbs_are_the_whole_message_commands() {
        assert_eq!(
            parse_line("/archon answer 73 yes"),
            parse_line("/answer 73 yes")
        );
        assert_eq!(
            parse_line("/Archon merge qi_01 --ignore-checks"),
            parse_line("/merge qi_01 --ignore-checks")
        );
        for verb in ["approve", "reject"] {
            assert_eq!(
                parse_line(&format!("/archon {verb} 4 ok")),
                parse_line(&format!("/{verb} 4 ok"))
            );
        }
        assert!(matches!(
            parse_line("/archon answer 73"),
            ChatInput::Invalid(_)
        ));
        assert_eq!(
            parse_line("/archon pause --hard"),
            ChatInput::Command(SlashCommand::Authority("/archon pause --hard".into()))
        );
    }

    #[test]
    fn every_operator_command_is_known_and_in_the_help() {
        let help = help();
        for command in OPERATOR_COMMANDS {
            let line = format!("/{}", command.name);
            if let ChatInput::Invalid(message) = parse_line(&line) {
                assert!(!message.starts_with("unknown command"), "{line}: {message}");
            }
            assert!(
                help.contains(&format!("  {line} ")),
                "{line} is in the help"
            );
        }
        assert!(!help.contains("/a "), "{help}");
    }

    #[test]
    fn upgrade_and_watch_go_to_the_daemon_unchanged() {
        assert_eq!(
            parse_line("/upgrade restart"),
            ChatInput::Command(SlashCommand::Authority("/upgrade restart".into()))
        );
        assert_eq!(
            parse_line("/watch scratch"),
            ChatInput::Command(SlashCommand::Authority("/watch scratch".into()))
        );
        let help = help();
        assert!(help.contains("  /upgrade "), "{help}");
        assert!(help.contains("  /watch <repository>"), "{help}");
    }

    #[test]
    fn pause_and_resume_scopes() {
        assert_eq!(
            parse_line("/pause"),
            ChatInput::Command(SlashCommand::Pause(PauseTarget::Global { hard: false }))
        );
        assert_eq!(
            parse_line("/resume hard"),
            ChatInput::Command(SlashCommand::Resume(PauseTarget::Global { hard: true }))
        );
        assert_eq!(
            parse_line("/pause repo api"),
            ChatInput::Command(SlashCommand::Pause(PauseTarget::Repo {
                name: RepoRef("api".into())
            }))
        );
        assert!(matches!(parse_line("/pause repo"), ChatInput::Invalid(_)));
    }

    #[test]
    fn simple_commands_and_unknown_ones() {
        assert_eq!(parse_line("/QUIT"), ChatInput::Command(SlashCommand::Quit));
        assert_eq!(
            parse_line("/verbose"),
            ChatInput::Command(SlashCommand::ToggleProgress)
        );
        assert_eq!(
            parse_line("/queue running failed"),
            ChatInput::Command(SlashCommand::Queue {
                states: vec!["running".into(), "failed".into()]
            })
        );
        assert!(matches!(parse_line("/status now"), ChatInput::Invalid(_)));
        assert!(matches!(parse_line("/item"), ChatInput::Invalid(_)));
        assert_eq!(
            parse_line("/merge qi_01 --ignore-checks"),
            ChatInput::Command(SlashCommand::Merge {
                item: "qi_01".into(),
                ignore_checks: true
            })
        );
        for malformed in ["/merge", "/merge --ignore-checks", "/merge qi_01 --admin"] {
            assert!(matches!(parse_line(malformed), ChatInput::Invalid(_)));
        }
        let ChatInput::Invalid(message) = parse_line("/frobnicate") else {
            panic!("unknown commands are refused");
        };
        assert!(message.contains("/help"));
    }
}
