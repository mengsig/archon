//! The SWE family's arms: per model, the best recent Archon release (`old`),
//! the candidate (`new`), each on its own Aether, and the native CLI.

use std::fmt;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use anyhow::{Context, bail, ensure};
use serde_json::{Value, json};

/// The Archon pipeline both Archon arms run.
pub const PIPELINE: &str = "direct";

/// The two cheap models every live bench run uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Model {
    Haiku,
    Luna,
}

impl Model {
    pub const ALL: [Model; 2] = [Model::Haiku, Model::Luna];

    pub fn name(self) -> &'static str {
        match self {
            Model::Haiku => "haiku",
            Model::Luna => "luna",
        }
    }

    /// The native harness the gate holds Archon against on this model.
    pub fn native_cli(self) -> &'static str {
        match self {
            Model::Haiku => "claude-code",
            Model::Luna => "codex",
        }
    }
}

impl serde::Serialize for Model {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.name())
    }
}

/// Which harness drives an arm.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Harness {
    /// The best recent Archon release on its released Aether.
    Old,
    /// The candidate Archon on its Aether.
    New,
    /// claude-code for Haiku, codex for Luna.
    Native,
}

impl Harness {
    pub const ALL: [Harness; 3] = [Harness::Old, Harness::New, Harness::Native];

    pub fn name(self) -> &'static str {
        match self {
            Harness::Old => "old",
            Harness::New => "new",
            Harness::Native => "native",
        }
    }
}

/// One arm: a harness on a model, keyed `model/harness` as in Aether's runner.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ArmKey {
    pub model: Model,
    pub harness: Harness,
}

impl fmt::Display for ArmKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.model.name(), self.harness.name())
    }
}

impl FromStr for ArmKey {
    type Err = anyhow::Error;

    /// Error messages never quote the input: holdout files are parsed with it.
    fn from_str(text: &str) -> anyhow::Result<Self> {
        let (model, harness) = text
            .split_once('/')
            .context("an arm key is model/harness")?;
        let model = Model::ALL
            .into_iter()
            .find(|m| m.name() == model)
            .context("unknown model in an arm key")?;
        let harness = Harness::ALL
            .into_iter()
            .find(|h| h.name() == harness)
            .context("unknown harness in an arm key")?;
        Ok(ArmKey { model, harness })
    }
}

/// An Archon build and the Aether it runs on, both as executables on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArchonBuild {
    /// What the report names it: a release tag such as `v1.4.9`, or a commit.
    pub label: String,
    pub archon_bin: PathBuf,
    pub aether_bin: PathBuf,
}

impl ArchonBuild {
    pub fn new(label: &str, archon_bin: &Path, aether_bin: &Path) -> anyhow::Result<Self> {
        ensure!(!label.trim().is_empty(), "an Archon build needs a label");
        for (what, path) in [("archon", archon_bin), ("aether", aether_bin)] {
            ensure!(
                path.is_absolute(),
                "the {what} binary {} is not absolute",
                path.display()
            );
            ensure!(
                path.is_file(),
                "the {what} binary {} is not a file",
                path.display()
            );
        }
        Ok(Self {
            label: label.to_owned(),
            archon_bin: archon_bin.to_owned(),
            aether_bin: aether_bin.to_owned(),
        })
    }
}

/// Every arm of a run, with the two Archon builds the Archon arms use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArmSet {
    pub old: ArchonBuild,
    pub new: ArchonBuild,
    pub models: Vec<Model>,
}

impl ArmSet {
    pub fn new(old: ArchonBuild, new: ArchonBuild, models: Vec<Model>) -> anyhow::Result<Self> {
        ensure!(!models.is_empty(), "an SWE run needs at least one model");
        ensure!(old.label != new.label, "old and new are both {}", old.label);
        if old.archon_bin == new.archon_bin && old.aether_bin == new.aether_bin {
            bail!("old and new run the same archon and aether binaries");
        }
        let mut models = models;
        models.sort();
        models.dedup();
        Ok(Self { old, new, models })
    }

    /// The arms in report order: per model, old, new and native.
    pub fn keys(&self) -> Vec<ArmKey> {
        let keys: Vec<ArmKey> = self
            .models
            .iter()
            .flat_map(|&model| Harness::ALL.map(|harness| ArmKey { model, harness }))
            .collect();
        assert_eq!(
            keys.len(),
            self.models.len() * Harness::ALL.len(),
            "three arms per model"
        );
        keys
    }

    pub fn build(&self, harness: Harness) -> Option<&ArchonBuild> {
        match harness {
            Harness::Old => Some(&self.old),
            Harness::New => Some(&self.new),
            Harness::Native => None,
        }
    }

    /// The arms file handed to Aether's runner (see `runner`).
    pub fn runner_arms(&self) -> Value {
        let arms: Vec<Value> = self
            .keys()
            .into_iter()
            .map(|key| match self.build(key.harness) {
                Some(build) => json!({
                    "key": key.to_string(), "model": key.model.name(), "harness": "archon",
                    "pipeline": PIPELINE, "label": build.label, "archon_bin": build.archon_bin,
                    "aether_bin": build.aether_bin,
                }),
                None => json!({
                    "key": key.to_string(), "model": key.model.name(), "harness": "native",
                    "cli": key.model.native_cli(),
                }),
            })
            .collect();
        json!({ "arms": arms })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build(dir: &Path, label: &str) -> ArchonBuild {
        let archon = dir.join(format!("archon-{label}"));
        let aether = dir.join(format!("aether-{label}"));
        std::fs::write(&archon, "").unwrap();
        std::fs::write(&aether, "").unwrap();
        ArchonBuild::new(label, &archon, &aether).unwrap()
    }

    #[test]
    fn keys_round_trip_and_reject_unknown_parts_without_quoting_them() {
        for model in Model::ALL {
            for harness in Harness::ALL {
                let key = ArmKey { model, harness };
                assert_eq!(key.to_string().parse::<ArmKey>().unwrap(), key);
            }
        }
        for bad in ["haiku", "opus/new", "haiku/archon", "sealed-instance-7/old"] {
            let error = bad.parse::<ArmKey>().unwrap_err().to_string();
            assert!(!error.contains(bad), "the error quotes its input: {error}");
        }
    }

    #[test]
    fn arm_set_lists_three_arms_per_model_and_pairs_each_archon_arm_with_its_aether() {
        let dir = tempfile::tempdir().unwrap();
        let set = ArmSet::new(
            build(dir.path(), "v1.4.9"),
            build(dir.path(), "head"),
            vec![Model::Luna, Model::Haiku, Model::Luna],
        )
        .unwrap();
        let keys: Vec<String> = set.keys().iter().map(ToString::to_string).collect();
        assert_eq!(
            keys,
            [
                "haiku/old",
                "haiku/new",
                "haiku/native",
                "luna/old",
                "luna/new",
                "luna/native"
            ]
        );
        let arms = set.runner_arms();
        let arms = arms["arms"].as_array().unwrap();
        assert_eq!(arms[0]["label"], "v1.4.9");
        assert!(
            arms[0]["aether_bin"]
                .as_str()
                .unwrap()
                .ends_with("aether-v1.4.9")
        );
        assert_eq!(arms[4]["label"], "head");
        assert_eq!(
            (arms[0]["pipeline"].as_str(), arms[4]["pipeline"].as_str()),
            (Some("direct"), Some("direct"))
        );
        assert!(
            arms[2].get("pipeline").is_none(),
            "a native arm runs no pipeline"
        );
        assert_eq!(arms[2]["cli"], "claude-code");
        assert_eq!(arms[5]["cli"], "codex");
    }

    #[test]
    fn arm_set_refuses_identical_builds_and_missing_binaries() {
        let dir = tempfile::tempdir().unwrap();
        let old = build(dir.path(), "v1.4.9");
        let mut same = old.clone();
        same.label = "head".into();
        assert!(ArmSet::new(old.clone(), same, vec![Model::Haiku]).is_err());
        assert!(ArmSet::new(old.clone(), old, vec![Model::Haiku]).is_err());
        let missing = dir.path().join("missing");
        assert!(ArchonBuild::new("head", &missing, &missing).is_err());
        assert!(ArchonBuild::new("head", Path::new("archon"), Path::new("aether")).is_err());
    }
}
