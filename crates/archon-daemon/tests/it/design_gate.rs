//! Live design sessions at the gate (design.md §8.6, T-I6, T-I9..T-I11): the
//! ledger is built from archond's records, never from the agent's output.

use crate::common::{self, Harness};
use archon_core::autonomy::{AutonomyPatch, ProposalMode};
use archon_core::config::Config;
use archon_core::design::{
    AssetSource, DesignAsset, DesignAssetId, DesignSession, FinishChoices, NoteKind, NoteStatus,
    SessionState, Viewport, ViewportPreset,
};
use archon_core::error::ArchonError;
use archon_core::id::QueueItemId;
use archon_core::intent::{ApprovedIntent, IntentBinding};
use archon_core::protocol::{ItemPatch, Reply, Request};
use archon_core::queue::QueueItem;
use archon_core::run::{GateSubject, OperatorGate, PipelineRun, StageOutcome};
use archon_queue::store::{FinishRecord, NewDesignNote, NewDesignSession, NoteChange};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

const PNG: &[u8] = &[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];

fn live_config() -> Config {
    let mut config = Config::default();
    config.review.auto_repair = false;
    config.learning.explore_rate = 0.0;
    config.design.enabled = true;
    config
}

pub(super) fn live() -> AutonomyPatch {
    AutonomyPatch {
        proposal: Some(ProposalMode::Live),
        ..AutonomyPatch::default()
    }
}

async fn enqueue(
    h: &Harness,
    autonomy: AutonomyPatch,
    intent: Option<IntentBinding>,
) -> Result<QueueItem, ArchonError> {
    let mut request = common::enqueue_request(
        common::REPO_NAME,
        common::PIPELINE,
        "restyle",
        common::plan(),
    );
    request.autonomy = autonomy;
    request.intent = intent;
    match h.cli(Request::Enqueue(request)).await? {
        Reply::Item(item) => Ok(item),
        other => panic!("enqueue replied {other:?}"),
    }
}

/// A live item with its design stage running, and the session it owns.
pub(super) async fn designing(h: &Harness, item: &QueueItem) -> (PipelineRun, DesignSession) {
    h.ctx
        .design
        .set_factory(std::sync::Arc::new(common::design::FakeFactory::default()));
    h.dispatch().await;
    let running = h.running().await;
    assert_eq!(
        running[0].stage_id, "design",
        "the design stage comes first"
    );
    let run = h.run(&item.id).await;
    let mut store = h.ctx.store.lock().await;
    let session = match store.design_current_session(&item.id).unwrap() {
        Some(session) => session,
        None => store
            .design_session_create(NewDesignSession {
                item: item.id.clone(),
                run: run.id.clone(),
                stage_id: running[0].stage_id.clone(),
                attempt: running[0].attempt,
                viewport_preset: ViewportPreset::Desktop,
                sandbox_off: false,
                agent: running[0].aether_agent.clone(),
            })
            .unwrap(),
    };
    // The agent's first preview report makes a session live; notes need one.
    let session = if session.state == SessionState::Starting {
        store
            .design_session_transition(
                &session.id,
                SessionState::Starting,
                SessionState::Live,
                None,
                false,
            )
            .unwrap()
    } else {
        session
    };
    (run, session)
}

fn commit(run: &PipelineRun, file: &str, body: &str) -> String {
    std::fs::write(run.workspace.join(file), body).unwrap();
    common::git(&run.workspace, &["add", "-A"]);
    common::git(
        &run.workspace,
        &["commit", "-m", &format!("design: {file}")],
    );
    common::git(&run.workspace, &["rev-parse", "HEAD"])
}

/// What a done note can carry besides its words and commit.
#[derive(Default)]
struct Pictures<'a> {
    before: Option<&'a [u8]>,
    after: Option<&'a [u8]>,
    fin: Option<&'a [u8]>,
    /// The element the operator pointed at, by its accessible name.
    pointed_at: Option<&'a str>,
    /// What the agent answered on the note.
    reply: Option<&'a str>,
}

fn stored(
    h: &Harness,
    session: &DesignSession,
    note: &str,
    commit: &str,
    source: AssetSource,
    picture: &[u8],
) -> DesignAsset {
    let sha256 = Sha256::digest(picture)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();
    let mut asset = DesignAsset {
        id: DesignAssetId::new(),
        item: session.item.clone(),
        sha256,
        mime: "image/png".into(),
        bytes: picture.len() as u64,
        width: 320,
        height: 200,
        path: String::new(),
        source,
        target: None,
        note: Some(note.to_owned()),
        commit: Some(commit.to_owned()),
        created_at: archon_core::now(),
    };
    let path = h.ctx.design.asset_path(&asset);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, picture).unwrap();
    asset.path = path.display().to_string();
    asset
}

fn target_named(name: &str) -> archon_core::design::Target {
    archon_core::design::Target {
        path: "/overview".into(),
        selector: None,
        role_name: Some(name.to_owned()),
        component: None,
        rect: archon_core::design::Rect {
            x: 10.0,
            y: 10.0,
            w: 200.0,
            h: 80.0,
        },
        viewport: Viewport {
            w: 1440,
            h: 900,
            preset: ViewportPreset::Desktop,
        },
        styles: BTreeMap::new(),
        region: false,
        steps: Box::default(),
    }
}

/// A done change note with its commit and pictures, as the agent loop leaves it.
async fn note_with(
    h: &Harness,
    session: &DesignSession,
    run: &PipelineRun,
    text: &str,
    pictures: Pictures<'_>,
) -> (String, String) {
    let file = format!("{}.txt", text.replace(' ', "-"));
    let sha = commit(run, &file, text);
    let mut store = h.ctx.store.lock().await;
    // The before picture is taken when the note is made, before its commit.
    let before = pictures.before.map(|bytes| {
        let asset = stored(h, session, "", &sha, AssetSource::Capture, bytes);
        store.design_asset_put(asset).unwrap().id
    });
    let (note, _) = store
        .design_note_add(NewDesignNote {
            item: session.item.clone(),
            session: session.id.clone(),
            kind: NoteKind::Change,
            text: text.into(),
            details_text: None,
            send_picture: false,
            target: pictures.pointed_at.map(target_named),
            viewport: Viewport {
                w: 1440,
                h: 900,
                preset: ViewportPreset::Desktop,
            },
            client_key: format!("key-{text}"),
            reverts: None,
            resent_from: None,
            follows: None,
            refs: Vec::new(),
            before_asset: before,
            created_by_device: None,
        })
        .unwrap();
    let base = common::git(&run.workspace, &["rev-parse", &format!("{sha}^")]);
    for change in [
        NoteChange::Sent {
            base_commit: base,
            delivery: 1,
        },
        NoteChange::Status {
            to: NoteStatus::Done,
            fail_reason: None,
            commit: Some(sha.clone()),
            no_change: false,
        },
    ] {
        store
            .design_note_change(&session.item, &note.id, change)
            .unwrap();
    }
    if let Some(reply) = pictures.reply {
        let entry = archon_core::design::ThreadEntry {
            at: archon_core::now(),
            from: archon_core::design::ReplyAuthor::Agent,
            text: reply.to_owned(),
        };
        store
            .design_note_change(&session.item, &note.id, NoteChange::Thread(entry))
            .unwrap();
    }
    if let Some(bytes) = pictures.after {
        let asset = stored(h, session, &note.id, &sha, AssetSource::After, bytes);
        let asset = store.design_asset_put(asset).unwrap();
        store
            .design_note_change(
                &session.item,
                &note.id,
                NoteChange::After {
                    asset: asset.id,
                    captured_commit: sha.clone(),
                },
            )
            .unwrap();
    }
    if let Some(bytes) = pictures.fin {
        let asset = stored(h, session, &note.id, &sha, AssetSource::Final, bytes);
        let asset = store.design_asset_put(asset).unwrap();
        store
            .design_note_change(
                &session.item,
                &note.id,
                NoteChange::Final {
                    asset: Some(asset.id),
                    included: Some(true),
                    captured_commit: Some(sha.clone()),
                },
            )
            .unwrap();
    }
    (note.id, sha)
}

/// A done change note with its commit and final picture.
pub(super) async fn done_note(
    h: &Harness,
    session: &DesignSession,
    run: &PipelineRun,
    text: &str,
    picture: &[u8],
) -> (String, String) {
    let mut bytes = PNG.to_vec();
    bytes.extend_from_slice(picture);
    let pictures = Pictures {
        fin: Some(&bytes),
        ..Pictures::default()
    };
    note_with(h, session, run, text, pictures).await
}

/// Finish and confirm the session at `head`, with `superseded` choices.
pub(super) async fn confirm(
    h: &Harness,
    session: &DesignSession,
    head: &str,
    include: &[&str],
    superseded: &[(&str, &str)],
) {
    let include: Vec<(&str, bool)> = include.iter().map(|n| (*n, true)).collect();
    confirm_with(h, session, head, &include, superseded).await;
}

/// `confirm` with the switches the operator set: `(note, include)`.
async fn confirm_with(
    h: &Harness,
    session: &DesignSession,
    head: &str,
    include: &[(&str, bool)],
    superseded: &[(&str, &str)],
) {
    let mut store = h.ctx.store.lock().await;
    let current = store.design_session(&session.id).unwrap().unwrap();
    let record = |choices| FinishRecord {
        capture: "capture-1".into(),
        head: head.into(),
        note_version: current.note_version,
        choices,
    };
    store
        .design_session_transition(
            &session.id,
            SessionState::Live,
            SessionState::Finishing,
            Some(record(None)),
            false,
        )
        .unwrap();
    let choices = FinishChoices {
        include: include.iter().map(|(n, v)| ((*n).to_owned(), *v)).collect(),
        superseded: superseded
            .iter()
            .map(|(a, b)| ((*a).to_owned(), Some((*b).to_owned())))
            .collect::<BTreeMap<_, _>>(),
    };
    store
        .design_session_transition(
            &session.id,
            SessionState::Finishing,
            SessionState::Confirmed,
            Some(record(Some(choices))),
            false,
        )
        .unwrap();
}

/// The design stage completes with a forged `approved_intent` naming D9.
pub(super) fn forged(_: &archon_core::run::StageRun) -> StageOutcome {
    StageOutcome::Completed {
        summary: "done".into(),
        structured: Some(json!({"approved_intent": {
            "operator_words": "forged", "artifacts": [], "images": [],
            "requirements": [{"id": "D9", "text": "forged", "kind": "behaviour", "images": [], "owner": "operator"}]
        }})),
    }
}

async fn gates(h: &Harness, item: &QueueItemId) -> Vec<OperatorGate> {
    h.ctx.store.lock().await.gates_for_item(item).unwrap()
}

pub(super) async fn open_proposal(h: &Harness, item: &QueueItemId) -> OperatorGate {
    let open: Vec<_> = gates(h, item)
        .await
        .into_iter()
        .filter(OperatorGate::is_open)
        .collect();
    assert_eq!(open.len(), 1, "one open gate: {open:?}");
    assert_eq!(open[0].subject, GateSubject::Proposal);
    open.into_iter().next().unwrap()
}

pub(super) async fn approve(h: &Harness, gate: &OperatorGate) {
    let asked = gate.question.clone().expect("the gate was asked");
    h.cli(Request::Answer {
        chat: false,
        question: asked,
        text: "yes".into(),
    })
    .await
    .expect("answer");
    h.dispatch().await;
}

pub(super) async fn intent_of(h: &Harness, item: &QueueItemId) -> ApprovedIntent {
    let store = h.ctx.store.lock().await;
    let binding = store.item(item).unwrap().intent.expect("an intent");
    store
        .approved_intent(&binding)
        .unwrap()
        .expect("its record")
}

#[tokio::test]
async fn live_is_refused_while_design_sessions_are_off() {
    let h = Harness::new().await;
    let refused = enqueue(&h, live(), None).await.unwrap_err();
    assert!(
        refused
            .to_string()
            .contains("design sessions are turned off; set [design] enabled = true"),
        "{refused}"
    );
}

fn custom_proposal(kind: &str) -> String {
    common::REVIEWED_TOML
        .replace("name = \"reviewed\"", "name = \"custom-proposal\"")
        .replacen(
            "[[stage]]",
            &format!("[[stage]]\nid = \"custom-first\"\nkind = \"{kind}\"\nprompt = \"implement.md\"\n\n[[stage]]"),
            1,
        )
}

#[tokio::test]
async fn custom_proposal_kind_must_agree_with_live_mode_and_enablement() {
    for (kind, enabled, proposal, reason) in [
        (
            "design",
            true,
            ProposalMode::Off,
            Some("needs proposal = live"),
        ),
        (
            "design",
            true,
            ProposalMode::Lavish,
            Some("needs proposal = live"),
        ),
        (
            "design",
            false,
            ProposalMode::Live,
            Some("design sessions are turned off"),
        ),
        (
            "propose",
            true,
            ProposalMode::Live,
            Some("declares a written propose stage"),
        ),
        ("design", true, ProposalMode::Live, None),
    ] {
        let mut config = live_config();
        config.design.enabled = enabled;
        let h = Harness::with_config(config).await;
        h.add_pipeline("custom-proposal", &custom_proposal(kind));
        let mut plan = common::plan();
        plan.0.remove("ship");
        plan.0.get_mut("review").unwrap().truncate(1);
        plan.0
            .insert("custom-first".into(), vec![plan.0["implement"][0].clone()]);
        let mut request =
            common::enqueue_request(common::REPO_NAME, "custom-proposal", "restyle", plan);
        request.autonomy.proposal = Some(proposal);
        let reply = h.cli(Request::Enqueue(request)).await;
        if let Some(reason) = reason {
            let refused = reply.unwrap_err().to_string();
            assert!(refused.contains(reason), "{refused}");
            assert!(h.backend.requests().is_empty());
        } else {
            let Reply::Item(item) = reply.unwrap() else {
                panic!("enqueued item")
            };
            h.dispatch().await;
            let running = h.running().await;
            assert_eq!(running[0].stage_id, "custom-first");
            assert!(item.model_plan.0.contains_key("intent-acceptance"));
            let store = h.ctx.store.lock().await;
            let session = store
                .design_current_session(&item.id)
                .unwrap()
                .expect("custom session");
            assert_eq!(session.stage_id, "custom-first");
        }
    }
}

#[tokio::test]
async fn live_cannot_be_chosen_or_dropped_on_an_existing_item() {
    let h = Harness::with_config(live_config()).await;
    let plain = enqueue(&h, AutonomyPatch::default(), None).await.unwrap();
    let update = |item: &QueueItem, list: &str| Request::UpdateItem {
        item: item.id.clone(),
        patch: ItemPatch {
            autonomy: Some(AutonomyPatch::parse_list(list).unwrap()),
            ..ItemPatch::default()
        },
    };
    let to = h.cli(update(&plain, "proposal=live")).await.unwrap_err();
    assert!(
        to.to_string()
            .contains("choose live when creating the item"),
        "{to}"
    );
    let item = enqueue(&h, live(), None).await.unwrap();
    assert_eq!(item.autonomy.proposal, ProposalMode::Live);
    let from = h.cli(update(&item, "proposal=off")).await.unwrap_err();
    assert!(
        from.to_string()
            .contains("choose live when creating the item"),
        "{from}"
    );
}

/// T-I9: a fresh live item through the real enqueue and answer handlers.
#[tokio::test]
async fn a_fresh_live_item_is_approved_from_archonds_records_not_the_agents() {
    let h = Harness::with_config(live_config()).await;
    let item = enqueue(&h, live(), None).await.unwrap();
    assert!(item.model_plan.0.contains_key("design"));
    assert!(item.model_plan.0.contains_key("intent-acceptance"));
    let (run, session) = designing(&h, &item).await;
    let (_, head) = done_note(&h, &session, &run, "make it green", b"green").await;
    confirm(&h, &session, &head, &["D1"], &[]).await;
    h.settle_running(forged).await;
    h.dispatch().await;
    let gate = open_proposal(&h, &item.id).await;
    assert!(gate.report.is_some(), "the ledger page was published");
    let page = std::path::PathBuf::from(gate.page.clone().unwrap());
    assert!(page.starts_with(h.ctx.paths.data_dir.join("design").join("ledger")));
    let directory = page.parent().unwrap();
    assert_eq!(
        std::fs::read_dir(directory).unwrap().count(),
        1,
        "publication snapshots only this page"
    );
    let html = std::fs::read_to_string(&page).unwrap();
    assert!(html.contains("D1") && !html.contains("D9") && !html.contains("forged"));
    approve(&h, &gate).await;
    let intent = intent_of(&h, &item.id).await;
    let ids: Vec<_> = intent.requirements.iter().map(|r| r.id.as_str()).collect();
    assert_eq!(ids, ["D1"]);
    assert_eq!(intent.images.len(), 1);
    assert_eq!(intent.images[0].id, "D1-final");
    assert_eq!(intent.requirements[0].images, ["D1-final"]);
}

#[tokio::test]
async fn a_build_past_design_recovers_when_design_is_turned_off() {
    let mut h = Harness::with_config(live_config()).await;
    let item = enqueue(&h, live(), None).await.unwrap();
    let (run, session) = designing(&h, &item).await;
    let (_, head) = done_note(&h, &session, &run, "make it green", b"green").await;
    confirm(&h, &session, &head, &["D1"], &[]).await;
    h.settle_running(forged).await;
    h.dispatch().await;
    approve(&h, &open_proposal(&h, &item.id).await).await;
    let before = h.running().await;
    assert_eq!(before.len(), 1);
    assert_eq!(before[0].stage_id, "implement");

    let mut config = h.ctx.config.clone();
    config.design.enabled = false;
    h.restart_with(config);
    archon_daemon::executor::recover(&h.ctx).await.unwrap();
    let resumed = h.run(&item.id).await;
    assert_eq!(
        resumed.state.live_stage(),
        Some(before[0].stage_id.as_str())
    );
    assert_eq!(h.running().await[0].id, before[0].id);
}

#[tokio::test]
async fn a_design_gate_recovers_but_its_revision_needs_design_enabled() {
    let mut h = Harness::with_config(live_config()).await;
    let item = enqueue(&h, live(), None).await.unwrap();
    let (run, session) = designing(&h, &item).await;
    let (_, head) = done_note(&h, &session, &run, "make it green", b"green").await;
    confirm(&h, &session, &head, &["D1"], &[]).await;
    h.settle_running(forged).await;
    h.dispatch().await;
    let gate = open_proposal(&h, &item.id).await;
    let mut config = h.ctx.config.clone();
    config.design.enabled = false;
    h.restart_with(config);
    archon_daemon::executor::recover(&h.ctx).await.unwrap();
    assert_eq!(open_proposal(&h, &item.id).await.question, gate.question);

    h.cli(Request::Answer {
        chat: false,
        question: gate.question.expect("an open proposal has a question"),
        text: "try a different layout".into(),
    })
    .await
    .unwrap();
    h.dispatch().await;
    let failed = h.run(&item.id).await;
    let archon_core::run::RunState::Failed { stage_id, error } = failed.state else {
        panic!(
            "the disabled design stage should not resume: {:?}",
            failed.state
        );
    };
    assert_eq!(stage_id, session.stage_id);
    assert!(error.contains("design sessions are turned off"), "{error}");
}

const BEFORE_PNG: &[u8] = include_bytes!("../fixtures/design/before.png");
const AFTER_PNG: &[u8] = include_bytes!("../fixtures/design/after.png");

/// The gate page is a decision page: every captured picture is on it, the
/// operator's own choice is the only reason one is withheld, and the same
/// pictures are the approved intent's.
#[tokio::test]
async fn the_gate_page_shows_before_and_after_and_withholds_only_what_the_operator_did() {
    let h = Harness::with_config(live_config()).await;
    let item = enqueue(&h, live(), None).await.unwrap();
    let (run, session) = designing(&h, &item).await;
    // D1: before + after captured, no final picture (the operator never had one to switch).
    let both = Pictures {
        before: Some(BEFORE_PNG),
        after: Some(AFTER_PNG),
        pointed_at: Some("Detections by type"),
        reply: Some(
            "The chart, tiles and legend now use purple for people and orange for vehicles.",
        ),
        ..Pictures::default()
    };
    note_with(&h, &session, &run, "use purple and orange", both).await;
    // D2: a final picture the operator switched off.
    let switched_off = Pictures {
        fin: Some(BEFORE_PNG),
        ..Pictures::default()
    };
    note_with(&h, &session, &run, "make it wider", switched_off).await;
    let shown = Pictures {
        before: Some(BEFORE_PNG),
        fin: Some(AFTER_PNG),
        ..Pictures::default()
    };
    let (_, head) = note_with(&h, &session, &run, "shrink the logo", shown).await;
    confirm_with(&h, &session, &head, &[("D2", false), ("D3", true)], &[]).await;
    h.settle_running(|_| StageOutcome::Completed {
        summary: "Two changes made".into(),
        structured: Some(json!({"checks": ["npm test (pass)"]})),
    })
    .await;
    h.dispatch().await;
    let gate = open_proposal(&h, &item.id).await;
    let html = std::fs::read_to_string(gate.page.clone().unwrap()).unwrap();
    if let Some(out) = std::env::var_os("ARCHON_EXAMPLE_PAGE_OUT") {
        std::fs::write(out, &html).unwrap();
    }
    // D1: both pictures, labelled visual, with its commit message and file.
    assert_eq!(
        html.matches("data:image/png").count(),
        4,
        "D1 before and after, D3 before and final"
    );
    assert!(html.contains("Pointed at: &quot;Detections by type&quot; on /overview"));
    assert!(html.contains("purple for people and orange for vehicles"));
    assert_eq!(html.matches("withheld by the operator").count(), 1);
    assert!(html.contains("<strong>Before</strong>") && html.contains("<strong>After</strong>"));
    assert!(html.contains("design: use-purple-and-orange.txt"));
    assert!(html.contains("use-purple-and-orange.txt</code>"));
    assert!(html.contains("npm test (pass)") && html.contains("Two changes made"));
    assert!(html.find("class=\"card\"").unwrap() < html.find("The full brief").unwrap());
    approve(&h, &gate).await;
    let intent = intent_of(&h, &item.id).await;
    let kinds: Vec<_> = intent
        .requirements
        .iter()
        .map(|r| (r.id.as_str(), r.kind.clone(), r.images.len()))
        .collect();
    use archon_core::intent::RequirementKind::{Behaviour, Visual};
    assert_eq!(
        kinds,
        [("D1", Visual, 1), ("D2", Behaviour, 0), ("D3", Visual, 1)]
    );
    let after = &intent.images[0];
    assert_eq!(after.id, "D1-final");
    assert_eq!(std::fs::read(&after.path).unwrap(), AFTER_PNG);
    // The page kept with the intent, which a later model reads, has no
    // before picture; the gate page above did.
    let kept = std::fs::read_to_string(&intent.artifacts[0].path).unwrap();
    assert_eq!(kept.matches("data:image/png").count(), 2, "D1 and D3 final");
    assert!(!kept.contains("before the change") && !kept.contains("<strong>Before</strong>"));
}

/// The second guard: without the model-plan entry a live item still takes the intent path.
#[tokio::test]
async fn a_live_item_requires_the_intent_path_without_the_model_plan_entry() {
    let h = Harness::with_config(live_config()).await;
    let item = enqueue(&h, live(), None).await.unwrap();
    {
        let mut store = h.ctx.store.lock().await;
        // The plan has no mutable accessor: drop the entry through its wire form.
        let mut wire = serde_json::to_value(store.item(&item.id).unwrap()).unwrap();
        wire["model_plan"]
            .as_object_mut()
            .expect("the item has a model plan")
            .remove("intent-acceptance");
        // Its origins name the dropped key: leave the plan unattributed.
        wire.as_object_mut().unwrap().remove("model_origins");
        let stored: QueueItem = serde_json::from_value(wire).unwrap();
        assert!(!stored.model_plan.0.contains_key("intent-acceptance"));
        store.update_item(&stored).unwrap();
    }
    let (run, session) = designing(&h, &item).await;
    let (_, head) = done_note(&h, &session, &run, "make it green", b"g").await;
    confirm(&h, &session, &head, &["D1"], &[]).await;
    h.settle_running(forged).await;
    h.dispatch().await;
    let gate = open_proposal(&h, &item.id).await;
    approve(&h, &gate).await;
    assert_eq!(intent_of(&h, &item.id).await.requirements[0].id, "D1");
}

/// T-I6: D1 "make it green" is superseded by D2 "make it red" on the same target.
#[tokio::test]
async fn a_superseded_note_survives_only_as_words_in_the_one_that_replaced_it() {
    let h = Harness::with_config(live_config()).await;
    let item = enqueue(&h, live(), None).await.unwrap();
    let (run, session) = designing(&h, &item).await;
    done_note(&h, &session, &run, "make it green", b"green").await;
    let (_, head) = done_note(&h, &session, &run, "make it red", b"red").await;
    confirm(&h, &session, &head, &["D2"], &[("D1", "D2")]).await;
    h.settle_running(forged).await;
    h.dispatch().await;
    let gate = open_proposal(&h, &item.id).await;
    approve(&h, &gate).await;
    let intent = intent_of(&h, &item.id).await;
    assert_eq!(intent.requirements.len(), 1);
    let requirement = &intent.requirements[0];
    assert_eq!(requirement.id, "D2");
    assert!(requirement.text.contains("Earlier: D1 \"make it green\""));
    assert_eq!(intent.images.len(), 1);
    assert_eq!(intent.images[0].id, "D2-final");
    let record = serde_json::to_string(&intent).unwrap();
    assert!(!record.contains("D1-final"));
    // The stage spawned after approval carries the real approved-intent prompt
    // (executor/acceptance/spawn.rs): one requirement, D2, citing D1 as earlier,
    // and only D2's final picture, as text and as an attachment.
    let request = h
        .backend
        .requests()
        .pop()
        .expect("a stage spawned after approval");
    let prompt = &request.instructions;
    assert!(
        prompt.contains("## Approved intent (binding acceptance)"),
        "{prompt}"
    );
    let (_, ledger) = prompt.split_once("Ledger: ").unwrap();
    let (ledger, _) = ledger.split_once("\nReference images").unwrap();
    let ledger: Vec<serde_json::Value> = serde_json::from_str(ledger).unwrap();
    assert_eq!(ledger.len(), 1, "{ledger:?}");
    assert_eq!(ledger[0]["id"], "D2");
    assert_eq!(ledger[0]["images"], json!(["D2-final"]));
    assert!(ledger[0]["text"].as_str().unwrap().contains("Earlier: D1"));
    assert!(!prompt.contains("D1-final"));
    let green = green_picture_sha();
    assert!(!prompt.contains(&green));
    // The harness's backend lacks reference support, so the images reach the
    // stage as the prompt's path list: exactly D2's red final picture.
    let (_, listed) = prompt
        .split_once("Reference images (open every image): ")
        .unwrap();
    let (listed, _) = listed.split_once("\n\nApproved artifacts").unwrap();
    let listed: Vec<archon_core::intent::ReferenceImage> = serde_json::from_str(listed).unwrap();
    assert_eq!(listed.len(), 1, "{listed:?}");
    assert_eq!(listed[0].id, "D2-final");
    let attached = std::fs::read(&listed[0].path).unwrap();
    assert_eq!(attached, [PNG, b"red".as_slice()].concat());
    assert!(
        request
            .references
            .iter()
            .all(|r| r.label.as_deref() != Some("D1-final"))
    );
}

/// The sha256 of the stored final picture `done_note` writes for "green".
fn green_picture_sha() -> String {
    Sha256::digest([PNG, b"green".as_slice()].concat())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// T-I11: the agent commits after the operator confirmed.
#[tokio::test]
async fn a_worktree_that_moved_after_finish_is_refused_until_it_is_restored() {
    let h = Harness::with_config(live_config()).await;
    let item = enqueue(&h, live(), None).await.unwrap();
    let (run, session) = designing(&h, &item).await;
    let (_, head) = done_note(&h, &session, &run, "make it green", b"green").await;
    confirm(&h, &session, &head, &["D1"], &[]).await;
    commit(&run, "extra.txt", "after finish");
    h.settle_running(forged).await;
    h.dispatch().await;
    assert!(gates(&h, &item.id).await.is_empty(), "no gate opened");
    common::git(&run.workspace, &["reset", "--hard", &head]);
    let running = h.running().await;
    assert!(
        running.iter().any(|s| s.stage_id == "design"),
        "the design stage goes on after the refusal"
    );
    h.settle_running(forged).await;
    h.dispatch().await;
    open_proposal(&h, &item.id).await;
}

#[tokio::test]
async fn approving_a_live_gate_rechecks_the_tree_and_the_frozen_picture() {
    let h = Harness::with_config(live_config()).await;
    let item = enqueue(&h, live(), None).await.unwrap();
    let (run, session) = designing(&h, &item).await;
    let (_, head) = done_note(&h, &session, &run, "make it green", b"green").await;
    confirm(&h, &session, &head, &["D1"], &[]).await;
    h.settle_running(forged).await;
    h.dispatch().await;
    let gate = open_proposal(&h, &item.id).await;
    let accept = || {
        archon_daemon::executor::decide_gate(
            &h.ctx,
            &gate,
            archon_core::run::GateDecision::Approve { note: None },
        )
    };
    commit(&run, "extra.txt", "after the gate opened");
    let refused = accept().await.unwrap_err().to_string();
    assert!(refused.contains("worktree moved after Finish"), "{refused}");
    assert!(h.item(&item.id).await.intent.is_none());
    common::git(&run.workspace, &["reset", "--hard", &head]);

    let image = h
        .ctx
        .paths
        .data_dir
        .join("approved-intent-artifacts")
        .join(format!("{}.png", green_picture_sha()));
    let bytes = std::fs::read(&image).unwrap();
    std::fs::write(&image, [PNG, b"red".as_slice()].concat()).unwrap();
    let refused = accept().await.unwrap_err().to_string();
    assert!(
        refused.contains("D1-final changed after review"),
        "{refused}"
    );
    assert!(open_proposal(&h, &item.id).await.is_open());
    std::fs::write(&image, bytes).unwrap();
    approve(&h, &gate).await;
    assert_eq!(intent_of(&h, &item.id).await.images[0].id, "D1-final");
}

/// T-I10: the new note appends only to the requirements this item selected.
#[tokio::test]
async fn a_live_item_with_an_earlier_intent_appends_to_it() {
    let h = Harness::with_config(live_config()).await;
    let path = h.dir.path().join("d1.png");
    std::fs::write(&path, PNG).unwrap();
    let hash = Sha256::digest(PNG)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();
    let earlier: ApprovedIntent = serde_json::from_value(json!({
        "id": "inbox1", "version": 1, "source": {"kind": "inbox", "seq": 3},
        "operator_words": "  Match the mockup\n", "artifacts": [],
        "images": [
            {"id": "D1-final", "path": path, "sha256": hash},
            {"id": "unselected", "path": path, "sha256": hash},
        ],
        "requirements": [
            {"id": "D1", "text": "Match the mockup", "kind": "visual", "owner": "operator", "images": ["D1-final"]},
            {"id": "elsewhere", "text": "Another item", "kind": "visual", "owner": "operator", "images": ["unselected"]},
        ]
    }))
    .unwrap();
    h.ctx
        .store
        .lock()
        .await
        .save_approved_intent(&earlier)
        .unwrap();
    let binding = IntentBinding {
        id: earlier.id.clone(),
        version: 1,
        requirements: vec!["D1".into()],
    };
    let item = enqueue(&h, live(), Some(binding)).await.unwrap();
    let (run, session) = designing(&h, &item).await;
    let (id, head) = done_note(&h, &session, &run, "make it red", b"red").await;
    assert_eq!(
        id, "D2",
        "the first note of the item continues the numbering"
    );
    confirm(&h, &session, &head, &["D2"], &[]).await;
    h.settle_running(forged).await;
    h.dispatch().await;
    let gate = open_proposal(&h, &item.id).await;
    approve(&h, &gate).await;
    let intent = intent_of(&h, &item.id).await;
    assert!(intent.operator_words.starts_with(&earlier.operator_words));
    let ids: Vec<_> = intent.requirements.iter().map(|r| r.id.as_str()).collect();
    assert_eq!(ids, ["D1", "D2"]);
    assert!(intent.requirements.contains(&earlier.requirements[0]));
    assert!(intent.images.contains(&earlier.images[0]));
    assert!(intent.images.iter().all(|image| image.id != "unselected"));
    assert_eq!(
        h.item(&item.id).await.intent.unwrap().requirements,
        ["D1", "D2"]
    );
}

/// A note an approval made a requirement stays bound through the approved
/// intent: it is no longer active, so a later session's ledger, which starts
/// from that intent, never adds it again ("requirement D1 already exists")
/// and no later note on the same element supersedes it.
#[tokio::test]
async fn an_approved_note_is_not_active_for_a_later_ledger() {
    let h = Harness::with_config(live_config()).await;
    let item = enqueue(&h, live(), None).await.unwrap();
    let (run, session) = designing(&h, &item).await;
    let (id, head) = done_note(&h, &session, &run, "make it green", b"green").await;
    let worktree = archon_daemon::design::WorktreeRef {
        repo: h.repo.clone(),
        worktree: run.workspace.clone(),
        branch: run.branch.clone(),
    };
    let active = |h: &Harness| {
        let store = h.ctx.store.try_lock().expect("store");
        h.ctx
            .design
            .active_notes(&store, &worktree, &item.id, &head, None)
            .unwrap()
            .into_iter()
            .map(|n| n.id)
            .collect::<Vec<_>>()
    };
    assert_eq!(
        active(&h),
        std::slice::from_ref(&id),
        "active until approved"
    );
    confirm(&h, &session, &head, &[id.as_str()], &[]).await;
    h.settle_running(forged).await;
    h.dispatch().await;
    let gate = open_proposal(&h, &item.id).await;
    approve(&h, &gate).await;
    let bound = h
        .ctx
        .store
        .lock()
        .await
        .item(&item.id)
        .unwrap()
        .intent
        .expect("approved")
        .requirements;
    assert!(bound.contains(&id), "{bound:?}");
    assert!(
        active(&h).is_empty(),
        "an approved note is not active again"
    );
    // F4: the session ends with the answer, so an open screen says
    // "Approved" and what comes next rather than waiting on the gate.
    let ended = h
        .ctx
        .store
        .lock()
        .await
        .design_session(&session.id)
        .unwrap()
        .unwrap();
    assert_eq!(ended.state, SessionState::Ended);
    assert_eq!(ended.end_reason.as_deref(), Some("approved"));
    // F2: an approved note offers no Undo in any later session.
    let store = h.ctx.store.lock().await;
    let notes = store.design_notes(&item.id).unwrap();
    let approved = archon_daemon::design::approved_ids(&store, &item.id).unwrap();
    let note = notes.iter().find(|n| n.id == id).unwrap();
    assert!(archon_daemon::design::note_view(note, &notes, None, &approved).approved);
}

/// Revise on a design gate resumes the kept agent in a new session that still
/// carries the item's notes; the old session ends.
#[tokio::test]
async fn revise_resumes_the_design_agent_in_a_new_session_with_its_notes() {
    let h = Harness::with_config(live_config()).await;
    let item = enqueue(&h, live(), None).await.unwrap();
    let (run, session) = designing(&h, &item).await;
    let agent = h.running().await[0].aether_agent.clone();
    let (_, head) = done_note(&h, &session, &run, "make it green", b"green").await;
    confirm(&h, &session, &head, &["D1"], &[]).await;
    h.settle_running(forged).await;
    h.dispatch().await;
    let gate = open_proposal(&h, &item.id).await;
    let asked = gate.question.clone().expect("asked");
    h.cli(Request::Answer {
        chat: false,
        question: asked,
        text: "a darker green please".into(),
    })
    .await
    .expect("answer");
    h.dispatch().await;
    let running = h.running().await;
    assert_eq!(
        (running[0].stage_id.as_str(), running[0].attempt),
        ("design", 2)
    );
    assert_eq!(running[0].aether_agent, agent, "the kept agent resumes");
    let store = h.ctx.store.lock().await;
    let old = store.design_session(&session.id).unwrap().unwrap();
    assert_eq!(old.state, SessionState::Ended);
    assert_eq!(
        old.end_reason.as_deref(),
        Some("revised"),
        "F2: the old screen hears it was revised, not left waiting"
    );
    let next = h
        .ctx
        .design
        .session_of(&store, &run.id, "design", 2)
        .unwrap()
        .expect("a new session");
    assert_ne!(next.id, session.id);
    assert!(matches!(
        next.state,
        SessionState::Starting | SessionState::Live
    ));
    assert_eq!(next.state, SessionState::Starting);
    assert_eq!(store.design_notes(&item.id).unwrap().len(), 1);
    drop(store);
    // The Ask for changes is the new session's first note once its preview is
    // up, so the agent makes it as a note the ledger and the gate show, never
    // as a commit outside one.
    let preview = || {
        Box::pin(archon_daemon::handlers::handle(
            &h.ctx,
            archon_core::protocol::ClientKind::ArchonMcp,
            Request::DesignPreview {
                url: "http://127.0.0.1:5173/".into(),
                caller: agent.clone(),
            },
        ))
    };
    preview().await.expect("preview reported");
    preview().await.expect("reported again");
    let mut store = h.ctx.store.lock().await;
    let notes = store.design_notes(&item.id).unwrap();
    assert_eq!(
        notes.len(),
        2,
        "one request, however often the preview is reported"
    );
    let d1 = notes.iter().find(|n| n.id == "D1").unwrap();
    assert_eq!(d1.status, NoteStatus::Done);
    let request = notes
        .iter()
        .find(|n| n.id == "D2")
        .expect("the request is a note");
    assert_eq!(request.session, next.id);
    assert_eq!(request.kind, archon_core::design::NoteKind::Page);
    assert_eq!(request.text, "a darker green please");
    assert_eq!(
        request.status,
        NoteStatus::Sent,
        "sent to the resumed agent"
    );
    // The session is lost before the agent made the change, and a fresh
    // agent retries the same attempt: its session gets the request again,
    // rather than the old session's note answering for it.
    store.design_session_end(&next.id, "test").unwrap();
    drop(store);
    let retry = Box::pin(h.ctx.design.stage_started(
        &h.ctx,
        &item.id,
        &run.id,
        "design",
        2,
        agent.as_ref().expect("an agent"),
        archon_daemon::design::WorktreeRef {
            repo: h.repo.clone(),
            worktree: run.workspace.clone(),
            branch: run.branch.clone(),
        },
    ))
    .await
    .expect("the retry's session starts");
    assert_ne!(retry, next.id);
    preview().await.expect("the retry's preview");
    let mut store = h.ctx.store.lock().await;
    let again: Vec<_> = store
        .design_notes(&item.id)
        .unwrap()
        .into_iter()
        .filter(|n| n.session == retry)
        .collect();
    assert_eq!(again.len(), 1, "{again:?}");
    assert_eq!(again[0].text, "a darker green please");
    assert_eq!(again[0].kind, archon_core::design::NoteKind::Page);
    // A session that ends before the request was delivered cancels it as
    // not delivered: that is no answer from the operator, so the next retry
    // gets it again.
    let retry_session = || {
        Box::pin(h.ctx.design.stage_started(
            &h.ctx,
            &item.id,
            &run.id,
            "design",
            2,
            agent.as_ref().expect("an agent"),
            archon_daemon::design::WorktreeRef {
                repo: h.repo.clone(),
                worktree: run.workspace.clone(),
                branch: run.branch.clone(),
            },
        ))
    };
    let cancel = |store: &mut archon_queue::Store, id: &str, why| {
        store
            .design_note_change(
                &item.id,
                id,
                NoteChange::Status {
                    to: NoteStatus::Cancelled,
                    fail_reason: why,
                    commit: None,
                    no_change: false,
                },
            )
            .unwrap();
    };
    cancel(
        &mut store,
        &again[0].id,
        Some(archon_core::design::FailReason::NotDelivered),
    );
    store.design_session_end(&retry, "test").unwrap();
    drop(store);
    let third = retry_session().await.expect("a third session starts");
    preview().await.expect("the third session's preview");
    let mut store = h.ctx.store.lock().await;
    let mine: Vec<_> = store
        .design_notes(&item.id)
        .unwrap()
        .into_iter()
        .filter(|n| n.session == third)
        .collect();
    assert_eq!(
        mine.len(),
        1,
        "an undelivered request is sent again: {mine:?}"
    );
    // The operator withdrew it there (a Finish that cancelled what was still
    // queued, say), then that session was lost too: the next retry does not
    // bring back a request the operator cancelled.
    cancel(&mut store, &mine[0].id, None);
    store.design_session_end(&third, "test").unwrap();
    drop(store);
    let fourth = retry_session().await.expect("a fourth session starts");
    preview().await.expect("the fourth session's preview");
    let store = h.ctx.store.lock().await;
    let notes = store.design_notes(&item.id).unwrap();
    assert!(
        notes.iter().all(|n| n.session != fourth),
        "a cancelled request stays cancelled: {notes:?}"
    );
}

/// A revised session that went live without its change request (a restart
/// between going live and adding it, or a store error then) gets the request
/// with the next pump, here the one an operator's note runs, rather than
/// only on a later preview report the agent may never send.
#[tokio::test]
async fn a_live_session_missing_its_change_request_gets_it_with_the_next_pump() {
    let h = Harness::with_config(live_config()).await;
    let item = enqueue(&h, live(), None).await.unwrap();
    let (run, session) = designing(&h, &item).await;
    let (_, head) = done_note(&h, &session, &run, "make it green", b"green").await;
    confirm(&h, &session, &head, &["D1"], &[]).await;
    h.settle_running(forged).await;
    h.dispatch().await;
    let gate = open_proposal(&h, &item.id).await;
    h.cli(Request::Answer {
        chat: false,
        question: gate.question.clone().expect("asked"),
        text: "a darker green please".into(),
    })
    .await
    .expect("answer");
    h.dispatch().await;
    let next = {
        let mut store = h.ctx.store.lock().await;
        let next = h
            .ctx
            .design
            .session_of(&store, &run.id, "design", 2)
            .unwrap()
            .expect("a new session");
        // Live, as its first preview made it, but the request was never added.
        store
            .design_session_transition(
                &next.id,
                SessionState::Starting,
                SessionState::Live,
                None,
                false,
            )
            .unwrap();
        assert_eq!(store.design_notes(&item.id).unwrap().len(), 1);
        next
    };
    h.cli(Request::DesignNoteAdd {
        session: next.id.clone(),
        client_key: "op-1".into(),
        text: "and a rounder button".into(),
        asset: None,
        send_picture: false,
        details_text: None,
        follows: None,
        refs: Vec::new(),
    })
    .await
    .expect("the operator's note");
    let store = h.ctx.store.lock().await;
    let mine: Vec<_> = store
        .design_notes(&item.id)
        .unwrap()
        .into_iter()
        .filter(|n| n.session == next.id)
        .collect();
    assert!(
        mine.iter()
            .any(|n| n.text == "a darker green please"
                && n.kind == archon_core::design::NoteKind::Page),
        "the request is added late, not lost: {mine:?}"
    );
}

/// E2E S5: after Revise the resumed agent completes without a preview; the
/// result is refused and the slot retried. The retry's agent opens the
/// attempt's session with its first `design_preview`, never "not live".
#[tokio::test]
async fn a_revised_design_retried_after_a_refusal_still_gets_its_preview() {
    let h = Harness::with_config(live_config()).await;
    let item = enqueue(&h, live(), None).await.unwrap();
    let (run, session) = designing(&h, &item).await;
    let (_, head) = done_note(&h, &session, &run, "make it green", b"green").await;
    confirm(&h, &session, &head, &["D1"], &[]).await;
    h.settle_running(forged).await;
    h.dispatch().await;
    let gate = open_proposal(&h, &item.id).await;
    h.cli(Request::Answer {
        chat: false,
        question: gate.question.clone().expect("asked"),
        text: "a darker green please".into(),
    })
    .await
    .expect("answer");
    h.dispatch().await;
    // The resumed agent completes on its own, with no session confirmed.
    h.settle_running(forged).await;
    h.dispatch().await;
    let running = h.running().await;
    let retry = running
        .iter()
        .find(|s| s.stage_id == "design")
        .expect("the design slot is retried");
    let agent = retry.aether_agent.clone().expect("an agent");
    // Whatever the retry's start did, its agent's first report works.
    {
        let mut store = h.ctx.store.lock().await;
        for s in store.design_sessions(&item.id).unwrap() {
            if s.state != SessionState::Ended {
                store.design_session_end(&s.id, "test").unwrap();
            }
        }
    }
    let reply = Box::pin(archon_daemon::handlers::handle(
        &h.ctx,
        archon_core::protocol::ClientKind::ArchonMcp,
        Request::DesignPreview {
            url: "http://127.0.0.1:5173/".into(),
            caller: Some(agent.clone()),
        },
    ))
    .await;
    assert!(reply.is_ok(), "{reply:?}");
    let store = h.ctx.store.lock().await;
    let now = h
        .ctx
        .design
        .session_of(&store, &run.id, "design", retry.attempt)
        .unwrap()
        .expect("the retry's session");
    assert_eq!(now.agent.as_ref(), Some(&agent));
    assert_eq!(now.state, SessionState::Live);
}

/// A first attempt whose agent completes before Confirm is refused and its
/// slot retried with a fresh agent at the same attempt: the attempt's open
/// session follows the new agent, whose design tools work at once.
#[tokio::test]
async fn a_refused_first_attempt_hands_its_session_to_the_new_agent() {
    let h = Harness::with_config(live_config()).await;
    let item = enqueue(&h, live(), None).await.unwrap();
    let (run, session) = designing(&h, &item).await;
    let first = session.agent.clone().expect("the first agent");
    h.settle_running(forged).await;
    h.dispatch().await;
    let retry = h
        .running()
        .await
        .into_iter()
        .find(|s| s.stage_id == "design")
        .expect("the design slot is retried");
    let agent = retry.aether_agent.clone().expect("an agent");
    assert_ne!(agent, first, "a fresh agent runs the retry");
    let reply = Box::pin(archon_daemon::handlers::handle(
        &h.ctx,
        archon_core::protocol::ClientKind::ArchonMcp,
        Request::DesignPreview {
            url: "http://127.0.0.1:5173/".into(),
            caller: Some(agent.clone()),
        },
    ))
    .await;
    assert!(reply.is_ok(), "{reply:?}");
    let store = h.ctx.store.lock().await;
    let now = h
        .ctx
        .design
        .session_of(&store, &run.id, "design", retry.attempt)
        .unwrap()
        .expect("the attempt's session");
    assert_eq!(
        now.agent.as_ref(),
        Some(&agent),
        "the session follows the new agent"
    );
    assert_ne!(now.state, SessionState::Ended);
}

/// A cancel while a Finish (or a note, or an undo) holds the item lock
/// settles at once: the run table never waits on the item lock, and the
/// session ends as soon as the holder lets go.
#[tokio::test]
async fn a_cancel_during_finish_does_not_stall_the_executor() {
    let h = Harness::with_config(live_config()).await;
    let item = enqueue(&h, live(), None).await.unwrap();
    let (_run, session) = designing(&h, &item).await;
    let lock = h.ctx.design.item_lock_for_tests(&item.id);
    let held = lock.clone().lock_owned().await;
    let cancel = h.cli(Request::CancelItem {
        item: item.id.clone(),
        reason: Some("changed my mind".into()),
    });
    tokio::time::timeout(std::time::Duration::from_secs(10), cancel)
        .await
        .expect("the cancel returns while the item lock is held")
        .expect("cancel");
    drop(held);
    let mut ended = false;
    for _ in 0..250 {
        let now = h
            .ctx
            .store
            .lock()
            .await
            .design_session(&session.id)
            .unwrap()
            .unwrap();
        if now.state == SessionState::Ended {
            ended = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(ended, "the session ends once the lock is free");
}

/// Item 6: cancelling while the proposal gate is open ends the confirmed session.
#[tokio::test]
async fn cancelling_with_the_gate_open_ends_the_confirmed_session() {
    let h = Harness::with_config(live_config()).await;
    let item = enqueue(&h, live(), None).await.unwrap();
    let (run, session) = designing(&h, &item).await;
    let (_, head) = done_note(&h, &session, &run, "make it green", b"green").await;
    confirm(&h, &session, &head, &["D1"], &[]).await;
    h.settle_running(forged).await;
    h.dispatch().await;
    open_proposal(&h, &item.id).await;
    let now = h
        .ctx
        .store
        .lock()
        .await
        .design_session(&session.id)
        .unwrap()
        .unwrap();
    assert_eq!(now.state, SessionState::Confirmed, "kept for the gate");
    h.cli(Request::CancelItem {
        item: item.id.clone(),
        reason: Some("no".into()),
    })
    .await
    .expect("cancel");
    let now = h
        .ctx
        .store
        .lock()
        .await
        .design_session(&session.id)
        .unwrap()
        .unwrap();
    assert_eq!(now.state, SessionState::Ended);
}

/// Item 7: a session of only "no change" notes is approvable with an empty delta.
#[tokio::test]
async fn a_no_change_review_reaches_the_gate_and_is_approved() {
    let h = Harness::with_config(live_config()).await;
    let item = enqueue(&h, live(), None).await.unwrap();
    let (run, session) = designing(&h, &item).await;
    {
        let mut store = h.ctx.store.lock().await;
        let (note, _) = store
            .design_note_add(NewDesignNote {
                item: session.item.clone(),
                session: session.id.clone(),
                kind: NoteKind::Change,
                text: "looks fine".into(),
                details_text: None,
                send_picture: false,
                target: None,
                viewport: Viewport {
                    w: 1440,
                    h: 900,
                    preset: ViewportPreset::Desktop,
                },
                client_key: "key-fine".into(),
                reverts: None,
                resent_from: None,
                follows: None,
                refs: Vec::new(),
                before_asset: None,
                created_by_device: None,
            })
            .unwrap();
        let base = common::git(&run.workspace, &["rev-parse", "HEAD"]);
        for change in [
            NoteChange::Sent {
                base_commit: base,
                delivery: 1,
            },
            NoteChange::Status {
                to: NoteStatus::Done,
                fail_reason: None,
                commit: None,
                no_change: true,
            },
        ] {
            store
                .design_note_change(&session.item, &note.id, change)
                .unwrap();
        }
    }
    let head = common::git(&run.workspace, &["rev-parse", "HEAD"]);
    confirm(&h, &session, &head, &[], &[]).await;
    h.settle_running(forged).await;
    h.dispatch().await;
    let gate = open_proposal(&h, &item.id).await;
    approve(&h, &gate).await;
    let intent = intent_of(&h, &item.id).await;
    assert_eq!(intent.requirements.len(), 1);
    assert!(intent.requirements[0].text.contains("no change"));
    assert!(intent.images.is_empty());
}

/// Item 7: a session of only "no change" notes is approvable with an empty delta.
#[tokio::test]
async fn a_no_change_review_with_a_change_gone_from_history_is_approved() {
    let h = Harness::with_config(live_config()).await;
    let item = enqueue(&h, live(), None).await.unwrap();
    let (run, session) = designing(&h, &item).await;
    {
        let mut store = h.ctx.store.lock().await;
        let (note, _) = store
            .design_note_add(NewDesignNote {
                item: session.item.clone(),
                session: session.id.clone(),
                kind: NoteKind::Change,
                text: "looks fine".into(),
                details_text: None,
                send_picture: false,
                target: None,
                viewport: Viewport {
                    w: 1440,
                    h: 900,
                    preset: ViewportPreset::Desktop,
                },
                client_key: "key-fine".into(),
                reverts: None,
                resent_from: None,
                follows: None,
                refs: Vec::new(),
                before_asset: None,
                created_by_device: None,
            })
            .unwrap();
        let base = common::git(&run.workspace, &["rev-parse", "HEAD"]);
        for change in [
            NoteChange::Sent {
                base_commit: base,
                delivery: 1,
            },
            NoteChange::Status {
                to: NoteStatus::Done,
                fail_reason: None,
                commit: None,
                no_change: true,
            },
        ] {
            store
                .design_note_change(&session.item, &note.id, change)
                .unwrap();
        }
        // A change whose commit is no longer in the code: done, not "no
        // change", and not active. Finish accepts this session (a done "no
        // change" note and nothing active), so the gate must too.
        let gone = common::git(
            &run.workspace,
            &["commit-tree", "HEAD^{tree}", "-p", "HEAD", "-m", "gone"],
        );
        let (note, _) = store
            .design_note_add(NewDesignNote {
                item: session.item.clone(),
                session: session.id.clone(),
                kind: NoteKind::Change,
                text: "make the title red".into(),
                details_text: None,
                send_picture: false,
                target: None,
                viewport: Viewport {
                    w: 1440,
                    h: 900,
                    preset: ViewportPreset::Desktop,
                },
                client_key: "key-gone".into(),
                reverts: None,
                resent_from: None,
                follows: None,
                refs: Vec::new(),
                before_asset: None,
                created_by_device: None,
            })
            .unwrap();
        let base = common::git(&run.workspace, &["rev-parse", "HEAD"]);
        for change in [
            NoteChange::Sent {
                base_commit: base,
                delivery: 2,
            },
            NoteChange::Status {
                to: NoteStatus::Done,
                fail_reason: None,
                commit: Some(gone),
                no_change: false,
            },
        ] {
            store
                .design_note_change(&session.item, &note.id, change)
                .unwrap();
        }
    }
    let head = common::git(&run.workspace, &["rev-parse", "HEAD"]);
    confirm(&h, &session, &head, &[], &[]).await;
    h.settle_running(forged).await;
    h.dispatch().await;
    let gate = open_proposal(&h, &item.id).await;
    approve(&h, &gate).await;
    let intent = intent_of(&h, &item.id).await;
    assert_eq!(intent.requirements.len(), 1);
    assert!(intent.requirements[0].text.contains("no change"));
    assert!(intent.images.is_empty());
}

/// The build after an approved live design owes `requirement_results` whose
/// pairs match each picture's recorded viewport. Its refusal is repaired at
/// the build, past the approved design: the repair's brief amendment once
/// sent the retried run back to the design stage, which refused every
/// attempt because no one would finish a new session.
#[tokio::test]
async fn a_refused_build_after_an_approved_design_is_repaired_past_the_design() {
    let mut config = live_config();
    config.review.auto_repair = true;
    let h = Harness::with_config(config).await;
    let item = enqueue(&h, live(), None).await.unwrap();
    let (run, session) = designing(&h, &item).await;
    let (_, head) = done_note(&h, &session, &run, "make it green", b"green").await;
    confirm(&h, &session, &head, &["D1"], &[]).await;
    h.settle_running(forged).await;
    h.dispatch().await;
    let gate = open_proposal(&h, &item.id).await;
    approve(&h, &gate).await;
    // The reference records the viewport its picture was taken at, and no
    // theme or state a built image could not show.
    let intent = intent_of(&h, &item.id).await;
    let image = &intent.images[0];
    assert_eq!(
        image.viewport,
        Some(archon_core::intent::Viewport {
            width: 1440,
            height: 900
        })
    );
    assert_eq!(
        (image.theme.as_deref(), image.state.as_deref()),
        (None, None)
    );
    let build = h.running().await.remove(0);
    assert_ne!(build.stage_id, "design", "the build runs after approval");
    let prompt = h.backend.requests().pop().unwrap().instructions;
    assert!(
        prompt.contains("\"width\": 1440") && prompt.contains("\"height\": 900"),
        "the owed results name the picture's viewport: {prompt}"
    );
    // A pair at another viewport is refused, naming both sizes.
    let pair = |width: u32| {
        json!({"requirement_results": [{"id": "D1", "status": "implemented", "evidence": "green",
            "visual_pairs": [{"reference": "D1-final", "built": "built.png",
                "viewport": {"width": width, "height": 900}, "theme": null, "state": null}]}]})
    };
    std::fs::write(run.workspace.join("built.png"), PNG).unwrap();
    let wrong = pair(1280);
    h.settle_running(move |_| StageOutcome::Completed {
        summary: "built".into(),
        structured: Some(wrong.clone()),
    })
    .await;
    h.dispatch().await;
    let repaired = h.item(&item.id).await;
    assert!(
        repaired
            .brief
            .contains("the reference is 1440x900, the built image 1280x900"),
        "{}",
        repaired.brief
    );
    let running = h.running().await;
    assert_eq!(running.len(), 1, "{running:?}");
    assert_eq!(
        running[0].stage_id, build.stage_id,
        "the repair retries the build, not the approved design"
    );
    assert_ne!(running[0].run, build.run, "in a new run");
}

/// A revised session is reviewed, confirmed and approved through to its
/// build, carrying the first review's choices. "D1 superseded by D2", chosen
/// at the first review, lived only in that review's record (an earlier
/// session's notes are read-only), so after Ask for changes D1 came back as
/// an active requirement with its picture.
#[tokio::test]
async fn a_revised_session_is_approved_with_the_first_reviews_replacements() {
    let h = Harness::with_config(live_config()).await;
    let item = enqueue(&h, live(), None).await.unwrap();
    let (run, session) = designing(&h, &item).await;
    done_note(&h, &session, &run, "make it green", b"green").await;
    let (_, head) = done_note(&h, &session, &run, "make it blue", b"blue").await;
    confirm(&h, &session, &head, &["D1", "D2"], &[("D1", "D2")]).await;
    h.settle_running(forged).await;
    h.dispatch().await;
    let gate = open_proposal(&h, &item.id).await;
    h.cli(Request::Answer {
        chat: false,
        question: gate.question.clone().expect("asked"),
        text: "a darker blue please".into(),
    })
    .await
    .expect("answer");
    h.dispatch().await;
    let next = {
        let mut store = h.ctx.store.lock().await;
        let next = h
            .ctx
            .design
            .session_of(&store, &run.id, "design", 2)
            .unwrap()
            .expect("the revised session");
        if next.state == SessionState::Starting {
            store
                .design_session_transition(
                    &next.id,
                    SessionState::Starting,
                    SessionState::Live,
                    None,
                    false,
                )
                .unwrap()
        } else {
            next
        }
    };
    done_note(&h, &next, &run, "darker blue", b"darker").await;
    let view = match h
        .cli(Request::DesignFinishCapture {
            session: next.id.clone(),
            cancel_queued: false,
            recapture: false,
        })
        .await
        .expect("finish")
    {
        Reply::DesignFinish(view) => view,
        other => panic!("{other:?}"),
    };
    let card = |id: &str| view.cards.iter().find(|c| c.note == id).expect("a card");
    assert_eq!(
        card("D1").superseded_by.as_deref(),
        Some("D2"),
        "the first review's replacement is shown"
    );
    assert!(card("D2").earlier && card("D2").superseded_by.is_none());
    assert!(!card("D3").earlier);
    // Confirm without restating it: the replacement is carried regardless.
    h.cli(Request::DesignFinishConfirm {
        session: next.id.clone(),
        capture: view.capture.clone(),
        include: view.cards.iter().map(|c| (c.note.clone(), true)).collect(),
        superseded: BTreeMap::new(),
        confirm_no_change: false,
    })
    .await
    .expect("confirmed");
    h.settle_running(forged).await;
    h.dispatch().await;
    let gate = open_proposal(&h, &item.id).await;
    approve(&h, &gate).await;
    let intent = intent_of(&h, &item.id).await;
    let ids: Vec<_> = intent.requirements.iter().map(|r| r.id.as_str()).collect();
    assert_eq!(ids, ["D2", "D3"], "D1 stays replaced by D2");
    let running = h.running().await;
    assert!(
        running.iter().any(|s| s.stage_id != "design"),
        "the build runs past the approved design"
    );
}
