use std::cell::RefCell;
use std::collections::VecDeque;

use super::*;

struct Fake {
    calls: RefCell<Vec<&'static str>>,
    /// The drain and requeue of each restart archond was asked for.
    asked: RefCell<Vec<(u64, bool)>>,
    aether: anyhow::Result<AetherUpdate>,
    running: Option<Running>,
    seen: RefCell<VecDeque<Seen>>,
    orchestrator_fails: bool,
    /// The call that never answers, as a wedged archond's would not.
    hangs: Option<&'static str>,
    /// The version the restarted orchestrator reports as its creator.
    created_by: &'static str,
    /// The creator of the orchestrator running before any restart.
    creator: Option<&'static str>,
}

impl Fake {
    fn new(seen: Vec<Seen>) -> Self {
        Self {
            calls: RefCell::default(),
            asked: RefCell::default(),
            aether: Ok(AetherUpdate {
                changed: false,
                line: "aether same".into(),
            }),
            running: Some(Running {
                pid: 7,
                version: "archon 1.4.1".into(),
            }),
            seen: RefCell::new(seen.into()),
            orchestrator_fails: false,
            hangs: None,
            created_by: "1.4.2",
            creator: Some("1.4.2"),
        }
    }

    /// Record `call`, then wait forever if it is the one that hangs.
    async fn log(&self, call: &'static str) {
        self.calls.borrow_mut().push(call);
        if self.hangs == Some(call) {
            std::future::pending::<()>().await;
        }
    }
}

impl Steps for Fake {
    async fn update_aether(&self) -> anyhow::Result<AetherUpdate> {
        self.log("aether").await;
        match &self.aether {
            Ok(update) => Ok(update.clone()),
            Err(error) => Err(anyhow::anyhow!("{error:#}")),
        }
    }
    async fn running_archond(&self) -> anyhow::Result<Option<Running>> {
        self.log("running").await;
        Ok(self.running.clone())
    }
    async fn request_restart(&self, plan: &Plan) -> anyhow::Result<()> {
        self.asked
            .borrow_mut()
            .push((plan.drain_secs, plan.requeue));
        self.log("request").await;
        Ok(())
    }
    async fn observe(&self, _: u32) -> Seen {
        self.log("observe").await;
        self.seen
            .borrow_mut()
            .pop_front()
            .unwrap_or(Seen::Down(None))
    }
    async fn refresh_mcp(&self) -> anyhow::Result<()> {
        self.log("mcp").await;
        Ok(())
    }
    async fn orchestrator_creator(&self) -> anyhow::Result<Option<String>> {
        self.log("creator").await;
        Ok(self.creator.map(str::to_owned))
    }
    async fn restart_orchestrator(&self) -> anyhow::Result<String> {
        self.log("orchestrator").await;
        match self.orchestrator_fails {
            true => anyhow::bail!("no link"),
            false => Ok(self.created_by.to_owned()),
        }
    }
    async fn restart_aether_later(&self, _: &Plan) -> anyhow::Result<String> {
        self.log("aether restart").await;
        Ok("Accepted a drained restart.".to_owned())
    }
    async fn pause(&self) {}
}

/// A fake whose archond already runs the installed archon.
fn current_archond() -> (Fake, Plan) {
    let mut fake = Fake::new(vec![]);
    fake.running = Some(Running {
        pid: 7,
        version: "1.4.2".into(),
    });
    let current = Plan {
        archon_changed: false,
        ..plan(true)
    };
    (fake, current)
}

fn plan(dedicated: bool) -> Plan {
    Plan {
        installed: "1.4.2".into(),
        archon_changed: true,
        dedicated,
        drain_secs: 10,
        requeue: false,
    }
}

#[tokio::test]
async fn the_restart_runs_in_order_and_ends_with_the_orchestrator() {
    let fake = Fake::new(vec![
        Seen::Old { upgrading: true },
        Seen::Down(None),
        Seen::New,
    ]);
    let outcome = apply(&fake, &plan(true), &Progress::new()).await.unwrap();
    assert!(outcome.restarted && outcome.warnings.is_empty());
    let calls = fake.calls.borrow().clone();
    assert_eq!(
        calls,
        [
            "aether",
            "running",
            "request",
            "observe",
            "observe",
            "observe",
            "orchestrator"
        ]
    );
}

#[tokio::test]
async fn a_shared_aetherd_is_refreshed_never_restarted() {
    let fake = Fake::new(vec![Seen::New]);
    let outcome = apply(&fake, &plan(false), &Progress::new()).await.unwrap();
    let calls = fake.calls.borrow().clone();
    assert_eq!(calls.last(), Some(&"orchestrator"));
    assert!(calls.contains(&"mcp"), "{calls:?}");
    let said = outcome.lines.join("\n");
    assert!(said.contains("stage agents kept running"), "{said}");
}

#[tokio::test]
async fn nothing_restarts_when_archond_is_not_running_or_already_current() {
    let mut fake = Fake::new(vec![]);
    fake.running = None;
    let outcome = apply(&fake, &plan(true), &Progress::new()).await.unwrap();
    assert!(!outcome.restarted);
    assert_eq!(*fake.calls.borrow(), ["aether", "running"]);

    for creator in [Some("1.4.2"), None] {
        let (mut fake, current) = current_archond();
        fake.creator = creator;
        let outcome = apply(&fake, &current, &Progress::new()).await.unwrap();
        assert!(!outcome.restarted, "{creator:?}: {outcome:?}");
        assert_eq!(*fake.calls.borrow(), ["aether", "running", "creator"]);
    }
}

#[tokio::test]
async fn an_orchestrator_left_on_the_old_archon_is_renewed_on_a_current_archond() {
    for dedicated in [true, false] {
        let (mut fake, current) = current_archond();
        fake.creator = Some("1.4.1");
        let plan = Plan {
            dedicated,
            ..current
        };
        let outcome = apply(&fake, &plan, &Progress::new()).await.unwrap();
        assert!(outcome.restarted, "{outcome:?}");
        let calls = fake.calls.borrow().clone();
        assert!(!calls.contains(&"request"), "{calls:?}");
        assert_eq!(calls.last(), Some(&"orchestrator"));
        assert_eq!(calls.contains(&"mcp"), !dedicated, "{calls:?}");
        let said = outcome.lines.join("\n");
        assert!(said.contains("created by 1.4.1, not 1.4.2"), "{said}");
    }
}

#[tokio::test]
async fn a_renewed_orchestrator_of_another_version_fails_on_a_current_archond() {
    let (mut fake, current) = current_archond();
    (fake.creator, fake.created_by) = (Some(UNRECORDED), "1.4.1");
    let error = apply(&fake, &current, &Progress::new())
        .await
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("created its new generation, not 1.4.2"),
        "{error}"
    );
}

/// How `apply` fails when `fake` hangs, watched past `deadline` so that a
/// lost deadline fails the test instead of hanging it.
async fn hung_apply_error(
    fake: &Fake,
    plan: &Plan,
    progress: &Progress,
    deadline: Duration,
) -> String {
    let watchdog = deadline + Duration::from_secs(60);
    let ended = tokio::time::timeout(watchdog, apply(fake, plan, progress)).await;
    let result = ended.unwrap_or_else(|_| panic!("{:?} hung past {watchdog:?}", fake.hangs));
    format!(
        "{:#}",
        result.expect_err("a call that hangs fails the restart")
    )
}

#[tokio::test(start_paused = true)]
async fn an_orchestrator_check_that_never_answers_ends_at_its_deadline() {
    let (mut fake, current) = current_archond();
    fake.hangs = Some("creator");
    let (progress, started) = (Progress::new(), tokio::time::Instant::now());
    let error = hung_apply_error(&fake, &current, &progress, CHECK_DEADLINE).await;
    assert_eq!(started.elapsed(), CHECK_DEADLINE, "ended at the deadline");
    assert!(
        error.contains("which archon created the orchestrator"),
        "{error}"
    );
    assert_eq!(progress.current(), Phase::CheckingOrchestrator);
}

fn newer_aether(fake: &mut Fake) {
    fake.aether = Ok(AetherUpdate {
        changed: true,
        line: "aether newer".into(),
    });
}

#[tokio::test]
async fn a_newer_aether_restarts_archons_own_aetherd_in_the_background() {
    let (mut fake, current) = current_archond();
    newer_aether(&mut fake);
    let outcome = apply(&fake, &current, &Progress::new()).await.unwrap();
    let calls = fake.calls.borrow().clone();
    assert_eq!(calls, ["aether", "running", "creator", "aether restart"]);
    let said = outcome.lines.join("\n");
    assert!(
        said.contains("restarts on the new Aether in the background"),
        "{said}"
    );

    let mut fake = Fake::new(vec![Seen::New]);
    newer_aether(&mut fake);
    let outcome = apply(&fake, &plan(true), &Progress::new()).await.unwrap();
    let calls = fake.calls.borrow().clone();
    assert_eq!(
        calls.last(),
        Some(&"aether restart"),
        "after archond: {calls:?}"
    );
    assert!(
        outcome.restarted && outcome.warnings.is_empty(),
        "{outcome:?}"
    );

    let mut fake = Fake::new(vec![Seen::New]);
    newer_aether(&mut fake);
    let outcome = apply(&fake, &plan(false), &Progress::new()).await.unwrap();
    assert!(!fake.calls.borrow().contains(&"aether restart"));
    let said = outcome.lines.join("\n");
    assert!(
        said.contains("once you restart it"),
        "the operator's: {said}"
    );
}

#[tokio::test]
async fn an_archond_still_draining_its_own_work_restarts_later_without_holding_the_update() {
    let fake = Fake::new(vec![Seen::Old { upgrading: true }; 100]);
    let started = std::time::Instant::now();
    let outcome = apply(&fake, &plan(true), &Progress::new()).await.unwrap();
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "the fake pauses for nothing"
    );
    assert!(
        !outcome.restarted && outcome.warnings.is_empty(),
        "{outcome:?}"
    );
    let said = outcome.lines.join("\n");
    assert!(
        said.contains("restarts on archon 1.4.2 once they end, or restarts anyway"),
        "an older archond is asked to requeue: {said}"
    );
    let calls = fake.calls.borrow().clone();
    assert!(!calls.contains(&"orchestrator"), "{calls:?}");
    let polls = calls.iter().filter(|call| **call == "observe").count();
    assert_eq!(polls as u64, SWITCH_LIMIT.as_secs(), "one poll a second");
}

#[tokio::test]
async fn a_failed_aether_update_is_a_warning_and_the_restart_goes_on() {
    let mut fake = Fake::new(vec![Seen::New]);
    fake.aether = Err(anyhow::anyhow!("offline"));
    let outcome = apply(&fake, &plan(true), &Progress::new()).await.unwrap();
    assert!(outcome.restarted);
    assert_eq!(outcome.warnings.len(), 1);
    assert!(outcome.warnings[0].contains("offline"), "{outcome:?}");
}

#[tokio::test]
async fn a_restart_archond_gave_up_is_an_error_that_restarts_nothing_else() {
    let fake = Fake::new(vec![Seen::Old { upgrading: false }; 3]);
    let error = apply(&fake, &plan(true), &Progress::new())
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("gave up the restart"), "{error}");
    assert!(!fake.calls.borrow().contains(&"orchestrator"));
}

#[tokio::test]
async fn an_archond_that_never_comes_back_fails_with_the_last_reason() {
    let fake = Fake::new(vec![Seen::Down(Some("refused".into()))]);
    let error = apply(&fake, &plan(true), &Progress::new())
        .await
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("no new one answered") && error.contains("refused"),
        "{error}"
    );
}

#[tokio::test]
async fn a_failed_orchestrator_restart_is_reported_with_its_command() {
    let mut fake = Fake::new(vec![Seen::New]);
    fake.orchestrator_fails = true;
    let error = format!(
        "{:#}",
        apply(&fake, &plan(true), &Progress::new())
            .await
            .unwrap_err()
    );
    assert!(error.contains("archon orchestrator restart") && error.contains("no link"));
}

#[tokio::test(start_paused = true)]
async fn an_orchestrator_restart_that_never_answers_ends_at_its_deadline() {
    let mut fake = Fake::new(vec![Seen::New]);
    fake.hangs = Some("orchestrator");
    let (progress, started) = (Progress::new(), tokio::time::Instant::now());
    let error = hung_apply_error(&fake, &plan(true), &progress, ORCHESTRATOR_DEADLINE).await;
    assert_eq!(
        started.elapsed(),
        ORCHESTRATOR_DEADLINE,
        "ended at the deadline"
    );
    assert!(
        error.contains("the orchestrator did not restart (`archon orchestrator restart`)")
            && error.contains("did not finish it within 180s"),
        "{error}"
    );
    assert_eq!(progress.current(), Phase::RestartingOrchestrator);
}

#[tokio::test(start_paused = true)]
async fn an_archond_poll_that_never_answers_ends_at_the_restart_deadline() {
    let mut fake = Fake::new(vec![]);
    fake.hangs = Some("observe");
    let started = tokio::time::Instant::now();
    let limit = SWITCH_LIMIT;
    let error = hung_apply_error(&fake, &plan(true), &Progress::new(), limit).await;
    assert_eq!(started.elapsed(), limit, "ended at the deadline");
    assert!(
        error.contains("no new one answered within 30s")
            && error.contains("the last poll got no answer"),
        "{error}"
    );
    assert!(!fake.calls.borrow().contains(&"orchestrator"));
}

#[tokio::test(start_paused = true)]
async fn a_restart_request_that_never_answers_ends_at_the_restart_deadline() {
    let mut fake = Fake::new(vec![]);
    fake.hangs = Some("request");
    let started = tokio::time::Instant::now();
    let limit = SWITCH_LIMIT;
    let error = hung_apply_error(&fake, &plan(true), &Progress::new(), limit).await;
    assert_eq!(started.elapsed(), limit, "ended at the deadline");
    assert!(
        error.contains("was not asked to restart within 30s")
            && error.contains("did not answer the request"),
        "{error}"
    );
    assert_eq!(*fake.calls.borrow(), ["aether", "running", "request"]);
}

#[tokio::test]
async fn an_orchestrator_of_another_version_fails_the_restart() {
    let mut fake = Fake::new(vec![Seen::New]);
    fake.created_by = "1.4.1";
    let error = apply(&fake, &plan(true), &Progress::new())
        .await
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("archon 1.4.1 created its new generation, not 1.4.2"),
        "{error}"
    );
}

/// `apply` against `fake`, ended by a SIGINT after `after`, and watched past
/// it so that a signal that is not selected fails the test, not hangs it.
async fn interrupted_after(
    fake: &Fake,
    after: Duration,
) -> (Ended<anyhow::Result<Outcome>>, Duration) {
    let (progress, started) = (Progress::new(), tokio::time::Instant::now());
    let signal = async {
        tokio::time::sleep(after).await;
        Interrupt::Keyboard
    };
    let plan = plan(true);
    let ended = until_interrupted(apply(fake, &plan, &progress), &progress, signal);
    let watchdog = after + Duration::from_secs(60);
    let ended = tokio::time::timeout(watchdog, ended)
        .await
        .unwrap_or_else(|_| panic!("{:?} hung past {watchdog:?}", fake.hangs));
    (ended, started.elapsed())
}

#[tokio::test(start_paused = true)]
async fn an_interrupt_ends_a_waiting_step_at_once_and_names_it() {
    let after = Duration::from_secs(5);
    for (hangs, phase) in [
        ("aether", Phase::UpdatingAether),
        ("request", Phase::AskingArchond),
        ("observe", Phase::RestartingArchond),
        ("orchestrator", Phase::RestartingOrchestrator),
    ] {
        let mut fake = Fake::new(vec![Seen::New]);
        fake.hangs = Some(hangs);
        let (ended, waited) = interrupted_after(&fake, after).await;
        assert_eq!(
            waited, after,
            "{hangs}: ended by the signal, not a deadline"
        );
        let Ended::Interrupted { by, during } = ended else {
            panic!("{hangs}: {ended:?}");
        };
        assert_eq!((by, during), (Interrupt::Keyboard, phase), "{hangs}");
        assert_eq!(by.exit_code(), 130);
    }
}

#[tokio::test]
async fn work_that_ends_first_is_not_interrupted() {
    let fake = Fake::new(vec![Seen::New]);
    let (ended, _) = interrupted_after(&fake, Duration::from_secs(3600)).await;
    let Ended::Done(Ok(outcome)) = ended else {
        panic!("{ended:?}");
    };
    assert!(outcome.restarted, "{outcome:?}");
}

#[test]
fn every_phase_names_itself_and_how_to_finish_by_hand() {
    use Phase::*;
    for phase in [
        Preparing,
        Asking,
        UpdatingAether,
        FindingArchond,
        AskingArchond,
        RestartingArchond,
        CheckingOrchestrator,
        RefreshingTools,
        RestartingOrchestrator,
        RestartingAether,
    ] {
        assert!(!phase.describe().is_empty(), "{phase:?}");
        assert!(phase.remedy().contains('`'), "{phase:?}: a command to run");
    }
    assert!(
        RestartingOrchestrator
            .remedy()
            .contains("archon orchestrator restart")
    );
    assert!(AskingArchond.remedy().contains("may not have been asked"));
    assert!(RestartingArchond.remedy().contains("as it was asked"));
}

#[test]
fn the_announcement_names_each_restart_and_spares_the_operators_aetherd() {
    let archond = Running {
        pid: 7,
        version: "archon 1.4.1".into(),
    };
    let text = announce(&plan(true), Some(&archond));
    for part in [
        "archond (pid 7), at once",
        "stage agents keep running",
        "Archon's own aetherd restarts",
        "in the background",
        "orchestrator",
        "Your own aetherd is not touched",
    ] {
        assert!(text.contains(part), "{part}: {text}");
    }
    assert!(
        !text.contains("after the running stage agents finish"),
        "{text}"
    );
    let shared = announce(&plan(false), Some(&archond));
    assert!(!shared.contains("Archon's own aetherd"), "{shared}");
    assert!(shared.contains("shared"), "{shared}");
    assert!(announce(&plan(true), None).contains("archond is not running"));
}

#[test]
fn only_an_answer_starting_with_n_declines() {
    for yes in ["", "y", "Yes", "  \n", "sure"] {
        assert!(is_yes(yes), "{yes:?}");
    }
    for no in ["n", "No", " no "] {
        assert!(!is_yes(no), "{no:?}");
    }
}

#[tokio::test]
async fn an_archond_that_waits_for_its_agents_is_asked_to_restart_anyway_soon() {
    for (version, asked) in [
        ("archon 1.4.8", (10, true)),
        ("not a version", (10, true)),
        ("1.4.9", (1800, false)),
        ("archon 1.5.0", (1800, false)),
    ] {
        let mut fake = Fake::new(vec![Seen::New]);
        fake.running = Some(Running {
            pid: 7,
            version: version.into(),
        });
        let plan = Plan {
            drain_secs: 1800,
            ..plan(true)
        };
        let outcome = apply(&fake, &plan, &Progress::new()).await.unwrap();
        assert!(outcome.restarted, "{version}: {outcome:?}");
        assert_eq!(*fake.asked.borrow(), [asked], "{version}");
    }
}
