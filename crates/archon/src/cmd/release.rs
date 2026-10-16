//! Read-only release health checks, shared by the CLI, doctor and release gate.

use std::path::Path;
use std::process::ExitCode;
use std::time::Duration;

use anyhow::Context as _;
use serde::Deserialize;

use super::doctor::registry::run_bounded;
use super::update::version::Version;
use crate::cli::ReleaseCommand;

const TARGETS: &[&str] = &[
    "x86_64-unknown-linux-musl",
    "aarch64-unknown-linux-musl",
    "aarch64-apple-darwin",
    "x86_64-apple-darwin",
];

#[derive(Deserialize)]
struct Tag {
    name: String,
}

#[derive(Deserialize)]
struct Asset {
    name: String,
    size: u64,
}

#[derive(Deserialize)]
struct Release {
    tag_name: String,
    draft: bool,
    prerelease: bool,
    assets: Vec<Asset>,
}

pub async fn run(command: ReleaseCommand, json: bool) -> anyhow::Result<ExitCode> {
    let ReleaseCommand::Check {
        repo,
        pending_tag,
        assets,
    } = command;
    assert_eq!(pending_tag.is_some(), assets.is_some());
    let config = archon_core::config::Config::load_default()?;
    config.validate()?;
    let pending = pending_tag.as_deref().zip(assets.as_deref());
    let problems = inspect(&config.github.binary, &repo, pending).await?;
    assert!(problems.iter().all(|problem| !problem.is_empty()));
    let ok = problems.is_empty();
    if json {
        crate::connect::print_json(&serde_json::json!({
            "repo": repo, "ok": ok, "problems": problems,
        }))?;
    } else if ok {
        println!("{repo}: release check passed");
    } else {
        for problem in &problems {
            println!("{repo}: {problem}");
        }
    }
    Ok(if ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

pub(super) async fn inspect(
    binary: &Path,
    repo: &str,
    pending: Option<(&str, &Path)>,
) -> anyhow::Result<Vec<String>> {
    anyhow::ensure!(
        !binary.as_os_str().is_empty(),
        "GitHub binary must not be empty"
    );
    let parts: Vec<_> = repo.split('/').collect();
    anyhow::ensure!(
        parts.len() == 2
            && parts.iter().all(|part| {
                !part.is_empty()
                    && *part != "."
                    && *part != ".."
                    && part
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
            }),
        "repository must be owner/name"
    );
    let product = parts[1];
    let mut problems = Vec::new();
    if let Some((tag, directory)) = pending {
        version(tag).context("pending tag must be a version")?;
        check_local(product, tag, directory, &mut problems)?;
    }
    let tags: Vec<Tag> = pages(binary, &format!("repos/{repo}/tags?per_page=100")).await?;
    let releases: Vec<Release> =
        pages(binary, &format!("repos/{repo}/releases?per_page=100")).await?;
    anyhow::ensure!(
        tags.iter().all(|tag| !tag.name.is_empty()),
        "GitHub returned an empty tag"
    );
    anyhow::ensure!(
        releases.iter().all(|release| !release.tag_name.is_empty()),
        "GitHub returned a release without a tag"
    );
    check_history(
        product,
        &tags,
        &releases,
        pending.map(|p| p.0),
        &mut problems,
    );
    check_latest(binary, repo, &releases, &mut problems).await?;
    check_ci(binary, repo, &mut problems).await?;
    Ok(problems)
}

async fn api<T: serde::de::DeserializeOwned>(binary: &Path, args: &[&str]) -> anyhow::Result<T> {
    assert_eq!(args.first(), Some(&"api"));
    assert!(args.len() >= 2);
    let output = run_bounded(binary, args, None, Duration::from_secs(15))
        .await
        .map_err(anyhow::Error::msg)?;
    anyhow::ensure!(
        output.status.success(),
        "gh {} failed: {}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr).trim()
    );
    serde_json::from_slice(&output.stdout).context("decoding gh API response")
}

async fn pages<T: serde::de::DeserializeOwned>(
    binary: &Path,
    endpoint: &str,
) -> anyhow::Result<Vec<T>> {
    let pages: Vec<Vec<T>> = api(binary, &["api", endpoint, "--paginate", "--slurp"]).await?;
    anyhow::ensure!(!pages.is_empty(), "gh returned no pages for {endpoint}");
    Ok(pages.into_iter().flatten().collect())
}

fn version(tag: &str) -> anyhow::Result<Version> {
    // Build metadata does not affect SemVer precedence.
    tag.split('+').next().unwrap_or(tag).parse()
}

fn required_assets(product: &str) -> Vec<String> {
    assert!(!product.is_empty() && !product.contains('/'));
    let mut names = vec!["SHA256SUMS".into(), "VERSION".into()];
    for target in TARGETS {
        let archive = format!("{product}-{target}.tar.gz");
        names.push(format!("{archive}.sha256"));
        names.push(archive);
    }
    assert_eq!(names.len(), 2 + TARGETS.len() * 2);
    names
}

fn check_assets(product: &str, tag: &str, assets: &[Asset], problems: &mut Vec<String>) {
    assert!(!tag.is_empty(), "a release must have a tag");
    for name in required_assets(product) {
        if !assets
            .iter()
            .any(|asset| asset.name == name && asset.size > 0)
        {
            problems.push(format!("{tag}: missing or empty asset {name}"));
        }
    }
}

fn check_local(
    product: &str,
    tag: &str,
    directory: &Path,
    problems: &mut Vec<String>,
) -> anyhow::Result<()> {
    assert!(
        version(tag).is_ok(),
        "pending tags are validated by inspect"
    );
    assert!(!product.is_empty() && !product.contains('/'));
    let mut assets = Vec::new();
    for entry in std::fs::read_dir(directory).context("reading pending release assets")? {
        let entry = entry?;
        let metadata = entry.metadata()?;
        if metadata.is_file() {
            assets.push(Asset {
                name: entry.file_name().to_string_lossy().into_owned(),
                size: metadata.len(),
            });
        }
    }
    check_assets(product, tag, &assets, problems);
    if assets.iter().any(|asset| asset.name == "VERSION") {
        let actual = std::fs::read_to_string(directory.join("VERSION"))?;
        if actual.trim() != tag {
            problems.push(format!("{tag}: VERSION contains {:?}", actual.trim()));
        }
    }
    Ok(())
}

fn check_history(
    product: &str,
    tags: &[Tag],
    releases: &[Release],
    pending: Option<&str>,
    problems: &mut Vec<String>,
) {
    assert!(tags.iter().all(|tag| !tag.name.is_empty()));
    assert!(pending.is_none_or(|tag| version(tag).is_ok()));
    for tag in tags.iter().filter(|tag| version(&tag.name).is_ok()) {
        if Some(tag.name.as_str()) != pending
            && !releases
                .iter()
                .any(|release| release.tag_name == tag.name && !release.draft)
        {
            problems.push(format!("{}: tag has no published release", tag.name));
        }
    }
    if let Some(tag) = pending
        && !tags.iter().any(|existing| existing.name == tag)
    {
        problems.push(format!("{tag}: pending tag does not exist"));
    }
    for release in releases {
        if Some(release.tag_name.as_str()) != pending {
            check_assets(product, &release.tag_name, &release.assets, problems);
        }
    }
}

async fn check_latest(
    binary: &Path,
    repo: &str,
    releases: &[Release],
    problems: &mut Vec<String>,
) -> anyhow::Result<()> {
    let highest = releases
        .iter()
        .filter(|r| !r.draft && !r.prerelease)
        .filter_map(|r| version(&r.tag_name).ok().map(|v| (v, r)))
        .filter(|(_, r)| !r.tag_name.split('+').next().unwrap_or("").contains('-'))
        .max_by(|a, b| a.0.cmp(&b.0));
    let Some((highest_version, highest)) = highest else {
        if !releases.is_empty() {
            problems.push("no published stable release is available as Latest".into());
        }
        return Ok(());
    };
    assert!(!highest.draft && !highest.prerelease);
    assert_eq!(
        version(&highest.tag_name).ok(),
        Some(highest_version.clone())
    );
    #[derive(Deserialize)]
    struct Latest {
        tag_name: String,
    }
    let latest: Latest = api(binary, &["api", &format!("repos/{repo}/releases/latest")]).await?;
    if version(&latest.tag_name).ok() != Some(highest_version) {
        problems.push(format!(
            "Latest is {}, highest stable semver is {}",
            latest.tag_name, highest.tag_name
        ));
    }
    Ok(())
}

async fn check_ci(binary: &Path, repo: &str, problems: &mut Vec<String>) -> anyhow::Result<()> {
    #[derive(Deserialize)]
    struct Runs {
        workflow_runs: Vec<Run>,
    }
    #[derive(Deserialize)]
    struct Run {
        id: u64,
    }
    #[derive(Deserialize)]
    struct Jobs {
        jobs: Vec<Job>,
    }
    #[derive(Deserialize)]
    struct Job {
        started_at: Option<String>,
        runner_id: Option<u64>,
    }
    let runs: Runs = api(
        binary,
        &[
            "api",
            &format!("repos/{repo}/actions/workflows/ci.yml/runs?status=completed&per_page=1"),
        ],
    )
    .await?;
    anyhow::ensure!(
        runs.workflow_runs.len() <= 1,
        "CI returned more than the requested latest run"
    );
    let Some(run) = runs.workflow_runs.first() else {
        problems.push("CI has no completed runs".into());
        return Ok(());
    };
    anyhow::ensure!(run.id > 0, "CI returned an invalid run id");
    let pages: Vec<Jobs> = api(
        binary,
        &[
            "api",
            &format!("repos/{repo}/actions/runs/{}/jobs?per_page=100", run.id),
            "--paginate",
            "--slurp",
        ],
    )
    .await?;
    if !pages.iter().flat_map(|page| &page.jobs).any(|job| {
        job.started_at.as_ref().is_some_and(|time| !time.is_empty())
            && job.runner_id.is_some_and(|id| id > 0)
    }) {
        problems.push(format!("CI run {} never started a job on a runner", run.id));
    }
    Ok(())
}
