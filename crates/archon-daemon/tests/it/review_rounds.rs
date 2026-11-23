use std::path::Path;

use archon_aether::events::{AetherEvent, EventKind};
use archon_aether::wire::TaskResult;
use archon_core::aether_ids::AetherAgentId;
use archon_core::queue::QueueItem;
use archon_core::run::{StageOutcome, StageRun, StageState};
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

impl Loop {
    async fn at_cap(all_fixed: bool) -> Self {
        let h = Harness::new().await;
        let item = adaptive_item(&h).await;
        let lead = h.running().await.remove(0);
        let workspace = h.ctx.store.lock().await.run(&lead.run).unwrap().workspace;
        let first = change(&workspace);
        let first_read = fail(
            &h,
            &lead,
            &first["commit"],
            None,
            json!([
                {"title": "missing null check", "blocking": true}
            ]),
        );
        let first_id = format!("{}:1", first_read.agent.as_ref().unwrap());
        handle(&h, &first_read).await;

        let second = change(&workspace);
        let second_read = fail(
            &h,
            &lead,
            &second["commit"],
            Some(&first["commit"]),
            json!([
                {"title": "null check still bypassed", "blocking": true, "id": first_id}
            ]),
        );
        handle(&h, &second_read).await;

        let third = change(&workspace);
        let third_read = fail(
            &h,
            &lead,
            &third["commit"],
            Some(&second["commit"]),
            json!([
                {"title": "null check still bypassed", "blocking": true, "id": first_id},
                {"title": "malformed record panics", "blocking": true}
            ]),
        );
        let new_id = format!("{}:2", third_read.agent.as_ref().unwrap());
        let mut fixes = vec![first_id.as_str()];
        if all_fixed {
            fixes.push(&new_id);
        }
        commit_fixes(&workspace, &fixes);
        let fixed = record(&workspace);
        handle(&h, &third_read).await;

        Self {
            h,
            item,
            lead,
            fixed,
            first_id,
            new_id,
            capped_head: third["commit"].as_str().unwrap().into(),
            reports: [first_read, second_read, third_read]
                .into_iter()
                .map(|e| e.agent.unwrap())
                .collect(),
        }
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
    h.backend.complete_delegated(&reviewer, verdict.clone());
    AetherEvent {
        seq: 1,
        at: archon_core::now(),
        agent: Some(reviewer),
        task: None,
        parent: Some(parent),
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
    let mut review = Loop::at_cap(true).await;
    assert_eq!(review.stage().await.state, StageState::Running);
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
    let mut built = review.fixed.clone();
    built
        .as_object_mut()
        .unwrap()
        .extend(pass_by(&review.reports).as_object().unwrap().clone());
    let result = StageOutcome::Completed {
        summary: "Confirmed".into(),
        structured: Some(built),
    };
    review.h.settle_running(|_| result.clone()).await;
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
    let review = Loop::at_cap(false).await;
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
    let mut review = Loop::at_cap(true).await;
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
    let mut review = Loop::at_cap(true).await;
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
    let review = Loop::at_cap(true).await;
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
