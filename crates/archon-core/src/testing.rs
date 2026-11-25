//! Helpers for tests across the workspace, behind the `testing` feature.

use std::io::Write as _;
use std::path::Path;
use std::process::{Command, Stdio};

/// One message a fake agent backend was asked to send.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SentMessage {
    /// The recipient.
    pub to: crate::aether_ids::AetherAgentId,
    /// The text.
    pub content: String,
    /// The idempotency key.
    pub key: String,
    /// The pictures; empty for a plain `send_message`.
    pub attachments: Vec<crate::attachment::ImageAttachment>,
}

/// What a fake agent backend keeps of its sent messages; tests read them
/// with [`SentMessages::all`]. Clones share one list.
#[derive(Debug, Clone, Default)]
pub struct SentMessages(std::sync::Arc<std::sync::Mutex<Vec<SentMessage>>>);

impl SentMessages {
    /// Record one message, attachments included.
    pub fn record(
        &self,
        to: &crate::aether_ids::AetherAgentId,
        content: &str,
        key: &str,
        attachments: &[crate::attachment::ImageAttachment],
    ) {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(SentMessage {
                to: to.clone(),
                content: content.to_owned(),
                key: key.to_owned(),
                attachments: attachments.to_vec(),
            });
    }

    /// Every message recorded so far, oldest first.
    pub fn all(&self) -> Vec<SentMessage> {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

/// Write `script` to `path` as an executable a test is about to run.
///
/// A `/bin/sh` child writes it under a temporary name, marks it executable
/// and renames it into place, so this process never holds it open for
/// writing. A fork on another test thread would inherit that descriptor, and
/// running the script before the fork's own exec closed it fails with
/// ETXTBSY ("Text file busy").
pub fn write_script(path: &Path, script: &str) {
    assert!(
        script.starts_with("#!"),
        "a script names its interpreter: {script:?}"
    );
    assert!(
        path.parent().is_some_and(Path::is_dir),
        "{} is not in an existing directory",
        path.display()
    );
    let install = r#"cat > "$1.partial" && chmod 755 "$1.partial" && mv -f "$1.partial" "$1""#;
    let mut writer = Command::new("/bin/sh")
        .args(["-c", install, "write_script"])
        .arg(path)
        .stdin(Stdio::piped())
        .spawn()
        .unwrap_or_else(|e| panic!("starting sh to write {}: {e}", path.display()));
    let mut stdin = writer.stdin.take().expect("stdin is piped");
    let sent = stdin.write_all(script.as_bytes());
    drop(stdin);
    let status = writer
        .wait()
        .unwrap_or_else(|e| panic!("waiting for sh to write {}: {e}", path.display()));
    assert!(
        status.success(),
        "sh failed to write {}: {status}",
        path.display()
    );
    sent.unwrap_or_else(|e| panic!("sending {} to sh: {e}", path.display()));
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Threads write scripts and run them at once while the others fork.
    #[test]
    fn a_script_runs_right_after_it_is_written_while_other_threads_fork() {
        let dir = tempfile::tempdir().unwrap();
        std::thread::scope(|scope| {
            for writer in 0..8 {
                let dir = dir.path();
                scope.spawn(move || {
                    for round in 0..100 {
                        let path = dir.join(format!("script-{writer}-{round}"));
                        write_script(&path, "#!/bin/sh\nexit 7\n");
                        let status = Command::new(&path)
                            .status()
                            .unwrap_or_else(|e| panic!("running {}: {e}", path.display()));
                        assert_eq!(status.code(), Some(7), "{}", path.display());
                    }
                });
            }
        });
        let partial = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .filter(|name| name.to_string_lossy().ends_with(".partial"))
            .count();
        assert_eq!(partial, 0, "every script was renamed into place");
    }

    #[test]
    fn write_script_replaces_an_existing_script() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("script");
        write_script(&path, "#!/bin/sh\nexit 3\n");
        write_script(&path, "#!/bin/sh\nexit 4\n");
        let status = Command::new(&path).status().unwrap();
        assert_eq!(status.code(), Some(4));
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text, "#!/bin/sh\nexit 4\n");
    }
}

/// The first model-name-like word of `text`: a model family of today's
/// providers, or a `provider/model` id. Guidance names tiers instead.
pub fn model_name_in(text: &str) -> Option<String> {
    const FAMILIES: [&str; 8] = [
        "opus", "sonnet", "haiku", "fable", "luna", "sol", "astra", "gpt",
    ];
    let lower = text.to_ascii_lowercase();
    let words = lower.split(|c: char| !(c.is_ascii_alphanumeric() || "-./_".contains(c)));
    words
        .filter(|w| !w.is_empty())
        .find(|word| {
            let parts = word.split(['-', '.', '/', '_']);
            let pinned = ["claude-code/", "codex/"].iter().any(|p| word.contains(p));
            pinned || parts.into_iter().any(|part| FAMILIES.contains(&part))
        })
        .map(str::to_owned)
}

#[cfg(test)]
mod model_name_tests {
    use super::model_name_in;

    #[test]
    fn finds_families_and_ids_but_not_tiers() {
        assert_eq!(
            model_name_in("use claude-opus-5-5 now").as_deref(),
            Some("claude-opus-5-5")
        );
        assert_eq!(
            model_name_in("pin `codex/x-1`").as_deref(),
            Some("codex/x-1")
        );
        assert_eq!(
            model_name_in("Sonnet, at medium").as_deref(),
            Some("sonnet")
        );
        assert_eq!(
            model_name_in("the strong tier; frontier only if asked; solve it"),
            None
        );
    }
}
