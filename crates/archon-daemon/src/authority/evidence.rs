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

/// Words that negate an answer word they shortly precede; any word ending in
/// `n't` (don't, won't, shouldn't) does too.
const NEGATIONS: [&str; 6] = ["no", "not", "never", "nor", "cannot", "dont"];
/// How many words before the answer word a negation reaches.
const NEGATION_REACH: usize = 4;
/// Punctuation that ends a clause: "no, approve it" negates nothing.
const CLAUSE_ENDS: [char; 9] = [',', ';', '.', '!', '?', ':', '\n', '—', '–'];

/// A gate answer must be backed by its answer word appearing in the quote,
/// and not negated there: "don't approve it yet" backs no `approve`. The word
/// is read as `GateDecision::from_answer` reads it: `approve!` is `approve`.
pub(crate) fn check_answer_quote(quote: &str, answer: &str) -> Result<(), ArchonError> {
    let first = answer
        .split_whitespace()
        .next()
        .ok_or_else(|| ArchonError::Invalid("a gate answer must not be empty".into()))?;
    let verb = first
        .trim_end_matches(|c: char| c.is_ascii_punctuation())
        .to_lowercase();
    if verb.is_empty() {
        return Err(ArchonError::Invalid(format!(
            "a gate answer must start with a word, not `{first}`"
        )));
    }
    let quote = quote.to_lowercase().replace('’', "'");
    if !contains_on_word_boundaries(&quote, &verb) {
        return Err(ArchonError::Forbidden(format!(
            "the quote must contain the answer word `{verb}`"
        )));
    }
    if negates(&quote, &verb) {
        return Err(ArchonError::Forbidden(format!(
            "the quote negates the answer word `{verb}`: a no, not, never or n't stands up to \
             {NEGATION_REACH} words before it in the same clause; ask the operator to say it plainly"
        )));
    }
    Ok(())
}

/// Whether a negation stands within [`NEGATION_REACH`] words before an
/// occurrence of `verb` in the same clause. Both are lowercase.
fn negates(quote: &str, verb: &str) -> bool {
    assert!(!verb.is_empty(), "an answer word is never empty");
    assert_eq!(
        quote,
        quote.to_lowercase(),
        "the quote is compared lowercase"
    );
    quote.split(CLAUSE_ENDS).any(|clause| {
        let words: Vec<&str> = clause
            .split(|c: char| !is_word_char(c) && c != '\'')
            .map(|word| word.trim_matches('\''))
            .filter(|word| !word.is_empty())
            .collect();
        words.iter().enumerate().any(|(at, word)| {
            *word == verb
                && words[at.saturating_sub(NEGATION_REACH)..at]
                    .iter()
                    .any(|before| NEGATIONS.contains(before) || before.ends_with("n't"))
        })
    })
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
        assert!(check_answer_quote("anything !", "!").is_err());
    }

    #[test]
    fn a_negated_answer_word_is_refused() {
        for negated in [
            "don't approve the migration gate until CI is green",
            "do not approve this yet",
            "never approve it",
            "Don’t Approve it",
            "I do not want you to approve this",
            "no, don't approve",
        ] {
            for answer in ["approve", "approve!", "Approve.", "approve, ship it"] {
                let refused = check_answer_quote(negated, answer).unwrap_err();
                assert!(
                    refused.to_string().contains("negates"),
                    "{negated} / {answer}: {refused}"
                );
            }
        }
    }

    #[test]
    fn ordinary_approvals_are_accepted() {
        for plain in [
            "approve it",
            "yes approve it",
            "approve, it's not risky",
            "no, approve it",
            "no problem. approve the gate",
            "approve it; do not wait for CI",
            "please 'approve' it",
        ] {
            assert!(check_answer_quote(plain, "approve").is_ok(), "{plain}");
            assert!(check_answer_quote(plain, "approve!").is_ok(), "{plain}");
        }
        assert!(check_answer_quote("not now, reject it", "reject").is_ok());
        assert!(check_answer_quote("I can't see why not to reject", "reject").is_err());
    }
}
