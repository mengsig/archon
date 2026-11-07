//! The old arm: the best recent Archon release, picked from the release-check
//! records by pass rate and then cost, and its released assets checked
//! against the release's `SHA256SUMS` before they run.

use std::path::Path;

use anyhow::{Context, bail, ensure};
use serde::Deserialize;
use sha2::{Digest, Sha256};

/// One model's measurement of a release in a release-check record.
#[derive(Debug, Clone, Copy, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Measured {
    pub passed: u32,
    pub trials: u32,
    pub usd_per_run: f64,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Record {
    pub archon: String,
    pub aether: String,
    pub source: String,
    pub haiku: Option<Measured>,
    pub luna: Option<Measured>,
}

/// `bench/results-swe/release-records.json`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Records {
    pub current: String,
    pub window: String,
    pub measure: String,
    pub records: Vec<Record>,
}

/// A release measured on both models, with its pooled figures.
#[derive(Debug, Clone, PartialEq)]
pub struct Ranked<'a> {
    pub record: &'a Record,
    pub passed: u32,
    pub trials: u32,
    pub usd_per_run: f64,
}

impl Ranked<'_> {
    fn rate(&self) -> f64 {
        f64::from(self.passed) / f64::from(self.trials)
    }
}

impl Records {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading release records {}", path.display()))?;
        let records: Self = serde_json::from_str(&text)
            .with_context(|| format!("parsing release records {}", path.display()))?;
        records.validate()?;
        Ok(records)
    }

    fn validate(&self) -> anyhow::Result<()> {
        ensure!(!self.records.is_empty(), "no release records");
        ensure!(
            self.records.iter().any(|r| r.archon == self.current),
            "the current release {} has no record",
            self.current
        );
        for record in &self.records {
            for measured in [record.haiku, record.luna].into_iter().flatten() {
                ensure!(
                    measured.trials > 0 && measured.passed <= measured.trials,
                    "{}: invalid pass count",
                    record.archon
                );
                ensure!(
                    measured.usd_per_run.is_finite() && measured.usd_per_run > 0.0,
                    "{}: invalid cost",
                    record.archon
                );
            }
        }
        Ok(())
    }

    /// Releases measured on both models, best first: by pooled pass rate,
    /// then by summed cost per run, then the current release first. An older
    /// release wins only by passing more, or as many for less.
    pub fn ranked(&self) -> Vec<Ranked<'_>> {
        let mut ranked: Vec<Ranked<'_>> = self
            .records
            .iter()
            .filter_map(|record| {
                let (h, l) = (record.haiku?, record.luna?);
                Some(Ranked {
                    record,
                    passed: h.passed + l.passed,
                    trials: h.trials + l.trials,
                    usd_per_run: h.usd_per_run + l.usd_per_run,
                })
            })
            .collect();
        ranked.sort_by(|a, b| {
            b.rate()
                .total_cmp(&a.rate())
                .then(a.usd_per_run.total_cmp(&b.usd_per_run))
                .then((a.record.archon != self.current).cmp(&(b.record.archon != self.current)))
        });
        ranked
    }

    pub fn best(&self) -> anyhow::Result<Ranked<'_>> {
        self.ranked()
            .into_iter()
            .next()
            .context("no release is measured on both models")
    }
}

/// The lowercase hex SHA-256 of a file.
pub fn sha256_hex(path: &Path) -> anyhow::Result<String> {
    let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let digest = Sha256::digest(&bytes);
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    assert_eq!(hex.len(), 64, "a SHA-256 digest is 64 hex digits");
    Ok(hex)
}

/// Check `asset` against its line in a release's `SHA256SUMS`; returns its digest.
pub fn verify_asset(sums: &Path, asset: &Path) -> anyhow::Result<String> {
    let name = asset
        .file_name()
        .and_then(|n| n.to_str())
        .context("the asset has no file name")?;
    let text =
        std::fs::read_to_string(sums).with_context(|| format!("reading {}", sums.display()))?;
    let listed: Vec<&str> = text
        .lines()
        .filter_map(|line| {
            let (digest, file) = line.split_once(char::is_whitespace)?;
            (file.trim_start().trim_start_matches('*') == name).then_some(digest)
        })
        .collect();
    let expected = match listed.as_slice() {
        [digest] => digest.to_ascii_lowercase(),
        [] => bail!("{name} is not listed in {}", sums.display()),
        _ => bail!("{name} is listed more than once in {}", sums.display()),
    };
    ensure!(
        expected.len() == 64 && expected.bytes().all(|b| b.is_ascii_hexdigit()),
        "{name}'s listed digest is not a SHA-256"
    );
    let actual = sha256_hex(asset)?;
    ensure!(
        actual == expected,
        "{name} does not match SHA256SUMS: {actual} != {expected}"
    );
    Ok(actual)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn committed() -> Records {
        Records::load(
            &Path::new(env!("CARGO_MANIFEST_DIR")).join("results-swe/release-records.json"),
        )
        .unwrap()
    }

    #[test]
    fn the_committed_records_pick_v1_4_9_on_its_released_aether() {
        let records = committed();
        let best = records.best().unwrap();
        assert_eq!(
            (best.record.archon.as_str(), best.record.aether.as_str()),
            ("v1.4.9", "v1.2.8")
        );
        assert_eq!((best.passed, best.trials), (16, 16));
        let order: Vec<&str> = records
            .ranked()
            .iter()
            .map(|r| r.record.archon.as_str())
            .collect();
        assert_eq!(
            order,
            ["v1.4.9", "v1.4.11", "v1.4.12"],
            "v1.4.10 lacks Luna"
        );
    }

    #[test]
    fn a_tie_keeps_the_current_release_and_cost_breaks_equal_rates() {
        let mut records = committed();
        let v1_4_9 = records
            .records
            .iter_mut()
            .find(|r| r.archon == "v1.4.9")
            .unwrap();
        v1_4_9.luna = Some(Measured {
            passed: 7,
            trials: 8,
            usd_per_run: 0.0493,
        });
        v1_4_9.haiku = Some(Measured {
            passed: 8,
            trials: 8,
            usd_per_run: 0.1225,
        });
        assert_eq!(
            records.best().unwrap().record.archon,
            "v1.4.11",
            "cheapest at 15/16"
        );
        let v1_4_11 = records
            .records
            .iter_mut()
            .find(|r| r.archon == "v1.4.11")
            .unwrap();
        v1_4_11.haiku.as_mut().unwrap().usd_per_run = 0.1252;
        v1_4_11.luna.as_mut().unwrap().usd_per_run = 0.0493;
        let v1_4_12 = records
            .records
            .iter_mut()
            .find(|r| r.archon == "v1.4.12")
            .unwrap();
        v1_4_12.luna.as_mut().unwrap().usd_per_run = 0.0493;
        v1_4_12.haiku.as_mut().unwrap().usd_per_run = 0.1225;
        assert_eq!(
            records.best().unwrap().record.archon,
            "v1.4.12",
            "the current release wins ties"
        );
    }

    #[test]
    fn assets_are_checked_against_their_sums_line() {
        let dir = tempfile::tempdir().unwrap();
        let asset = dir.path().join("archon-x86_64-unknown-linux-musl.tar.gz");
        std::fs::write(&asset, b"release bytes").unwrap();
        let digest = sha256_hex(&asset).unwrap();
        let sums = dir.path().join("SHA256SUMS");
        std::fs::write(
            &sums,
            format!("{digest}  archon-x86_64-unknown-linux-musl.tar.gz\n"),
        )
        .unwrap();
        assert_eq!(verify_asset(&sums, &asset).unwrap(), digest);
        std::fs::write(&asset, b"tampered").unwrap();
        assert!(
            verify_asset(&sums, &asset)
                .unwrap_err()
                .to_string()
                .contains("does not match")
        );
        std::fs::write(&sums, format!("{digest} *other.tar.gz\n")).unwrap();
        assert!(
            verify_asset(&sums, &asset)
                .unwrap_err()
                .to_string()
                .contains("not listed")
        );
    }
}
