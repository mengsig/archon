//! `get_stage`: its arguments, its unclipped and paged text, and the
//! `get_item` text that points to it when a stage summary is clipped.

use archon_core::id::{PipelineRunId, QueueItemId, StageRunId};
use archon_core::model::{Effort, ModelChoice};
use archon_core::protocol::{
    ItemDetail, STAGE_PAGE_CHARS, StageDetail, StageQuery, StageRunView, TextPage,
};
use archon_core::queue::QueueState;
use archon_core::run::{PipelineRun, RunState, StageRun, StageState, Verdict};
use serde_json::json;

use super::tests::qi;
use super::*;

fn choice() -> ModelChoice {
    ModelChoice {
        model: "codex/gpt-6-sol".parse().expect("model ref"),
        effort: Effort::High,
    }
}

fn view(slot: u32, output: Option<TextPage>) -> StageRunView {
    StageRunView {
        stage_id: "review".into(),
        attempt: 2,
        slot,
        choice: choice(),
        state: StageState::Completed,
        verdict: Some(Verdict::Fail),
        cost_usd: 0.25,
        delegation: None,
        aether_agent: None,
        output,
    }
}

fn detail(stage: Option<&str>, stages: Vec<StageRunView>) -> StageDetail {
    StageDetail {
        item: QueueItemId::new(),
        run: PipelineRunId::new(),
        stage: stage.map(str::to_owned),
        stages,
    }
}

fn refusal(arguments: Value) -> String {
    match to_request("get_stage", arguments) {
        Err(ToolError(text)) => text,
        Ok(request) => panic!("get_stage accepted invalid arguments as {request:?}"),
    }
}

#[test]
fn get_stage_maps_its_arguments_and_refuses_picks_without_a_stage() {
    let item = qi();
    let arguments = json!({"item": item, "stage": "review", "attempt": 2, "slot": 1, "offset": 10});
    let Ok(Request::GetStage(query)) = to_request("get_stage", arguments) else {
        panic!("get_stage maps to GetStage");
    };
    let expected = StageQuery {
        item: item.parse().expect("qi id"),
        run: None,
        stage: Some("review".into()),
        attempt: Some(2),
        slot: Some(1),
        offset: 10,
        limit: None,
    };
    assert_eq!(query, expected);
    let listing = to_request("get_stage", json!({"item": qi()})).expect("a listing");
    assert!(matches!(
        listing,
        Request::GetStage(StageQuery {
            stage: None,
            offset: 0,
            ..
        })
    ));
    for key in ["attempt", "slot", "offset", "limit"] {
        let why = refusal(json!({"item": qi(), key: 1}));
        assert!(why.contains(&format!("`{key}` needs `stage`")), "{why}");
    }
    let blank = refusal(json!({"item": qi(), "stage": "  "}));
    assert!(blank.contains("`stage`"), "{blank}");
}

#[test]
fn get_stage_takes_a_run_and_refuses_one_that_is_not_a_run_id() {
    let run = PipelineRunId::new();
    let arguments = json!({"item": qi(), "run": run.to_string()});
    let Ok(Request::GetStage(query)) = to_request("get_stage", arguments) else {
        panic!("get_stage maps to GetStage");
    };
    assert_eq!(query.run, Some(run));
    assert_eq!(query.stage, None, "a run alone lists that run's stages");
    let wrong = refusal(json!({"item": qi(), "run": qi()}));
    assert!(wrong.contains("`run`"), "{wrong}");
}

#[test]
fn a_stage_renders_its_whole_summary_and_pretty_structured_result() {
    let summary = format!("{}END", "a finding worth reading. ".repeat(200));
    let structured = json!({"verdict": "fail", "findings": [{"path": "src/a.rs", "line": 7}]});
    let full = format!(
        "summary:\n{summary}\n\nstructured:\n{}",
        serde_json::to_string_pretty(&structured).unwrap()
    );
    assert!(full.chars().count() > 1_500, "longer than get_item's clip");
    let page = TextPage::of(&full, 0, STAGE_PAGE_CHARS);
    let reply = Reply::StageDetail(Box::new(detail(Some("review"), vec![view(0, Some(page))])));
    let text = render("get_stage", &reply);
    assert!(text.contains(&summary), "not clipped: {text}");
    assert!(
        !text.contains("clipped") && !text.contains("offset"),
        "{text}"
    );
    assert!(
        text.contains("\"line\": 7,\n") && text.contains("\"path\": \"src/a.rs\""),
        "pretty-printed: {text}"
    );
    assert!(
        text.contains("stage review attempt 2, 1 stage run(s)")
            && text.contains(
                "## review attempt 2 slot 0: codex/gpt-6-sol high [completed] verdict fail"
            ),
        "{text}"
    );
}

#[test]
fn a_page_that_is_not_the_last_names_the_next_call() {
    let full = "x".repeat(250);
    let paged = |offset| {
        let page = TextPage::of(&full, offset, 100);
        let found = detail(Some("review"), vec![view(1, Some(page))]);
        let ids = (found.item.clone(), found.run.clone());
        (
            render("get_stage", &Reply::StageDetail(Box::new(found))),
            ids,
        )
    };
    let (first, (item, run)) = paged(0);
    let next = format!(
        "[characters 0-100 of 250; the rest: get_stage with item {item}, run {run}, stage \
         \"review\", attempt 2, slot 1, offset 100]"
    );
    assert!(first.ends_with(&next), "{first}");
    let (last, _) = paged(200);
    assert!(
        last.ends_with("[characters 200-250 of 250: the end]"),
        "{last}"
    );
    let (past, _) = paged(300);
    assert!(
        past.ends_with("[offset 300 is past the end: 250 characters in all]"),
        "{past}"
    );
}

#[test]
fn the_listing_names_each_stage_run_and_how_to_read_one() {
    let failed = StageRunView {
        state: StageState::Failed {
            error: "e".repeat(500),
        },
        verdict: None,
        ..view(1, None)
    };
    let reply = Reply::StageDetail(Box::new(detail(None, vec![view(0, None), failed])));
    let text = render("get_stage", &reply);
    assert!(text.contains(": 2 stage run(s). Pass `stage`"), "{text}");
    assert!(
        text.contains(
            "\n- review attempt 2 slot 0: codex/gpt-6-sol high [completed] verdict fail, cost $0.25"
        ),
        "{text}"
    );
    assert!(
        text.contains("slot 1: codex/gpt-6-sol high [failed: eee"),
        "{text}"
    );
    assert!(text.contains("[clipped: 500 chars in full]"), "{text}");
    let empty = render(
        "get_stage",
        &Reply::StageDetail(Box::new(detail(None, vec![]))),
    );
    assert!(empty.ends_with(": no stage has run yet."), "{empty}");
}

/// A stage that delegated says how its cost splits, in the listing and in
/// the item, so the orchestrator compares what a stage cost in all.
#[test]
fn a_delegating_stage_shows_its_cost_split_in_the_listing_and_the_item() {
    use archon_core::protocol::{DelegatedAgent, StageDelegation};
    let lead = archon_core::aether_ids::AetherAgentId::parse("agt_lead").unwrap();
    let helper = DelegatedAgent {
        agent: archon_core::aether_ids::AetherAgentId::parse("agt_helper").unwrap(),
        parent: lead.clone(),
        model: "codex/astra".parse().unwrap(),
        effort: Some(Effort::Medium),
        cost_usd: 0.75,
    };
    let split = StageDelegation::of(0.25, vec![helper]);
    let expected = "cost split: $0.25 own + $0.75 for 1 delegated agent(s): \
                    agt_helper codex/astra (medium) $0.75";
    let delegating = StageRunView {
        cost_usd: split.total_usd(),
        delegation: Some(split.clone()),
        aether_agent: Some(lead),
        ..view(0, None)
    };
    let listing = Reply::StageDetail(Box::new(detail(None, vec![delegating])));
    let text = render("get_stage", &listing);
    let line = format!("cost $1.00, agent agt_lead; {expected}");
    assert!(text.contains(&line), "{text}");

    let mut stage = long_review(PipelineRunId::new());
    stage.cost_usd = split.total_usd();
    let mut item = item_detail(stage);
    item.delegations.insert(item.stages[0].id.clone(), split);
    let text = render("get_item", &Reply::ItemDetail(Box::new(item)));
    assert!(text.contains("cost $1.00"), "{text}");
    assert!(text.contains(&format!("\n    {expected}")), "{text}");
}

fn item_detail(stage: StageRun) -> ItemDetail {
    let run = PipelineRun {
        id: stage.run.clone(),
        item: QueueItemId::new(),
        pipeline: "refactor".into(),
        shipped: false,
        aether_run: None,
        workspace: "/srv/wt".into(),
        team_root: None,
        branch: "archon/x".into(),
        state: RunState::Running {
            stage_id: "review".into(),
            attempt: 1,
        },
        cost_usd: 0.0,
        started_at: archon_core::now(),
        finished_at: None,
    };
    let mut item = super::tests::item(QueueState::Running {
        run: run.id.clone(),
    });
    item.id = run.item.clone();
    ItemDetail {
        steers: Vec::new(),
        waiting_on: None,
        waiting_reason: None,
        item,
        runs: vec![run],
        stages: vec![stage],
        spent_usd: 0.0,
        gates: vec![],
        delegations: Default::default(),
        switched_to: None,
    }
}

fn long_review(run: PipelineRunId) -> StageRun {
    StageRun {
        id: StageRunId::new(),
        run,
        stage_id: "review".into(),
        attempt: 1,
        fan_index: 0,
        model_segments: Vec::new(),
        infrastructure_failure: false,
        choice: choice(),
        aether_agent: None,
        aether_task: None,
        state: StageState::Completed,
        verdict: Some(Verdict::Fail),
        summary: Some("s".repeat(3_000)),
        structured: Some(json!({"findings": []})),
        cost_usd: 0.0,
        started_at: archon_core::now(),
        finished_at: None,
    }
}

#[test]
fn get_item_names_the_run_in_the_hints_of_an_older_run_only() {
    let mut detail = item_detail(long_review(PipelineRunId::new()));
    let older = detail.runs[0].clone();
    let latest = PipelineRun {
        id: PipelineRunId::new(),
        ..older.clone()
    };
    detail.runs.push(latest.clone());
    detail.stages.push(long_review(latest.id.clone()));
    let item = detail.item.id.clone();
    let text = render("get_item", &Reply::ItemDetail(Box::new(detail)));
    let of_older = format!(
        "get_stage with item {item}, run {}, stage \"review\", attempt 1, slot 0 shows it all",
        older.id
    );
    let of_latest =
        format!("get_stage with item {item}, stage \"review\", attempt 1, slot 0 shows it all");
    assert_eq!(text.matches(&of_older).count(), 1, "{text}");
    assert_eq!(text.matches(&of_latest).count(), 1, "{text}");
    assert!(!text.contains(&format!("run {},", latest.id)), "{text}");
}

#[test]
fn get_item_clips_a_long_stage_summary_and_points_to_get_stage() {
    let detail = item_detail(long_review(PipelineRunId::new()));
    let item = detail.item.id.clone();
    let text = render("get_item", &Reply::ItemDetail(Box::new(detail)));
    let hint = format!(
        "… [clipped: 3000 chars in full; get_stage with item {item}, stage \"review\", \
         attempt 1, slot 0 shows it all]"
    );
    assert!(text.contains(&hint), "{text}");
    assert!(
        text.contains(", structured result (get_stage shows it)"),
        "{text}"
    );
}

/// A stage a steer retuned while it ran is listed on the choice it runs now.
#[test]
fn get_item_names_the_choice_a_retuned_stage_runs_now() {
    let mut stage = long_review(PipelineRunId::new());
    let retuned = ModelChoice {
        model: "claude-code/opus".parse().expect("model ref"),
        effort: Effort::Low,
    };
    stage.model_segments.push(archon_core::run::ModelSegment {
        delivery: "steer-1".into(),
        choice: retuned.clone(),
        started_at: archon_core::now(),
        cost_before_usd: 0.0,
    });
    let text = render("get_item", &Reply::ItemDetail(Box::new(item_detail(stage))));
    let line = format!(
        "review attempt 1 slot 0: {} {}",
        retuned.model, retuned.effort
    );
    assert!(text.contains(&line), "{text}");
    assert!(!text.contains(&choice().model.to_string()), "{text}");
}
