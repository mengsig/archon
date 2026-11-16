use crate::{context::DaemonContext, supervisor};
use archon_core::error::ArchonError;
use archon_core::intent::{ApprovedIntent, IntentSource};
use archon_core::protocol::Reply;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

pub(crate) fn inbox_key(agent: &str, seq: u64) -> String {
    format!("intent_inbox:{agent}:{seq}")
}

pub(super) async fn approve(
    ctx: &DaemonContext,
    mut record: ApprovedIntent,
) -> Result<Reply, ArchonError> {
    record.validate()?;
    let IntentSource::Inbox { seq } = record.source else {
        return Err(ArchonError::Invalid(
            "approve_intent requires an inbox source".into(),
        ));
    };
    let agent = supervisor::current_orchestrator(ctx)
        .await?
        .ok_or_else(|| ArchonError::Unavailable("no orchestrator".into()))?;
    let quote = verified_message(ctx, &agent, seq).await?;
    verify_quote(&record.operator_words, &quote)?;
    let destination = ctx.paths.data_dir.join("approved-intent-artifacts");
    for index in 0..record.artifacts.len() {
        let artifact = record.artifacts[index].clone();
        let saved = preserve(&destination, &artifact.path, &artifact.sha256)?;
        crate::executor::intent::extract_artifact(&artifact.path, &destination, &mut record)?;
        record.artifacts[index].path = saved;
    }
    for image in &mut record.images {
        image.path = preserve(&destination, &image.path, &image.sha256)?;
    }
    record.validate()?;
    record.check_operator_ledger()?;
    ctx.store.lock().await.save_approved_intent(&record)?;
    Ok(Reply::Ok)
}

async fn verified_message(
    ctx: &DaemonContext,
    agent: &archon_core::aether_ids::AetherAgentId,
    seq: u64,
) -> Result<String, ArchonError> {
    let key = inbox_key(&agent.to_string(), seq);
    if let Some(text) = ctx.store.lock().await.kv_get(&key)? {
        return Ok(text);
    }
    let client = ctx.aether.client()?;
    let query = archon_aether::wire::EventQuery {
        agent: Some(agent.clone()),
        types: vec!["inbox.delivered".into()],
        ..Default::default()
    };
    let mut after = 0;
    for _ in 0..8 {
        let page = client.events_after(after, query.clone(), 5000).await?;
        for event in &page {
            if event.agent.as_ref() != Some(agent) {
                continue;
            }
            if let archon_aether::events::EventKind::InboxDelivered {
                seq: delivered,
                message: Some(message),
                ..
            } = &event.kind
                && *delivered == seq
                && message.channel.is_some()
            {
                return Ok(message.text.clone());
            }
        }
        if page.len() < 5000 {
            return Err(ArchonError::NotFound(format!(
                "verified operator inbox message {seq}"
            )));
        }
        after = page.last().expect("full page").seq;
    }
    Err(ArchonError::Unavailable(
        "operator inbox history exceeds the search limit".into(),
    ))
}

fn verify_quote(words: &str, delivered: &str) -> Result<(), ArchonError> {
    if words.trim().is_empty() || words != delivered {
        return Err(ArchonError::Invalid(
            "operator words must equal the verified inbox message".into(),
        ));
    }
    Ok(())
}

pub(crate) fn preserve(
    directory: &Path,
    source: &Path,
    expected: &str,
) -> Result<PathBuf, ArchonError> {
    let metadata = std::fs::metadata(source)
        .map_err(|e| ArchonError::io(format!("read {}", source.display()), e))?;
    if !metadata.is_file() || metadata.len() > 32 * 1024 * 1024 {
        return Err(ArchonError::Invalid(format!(
            "{} is not a file within the 32 MiB limit",
            source.display()
        )));
    }
    let bytes = std::fs::read(source)
        .map_err(|e| ArchonError::io(format!("read {}", source.display()), e))?;
    let hash = Sha256::digest(&bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    if hash != expected {
        return Err(ArchonError::Invalid(format!(
            "sha256 mismatch for {}",
            source.display()
        )));
    }
    std::fs::create_dir_all(directory)
        .map_err(|e| ArchonError::io("create intent artifact directory", e))?;
    let extension = source
        .extension()
        .and_then(|s| s.to_str())
        .filter(|s| s.len() <= 8 && s.bytes().all(|b| b.is_ascii_alphanumeric()))
        .unwrap_or("bin");
    let target = directory.join(format!("{hash}.{extension}"));
    save_bytes(&target, &bytes)?;
    Ok(target)
}

pub(crate) fn save_bytes(target: &Path, bytes: &[u8]) -> Result<(), ArchonError> {
    use std::io::Write;
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(target)
    {
        Ok(mut file) => {
            file.write_all(bytes)
                .and_then(|()| file.sync_all())
                .map_err(|e| ArchonError::io(format!("write {}", target.display()), e))?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            let metadata = std::fs::symlink_metadata(target)
                .map_err(|e| ArchonError::io("inspect intent artifact", e))?;
            if !metadata.is_file() {
                return Err(ArchonError::Corrupt(format!(
                    "intent artifact {} is not a regular file",
                    target.display()
                )));
            }
            let existing = std::fs::read(target)
                .map_err(|e| ArchonError::io(format!("read {}", target.display()), e))?;
            if existing != bytes {
                return Err(ArchonError::Corrupt(format!(
                    "intent artifact {} changed",
                    target.display()
                )));
            }
        }
        Err(error) => {
            return Err(ArchonError::io(
                format!("create {}", target.display()),
                error,
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_exact_nonempty_operator_words_are_accepted() {
        assert!(verify_quote("Build this", "Build this").is_ok());
        assert!(verify_quote("Build", "Build this").is_err());
        assert!(verify_quote("", "").is_err());
    }

    #[test]
    fn files_are_copied_only_after_hash_verification() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("reference.png");
        std::fs::write(&source, b"reference").unwrap();
        let hash = Sha256::digest(b"reference")
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let saved = preserve(&root.path().join("saved"), &source, &hash).unwrap();
        std::fs::write(&source, b"changed").unwrap();
        assert_eq!(std::fs::read(saved).unwrap(), b"reference");
        assert!(preserve(&root.path().join("saved"), &source, &hash).is_err());
    }
}
