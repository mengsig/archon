//! Startup against a real shared lock, including a terminal confirmation.
#![cfg(target_os = "linux")]

mod common;

use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::OwnedFd;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use rustix::fs::{Mode, OFlags};
use rustix::pty::{OpenptFlags, grantpt, openpt, ptsname, unlockpt};

struct Started(Child);

impl Drop for Started {
    fn drop(&mut self) {
        if self.0.try_wait().unwrap().is_none() {
            self.0.kill().unwrap();
        }
        self.0.wait().unwrap();
    }
}

#[test]
fn shared_lock_child() {
    let Some(path) = std::env::var_os("ARCHON_START_LOCK_CHILD") else {
        return;
    };
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
        .unwrap();
    file.try_lock_shared().unwrap();
    std::fs::write(Path::new(&path).with_extension("ready"), "ready").unwrap();
    std::thread::sleep(Duration::from_secs(20));
}

fn lock_holder(path: &Path) -> Started {
    let child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "shared_lock_child"])
        .env("ARCHON_START_LOCK_CHILD", path)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut child = Started(child);
    let deadline = Instant::now() + Duration::from_secs(5);
    while !path.with_extension("ready").exists() {
        assert!(Instant::now() < deadline, "lock holder did not start");
        assert!(child.0.try_wait().unwrap().is_none(), "lock holder exited");
        std::thread::sleep(Duration::from_millis(20));
    }
    child
}

fn terminal() -> (File, File) {
    let master = openpt(OpenptFlags::RDWR | OpenptFlags::NOCTTY).unwrap();
    grantpt(&master).unwrap();
    unlockpt(&master).unwrap();
    let name = ptsname(&master, Vec::new()).unwrap();
    let slave: OwnedFd = rustix::fs::open(
        std::ffi::OsStr::from_bytes(name.to_bytes()),
        OFlags::RDWR | OFlags::NOCTTY,
        Mode::empty(),
    )
    .unwrap();
    rustix::fs::fcntl_setfl(&master, OFlags::NONBLOCK).unwrap();
    (master.into(), slave.into())
}

fn wait_for_prompt(master: &mut File, child: &mut Child) -> String {
    let mut text = String::new();
    let deadline = Instant::now() + Duration::from_secs(8);
    while !text.contains("[y/N]") {
        let mut buffer = [0; 4096];
        match master.read(&mut buffer) {
            Ok(n) => text.push_str(&String::from_utf8_lossy(&buffer[..n])),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(error) => panic!("reading terminal: {error}"),
        }
        assert!(
            child.try_wait().unwrap().is_none(),
            "daemon exited before prompt: {text}"
        );
        assert!(Instant::now() < deadline, "prompt timed out: {text}");
        std::thread::sleep(Duration::from_millis(20));
    }
    text
}

#[test]
fn confirming_stop_retries_real_daemon_start_and_no_leaves_holder() {
    for (args, answer) in [
        (&["daemon", "run"][..], "no"),
        (&["daemon", "start"][..], "no"),
        (&["daemon", "run"][..], "yes"),
    ] {
        let env = common::Env::new();
        std::fs::create_dir_all(env.path("data")).unwrap();
        let path = env.path("data/data.lock");
        File::create(&path).unwrap();
        let mut holder = lock_holder(&path);
        let (mut master, slave) = terminal();
        let child = env
            .command(args)
            .stdin(slave.try_clone().unwrap())
            .stdout(Stdio::null())
            .stderr(slave)
            .spawn()
            .unwrap();
        let mut daemon = Started(child);
        let text = wait_for_prompt(&mut master, &mut daemon.0);
        assert!(text.contains(&holder.0.id().to_string()), "{text}");
        master.write_all(format!("{answer}\n").as_bytes()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(8);
        loop {
            if answer == "no" && daemon.0.try_wait().unwrap().is_some() {
                break;
            }
            if answer == "yes" && env.path("archond.sock").exists() {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "startup did not complete for {answer}"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        if answer == "no" {
            assert!(holder.0.try_wait().unwrap().is_none());
            assert!(!daemon.0.try_wait().unwrap().unwrap().success());
        } else {
            assert!(holder.0.try_wait().unwrap().is_some());
            assert!(daemon.0.try_wait().unwrap().is_none());
        }
    }
}

#[test]
fn noninteractive_start_names_holder_and_does_not_stop_it() {
    let env = common::Env::new();
    std::fs::create_dir_all(env.path("data")).unwrap();
    let path = env.path("data/data.lock");
    File::create(&path).unwrap();
    let mut holder = lock_holder(&path);
    let output = env
        .command(&["daemon", "run"])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "{text}");
    assert!(
        text.contains(&holder.0.id().to_string()) && text.contains("kill -TERM"),
        "{text}"
    );
    assert!(!text.contains("[y/N]"), "{text}");
    assert!(holder.0.try_wait().unwrap().is_none());
}
