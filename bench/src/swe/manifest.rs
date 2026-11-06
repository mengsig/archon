//! The dev manifest of Aether's SWE set, read in place from an Aether
//! checkout (`bench/release-set/swe-v1/`): archon-bench keeps no copy. It
//! needs only the instance count, for the estimate, and the pinned dataset
//! revision, which the run prints.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, ensure};
use serde::Deserialize;

use super::{sealed, stats};

/// The set's name, as the runner's `--set` takes it and its aggregate records it.
pub const SET_NAME: &str = "swe-v1";

/// The set's directory inside an Aether checkout.
pub const SET_DIR: &str = "bench/release-set/swe-v1";

/// The set format of Aether's SWE manifests.
pub const FORMAT: &str = "swe/1";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DevManifest {
    pub instances: u32,
    pub dataset: String,
    pub revision: String,
}

#[derive(Debug, Deserialize)]
struct Wire {
    format: String,
    splits: Splits,
}

/// Only the dev split is read; serde skips whatever else the manifest records.
#[derive(Debug, Deserialize)]
struct Splits {
    dev: Dev,
}

#[derive(Debug, Deserialize)]
struct Dev {
    dataset: String,
    revision: String,
    /// Instance id to the SHA-256 that pins its record.
    instances: BTreeMap<String, String>,
}

impl DevManifest {
    pub fn load(aether_checkout: &Path) -> anyhow::Result<Self> {
        sealed::refuse_inside_store(aether_checkout)?;
        let path = aether_checkout.join(SET_DIR).join("manifest.json");
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("reading the SWE dev manifest {}", path.display()))?;
        Self::parse(&text).with_context(|| format!("parsing {}", path.display()))
    }

    fn parse(text: &str) -> anyhow::Result<Self> {
        let wire: Wire = serde_json::from_str(text)?;
        ensure!(
            wire.format == FORMAT,
            "the manifest is not the {FORMAT} format"
        );
        let dev = wire.splits.dev;
        ensure!(
            !dev.dataset.trim().is_empty(),
            "the dev split names no dataset"
        );
        ensure!(
            !dev.revision.trim().is_empty(),
            "the dataset revision is not pinned"
        );
        ensure!(
            (1..=stats::MAX_PAIRS as usize).contains(&dev.instances.len()),
            "the dev manifest lists {} instances, not 1 to {}",
            dev.instances.len(),
            stats::MAX_PAIRS
        );
        let pinned =
            |hash: &String| hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit());
        ensure!(
            dev.instances.values().all(pinned),
            "a dev instance has no SHA-256 pin"
        );
        Ok(Self {
            instances: u32::try_from(dev.instances.len()).expect("bounded by MAX_PAIRS above"),
            dataset: dev.dataset,
            revision: dev.revision,
        })
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn manifest() -> serde_json::Value {
        let pin = "a".repeat(64);
        json!({"format": "swe/1", "splits": {"dev": {
            "source": "verified", "dataset": "SWE-bench/SWE-bench_Verified", "revision": "78f471bf",
            "instances": {"a__b-1": pin, "c__d-2": pin}}}})
    }

    #[test]
    fn a_dev_manifest_gives_its_count_dataset_and_revision() {
        let parsed = DevManifest::parse(&manifest().to_string()).unwrap();
        assert_eq!(parsed.instances, 2);
        assert_eq!(Path::new(SET_DIR).file_name().unwrap(), SET_NAME);
        assert_eq!(
            (parsed.dataset.as_str(), parsed.revision.as_str()),
            ("SWE-bench/SWE-bench_Verified", "78f471bf")
        );
    }

    #[test]
    fn unpinned_empty_or_foreign_manifests_are_refused() {
        let mut value = manifest();
        value["splits"]["dev"]["revision"] = json!(" ");
        assert!(
            DevManifest::parse(&value.to_string()).is_err(),
            "unpinned revision"
        );
        let mut value = manifest();
        value["splits"]["dev"]["instances"]["a__b-1"] = json!("short");
        assert!(
            DevManifest::parse(&value.to_string()).is_err(),
            "unpinned instance"
        );
        let mut value = manifest();
        value["splits"]["dev"]["instances"] = json!({});
        assert!(
            DevManifest::parse(&value.to_string()).is_err(),
            "no instance"
        );
        let mut value = manifest();
        value["format"] = json!("release-set/2");
        assert!(
            DevManifest::parse(&value.to_string()).is_err(),
            "another format"
        );
    }
}
