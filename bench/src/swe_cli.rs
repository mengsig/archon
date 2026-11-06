//! `archon-bench swe`: the SWE family's command line (see `archon_bench::swe`).

use std::path::{Path, PathBuf};

use anyhow::{Context, ensure};
use archon_bench::plan;
use archon_bench::swe::aggregate::{Aggregate, Split};
use archon_bench::swe::arms::{ArchonBuild, ArmSet, Model};
use archon_bench::swe::manifest::DevManifest;
use archon_bench::swe::release::{self, Records};
use archon_bench::swe::runner::{Request, Runner};
use archon_bench::swe::{estimate, gate, report, sealed, stats};
use clap::{Args, Subcommand, ValueEnum};

#[derive(Debug, Subcommand)]
pub enum SweCommand {
    /// Print the arms, estimate and noise bound, then run one split through Aether's runner.
    Run(Box<RunArgs>),
    /// Gate a release on dev and holdout aggregates; exits nonzero unless every comparison passes.
    Gate(GateArgs),
    /// Rank the release-check records and name the best recent release for the old arm.
    BestRelease {
        #[arg(long, default_value = concat!(env!("CARGO_MANIFEST_DIR"), "/results-swe/release-records.json"))]
        records: PathBuf,
    },
    /// Check downloaded release assets against the release's SHA256SUMS.
    VerifyRelease {
        #[arg(long)]
        sums: PathBuf,
        #[arg(long, required = true, num_args = 1..)]
        asset: Vec<PathBuf>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum SplitArg {
    Dev,
    Holdout,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum ModelArg {
    Haiku,
    Luna,
}

fn models(args: &[ModelArg]) -> Vec<Model> {
    args.iter()
        .map(|m| match m {
            ModelArg::Haiku => Model::Haiku,
            ModelArg::Luna => Model::Luna,
        })
        .collect()
}

#[derive(Debug, Args)]
pub struct RunArgs {
    /// An Aether checkout holding bench/release-set/swe-v1 and the SWE runner.
    #[arg(long)]
    aether_checkout: PathBuf,
    #[arg(long, default_value = "python3")]
    python: PathBuf,
    #[arg(long, value_enum, default_value = "dev")]
    split: SplitArg,
    #[arg(long, value_enum, value_delimiter = ',', default_value = "haiku,luna")]
    models: Vec<ModelArg>,
    /// The best recent release, such as v1.4.9 (see `swe best-release`).
    #[arg(long)]
    old_label: String,
    #[arg(long)]
    old_archon: PathBuf,
    /// The released Aether the old Archon shipped with.
    #[arg(long)]
    old_aether: PathBuf,
    /// The candidate, such as a commit of the release head.
    #[arg(long)]
    new_label: String,
    #[arg(long)]
    new_archon: PathBuf,
    #[arg(long)]
    new_aether: PathBuf,
    #[arg(long, default_value_t = 1)]
    repeats: u32,
    #[arg(long, default_value_t = 3)]
    jobs: u32,
    /// The runner starts no trial above this 1-minute load.
    #[arg(long, default_value_t = 12.0)]
    load_max: f64,
    /// The runner admits no trial once the run has spent this many dollars.
    #[arg(long)]
    budget_usd: f64,
    /// The holdout's size, as the seal command printed it; archon-bench never reads the holdout.
    #[arg(long, default_value_t = 25)]
    holdout_instances: u32,
    /// Dev only: run the first N instances, to smoke-test the wiring.
    #[arg(long)]
    limit: Option<u32>,
    /// An earlier aggregate of the same split whose measured means replace the priors.
    #[arg(long)]
    estimate_from: Option<PathBuf>,
    #[arg(long)]
    work_dir: Option<PathBuf>,
    #[arg(long, default_value_os_t = plan::default_results_dir())]
    results_dir: PathBuf,
    /// Print the plan only.
    #[arg(long)]
    dry_run: bool,
}

#[derive(Debug, Args)]
pub struct GateArgs {
    #[arg(long)]
    dev: PathBuf,
    /// The holdout aggregate the validation item produced; without it the gate is incomplete.
    #[arg(long)]
    holdout: Option<PathBuf>,
    #[arg(long)]
    old_label: String,
    #[arg(long, value_enum, value_delimiter = ',', default_value = "haiku,luna")]
    models: Vec<ModelArg>,
    /// Where gate.md and gate.json go; defaults to the dev aggregate's directory.
    #[arg(long)]
    out: Option<PathBuf>,
}

pub fn run(command: SweCommand) -> anyhow::Result<()> {
    match command {
        SweCommand::Run(args) => run_split(&args),
        SweCommand::Gate(args) => run_gate(&args),
        SweCommand::BestRelease { records } => best_release(&records),
        SweCommand::VerifyRelease { sums, asset } => {
            for path in std::iter::once(&sums).chain(&asset) {
                sealed::refuse_inside_store(path)?;
            }
            for asset in &asset {
                let digest = release::verify_asset(&sums, asset)?;
                println!("ok\t{digest}\t{}", asset.display());
            }
            Ok(())
        }
    }
}

fn run_split(args: &RunArgs) -> anyhow::Result<()> {
    let split = match args.split {
        SplitArg::Dev => Split::Dev,
        SplitArg::Holdout => Split::Holdout,
    };
    let work_dir = match &args.work_dir {
        Some(dir) => dir.clone(),
        None => plan::default_work_dir()?,
    };
    for path in [&args.aether_checkout, &work_dir, &args.results_dir] {
        sealed::refuse_inside_store(path)?;
    }
    let arms = arm_set(args)?;
    let runner = Runner::in_checkout(&args.python, &args.aether_checkout)?;
    let request = Request {
        split,
        arms: &arms,
        repeats: args.repeats,
        jobs: args.jobs,
        load_max: args.load_max,
        budget_usd: args.budget_usd,
        limit: args.limit,
    };
    request.validate()?;
    let instances = instance_count(args, split)?;
    print_plan(args, &request, instances)?;
    if args.dry_run {
        return Ok(());
    }
    // Made before the run, so an unwritable results directory costs no trials.
    let dir = args.results_dir.join(format!(
        "swe-{split}-{}",
        archon_core::now().format("%Y%m%dT%H%M%SZ")
    ));
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let aggregate = runner.run(&request, &work_dir)?;
    let path = dir.join(format!("{split}-aggregate.json"));
    std::fs::write(&path, aggregate.to_json())
        .with_context(|| format!("writing {}", path.display()))?;
    let gate = gate::evaluate(&arms.models, std::slice::from_ref(&aggregate));
    println!("{}", report::render(&gate, &[aggregate], &arms.old.label));
    println!("aggregate: {}", path.display());
    Ok(())
}

/// The instances one run covers: the dev manifest's (or its first `--limit`),
/// or the holdout size the seal printed, within what the gate's test handles.
fn instance_count(args: &RunArgs, split: Split) -> anyhow::Result<u32> {
    let instances = match split {
        Split::Dev => {
            let manifest = DevManifest::load(&args.aether_checkout)?;
            println!("dev set: {}@{}", manifest.dataset, manifest.revision);
            args.limit
                .map_or(manifest.instances, |limit| limit.min(manifest.instances))
        }
        Split::Holdout => args.holdout_instances,
    };
    ensure!(
        (1..=stats::MAX_PAIRS).contains(&instances),
        "a {split} run covers 1 to {} instances, not {instances}",
        stats::MAX_PAIRS
    );
    Ok(instances)
}

fn arm_set(args: &RunArgs) -> anyhow::Result<ArmSet> {
    for path in [
        &args.old_archon,
        &args.old_aether,
        &args.new_archon,
        &args.new_aether,
    ] {
        sealed::refuse_inside_store(path)?;
    }
    let absolute = |path: &Path| std::path::absolute(path).context("resolving a binary path");
    let old = ArchonBuild::new(
        &args.old_label,
        &absolute(&args.old_archon)?,
        &absolute(&args.old_aether)?,
    )?;
    let new = ArchonBuild::new(
        &args.new_label,
        &absolute(&args.new_archon)?,
        &absolute(&args.new_aether)?,
    )?;
    ArmSet::new(old, new, models(&args.models))
}

fn print_plan(args: &RunArgs, request: &Request<'_>, instances: u32) -> anyhow::Result<()> {
    println!(
        "SWE {} split: {instances} instances, {} repeat(s)",
        request.split, request.repeats
    );
    for key in request.arms.keys() {
        match request.arms.build(key.harness) {
            Some(build) => println!(
                "  {key}: archon {} ({}) on aether {}",
                build.label,
                build.archon_bin.display(),
                build.aether_bin.display()
            ),
            None => println!("  {key}: native {}", key.model.native_cli()),
        }
    }
    let earlier = match &args.estimate_from {
        Some(path) => Some(read_aggregate(path, request.split)?),
        None => None,
    };
    let keys = request.arms.keys();
    let figures = estimate::estimate(
        &keys,
        instances,
        request.repeats,
        request.jobs,
        earlier.as_ref(),
    );
    print!("{}", estimate::render(&figures, request.jobs));
    println!("budget: ${:.2}", request.budget_usd);
    if request.budget_usd < figures.usd {
        println!(
            "  below the estimate: the runner may stop early, and the gate calls a short run incomplete"
        );
    }
    let bound: Vec<(u32, u32)> = gate::noise_bound(instances)
        .into_iter()
        .filter_map(|(discordant, losses)| losses.map(|k| (discordant, k)))
        .collect();
    let Some(&(smallest, _)) = bound.first() else {
        println!("noise bound: {instances} instances are too few for any comparison to fail");
        return Ok(());
    };
    let listed: Vec<String> = bound.iter().map(|(d, k)| format!("{k} of {d}")).collect();
    println!(
        "noise bound (one-sided paired test, α = {}): a comparison fails only when the \
         reference wins at least {} discordant instances; with fewer than {smallest} \
         discordant instances none fails",
        gate::ALPHA,
        listed.join(", ")
    );
    Ok(())
}

fn read_aggregate(path: &Path, split: Split) -> anyhow::Result<Aggregate> {
    sealed::refuse_inside_store(path)?;
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading the {split} aggregate {}", path.display()))?;
    Aggregate::parse_saved(&text, split).with_context(|| format!("checking {}", path.display()))
}

fn run_gate(args: &GateArgs) -> anyhow::Result<()> {
    let mut aggregates = vec![read_aggregate(&args.dev, Split::Dev)?];
    if let Some(path) = &args.holdout {
        aggregates.push(read_aggregate(path, Split::Holdout)?);
    }
    let gate = gate::evaluate(&models(&args.models), &aggregates);
    let markdown = report::render(&gate, &aggregates, &args.old_label);
    let out = match &args.out {
        Some(dir) => dir.clone(),
        None => args
            .dev
            .parent()
            .context("the dev aggregate has no directory")?
            .to_owned(),
    };
    sealed::refuse_inside_store(&out)?;
    std::fs::create_dir_all(&out).with_context(|| format!("creating {}", out.display()))?;
    std::fs::write(out.join("gate.md"), &markdown)?;
    std::fs::write(out.join("gate.json"), serde_json::to_string_pretty(&gate)?)?;
    println!("{markdown}\ngate: {}", out.join("gate.md").display());
    ensure!(gate.passed(), "the SWE gate fails: see the tables above");
    Ok(())
}

fn best_release(path: &Path) -> anyhow::Result<()> {
    sealed::refuse_inside_store(path)?;
    let records = Records::load(path)?;
    println!("window: {}\nmeasure: {}\n", records.window, records.measure);
    println!(
        "| Rank | Archon | Aether | Passed | Cost/run (Haiku + Luna) | Source |\n|---:|---|---|---:|---:|---|"
    );
    for (rank, r) in records.ranked().iter().enumerate() {
        println!(
            "| {} | {} | {} | {}/{} | ${:.4} | {} |",
            rank + 1,
            r.record.archon,
            r.record.aether,
            r.passed,
            r.trials,
            r.usd_per_run,
            r.record.source
        );
    }
    let best = records.best()?;
    println!(
        "\nbest: archon {} on aether {} ({}/{} passed; the current release is {})",
        best.record.archon, best.record.aether, best.passed, best.trials, records.current
    );
    Ok(())
}
