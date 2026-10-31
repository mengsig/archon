//! A pseudo-terminal to run the built binary on, as a person at a terminal would.
#![allow(dead_code, reason = "each test binary uses a different subset")]

use std::fs::File;
use std::io::IsTerminal as _;
use std::io::{Read as _, Write as _};
use std::os::fd::OwnedFd;
use std::os::unix::ffi::OsStrExt as _;
use std::process::{Child, Command, Output, Stdio};
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

use rustix::fs::{Mode, OFlags};
use rustix::pty::{OpenptFlags, grantpt, openpt, ptsname, unlockpt};

const WAIT: Duration = Duration::from_secs(30);

/// A pseudo-terminal whose far end is a child's standard input and output.
pub struct Terminal {
    master: File,
    slave: File,
}

impl Terminal {
    pub fn open() -> Self {
        let master = openpt(OpenptFlags::RDWR | OpenptFlags::NOCTTY).unwrap();
        grantpt(&master).unwrap();
        unlockpt(&master).unwrap();
        let name = ptsname(&master, Vec::new()).unwrap();
        let path = std::ffi::OsStr::from_bytes(name.to_bytes());
        let slave: OwnedFd =
            rustix::fs::open(path, OFlags::RDWR | OFlags::NOCTTY, Mode::empty()).unwrap();
        assert!(slave.is_terminal(), "{path:?} is a terminal");
        Self {
            master: master.into(),
            slave: slave.into(),
        }
    }

    /// Run `command` on this terminal to its end, with its standard error captured.
    pub fn run(&self, command: &mut Command) -> Output {
        self.start(command).finish()
    }

    pub fn start(&self, command: &mut Command) -> Session {
        let child = command
            .stdin(self.slave.try_clone().unwrap())
            .stdout(self.slave.try_clone().unwrap())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut master = self.master.try_clone().unwrap();
        let (sender, screen) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut chunk = [0_u8; 1024];
            // Linux reports a terminal with no far end left as an error.
            while let Ok(read @ 1..) = master.read(&mut chunk) {
                let text = String::from_utf8_lossy(&chunk[..read]).into_owned();
                if sender.send(text).is_err() {
                    break;
                }
            }
        });
        Session {
            child,
            keyboard: self.master.try_clone().unwrap(),
            screen,
            shown: String::new(),
        }
    }
}

/// A child on a [`Terminal`]: what it showed, and a keyboard to type on.
pub struct Session {
    child: Child,
    keyboard: File,
    screen: Receiver<String>,
    shown: String,
}

impl Session {
    /// Wait until the terminal has shown `text`.
    pub fn wait_for(&mut self, text: &str) {
        let deadline = Instant::now() + WAIT;
        while !self.shown.contains(text) {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.screen.recv_timeout(left) {
                Ok(chunk) => self.shown.push_str(&chunk),
                Err(error) => panic!("no {text:?} on the terminal ({error}):\n{}", self.shown),
            }
        }
    }

    /// Type `line` and Enter.
    pub fn type_line(&mut self, line: &str) {
        assert!(!line.contains('\n'), "{line:?} is one line");
        self.keyboard
            .write_all(format!("{line}\n").as_bytes())
            .unwrap();
    }

    /// The child's process id, to signal it as Ctrl+C on its own terminal would.
    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    /// What the terminal has shown so far.
    pub fn shown(&self) -> &str {
        &self.shown
    }

    /// Ctrl-D on the empty prompt: the end of input, which ends the chat.
    pub fn end_input(&mut self) {
        self.keyboard.write_all(&[0x04]).unwrap();
    }

    /// Wait for the child to end; its standard output was the terminal.
    pub fn finish(mut self) -> Output {
        let deadline = Instant::now() + WAIT;
        while self.child.try_wait().unwrap().is_none() {
            // Keep reading, so a child that drains its output is not held up.
            if let Ok(chunk) = self.screen.recv_timeout(Duration::from_millis(20)) {
                self.shown.push_str(&chunk);
            }
            assert!(
                Instant::now() < deadline,
                "the child did not end; the terminal showed:\n{}",
                self.shown
            );
        }
        let mut stderr = Vec::new();
        let mut pipe = self.child.stderr.take().expect("standard error is piped");
        pipe.read_to_end(&mut stderr).unwrap();
        Output {
            status: self.child.wait().unwrap(),
            stdout: Vec::new(),
            stderr,
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _already_gone = self.child.kill();
        let _reaped = self.child.wait();
    }
}
