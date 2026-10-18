//! Citations are looked up in archond's own durable event observations.

use archon_core::aether_ids::AetherAgentId;
use archon_core::authority::OperatorMessage;
use archon_core::error::ArchonError;

use crate::context::DaemonContext;

const PREFIX: &str = "operator_message:";
pub(crate) const MAX_MESSAGE_CHARS: usize = 4000;

fn key(agent: &AetherAgentId, seq: u64) -> String {
    format!("{PREFIX}{agent}:{seq}")
}

pub(crate) async fn record(
    ctx: &DaemonContext,
    message: &OperatorMessage,
) -> Result<(), ArchonError> {
    let mut store = ctx.store.lock().await;
    let agent = message.agent.as_ref().ok_or_else(|| {
        ArchonError::Invalid("only inbox deliveries can be recorded as citations".into())
    })?;
    let key = key(agent, message.seq);
    // Replays cannot change the original provenance or renew consent.
    if store.kv_get(&key)?.is_none() {
        store.kv_set(&key, &serde_json::to_string(message)?)?;
    }
    let now = archon_core::now();
    const PRUNED: &str = "operator_messages_pruned_at";
    if let Some(last) = store.kv_get(PRUNED)? {
        let last: archon_core::Timestamp = serde_json::from_str(&last)?;
        if now - last < chrono::Duration::hours(1) {
            return Ok(());
        }
    }
    let cutoff = now - chrono::Duration::days(7);
    for (key, raw) in store.kv_prefix(PREFIX)? {
        let old: OperatorMessage = serde_json::from_str(&raw)?;
        if old.at < cutoff {
            store.kv_delete(&key)?;
        }
    }
    store.kv_set(PRUNED, &serde_json::to_string(&now)?)?;
    Ok(())
}

pub(crate) async fn cite(
    ctx: &DaemonContext,
    agent: &AetherAgentId,
    seq: u64,
    quote: &str,
) -> Result<OperatorMessage, ArchonError> {
    let raw = ctx.store.lock().await.kv_get(&key(agent, seq))?.ok_or_else(|| {
        ArchonError::Forbidden("the cited inbox message is not a verified operator message; ask the operator again".into())
    })?;
    let message: OperatorMessage = serde_json::from_str(&raw)?;
    if !message.channel.is_github()
        && ctx.config.aether.mode != archon_core::config::AetherMode::Dedicated
    {
        return Err(ArchonError::Forbidden(
            "operator chat citations are disabled in shared mode".into(),
        ));
    }
    let age = archon_core::now() - message.at;
    if age < chrono::Duration::zero() || age > chrono::Duration::minutes(60) {
        return Err(ArchonError::Forbidden(
            "the operator citation is outside its 60-minute window; ask again".into(),
        ));
    }
    check_quote(&message.text, quote)?;
    Ok(message)
}

const MIN_QUOTE_WORDS: usize = 3;
const MIN_QUOTE_CHARS: usize = 12;

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// True when `needle` occurs in `haystack` starting and ending on word boundaries.
fn contains_on_word_boundaries(haystack: &str, needle: &str) -> bool {
    haystack.char_indices().any(|(start, _)| {
        if !haystack[start..].starts_with(needle) {
            return false;
        }
        let before = haystack[..start].chars().next_back();
        let after = haystack[start + needle.len()..].chars().next();
        let starts_ok = needle.chars().next().is_some_and(|c| !is_word_char(c))
            || !before.is_some_and(is_word_char);
        let ends_ok = needle.chars().next_back().is_some_and(|c| !is_word_char(c))
            || !after.is_some_and(is_word_char);
        starts_ok && ends_ok
    })
}

/// The quote must be verbatim, on word boundaries, and substantial (or the
/// whole message): a fragment such as `t` proves nothing about intent.
fn check_quote(message: &str, quote: &str) -> Result<(), ArchonError> {
    let trimmed = quote.trim();
    if trimmed.is_empty() || !contains_on_word_boundaries(message, trimmed) {
        return Err(ArchonError::Forbidden(
            "quote must occur verbatim, on word boundaries, in the verified operator message"
                .into(),
        ));
    }
    let words = trimmed.split_whitespace().count();
    let chars = trimmed.chars().filter(|c| !c.is_whitespace()).count();
    let whole_message = trimmed == message.trim();
    if !whole_message && words < MIN_QUOTE_WORDS && chars < MIN_QUOTE_CHARS {
        return Err(ArchonError::Forbidden(format!(
            "quote must be at least {MIN_QUOTE_WORDS} words or {MIN_QUOTE_CHARS} characters, or the whole message"
        )));
    }
    Ok(())
}

/// A gate answer must be backed by its answer word appearing in the quote.
pub(crate) fn check_answer_quote(quote: &str, answer: &str) -> Result<(), ArchonError> {
    let verb = answer
        .split_whitespace()
        .next()
        .ok_or_else(|| ArchonError::Invalid("a gate answer must not be empty".into()))?;
    if !contains_on_word_boundaries(&quote.to_lowercase(), &verb.to_lowercase()) {
        return Err(ArchonError::Forbidden(format!(
            "the quote must contain the answer word `{verb}`"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_fragments_and_mid_word_quotes_are_refused() {
        let message = "what is the status of the queue?";
        assert!(check_quote(message, "t").is_err());
        assert!(check_quote(message, "s ").is_err());
        assert!(check_quote(message, "stat").is_err());
        assert!(check_quote(message, "status of the").is_ok());
        assert!(check_quote("yes", "yes").is_ok());
        assert!(check_quote(message, "the queue").is_err());
        assert!(check_quote("cargo go go go", "go go go").is_ok());
    }

    #[test]
    fn answer_word_must_appear_in_the_quote() {
        assert!(check_answer_quote("please approve this proposal", "approve").is_ok());
        assert!(check_answer_quote("what is the status", "approve").is_err());
        assert!(check_answer_quote("anything", " ").is_err());
    }
}
