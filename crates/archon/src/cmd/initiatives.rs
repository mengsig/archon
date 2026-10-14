//! Start, inspect and control coordinated initiatives.

use super::{emit, read_map, read_text};
use crate::cli::InitiativeCommand;
use crate::connect::{connect, reply_data, unexpected};
use archon_client::ArchonClient;
use archon_core::error::ArchonError;
use archon_core::id::{InitiativeId, id_prefix};
use archon_core::initiative::{ContractChange, Initiative, InitiativeDetail};
use archon_core::protocol::{Reply, Request};
use std::path::PathBuf;
use std::process::ExitCode;

pub async fn run(command: Option<InitiativeCommand>, json: bool) -> anyhow::Result<ExitCode> {
    let (client, request) = prepare(command.unwrap_or(InitiativeCommand::List)).await?;
    let reply = client.request(request).await?;
    let text = match &reply {
        Reply::Initiatives(initiatives) => list_text(initiatives),
        Reply::Initiative(detail) => detail_text(detail),
        other => return Err(unexpected(other, "initiative")),
    };
    emit(json, || reply_data(&reply), &text)?;
    Ok(ExitCode::SUCCESS)
}

/// A connection and the request for `command`; a file it names is read
/// before archond is started for it.
async fn prepare(command: InitiativeCommand) -> anyhow::Result<(ArchonClient, Request)> {
    Ok(match command {
        InitiativeCommand::List => (connect().await?, Request::ListInitiatives),
        InitiativeCommand::Create { file } => {
            let request = Request::EnqueueInitiative(read_map(&file)?);
            (connect().await?, request)
        }
        InitiativeCommand::Show { initiative } => {
            let (client, initiative) = connect_for(&initiative).await?;
            (client, Request::GetInitiative { initiative })
        }
        InitiativeCommand::Update {
            initiative,
            append_file,
            contract_file,
        } => {
            let change = contract_change(append_file, contract_file)?;
            let (client, initiative) = connect_for(&initiative).await?;
            (client, Request::UpdateInitiative { initiative, change })
        }
        InitiativeCommand::Cancel { initiative } => {
            let (client, initiative) = connect_for(&initiative).await?;
            (client, Request::CancelInitiative { initiative })
        }
        InitiativeCommand::Retry { initiative } => {
            let (client, initiative) = connect_for(&initiative).await?;
            (client, Request::RetryInitiative { initiative })
        }
    })
}

/// The change one of the two files holds; clap requires exactly one.
fn contract_change(
    append_file: Option<PathBuf>,
    contract_file: Option<PathBuf>,
) -> anyhow::Result<ContractChange> {
    Ok(match (append_file, contract_file) {
        (Some(path), None) => ContractChange::Append(read_text(&path, "contract addition")?),
        (None, Some(path)) => ContractChange::Replace(read_text(&path, "contract")?),
        both => unreachable!("clap requires exactly one file, not {both:?}"),
    })
}

/// A connection and the initiative `text` names.
async fn connect_for(text: &str) -> anyhow::Result<(ArchonClient, InitiativeId)> {
    let client = connect().await?;
    let initiative = resolve(&client, text).await?;
    Ok((client, initiative))
}

/// The initiative `text` names: a whole id, or the start of one, with or
/// without `ini_` and in any case, as for queue items.
async fn resolve(client: &ArchonClient, text: &str) -> anyhow::Result<InitiativeId> {
    let prefix = id_prefix(text, InitiativeId::PREFIX, "initiative")?;
    if let Ok(id) = InitiativeId::parse(&prefix) {
        return Ok(id);
    }
    let reply = client.request(Request::ListInitiatives).await?;
    let Reply::Initiatives(all) = reply else {
        return Err(unexpected(&reply, "initiatives"));
    };
    let id = only_match(&prefix, &all)?.id.clone();
    assert!(
        id.as_str().starts_with(&prefix),
        "{prefix} resolved to {id}"
    );
    Ok(id)
}

/// The one of `all` whose id starts with `prefix`.
fn only_match<'a>(prefix: &str, all: &'a [Initiative]) -> Result<&'a Initiative, ArchonError> {
    let matches: Vec<&Initiative> = all
        .iter()
        .filter(|i| i.id.as_str().starts_with(prefix))
        .collect();
    match matches.as_slice() {
        [] => Err(ArchonError::NotFound(format!(
            "initiative starting with {prefix}"
        ))),
        [one] => Ok(one),
        many => {
            let named: Vec<String> = many
                .iter()
                .map(|i| format!("{} ({})", i.id, i.title))
                .collect();
            Err(ArchonError::Invalid(format!(
                "{prefix} is ambiguous; it starts {} initiatives: {}; give more of the id",
                many.len(),
                named.join(", ")
            )))
        }
    }
}

fn list_text(initiatives: &[Initiative]) -> String {
    if initiatives.is_empty() {
        return "No initiatives. Start one with `archon initiatives create <file>`.".to_owned();
    }
    initiatives
        .iter()
        .map(|i| format!("{} [{}] {}", i.id, i.state.as_str(), i.title))
        .collect::<Vec<_>>()
        .join("\n")
}

fn detail_text(detail: &InitiativeDetail) -> String {
    let i = &detail.initiative;
    let members = detail
        .members
        .iter()
        .map(|m| format!("{} [{}] {}", m.id, m.state.kind(), m.title))
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "{} [{}] {}\natomic: {}\n\n{}\n\n{members}",
        i.id,
        i.state.as_str(),
        i.title,
        i.atomic,
        i.contract
    )
}
