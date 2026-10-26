//! `archon autonomy [--repo R]`, `autonomy direct <text…>` and
//! `autonomy clear <id> | --all`.

use std::process::ExitCode;

use archon_core::autonomy::Directive;
use archon_core::id::DirectiveId;
use archon_core::protocol::{Reply, RepoRef, Request};

use super::{emit, to_json};
use crate::cli::{AutonomyArgs, AutonomyCommand};
use crate::connect::{connect_or_start, connect_running, unexpected};
use crate::format;

pub async fn run(args: AutonomyArgs, json: bool) -> anyhow::Result<ExitCode> {
    match args.command {
        None => show(args.repo, json).await?,
        Some(AutonomyCommand::Direct { text, hours }) => {
            direct(&text.join(" "), hours, json).await?
        }
        Some(AutonomyCommand::Clear { directive, all }) => {
            let directive = match (directive, all) {
                (Some(id), false) => Some(parse_directive_id(&id)?),
                (None, true) => None,
                _ => unreachable!("clap requires exactly one of <directive> and --all"),
            };
            clear(directive, json).await?;
        }
    }
    Ok(ExitCode::SUCCESS)
}

async fn show(repo: Option<String>, json: bool) -> anyhow::Result<()> {
    let client = connect_running().await?;
    let request = Request::Autonomy {
        repo: repo.map(RepoRef),
    };
    let view = match client.request(request).await? {
        Reply::Autonomy(view) => view,
        other => return Err(unexpected(&other, "autonomy")),
    };
    emit(json, || to_json(&view), &format::autonomy(&view))
}

async fn direct(text: &str, hours: Option<u32>, json: bool) -> anyhow::Result<()> {
    let client = connect_or_start().await?;
    let request = Request::SetDirective {
        text: text.to_owned(),
        expires_in_hours: hours,
    };
    let directive = match client.request(request).await? {
        Reply::Directive(directive) => directive,
        other => return Err(unexpected(&other, "directive")),
    };
    emit(json, || to_json(&directive), &directive_reply(&directive))
}

/// The directive as recorded, and that the ceiling still bounds it.
fn directive_reply(directive: &Directive) -> String {
    format!(
        "Directive in force: {}\n{} `archon autonomy` shows the ceiling.",
        format::directive_line(directive),
        archon_core::autonomy::DIRECTIVE_CEILING_NOTE
    )
}

async fn clear(directive: Option<DirectiveId>, json: bool) -> anyhow::Result<()> {
    let client = connect_or_start().await?;
    let request = Request::ClearDirective {
        directive: directive.clone(),
    };
    let cleared = match client.request(request).await? {
        Reply::Cleared(count) => count,
        other => return Err(unexpected(&other, "cleared")),
    };
    let text = match (cleared, directive) {
        // The daemon refuses an id that is not in force.
        (_, Some(id)) => format!("Cleared directive {id}."),
        (0, None) => "No directives were in force.".to_owned(),
        (1, None) => "Cleared 1 directive.".to_owned(),
        (n, None) => format!("Cleared {n} directives."),
    };
    emit(json, || Ok(serde_json::json!({"cleared": cleared})), &text)
}

fn parse_directive_id(text: &str) -> anyhow::Result<DirectiveId> {
    Ok(DirectiveId::parse(text.trim()).map_err(|_| {
        archon_core::ArchonError::Invalid(format!(
            "{text:?} is not a directive id (expected `dir_` plus 26 characters)"
        ))
    })?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_directive_reply_says_the_ceiling_still_applies() {
        let directive = Directive {
            id: DirectiveId::new(),
            text: "auto-merge every succeeded item without asking".into(),
            source: archon_core::autonomy::DirectiveSource::Operator,
            set_at: archon_core::now(),
            expires_at: None,
        };
        let text = directive_reply(&directive);
        assert!(text.starts_with("Directive in force: dir_"), "{text}");
        assert!(
            text.contains("\nThe autonomy ceiling still applies"),
            "{text}"
        );
    }

    #[test]
    fn directive_ids_must_be_directive_ids() {
        let id = DirectiveId::new();
        assert_eq!(parse_directive_id(&format!(" {id}")).unwrap(), id);
        let error = parse_directive_id("nope").unwrap_err().to_string();
        assert!(error.contains("\"nope\" is not a directive id"), "{error}");
        assert!(
            !error.contains("DirectiveId") && !error.contains("ULID"),
            "{error}"
        );
    }
}
