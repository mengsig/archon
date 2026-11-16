//! Optional `lavish-axi` plugin (ARCHITECTURE §14): publish HTML reports as
//! review surfaces and turn operator annotations into orchestrator input.
//! Absent binary or `enabled = "off"` yields `None` from [`Lavish::detect`];
//! callers then fall back to plain file paths. Nothing here fails because
//! Lavish is missing.

pub mod intent;

mod toon;

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use archon_core::config::{LavishConfig, Toggle};
use archon_core::error::ArchonError;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

/// `open` and `end` return promptly; only `poll` may block indefinitely.
const COMMAND_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lavish {
    binary: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LavishSession {
    pub html: PathBuf,
    pub url: String,
}

/// What one `lavish-axi poll` returned. `raw` is always the full output so
/// nothing the operator wrote is lost to a parser. The daemon passes it on
/// without the page's `dom_snapshot`, byte-bounded (ARCHITECTURE §14).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LavishFeedback {
    Prompts {
        raw: String,
        prompts: Vec<String>,
    },
    /// `Send & End`: final feedback delivered; never reopen uninvited.
    /// `prompts` is its final feedback, parsed best effort.
    Ended {
        raw: String,
        prompts: Vec<String>,
    },
    /// Review window gone past its grace period; ask before reopening.
    Disconnected {
        raw: String,
    },
    /// The review surface itself failed to load.
    ArtifactFailure {
        raw: String,
    },
}

impl Lavish {
    /// `Ok(None)` when disabled or (in `auto` mode) not installed; an error
    /// only when `enabled = "on"` and the binary cannot be found.
    pub fn detect(config: &LavishConfig) -> Result<Option<Self>, ArchonError> {
        if config.enabled == Toggle::Off {
            return Ok(None);
        }
        match find_executable(&config.binary) {
            Some(binary) => Ok(Some(Self { binary })),
            None if config.enabled == Toggle::On => Err(ArchonError::Unavailable(format!(
                "lavish is enabled but {} was not found",
                config.binary.display()
            ))),
            None => Ok(None),
        }
    }

    pub fn binary(&self) -> &Path {
        &self.binary
    }

    /// `lavish-axi <html>`: open or resume a session and return its URL. A
    /// session the operator ended from the browser is not reopened.
    pub async fn open(&self, html: &Path) -> Result<LavishSession, ArchonError> {
        self.open_session(html, false).await
    }

    /// `lavish-axi <html> --reopen`: like [`Lavish::open`], but a session the
    /// operator ended from the browser is reopened too. Only for an explicit
    /// operator request.
    pub async fn reopen(&self, html: &Path) -> Result<LavishSession, ArchonError> {
        self.open_session(html, true).await
    }

    /// `reopen` passes `--reopen`, after which lavish-axi never refuses.
    async fn open_session(&self, html: &Path, reopen: bool) -> Result<LavishSession, ArchonError> {
        require_absolute(html)?;
        let mut args = vec![html.as_os_str()];
        if reopen {
            args.push(OsStr::new("--reopen"));
        }
        let raw = self.run(&args, None, Some(COMMAND_TIMEOUT)).await?;
        let status = toon::session_field(&raw, "status");
        match status.as_deref() {
            Some("user-ended") if reopen => return Err(unexpected("open --reopen", &raw)),
            Some("user-ended") => {
                return Err(ArchonError::Unavailable(format!(
                    "the operator ended the review of {}; not reopening it uninvited",
                    html.display()
                )));
            }
            _ => {}
        }
        let url = toon::session_field(&raw, "url")
            .filter(|url| url.starts_with("http"))
            .ok_or_else(|| unexpected("open", &raw))?;
        Ok(LavishSession {
            html: html.to_path_buf(),
            url,
        })
    }

    /// `lavish-axi poll <html> [--agent-reply-file -]`: blocks until the
    /// operator sends feedback. Run one supervised task per open report.
    pub async fn poll(
        &self,
        html: &Path,
        reply: Option<&str>,
    ) -> Result<LavishFeedback, ArchonError> {
        require_absolute(html)?;
        let mut reply = reply.filter(|text| !text.trim().is_empty());
        loop {
            let mut args = vec![OsStr::new("poll"), html.as_os_str()];
            if reply.is_some() {
                args.extend([OsStr::new("--agent-reply-file"), OsStr::new("-")]);
            }
            let raw = self.run(&args, reply, None).await?;
            // lavish-axi posts the reply before it waits, so a re-poll must not repeat it.
            reply = None;
            if let Some(feedback) = classify(raw)? {
                return Ok(feedback);
            }
        }
    }

    pub async fn end(&self, html: &Path) -> Result<(), ArchonError> {
        require_absolute(html)?;
        let raw = self
            .run(
                &[OsStr::new("end"), html.as_os_str()],
                None,
                Some(COMMAND_TIMEOUT),
            )
            .await?;
        match toon::session_field(&raw, "status").as_deref() {
            Some("ended") => Ok(()),
            _ => Err(unexpected("end", &raw)),
        }
    }

    /// Runs the binary and returns stdout; a non-zero exit is an error carrying
    /// both streams. The child is killed if the future is dropped.
    async fn run(
        &self,
        args: &[&OsStr],
        stdin: Option<&str>,
        limit: Option<Duration>,
    ) -> Result<String, ArchonError> {
        assert!(!args.is_empty(), "lavish-axi always takes an argument");
        let context = || format!("{} {:?}", self.binary.display(), args);
        let mut child = Command::new(&self.binary)
            .args(args)
            .stdin(if stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| ArchonError::io(context(), e))?;
        if let Some(text) = stdin {
            let mut pipe = child.stdin.take().expect("stdin was requested as a pipe");
            pipe.write_all(text.as_bytes())
                .await
                .map_err(|e| ArchonError::io(context(), e))?;
        }
        let waited = child.wait_with_output();
        let output = match limit {
            Some(limit) => tokio::time::timeout(limit, waited).await.map_err(|_| {
                ArchonError::Unavailable(format!("{} timed out after {limit:?}", context()))
            })?,
            None => waited.await,
        }
        .map_err(|e| ArchonError::io(context(), e))?;
        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        if output.status.success() {
            return Ok(stdout);
        }
        let stderr = String::from_utf8_lossy(&output.stderr);
        Err(ArchonError::Unavailable(format!(
            "{} failed ({}): {}{}",
            context(),
            output.status,
            stdout.trim(),
            stderr.trim()
        )))
    }
}

/// `None` for `waiting` (lavish-axi's own timeout): poll again.
fn classify(raw: String) -> Result<Option<LavishFeedback>, ArchonError> {
    let status = toon::session_field(&raw, "status").ok_or_else(|| unexpected("poll", &raw))?;
    let feedback = match status.as_str() {
        "waiting" => return Ok(None),
        "ended" => LavishFeedback::Ended {
            prompts: toon::prompts(&raw),
            raw,
        },
        "browser_disconnected" => LavishFeedback::Disconnected { raw },
        "feedback" if toon::session_field(&raw, "session_ended").as_deref() == Some("true") => {
            LavishFeedback::Ended {
                prompts: toon::prompts(&raw),
                raw,
            }
        }
        "feedback" if toon::has_artifact_failures(&raw) => LavishFeedback::ArtifactFailure { raw },
        "feedback" => LavishFeedback::Prompts {
            prompts: toon::prompts(&raw),
            raw,
        },
        _ => return Err(unexpected("poll", &raw)),
    };
    Ok(Some(feedback))
}

/// lavish-axi resolves relative paths against its own cwd, not the caller's.
fn require_absolute(html: &Path) -> Result<(), ArchonError> {
    if html.is_absolute() {
        return Ok(());
    }
    Err(ArchonError::Invalid(format!(
        "report path {} must be absolute",
        html.display()
    )))
}

fn unexpected(command: &str, raw: &str) -> ArchonError {
    ArchonError::Protocol(format!("unexpected lavish-axi {command} output: {raw}"))
}

/// An absolute or relative path is taken as is; a bare name is searched on
/// `PATH`.
fn find_executable(binary: &Path) -> Option<PathBuf> {
    if binary.components().count() > 1 {
        return binary.is_file().then(|| binary.to_path_buf());
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(binary))
        .find(|candidate| candidate.is_file())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn off_and_missing_degrade_to_none_but_on_requires_the_binary() {
        let off = LavishConfig {
            enabled: Toggle::Off,
            ..LavishConfig::default()
        };
        assert_eq!(Lavish::detect(&off).unwrap(), None);
        let missing = LavishConfig {
            enabled: Toggle::Auto,
            binary: "/nonexistent/lavish-axi".into(),
        };
        assert_eq!(Lavish::detect(&missing).unwrap(), None);
        let required = LavishConfig {
            enabled: Toggle::On,
            binary: "/nonexistent/lavish-axi".into(),
        };
        assert!(matches!(
            Lavish::detect(&required),
            Err(ArchonError::Unavailable(_))
        ));
    }

    #[test]
    fn explicit_paths_are_used_as_given() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("lavish-axi");
        std::fs::write(&bin, "#!/bin/sh\n").unwrap();
        let found = Lavish::detect(&LavishConfig {
            enabled: Toggle::On,
            binary: bin.clone(),
        })
        .unwrap()
        .unwrap();
        assert_eq!(found.binary(), bin);
    }
}
