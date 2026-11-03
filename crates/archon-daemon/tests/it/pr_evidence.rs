//! The summary and evidence of every pull request archond delivers
//! (ARCHITECTURE §8.7): the body opens with the risk level and what changed,
//! then the evidence, with screenshot files uploaded to a ref of their own
//! and never to the run branch; a screenshot that cannot be attached is
//! told, and the delivery still succeeds.

use std::path::{Path, PathBuf};

use crate::common;

use archon_core::model::ModelPlan;
use archon_core::queue::{QueueItem, QueueState};
use common::delivery::{FakeGh, add_origin, shipped_by};
use common::{Harness, REVIEWED, REVIEWED_TOML, git};
use serde_json::{Value, json};

const PNG: &[u8] = b"\x89PNG\r\n\x1a\n";

async fn delivering() -> (Harness, FakeGh, PathBuf) {
    let h = Harness::new().await;
    h.add_pipeline(REVIEWED, REVIEWED_TOML);
    let origin = add_origin(&h);
    let gh = FakeGh::install(&h);
    (h, gh, origin)
}

/// A `direct` item whose agent leaves `files` (name, bytes) in
/// `.archon/evidence/` of its worktree, uncommitted, and completes with its
/// ship record plus `extra`; the dispatcher then delivers it. With `blocked`,
/// that origin already holds a ref where the item's evidence ref would go.
async fn delivered(
    h: &Harness,
    files: &[(&str, Vec<u8>)],
    extra: Value,
    blocked: Option<&Path>,
) -> QueueItem {
    let item = h
        .enqueue_in("direct", "a visible change", ModelPlan::default())
        .await
        .expect("enqueue");
    if let Some(origin) = blocked {
        let head = git(origin, &["rev-parse", "HEAD"]);
        let in_the_way = format!("refs/archon/evidence/{}", item.id);
        git(origin, &["update-ref", &in_the_way, head.trim()]);
    }
    h.dispatch().await;
    let stage = h.running().await.remove(0);
    let workspace = h.run(&item.id).await.workspace;
    let shots = workspace.join(".archon/evidence");
    std::fs::create_dir_all(&shots).expect("evidence dir");
    for (name, bytes) in files {
        std::fs::write(shots.join(name), bytes).expect("screenshot");
    }
    let outcome = shipped_by(h, &stage, "Changed it; cargo test passes", extra).await;
    h.settle_running(|_| outcome.clone()).await;
    h.dispatch().await;
    item
}

fn image(text: &str) -> Vec<u8> {
    [PNG, text.as_bytes()].concat()
}

fn record_extra(evidence: Value) -> Value {
    json!({
        "pr": {"title": "Show each item's risk", "body": "Adds a chip to the queue row.",
               "summary": "made the queue show each item's risk. before: none → now: a chip."},
        "risk": {"level": "medium", "why": "A new element on one screen; cargo test passes."},
        "evidence": evidence,
    })
}

fn screenshots() -> Value {
    json!({"kind": "screenshot", "caption": "Queue screen",
           "before": ".archon/evidence/before.png", "after": ".archon/evidence/after.png"})
}

async fn summary(h: &Harness, item: &QueueItem) -> String {
    match h.state(&item.id).await {
        QueueState::Succeeded { summary, .. } => summary,
        other => panic!("the item ships: {other:?}"),
    }
}

/// The evidence refs `origin` holds: `(name, commit)`.
fn evidence_refs(origin: &Path) -> Vec<(String, String)> {
    let listed = git(
        origin,
        &[
            "for-each-ref",
            "--format=%(refname) %(objectname)",
            "refs/archon/evidence/",
        ],
    );
    listed
        .lines()
        .map(|line| {
            let (name, commit) = line.split_once(' ').expect("two fields");
            (name.to_owned(), commit.to_owned())
        })
        .collect()
}

async fn assert_uploaded(h: &Harness, item: &QueueItem, origin: &Path) {
    let refs = evidence_refs(origin);
    assert_eq!(refs.len(), 1, "{refs:?}");
    let (name, commit) = &refs[0];
    assert_eq!(name, &format!("refs/archon/evidence/{}/{commit}", item.id));
    assert_eq!(
        git(origin, &["ls-tree", "--name-only", commit]),
        "1.png\n2.png"
    );
    // The run worktree is gone by now: its evidence files are no work to keep.
    let before = origin.with_file_name("before.png");
    std::fs::write(&before, image("before")).expect("a copy of the screenshot");
    let blob = git(origin, &["rev-parse", &format!("{commit}:1.png")]);
    assert_eq!(
        blob,
        git(origin, &["hash-object", before.to_str().unwrap()])
    );
    let branch = common::delivery::pushed(origin, &h.run(&item.id).await.branch).unwrap();
    let shipped = git(origin, &["ls-tree", "-r", "--name-only", &branch]);
    assert!(
        !shipped.contains(".archon/evidence"),
        "never on the branch: {shipped}"
    );
}

#[tokio::test]
async fn a_delivery_uploads_evidence_and_does_not_invent_github_links_for_a_local_origin() {
    let (h, gh, origin) = delivering().await;
    let files = [
        ("before.png", image("before")),
        ("after.png", image("after")),
        ("fake.png", b"not an image".to_vec()),
    ];
    let evidence = json!([
        screenshots(),
        {"kind": "table", "caption": "queue render", "columns": ["", "before", "after"],
         "rows": [["p50 ms", "31", "30"]]},
        {"kind": "screenshot", "caption": "Detail", "before": "/home/me/x.png",
         "after": ".archon/evidence/fake.png"},
        {"kind": "video", "caption": "demo"},
    ]);
    let item = delivered(&h, &files, record_extra(evidence), None).await;

    let said = summary(&h, &item).await;
    assert!(
        said.contains("RISK: medium, labelled `risk: medium`"),
        "{said}"
    );
    assert!(
        said.contains("; 3 screenshot(s) could not be attached"),
        "{said}"
    );
    assert_uploaded(&h, &item, &origin).await;

    let (_, body) = gh.pr_labels(1);
    let top = "<!-- archon-summary -->\nRisk level: medium\n\n## What changed\n\n\
               made the queue show each item's risk. before: none → now: a chip.\n";
    assert!(body.starts_with(top), "{body}");
    for said in [
        "| _The screenshot could not be attached: the evidence push destination is not a GitHub repository_ |".to_owned(),
        "| p50 ms | 31 | 30 |".to_owned(),
        "**Detail**\n\n_The screenshot could not be attached: it is not a PNG".to_owned(),
        "_Note: `before` left out: `/home/me/x.png` is a local path".to_owned(),
        "- entry 4: unknown kind `video`".to_owned(),
        "## Technical detail\n\nAdds a chip to the queue row.".to_owned(),
    ] {
        assert!(body.contains(&said), "missing {said:?} in {body}");
    }
    assert!(!body.contains("https://github.com/o/r/blob/"), "{body}");
    let at = |part: &str| {
        body.find(part)
            .unwrap_or_else(|| panic!("{part} in {body}"))
    };
    assert!(at("## What changed") < at("## Evidence"));
    assert!(at("## Evidence") < at("## Technical detail"));
    assert!(at("## Technical detail") < at("## Risk: medium"));
}

/// The evidence ref cannot be written: the pull request still ships, and
/// the section says the screenshots could not be attached.
#[tokio::test]
async fn screenshots_that_cannot_be_hosted_are_told_and_the_delivery_still_succeeds() {
    let (h, gh, origin) = delivering().await;
    let files = [("before.png", image("b")), ("after.png", image("a"))];
    let extra = record_extra(json!([screenshots()]));
    let item = delivered(&h, &files, extra, Some(&origin)).await;

    let said = summary(&h, &item).await;
    assert!(
        said.contains("; 2 screenshot(s) could not be attached"),
        "{said}"
    );
    assert!(
        said.lines()
            .last()
            .unwrap()
            .starts_with("SHIPPED: https://github.com/o/r/pull/1")
    );
    let (_, body) = gh.pr_labels(1);
    let failed = "| _The screenshot could not be attached: `origin` did not take";
    assert!(body.contains(failed), "{body}");
    assert!(!body.contains("![Queue screen"), "{body}");
}

/// A docs change says why it shows nothing, and a record without a summary
/// or evidence is told as such, never silently left out.
#[tokio::test]
async fn no_evidence_and_a_missing_summary_are_said() {
    let (h, gh, origin) = delivering().await;
    let none = json!({"pr": {"title": "Document it", "body": "Docs only."},
        "risk": {"level": "low", "why": "Docs only."},
        "evidence": {"none": "docs only; nothing runs differently"}});
    let item = delivered(&h, &[], none, None).await;

    summary(&h, &item).await;
    assert!(evidence_refs(&origin).is_empty(), "nothing to upload");
    let (_, body) = gh.pr_labels(1);
    for said in [
        "_No high-level summary: `implement` gave no `pr.summary`._",
        "No before/after evidence: docs only; nothing runs differently",
    ] {
        assert!(body.contains(said), "missing {said:?} in {body}");
    }
}

#[tokio::test]
async fn an_oversized_existing_body_keeps_evidence_and_saves_the_full_text_in_the_summary() {
    let (h, gh, _) = delivering().await;
    let item = h
        .enqueue_in("direct", "fit the body", ModelPlan::default())
        .await
        .unwrap();
    h.dispatch().await;
    let stage = h.running().await.remove(0);
    let evidence = json!([{"kind": "table", "caption": "Behaviour", "columns": ["before", "after"],
        "rows": [["missing sections", "visible evidence"]]}]);
    let outcome = shipped_by(&h, &stage, "Changed it", record_extra(evidence)).await;
    let branch = h.run(&item.id).await.branch;
    gh.open(&branch, "main");
    let detail = format!(
        "## Technical detail\n\n{}\nFull text ends here.",
        "x".repeat(60_000)
    );
    gh.label_pr(7, &[], &detail);
    h.settle_running(|_| outcome.clone()).await;
    h.dispatch().await;

    let said = summary(&h, &item).await;
    let (_, body) = gh.pr_labels(7);
    assert!(body.chars().count() <= 60_000);
    assert!(body.starts_with("<!-- archon-summary -->\nRisk level: medium"));
    assert!(body.contains("| missing sections | visible evidence |"));
    assert!(body.contains("(truncated:") && body.contains("full text in the run summary)"));
    assert!(!body.contains("Full text ends here."));
    assert!(said.contains(&detail), "the omitted text is recoverable");
    assert!(!said.contains("sections could not be written"));
}
