//! The one adapter to Aether's SWE runner (aether `evals/release_bench.py
//! run --set swe-v1`). archon-bench never prepares, runs or grades an SWE
//! instance itself: the runner owns the set, the image cache, the trials and
//! the grading, for both splits. This module hands it the arms and reads back
//! its aggregate file (`output`).
//!
//! On the holdout split the runner's output is discarded unread and the
//! scratch it ran in deleted, so archon-bench neither prints nor keeps
//! anything from the sealed set but the validated aggregate.

pub mod output;

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};

use anyhow::{Context, anyhow, bail, ensure};

use super::aggregate::{Aggregate, Split};
use super::arms::{ArchonBuild, ArmKey, ArmSet, Harness};
use super::{manifest, sealed};
use crate::workspace;

/// The runner script inside an Aether checkout.
pub const RUNNER_SCRIPT: &str = "evals/release_bench.py";

/// Where, inside the checkout, the runner writes a dev run (by `--run-id`)
/// and each holdout run (under a name it picks).
const DEV_RUNS: &str = "target/release-bench/runs";
const HOLDOUT_RUNS: &str = "target/release-bench/holdout";
const AGGREGATE_FILE: &str = "aggregate.json";

/// The most repeats and parallel jobs a run accepts: well above this host's
/// 3 jobs, and low enough that every count in the estimate fits in a u32.
pub const MAX_REPEATS: u32 = 10;
pub const MAX_JOBS: u32 = 32;

/// The runner's name for an arm: `haiku/native`, `haiku/archon-old`, ...
pub fn runner_arm(key: ArmKey) -> String {
    match key.harness {
        Harness::Native => format!("{}/native", key.model.name()),
        Harness::Old | Harness::New => {
            format!("{}/archon-{}", key.model.name(), key.harness.name())
        }
    }
}

/// How to start Aether's runner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Runner {
    pub python: PathBuf,
    pub checkout: PathBuf,
}

/// What one runner invocation runs.
#[derive(Debug, Clone, PartialEq)]
pub struct Request<'a> {
    pub split: Split,
    pub arms: &'a ArmSet,
    pub repeats: u32,
    pub jobs: u32,
    pub load_max: f64,
    /// The runner admits no trial once the run has spent this much.
    pub budget_usd: f64,
    /// Dev only: the first N instances, for a smoke of the wiring.
    pub limit: Option<u32>,
}

impl Request<'_> {
    pub fn validate(&self) -> anyhow::Result<()> {
        ensure!(
            (1..=MAX_REPEATS).contains(&self.repeats),
            "--repeats must be between 1 and {MAX_REPEATS}"
        );
        ensure!(
            (1..=MAX_JOBS).contains(&self.jobs),
            "--jobs must be between 1 and {MAX_JOBS}"
        );
        for (flag, value) in [
            ("--load-max", self.load_max),
            ("--budget-usd", self.budget_usd),
        ] {
            ensure!(value.is_finite() && value > 0.0, "{flag} must be positive");
        }
        ensure!(self.limit != Some(0), "--limit must be positive");
        ensure!(
            self.limit.is_none() || self.split == Split::Dev,
            "--limit applies to the dev split only: the holdout runs whole"
        );
        Ok(())
    }
}

/// `LABEL=ARCHON,AETHER` for the runner's `--archon-pair`.
fn archon_pair(harness: Harness, build: &ArchonBuild) -> OsString {
    let mut pair = OsString::from(format!("{}=", harness.name()));
    pair.push(&build.archon_bin);
    pair.push(",");
    pair.push(&build.aether_bin);
    pair
}

impl Runner {
    pub fn in_checkout(python: &Path, aether_checkout: &Path) -> anyhow::Result<Self> {
        sealed::refuse_inside_store(aether_checkout)?;
        let checkout = std::path::absolute(aether_checkout)
            .with_context(|| format!("resolving {}", aether_checkout.display()))?;
        let script = checkout.join(RUNNER_SCRIPT);
        ensure!(script.is_file(), "no SWE runner at {}", script.display());
        Ok(Self {
            python: python.to_owned(),
            checkout,
        })
    }

    /// The runner's arguments for `request`, with workspaces under `work_root`
    /// and, on dev, the run named `run_id`.
    pub fn args(
        &self,
        request: &Request<'_>,
        work_root: &Path,
        run_id: Option<&str>,
    ) -> Vec<OsString> {
        assert!(
            work_root.is_absolute(),
            "the runner's work root is absolute"
        );
        assert_eq!(
            run_id.is_some(),
            request.split == Split::Dev,
            "only a dev run is named; the holdout names its own"
        );
        let arms: Vec<String> = request.arms.keys().into_iter().map(runner_arm).collect();
        let (old, new) = (&request.arms.old, &request.arms.new);
        let flags: [(&str, OsString); 12] = [
            ("--set", manifest::SET_NAME.into()),
            ("--split", request.split.to_string().into()),
            ("--old", old.aether_bin.clone().into()),
            ("--new", new.aether_bin.clone().into()),
            ("--arms", arms.join(",").into()),
            ("--archon-pair", archon_pair(Harness::Old, old)),
            ("--archon-pair", archon_pair(Harness::New, new)),
            ("--repeats", request.repeats.to_string().into()),
            ("--jobs", request.jobs.to_string().into()),
            ("--load-max", request.load_max.to_string().into()),
            ("--budget", request.budget_usd.to_string().into()),
            ("--work-root", work_root.into()),
        ];
        let mut args: Vec<OsString> = vec![self.checkout.join(RUNNER_SCRIPT).into(), "run".into()];
        for (flag, value) in flags {
            args.extend([flag.into(), value]);
        }
        if let Some(limit) = request.limit {
            args.extend(["--limit".into(), limit.to_string().into()]);
        }
        if let Some(run_id) = run_id {
            args.extend(["--run-id".into(), run_id.into()]);
        }
        args
    }

    /// Run the runner on `request` with its workspaces in a scratch directory
    /// under `work_dir`, deleted afterwards whatever happens.
    pub fn run(&self, request: &Request<'_>, work_dir: &Path) -> anyhow::Result<Aggregate> {
        request.validate()?;
        let scratch = sealed::private_scratch(work_dir)?;
        let outcome = self.run_in(request, &scratch);
        let removed = std::fs::remove_dir_all(&scratch)
            .with_context(|| format!("removing the runner scratch {}", scratch.display()));
        match (outcome, removed) {
            (Ok(aggregate), Ok(())) => {
                assert!(!scratch.exists(), "the runner scratch is gone");
                Ok(aggregate)
            }
            (Err(run), Ok(())) => Err(run),
            (Ok(_), Err(removal)) => Err(removal),
            (Err(run), Err(removal)) => Err(run.context(format!("and {removal:#}"))),
        }
    }

    fn run_in(&self, request: &Request<'_>, scratch: &Path) -> anyhow::Result<Aggregate> {
        let run_id = match request.split {
            Split::Dev => Some(format!("archon-bench-{}", workspace::unguessable_name()?)),
            Split::Holdout => None,
        };
        let holdout_before = self.holdout_runs()?;
        let mut command = Command::new(&self.python);
        command
            .args(self.args(request, scratch, run_id.as_deref()))
            .current_dir(&self.checkout)
            .stdin(Stdio::null());
        if request.split == Split::Holdout {
            command.stdout(Stdio::null()).stderr(Stdio::null());
        }
        let status = command
            .status()
            .with_context(|| format!("starting {} {RUNNER_SCRIPT}", self.python.display()))?;
        let path = match &run_id {
            Some(run_id) => self
                .checkout
                .join(DEV_RUNS)
                .join(run_id)
                .join(AGGREGATE_FILE),
            None => self.new_holdout_aggregate(&holdout_before, status)?,
        };
        if !path.is_file() {
            return Err(no_aggregate(request.split, status));
        }
        if !status.success() {
            // The runner exits nonzero on its own non-PASS verdict; archon-bench gates for itself.
            eprintln!("the SWE runner exited {status} after writing its aggregate");
        }
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("reading the runner's aggregate {}", path.display()))?;
        output::parse(
            &text,
            request.split,
            request.repeats,
            request.limit,
            &request.arms.keys(),
        )
    }

    /// The holdout run directories the runner has written so far.
    fn holdout_runs(&self) -> anyhow::Result<BTreeSet<OsString>> {
        let dir = self.checkout.join(HOLDOUT_RUNS);
        if !dir.exists() {
            return Ok(BTreeSet::new());
        }
        let listing = || format!("listing {}", dir.display());
        std::fs::read_dir(&dir)
            .with_context(listing)?
            .map(|entry| Ok(entry?.file_name()))
            .collect::<std::io::Result<_>>()
            .with_context(listing)
    }

    /// The aggregate of the one holdout run directory that appeared during this run.
    fn new_holdout_aggregate(
        &self,
        before: &BTreeSet<OsString>,
        status: ExitStatus,
    ) -> anyhow::Result<PathBuf> {
        let after = self.holdout_runs()?;
        let new: Vec<&OsString> = after.difference(before).collect();
        match new.as_slice() {
            [] => Err(no_aggregate(Split::Holdout, status)),
            [one] => Ok(self
                .checkout
                .join(HOLDOUT_RUNS)
                .join(one)
                .join(AGGREGATE_FILE)),
            more => bail!(
                "{} holdout runs appeared during this one; run the holdout alone",
                more.len()
            ),
        }
    }
}

/// The error for a runner that wrote no aggregate, without its output on the holdout.
fn no_aggregate(split: Split, status: ExitStatus) -> anyhow::Error {
    match split {
        Split::Dev => anyhow!("the SWE runner wrote no aggregate ({status}); its output is above"),
        Split::Holdout => anyhow!(
            "the SWE runner wrote no holdout aggregate ({status}); archon-bench discards its \
             output, so rerun it under the validation item to diagnose"
        ),
    }
}

#[cfg(test)]
mod tests;
