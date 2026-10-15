use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use anyhow::{Context, ensure};
use archon_bench::arms::PrivateSockets;
use archon_bench::{plan, smoke};
use archon_core::model::{Effort, ModelRef};
use clap::Args;

use super::RunArgs;

#[derive(Debug, Args)]
pub struct SmokeArgs {
    #[arg(long, default_value = "claude-code/claude-haiku-5-5")]
    model: ModelRef,
    #[arg(long, default_value = "medium")]
    effort: Effort,
    /// Admission stop and final cost check; an in-flight trial may overshoot.
    #[arg(long, default_value_t = smoke::DEFAULT_MAX_USD)]
    max_usd: f64,
    #[arg(long, default_value = concat!(env!("CARGO_MANIFEST_DIR"), "/results-smoke/baseline.json"))]
    baseline: PathBuf,
    /// Run without a baseline and write candidate-baseline.json for review.
    #[arg(long, conflicts_with = "check_results")]
    bootstrap: bool,
    /// Check existing results without spending money.
    #[arg(long, conflicts_with = "dry_run")]
    check_results: Option<PathBuf>,
    #[arg(long)]
    dry_run: bool,
    #[arg(long, default_value_os_t = plan::default_results_dir())]
    results_dir: PathBuf,
    #[arg(long, default_value = "archon")]
    archon_bin: PathBuf,
    /// Required for a live run; must point to a private benchmark daemon.
    #[arg(long)]
    aether_socket: Option<PathBuf>,
}

pub async fn run(tasks_root: &Path, args: SmokeArgs) -> anyhow::Result<()> {
    if args.dry_run {
        super::run(tasks_root, into_run_args(args)).await?;
        return Ok(());
    }
    if args.bootstrap {
        let dir = super::run(tasks_root, into_run_args(args))
            .await?
            .context("bootstrap did not produce a results directory")?;
        smoke::write_candidate(&dir)?;
        return Ok(());
    }
    let baseline = smoke::Baseline::load(&args.baseline)?;
    if let Some(dir) = &args.check_results {
        super::render(dir, false)?;
        smoke::check_directory(dir, &baseline, args.max_usd)?;
        return Ok(());
    }
    ensure!(
        args.model.to_string() == baseline.model,
        "model differs from stored baseline"
    );
    ensure!(
        args.effort.to_string() == baseline.effort,
        "effort differs from stored baseline"
    );
    ensure!(
        args.max_usd <= baseline.max_usd,
        "smoke cost cap exceeds baseline cap"
    );
    let max_usd = args.max_usd;
    let dir = super::run(tasks_root, into_run_args(args))
        .await?
        .context("smoke did not produce a results directory")?;
    smoke::check_directory(&dir, &baseline, max_usd)?;
    Ok(())
}

pub(super) fn validate_sockets(aether: Option<&Path>) -> anyhow::Result<PrivateSockets> {
    let aether = aether.context("daemon arms need --aether-socket for a private sandbox")?;
    let archon = std::env::var_os("ARCHON_SOCKET")
        .map(PathBuf::from)
        .context("daemon arms need ARCHON_SOCKET set to a private sandbox path")?;
    let sandbox = plan::default_work_dir()?
        .parent()
        .context("work directory lacks a parent")?
        .canonicalize()
        .context("create the archon-bench sandbox directory first")?;
    validate_socket_paths(aether, &archon, &sandbox)?;
    let mut directories = BTreeSet::new();
    for name in [
        "ARCHON_CONFIG_DIR",
        "ARCHON_DATA_DIR",
        "AETHER_CONFIG_DIR",
        "AETHER_DATA_DIR",
    ] {
        let path = std::env::var_os(name).map(PathBuf::from);
        let resolved = validate_private_directory(name, path.as_deref(), &sandbox)?;
        ensure!(
            directories.insert(resolved),
            "sandbox config/data directories must be distinct ({name})"
        );
    }
    Ok(PrivateSockets {
        aether: aether.to_path_buf(),
        archon,
    })
}

fn validate_private_directory(
    name: &str,
    path: Option<&Path>,
    sandbox: &Path,
) -> anyhow::Result<PathBuf> {
    let path = path
        .with_context(|| format!("daemon arms need {name} set to a private sandbox directory"))?;
    ensure!(path.is_absolute(), "{name} must be absolute");
    let resolved = path
        .canonicalize()
        .with_context(|| format!("resolving {name}"))?;
    ensure!(resolved.is_dir(), "{name} must name a directory");
    ensure!(
        resolved.starts_with(sandbox) && resolved != sandbox,
        "{name} must be below {}",
        sandbox.display()
    );
    Ok(resolved)
}

fn validate_socket_paths(aether: &Path, archon: &Path, sandbox: &Path) -> anyhow::Result<()> {
    ensure!(sandbox.is_absolute(), "sandbox root must be absolute");
    ensure!(aether != archon, "Aether and Archon need separate sockets");
    for socket in [aether, archon] {
        ensure!(socket.is_absolute(), "sandbox socket must be absolute");
        let parent = socket
            .parent()
            .context("socket lacks a parent")?
            .canonicalize()?;
        ensure!(
            parent.starts_with(sandbox),
            "socket must be under {}",
            sandbox.display()
        );
        ensure!(!socket.is_symlink(), "sandbox socket cannot be a symlink");
    }
    Ok(())
}

fn into_run_args(args: SmokeArgs) -> RunArgs {
    RunArgs {
        arms: smoke::ARMS.to_vec(),
        only: smoke::TASKS.iter().map(|s| (*s).to_owned()).collect(),
        trials: 1,
        parallel: 1,
        model: args.model,
        archon_model: None,
        aether_model: None,
        claude_model: None,
        codex_model: None,
        effort: args.effort,
        max_usd: args.max_usd,
        max_trials: smoke::TASKS.len() * smoke::ARMS.len(),
        timeout_factor: 1.0,
        price: Vec::new(),
        dry_run: args.dry_run,
        lavish: false,
        work_dir: None,
        keep_workspaces: true,
        results_dir: args.results_dir,
        claude_bin: "claude".into(),
        codex_bin: "codex".into(),
        archon_bin: args.archon_bin,
        aether_socket: args.aether_socket,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use archon_bench::metrics::Arm;
    use clap::Parser;

    #[test]
    fn smoke_preset_fixes_tasks_arms_trials_and_budget() {
        let cli = crate::Cli::parse_from(["archon-bench", "smoke", "--dry-run"]);
        let crate::BenchCommand::Smoke(args) = cli.command else {
            panic!("parsed another command");
        };
        let run = into_run_args(*args);
        assert_eq!(run.only, smoke::TASKS);
        assert_eq!(run.arms, [Arm::Archon, Arm::Aether]);
        assert_eq!((run.trials, run.parallel, run.max_trials), (1, 1, 8));
        assert_eq!(run.max_usd, 7.0);
        assert_eq!(run.model.to_string(), "claude-code/claude-haiku-5-5");
        assert_eq!(run.effort, Effort::Medium);
        assert!(
            crate::Cli::try_parse_from(["archon-bench", "smoke", "--only", "fix-off-by-one"])
                .is_err()
        );
    }

    #[test]
    fn luna_can_be_selected_with_its_own_baseline() {
        let cli = crate::Cli::parse_from([
            "archon-bench",
            "smoke",
            "--model",
            "codex/gpt-6-luna",
            "--baseline",
            "luna.json",
            "--dry-run",
        ]);
        let crate::BenchCommand::Smoke(args) = cli.command else {
            panic!("parsed another command");
        };
        assert_eq!(args.baseline, Path::new("luna.json"));
        assert_eq!(into_run_args(*args).model.to_string(), "codex/gpt-6-luna");
    }

    #[test]
    fn all_sandbox_directories_are_required_and_cannot_escape() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let private = root.join("private");
        std::fs::create_dir(&private).unwrap();
        assert!(validate_private_directory("ARCHON_DATA_DIR", Some(&private), &root).is_ok());
        assert!(validate_private_directory("ARCHON_DATA_DIR", None, &root).is_err());
        assert!(validate_private_directory("ARCHON_DATA_DIR", Some(&root), &root).is_err());
        assert!(validate_private_directory("ARCHON_DATA_DIR", root.parent(), &root).is_err());
        std::os::unix::fs::symlink(root.parent().unwrap(), root.join("escape")).unwrap();
        assert!(
            validate_private_directory("ARCHON_DATA_DIR", Some(&root.join("escape")), &root)
                .is_err()
        );
    }

    #[test]
    fn sandbox_sockets_cannot_escape_or_alias_each_other() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let aether = root.join("aether.sock");
        let archon = root.join("archon.sock");
        assert!(validate_socket_paths(&aether, &archon, &root).is_ok());
        assert!(validate_socket_paths(&aether, &aether, &root).is_err());
        assert!(validate_socket_paths(Path::new("aether.sock"), &archon, &root).is_err());
        assert!(
            validate_socket_paths(&root.parent().unwrap().join("outside.sock"), &archon, &root)
                .is_err()
        );
        std::os::unix::fs::symlink(&archon, &aether).unwrap();
        assert!(validate_socket_paths(&aether, &archon, &root).is_err());
    }
}
