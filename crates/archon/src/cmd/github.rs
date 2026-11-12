//! `archon github import <issue>`: enqueue a GitHub issue as an item, once
//! (CONTRACTS.md §1.3 `import_issue`). Importing it again returns the first item.

use std::process::ExitCode;

use archon_core::error::ArchonError;
use archon_core::github::GithubRepo;
use archon_core::protocol::{GithubKind, GithubRef, Reply, Request};
use archon_core::queue::Priority;
use archon_core::repo::GH_HOST;

use super::emit;
use crate::connect::{connect_or_start, reply_data, unexpected};

#[derive(Debug, clap::Subcommand)]
pub enum GithubCommand {
    /// Enqueue a GitHub issue of a registered repository as an item. Its text
    /// becomes the brief, marked untrusted; a second import returns the first item.
    Import(ImportArgs),
}

#[derive(Debug, clap::Args)]
pub struct ImportArgs {
    /// `owner/name#123`, or the issue's `https://github.com/owner/name/issues/123`.
    pub issue: String,
    /// The pipeline; the repository's default when left out.
    #[arg(long)]
    pub pipeline: Option<String>,
    /// low, normal, high or urgent.
    #[arg(long)]
    pub priority: Option<String>,
}

pub async fn run(command: GithubCommand, json: bool) -> anyhow::Result<ExitCode> {
    let GithubCommand::Import(args) = command;
    let reference = parse_issue(&args.issue)?;
    let priority = args
        .priority
        .map(|p| serde_json::from_value::<Priority>(serde_json::Value::String(p)))
        .transpose()
        .map_err(|e| ArchonError::Invalid(format!("--priority: {e}")))?;
    let client = connect_or_start().await?;
    let request = Request::ImportIssue {
        reference,
        pipeline: args.pipeline,
        priority,
    };
    let reply = client.request(request).await?;
    let (Reply::Item(item) | Reply::ItemBudgetsOff(item)) = &reply else {
        return Err(unexpected(&reply, "item"));
    };
    emit(
        json,
        || reply_data(&reply),
        &format!("{} {}", item.id, item.title),
    )?;
    Ok(ExitCode::SUCCESS)
}

/// The issue `text` names: `owner/name#N` or a github.com issue URL.
fn parse_issue(text: &str) -> Result<GithubRef, ArchonError> {
    let text = text.trim();
    let url_path = ["https://github.com/", "http://github.com/"]
        .into_iter()
        .find_map(|prefix| text.strip_prefix(prefix));
    let (repo, number) = match url_path {
        Some(path) => {
            let path = path.trim_end_matches('/');
            let mut parts = path.splitn(4, '/');
            let (owner, name, issues, number) =
                (parts.next(), parts.next(), parts.next(), parts.next());
            match (owner, name, issues, number) {
                (Some(owner), Some(name), Some("issues"), Some(number)) => {
                    (format!("{owner}/{name}"), number)
                }
                _ => return Err(not_an_issue(text)),
            }
        }
        None => {
            let (repo, number) = text.split_once('#').ok_or_else(|| not_an_issue(text))?;
            (repo.to_owned(), number)
        }
    };
    let repo = GithubRepo::parse(&repo).map_err(|_| not_an_issue(text))?;
    let number: u64 = number.parse().map_err(|_| not_an_issue(text))?;
    if number == 0 {
        return Err(not_an_issue(text));
    }
    Ok(GithubRef {
        host: GH_HOST.into(),
        repo: repo.into(),
        number,
        kind: GithubKind::Issue,
    })
}

fn not_an_issue(text: &str) -> ArchonError {
    ArchonError::Invalid(format!(
        "{text:?} names no GitHub issue; give owner/name#123 or \
         https://github.com/owner/name/issues/123"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_issue_is_named_by_its_short_form_or_its_url() {
        for text in [
            "Mengsig/Archon#385",
            " https://github.com/mengsig/archon/issues/385/ ",
        ] {
            let reference = parse_issue(text).unwrap();
            assert_eq!(reference.repo, "mengsig/archon", "{text}");
            assert_eq!(reference.number, 385, "{text}");
            assert_eq!(reference.kind, GithubKind::Issue);
        }
    }

    #[test]
    fn anything_else_is_refused() {
        for text in [
            "mengsig/archon",
            "mengsig/archon#0",
            "mengsig/archon#x",
            "archon#1",
            "https://github.com/mengsig/archon/pull/1",
            "https://example.com/o/r/issues/1",
        ] {
            assert!(parse_issue(text).is_err(), "{text}");
        }
    }
}
