//! A stage agent can call `submit_verdict` before Archon has recorded that
//! the agent is its stage's: a spawn whose reply is still on its way, a wake
//! not yet bound to the replacement stage run. The call waits for the record
//! instead of being refused, and nobody else waits.

use std::time::Duration;

use archon_core::run::{CheckDecision, StageState, VerdictNote};

use super::replaced::woken_at_deadline;
use super::{E2E, PASS, at_e2e, call, disputed, e2e_plan, forwarded, kept, key, submit};
use crate::common::{Harness, completed};

/// Longer than any refusal takes, shorter than the wait for a binding.
const AT_ONCE: Duration = Duration::from_secs(2);

#[tokio::test]
async fn an_agent_submitting_before_its_spawn_is_recorded_waits_for_the_record() {
    let h = Harness::new().await;
    h.enqueue_in(E2E, "journeys", e2e_plan()).await.unwrap();
    h.dispatch().await;
    let setup = h.settle_running(|_| completed("start.sh ready")).await;
    let setup_agent = setup[0].aether_agent.clone().expect("an agent");
    // Aether creates the `e2e` agent, but its reply never reaches Archon.
    h.backend.lose_next_reply();
    h.dispatch().await;
    let live = h.live_stages().await;
    let [spawning] = live.as_slice() else {
        panic!("one live stage: {live:?}");
    };
    assert_eq!(
        (spawning.stage_id.as_str(), &spawning.state),
        ("e2e", &StageState::Spawning)
    );
    let agent = h.backend.created_for(spawning.id.as_str());
    let agent = agent.expect("Aether created the agent");

    // An agent a stage run holds is no spawn's: it is refused without the wait.
    let finished = call(Some(&setup_agent), PASS, disputed(), false);
    let refused = tokio::time::timeout(AT_ONCE, forwarded(&h, &setup_agent, finished)).await;
    assert!(matches!(refused, Ok(Err(_))), "{refused:?}");

    let submitting = forwarded(&h, &agent, call(Some(&agent), PASS, disputed(), false));
    let recording = async {
        tokio::time::sleep(Duration::from_millis(200)).await;
        h.dispatch().await;
    };
    let (asked, ()) = tokio::join!(submitting, recording);
    let asked = asked.expect("its stage was bound while it waited");
    assert_eq!(asked.decision, CheckDecision::Restate);
    let running = super::only_running(&h, "e2e").await;
    assert_eq!(running.aether_agent.as_ref(), Some(&agent));
    let ask = kept(&h, &running).await.expect("the ask is kept");
    assert_eq!(ask.asked, VerdictNote::DisputedPass);
}

#[tokio::test]
async fn a_woken_agent_submitting_before_its_wake_is_recorded_waits_for_the_record() {
    let h = Harness::new().await;
    let (_, e2e) = at_e2e(&h).await;
    submit(&h, &e2e, PASS, disputed()).await;
    let agent = e2e.aether_agent.clone().expect("an agent");
    // The wake reaches Aether, and the agent, before Archon records it.
    let (at_wake, woke) = tokio::sync::oneshot::channel();
    h.backend.on_next_update(move || {
        at_wake.send(()).expect("the test listens");
    });
    h.backend.slow_answers(&agent, Duration::from_millis(200));

    let submitting = async {
        woke.await.expect("the wake was sent");
        let held = h.ctx.store.lock().await.stage_run_by_agent(&agent);
        let held = held.expect("the store reads").expect("its stopped run");
        assert_eq!(held.id, e2e.id, "the wake is not on record yet");
        assert!(held.state.is_terminal(), "{:?}", held.state);
        forwarded(&h, &agent, call(Some(&agent), PASS, disputed(), false)).await
    };
    let (woken, again) = tokio::join!(woken_at_deadline(&h, &e2e), submitting);
    let again = again.expect("the woken run was bound while it waited");
    assert_eq!(again.decision, CheckDecision::Restate, "the ask it owes");
    assert_eq!(key(&woken), key(&e2e), "in the same attempt");
    assert_eq!(woken.aether_agent.as_ref(), Some(&agent));
}
