//! The risk label of a delivered pull request (ARCHITECTURE §8.7): the
//! repository gets the three `risk:` labels where it lacks them, the pull
//! request exactly the one its delivery settled on, and its body a "Risk"
//! section at the top. Strictly advisory: nothing merges or gates by it,
//! and a step GitHub refuses is told in the body and the run's summary,
//! never a reason the delivery fails.

use std::path::Path;

use archon_core::error::ArchonError;
use archon_core::id::QueueItemId;
use archon_core::repo::Repo;
use archon_core::risk::{LABEL_PREFIX, RiskLevel};
use archon_pipeline::risk::{self, Assessment, DiffStat, Evidence};
use serde::Deserialize;

use super::super::gh;
use crate::context::DaemonContext;

/// GitHub refuses a longer pull request body.
const MAX_BODY_CHARS: usize = 65_536;

/// What a delivery labels its pull request by.
pub(crate) struct Labelling<'a> {
    pub assessment: Assessment,
    /// Whether the commits were added to a pull request that had others:
    /// its label then never goes lower than it was.
    pub onto: bool,
    pub diff: Option<&'a DiffStat>,
    /// What `diff` is measured against, in words.
    pub against: String,
    pub checks: &'a [String],
    pub review: Option<&'a str>,
}

/// What `gh pr view` shows of a pull request's labels and body.
#[derive(Debug, Deserialize)]
struct Shown {
    #[serde(default)]
    labels: Vec<Named>,
    #[serde(default)]
    body: String,
}

#[derive(Debug, Deserialize)]
struct Named {
    name: String,
}

/// Label the pull request at `url` and write its risk section, then record
/// the level on the `item` that delivered it, if one did. The line it returns
/// says how that went, for the run's summary; only a failing store is an `Err`.
pub(crate) async fn label(
    ctx: &DaemonContext,
    repo: &Repo,
    item: Option<&QueueItemId>,
    url: &str,
    labelling: Labelling<'_>,
) -> Result<String, ArchonError> {
    assert!(url.starts_with("https://"), "a pull request GitHub listed");
    assert!(!labelling.against.is_empty(), "the diff's base is named");
    let binary = &ctx.config.github.binary;
    let shown = view(binary, repo, url).await;
    let mut assessment = labelling.assessment.clone();
    if labelling.onto {
        let before = shown
            .as_ref()
            .ok()
            .and_then(|shown| highest_label(&shown.labels));
        assessment = assessment.onto(before);
    }
    let level = assessment.level;
    // Without the labels it has, a new one could lower it or sit beside another.
    let labelled = match &shown {
        Ok(shown) => set_label(binary, repo, url, level, &shown.labels).await,
        Err(why) => Err(format!("the pull request could not be read: {why}")),
    };
    let label_note = match &labelled {
        Ok(()) => format!("`{}` is set.", level.label()),
        Err(why) => format!(
            "`{}` could not be set, so set it by hand: {why}",
            level.label()
        ),
    };
    let evidence = Evidence {
        diff: labelling.diff,
        against: &labelling.against,
        checks: labelling.checks,
        review: labelling.review,
        label: &label_note,
    };
    let section = risk::section(&assessment, evidence);
    let written = match &shown {
        Ok(shown) => {
            write_body(
                binary,
                repo,
                url,
                &risk::with_section(&shown.body, &section),
            )
            .await
        }
        Err(why) => Err(format!("the pull request could not be read: {why}")),
    };
    if let Some(item) = item {
        record(ctx, item, level).await?;
    }
    let note = summary_line(level, &labelled, &written);
    tracing::info!(%url, %level, %note, "risk labelled");
    Ok(note)
}

/// The one line of the run's summary on how labelling went.
fn summary_line(
    level: RiskLevel,
    labelled: &Result<(), String>,
    written: &Result<(), String>,
) -> String {
    let mut line = format!("RISK: {level}");
    match labelled {
        Ok(()) => line.push_str(&format!(", labelled `{}`", level.label())),
        Err(why) => line.push_str(&format!("; the label could not be set: {why}")),
    }
    if let Err(why) = written {
        line.push_str(&format!(
            "; the body's risk section could not be written: {why}"
        ));
    }
    let line: String = line.split_whitespace().collect::<Vec<_>>().join(" ");
    assert!(!line.contains('\n'), "one line");
    line
}

/// The highest risk level among `labels`, if any is a risk label.
fn highest_label(labels: &[Named]) -> Option<RiskLevel> {
    labels
        .iter()
        .filter_map(|l| RiskLevel::of_label(&l.name))
        .max()
}

async fn view(binary: &Path, repo: &Repo, url: &str) -> Result<Shown, String> {
    let shown = gh::text(binary, repo, &["pr", "view", url, "--json", "labels,body"])
        .await
        .map_err(|e| e.to_string())?;
    serde_json::from_str(&shown).map_err(|e| format!("invalid `gh pr view` response: {e}"))
}

/// Give the pull request `level`'s label and take every other risk label
/// off it, once the repository has the labels.
async fn set_label(
    binary: &Path,
    repo: &Repo,
    url: &str,
    level: RiskLevel,
    present: &[Named],
) -> Result<(), String> {
    ensure_labels(binary, repo).await?;
    let wanted = level.label();
    let stale: Vec<&str> = present
        .iter()
        .map(|l| l.name.as_str())
        .filter(|name| name.starts_with(LABEL_PREFIX) && *name != wanted)
        .collect();
    if stale.is_empty() && present.iter().any(|l| l.name == wanted) {
        return Ok(());
    }
    let mut args = vec!["pr", "edit", url, "--add-label", wanted.as_str()];
    for name in &stale {
        args.extend(["--remove-label", name]);
    }
    gh::text(binary, repo, &args)
        .await
        .map(drop)
        .map_err(|e| e.to_string())
}

/// Create whichever of the three risk labels the repository lacks.
async fn ensure_labels(binary: &Path, repo: &Repo) -> Result<(), String> {
    let listed = gh::text(
        binary,
        repo,
        &["label", "list", "--json", "name", "--limit", "1000"],
    )
    .await
    .map_err(|e| format!("the repository's labels could not be listed: {e}"))?;
    let names: Vec<Named> = serde_json::from_str(&listed)
        .map_err(|e| format!("invalid `gh label list` response: {e}"))?;
    for level in RiskLevel::ALL {
        let label = level.label();
        if names.iter().any(|n| n.name == label) {
            continue;
        }
        let args = [
            "label",
            "create",
            label.as_str(),
            "--color",
            level.color(),
            "--description",
            level.meaning(),
        ];
        let created = gh::run(binary, repo, &args, None)
            .await
            .map_err(|e| e.to_string())?;
        // Another delivery may have created it meanwhile.
        if !created.ok && !created.detail().to_lowercase().contains("already exists") {
            return Err(format!(
                "the label `{label}` could not be created: {}",
                created.detail()
            ));
        }
    }
    Ok(())
}

async fn write_body(binary: &Path, repo: &Repo, url: &str, body: &str) -> Result<(), String> {
    assert!(!body.trim().is_empty(), "a body with its risk section");
    if body.chars().count() >= MAX_BODY_CHARS {
        return Err(format!(
            "with it the body would pass GitHub's {MAX_BODY_CHARS} characters"
        ));
    }
    let args = ["pr", "edit", url, "--body-file", "-"];
    let ran = gh::run(binary, repo, &args, Some(body))
        .await
        .map_err(|e| e.to_string())?;
    match ran.ok {
        true => Ok(()),
        false => Err(format!("`gh pr edit` failed: {}", ran.detail())),
    }
}

/// Keep `level` on the item, for `get_item` and `list_queue` to show.
async fn record(
    ctx: &DaemonContext,
    item: &QueueItemId,
    level: RiskLevel,
) -> Result<(), ArchonError> {
    let mut store = ctx.store.lock().await;
    let mut stored = store.item(item)?;
    assert_eq!(&stored.id, item, "the delivering item's own record");
    if stored.risk == Some(level) {
        return Ok(());
    }
    stored.risk = Some(level);
    stored.updated_at = archon_core::now();
    store.update_item(&stored)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn named(names: &[&str]) -> Vec<Named> {
        names.iter().map(|n| Named { name: (*n).into() }).collect()
    }

    #[test]
    fn the_highest_risk_label_counts_and_others_are_ignored() {
        assert_eq!(
            highest_label(&named(&["bug", "risk: low", "risk: high"])),
            Some(RiskLevel::High)
        );
        assert_eq!(highest_label(&named(&["bug", "risk: urgent"])), None);
    }

    #[test]
    fn the_summary_line_says_what_failed_in_one_line() {
        let set = summary_line(RiskLevel::Medium, &Ok(()), &Ok(()));
        assert_eq!(set, "RISK: medium, labelled `risk: medium`");
        let refused = Err("HTTP 403:\nResource not accessible".to_owned());
        let line = summary_line(RiskLevel::High, &refused, &Err("too long".into()));
        assert!(
            line.starts_with("RISK: high; the label could not be set: HTTP 403:"),
            "{line}"
        );
        assert!(
            line.contains("risk section could not be written: too long") && !line.contains('\n')
        );
    }
}
