use std::path::Path;

use archon_aether::events::{AetherEvent, EventKind};
use archon_aether::wire::TaskResult;
use archon_core::aether_ids::AetherAgentId;
use archon_core::error::ArchonError;
use archon_core::queue::{QueueItem, QueueState};
use archon_core::run::{StageOutcome, StageRun, StageState, review_tally_key};
use archon_daemon::pump;
use serde_json::{Value, json};

use crate::common::delivery::{change, pass_by, record};
use crate::common::{Harness, REVIEW_MODEL, git};
use crate::executor::adaptive_item;

struct Loop {
    h: Harness,
    item: QueueItem,
    lead: StageRun,
    reports: Vec<AetherAgentId>,
    capped_head: String,
    fixed: Value,
    first_id: String,
    new_id: String,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Fixes {
    None,
    All,
    OnlyOne,
    OmittedEarlier,
    Large,
}

impl Loop {
    async fn at_cap(fixes: Fixes) -> Self {
        let (review, third) = Self::pending_cap(fixes).await;
        handle(&review.h, &third).await;
        review
    }

    async fn pending_cap(fixes: Fixes) -> (Self, AetherEvent) {
        let h = Harness::new().await;
        let item = adaptive_item(&h).await;
        let lead = h.running().await.remove(0);
        let workspace = h.ctx.store.lock().await.run(&lead.run).unwrap().workspace;
        let (first_id, second_head, [first_read, second_read]) =
            first_two_reviews(&h, &lead, &workspace, fixes == Fixes::OmittedEarlier).await;
        let third = change(&workspace);
        let mut findings = vec![
            json!({"title": "null check still bypassed", "blocking": true, "id": first_id}),
            json!({"title": "malformed record panics", "blocking": true}),
        ];
        if fixes == Fixes::Large {
            findings.extend(large_findings());
        }
        let count = findings.len();
        let third_read = fail(
            &h,
            &lead,
            &third["commit"],
            Some(&second_head),
            json!(findings),
        );
        let reviewer = third_read.agent.as_ref().unwrap();
        let new_id = format!("{reviewer}:2");
        let mut addressed = vec![first_id.clone()];
        if fixes != Fixes::OnlyOne {
            addressed.extend((2..=count).map(|n| format!("{reviewer}:{n}")));
        }
        if fixes != Fixes::None {
            commit_fixes(
                &workspace,
                &addressed.iter().map(String::as_str).collect::<Vec<_>>(),
            );
        }
        let fixed = record(&workspace);
        let review = Self {
            h,
            item,
            lead,
            fixed,
            first_id,
            new_id,
            capped_head: third["commit"].as_str().unwrap().into(),
            reports: [first_read, second_read, third_read.clone()]
                .into_iter()
                .map(|e| e.agent.unwrap())
                .collect(),
        };
        (review, third_read)
    }

    async fn stage(&self) -> StageRun {
        self.h
            .ctx
            .store
            .lock()
            .await
            .stage_run(&self.lead.id)
            .unwrap()
    }

    async fn finish(&self) {
        finish(&self.h, &self.fixed, &self.reports).await;
    }

    async fn cannot_fix(&self) {
        let outcome = StageOutcome::Failed {
            error: "could not fix every blocker".into(),
            infra: false,
        };
        tokio::time::timeout(
            std::time::Duration::from_secs(8),
            self.h.settle_running(|_| outcome.clone()),
        )
        .await
        .expect("reporting the failed stage settles the cap");
    }

    async fn workspace(&self) -> std::path::PathBuf {
        self.h
            .ctx
            .store
            .lock()
            .await
            .run(&self.lead.run)
            .unwrap()
            .workspace
    }

    async fn confirm(&mut self, verdict: &str, resolved: Value, findings: Value) {
        let result = json!({
            "verdict": verdict, "head": self.fixed["commit"], "base": self.capped_head,
            "resolved_findings": resolved, "findings": findings
        });
        let event = reporter(&self.h, &self.lead, result);
        handle(&self.h, &event).await;
        handle(&self.h, &event).await;
        self.reports.push(event.agent.unwrap());
    }
}

async fn finish(h: &Harness, fixed: &Value, approvals: &[AetherAgentId]) {
    assert!(!approvals.is_empty());
    let mut built = fixed.clone();
    built["review"] = pass_by(approvals)["review"].clone();
    let result = StageOutcome::Completed {
        summary: "Reviewed".into(),
        structured: Some(built),
    };
    h.settle_running(|_| result.clone()).await;
}

async fn first_two_reviews(
    h: &Harness,
    lead: &StageRun,
    workspace: &Path,
    omit_earlier: bool,
) -> (String, Value, [AetherEvent; 2]) {
    let first = change(workspace);
    let mut findings = vec![json!({"title": "missing null check", "blocking": true})];
    if omit_earlier {
        findings.push(json!({"title": "an earlier blocker was omitted", "blocking": true}));
    }
    let first_read = fail(h, lead, &first["commit"], None, json!(findings));
    let first_id = format!("{}:1", first_read.agent.as_ref().unwrap());
    handle(h, &first_read).await;

    let second = change(workspace);
    let second_read = fail(
        h,
        lead,
        &second["commit"],
        Some(&first["commit"]),
        json!([
            {"title": "null check still bypassed", "blocking": true, "id": first_id}
        ]),
    );
    handle(h, &second_read).await;
    (
        first_id,
        second["commit"].clone(),
        [first_read, second_read],
    )
}

fn commit_fixes(workspace: &Path, ids: &[&str]) {
    std::fs::write(workspace.join("change.txt"), "checked inputs\n").unwrap();
    git(workspace, &["add", "change.txt"]);
    let trailers = ids
        .iter()
        .map(|id| format!("Review-Fix: {id}"))
        .collect::<Vec<_>>()
        .join("\n");
    git(
        workspace,
        &["commit", "-qm", "Fix input validation", "-m", &trailers],
    );
}

fn fail(
    h: &Harness,
    lead: &StageRun,
    head: &Value,
    base: Option<&Value>,
    findings: Value,
) -> AetherEvent {
    reporter(
        h,
        lead,
        json!({"verdict": "fail", "head": head, "base": base, "findings": findings}),
    )
}

fn reporter(h: &Harness, lead: &StageRun, verdict: Value) -> AetherEvent {
    let parent = lead.aether_agent.clone().unwrap();
    let reviewer = h.backend.delegate(&parent, REVIEW_MODEL);
    report_event(h, lead, &reviewer, verdict)
}

fn report_event(
    h: &Harness,
    lead: &StageRun,
    reviewer: &AetherAgentId,
    verdict: Value,
) -> AetherEvent {
    h.backend.complete_delegated(reviewer, verdict.clone());
    AetherEvent {
        seq: 1,
        at: archon_core::now(),
        agent: Some(reviewer.clone()),
        task: None,
        parent: lead.aether_agent.clone(),
        run: None,
        kind: EventKind::TaskCompleted {
            result: TaskResult {
                summary: "Reviewed".into(),
                structured: Some(verdict),
                artifacts: Vec::new(),
            },
        },
    }
}

async fn handle(h: &Harness, event: &AetherEvent) {
    tokio::time::timeout(
        std::time::Duration::from_secs(8),
        pump::handle_fleet_event(&h.ctx, event),
    )
    .await
    .expect("a review event does not deadlock")
    .unwrap();
}

#[tokio::test]
async fn committed_fixes_get_one_confirmation_and_ship_in_the_same_run() {
    let mut review = Loop::at_cap(Fixes::None).await;
    assert_eq!(review.stage().await.state, StageState::Running);
    assert_eq!(confirmation_notes(&review.h), 0);
    assert!(review.h.backend.messages().iter().any(|(_, note)| {
        note.contains("commit fixes for every open blocker") && note.contains(&review.new_id)
    }));
    let workspace = review.workspace().await;
    commit_fixes(&workspace, &[&review.first_id, &review.new_id]);
    review.fixed = record(&workspace);
    for _ in 0..2 {
        tokio::time::timeout(std::time::Duration::from_secs(8), review.h.dispatch())
            .await
            .expect("the sweep checks for committed fixes");
    }
    let messages = review.h.backend.messages();
    assert_eq!(
        messages
            .iter()
            .filter(|(_, note)| note.contains("exactly one reviewer"))
            .count(),
        1
    );
    assert!(
        messages
            .iter()
            .any(|(_, note)| note.contains(&review.first_id) && note.contains("next review"))
    );
    let note = &messages.last().unwrap().1;
    assert!(
        note.contains(&review.first_id) && note.contains(&review.new_id),
        "{note}"
    );
    assert!(note.contains("Any real blocker"), "{note}");
    let resolved = json!([review.first_id, review.new_id]);
    review.confirm("pass", resolved, json!([])).await;
    review.finish().await;
    let stage = review.stage().await;
    assert_eq!(stage.state, StageState::Completed);
    let history = &stage.structured.unwrap()["review_rounds"];
    let rounds = history["rounds"].as_array().unwrap();
    assert_eq!(rounds.len(), 4);
    let first = &rounds[0]["reviews"][0];
    let third = &rounds[2]["reviews"][0];
    let confirmation = &rounds[3]["reviews"][0];
    assert_eq!(first["finding_count"], 1);
    assert_eq!(first["model"], REVIEW_MODEL);
    assert_eq!(first["verdict"], "fail");
    assert_eq!(first["next_round_resolved"], false);
    assert_eq!(third["finding_count"], 2);
    assert_eq!(third["next_round_resolved"], true);
    assert_eq!(confirmation["verdict"], "pass");
    let finding = &history["findings"][&review.first_id];
    assert_eq!(finding["found_in"]["number"], 1);
    assert_eq!(finding["resolved_in"]["number"], 4);
    let new = &history["findings"][&review.new_id];
    assert_eq!(new["found_in"]["number"], 3);
    assert_eq!(new["resolved_in"]["number"], 4);
    assert!(matches!(
        review.h.state(&review.item.id).await,
        archon_core::queue::QueueState::Succeeded { .. }
    ));
}

#[tokio::test]
async fn partial_commit_evidence_still_escalates_at_the_cap() {
    let review = Loop::at_cap(Fixes::OnlyOne).await;
    assert_eq!(review.stage().await.state, StageState::Running);
    review.h.dispatch().await;
    assert_eq!(confirmation_notes(&review.h), 0);
    review.cannot_fix().await;
    let StageState::Failed { error } = review.stage().await.state else {
        panic!("a partial fix must escalate");
    };
    assert!(error.contains("failed 3 rounds"), "{error}");
    assert!(
        !review
            .h
            .backend
            .messages()
            .iter()
            .any(|(_, note)| note.contains("exactly one reviewer"))
    );
}

#[tokio::test]
async fn a_new_blocker_in_confirmation_still_escalates() {
    let mut review = Loop::at_cap(Fixes::All).await;
    let resolved = json!([review.first_id, review.new_id]);
    review
        .confirm(
            "fail",
            resolved,
            json!([
                {"title": "new blocker outside fix delta", "blocking": true, "path": "untouched.rs"}
            ]),
        )
        .await;
    let stage = review.stage().await;
    let StageState::Failed { error } = stage.state else {
        panic!("a new blocker must escalate");
    };
    assert!(error.contains("new blocker outside fix delta"), "{error}");
    assert!(error.contains("final confirmation did not pass"), "{error}");
    assert_eq!(
        stage.structured.unwrap()["review_rounds"]["rounds"][3]["reviews"][0]["verdict"],
        "fail"
    );
}

#[tokio::test]
async fn a_confirmation_pass_without_every_disposition_escalates() {
    let mut review = Loop::at_cap(Fixes::All).await;
    let resolved = json!([review.first_id]);
    review.confirm("pass", resolved, json!([])).await;
    let StageState::Failed { error } = review.stage().await.state else {
        panic!("an unanswered blocker must escalate");
    };
    assert!(
        error.contains("did not pass and resolve every capped finding"),
        "{error}"
    );
}

#[tokio::test]
async fn a_confirmation_of_the_capped_head_spends_the_one_confirmation() {
    let review = Loop::at_cap(Fixes::All).await;
    let wrong_head = reporter(
        &review.h,
        &review.lead,
        json!({
            "verdict": "pass", "head": review.capped_head,
            "resolved_findings": [review.first_id, review.new_id], "findings": []
        }),
    );
    handle(&review.h, &wrong_head).await;
    let StageState::Failed { error } = review.stage().await.state else {
        panic!("a confirmation of the old head must escalate");
    };
    assert!(
        error.contains("read a different revision or delta"),
        "{error}"
    );
}

#[tokio::test]
async fn omitted_blockers_stay_in_the_prompt_and_count_at_the_cap() {
    let (review, third) = Loop::pending_cap(Fixes::OmittedEarlier).await;
    let prior_id = format!("{}:2", review.reports[0]);
    let notes = review.h.backend.messages();
    let note = &notes.last().unwrap().1;
    assert!(
        note.contains(&prior_id) && note.contains("an earlier blocker was omitted"),
        "{note}"
    );
    handle(&review.h, &third).await;
    review.cannot_fix().await;
    let stage = review.stage().await;
    let StageState::Failed { error } = stage.state else {
        panic!("the omitted blocker still needs a committed fix");
    };
    assert!(error.contains("failed 3 rounds"), "{error}");
    let history = &stage.structured.unwrap()["review_rounds"];
    assert!(history["findings"][&prior_id]["resolved_in"].is_null());
    assert_eq!(
        history["rounds"][0]["reviews"][0]["next_round_resolved"],
        false
    );
}

#[tokio::test]
async fn a_live_parallel_review_does_not_consume_the_confirmation() {
    let (mut review, third) = Loop::pending_cap(Fixes::All).await;
    let parent = review.lead.aether_agent.as_ref().unwrap();
    let peer = review.h.backend.delegate(parent, REVIEW_MODEL);
    handle(&review.h, &third).await;
    assert_eq!(review.stage().await.state, StageState::Running);
    assert_eq!(confirmation_notes(&review.h), 0);

    let parallel_pass = report_event(
        &review.h,
        &review.lead,
        &peer,
        json!({
            "verdict": "pass", "head": review.capped_head, "findings": []
        }),
    );
    handle(&review.h, &parallel_pass).await;
    handle(&review.h, &third).await;
    assert_eq!(review.stage().await.state, StageState::Running);
    assert_eq!(confirmation_notes(&review.h), 1);

    let resolved = json!([review.first_id, review.new_id]);
    review.confirm("pass", resolved, json!([])).await;
    review.finish().await;
    assert_eq!(review.stage().await.state, StageState::Completed);
    assert_eq!(confirmation_notes(&review.h), 1);
}

#[tokio::test]
async fn a_new_blocker_from_an_existing_parallel_reviewer_still_stops_the_stage() {
    let (review, third) = Loop::pending_cap(Fixes::All).await;
    let peer = review
        .h
        .backend
        .delegate(review.lead.aether_agent.as_ref().unwrap(), REVIEW_MODEL);
    handle(&review.h, &third).await;
    let parallel_fail = report_event(
        &review.h,
        &review.lead,
        &peer,
        json!({
            "verdict": "fail", "head": review.capped_head,
            "findings": [{"title": "late parallel blocker", "blocking": true}]
        }),
    );
    handle(&review.h, &parallel_fail).await;
    let StageState::Failed { error } = review.stage().await.state else {
        panic!("the new blocker also needs a committed fix");
    };
    assert!(error.contains("late parallel blocker"), "{error}");
    assert_eq!(confirmation_notes(&review.h), 0);
}

fn confirmation_notes(h: &Harness) -> usize {
    h.backend
        .messages()
        .iter()
        .filter(|(_, note)| note.contains("exactly one reviewer"))
        .count()
}

fn large_findings() -> Vec<Value> {
    (1..=20).map(|n| json!({"title": format!("large report finding {n}: {}", "λ".repeat(180)), "blocking": true})).collect()
}

fn full_handoff(workspace: &Path, prompt: &str) -> String {
    let path = prompt
        .split('`')
        .find(|word| word.starts_with(".archon/reports/review-findings-"))
        .expect("the prompt points to the full findings");
    assert!(prompt.len() < 1024 || !prompt.starts_with("The review"));
    assert!(
        !prompt.contains("large report finding 12"),
        "large details stay in the file"
    );
    std::fs::read_to_string(workspace.join(path)).unwrap()
}

#[tokio::test]
async fn oversized_caps_link_to_the_complete_confirmation_list() {
    let review = Loop::at_cap(Fixes::Large).await;
    let notes = review.h.backend.messages();
    let note = &notes.last().unwrap().1;
    assert!(note.contains("exactly one reviewer"), "{note}");
    let full = full_handoff(&review.workspace().await, note);
    assert!(
        full.contains(&review.first_id),
        "the earlier blocker is included"
    );
    for n in 2..=22 {
        assert!(
            full.contains(&format!("{}:{n}: ", review.reports[2])),
            "missing ID {n}"
        );
    }
}

#[tokio::test]
async fn oversized_open_findings_link_to_the_complete_list_after_a_resume() {
    let h = Harness::new().await;
    let item = adaptive_item(&h).await;
    let lead = h.running().await.remove(0);
    let workspace = h.run(&item.id).await.workspace;
    let initial = change(&workspace);
    let event = fail(&h, &lead, &initial["commit"], None, json!(large_findings()));
    handle(&h, &event).await;
    let first = h.backend.messages();
    let first = full_handoff(&workspace, &first.last().unwrap().1);
    assert_eq!(first.lines().count(), 20);

    h.backend.fail_agent(lead.aether_agent.as_ref().unwrap());
    h.settle_running(|_| StageOutcome::Failed {
        error: "engine died".into(),
        infra: true,
    })
    .await;
    h.dispatch().await;
    let resumed = h.running().await.remove(0);
    assert_ne!(lead.id, resumed.id);
    let prompt = h.backend.resumed_with(&resumed);
    let full = full_handoff(&workspace, &prompt);
    assert_eq!(full, first);
    assert!(full.contains(&format!("{}:20: ", event.agent.unwrap())));
}

#[tokio::test]
async fn a_refused_finding_handoff_escalates_visibly() {
    let h = Harness::new().await;
    let item = adaptive_item(&h).await;
    let lead = h.running().await.remove(0);
    let workspace = h.run(&item.id).await.workspace;
    let initial = change(&workspace);
    let event = fail(&h, &lead, &initial["commit"], None, json!(large_findings()));
    h.backend
        .fail_next_message(ArchonError::Forbidden("test refused the handoff".into()));
    handle(&h, &event).await;
    let stage = h.ctx.store.lock().await.stage_run(&lead.id).unwrap();
    let StageState::Failed { error } = stage.state else {
        panic!("the refused handoff must be visible")
    };
    assert!(
        error.contains("review finding IDs could not be delivered"),
        "{error}"
    );
    assert!(error.starts_with("ESCALATE:"), "{error}");
}

#[tokio::test]
async fn an_upgraded_tally_restores_already_counted_findings_and_their_ordinals() {
    let h = Harness::new().await;
    let item = adaptive_item(&h).await;
    let lead = h.running().await.remove(0);
    let workspace = h.run(&item.id).await.workspace;
    let initial = change(&workspace);
    let first = fail(
        &h,
        &lead,
        &initial["commit"],
        None,
        json!([
            {"title": "a follow-up", "blocking": false}, {"title": "older blocker", "blocking": true}
        ]),
    );
    let reviewer = first.agent.unwrap();
    let legacy = json!({"fails": [{"agent": reviewer, "head": initial["commit"], "findings": ["older blocker"]}], "read": [reviewer]});
    let key = review_tally_key(&lead.id);
    h.ctx
        .store
        .lock()
        .await
        .kv_set(&key, &legacy.to_string())
        .unwrap();
    let next = change(&workspace);
    let second = fail(
        &h,
        &lead,
        &next["commit"],
        Some(&initial["commit"]),
        json!([{"title": "another blocker", "blocking": true}]),
    );
    handle(&h, &second).await;
    let notes = h.backend.messages();
    let note = &notes.last().unwrap().1;
    assert!(
        note.contains(&format!("{reviewer}:1: a follow-up")),
        "{note}"
    );
    assert!(
        note.contains(&format!("{reviewer}:2: older blocker")),
        "{note}"
    );
    handle(&h, &second).await;
    assert_eq!(
        h.backend.messages().len(),
        notes.len(),
        "replay keeps the same IDs"
    );
}

#[tokio::test]
async fn a_delta_pass_must_confirm_the_blocker_even_if_the_lead_omits_its_reviewer() {
    for (claim, named, ship) in [
        ("omitted", false, false),
        ("still open", true, false),
        ("resolved", true, true),
    ] {
        let h = Harness::new().await;
        let item = adaptive_item(&h).await;
        let lead = h.running().await.remove(0);
        let workspace = h.run(&item.id).await.workspace;
        let first = change(&workspace);
        let fail = fail(
            &h,
            &lead,
            &first["commit"],
            None,
            json!([{"title": "invalid input panics", "blocking": true}]),
        );
        handle(&h, &fail).await;
        let failed = fail.agent.unwrap();
        let id = format!("{failed}:1");
        let fixed = change(&workspace);
        let mut result = json!({"verdict": "pass", "head": fixed["commit"], "base": first["commit"], "findings": []});
        match claim {
            "still open" => {
                result["findings"] = json!([{"title": "still open", "blocking": false, "id": id}])
            }
            "resolved" => result["resolved_findings"] = json!([id]),
            _ => {}
        }
        let delta = reporter(&h, &lead, result);
        handle(&h, &delta).await;
        let mut approvals = vec![delta.agent.unwrap()];
        if named {
            approvals.push(failed);
        }
        finish(&h, &fixed, &approvals).await;
        h.dispatch().await;
        if ship {
            assert!(matches!(
                h.state(&item.id).await,
                QueueState::Succeeded { .. }
            ));
        } else {
            let QueueState::Failed { error, .. } = h.state(&item.id).await else {
                panic!("{claim}: a missing disposition must block shipping")
            };
            assert!(
                error.contains(&format!(
                    "no subsequent review confirmed blocker {id} resolved"
                )),
                "{error}"
            );
        }
    }
}
