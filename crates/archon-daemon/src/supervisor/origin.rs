//! What each orchestrator generation was created with, so `status`, `doctor`
//! and `queue_status` can say when its prompt, tools or lessons are stale.

use archon_core::error::ArchonError;
use archon_core::id::LessonId;
use archon_core::mcp_tools::TOOLS;
use archon_core::protocol::{OrchestratorFreshness, SettingsDrift};
use archon_queue::Store;
use serde::{Deserialize, Serialize};

use super::ORCHESTRATOR_HIDDEN_TOOLS;

/// The newest generation's [`Origin`], as JSON.
const ORIGIN_KEY: &str = "orchestrator_origin";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Origin {
    generation: u64,
    archon_version: String,
    /// [`fingerprint`] of its instructions before the lessons were added.
    prompt: String,
    tools: String,
    /// `provider/model effort` it was created with; empty when an older
    /// archond recorded the origin.
    #[serde(default)]
    settings: String,
    /// The lessons it was chosen, in selection order.
    lessons: Vec<LessonId>,
}

/// Record the lessons generation `generation` is chosen, unless already
/// recorded: a retried creation sends the lessons its first attempt chose.
pub(super) fn record_lessons(store: &mut Store, generation: u64) -> Result<(), ArchonError> {
    assert!(generation > 0, "generations start at 1");
    if read(store)?.is_some_and(|origin| origin.generation == generation) {
        return Ok(());
    }
    let origin = Origin {
        generation,
        archon_version: env!("CARGO_PKG_VERSION").to_owned(),
        prompt: String::new(),
        tools: tools_fingerprint(),
        settings: String::new(),
        lessons: chosen_lessons(store)?,
    };
    assert!(origin.lessons.len() <= archon_core::lesson::ORCHESTRATOR_LESSONS);
    store.kv_set(ORIGIN_KEY, &serde_json::to_string(&origin)?)
}

/// Record what Aether created generation `generation` with: `settings`, the
/// model and effort of the successful request, and `prompt`, the
/// part of `sent` before the lessons, unless `created` (the agent's
/// instructions, when Aether returns them) differs from `sent`, as when it
/// replays an earlier attempt's creation; then that text is recorded.
pub(super) fn record_created(
    store: &mut Store,
    generation: u64,
    prompt: &str,
    sent: &str,
    created: Option<&str>,
    settings: &str,
) -> Result<(), ArchonError> {
    assert!(!settings.is_empty(), "the orchestrator has a model");
    assert!(
        !prompt.trim().is_empty(),
        "the orchestrator has instructions"
    );
    assert!(sent.starts_with(prompt), "the lessons follow the prompt");
    let mut origin = read(store)?
        .filter(|origin| origin.generation == generation)
        .expect("record_lessons ran for this generation");
    let received = match created {
        Some(text) if text != sent => text,
        _ => prompt,
    };
    origin.archon_version = env!("CARGO_PKG_VERSION").to_owned();
    origin.prompt = fingerprint(received);
    origin.tools = tools_fingerprint();
    origin.settings = settings.to_owned();
    store.kv_set(ORIGIN_KEY, &serde_json::to_string(&origin)?)
}

/// How generation `generation` compares with one started now with any of
/// `instructions` (empty when they cannot be built) and `settings`; `None`
/// when its creation was not recorded.
pub(super) fn freshness(
    store: &Store,
    generation: u64,
    instructions: &[String],
    settings: &str,
) -> Result<Option<OrchestratorFreshness>, ArchonError> {
    let Some(origin) = read(store)?.filter(|origin| origin.generation == generation) else {
        return Ok(None);
    };
    assert!(
        !origin.archon_version.is_empty(),
        "recorded by a versioned archond"
    );
    let new_lessons = chosen_lessons(store)?
        .iter()
        .filter(|id| !origin.lessons.contains(id))
        .count();
    assert!(new_lessons <= archon_core::lesson::ORCHESTRATOR_LESSONS);
    assert!(!settings.is_empty(), "the orchestrator has a model");
    let settings_drift =
        (!origin.settings.is_empty() && origin.settings != settings).then(|| SettingsDrift {
            configured: settings.to_owned(),
            running: origin.settings.clone(),
        });
    Ok(Some(OrchestratorFreshness {
        created_by: origin.archon_version,
        prompt_outdated: (!instructions.is_empty()).then(|| {
            instructions
                .iter()
                .all(|text| fingerprint(text) != origin.prompt)
        }),
        tools_outdated: tools_fingerprint() != origin.tools,
        new_lessons: u32::try_from(new_lessons).expect("at most ORCHESTRATOR_LESSONS"),
        settings_drift,
    }))
}

fn read(store: &Store) -> Result<Option<Origin>, ArchonError> {
    let Some(raw) = store.kv_get(ORIGIN_KEY)?.filter(|raw| !raw.is_empty()) else {
        return Ok(None);
    };
    serde_json::from_str(&raw)
        .map(Some)
        .map_err(|e| ArchonError::Store(format!("kv {ORIGIN_KEY}: {e}")))
}

/// The lessons a generation created now would be given.
fn chosen_lessons(store: &Store) -> Result<Vec<LessonId>, ArchonError> {
    let lessons = store.orchestrator_lessons()?;
    assert!(lessons.len() <= archon_core::lesson::ORCHESTRATOR_LESSONS);
    Ok(lessons.into_iter().map(|lesson| lesson.id).collect())
}

/// Archon's MCP tools and the ones the orchestrator is not shown.
fn tools_fingerprint() -> String {
    let mut listing = String::new();
    for tool in TOOLS {
        listing.push_str(tool.name);
        listing.push(':');
        listing.push_str(tool.class.aether_name());
        listing.push('\n');
    }
    for hidden in ORCHESTRATOR_HIDDEN_TOOLS {
        listing.push_str("hidden:");
        listing.push_str(hidden);
        listing.push('\n');
    }
    fingerprint(&listing)
}

/// 64-bit FNV-1a in hex: stable across builds and toolchains, unlike std's
/// hasher, and only ever compared for equality.
fn fingerprint(text: &str) -> String {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let hash = text.bytes().fold(OFFSET, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(PRIME)
    });
    format!("{hash:016x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fingerprints_are_stable_and_tell_texts_apart() {
        assert_eq!(fingerprint(""), "cbf29ce484222325");
        assert_eq!(fingerprint("a"), "af63dc4c8601ec8c");
        assert_ne!(fingerprint("prompt v1"), fingerprint("prompt v2"));
    }
}
