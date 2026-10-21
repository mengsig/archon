//! A bare `archon`: the terminal UI, or the line chat where the terminal UI
//! cannot run. Any other failure to open the terminal UI stays an error.

use std::io::IsTerminal as _;
use std::process::ExitCode;

use anyhow::Context as _;
use archon_core::config::Config;

use super::{first_run, instance};
use crate::chat;
use crate::cli::TuiArgs;

const NOT_A_TERMINAL: &str = "standard input or output is not a terminal";
const TCP_ADDRESS: &str = "[aether] address (TCP) is set, which the terminal UI cannot use";
const TUI_FAILED: &str = "opening the terminal UI (`archon chat` is the line chat; \
                          `archon doctor` checks the setup)";

/// What a bare `archon` opens.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Front {
    Tui,
    /// The line chat, and why the terminal UI is not used.
    Chat(&'static str),
}

/// `terminal`: standard input and output are both terminals. `tcp_address`:
/// `[aether] address` is configured.
pub fn front(terminal: bool, tcp_address: bool) -> Front {
    if !terminal {
        return Front::Chat(NOT_A_TERMINAL);
    }
    if tcp_address {
        return Front::Chat(TCP_ADDRESS);
    }
    Front::Tui
}

pub async fn run() -> anyhow::Result<ExitCode> {
    let config = Config::load_default()?;
    let terminal = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
    match front(terminal, config.aether.address.is_some()) {
        Front::Tui => {
            first_run::offer(&config).await?;
            instance::tui_checked(TuiArgs::default())
                .await
                .context(TUI_FAILED)
        }
        Front::Chat(reason) => {
            if !terminal && let Some(hint) = first_run::non_interactive_hint(&config)? {
                eprintln!("archon: {hint}");
            }
            eprintln!("archon: {reason}; opening the line chat (`archon chat`) instead");
            chat::run(false).await
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_terminal_ui_opens_only_on_a_terminal_without_a_tcp_address() {
        assert_eq!(front(true, false), Front::Tui);
        assert_eq!(front(true, true), Front::Chat(TCP_ADDRESS));
        assert_eq!(front(false, false), Front::Chat(NOT_A_TERMINAL));
        assert_eq!(front(false, true), Front::Chat(NOT_A_TERMINAL));
    }

    #[test]
    fn a_failed_terminal_ui_names_the_line_chat_and_the_doctor() {
        for said in ["terminal UI", "`archon chat`", "`archon doctor`"] {
            assert!(TUI_FAILED.contains(said), "missing {said:?}");
        }
        for reason in [NOT_A_TERMINAL, TCP_ADDRESS] {
            assert!(!reason.contains('\n'), "the reason is one line: {reason:?}");
        }
    }
}
