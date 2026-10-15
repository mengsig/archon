//! The real [`OpsPort`]: the programs on this machine, run without a shell
//! under a time limit, and the detached `archon daemon relaunch` helper.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use archon_core::error::ArchonError;
use tokio::process::Command;

use super::install::{InstallStep, Program};
use super::{OpsPort, RestartPlan};
use crate::executor::clip;

/// Most one install may take: `cargo install` builds Archon from source.
const INSTALL_TIMEOUT: Duration = Duration::from_secs(45 * 60);
/// Most a `--version` or `update --check` may take.
const PROBE_TIMEOUT: Duration = Duration::from_secs(60);
/// Characters of a failed command's output kept in its error.
const MAX_DETAIL_CHARS: usize = 1_500;
/// `<program> update --check` exits with this when a newer release exists.
const UPDATE_AVAILABLE: i32 = 10;

#[derive(Debug, Clone)]
pub struct SystemOps {
    /// This archond's executable; the restart helper is the same binary.
    pub archon_bin: PathBuf,
    /// Where commands run: never a repository, nor the operator's directory.
    pub workdir: PathBuf,
    /// archond's startup log, which the restart helper appends to.
    pub startup_log: PathBuf,
}

struct Ran {
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

impl SystemOps {
    async fn run(
        &self,
        program: &Path,
        args: &[&str],
        limit: Duration,
    ) -> Result<Ran, ArchonError> {
        let what = format!("{} {}", program.display(), args.join(" "));
        let child = Command::new(program)
            .args(args)
            .current_dir(&self.workdir)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0)
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| ArchonError::io(format!("running {what}"), e))?;
        let group = child
            .id()
            .and_then(|pid| rustix::process::Pid::from_raw(pid as i32));
        let Ok(output) = tokio::time::timeout(limit, child.wait_with_output()).await else {
            if let Some(group) = group {
                // Gone already is fine: kill_on_drop reaps it.
                let _ = rustix::process::kill_process_group(group, rustix::process::Signal::KILL);
            }
            return Err(ArchonError::Unavailable(format!(
                "{what} timed out after {}s",
                limit.as_secs()
            )));
        };
        let output = output.map_err(|e| ArchonError::io(what, e))?;
        Ok(Ran {
            code: output.status.code(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }
}

#[async_trait::async_trait]
impl OpsPort for SystemOps {
    fn locate(&self, program: Program) -> Option<PathBuf> {
        match program {
            Program::Archon => Some(self.archon_bin.clone()),
            Program::Aether => find_on_path(program.name()),
        }
    }

    fn cargo_bin(&self) -> Option<PathBuf> {
        archon_core::paths::cargo_bin_dir()
    }

    async fn install(&self, step: &InstallStep) -> Result<(), ArchonError> {
        let args: Vec<&str> = step.args.iter().map(String::as_str).collect();
        let ran = self.run(&step.command, &args, INSTALL_TIMEOUT).await?;
        if ran.code == Some(0) {
            return Ok(());
        }
        let said = format!("{}{}", ran.stderr.trim(), ran.stdout.trim());
        Err(ArchonError::Unavailable(format!(
            "{} failed ({}): {}",
            step.describe(),
            exit_text(ran.code),
            tail(&said)
        )))
    }

    async fn installed_version(&self, program: Program) -> Result<Option<String>, ArchonError> {
        let Some(binary) = self.locate(program) else {
            return Ok(None);
        };
        let ran = self.run(&binary, &["--version"], PROBE_TIMEOUT).await?;
        if ran.code != Some(0) {
            return Err(ArchonError::Unavailable(format!(
                "`{} --version` failed ({})",
                binary.display(),
                exit_text(ran.code)
            )));
        }
        let line = ran.stdout.lines().next().unwrap_or("");
        let version = archon_core::ops::bare_version(line);
        Ok((!version.is_empty()).then(|| version.to_owned()))
    }

    async fn latest_release(&self, program: Program) -> Result<Option<String>, ArchonError> {
        let Some(binary) = self.locate(program) else {
            return Ok(None);
        };
        let args = ["update", "--check", "--json"];
        let ran = self.run(&binary, &args, PROBE_TIMEOUT).await?;
        if !matches!(ran.code, Some(0 | UPDATE_AVAILABLE)) {
            return Err(ArchonError::Unavailable(format!(
                "`{} update --check` failed ({}): {}",
                binary.display(),
                exit_text(ran.code),
                tail(ran.stderr.trim())
            )));
        }
        latest_in(&ran.stdout).map(Some)
    }

    fn spawn_restart(&self, plan: &RestartPlan) -> Result<(), ArchonError> {
        let log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.startup_log)
            .map_err(|e| ArchonError::io(format!("opening {}", self.startup_log.display()), e))?;
        let stderr = log
            .try_clone()
            .map_err(|e| ArchonError::io("sharing the startup log", e))?;
        let mut command = Command::new(&self.archon_bin);
        command
            .args(plan.helper_args())
            .current_dir(&self.workdir)
            .stdin(Stdio::null())
            .stdout(log)
            .stderr(stderr)
            // Its own group, and never killed with archond: it outlives it.
            .process_group(0)
            .kill_on_drop(false);
        let mut child = command
            .spawn()
            .map_err(|e| ArchonError::io("starting `archon daemon relaunch`", e))?;
        // Reaped while archond still runs; once it exits, init adopts it.
        tokio::spawn(async move {
            match child.wait().await {
                Ok(status) => tracing::info!(%status, "restart helper exited"),
                Err(error) => tracing::warn!(%error, "restart helper not reaped"),
            }
        });
        Ok(())
    }
}

/// The release a `<program> update --check --json` names: archon's
/// `release`, aether's `latest`.
fn latest_in(json: &str) -> Result<String, ArchonError> {
    let value: serde_json::Value = serde_json::from_str(json.trim())?;
    let release = ["release", "latest"]
        .into_iter()
        .find_map(|key| value[key].as_str())
        .ok_or_else(|| {
            ArchonError::Protocol(format!("no release named in {}", tail(json.trim())))
        })?;
    Ok(archon_core::ops::bare_version(release).to_owned())
}

fn exit_text(code: Option<i32>) -> String {
    code.map_or_else(
        || "killed by a signal".into(),
        |code| format!("exit {code}"),
    )
}

/// The end of a command's output, where its error is.
fn tail(text: &str) -> String {
    let chars = text.chars().count();
    let skipped: String = text
        .chars()
        .skip(chars.saturating_sub(MAX_DETAIL_CHARS))
        .collect();
    clip(&skipped, MAX_DETAIL_CHARS)
}

/// The executable `name` on archond's `PATH`, canonical.
fn find_on_path(name: &str) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt as _;
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|candidate| {
            candidate
                .metadata()
                .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        })
        .and_then(|found| found.canonicalize().ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_latest_release_is_read_from_either_programs_check() {
        let archon = r#"{"status":"upgrade","current":"1.2.2","release":"1.2.3"}"#;
        assert_eq!(latest_in(archon).unwrap(), "1.2.3");
        let aether = r#"{"current":"1.0.6","latest":"v1.0.7","update_available":true}"#;
        assert_eq!(latest_in(aether).unwrap(), "1.0.7");
        assert!(latest_in(r#"{"current":"1.0.6"}"#).is_err());
        assert!(latest_in("not json").is_err());
    }

    fn script(dir: &Path, name: &str, body: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt as _;
        let path = dir.join(name);
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    fn ops(dir: &Path, archon_bin: PathBuf) -> SystemOps {
        SystemOps {
            archon_bin,
            workdir: dir.to_path_buf(),
            startup_log: dir.join("archond.log"),
        }
    }

    #[tokio::test]
    async fn a_fake_archon_reports_its_versions_and_a_failed_install() {
        let dir = tempfile::tempdir().unwrap();
        let body = r#"case "$1" in
--version) echo "archon 1.2.2" ;;
update) echo '{"status":"upgrade","current":"1.2.2","release":"1.2.3"}'; exit 10 ;;
esac"#;
        let ops = ops(dir.path(), script(dir.path(), "archon", body));
        let version = ops.installed_version(Program::Archon).await.unwrap();
        assert_eq!(version.as_deref(), Some("1.2.2"));
        let latest = ops.latest_release(Program::Archon).await.unwrap();
        assert_eq!(latest.as_deref(), Some("1.2.3"));

        let failing = script(dir.path(), "installer", "echo 'no such tag' >&2; exit 3");
        let step = InstallStep {
            program: Program::Archon,
            command: failing,
            args: vec!["update".into()],
        };
        let error = ops.install(&step).await.unwrap_err().to_string();
        assert!(
            error.contains("exit 3") && error.contains("no such tag"),
            "{error}"
        );
    }

    #[tokio::test]
    async fn the_restart_helper_is_the_same_binary_detached() {
        let dir = tempfile::tempdir().unwrap();
        let seen = dir.path().join("args");
        let body = format!("echo \"$@\" > {}.part && mv {0}.part {0}", seen.display());
        let ops = ops(dir.path(), script(dir.path(), "archon", &body));
        let plan = RestartPlan {
            restart_aether: false,
        };
        ops.spawn_restart(&plan).unwrap();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while !seen.exists() && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let args = std::fs::read_to_string(&seen).expect("the helper ran");
        assert_eq!(args.trim(), "daemon relaunch --keep-aether");
    }
}
