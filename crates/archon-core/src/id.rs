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
                let invalid = || {
                    ArchonError::Invalid(format!(
                        "{raw:?} is not a valid id here (expected `{}_` plus {ULID_LEN} characters)",
                        $prefix
                    ))
                };
                let rest = raw.strip_prefix(concat!($prefix, "_")).ok_or_else(invalid)?;
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
    id_prefix(text, QueueItemId::PREFIX, "queue item")
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
    if ulid.is_empty() || ulid.len() > ULID_LEN {
        return Err(ArchonError::Invalid(format!(
            "{text:?} is not a {what} id or its start: expected `{kind}_` plus 1 to \
             {ULID_LEN} characters, such as `{kind}_01HZX`"
        )));
    }
    if let Some(bad) = ulid.chars().find(|c| !ULID_ALPHABET.contains(*c)) {
        return Err(ArchonError::Invalid(format!(
            "{text:?} is not a {what} id or its start: expected `{kind}_` plus characters \
             from 0-9 and A-Z without I, L, O or U, such as `{kind}_01HZX`, but found {bad:?}"
        )));
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
                .contains("is not a initiative id or its start"),
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
