//! Whether the installed `aether` serves a protocol an archon speaks: the
//! ranges of `archon update --requirements` and of `aether schema`.

use std::ffi::OsStr;
use std::path::Path;
use std::time::Duration;

use anyhow::Context as _;
use archon_aether::wire::{MIN_PROTOCOL_VERSION, PROTOCOL_VERSION};
use serde::{Deserialize, Serialize};

use super::super::doctor::find_program;
use super::tools;
use crate::instance::AETHER_PROGRAM;

const PROBE_TIMEOUT: Duration = Duration::from_secs(20);
const MIN_KEY: &str = "aether_protocol_min";
const MAX_KEY: &str = "aether_protocol_max";

/// The Aether protocols an archon speaks, oldest to newest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Requirement {
    pub min: u32,
    pub max: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub minimum_version: Option<String>,
}

impl Requirement {
    /// What this binary speaks.
    pub fn own() -> Self {
        Self {
            min: MIN_PROTOCOL_VERSION,
            max: PROTOCOL_VERSION,
            minimum_version: Some(crate::cmd::REQUIRED_AETHER_VERSION.to_owned()),
        }
    }

    /// The lines `archon update --requirements` prints; `install.sh` reads them.
    pub fn render(&self) -> String {
        format!("{MIN_KEY} {}\n{MAX_KEY} {}", self.min, self.max)
    }

    /// Ask the archon at `archon` what it speaks.
    pub async fn of(archon: &Path) -> anyhow::Result<Self> {
        let args = [
            OsStr::new("--json"),
            OsStr::new("update"),
            OsStr::new("--requirements"),
        ];
        let output = tools::run(archon, &args, PROBE_TIMEOUT).await?;
        let requirement: Self = serde_json::from_slice(&output.stdout)
            .with_context(|| format!("reading what {} needs", archon.display()))?;
        anyhow::ensure!(
            requirement.min <= requirement.max,
            "the protocol range is empty"
        );
        anyhow::ensure!(
            requirement.minimum_version.is_some(),
            "the target Archon did not report its minimum Aether release"
        );
        Ok(requirement)
    }
}

/// The protocols an installed aether serves, and its version line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Served {
    pub version: String,
    pub min: u32,
    pub max: u32,
}

#[derive(Deserialize)]
struct Schema {
    min_protocol_version: u32,
    protocol_version: u32,
}

/// What the `aether` on PATH serves; `None` when there is none.
pub async fn installed() -> anyhow::Result<Option<Served>> {
    let Some(aether) = find_program(Path::new(AETHER_PROGRAM)) else {
        return Ok(None);
    };
    let version = tools::run(&aether, &[OsStr::new("--version")], PROBE_TIMEOUT).await?;
    let schema = tools::run(&aether, &[OsStr::new("schema")], PROBE_TIMEOUT).await?;
    let schema: Schema = serde_json::from_slice(&schema.stdout)
        .with_context(|| format!("reading `{} schema`", aether.display()))?;
    anyhow::ensure!(
        schema.min_protocol_version <= schema.protocol_version,
        "`{} schema` names an empty protocol range",
        aether.display()
    );
    Ok(Some(Served {
        version: String::from_utf8_lossy(&version.stdout).trim().to_owned(),
        min: schema.min_protocol_version,
        max: schema.protocol_version,
    }))
}

/// Whether `served` and `needed` share a protocol, and one line saying so
/// for `archon` (a name and version), with the command to run when not.
pub fn verdict(archon: &str, needed: Requirement, served: Option<&Served>) -> (bool, String) {
    assert!(needed.min <= needed.max, "a requirement is a range");
    let needs = format!("{archon} needs protocol {} to {}", needed.min, needed.max);
    let Some(served) = served else {
        let line = format!("Aether: no `aether` on PATH; {needs}. Install Aether before use.");
        return (false, line);
    };
    if let Some(problem) =
        version_problem(archon, needed.minimum_version.as_deref(), &served.version)
    {
        return (false, problem);
    }
    let speaks = format!(
        "{} speaks protocol {} to {}",
        served.version, served.min, served.max
    );
    if served.max < needed.min {
        let line = format!("Aether: {speaks}, too old: {needs}. Run `aether update`.");
        return (false, line);
    }
    if served.min > needed.max {
        let line = format!("Aether: {speaks}, newer than {archon} supports: {needs}.");
        return (false, line);
    }
    (true, format!("Aether: {speaks}; {needs}: compatible."))
}

fn version_problem(archon: &str, minimum: Option<&str>, version_line: &str) -> Option<String> {
    let Some(minimum) = minimum else {
        return Some(format!(
            "Aether: {archon} did not report its minimum Aether release; run `archon doctor` after updating."
        ));
    };
    let minimum = match minimum.parse::<super::version::Version>() {
        Ok(version) => version,
        Err(_) => {
            return Some(format!(
                "Aether: {archon} reported an invalid minimum release {minimum}; run `archon doctor`."
            ));
        }
    };
    let installed = version_line
        .split_whitespace()
        .nth(1)
        .and_then(|v| v.parse::<super::version::Version>().ok());
    match installed {
        Some(version) if version < minimum => {
            return Some(format!(
                "Aether: {} is below the {minimum} required by {archon}; run `aether update --version v{minimum}`.",
                version_line
            ));
        }
        None => {
            return Some(format!(
                "Aether: cannot compare {} with required {minimum}; run `archon doctor`.",
                version_line
            ));
        }
        _ => {}
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn served(min: u32, max: u32) -> Served {
        Served {
            version: "aether 0.9.0".to_owned(),
            min,
            max,
        }
    }

    #[test]
    fn requirements_include_the_minimum_in_json_and_keep_the_text_format() {
        let own = Requirement::own();
        let encoded = serde_json::to_string(&own).unwrap();
        let decoded: Requirement = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded, own);
        assert_eq!(
            own.minimum_version.as_deref(),
            Some(crate::cmd::REQUIRED_AETHER_VERSION)
        );
        assert_eq!(
            own.render(),
            format!("{MIN_KEY} {}\n{MAX_KEY} {}", own.min, own.max)
        );
    }

    #[test]
    fn minimum_release_is_checked_even_when_protocols_overlap() {
        let needed = Requirement::own();
        let mut aether = Served {
            version: "aether 1.4.3".into(),
            min: 17,
            max: 26,
        };
        let (ok, line) = verdict("archon 1.6.5", needed.clone(), Some(&aether));
        assert!(
            !ok && line.contains("1.4.3") && line.contains("1.4.4"),
            "{line}"
        );
        assert!(line.contains("aether update"));
        aether.version = "aether 1.4.4".into();
        assert!(verdict("archon 1.6.5", needed, Some(&aether)).0);
    }

    #[test]
    fn an_aether_is_compatible_when_the_ranges_overlap() {
        let needed = Requirement {
            min: 14,
            max: 17,
            minimum_version: Some("0.9.0".into()),
        };
        for (min, max) in [(14, 17), (16, 17), (12, 14), (17, 19)] {
            let (ok, line) = verdict("archon 2.0.0", needed.clone(), Some(&served(min, max)));
            assert!(ok && line.ends_with("compatible."), "{min}..{max}: {line}");
        }
        let (ok, line) = verdict("archon 2.0.0", needed.clone(), Some(&served(12, 13)));
        assert!(!ok && line.ends_with("Run `aether update`."), "{line}");
        assert!(
            line.contains("aether 0.9.0 speaks protocol 12 to 13"),
            "{line}"
        );
        assert!(
            line.contains("archon 2.0.0 needs protocol 14 to 17"),
            "{line}"
        );
        let (ok, line) = verdict("archon 2.0.0", needed.clone(), Some(&served(18, 19)));
        assert!(
            !ok && line.contains("newer than archon 2.0.0 supports"),
            "{line}"
        );
        assert!(!line.contains("aether update"), "{line}");
        let (ok, line) = verdict("archon 2.0.0", needed.clone(), None);
        assert!(!ok && line.contains("no `aether` on PATH"), "{line}");
    }
}
