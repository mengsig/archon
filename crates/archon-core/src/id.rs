//! Prefixed ULID identifiers. The prefix makes an id self-describing in logs
//! and refuses a task id where a run id was meant.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::error::ArchonError;

/// The next ULID of this process. Ids made in the same millisecond still
/// sort in the order they were made, so a delivery order is never a coin flip.
fn next_ulid() -> ulid::Ulid {
    static GENERATOR: std::sync::Mutex<Option<ulid::Generator>> = std::sync::Mutex::new(None);
    let mut guard = GENERATOR.lock().unwrap_or_else(|e| e.into_inner());
    let generator = guard.get_or_insert_with(ulid::Generator::new);
    match generator.generate() {
        Ok(ulid) => ulid,
        // Over 2^80 ids in one millisecond: the random fallback is as good as it gets.
        Err(overflow) => overflow.commit_overflow_random(),
    }
}

macro_rules! prefixed_id {
    ($name:ident, $prefix:literal) => {
        #[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
        #[serde(try_from = "String", into = "String")]
        pub struct $name(String);

        impl $name {
            pub const PREFIX: &'static str = $prefix;

            pub fn new() -> Self {
                Self(format!("{}_{}", $prefix, next_ulid()))
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }

            /// Accepts only `<prefix>_<ULID>`; anything else is a caller bug or
            /// corrupted storage, never silently accepted.
            pub fn parse(raw: &str) -> Result<Self, ArchonError> {
                let invalid =
                    || ArchonError::Invalid(bad_id_message(raw, $prefix, kind_name($prefix)));
                let rest = raw
                    .strip_prefix(concat!($prefix, "_"))
                    .ok_or_else(invalid)?;
                ulid::Ulid::from_string(rest).map_err(|_| invalid())?;
                Ok(Self(raw.to_owned()))
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl FromStr for $name {
            type Err = ArchonError;
            fn from_str(s: &str) -> Result<Self, Self::Err> {
                Self::parse(s)
            }
        }

        impl TryFrom<String> for $name {
            type Error = ArchonError;
            fn try_from(s: String) -> Result<Self, Self::Error> {
                Self::parse(&s)
            }
        }

        impl From<$name> for String {
            fn from(id: $name) -> String {
                id.0
            }
        }
    };
}

prefixed_id!(RepoId, "repo");
prefixed_id!(QueueItemId, "qi");
prefixed_id!(PipelineRunId, "prun");
prefixed_id!(StageRunId, "stg");
prefixed_id!(NotificationId, "ntf");
prefixed_id!(QuestionId, "q");
prefixed_id!(FeedItemId, "feed");
prefixed_id!(ReportId, "rpt");
prefixed_id!(DirectiveId, "dir");
prefixed_id!(InitiativeId, "ini");
prefixed_id!(LessonId, "lsn");
prefixed_id!(FeedbackId, "fb");
prefixed_id!(MergeRequestId, "mr");
// One Archon store's identity. Keeps Archon's idempotency keys distinct
// across stores, since a fresh store restarts its counters.
prefixed_id!(StoreId, "store");

/// Crockford base32, the alphabet of a ULID's 26 characters.
pub(crate) const ULID_ALPHABET: &str = "0123456789ABCDEFGHJKMNPQRSTVWXYZ";
pub(crate) const ULID_LEN: usize = 26;

/// The start of a queue item id as an operator types it: `01k2`, `QI_01K2`
/// or a whole id. Returns it as ids are stored, `qi_` and upper case.
pub fn queue_item_prefix(text: &str) -> Result<String, ArchonError> {
    id_prefix(text, QueueItemId::PREFIX, "item")
}

fn kind_name(kind: &str) -> &str {
    match kind {
        "qi" => "item",
        "prun" => "pipeline run",
        "q" => "question",
        other => other,
    }
}

/// What to tell an operator who typed something that is not an id of `kind`:
/// the value, the expected format and where to find real ids.
fn bad_id_message(text: &str, kind: &str, what: &str) -> String {
    let listing = if kind == QueueItemId::PREFIX {
        "run `archon queue` to list item ids"
    } else {
        "list them with the matching `archon` list command"
    };
    format!(
        "'{}' is not a valid {what} id. Expected `{kind}_` followed by {ULID_LEN} characters \
         (0-9 and A-Z without I, L, O or U), or the start of one, such as `{kind}_01HZX`; {listing}",
        text.trim()
    )
}

/// The start of an id of `kind` (its prefix, such as `ini`) as an operator
/// types it, with or without `<kind>_`, in any case; `what` names the kind.
pub fn id_prefix(text: &str, kind: &str, what: &str) -> Result<String, ArchonError> {
    assert!(!kind.is_empty() && kind.is_ascii(), "id kinds are ASCII");
    let trimmed = text.trim();
    let head = kind.len() + 1;
    let bare = match (trimmed.get(..kind.len()), trimmed.get(kind.len()..head)) {
        (Some(given), Some("_")) if given.eq_ignore_ascii_case(kind) => &trimmed[head..],
        _ => trimmed,
    };
    let ulid = bare.to_ascii_uppercase();
    let well_formed = !ulid.is_empty()
        && ulid.len() <= ULID_LEN
        && ulid.chars().all(|c| ULID_ALPHABET.contains(c));
    if !well_formed {
        return Err(ArchonError::Invalid(bad_id_message(text, kind, what)));
    }
    let prefix = format!("{kind}_{ulid}");
    assert!(prefix.is_ascii(), "a checked prefix is plain ASCII");
    Ok(prefix)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_made_in_a_row_sort_in_the_order_they_were_made() {
        let ids: Vec<NotificationId> = (0..1000).map(|_| NotificationId::new()).collect();
        assert!(ids.windows(2).all(|pair| pair[0] < pair[1]));
    }

    #[test]
    fn item_prefixes_take_any_case_with_or_without_qi() {
        let id = QueueItemId::new();
        let ulid = &id.as_str()[3..];
        assert_eq!(queue_item_prefix(id.as_str()).unwrap(), id.as_str());
        assert_eq!(queue_item_prefix(ulid).unwrap(), id.as_str());
        assert_eq!(queue_item_prefix(" 01k2 ").unwrap(), "qi_01K2");
        assert_eq!(queue_item_prefix("QI_01k2").unwrap(), "qi_01K2");
        for bad in ["", "qi_", "01k2%", "qi_01_", "01U", &format!("{ulid}0")] {
            assert!(queue_item_prefix(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn a_malformed_item_id_is_named_with_the_format_and_where_to_look() {
        let error = queue_item_prefix("nonexistent").unwrap_err().to_string();
        assert!(
            error.contains("'nonexistent' is not a valid item id"),
            "{error}"
        );
        assert!(error.contains("`qi_` followed by 26 characters"), "{error}");
        assert!(error.contains("archon queue"), "{error}");
        assert!(!error.contains("found"), "{error}");
        let parsed = QueueItemId::parse("nonexistent").unwrap_err().to_string();
        assert!(
            parsed.contains("'nonexistent' is not a valid item id"),
            "{parsed}"
        );
    }

    #[test]
    fn prefixes_of_other_kinds_take_their_own_prefix() {
        let id = InitiativeId::new();
        assert_eq!(
            id_prefix(id.as_str(), "ini", "initiative").unwrap(),
            id.as_str()
        );
        assert_eq!(
            id_prefix("INI_01k2", "ini", "initiative").unwrap(),
            "ini_01K2"
        );
        assert_eq!(id_prefix("01k2", "ini", "initiative").unwrap(), "ini_01K2");
        let wrong = id_prefix("qi_01K2", "ini", "initiative").unwrap_err();
        assert!(
            wrong
                .to_string()
                .contains("'qi_01K2' is not a valid initiative id"),
            "{wrong}"
        );
    }

    #[test]
    fn round_trips_through_text_and_json() {
        let id = QueueItemId::new();
        assert!(id.as_str().starts_with("qi_"));
        assert_eq!(QueueItemId::parse(id.as_str()).unwrap(), id);
        let json = serde_json::to_string(&id).unwrap();
        let back: QueueItemId = serde_json::from_str(&json).unwrap();
        assert_eq!(back, id);
    }

    #[test]
    fn refuses_foreign_prefixes_and_garbage() {
        let run = PipelineRunId::new();
        assert!(QueueItemId::parse(run.as_str()).is_err());
        assert!(QueueItemId::parse("qi_notaulid").is_err());
        assert!(QueueItemId::parse("").is_err());
    }
}
