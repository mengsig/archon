//! Inspect and control a coordinated initiative.

use super::emit;
use crate::cli::InitiativeCommand;
use crate::connect::{connect, reply_data, unexpected};
use archon_core::id::InitiativeId;
use archon_core::protocol::{Reply, Request};
use std::process::ExitCode;

pub async fn run(command: Option<InitiativeCommand>, json: bool) -> anyhow::Result<ExitCode> {
    let request = match command {
        None | Some(InitiativeCommand::List) => Request::ListInitiatives,
        Some(InitiativeCommand::Show { initiative }) => Request::GetInitiative {
            initiative: InitiativeId::parse(&initiative)?,
        },
        Some(InitiativeCommand::Cancel { initiative }) => Request::CancelInitiative {
            initiative: InitiativeId::parse(&initiative)?,
        },
        Some(InitiativeCommand::Retry { initiative }) => Request::RetryInitiative {
            initiative: InitiativeId::parse(&initiative)?,
        },
    };
    let client = connect().await?;
    let reply = client.request(request).await?;
    let text = match &reply {
        Reply::Initiatives(initiatives) => initiatives
            .iter()
            .map(|i| format!("{} [{}] {}", i.id, i.state.as_str(), i.title))
            .collect::<Vec<_>>()
            .join("\n"),
        Reply::Initiative(detail) => {
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
        other => return Err(unexpected(other, "initiative")),
    };
    emit(json, || reply_data(&reply), &text)?;
    Ok(ExitCode::SUCCESS)
}
