//! Persistent fake GitHub over real Git repositories.

use super::*;
use serde_json::Value;

pub fn install_gh(h: &Harness) {
    std::fs::write(
        &h.ctx.config.github.binary,
        include_str!("initiative_gh.py"),
    )
    .unwrap();
    std::fs::set_permissions(
        &h.ctx.config.github.binary,
        std::fs::Permissions::from_mode(0o755),
    )
    .unwrap();
}

pub fn prs(h: &Harness) -> Value {
    let raw = std::fs::read_to_string(h.dir.path().join("prs.json")).unwrap();
    serde_json::from_str(&raw).unwrap()
}

pub fn set_pr(h: &Harness, branch: &str, field: &str, value: Value) {
    let mut state = prs(h);
    state[branch][field] = value;
    std::fs::write(
        h.dir.path().join("prs.json"),
        serde_json::to_string(&state).unwrap(),
    )
    .unwrap();
}

pub fn commit(tree: &Path, file: &str, content: &str) -> String {
    std::fs::write(tree.join(file), content).unwrap();
    git(tree, &["add", file]);
    git(tree, &["commit", "-m", "Change fixture"]);
    git(tree, &["rev-parse", "HEAD"])
}

pub async fn finish_item(h: &Harness, item: &QueueItemId) {
    for _ in 0..5 {
        let stages = h.running().await;
        let run = h.run(item).await;
        for stage in stages.iter().filter(|s| s.run == run.id) {
            let outcome = if stage.stage_id == "review" {
                verdict(true)
            } else {
                completed("passed")
            };
            archon_daemon::executor::stage_settled(
                &h.ctx,
                stage.aether_agent.as_ref().unwrap(),
                outcome,
            )
            .await
            .unwrap();
        }
        h.dispatch().await;
        if matches!(h.state(item).await, QueueState::Succeeded { .. }) {
            return;
        }
    }
    panic!("item did not succeed: {:?}", h.state(item).await);
}
