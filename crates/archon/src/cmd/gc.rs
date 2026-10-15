//! `archon gc`: collect what finished runs left on disk (ARCHITECTURE
//! §13.2). A running archond collects on its own run table; without one,
//! or with `--offline`, the CLI collects itself under the daemon lock.

use std::process::ExitCode;
use std::time::Duration;

use anyhow::Context as _;
use archon_core::age::Age;
use archon_core::config::Config;
use archon_core::error::ArchonError;
use archon_core::protocol::{GcReport, Reply, Request, gib};
use archon_daemon::context::DaemonPaths;

use super::{emit, to_json};
use crate::connect::{archon_bin, connect_existing, unexpected};

/// A pass deletes caches and worktrees one by one; on a large data dir
/// that outlasts the client's usual request timeout.
const PASS_TIMEOUT: Duration = Duration::from_secs(30 * 60);

pub async fn run(
    older_than: Option<&str>,
    force: bool,
    offline: bool,
    json: bool,
) -> anyhow::Result<ExitCode> {
    let older_than = older_than.map(Age::parse).transpose()?;
    let report = if offline {
        collect_offline(older_than, force).await?
    } else if let Some(client) = connect_existing().await? {
        let request = Request::Gc {
            older_than_secs: older_than.map(Age::secs),
            force,
        };
        match client.request_within(request, PASS_TIMEOUT).await? {
            Reply::Gc(report) => *report,
            other => return Err(unexpected(&other, "gc")),
        }
    } else {
        // Starting archond just to collect would fail on the full disk
        // that is the usual reason to collect.
        collect_offline(older_than, force).await?
    };
    emit(json, || to_json(&report), &summary(&report))?;
    Ok(exit_code(&report))
}

async fn collect_offline(older_than: Option<Age>, force: bool) -> anyhow::Result<GcReport> {
    let socket = archon_client::default_socket();
    if let Some(mix) = archon_core::paths::partial_override(&socket) {
        return Err(ArchonError::Unavailable(format!(
            "archond is not running, and gc does not collect without it here: {mix}"
        ))
        .into());
    }
    let config = Config::load_default()?;
    let paths = DaemonPaths::resolve(&config, archon_bin()?);
    let pass = tokio::task::spawn_blocking(move || {
        archon_daemon::gc::collect_offline(&config, &paths, older_than, force)
    });
    match pass.await.context("running gc offline")? {
        Ok(report) => Ok(report),
        Err(ArchonError::Unavailable(why)) => Err(anyhow::anyhow!(
            "{why}; `archon gc` without --offline asks the running archond to collect"
        )),
        Err(error) => Err(anyhow::Error::from(error).context("collecting without archond")),
    }
}

/// Skipped worktrees were kept on purpose; only a real error fails gc.
fn exit_code(report: &GcReport) -> ExitCode {
    if report.failed.is_empty() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

fn summary(report: &GcReport) -> String {
    let mut lines = vec![format!(
        "Removed {} worktree(s), set-aside and emptied team directories and deleted the \
         build caches of {}; {} freed.",
        report.removed.len(),
        report.reclaimed.len(),
        gib(report.bytes_freed)
    )];
    if !report.caches.is_empty() || !report.item_caches.is_empty() || !report.artifacts.is_empty() {
        lines.push(format!(
            "Deleted {} shared build cache(s), the build directories of {} item(s) and the \
             artifacts of {} item(s).",
            report.caches.len(),
            report.item_caches.len(),
            report.artifacts.len()
        ));
    }
    if !report.reports.is_empty() {
        lines.push(format!(
            "Deleted {} report snapshot(s).",
            report.reports.len()
        ));
    }
    if report.pressure {
        lines.push("Disk pressure: finished runs and idle caches were collected sooner.".into());
    }
    for (path, reason) in &report.skipped {
        lines.push(format!("skipped {}: {reason}", path.display()));
    }
    for (path, reason) in &report.failed {
        lines.push(format!("failed {}: {reason}", path.display()));
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use archon_core::protocol::Reclaimed;

    use super::*;

    #[test]
    fn the_summary_counts_what_went_and_only_failures_fail() {
        let mut report = GcReport {
            removed: vec![PathBuf::from("/w/qi_1")],
            reclaimed: vec![Reclaimed {
                path: PathBuf::from("/w/qi_2"),
                bytes: 3 << 30,
            }],
            skipped: vec![(
                PathBuf::from("/w/qi_3"),
                "it has uncommitted changes".into(),
            )],
            failed: vec![],
            caches: vec![],
            artifacts: vec![],
            item_caches: vec![],
            reports: vec![],
            bytes_freed: 3 << 30,
            pressure: false,
        };
        let text = summary(&report);
        assert!(text.starts_with("Removed 1 worktree(s)"), "{text}");
        assert!(text.contains("build caches of 1; 3.0 GiB freed"), "{text}");
        assert!(
            text.contains("skipped /w/qi_3: it has uncommitted"),
            "{text}"
        );
        assert!(!text.contains("shared build cache"), "{text}");
        assert_eq!(exit_code(&report), ExitCode::SUCCESS);
        report.caches.push(Reclaimed {
            path: PathBuf::from("/d/cache/web"),
            bytes: 1,
        });
        report.pressure = true;
        let text = summary(&report);
        assert!(
            text.contains(
                "Deleted 1 shared build cache(s), the build directories of 0 item(s) and the \
                 artifacts of 0 item(s)."
            ),
            "{text}"
        );
        assert!(text.contains("Disk pressure:"), "{text}");
        report
            .failed
            .push((PathBuf::from("/w/qi_4"), "denied".into()));
        assert!(summary(&report).ends_with("failed /w/qi_4: denied"));
        assert_eq!(exit_code(&report), ExitCode::FAILURE);
    }
}
