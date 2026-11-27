//! Review rounds end: a re-review's new test gaps are follow-ups, a defect
//! still blocks, and a capped lead is never left waiting on Archon.

use archon_core::run::{StageRun, StageState, review_tally_key};
use serde_json::{Value, json};

use crate::common::Harness;
use crate::common::delivery::{change, record};
use crate::executor::adaptive_item;
use crate::review_rounds::{
    Fixes, Loop, fail, findings_notes, finish, handle, read_tally, reporter,
};

fn rounds(tally: &Value) -> usize {
    tally["fails"].as_array().map_or(0, Vec::len)
}

/// Each fix round's re-review finds one more test to ask for, as small models
/// did until the stage timed out. The gaps become follow-ups: the defect the
/// second round found is fixed, the third round passes and the change ships.
#[tokio::test]
async fn successive_new_test_gaps_converge_as_follow_ups() {
    let h = Harness::new().await;
    adaptive_item(&h).await;
    let lead = h.running().await.remove(0);
    let workspace = h.ctx.store.lock().await.run(&lead.run).unwrap().workspace;
    let first = change(&workspace);
    let whole = fail(
        &h,
        &lead,
        &first["commit"],
        None,
        json!([{"title": "lock held across await", "blocking": true}]),
    );
    let first_id = format!("{}:1", whole.agent.as_ref().unwrap());
    handle(&h, &whole).await;

    let second = change(&workspace);
    let gap_and_defect = reporter(
        &h,
        &lead,
        json!({
            "verdict": "fail", "head": second["commit"], "base": first["commit"],
            "resolved_findings": [first_id],
            "findings": [
                {"title": "the fix releases the lock twice", "blocking": true},
                {"title": "no test for the lock timeout", "blocking": true, "category": "test-gap"}
            ]
        }),
    );
    let defect_id = format!("{}:1", gap_and_defect.agent.as_ref().unwrap());
    let first_gap = format!("{}:2", gap_and_defect.agent.as_ref().unwrap());
    handle(&h, &gap_and_defect).await;
    assert_eq!(
        rounds(&read_tally(&h, &lead).await),
        2,
        "the defect is a round"
    );
    let notes = findings_notes(&h);
    let last = notes.last().expect("the open defect is told");
    assert!(last.contains(&defect_id), "{last}");
    assert!(
        !last.contains(&first_gap),
        "a new test gap is no blocker: {last}"
    );

    let third = change(&workspace);
    let gap_only = reporter(
        &h,
        &lead,
        json!({
            "verdict": "fail", "head": third["commit"], "base": second["commit"],
            "resolved_findings": [defect_id],
            "findings": [
                {"title": "no test for a poisoned lock", "blocking": true, "category": "Test-Gap"}
            ]
        }),
    );
    let second_gap = format!("{}:1", gap_only.agent.as_ref().unwrap());
    handle(&h, &gap_only).await;
    assert_eq!(
        rounds(&read_tally(&h, &lead).await),
        2,
        "a gap adds no round"
    );
    assert_eq!(
        findings_notes(&h).len(),
        notes.len(),
        "nothing is open to tell"
    );

    let reviewers = [whole, gap_and_defect, gap_only].map(|e| e.agent.unwrap());
    finish(&h, &record(&workspace), &reviewers).await;
    let stage = stage_of(&h, &lead).await;
    assert_eq!(stage.state, StageState::Completed, "the change ships");
    let findings = &stage.structured.unwrap()["review_rounds"]["findings"];
    for gap in [&first_gap, &second_gap] {
        assert_eq!(
            findings[gap]["blocking"], false,
            "{gap} is a follow-up: {findings}"
        );
    }
    assert_eq!(findings[&defect_id]["blocking"], true, "{findings}");
}

/// A re-review's new defect blocks, and so does an earlier ID it still
/// lists, whatever category it gives it.
#[tokio::test]
async fn a_defect_or_an_open_earlier_id_in_a_re_review_still_blocks() {
    let h = Harness::new().await;
    adaptive_item(&h).await;
    let lead = h.running().await.remove(0);
    let workspace = h.ctx.store.lock().await.run(&lead.run).unwrap().workspace;
    let first = change(&workspace);
    let whole = fail(
        &h,
        &lead,
        &first["commit"],
        None,
        json!([{"title": "no test for concurrent writers", "blocking": true, "category": "test-gap"}]),
    );
    let first_id = format!("{}:1", whole.agent.as_ref().unwrap());
    handle(&h, &whole).await;
    assert_eq!(
        rounds(&read_tally(&h, &lead).await),
        1,
        "a first read's gap blocks"
    );

    let second = change(&workspace);
    let still_open = reporter(
        &h,
        &lead,
        json!({
            "verdict": "fail", "head": second["commit"], "base": first["commit"],
            "findings": [
                {"title": "deadlock when both writers retry", "blocking": true, "category": "defect"},
                {"title": "writers test still missing", "blocking": true, "category": "test-gap", "id": first_id}
            ]
        }),
    );
    let defect_id = format!("{}:1", still_open.agent.as_ref().unwrap());
    handle(&h, &still_open).await;
    assert_eq!(
        rounds(&read_tally(&h, &lead).await),
        2,
        "the re-review is a round"
    );
    let notes = findings_notes(&h);
    let last = notes.last().unwrap();
    assert!(
        last.contains(&defect_id) && last.contains(&first_id),
        "{last}"
    );
    assert_eq!(stage_of(&h, &lead).await.state, StageState::Running);
}

/// Inbox report: a capped lead said it was "waiting for Archon's one final
/// confirmation request" and sat idle for hours. Archon names the trailers it
/// still owes, then ends the stage itself once the bound passes.
#[tokio::test]
async fn a_capped_lead_missing_trailers_is_told_and_then_ended() {
    let review = Loop::at_cap(Fixes::OnlyOne).await;
    let owed = format!("trailer for {}", review.new_id);
    assert!(
        review
            .h
            .backend
            .messages()
            .iter()
            .any(|(_, note)| note.contains(&owed)),
        "the lead is told which trailer it owes"
    );
    review.h.dispatch().await;
    assert_eq!(
        review.stage().await.state,
        StageState::Running,
        "within the bound"
    );
    age_lead_wait(&review.h, &review.lead).await;
    review.h.dispatch().await;
    let StageState::Failed { error } = review.stage().await.state else {
        panic!("the sweep ends a stage left waiting");
    };
    assert!(error.starts_with("ESCALATE:"), "{error}");
    assert!(
        error.contains("no commit carried a `Review-Fix` trailer"),
        "{error}"
    );
    assert!(error.contains(&review.new_id), "{error}");
}

/// Fixes committed and the confirmation instructions sent, but no reviewer
/// follows: the bound starts again at the instructions and then ends the stage.
#[tokio::test]
async fn a_capped_lead_that_never_delegates_the_confirmation_is_ended() {
    let review = Loop::at_cap(Fixes::All).await;
    let told = review.h.backend.messages();
    assert!(
        told.iter()
            .any(|(_, note)| note.contains("exactly one reviewer"))
    );
    review.h.dispatch().await;
    assert_eq!(review.stage().await.state, StageState::Running);
    let tally = read_tally(&review.h, &review.lead).await;
    assert!(
        tally["fix_wait"].is_null(),
        "the instructions were delivered"
    );
    age_lead_wait(&review.h, &review.lead).await;
    review.h.dispatch().await;
    let StageState::Failed { error } = review.stage().await.state else {
        panic!("the sweep ends a stage left waiting");
    };
    assert!(
        error.contains("no confirmation reviewer passed or failed"),
        "{error}"
    );
    assert_eq!(
        review.h.backend.messages().len(),
        told.len(),
        "the sweep resends no instructions"
    );
}

async fn stage_of(h: &Harness, lead: &StageRun) -> StageRun {
    h.ctx.store.lock().await.stage_run(&lead.id).unwrap()
}

/// Move the cap's wait on the lead past the bound, as if it had sat idle.
async fn age_lead_wait(h: &Harness, lead: &StageRun) {
    let mut tally = read_tally(h, lead).await;
    assert!(
        tally["lead_wait"].is_string(),
        "the cap waits on the lead: {tally}"
    );
    let minutes = archon_pipeline::contract::REVIEW_CAP_WAIT_MINUTES + 1;
    let since = archon_core::now() - chrono::Duration::minutes(minutes);
    tally["lead_wait"] = json!(since);
    let key = review_tally_key(&lead.id);
    let mut store = h.ctx.store.lock().await;
    store.kv_set(&key, &tally.to_string()).unwrap();
}
