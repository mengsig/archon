//! `archon-bench`: compare Archon, plain Aether, `claude -p` and
//! `codex exec` on a fixed task suite (ARCHITECTURE §15). Trial workspaces
//! live outside the checkout (`~/.local/share/archon-bench/work/` by
//! default), results under `bench/results/<stamp>/`.

use std::path::{Path, PathBuf};

use anyhow::{bail, ensure};
use archon_bench::arms::{ArmSettings, PrivateSockets};
use archon_bench::isolation::Protected;
use archon_bench::metrics::Arm;
use archon_bench::plan::{self, RunOptions};
use archon_bench::report;
use archon_bench::runner;
use archon_bench::summary;
use archon_bench::task::BenchTask;
use archon_bench::usage::PriceArg;
use archon_bench::verify;
use archon_core::model::{Effort, ModelRef};
use clap::{Args, Parser, Subcommand};

mod smoke_cli;
mod swe_cli;

#[derive(Debug, Parser)]
#[command(
    name = "archon-bench",
    about = "Archon vs plain Aether vs claude -p vs codex exec"
)]
struct Cli {
    /// Task suite root.
    #[arg(long, default_value = concat!(env!("CARGO_MANIFEST_DIR"), "/tasks"))]
    tasks: PathBuf,
    #[command(subcommand)]
    command: BenchCommand,
}

#[derive(Debug, Subcommand)]
enum BenchCommand {
    /// List the suite.
    List,
    /// Run trials and write results.jsonl, run.json, summary.md, chart.json and report.html.
    Run(Box<RunArgs>),
    /// Run the fixed quality gate or bootstrap a candidate baseline.
    Smoke(Box<smoke_cli::SmokeArgs>),
    /// Re-render summary.md, chart.json and report.html from a results directory.
    Report {
        results: PathBuf,
        /// Open report.html in Lavish for review.
        #[arg(long)]
        lavish: bool,
    },
    /// The SWE family on Aether's shared swe-v1 set, and its release gate.
    #[command(subcommand)]
    Swe(swe_cli::SweCommand),
    /// Check graders against untouched, reference and optional naive fixtures.
    Verify {
        /// Task ids; empty checks every task.
        #[arg(long, value_delimiter = ',')]
        only: Vec<String>,
    },
}

#[derive(Debug, Args)]
struct RunArgs {
    #[arg(
        long,
        value_delimiter = ',',
        default_value = "archon,aether,claude,codex"
    )]
    arms: Vec<Arm>,
    /// Task ids; empty runs every task.
    #[arg(long, value_delimiter = ',')]
    only: Vec<String>,
    /// Trials per (task, arm).
    #[arg(long, default_value_t = 3)]
    trials: u32,
    /// Trials running at once (archon trials still run one at a time).
    #[arg(long, default_value_t = 1)]
    parallel: usize,
    /// The model every Claude-based arm uses, as provider/model.
    #[arg(long, default_value = "claude-code/claude-haiku-5-5")]
    model: ModelRef,
    /// Override --model for the archon arm (orchestrator and stages).
    #[arg(long)]
    archon_model: Option<ModelRef>,
    /// Override --model for the aether arm.
    #[arg(long)]
    aether_model: Option<ModelRef>,
    /// `claude --model`; defaults to --model's name when it is a claude-code model.
    #[arg(long)]
    claude_model: Option<String>,
    /// `codex exec -m`; defaults to gpt-6-luna.
    #[arg(long)]
    codex_model: Option<String>,
    #[arg(long, default_value = "high")]
    effort: Effort,
    /// Stop starting trials once recorded spend reaches this many dollars.
    #[arg(long, default_value_t = 25.0)]
    max_usd: f64,
    /// Refuse plans with more trials than this.
    #[arg(long, default_value_t = 100)]
    max_trials: usize,
    /// Multiplies every task's timeout_minutes.
    #[arg(long, default_value_t = 1.0)]
    timeout_factor: f64,
    /// List price for unpriced arms: MODEL=IN,CACHED_IN,OUT in USD per Mtok.
    #[arg(long)]
    price: Vec<PriceArg>,
    /// Print the plan and every arm's command line; run nothing.
    #[arg(long)]
    dry_run: bool,
    /// Open report.html in Lavish when the run finishes.
    #[arg(long)]
    lavish: bool,
    /// Trial workspaces; must be outside the checkout, the task suite, the
    /// results and /tmp. Defaults to $XDG_DATA_HOME/archon-bench/work.
    #[arg(long)]
    work_dir: Option<PathBuf>,
    /// Keep trial workspaces after grading instead of deleting them.
    #[arg(long)]
    keep_workspaces: bool,
    #[arg(long, default_value_os_t = plan::default_results_dir())]
    results_dir: PathBuf,
    #[arg(long, default_value = "claude")]
    claude_bin: PathBuf,
    #[arg(long, default_value = "codex")]
    codex_bin: PathBuf,
    #[arg(long, default_value = "archon")]
    archon_bin: PathBuf,
    /// The private sandbox aetherd's socket; the daemon arms require it.
    #[arg(long)]
    aether_socket: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        BenchCommand::List => {
            for task in BenchTask::load_all(&cli.tasks)? {
                let kind = if task.is_multi_repo() {
                    "multi-repo"
                } else {
                    "single"
                };
                println!(
                    "{}\t{}\t{}m\t{}",
                    task.file.id, kind, task.file.timeout_minutes, task.file.title
                );
            }
            Ok(())
        }
        BenchCommand::Run(args) => run(&cli.tasks, *args).await.map(|_| ()),
        BenchCommand::Smoke(args) => smoke_cli::run(&cli.tasks, *args).await,
        BenchCommand::Swe(command) => swe_cli::run(command),
        BenchCommand::Report { results, lavish } => render(&results, lavish),
        BenchCommand::Verify { only } => verify_all(&cli.tasks, &only).await,
    }
}

async fn run(tasks_root: &Path, args: RunArgs) -> anyhow::Result<Option<PathBuf>> {
    let sockets = private_sockets(&args)?;
    let tasks = BenchTask::select(BenchTask::load_all(tasks_root)?, &args.only)?;
    let protected = Protected::for_run(tasks_root, &args.results_dir)?;
    let work_dir = match &args.work_dir {
        Some(dir) => dir.clone(),
        None => plan::default_work_dir()?,
    };
    protected.check_work_dir(&work_dir)?;
    let mut settings = settings(&args)?;
    settings.sockets = sockets;
    settings.env = protected.scrub_env(std::env::vars_os());
    let options = RunOptions {
        arms: args.arms.clone(),
        trials: args.trials,
        parallel: args.parallel,
        max_usd: args.max_usd,
        max_trials: args.max_trials,
        timeout_factor: args.timeout_factor,
        work_dir,
        results_dir: args.results_dir.clone(),
        stamp: archon_core::now().format("%Y%m%dT%H%M%SZ").to_string(),
        keep_workspaces: args.keep_workspaces,
    };
    options.validate()?;
    if args.dry_run {
        let specs = plan::plan(tasks.len(), &options.arms, options.trials);
        plan::check_plan(&specs, &options)?;
        let versions = runner::collect_versions(&settings, &options.arms).await;
        let versions: Vec<(Arm, String)> = options
            .arms
            .iter()
            .filter_map(|&arm| versions.get(program_of(arm)).map(|v| (arm, v.clone())))
            .collect();
        print!(
            "{}",
            plan::render_dry_run(&tasks, &specs, &settings, &options, &versions)?
        );
        return Ok(None);
    }
    let dir = runner::run(tasks, settings, options).await?;
    render(&dir, args.lavish)?;
    Ok(Some(dir))
}

/// The sandbox sockets the daemon arms use. A dry run may go without, but
/// then nothing resolves, probes or prints a default socket.
fn private_sockets(args: &RunArgs) -> anyhow::Result<Option<PrivateSockets>> {
    let daemons = args
        .arms
        .iter()
        .any(|arm| matches!(arm, Arm::Archon | Arm::Aether));
    if !daemons {
        return Ok(None);
    }
    match smoke_cli::validate_sockets(args.aether_socket.as_deref()) {
        Ok(sockets) => Ok(Some(sockets)),
        Err(e) if args.dry_run => {
            eprintln!("dry run with no private sandbox, so no daemon is probed: {e:#}");
            Ok(None)
        }
        Err(e) => Err(e),
    }
}

fn program_of(arm: Arm) -> &'static str {
    match arm {
        Arm::Archon => "archon",
        Arm::Aether => "aetherd",
        Arm::Claude => "claude",
        Arm::Codex => "codex",
    }
}

fn settings(args: &RunArgs) -> anyhow::Result<ArmSettings> {
    let claude_model = match (&args.claude_model, args.model.provider.as_str()) {
        (Some(model), _) => model.clone(),
        (None, "claude-code") => args.model.model.clone(),
        (None, provider) if args.arms.contains(&Arm::Claude) => {
            bail!("--model uses provider {provider}; name the claude CLI model with --claude-model")
        }
        (None, _) => String::new(),
    };
    let codex_model = match &args.codex_model {
        Some(model) => model.clone(),
        None if args.arms.contains(&Arm::Codex) => "gpt-6-luna".to_owned(),
        None => String::new(),
    };
    ensure!(
        !args.arms.contains(&Arm::Claude) || !claude_model.is_empty(),
        "empty --claude-model"
    );
    Ok(ArmSettings {
        claude_bin: args.claude_bin.clone(),
        codex_bin: args.codex_bin.clone(),
        archon_bin: args.archon_bin.clone(),
        sockets: None,
        archon_model: args
            .archon_model
            .clone()
            .unwrap_or_else(|| args.model.clone()),
        aether_model: args
            .aether_model
            .clone()
            .unwrap_or_else(|| args.model.clone()),
        claude_model,
        codex_model,
        effort: args.effort,
        prices: args.price.clone(),
        env: Default::default(),
    })
}

fn render(dir: &Path, lavish: bool) -> anyhow::Result<()> {
    let summary = summary::write_summary(dir)?;
    let markdown = std::fs::read_to_string(dir.join("summary.md"))?;
    let html = report::write_report(dir, &summary, &markdown)?;
    println!("{markdown}");
    println!("results: {}\nreport:  {}", dir.display(), html.display());
    if lavish {
        report::open_in_lavish(&html)?;
    }
    Ok(())
}

async fn verify_all(tasks_root: &Path, only: &[String]) -> anyhow::Result<()> {
    let tasks = BenchTask::select(BenchTask::load_all(tasks_root)?, only)?;
    let base = plan::default_work_dir()?.join(format!(
        "verify-{}",
        archon_core::now().format("%Y%m%dT%H%M%S%.3fZ")
    ));
    let mut broken = Vec::new();
    for task in &tasks {
        let checked = verify::verify(task, &base.join(&task.file.id)).await?;
        let verdict = if checked.holds() { "ok" } else { "BROKEN" };
        println!(
            "{verdict}\t{}\tuntouched: pass={} score={:?}\treference: pass={} score={:?}",
            checked.task,
            checked.untouched.success,
            checked.untouched.score,
            checked.reference.success,
            checked.reference.score
        );
        if let Some(naive) = &checked.naive {
            println!("\tnaive: pass={} score={:?}", naive.success, naive.score);
        }
        if !checked.holds() {
            broken.push(checked.task);
        }
    }
    ensure!(
        broken.is_empty(),
        "graders broken for {broken:?}; logs under {}",
        base.display()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paid_run_defaults_use_cheap_models_without_local_config() {
        let cli = Cli::parse_from(["archon-bench", "run", "--dry-run"]);
        let BenchCommand::Run(args) = cli.command else {
            panic!("parsed another command");
        };
        let settings = settings(&args).unwrap();
        assert_eq!(
            settings.archon_model.to_string(),
            "claude-code/claude-haiku-5-5"
        );
        assert_eq!(settings.aether_model, settings.archon_model);
        assert_eq!(settings.claude_model, "claude-haiku-5-5");
        assert_eq!(settings.codex_model, "gpt-6-luna");
    }
}
