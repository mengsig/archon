//! Aether's identifiers as Archon stores them. Distinct types so an agent id
//! can never be passed where a task id is expected: Aether's own docs warn
//! the two are different identifiers.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::error::ArchonError;

macro_rules! aether_id {
    ($name:ident, $prefix:literal) => {
        #[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
        #[serde(try_from = "String", into = "String")]
        pub struct $name(String);

        impl $name {
            pub const PREFIX: &'static str = $prefix;

            /// Checks only the prefix: the suffix is Aether's to define.
            pub fn parse(raw: &str) -> Result<Self, ArchonError> {
                let valid = raw
                    .strip_prefix(concat!($prefix, "_"))
                    .is_some_and(|rest| !rest.is_empty());
                if !valid {
                    return Err(ArchonError::Invalid(format!(
                        "{raw:?} is not an Aether {} (expected prefix {}_)",
                        stringify!($name),
                        $prefix
                    )));
                }
                Ok(Self(raw.to_owned()))
            }

            pub fn as_str(&self) -> &str {
                &self.0
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

aether_id!(AetherAgentId, "agt");
aether_id!(AetherTaskId, "tsk");
aether_id!(AetherRunId, "run");
aether_id!(AetherArtifactId, "art");
aether_id!(AetherMemoryFeedbackId, "mfb");

/// An Aether memory id, checked exactly as Aether's schema states it
/// (`mem_` and an upper-case ULID): the operator types it for a forget.
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct AetherMemoryId(String);

impl AetherMemoryId {
    pub const PREFIX: &'static str = "mem";

    pub fn parse(raw: &str) -> Result<Self, ArchonError> {
        let ulid = raw.strip_prefix("mem_").unwrap_or_default();
        let exact = ulid.len() == crate::id::ULID_LEN
            && ulid.chars().all(|c| crate::id::ULID_ALPHABET.contains(c));
        if !exact {
            return Err(ArchonError::Invalid(format!(
                "{raw:?} is not an Aether memory id (expected mem_ and 26 upper-case ULID characters)"
            )));
        }
        assert!(
            raw.len() == 30 && raw.is_ascii(),
            "a checked memory id is fixed width"
        );
        Ok(Self(raw.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for AetherMemoryId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl fmt::Display for AetherMemoryId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl TryFrom<String> for AetherMemoryId {
    type Error = ArchonError;
    fn try_from(s: String) -> Result<Self, Self::Error> {
        Self::parse(&s)
    }
}

impl From<AetherMemoryId> for String {
    fn from(id: AetherMemoryId) -> String {
        id.0
    }
}

/// A pending Aether `ask_user` question: answered with `reply_to_question`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AetherQuestionRef {
    pub agent: AetherAgentId,
    /// Event sequence of the `question.asked` event.
    pub seq: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_and_task_ids_do_not_mix() {
        assert!(AetherAgentId::parse("agt_01ABC").is_ok());
        assert!(AetherAgentId::parse("tsk_01ABC").is_err());
        assert!(AetherTaskId::parse("tsk_").is_err());
        let id: AetherTaskId = serde_json::from_str("\"tsk_1\"").unwrap();
        assert_eq!(id.as_str(), "tsk_1");
    }

    #[test]
    fn memory_ids_match_aethers_pattern_exactly() {
        let id = AetherMemoryId::parse("mem_01ARZ3NDEKTSV4RRFFQ69G5FAV").unwrap();
        assert_eq!(id.to_string(), "mem_01ARZ3NDEKTSV4RRFFQ69G5FAV");
        for bad in [
            "mem_01arz3ndektsv4rrffq69g5fav",
            "mem_01ARZ3NDEKTSV4RRFFQ69G5FA",
            "mem_01ARZ3NDEKTSV4RRFFQ69G5FAVX",
            "mem_01ARZ3NDEKTSV4RRFFQ69G5FAI",
            "lsn_01ARZ3NDEKTSV4RRFFQ69G5FAV",
            "",
        ] {
            assert!(AetherMemoryId::parse(bad).is_err(), "{bad}");
        }
        assert!(serde_json::from_str::<AetherMemoryId>("\"mem_1\"").is_err());
    }
}
