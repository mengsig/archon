//! Diagnose a shared store lock after a daemon start was refused.

use std::fs::{File, OpenOptions, TryLockError};
use std::io::{self, BufRead, IsTerminal, Write};
use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::Context as _;

use crate::store_holders;

const SHARED_REFUSAL: &str = "open without archond";
const EXCLUSIVE_REFUSAL: &str = "two daemons on one store";
const STOP_WAIT: Duration = Duration::from_secs(5);
const POLL: Duration = Duration::from_millis(100);

#[derive(Debug, PartialEq, Eq)]
enum LockState {
    Free,
    Shared,
    Exclusive,
}

/// Only handle a store-lock refusal, never offer to stop an archond.
pub(crate) async fn recover(error: &str) -> anyhow::Result<bool> {
    if !error.contains(SHARED_REFUSAL) && !error.contains(EXCLUSIVE_REFUSAL) {
        return Ok(false);
    }
    let data = std::path::absolute(archon_core::paths::data_dir())?;
    let path = archon_core::paths::data_lock_path_for(&data);
    let interactive = io::stdin().is_terminal()
        && io::stderr().is_terminal()
        && matches!(
            crate::connect::principal(),
            archon_client::Principal::Operator
        );
    let stdin = io::stdin();
    let stderr = io::stderr();
    recover_with(&path, interactive, &mut stdin.lock(), &mut stderr.lock()).await
}

async fn recover_with(
    path: &Path,
    interactive: bool,
    input: &mut impl BufRead,
    output: &mut impl Write,
) -> anyhow::Result<bool> {
    assert!(
        path.is_absolute(),
        "the data lock path is resolved before probing"
    );
    match probe(path)? {
        LockState::Free => {
            writeln!(
                output,
                "archon: the store lock holder is gone; retrying startup."
            )?;
            return Ok(true);
        }
        LockState::Shared | LockState::Exclusive => {}
    }
    let holders = match store_holders::discover(path) {
        Ok(holders) => holders,
        Err(error) => {
            writeln!(
                output,
                "archon: cannot identify store lock holders: {error}; wait for them to finish, then run `archon daemon start`."
            )?;
            return Ok(false);
        }
    };
    if holders.is_empty() {
        if probe(path)? == LockState::Free {
            writeln!(
                output,
                "archon: the store lock holder is gone; retrying startup."
            )?;
            return Ok(true);
        }
        writeln!(
            output,
            "archon: store lock holders could not be identified; wait for them to finish, then run `archon daemon start`."
        )?;
        return Ok(false);
    }
    describe_holders(&holders, output)?;
    if !can_offer_stop(&holders) {
        writeln!(
            output,
            "archon: wait for the holders to finish, then run `archon daemon start`."
        )?;
        return Ok(false);
    }
    if !interactive || !confirm_stop(&holders, input, output)? {
        return Ok(false);
    }
    stop_and_wait(path, &holders).await?;
    Ok(true)
}

async fn stop_and_wait(path: &Path, holders: &[store_holders::Holder]) -> anyhow::Result<()> {
    assert!(
        can_offer_stop(holders),
        "only a verified holder can be stopped"
    );
    store_holders::terminate(path, &holders[0]).context("not stopping the store holder")?;
    let deadline = Instant::now() + STOP_WAIT;
    loop {
        if probe(path)? == LockState::Free {
            return Ok(());
        }
        if Instant::now() >= deadline {
            anyhow::bail!(
                "store is still locked after {STOP_WAIT:?}; no further signals were sent"
            );
        }
        tokio::time::sleep(POLL).await;
    }
}

fn describe_holders(holders: &[store_holders::Holder], output: &mut impl Write) -> io::Result<()> {
    assert!(!holders.is_empty(), "there are holders to describe");
    for holder in holders {
        let pid = holder
            .pid
            .map_or_else(|| "unknown".to_owned(), |pid| pid.to_string());
        let command = holder.command.as_deref().unwrap_or("command unavailable");
        let age = holder.age.map_or_else(
            || "age unavailable".to_owned(),
            |age| format!("running {}s", age.as_secs()),
        );
        writeln!(
            output,
            "archon: process {pid} ({command}) has the store open ({age})."
        )?;
        if let Some(pid) = holder.pid.filter(|_| holder.may_stop()) {
            writeln!(
                output,
                "archon: after checking the process, run `kill -TERM {pid}` and then `archon daemon start`."
            )?;
        }
    }
    Ok(())
}

fn can_offer_stop(holders: &[store_holders::Holder]) -> bool {
    holders.len() == 1
        && holders
            .iter()
            .all(|holder| holder.pid.is_some() && holder.command.is_some() && holder.may_stop())
}

fn confirm_stop(
    holders: &[store_holders::Holder],
    input: &mut impl BufRead,
    output: &mut impl Write,
) -> io::Result<bool> {
    assert!(
        can_offer_stop(holders),
        "only identified shared holders can be offered"
    );
    let names = holders
        .iter()
        .map(|holder| {
            format!(
                "{} ({})",
                holder.pid.unwrap(),
                holder.command.as_deref().unwrap()
            )
        })
        .collect::<Vec<_>>()
        .join(", ");
    let (subject, pronoun) = if holders.len() == 1 {
        ("Process", "it")
    } else {
        ("Processes", "them")
    };
    write!(
        output,
        "{subject} {names} has Archon's store open. Stop {pronoun} and start Archon? [y/N] "
    )?;
    output.flush()?;
    let mut answer = String::new();
    input.read_line(&mut answer)?;
    Ok(matches!(
        answer.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}

fn probe(path: &Path) -> anyhow::Result<LockState> {
    let file = match OpenOptions::new().read(true).write(true).open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(LockState::Free),
        Err(error) => return Err(error).with_context(|| format!("opening {}", path.display())),
    };
    probe_file(&file).with_context(|| format!("probing {}", path.display()))
}

fn probe_file(file: &File) -> io::Result<LockState> {
    match file.try_lock() {
        Ok(()) => return Ok(LockState::Free),
        Err(TryLockError::Error(error)) => return Err(error),
        Err(TryLockError::WouldBlock) => {}
    }
    match file.try_lock_shared() {
        Ok(()) => Ok(LockState::Shared),
        Err(TryLockError::WouldBlock) => Ok(LockState::Exclusive),
        Err(TryLockError::Error(error)) => Err(error),
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use std::io::{BufReader, Cursor};
    use std::process::{Child, Command, Stdio};
    use std::sync::mpsc;

    struct HolderProcess(Child);

    impl Drop for HolderProcess {
        fn drop(&mut self) {
            if self.0.try_wait().unwrap().is_none() {
                self.0.kill().unwrap();
            }
            self.0.wait().unwrap();
        }
    }

    fn holder(path: &Path) -> HolderProcess {
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "startup_lock::tests::child_holds_shared_lock",
                "--nocapture",
            ])
            .env("ARCHON_TEST_SHARED_LOCK", path)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let stdout = child.stdout.take().unwrap();
        let (sender, receiver) = mpsc::channel();
        std::thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            let mut line = String::new();
            while reader.read_line(&mut line).unwrap() != 0 {
                if line.contains("HOLDER_READY") {
                    sender.send(()).unwrap();
                    return;
                }
                line.clear();
            }
        });
        let child = HolderProcess(child);
        receiver
            .recv_timeout(Duration::from_secs(5))
            .expect("holder did not become ready");
        child
    }

    #[test]
    fn child_holds_shared_lock() {
        let Some(path) = std::env::var_os("ARCHON_TEST_SHARED_LOCK") else {
            return;
        };
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .unwrap();
        file.try_lock_shared().unwrap();
        println!("HOLDER_READY");
        io::stdout().flush().unwrap();
        std::thread::sleep(Duration::from_secs(20));
    }

    fn lock_file() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("data.lock");
        File::create(&path).unwrap();
        (dir, path)
    }

    #[tokio::test]
    async fn stale_holder_retries_without_a_prompt() {
        let (_dir, path) = lock_file();
        let mut output = Vec::new();
        assert!(
            recover_with(&path, true, &mut Cursor::new(""), &mut output)
                .await
                .unwrap()
        );
        let text = String::from_utf8(output).unwrap();
        assert!(text.contains("holder is gone"), "{text}");
        assert!(!text.contains("[y/N]"), "{text}");
    }

    #[tokio::test]
    async fn exclusive_holder_is_diagnosed_but_never_offered_for_stopping() {
        let (_dir, path) = lock_file();
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        file.try_lock().unwrap();
        let mut output = Vec::new();
        assert!(
            !recover_with(&path, true, &mut Cursor::new("yes\n"), &mut output)
                .await
                .unwrap()
        );
        let text = String::from_utf8(output).unwrap();
        assert!(text.contains(&std::process::id().to_string()), "{text}");
        assert!(
            !text.contains("[y/N]") && !text.contains("kill -TERM"),
            "{text}"
        );
    }

    #[tokio::test]
    async fn noninteractive_and_no_leave_the_holder_running() {
        let (_dir, path) = lock_file();
        let mut child = holder(&path);
        for (interactive, answer) in [(false, "yes\n"), (true, "no\n")] {
            let mut output = Vec::new();
            assert!(
                !recover_with(&path, interactive, &mut Cursor::new(answer), &mut output)
                    .await
                    .unwrap()
            );
            let text = String::from_utf8(output).unwrap();
            assert!(text.contains(&child.0.id().to_string()), "{text}");
            assert!(text.contains("kill -TERM"), "{text}");
            assert_eq!(text.contains("[y/N]"), interactive, "{text}");
            assert!(child.0.try_wait().unwrap().is_none());
        }
    }

    #[tokio::test]
    async fn yes_stops_holder_and_allows_retry() {
        let (_dir, path) = lock_file();
        let mut child = holder(&path);
        let mut output = Vec::new();
        assert!(
            recover_with(&path, true, &mut Cursor::new("yes\n"), &mut output)
                .await
                .unwrap()
        );
        assert!(child.0.try_wait().unwrap().is_some());
        assert_eq!(probe(&path).unwrap(), LockState::Free);
        assert!(String::from_utf8(output).unwrap().contains("[y/N]"));
    }
}
