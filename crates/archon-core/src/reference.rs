//! Explicit attachments sent through Aether's references capability.
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ReferenceKind {
    Image,
    File,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Reference {
    pub path: PathBuf,
    pub kind: ReferenceKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}
