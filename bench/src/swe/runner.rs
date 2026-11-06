//! The one adapter to Aether's SWE runner command line. archon-bench never
//! prepares, runs or grades an SWE instance itself: Aether's runner owns the
//! set, the image cache, the trials and the grading, for both splits. This
//! module hands it the arms and reads back its aggregate file.
//!
//! On the holdout split the runner's output is discarded unread and its
//! scratch directory deleted, so archon-bench neither prints nor keeps
//! anything from the sealed set but the validated aggregate.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, bail, ensure};

use super::aggregate::{Aggregate, Split};
use super::arms::ArmSet;
use super::sealed;

/// The runner script inside an Aether checkout.
pub const RUNNER_SCRIPT: &str = "evals/swe_bench.py";

/// How to start Aether's runner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Runner {
    pub python: PathBuf,
    pub script: PathBuf,
}

/// What one runner invocation runs.
#[derive(Debug, Clone, PartialEq)]
pub struct Request<'a> {
    pub split: Split,
    pub arms: &'a ArmSet,
    pub repeats: u32,
    pub jobs: u32,
    pub load_max: f64,
}

impl Request<'_> {
    pub fn validate(&self) -> anyhow::Result<()> {
        ensure!(self.repeats >= 1, "an SWE run needs at least one repeat");
        ensure!(self.jobs >= 1, "an SWE run needs at least one job");
        ensure!(
            self.load_max.is_finite() && self.load_max > 0.0,
            "--load-max must be positive"
        );
        Ok(())
    }
}

impl Runner {
    pub fn in_checkout(python: &Path, aether_checkout: &Path) -> anyhow::Result<Self> {
        sealed::refuse_inside_store(aether_checkout)?;
        let script = aether_checkout.join(RUNNER_SCRIPT);
        ensure!(script.is_file(), "no SWE runner at {}", script.display());
        Ok(Self {
            python: python.to_owned(),
            script,
        })
    }

    /// The runner's arguments for `request`, writing its aggregate to `aggregate_out`.
    pub fn args(
        &self,
        request: &Request<'_>,
        arms_file: &Path,
        aggregate_out: &Path,
    ) -> Vec<OsString> {
        assert!(
            arms_file.is_absolute() && aggregate_out.is_absolute(),
            "runner paths are absolute"
        );
        let mut args: Vec<OsString> = vec![self.script.clone().into(), "run".into()];
        let pairs = [
            ("--split", request.split.to_string()),
            ("--repeats", request.repeats.to_string()),
            ("--jobs", request.jobs.to_string()),
            ("--load-max", request.load_max.to_string()),
        ];
        for (flag, value) in pairs {
            args.extend([flag.into(), value.into()]);
        }
        args.extend([
            "--arms".into(),
            arms_file.into(),
            "--aggregate-out".into(),
            aggregate_out.into(),
        ]);
        args
    }

    /// Run the runner on `request` with a scratch directory under `work_dir`,
    /// deleted afterwards whatever happens; returns the validated aggregate.
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
        let arms_file = scratch.join("arms.json");
        let aggregate_out = scratch.join("aggregate.json");
        std::fs::write(&arms_file, request.arms.runner_arms().to_string())
            .with_context(|| format!("writing {}", arms_file.display()))?;
        let mut command = Command::new(&self.python);
        command
            .args(self.args(request, &arms_file, &aggregate_out))
            .stdin(Stdio::null());
        if request.split == Split::Holdout {
            command.stdout(Stdio::null()).stderr(Stdio::null());
        }
        let status = command.status().with_context(|| {
            format!(
                "starting {} {}",
                self.python.display(),
                self.script.display()
            )
        })?;
        if !status.success() {
            match request.split {
                Split::Dev => bail!("the SWE runner failed ({status}); its output is above"),
                Split::Holdout => bail!(
                    "the SWE runner failed on the holdout ({status}); archon-bench discards its \
                     output, so rerun it under the validation item to diagnose"
                ),
            }
        }
        let text = std::fs::read_to_string(&aggregate_out).with_context(|| {
            format!(
                "the SWE runner wrote no aggregate on the {} split",
                request.split
            )
        })?;
        Aggregate::parse(&text, request.split, Some(&request.arms.keys()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::swe::arms::{ArchonBuild, Model};

    fn arm_set(dir: &Path) -> ArmSet {
        let build = |label: &str| {
            let bin = dir.join(label);
            std::fs::write(&bin, "").unwrap();
            ArchonBuild::new(label, &bin, &bin).unwrap()
        };
        ArmSet::new(build("v1.4.9"), build("head"), vec![Model::Haiku]).unwrap()
    }

    #[test]
    fn the_runner_gets_the_split_the_caps_and_both_files() {
        let dir = tempfile::tempdir().unwrap();
        let arms = arm_set(dir.path());
        let runner = Runner {
            python: "python3".into(),
            script: "/aether/evals/swe_bench.py".into(),
        };
        let request = Request {
            split: Split::Dev,
            arms: &arms,
            repeats: 1,
            jobs: 3,
            load_max: 12.0,
        };
        let args = runner.args(
            &request,
            Path::new("/s/arms.json"),
            Path::new("/s/out.json"),
        );
        let args: Vec<&str> = args.iter().map(|a| a.to_str().unwrap()).collect();
        assert_eq!(
            args,
            [
                "/aether/evals/swe_bench.py",
                "run",
                "--split",
                "dev",
                "--repeats",
                "1",
                "--jobs",
                "3",
                "--load-max",
                "12",
                "--arms",
                "/s/arms.json",
                "--aggregate-out",
                "/s/out.json"
            ]
        );
        assert!(
            Request {
                repeats: 0,
                ..request.clone()
            }
            .validate()
            .is_err()
        );
        assert!(
            Request {
                load_max: f64::NAN,
                ..request
            }
            .validate()
            .is_err()
        );
    }
}
