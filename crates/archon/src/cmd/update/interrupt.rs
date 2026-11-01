//! The signals that end an update early. An update listens for them so that
//! it stops where it waits and removes what it staged, instead of dying.

use anyhow::Context as _;
use tokio::signal::unix::{Signal, SignalKind, signal};

/// A shell reports a command that a signal ended as 128 plus the signal.
const SIGNAL_EXIT_BASE: i32 = 128;

/// A signal that asked the update to stop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Interrupt {
    Hangup,
    Keyboard,
    Terminate,
}

impl Interrupt {
    const ALL: [Self; 3] = [Self::Hangup, Self::Keyboard, Self::Terminate];

    /// The interrupt an update that exited with `code` was ended by.
    pub fn ended(code: std::process::ExitCode) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|interrupt| std::process::ExitCode::from(interrupt.exit_code()) == code)
    }

    fn kind(self) -> SignalKind {
        match self {
            Self::Hangup => SignalKind::hangup(),
            Self::Keyboard => SignalKind::interrupt(),
            Self::Terminate => SignalKind::terminate(),
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Hangup => "SIGHUP",
            Self::Keyboard => "SIGINT",
            Self::Terminate => "SIGTERM",
        }
    }

    /// The exit status of a command this signal ended: 130 for SIGINT.
    pub fn exit_code(self) -> u8 {
        let code = SIGNAL_EXIT_BASE + self.kind().as_raw_value();
        u8::try_from(code).expect("a signal number is below 128")
    }
}

/// Handlers for every [`Interrupt`]. Once installed they stay for the life
/// of the process: a signal ends only an update waiting on [`Interrupts::next`].
#[derive(Debug)]
pub struct Interrupts {
    hangup: Signal,
    interrupt: Signal,
    terminate: Signal,
}

impl Interrupts {
    pub fn listen() -> anyhow::Result<Self> {
        let listen = |interrupt: Interrupt| {
            signal(interrupt.kind())
                .with_context(|| format!("installing the {} handler", interrupt.name()))
        };
        Ok(Self {
            hangup: listen(Interrupt::Hangup)?,
            interrupt: listen(Interrupt::Keyboard)?,
            terminate: listen(Interrupt::Terminate)?,
        })
    }

    /// The first signal to arrive; waits for as long as none does.
    pub async fn next(&mut self) -> Interrupt {
        tokio::select! {
            Some(()) = self.hangup.recv() => Interrupt::Hangup,
            Some(()) = self.interrupt.recv() => Interrupt::Keyboard,
            Some(()) = self.terminate.recv() => Interrupt::Terminate,
            // A closed stream delivers nothing more, which is no interrupt.
            else => std::future::pending().await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_interrupt_exits_as_a_shell_reports_its_signal() {
        let codes = Interrupt::ALL.map(|interrupt| (interrupt.name(), interrupt.exit_code()));
        assert_eq!(codes, [("SIGHUP", 129), ("SIGINT", 130), ("SIGTERM", 143)]);
        let ended = |code: u8| Interrupt::ended(std::process::ExitCode::from(code));
        assert_eq!(ended(130), Some(Interrupt::Keyboard));
        assert_eq!((ended(0), ended(1)), (None, None));
    }
}
