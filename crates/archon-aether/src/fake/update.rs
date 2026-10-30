//! `update_agent` over a fake's agent list, as aetherd applies it: the named
//! agent takes the patch, and with `subtree` each live agent beneath it has a
//! patched deadline or cost budget raised where its own is lower, never lowered.

use std::collections::BTreeSet;

use archon_core::Timestamp;
use serde_json::{Value, json};

use super::Scripted;

/// Apply the `update_agent` `frame` to `agents` (`Agent` payloads linked by
/// `parent`) and reply with the named agent, as a responder does.
pub fn update_agent(agents: &mut [Value], frame: &Value) -> Scripted {
    assert_eq!(frame["cmd"], "update_agent", "an update_agent frame");
    let id = frame["id"].as_str().expect("update_agent names an agent");
    let Some(named) = agents.iter().position(|agent| agent["id"] == id) else {
        return Scripted::refuse("not_found", &format!("no agent {id}"));
    };
    let patch = &frame["patch"];
    // `"clear"` has no `set`: no deadline, which lifts every other one.
    let deadline = patch.get("deadline").map(|update| update["set"].clone());
    let budget = patch.get("cost_budget").cloned();
    apply_patch(&mut agents[named], patch);
    if frame["subtree"] == json!(true) {
        for below in beneath(agents, id) {
            raise(&mut agents[below], deadline.as_ref(), budget.as_ref());
        }
    }
    Scripted::reply("agent", agents[named].clone())
}

fn apply_patch(agent: &mut Value, patch: &Value) {
    if let Some(model) = patch.get("model") {
        agent["model"] = model.clone();
    }
    for key in ["effort", "cost_budget"] {
        if let Some(value) = patch.get(key) {
            agent["config"][key] = value.clone();
        }
    }
    if let Some(update) = patch.get("deadline") {
        agent["config"]["deadline"] = update["set"].clone();
    }
}

/// The indexes of the agents beneath `root`, at any depth.
fn beneath(agents: &[Value], root: &str) -> Vec<usize> {
    let mut parents = BTreeSet::from([root.to_owned()]);
    let mut found = BTreeSet::new();
    loop {
        let before = found.len();
        for (at, agent) in agents.iter().enumerate() {
            let parent = agent["parent"].as_str().unwrap_or_default();
            if parents.contains(parent) && found.insert(at) {
                parents.insert(agent["id"].as_str().expect("an agent id").to_owned());
            }
        }
        if found.len() == before {
            break;
        }
    }
    assert!(
        found.len() < agents.len(),
        "the named agent is not beneath itself"
    );
    found.into_iter().collect()
}

/// Lift a live agent's limits to the targets where they are higher.
fn raise(agent: &mut Value, deadline: Option<&Value>, budget: Option<&Value>) {
    let state = agent["status"]["state"].as_str().unwrap_or_default();
    let ended = ["completed", "failed", "cancelled", "imported"].contains(&state);
    if ended || !agent["restarted_as"].is_null() {
        return;
    }
    let config = &mut agent["config"];
    if let Some(target) = deadline
        && lifts(instant(&config["deadline"]), instant(target))
    {
        config["deadline"] = target.clone();
    }
    if let Some(target) = budget
        && lifts(config["cost_budget"].as_f64(), target.as_f64())
    {
        config["cost_budget"] = target.clone();
    }
}

/// Whether `target` lifts `current`: no limit is the highest, so an agent
/// without one keeps it.
fn lifts<T: PartialOrd>(current: Option<T>, target: Option<T>) -> bool {
    match (current, target) {
        (None, _) => false,
        (Some(_), None) => true,
        (Some(current), Some(target)) => target > current,
    }
}

fn instant(value: &Value) -> Option<Timestamp> {
    serde_json::from_value(value.clone()).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn held(id: &str, parent: Option<&str>, state: &str, config: Value) -> Value {
        let mut agent = super::super::agent(id, None);
        agent["parent"] = json!(parent);
        agent["status"] = json!({"state": state});
        agent["config"] = config;
        agent
    }

    #[test]
    fn a_subtree_update_raises_only_lower_live_limits() {
        let early = json!({"deadline": "2026-10-27T10:00:00Z", "cost_budget": 5.0});
        let late = json!({"deadline": "2026-10-27T13:00:00Z", "cost_budget": 50.0});
        let mut agents = vec![
            held("agt_lead", None, "running", early.clone()),
            held("agt_child", Some("agt_lead"), "running", early.clone()),
            held("agt_grandchild", Some("agt_child"), "ready", early.clone()),
            held("agt_late", Some("agt_lead"), "running", late.clone()),
            held("agt_unlimited", Some("agt_lead"), "running", json!({})),
            held("agt_done", Some("agt_lead"), "completed", early.clone()),
            held("agt_other", None, "running", early.clone()),
        ];
        let frame = json!({"cmd": "update_agent", "id": "agt_lead", "subtree": true,
            "patch": {"deadline": {"set": "2026-10-27T11:00:00Z"}, "cost_budget": 9.0,
                      "effort": "high"}});
        let Scripted::Reply { data, .. } = update_agent(&mut agents, &frame) else {
            panic!("the named agent is known");
        };
        let raised = json!({"deadline": "2026-10-27T11:00:00Z", "cost_budget": 9.0});
        assert_eq!(data["config"]["effort"], "high");
        for at in [1, 2] {
            assert_eq!(agents[at]["config"], raised, "{}", agents[at]["id"]);
        }
        assert_eq!(agents[3]["config"], late, "a later limit is kept");
        assert_eq!(agents[4]["config"], json!({}), "no limit is kept");
        assert_eq!(agents[5]["config"], early, "an ended agent is left");
        assert_eq!(agents[6]["config"], early, "outside the subtree");
    }

    #[test]
    fn without_subtree_only_the_named_agent_changes() {
        let early = json!({"deadline": "2026-10-27T10:00:00Z"});
        let mut agents = vec![
            held("agt_lead", None, "running", early.clone()),
            held("agt_child", Some("agt_lead"), "running", early.clone()),
        ];
        let frame = json!({"cmd": "update_agent", "id": "agt_lead",
            "patch": {"deadline": {"set": "2026-10-27T11:00:00Z"}}});
        assert!(matches!(
            update_agent(&mut agents, &frame),
            Scripted::Reply { .. }
        ));
        assert_eq!(agents[0]["config"]["deadline"], "2026-10-27T11:00:00Z");
        assert_eq!(agents[1]["config"], early);
        let unknown = json!({"cmd": "update_agent", "id": "agt_gone", "patch": {}});
        assert!(matches!(
            update_agent(&mut agents, &unknown),
            Scripted::Refuse { .. }
        ));
    }
}
