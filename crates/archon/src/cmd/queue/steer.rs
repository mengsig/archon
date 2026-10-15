//! `archon queue steer`: the operator's side of `steer_item`.

use archon_client::ArchonClient;
use archon_core::id::QueueItemId;
use archon_core::protocol::{Reply, Request};
use archon_core::steer::{InsertStage, Steer, Then};

use crate::cli::{AfterAbort, QueueSteerArgs};
use crate::cmd::emit;
use crate::connect::{connect_or_start, reply_data, unexpected};
use crate::views;

pub async fn run(args: QueueSteerArgs, json: bool) -> anyhow::Result<()> {
    let name = args.item.clone();
    let steer = steer_of(args);
    steer.check()?;
    let client = connect_or_start().await?;
    let item = views::resolve_item(&client, &name).await?;
    let request = Request::SteerItem {
        item: item.clone(),
        steer,
    };
    let reply = client.request(request).await?;
    if !matches!(reply, Reply::Item(_) | Reply::ItemBudgetsOff(_)) {
        return Err(unexpected(&reply, "item"));
    }
    super::note_budgets_off(&reply);
    if json {
        return emit(true, || reply_data(&reply), "");
    }
    let done = format!(
        "Steered {item}: {}.",
        last_change(&client, item.clone()).await?
    );
    emit(false, || reply_data(&reply), &done)
}

/// What archond recorded for the steer just made: the item's newest one.
async fn last_change(client: &ArchonClient, item: QueueItemId) -> anyhow::Result<String> {
    let reply = client.request(Request::GetItem { item }).await?;
    let Reply::ItemDetail(detail) = &reply else {
        return Err(unexpected(&reply, "item detail"));
    };
    let Some(last) = detail.steers.last() else {
        anyhow::bail!(
            "archond accepted the steer but recorded none for {}",
            detail.item.id
        );
    };
    Ok(last.what.clone())
}

/// The flags as one steer. `--remove` is an abort of an unstarted stage
/// that skips it; `--goto` names where an aborted run continues.
fn steer_of(args: QueueSteerArgs) -> Steer {
    assert!(
        args.remove.is_none() || (args.stage.is_none() && !args.abort),
        "clap refuses --remove with --stage or --abort"
    );
    assert!(
        args.abort || (args.then.is_none() && args.goto.is_none()),
        "clap refuses --then and --goto without --abort"
    );
    let abort = match (args.goto, args.then) {
        _ if args.remove.is_some() => Some(Then::Skip),
        _ if !args.abort => None,
        (Some(to), _) => Some(Then::Goto { to }),
        (None, Some(AfterAbort::Skip)) => Some(Then::Skip),
        (None, Some(AfterAbort::Retry) | None) => Some(Then::Retry),
    };
    Steer {
        note: args.note,
        stage: args.remove.or(args.stage),
        model: args.model,
        effort: args.effort,
        budget_usd: args.budget_usd,
        extend_minutes: args.extend,
        abort,
        insert_stage: args.insert.map(|stage| InsertStage {
            stage,
            from: args.from,
            after: args.after,
        }),
        switch_pipeline: args.switch,
    }
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::*;
    use crate::cli::{Cli, Command, QueueCommand};

    fn steer(flags: &[&str]) -> Steer {
        let line = [&["archon", "queue", "steer", "qi_1"], flags].concat();
        let cli = Cli::try_parse_from(line).expect("the flags parse");
        let Some(Command::Queue(QueueCommand::Steer(args))) = cli.command else {
            panic!("queue steer parses to its own command");
        };
        steer_of(*args)
    }

    #[test]
    fn an_abort_retries_unless_told_to_skip_or_go_elsewhere() {
        assert_eq!(steer(&["--abort"]).abort, Some(Then::Retry));
        let skipped = steer(&["--abort", "--then", "skip", "--stage", "review"]);
        assert_eq!(skipped.abort, Some(Then::Skip));
        assert_eq!(skipped.stage.as_deref(), Some("review"));
        let back = steer(&["--abort", "--goto", "implement"]);
        let to = "implement".to_owned();
        assert_eq!(back.abort, Some(Then::Goto { to }));
        assert_eq!(steer(&["--note", "n"]).abort, None);
    }

    #[test]
    fn remove_skips_the_stage_it_names() {
        let removed = steer(&["--remove", "review"]);
        assert_eq!(removed.stage.as_deref(), Some("review"));
        assert_eq!(removed.abort, Some(Then::Skip));
        removed.check().expect("a removal is a whole steer");
    }

    #[test]
    fn every_other_flag_lands_in_its_field() {
        let flags = [
            "--note",
            "Use the v2 client.",
            "--stage",
            "implement",
            "--model",
            "claude-code/claude-opus-5-5",
            "--effort",
            "xhigh",
            "--budget-usd",
            "40",
            "--extend",
            "30",
            "--insert",
            "verify",
            "--from",
            "deep-review",
            "--after",
            "review",
        ];
        let all = steer(&flags);
        all.check().expect("a valid steer");
        assert_eq!(all.note.as_deref(), Some("Use the v2 client."));
        assert_eq!(
            all.model.expect("model").to_string(),
            "claude-code/claude-opus-5-5"
        );
        assert_eq!(all.effort, Some(archon_core::model::Effort::XHigh));
        assert_eq!((all.budget_usd, all.extend_minutes), (Some(40.0), Some(30)));
        let insert = InsertStage {
            stage: "verify".into(),
            from: Some("deep-review".into()),
            after: Some("review".into()),
        };
        assert_eq!(all.insert_stage, Some(insert));
        let switched = steer(&["--switch", "quick"]);
        assert_eq!(switched.switch_pipeline.as_deref(), Some("quick"));
        assert!(
            steer(&["--stage", "review"]).check().is_err(),
            "nothing changes"
        );
    }
}
