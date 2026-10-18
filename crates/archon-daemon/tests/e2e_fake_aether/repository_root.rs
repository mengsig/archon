use super::*;

fn assert_identity(create: &Value, root: Option<&std::path::Path>) {
    assert!(create["scope"].is_object());
    assert!(create["scope"].get("repository_root").is_none());
    match root {
        Some(root) => {
            assert_eq!(create["repository_root"], json!(root));
            assert!(create["scope"].get("repository").is_none());
            assert_ne!(create["scope"]["workspace_root"], json!(root));
        }
        None => {
            assert!(create.get("repository_root").is_none());
            assert_eq!(create["scope"]["repository"], common::REPO_NAME);
        }
    }
}

async fn reconnect(h: &Harness, aether: &FakeAether) {
    let client = AetherClient::connect_with(
        &aether.endpoint(),
        ClientOptions {
            request_timeout: Duration::from_millis(100),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(client.welcome().protocol_version, 23);
    assert!(!client.welcome().instance_id.is_empty());
    h.ctx.aether.replace(Some(Arc::new(client))).unwrap();
}

async fn restart(h: &mut Harness, aether: &FakeAether) {
    h.restart();
    let ctx = Arc::get_mut(&mut h.ctx).unwrap();
    ctx.backend = Arc::new(AetherBackend::new(
        ctx.aether.clone(),
        ctx.config.models.policy(),
    ));
    reconnect(h, aether).await;
    assert!(h.ctx.runs.lock().await.drivers.is_empty());
    executor::recover(&h.ctx).await.unwrap();
    assert!(!h.ctx.runs.lock().await.drivers.is_empty());
}

fn without_request_id(mut frame: Value) -> Value {
    frame.as_object_mut().unwrap().remove("req").unwrap();
    frame
}

#[tokio::test]
async fn registered_checkout_names_nodes_and_stages_only_when_advertised() {
    for supported in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let aether = start_aether(dir.path()).await;
        aether.set_features(if supported {
            vec!["repository_root".into()]
        } else {
            vec![]
        });
        script_fleet(&aether);
        let h = harness(&aether).await;
        let item = h.enqueue("share checkout memory").await;
        h.dispatch().await;
        let creates = aether.received_named("create_agent");
        assert_eq!(creates.len(), 2);
        for create in &creates {
            assert_identity(create, supported.then_some(h.repo.as_path()));
        }
        assert_eq!(h.stages(&item.id).await[0].state, StageState::Running);
    }
}

#[tokio::test]
async fn lost_node_and_stage_replies_replay_the_frozen_identity_after_restart() {
    for (supported, stage_pending) in [(false, false), (false, true), (true, false), (true, true)] {
        let dir = tempfile::tempdir().unwrap();
        let aether = start_aether(dir.path()).await;
        aether.set_features(if supported {
            vec!["repository_root".into()]
        } else {
            vec![]
        });
        let fleet = script_fleet(&aether);
        let mut h = harness(&aether).await;
        reconnect(&h, &aether).await;
        if stage_pending {
            aether.script(
                "create_agent",
                Scripted::reply("agent", fake::agent(&node_id(1), None)),
            );
        }
        fleet.lock().unwrap().lose_next_reply = true;
        let item = h.enqueue("replay repository identity").await;
        h.dispatch().await;
        assert_eq!(h.stages(&item.id).await[0].state, StageState::Spawning);
        let before = aether.received_named("create_agent");
        aether.set_features(vec!["repository_root".into()]);
        restart(&mut h, &aether).await;
        h.dispatch().await;
        let after = aether.received_named("create_agent");
        assert_eq!(
            without_request_id(before.last().unwrap().clone()),
            without_request_id(after[before.len()].clone())
        );
        for create in &after {
            assert_identity(create, supported.then_some(h.repo.as_path()));
        }
        assert_eq!(h.stages(&item.id).await[0].state, StageState::Running);
    }
}

#[tokio::test]
async fn stages_keep_the_nodes_name_after_aether_gains_the_feature() {
    let dir = tempfile::tempdir().unwrap();
    let aether = start_aether(dir.path()).await;
    let fleet = script_fleet(&aether);
    let h = harness(&aether).await;
    let item = h.enqueue("keep the original memory scope").await;
    h.dispatch().await;
    aether.set_features(vec!["repository_root".into()]);
    reconnect(&h, &aether).await;
    complete_running(&h, &fleet, |_| json!({"summary": "implemented"})).await;
    h.dispatch().await;
    let creates = aether.received_named("create_agent");
    assert_eq!(creates.len(), 4);
    for create in &creates {
        assert_identity(create, None);
    }
    assert_children_of(&creates[1..], &node_id(1));
    assert_eq!(h.running().await.len(), 2);
    assert_eq!(
        h.run(&item.id).await.aether_run.unwrap().as_str(),
        "run_0001"
    );
}

#[tokio::test]
async fn a_refused_root_node_advances_its_generation_once() {
    for code in ["invalid_argument", "not_found"] {
        let dir = tempfile::tempdir().unwrap();
        let aether = start_aether(dir.path()).await;
        aether.set_features(vec!["repository_root".into()]);
        script_fleet(&aether);
        aether.script(
            "create_agent",
            Scripted::refuse(code, "stale submission key"),
        );
        let h = harness(&aether).await;
        let item = h.enqueue("replace refused root key").await;
        h.dispatch().await;
        let creates = aether.received_named("create_agent");
        assert_eq!(creates.len(), 3, "two node generations and their stage");
        let run = h.run(&item.id).await;
        for (index, create) in creates[..2].iter().enumerate() {
            assert_identity(create, Some(&h.repo));
            assert_eq!(
                create["idempotency_key"],
                format!("{}:node:{}", run.id, index + 1)
            );
        }
        assert_identity(&creates[2], Some(&h.repo));
        assert_children_of(&creates[2..], &node_id(1));
        assert_eq!(h.stages(&item.id).await[0].state, StageState::Running);
    }
}

#[tokio::test]
async fn a_root_node_refused_twice_fails_with_the_original_reason() {
    for code in ["invalid_argument", "not_found"] {
        let dir = tempfile::tempdir().unwrap();
        let aether = start_aether(dir.path()).await;
        aether.set_features(vec!["repository_root".into()]);
        script_fleet(&aether);
        for _ in 0..2 {
            aether.script("create_agent", Scripted::refuse(code, "model unavailable"));
        }
        let h = harness(&aether).await;
        let item = h.enqueue("root refused twice").await;
        h.dispatch().await;
        let QueueState::Failed { error, .. } = h.state(&item.id).await else {
            panic!("two refusals must fail the item");
        };
        assert!(error.contains("model unavailable"), "{error}");
        assert!(!error.contains("checkout"), "{error}");
        let creates = aether.received_named("create_agent");
        assert_eq!(creates.len(), 2, "only one generation advance is allowed");
        let run = h.run(&item.id).await;
        for (index, create) in creates.iter().enumerate() {
            assert_identity(create, Some(&h.repo));
            assert_eq!(
                create["idempotency_key"],
                format!("{}:node:{}", run.id, index + 1)
            );
        }
        assert!(h.kv(&format!("run_node:{}", run.id)).await.is_none());
        h.dispatch().await;
        assert_eq!(aether.received_named("create_agent"), creates);
    }
}

#[tokio::test]
async fn a_root_node_with_a_changed_lean_replay_advances_its_generation() {
    let dir = tempfile::tempdir().unwrap();
    let aether = start_aether(dir.path()).await;
    aether.set_features(vec!["repository_root".into(), "host_context".into()]);
    let fleet = script_fleet(&aether);
    fleet.lock().unwrap().lose_next_reply = true;
    let mut config = Config::default();
    config.aether.host_context = archon_core::config::HostContext::Lean;
    let options = ClientOptions {
        request_timeout: Duration::from_millis(100),
        ..Default::default()
    };
    let h = harness_with_config(&aether, options, config).await;
    let mut plan = common::plan();
    plan.0.get_mut("implement").unwrap()[0] =
        common::choice("claude-code/claude-haiku-5-5", Effort::Low);
    let item = h
        .enqueue_with("root replay falls back to full", plan)
        .await
        .unwrap();
    h.dispatch().await;
    assert_eq!(h.stages(&item.id).await[0].state, StageState::Spawning);
    aether.set_features(vec!["repository_root".into()]);
    reconnect(&h, &aether).await;
    h.dispatch().await;
    let creates = aether.received_named("create_agent");
    assert_eq!(
        creates.len(),
        4,
        "original, refused replay, replacement, stage"
    );
    assert_eq!(creates[0]["config"]["host_context"], "lean");
    assert!(creates[1]["config"].get("host_context").is_none());
    assert_eq!(creates[0]["idempotency_key"], creates[1]["idempotency_key"]);
    let run = h.run(&item.id).await;
    assert_eq!(creates[2]["idempotency_key"], format!("{}:node:2", run.id));
    for create in &creates {
        assert_identity(create, Some(&h.repo));
    }
    assert_children_of(&creates[3..], &node_id(2));
    assert_eq!(h.stages(&item.id).await[0].state, StageState::Running);
}

#[tokio::test]
async fn a_moved_frozen_checkout_replays_and_cancels_its_pending_node() {
    let dir = tempfile::tempdir().unwrap();
    let aether = start_aether(dir.path()).await;
    aether.set_features(vec!["repository_root".into()]);
    let fleet = script_fleet(&aether);
    let h = harness(&aether).await;
    reconnect(&h, &aether).await;
    fleet.lock().unwrap().lose_next_reply = true;
    let item = h.enqueue("checkout moved after identity froze").await;
    h.dispatch().await;
    assert_eq!(h.stages(&item.id).await[0].state, StageState::Spawning);
    std::fs::rename(&h.repo, h.repo.with_file_name("moved-checkout")).unwrap();
    h.dispatch().await;
    let QueueState::Failed { error, .. } = h.state(&item.id).await else {
        panic!("a moved root must fail the item");
    };
    assert!(error.contains(&h.repo.display().to_string()), "{error}");
    assert!(error.contains("is missing"), "{error}");
    assert!(
        error.contains("restore it there and retry the item"),
        "{error}"
    );
    assert!(
        error.contains("a re-registered path applies to new runs"),
        "{error}"
    );
    assert!(!error.contains("Aether refused"), "{error}");
    let creates = aether.received_named("create_agent");
    assert_eq!(creates.len(), 2, "only cleanup replays the missing root");
    assert_eq!(
        without_request_id(creates[0].clone()),
        without_request_id(creates[1].clone())
    );
    let cancels = aether.received_named("cancel");
    assert_eq!(cancels.len(), 1);
    assert_eq!(cancels[0]["agent"], node_id(1));
    let run = h.run(&item.id).await;
    assert!(h.kv(&format!("run_node:{}", run.id)).await.is_none());
    h.dispatch().await;
    assert_eq!(aether.received_named("create_agent"), creates);
    assert_eq!(aether.received_named("cancel"), cancels);
}

#[tokio::test]
async fn replacing_a_dead_generation_two_node_keeps_its_root_after_feature_loss() {
    let dir = tempfile::tempdir().unwrap();
    let aether = start_aether(dir.path()).await;
    aether.set_features(vec!["repository_root".into()]);
    let fleet = script_fleet(&aether);
    let h = harness(&aether).await;
    let item = h.enqueue("keep root after generation two dies").await;
    h.dispatch().await;
    complete_running(&h, &fleet, |_| json!({"summary": "implemented"})).await;
    aether.script("get_agent", Scripted::refuse("not_found", "node one lost"));
    h.dispatch().await;
    let run = h.run(&item.id).await;
    let key = format!("run_node:{}", run.id);
    let raw = h.kv(&key).await.unwrap();
    let second: Value = serde_json::from_str(&raw).unwrap();
    assert_eq!(second["generation"], 2);
    assert_eq!(second["agent"], node_id(2));
    assert_eq!(h.running().await.len(), 2);

    aether.set_features(vec![]);
    reconnect(&h, &aether).await;
    let pass =
        json!({"summary": "VERDICT: pass", "structured": {"verdict": "pass", "findings": []}});
    complete_running(&h, &fleet, |_| pass.clone()).await;
    aether.script("get_agent", Scripted::refuse("not_found", "node two lost"));
    let before = aether.received_named("create_agent");
    h.dispatch().await;
    let raw = h.kv(&key).await.unwrap();
    let pending: Value = serde_json::from_str(&raw).unwrap();
    assert_eq!(pending["generation"], 3);
    assert_eq!(
        pending["repository_identity"],
        second["repository_identity"]
    );
    assert_eq!(pending["request"]["repository_root"], json!(h.repo));
    assert!(pending["request"]["repository"].is_null());
    assert!(
        h.stages(&item.id)
            .await
            .iter()
            .any(|s| s.state == StageState::Spawning)
    );
    h.dispatch().await;
    assert_eq!(
        h.kv(&key).await.unwrap(),
        raw,
        "retries keep the frozen generation"
    );
    assert_eq!(aether.received_named("create_agent"), before);
}
