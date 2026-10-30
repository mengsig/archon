//! A steer's extension and budget regrant reach the agents a stage's lead
//! delegated to (ARCHITECTURE §6.6): in one `subtree` update when aetherd
//! lists `subtree_update`, one keyed update per helper otherwise. A helper
//! that holds a later deadline or a larger budget keeps it.

use super::*;
use archon_aether::wire::SUBTREE_UPDATE_FEATURE;
use archon_core::Timestamp;
use archon_core::protocol::{ItemPatch, Reply, Request};
use archon_core::queue::QueueItem;
use archon_core::steer::Steer;

const LEAD: &str = "agt_0001";
const HELPER: &str = "agt_helper";
const GRANTED_USD: f64 = 10.0;

/// A running stage whose lead delegated one helper, on a fake aetherd that
/// holds both agents' limits.
struct Delegating {
    aether: FakeAether,
    h: Harness,
    item: QueueItem,
    /// The run node, the lead and the helper, as Aether holds them.
    tree: Arc<Mutex<Vec<Value>>>,
    /// The lead's deadline at its spawn.
    deadline: Timestamp,
}

impl Delegating {
    fn config(&self, agent: &str) -> Value {
        let tree = self.tree.lock().expect("tree");
        let found = tree
            .iter()
            .find(|a| a["id"] == agent)
            .expect("a known agent");
        found["config"].clone()
    }

    fn deadline_of(&self, agent: &str) -> Timestamp {
        serde_json::from_value(self.config(agent)["deadline"].clone()).expect("a deadline")
    }

    async fn steer(&self, steer: Steer) {
        let request = Request::SteerItem {
            item: self.item.id.clone(),
            steer,
        };
        let reply = self.h.cli(request).await.expect("steered");
        assert!(matches!(reply, Reply::Item(_)), "{reply:?}");
    }
}

/// Start a stage capped at [`GRANTED_USD`] in Aether, whose lead delegated
/// one helper with the config `helper` makes from the lead's. The helper
/// copies it, as Aether's `child_spec` does, unless `helper` says otherwise.
async fn delegating(
    dir: &std::path::Path,
    subtree: bool,
    helper: impl FnOnce(&Value) -> Value,
) -> Delegating {
    let aether = start_aether(dir).await;
    if subtree {
        aether.set_features(vec![SUBTREE_UPDATE_FEATURE.into()]);
    }
    let fleet = script_fleet(&aether);
    script_budget_models(&aether);
    let h = harness(&aether).await;
    let item = h.enqueue("helped").await;
    let capped = Request::UpdateItem {
        item: item.id.clone(),
        patch: ItemPatch {
            budget_usd: Some(GRANTED_USD),
            ..ItemPatch::default()
        },
    };
    h.cli(capped).await.expect("a budget before the run");
    h.dispatch().await;
    let creates = aether.wait_for("create_agent", 2, WAIT).await;
    let lead_config = creates[1]["config"].clone();
    assert_eq!(lead_config["cost_budget"], json!(GRANTED_USD));
    let deadline = serde_json::from_value(lead_config["deadline"].clone()).expect("a deadline");
    let mut agents = fleet.lock().expect("fleet").agents();
    let lead = agents.iter_mut().find(|a| a["id"] == LEAD).expect("lead");
    lead["config"] = lead_config.clone();
    let mut child = fake::agent(HELPER, Some("tsk_helper"));
    child["parent"] = json!(LEAD);
    child["status"] = json!({"state": "running"});
    child["config"] = helper(&lead_config);
    agents.push(child);
    let tree = Arc::new(Mutex::new(agents));
    script_tree(&aether, &tree);
    Delegating {
        aether,
        h,
        item,
        tree,
        deadline,
    }
}

/// `get_agent`, `list_agents` and `update_agent` read and change `tree`.
fn script_tree(aether: &FakeAether, tree: &Arc<Mutex<Vec<Value>>>) {
    let read = tree.clone();
    aether.set_responder(
        "get_agent",
        Arc::new(move |frame: &Value| {
            let tree = read.lock().expect("tree");
            match tree.iter().find(|a| a["id"] == frame["id"]) {
                Some(agent) => Scripted::reply("agent", agent.clone()),
                None => Scripted::refuse("not_found", "no such agent"),
            }
        }),
    );
    let listed = tree.clone();
    aether.set_responder(
        "list_agents",
        Arc::new(move |_| Scripted::reply("agents", json!(*listed.lock().expect("tree")))),
    );
    let updated = tree.clone();
    aether.set_responder(
        "update_agent",
        Arc::new(move |frame: &Value| {
            fake::update_agent(&mut updated.lock().expect("tree"), frame)
        }),
    );
}

/// Each `update_agent` sent: one `subtree` update on the lead with the
/// feature; without it, the lead's then one keyed update per raised helper.
fn assert_updates(aether: &FakeAether, subtree: bool, helper_raised: bool) {
    let sent = aether.received_named("update_agent");
    let expected = if subtree || !helper_raised { 1 } else { 2 };
    assert_eq!(sent.len(), expected, "{sent:?}");
    assert_eq!(sent[0]["id"], LEAD);
    assert_eq!(sent[0].get("subtree").is_some(), subtree, "{}", sent[0]);
    if let Some(helper) = sent.get(1) {
        assert_eq!(helper["id"], HELPER);
        assert!(helper.get("subtree").is_none(), "{helper}");
        let key = helper["idempotency_key"].as_str().expect("keyed");
        assert!(key.ends_with(&format!(":{HELPER}")), "{key}");
    }
}

#[tokio::test]
async fn an_extension_reaches_the_helper_on_both_paths() {
    for subtree in [true, false] {
        let dir = tempfile::tempdir().expect("tempdir");
        let d = delegating(dir.path(), subtree, Value::clone).await;
        let extend = Steer {
            extend_minutes: Some(30),
            ..Steer::default()
        };
        d.steer(extend).await;
        let later = d.deadline + chrono::Duration::minutes(30);
        assert_eq!(d.deadline_of(LEAD), later, "subtree: {subtree}");
        assert_eq!(d.deadline_of(HELPER), later, "subtree: {subtree}");
        assert_eq!(d.config(HELPER)["cost_budget"], json!(GRANTED_USD));
        assert_updates(&d.aether, subtree, true);
    }
}

#[tokio::test]
async fn a_budget_regrant_reaches_the_helper_on_both_paths() {
    for subtree in [true, false] {
        let dir = tempfile::tempdir().expect("tempdir");
        let d = delegating(dir.path(), subtree, Value::clone).await;
        let regrant = Steer {
            budget_usd: Some(25.0),
            ..Steer::default()
        };
        d.steer(regrant).await;
        for agent in [LEAD, HELPER] {
            let config = d.config(agent);
            assert_eq!(
                config["cost_budget"],
                json!(25.0),
                "{agent}, subtree: {subtree}"
            );
            assert_eq!(d.deadline_of(agent), d.deadline, "the deadline is left");
        }
        assert_updates(&d.aether, subtree, true);
    }
}

#[tokio::test]
async fn a_helper_with_a_later_deadline_and_a_larger_budget_keeps_them() {
    for subtree in [true, false] {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut kept = None;
        let d = delegating(dir.path(), subtree, |lead| {
            let at: Timestamp = serde_json::from_value(lead["deadline"].clone()).unwrap();
            let own = json!({"deadline": at + chrono::Duration::hours(2), "cost_budget": 100.0});
            kept = Some(own.clone());
            own
        })
        .await;
        let both = Steer {
            extend_minutes: Some(30),
            budget_usd: Some(25.0),
            ..Steer::default()
        };
        d.steer(both).await;
        let later = d.deadline + chrono::Duration::minutes(30);
        assert_eq!(d.deadline_of(LEAD), later);
        assert_eq!(d.config(LEAD)["cost_budget"], json!(25.0));
        assert_eq!(Some(d.config(HELPER)), kept, "subtree: {subtree}");
        assert_updates(&d.aether, subtree, false);
    }
}
