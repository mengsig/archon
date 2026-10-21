//! What follows an install, so that the update takes effect now: update
//! Aether, restart archond and Archon's own aetherd through the drained
//! restart archond already has, then give the orchestrator its new tools.
//! The sequence runs against [`Steps`], so tests drive it with a fake.

use std::ffi::OsStr;
use std::path::Path;
use std::time::Duration;

use anyhow::Context as _;
use archon_core::ops::{UpgradeRequest, bare_version};
use archon_core::protocol::{Reply, Request};

use super::tools;
use crate::cmd::doctor::find_program;
use crate::connect::{connect_existing, connect_running, unexpected};
use crate::instance::AETHER_PROGRAM;
use crate::views::fetch_status;

const POLL: Duration = Duration::from_secs(1);
/// After the drain: the helper waits for archond to exit, then both restart.
const RESTART_MARGIN: Duration = Duration::from_secs(300);
const AETHER_UPDATE_TIMEOUT: Duration = Duration::from_secs(600);
/// Polls in a row that find archond not upgrading before the restart is
/// taken as given up.
const GIVE_UP_AFTER: u32 = 3;
const ORCHESTRATOR_TRIES: u32 = 60;

/// What `archon update` decided before the restart.
#[derive(Debug, Clone)]
pub(super) struct Plan {
    /// The version of the archon that is installed now.
    pub installed: String,
    pub archon_changed: bool,
    /// `[aether] mode = "dedicated"`: Archon runs its own aetherd.
    pub dedicated: bool,
    pub drain_secs: u64,
    pub requeue: bool,
}

/// archond as `observe` sees it while it restarts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Seen {
    Down(Option<String>),
    /// The archond that was asked, still running; `upgrading` is whether it
    /// still holds an upgrade.
    Old {
        upgrading: bool,
    },
    New,
}

/// A running archond: its pid and the version it runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Running {
    pub pid: u32,
    pub version: String,
}

/// The effect of `aether update`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct AetherUpdate {
    pub changed: bool,
    pub line: String,
}

/// Every effect of the restart, so tests can stand in for the machine.
pub(super) trait Steps {
    async fn update_aether(&self) -> anyhow::Result<AetherUpdate>;
    async fn running_archond(&self) -> anyhow::Result<Option<Running>>;
    async fn request_restart(&self, plan: &Plan) -> anyhow::Result<()>;
    async fn observe(&self, old_pid: u32) -> Seen;
    async fn refresh_mcp(&self) -> anyhow::Result<()>;
    async fn restart_orchestrator(&self) -> anyhow::Result<()>;
    async fn pause(&self);
}

/// What the sequence did, line by line.
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct Outcome {
    pub lines: Vec<String>,
    /// The restart ran to its end.
    pub restarted: bool,
    /// A step that failed but stopped nothing; the update then exits 1.
    pub warnings: Vec<String>,
}

/// The lines that say what is about to be updated and restarted.
pub(super) fn announce(plan: &Plan, running: Option<&Running>) -> String {
    let mut lines = vec!["The update takes effect once these restart:".to_owned()];
    match running {
        None => lines.push(
            "  - nothing: archond is not running, and the next command that needs it starts the \
             new version."
                .to_owned(),
        ),
        Some(archond) => {
            lines.push(format!(
                "  - archond (pid {}), after the running stage agents finish (at most {}s{})",
                archond.pid,
                plan.drain_secs,
                match plan.requeue {
                    true => ", then regardless: stopped stages are retried",
                    false => "; the restart is given up if they do not",
                }
            ));
            if plan.dedicated {
                lines.push("  - Archon's own aetherd; running items resume".to_owned());
            }
            lines.push(
                "  - the orchestrator agent, which is recreated with the new tools".to_owned(),
            );
        }
    }
    lines.push("Aether is updated first (`aether update`).".to_owned());
    lines.push(match plan.dedicated {
        true => "Your own aetherd is not touched.".to_owned(),
        false => "Your aetherd is not restarted: [aether] mode is \"shared\".".to_owned(),
    });
    lines.join("\n")
}

/// Run the sequence. A failure of the restart itself is an error; a failed
/// `aether update` is a warning, since archond restarts on what is installed.
pub(super) async fn apply(steps: &impl Steps, plan: &Plan) -> anyhow::Result<Outcome> {
    assert!(plan.drain_secs > 0, "a restart waits at least a second");
    let mut outcome = Outcome::default();
    let mut aether_changed = false;
    match steps.update_aether().await {
        Ok(update) => {
            aether_changed = update.changed;
            outcome.lines.push(update.line);
        }
        Err(error) => outcome
            .warnings
            .push(format!("aether was not updated: {error:#}")),
    }
    let Some(old) = steps.running_archond().await? else {
        outcome
            .lines
            .push("archond is not running; nothing to restart.".to_owned());
        return Ok(outcome);
    };
    let current = bare_version(&old.version) == bare_version(&plan.installed);
    if !plan.archon_changed && !aether_changed && current {
        let line = format!("archond already runs archon {}.", plan.installed);
        outcome.lines.push(line);
        return Ok(outcome);
    }
    restart(steps, plan, &old).await?;
    follow_up(steps, plan, &mut outcome).await?;
    outcome.restarted = true;
    Ok(outcome)
}

async fn restart(steps: &impl Steps, plan: &Plan, old: &Running) -> anyhow::Result<()> {
    steps.request_restart(plan).await?;
    let polls = (plan.drain_secs + RESTART_MARGIN.as_secs()) / POLL.as_secs();
    let (mut idle, mut last_down) = (0, None);
    for _ in 0..polls {
        steps.pause().await;
        match steps.observe(old.pid).await {
            Seen::New => return Ok(()),
            Seen::Down(why) => {
                idle = 0;
                last_down = why.or(last_down);
            }
            Seen::Old { upgrading: true } => idle = 0,
            Seen::Old { upgrading: false } => idle += 1,
        }
        anyhow::ensure!(
            idle < GIVE_UP_AFTER,
            "archond (pid {}) gave up the restart: stage agents still ran after {}s. Nothing was \
             restarted; `archon log` has the reason. Ask again with `archon update --wait SECS`, \
             or `--requeue` to restart anyway.",
            old.pid,
            plan.drain_secs
        );
    }
    let why = last_down.map_or(String::new(), |why| format!(" ({why})"));
    anyhow::bail!(
        "archond (pid {}) was asked to restart, and no new one answered within {polls}s{why}; \
         see its startup log with `archon daemon status`",
        old.pid
    )
}

async fn follow_up(steps: &impl Steps, plan: &Plan, outcome: &mut Outcome) -> anyhow::Result<()> {
    outcome.lines.push(match plan.dedicated {
        true => "Restarted archond and Archon's own aetherd.".to_owned(),
        false => "Restarted archond.".to_owned(),
    });
    if !plan.dedicated {
        steps
            .refresh_mcp()
            .await
            .context("archond restarted, but your aetherd could not refresh Archon's tools")?;
        outcome
            .lines
            .push("Refreshed Archon's tools in your aetherd.".to_owned());
    }
    steps.restart_orchestrator().await.context(
        "archond restarted, but the orchestrator did not (`archon orchestrator restart`)",
    )?;
    outcome.lines.push("Restarted the orchestrator.".to_owned());
    Ok(())
}

/// The steps on this machine.
pub(super) struct Live;

impl Steps for Live {
    async fn update_aether(&self) -> anyhow::Result<AetherUpdate> {
        let aether = find_program(Path::new(AETHER_PROGRAM))
            .context("no `aether` was found on PATH to update")?;
        let before = aether_version(&aether).await?;
        let args = [OsStr::new("update"), OsStr::new("--yes")];
        let ran = tools::run(&aether, &args, AETHER_UPDATE_TIMEOUT).await?;
        let after = aether_version(&aether).await?;
        let said = String::from_utf8_lossy(&ran.stdout);
        let line = match before == after {
            true => format!("Aether is {after}; `aether update` changed nothing."),
            false => format!("Updated Aether from {before} to {after}."),
        };
        tracing::info!(output = %said.trim(), "aether update");
        Ok(AetherUpdate {
            changed: before != after,
            line,
        })
    }

    async fn running_archond(&self) -> anyhow::Result<Option<Running>> {
        Ok(connect_existing().await?.map(|client| {
            let welcome = client.welcome();
            Running {
                pid: welcome.pid,
                version: welcome.daemon_version.clone(),
            }
        }))
    }

    async fn request_restart(&self, plan: &Plan) -> anyhow::Result<()> {
        let request = UpgradeRequest {
            archon: None,
            aether: None,
            drain_secs: plan.drain_secs,
            requeue: plan.requeue,
            keep_aether: false,
        };
        request.check()?;
        let client = connect_running().await?;
        match client.call(Request::Upgrade(request)).await? {
            Ok(Reply::UpgradeStarted { .. }) => Ok(()),
            Ok(other) => Err(unexpected(&other, "upgrade_started")),
            Err(refusal) => Err(archon_client::refusal_error(refusal).into()),
        }
    }

    async fn observe(&self, old_pid: u32) -> Seen {
        let client = match connect_existing().await {
            Ok(Some(client)) => client,
            Ok(None) => return Seen::Down(None),
            Err(error) => return Seen::Down(Some(format!("{error:#}"))),
        };
        if client.welcome().pid != old_pid {
            return Seen::New;
        }
        match fetch_status(&client).await {
            Ok(status) => Seen::Old {
                upgrading: status.upgrade.is_some(),
            },
            Err(error) => Seen::Down(Some(format!("{error:#}"))),
        }
    }

    async fn refresh_mcp(&self) -> anyhow::Result<()> {
        let aether = find_program(Path::new(AETHER_PROGRAM))
            .context("no `aether` was found on PATH to refresh Archon's tools")?;
        let args = ["mcp", "refresh", "archon"].map(OsStr::new);
        tools::run(&aether, &args, AETHER_UPDATE_TIMEOUT).await?;
        Ok(())
    }

    async fn restart_orchestrator(&self) -> anyhow::Result<()> {
        // The new archond answers before it reaches Aether, which it is told to do within seconds.
        let mut last = None;
        for _ in 0..ORCHESTRATOR_TRIES {
            let client = connect_running().await?;
            match client.call(Request::OrchestratorRestart).await? {
                Ok(_) => return Ok(()),
                Err(refusal) => last = Some(archon_client::refusal_error(refusal)),
            }
            tokio::time::sleep(POLL).await;
        }
        Err(last.context("the orchestrator was never asked")?.into())
    }

    async fn pause(&self) {
        tokio::time::sleep(POLL).await;
    }
}

async fn aether_version(aether: &Path) -> anyhow::Result<String> {
    let args = [OsStr::new("--version")];
    let output = tools::run(aether, &args, Duration::from_secs(20)).await?;
    let version = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    anyhow::ensure!(
        !version.is_empty(),
        "`{} --version` printed nothing",
        aether.display()
    );
    Ok(version)
}

/// Whether a typed answer to `[Y/n]` is a yes: empty is, as in `install.sh`.
pub(super) fn is_yes(answer: &str) -> bool {
    !answer.trim().to_ascii_lowercase().starts_with('n')
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::VecDeque;

    use super::*;

    struct Fake {
        calls: RefCell<Vec<&'static str>>,
        aether: anyhow::Result<AetherUpdate>,
        running: Option<Running>,
        seen: RefCell<VecDeque<Seen>>,
        orchestrator_fails: bool,
    }

    impl Fake {
        fn new(seen: Vec<Seen>) -> Self {
            Self {
                calls: RefCell::default(),
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
            }
        }

        fn log(&self, call: &'static str) {
            self.calls.borrow_mut().push(call);
        }
    }

    impl Steps for Fake {
        async fn update_aether(&self) -> anyhow::Result<AetherUpdate> {
            self.log("aether");
            match &self.aether {
                Ok(update) => Ok(update.clone()),
                Err(error) => Err(anyhow::anyhow!("{error:#}")),
            }
        }
        async fn running_archond(&self) -> anyhow::Result<Option<Running>> {
            self.log("running");
            Ok(self.running.clone())
        }
        async fn request_restart(&self, _: &Plan) -> anyhow::Result<()> {
            self.log("request");
            Ok(())
        }
        async fn observe(&self, _: u32) -> Seen {
            self.log("observe");
            self.seen
                .borrow_mut()
                .pop_front()
                .unwrap_or(Seen::Down(None))
        }
        async fn refresh_mcp(&self) -> anyhow::Result<()> {
            self.log("mcp");
            Ok(())
        }
        async fn restart_orchestrator(&self) -> anyhow::Result<()> {
            self.log("orchestrator");
            match self.orchestrator_fails {
                true => anyhow::bail!("no link"),
                false => Ok(()),
            }
        }
        async fn pause(&self) {}
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
        let outcome = apply(&fake, &plan(true)).await.unwrap();
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
        let outcome = apply(&fake, &plan(false)).await.unwrap();
        let calls = fake.calls.borrow().clone();
        assert_eq!(calls.last(), Some(&"orchestrator"));
        assert!(calls.contains(&"mcp"), "{calls:?}");
        assert!(outcome.lines.contains(&"Restarted archond.".to_owned()));
    }

    #[tokio::test]
    async fn nothing_restarts_when_archond_is_not_running_or_already_current() {
        let mut fake = Fake::new(vec![]);
        fake.running = None;
        let outcome = apply(&fake, &plan(true)).await.unwrap();
        assert!(!outcome.restarted);
        assert_eq!(*fake.calls.borrow(), ["aether", "running"]);

        let mut fake = Fake::new(vec![]);
        fake.running = Some(Running {
            pid: 7,
            version: "1.4.2".into(),
        });
        let current = Plan {
            archon_changed: false,
            ..plan(true)
        };
        let outcome = apply(&fake, &current).await.unwrap();
        assert!(!outcome.restarted && !fake.calls.borrow().contains(&"request"));
    }

    #[tokio::test]
    async fn a_newer_aether_restarts_an_archond_that_is_current() {
        let mut fake = Fake::new(vec![Seen::New]);
        fake.aether = Ok(AetherUpdate {
            changed: true,
            line: "aether newer".into(),
        });
        fake.running = Some(Running {
            pid: 7,
            version: "1.4.2".into(),
        });
        let current = Plan {
            archon_changed: false,
            ..plan(true)
        };
        assert!(apply(&fake, &current).await.unwrap().restarted);
    }

    #[tokio::test]
    async fn a_failed_aether_update_is_a_warning_and_the_restart_goes_on() {
        let mut fake = Fake::new(vec![Seen::New]);
        fake.aether = Err(anyhow::anyhow!("offline"));
        let outcome = apply(&fake, &plan(true)).await.unwrap();
        assert!(outcome.restarted);
        assert_eq!(outcome.warnings.len(), 1);
        assert!(outcome.warnings[0].contains("offline"), "{outcome:?}");
    }

    #[tokio::test]
    async fn a_restart_archond_gave_up_is_an_error_that_restarts_nothing_else() {
        let fake = Fake::new(vec![Seen::Old { upgrading: false }; 3]);
        let error = apply(&fake, &plan(true)).await.unwrap_err().to_string();
        assert!(error.contains("gave up the restart"), "{error}");
        assert!(!fake.calls.borrow().contains(&"orchestrator"));
    }

    #[tokio::test]
    async fn an_archond_that_never_comes_back_fails_with_the_last_reason() {
        let fake = Fake::new(vec![Seen::Down(Some("refused".into()))]);
        let error = apply(&fake, &plan(true)).await.unwrap_err().to_string();
        assert!(
            error.contains("no new one answered") && error.contains("refused"),
            "{error}"
        );
    }

    #[tokio::test]
    async fn a_failed_orchestrator_restart_is_reported_with_its_command() {
        let mut fake = Fake::new(vec![Seen::New]);
        fake.orchestrator_fails = true;
        let error = format!("{:#}", apply(&fake, &plan(true)).await.unwrap_err());
        assert!(error.contains("archon orchestrator restart") && error.contains("no link"));
    }

    #[test]
    fn the_announcement_names_each_restart_and_spares_the_operators_aetherd() {
        let archond = Running {
            pid: 7,
            version: "archon 1.4.1".into(),
        };
        let text = announce(&plan(true), Some(&archond));
        for part in [
            "archond (pid 7)",
            "Archon's own aetherd",
            "orchestrator",
            "Your own aetherd is not touched",
        ] {
            assert!(text.contains(part), "{part}: {text}");
        }
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
}
