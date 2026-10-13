//! Prefixed ULID identifiers. The prefix makes an id self-describing in logs
//! and refuses a task id where a run id was meant.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::error::ArchonError;

macro_rules! prefixed_id {
    ($name:ident, $prefix:literal) => {
        #[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
        #[serde(try_from = "String", into = "String")]
        pub struct $name(String);

        impl $name {
            pub const PREFIX: &'static str = $prefix;

            pub fn new() -> Self {
                Self(format!("{}_{}", $prefix, ulid::Ulid::generate()))
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }

            /// Accepts only `<prefix>_<ULID>`; anything else is a caller bug or
            /// corrupted storage, never silently accepted.
            pub fn parse(raw: &str) -> Result<Self, ArchonError> {
                let Some(rest) = raw.strip_prefix(concat!($prefix, "_")) else {
                    return Err(ArchonError::Invalid(format!(
                        "{raw:?} is not a {} id (expected prefix {})",
                        stringify!($name),
                        $prefix
                    )));
                };
                ulid::Ulid::from_string(rest).map_err(|e| {
                    ArchonError::Invalid(format!("{raw:?} has a malformed ULID: {e}"))
                })?;
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
// One Archon store's identity. Keeps Archon's idempotency keys distinct
// across stores, since a fresh store restarts its counters.
prefixed_id!(StoreId, "store");

/// Crockford base32, the alphabet of a ULID's 26 characters.
const ULID_ALPHABET: &str = "0123456789ABCDEFGHJKMNPQRSTVWXYZ";
const ULID_LEN: usize = 26;

/// The start of a queue item id as an operator types it: `01k2`, `QI_01K2`
/// or a whole id. Returns it as ids are stored, `qi_` and upper case.
pub fn queue_item_prefix(text: &str) -> Result<String, ArchonError> {
    let trimmed = text.trim();
    let bare = match trimmed.get(..3) {
        Some(head) if head.eq_ignore_ascii_case("qi_") => &trimmed[3..],
        _ => trimmed,
    };
    let ulid = bare.to_ascii_uppercase();
    if ulid.is_empty() || ulid.len() > ULID_LEN {
        return Err(ArchonError::Invalid(format!(
            "{text:?} is not a queue item id or its start: give 1 to {ULID_LEN} characters \
             after qi_"
        )));
    }
    if let Some(bad) = ulid.chars().find(|c| !ULID_ALPHABET.contains(*c)) {
        return Err(ArchonError::Invalid(format!(
            "{text:?} is not a queue item id or its start: {bad:?} is not a ULID character"
        )));
    }
    let prefix = format!("{}_{ulid}", QueueItemId::PREFIX);
    assert!(prefix.is_ascii(), "a checked prefix is plain ASCII");
    Ok(prefix)
}

#[cfg(test)]
mod tests {
    use super::*;

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
