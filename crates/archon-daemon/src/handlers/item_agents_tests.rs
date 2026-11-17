use super::*;

fn child(agent: &str, parent: &str) -> Descendant {
    Descendant {
        agent: AetherAgentId::parse(agent).unwrap(),
        parent: AetherAgentId::parse(parent).unwrap(),
        model: "codex/gpt-5".parse().unwrap(),
        role: "worker".into(),
        effort: None,
        state: DescendantState::Live,
        task: None,
        deadline: None,
        cost_budget_usd: None,
    }
}

#[test]
fn validates_unordered_nested_trees_and_empty_forests() {
    let root = AetherAgentId::parse("agt_root").unwrap();
    let descendants = [
        child("agt_nested", "agt_child"),
        child("agt_child", "agt_root"),
    ];
    assert!(validate_tree(&root, &descendants).is_ok());
    assert!(validate_tree(&root, &[]).is_ok());
}

#[test]
fn refuses_foreign_agents_cycles_and_duplicate_ids() {
    let root = AetherAgentId::parse("agt_root").unwrap();
    for descendants in [
        vec![child("agt_foreign", "agt_other_item")],
        vec![child("agt_a", "agt_b"), child("agt_b", "agt_a")],
        vec![child("agt_a", "agt_root"), child("agt_a", "agt_root")],
        vec![child("agt_root", "agt_root")],
    ] {
        let error = validate_tree(&root, &descendants).unwrap_err();
        assert!(matches!(error, ArchonError::Aether(_)));
        assert!(
            error
                .to_string()
                .contains("item_agent_states beneath agt_root")
        );
    }
}
