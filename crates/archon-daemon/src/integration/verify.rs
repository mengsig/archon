//! Verifying the integration branch's head, and bisecting a red head over
//! the merges since the last green commit to the first red one. A bisection
//! left unfinished (CI still running, a failed probe, a restart) is kept in
//! the branch's state and resumed on a later look.

use std::path::PathBuf;
use std::sync::Arc;

use archon_core::error::ArchonError;

use super::bisect::Bisect;
use super::branch::{Checkout, Head};
use super::full::{self, FullRun, Slot};
use super::github::{self, CommitChecks, GithubRepo};
use super::state::{self, BranchState, Culprit, Verdict};
use super::{Reading, Watch, blocking};
use crate::context::DaemonContext;

pub(super) const VIA_FULL: &str = "checks.full";
pub(super) const VIA_GITHUB: &str = "GitHub checks";
const NOTHING_VERIFIES: &str = "nothing verifies it: set `archon repo set --full-check`, confirm the committed \
     `[checks] full` with `--confirm-full-check`, or run GitHub CI on the branch";

/// How one commit fared.
#[derive(Debug)]
enum Outcome {
    Green(&'static str),
    Red {
        via: &'static str,
        excerpt: String,
    },
    /// GitHub's checks still run.
    Pending,
    Unverifiable(String),
}

/// Where a bisection got to.
#[derive(Debug)]
enum Search {
    Found(Culprit),
    /// No merge can be blamed: the baseline is red, or a merge cannot be verified.
    Blameless,
    /// A probe waits for GitHub's checks.
    Waiting,
}

/// Verify the head `reading` names, and bisect it when it is red.
pub(super) async fn head(
    ctx: &DaemonContext,
    watch: &Watch,
    state: &mut BranchState,
    reading: &Reading,
) -> Result<(), ArchonError> {
    let commit = &reading.head.commit;
    assert_eq!(state.head.as_ref(), Some(commit), "the head was observed");
    let mut session = Session::new(ctx, watch, &reading.head);
    let outcome = session.verify(state, commit).await?;
    session.judge(state, reading, outcome).await
}

/// Read GitHub's checks again on a head they judged, as a rerun may turn
/// them: a turned verdict is judged afresh, a running rerun shown.
pub(super) async fn recheck(
    ctx: &DaemonContext,
    watch: &Watch,
    state: &mut BranchState,
    reading: &Reading,
) -> Result<(), ArchonError> {
    let held = state
        .verdict_on_head()
        .cloned()
        .expect("a verdict on the head");
    assert_eq!(
        held.via, VIA_GITHUB,
        "only GitHub's verdicts are read again"
    );
    assert!(
        state.bisecting.is_none(),
        "an owed bisection is resumed instead"
    );
    let Some(github) = &watch.github else {
        return Ok(());
    };
    let (binary, repo) = (&ctx.config.github.binary, &watch.repo);
    let outcome = match github::commit_checks(binary, repo, github, &held.commit).await? {
        CommitChecks::Passed if !held.green => Outcome::Green(VIA_GITHUB),
        // Only a turned verdict reads the failing job's log.
        CommitChecks::Failed(failed) if held.green => Outcome::Red {
            via: VIA_GITHUB,
            excerpt: github::failure_excerpt(binary, repo, github, &failed).await,
        },
        CommitChecks::Pending => Outcome::Pending,
        CommitChecks::Passed | CommitChecks::Failed(_) | CommitChecks::None => return Ok(()),
    };
    Session::new(ctx, watch, &reading.head)
        .judge(state, reading, outcome)
        .await
}

/// Go on with the bisection the red head still owes.
pub(super) async fn resume(
    ctx: &DaemonContext,
    watch: &Watch,
    state: &mut BranchState,
    reading: &Reading,
) -> Result<(), ArchonError> {
    assert!(state.bisecting.is_some(), "a bisection is owed");
    assert!(
        state.verdict_on_head().is_some_and(|v| !v.green),
        "only a red head is bisected"
    );
    Session::new(ctx, watch, &reading.head)
        .bisect(state, reading)
        .await
}

/// The earliest commit known red among the merges since green: where an
/// unfinished bisection stood, else the head an earlier look found red.
fn known_red(state: &BranchState, reading: &Reading) -> Option<String> {
    let earlier_red = state.verdict.as_ref().filter(|v| !v.green);
    let earliest = state
        .bisecting
        .clone()
        .or_else(|| earlier_red.map(|v| v.commit.clone()));
    earliest.filter(|commit| reading.since_green.contains(commit))
}

/// What a commit's outcome tells a bisection: whether it is red, or why
/// the search stops.
fn told(outcome: Outcome) -> Result<bool, Search> {
    match outcome {
        Outcome::Green(_) => Ok(false),
        Outcome::Red { .. } => Ok(true),
        Outcome::Pending => Err(Search::Waiting),
        Outcome::Unverifiable(_) => Err(Search::Blameless),
    }
}

/// One look's verifications, sharing the checkout once it is made.
struct Session<'a> {
    ctx: &'a DaemonContext,
    watch: &'a Watch,
    head: &'a Head,
    checkout: Option<Arc<Checkout>>,
}

impl<'a> Session<'a> {
    fn new(ctx: &'a DaemonContext, watch: &'a Watch, head: &'a Head) -> Self {
        Self {
            ctx,
            watch,
            head,
            checkout: None,
        }
    }

    /// Take the head's `outcome`: a red head is bisected unless the culprit
    /// an earlier look found still stands; running GitHub checks show as verifying.
    async fn judge(
        &mut self,
        state: &mut BranchState,
        reading: &Reading,
        outcome: Outcome,
    ) -> Result<(), ArchonError> {
        let commit = reading.head.commit.clone();
        assert_eq!(state.head.as_ref(), Some(&commit), "the head was observed");
        let known_red = known_red(state, reading);
        let culprit_stands = state.bisecting.is_none()
            && known_red.is_some()
            && state
                .culprit
                .as_ref()
                .is_some_and(|c| reading.since_green.contains(&c.commit));
        let verdict = |green, via: &str, excerpt| Verdict {
            commit: commit.clone(),
            green,
            at: archon_core::now(),
            via: via.to_owned(),
            excerpt,
        };
        match outcome {
            Outcome::Pending => {
                state.verifying = Some(commit.clone());
                Ok(())
            }
            Outcome::Unverifiable(why) => {
                state.set_problem(&why);
                Ok(())
            }
            Outcome::Green(via) => {
                let green = verdict(true, via, String::new());
                super::turned_green(self.ctx, self.watch, state, green).await
            }
            Outcome::Red { via, excerpt } => {
                state.verdict = Some(verdict(false, via, excerpt));
                if culprit_stands {
                    return Ok(());
                }
                state.culprit = None;
                state.bisecting = Some(known_red.unwrap_or(commit));
                self.bisect(state, reading).await
            }
        }
    }

    /// GitHub's checks on `commit` when they cover it, else the full check.
    async fn verify(
        &mut self,
        state: &mut BranchState,
        commit: &str,
    ) -> Result<Outcome, ArchonError> {
        if let Some(github) = &self.watch.github
            && let Some(outcome) = self.github(github, commit).await?
        {
            return Ok(outcome);
        }
        let Some(command) = self.watch.full.clone() else {
            return Ok(Outcome::Unverifiable(NOTHING_VERIFIES.to_owned()));
        };
        self.full(state, commit, &command).await
    }

    /// `None` when GitHub has no check on `commit`, or cannot be read while
    /// the full check can stand in.
    async fn github(
        &self,
        github: &GithubRepo,
        commit: &str,
    ) -> Result<Option<Outcome>, ArchonError> {
        let (binary, repo) = (&self.ctx.config.github.binary, &self.watch.repo);
        match github::commit_checks(binary, repo, github, commit).await {
            Ok(CommitChecks::Passed) => Ok(Some(Outcome::Green(VIA_GITHUB))),
            Ok(CommitChecks::Failed(failed)) => Ok(Some(Outcome::Red {
                via: VIA_GITHUB,
                excerpt: github::failure_excerpt(binary, repo, github, &failed).await,
            })),
            Ok(CommitChecks::Pending) => Ok(Some(Outcome::Pending)),
            Ok(CommitChecks::None) => Ok(None),
            Err(error) if error.is_store() => Err(error),
            Err(error) if self.watch.full.is_some() => {
                tracing::warn!(repo = %self.watch.repo.name, %error, "GitHub checks unread; running the full check");
                Ok(None)
            }
            Err(error) => Ok(Some(Outcome::Unverifiable(format!(
                "GitHub's checks could not be read: {error}"
            )))),
        }
    }

    /// Run `command` on `commit` in the host's build slot, the branch shown
    /// as verifying meanwhile.
    async fn full(
        &mut self,
        state: &mut BranchState,
        commit: &str,
        command: &str,
    ) -> Result<Outcome, ArchonError> {
        let slot = Slot::acquire(&full::slot_lock(&self.ctx.config.integration)).await?;
        let repo = &self.watch.repo.id;
        state.verifying = Some(commit.to_owned());
        state::save(&mut *self.ctx.store.lock().await, repo, state)?;
        let ran = self.run_at(slot, commit, command).await;
        state.verifying = None;
        state::save(&mut *self.ctx.store.lock().await, repo, state)?;
        let ran = ran?;
        tracing::info!(repo = %self.watch.repo.name, commit, passed = ran.passed, "full check ran");
        Ok(match ran.passed {
            true => Outcome::Green(VIA_FULL),
            false => Outcome::Red {
                via: VIA_FULL,
                excerpt: ran.excerpt,
            },
        })
    }

    async fn run_at(
        &mut self,
        slot: Slot,
        commit: &str,
        command: &str,
    ) -> Result<FullRun, ArchonError> {
        let checkout = self.checkout().await?;
        let work = {
            let (checkout, commit) = (checkout.clone(), commit.to_owned());
            blocking(move || checkout.at(&commit).map(PathBuf::from)).await?
        };
        let limit = self.ctx.config.integration.full_timeout.duration();
        full::run(slot, command, &work, &checkout.log, limit).await
    }

    /// The checkout, made and fetched on first use.
    async fn checkout(&mut self) -> Result<Arc<Checkout>, ArchonError> {
        if let Some(checkout) = &self.checkout {
            return Ok(checkout.clone());
        }
        let (source, name) = (self.watch.source.clone(), self.watch.repo.name.clone());
        let head = self.head.clone();
        let made = blocking(move || Checkout::prepare(&source, &name, &head)).await?;
        let made = Arc::new(made);
        self.checkout = Some(made.clone());
        Ok(made)
    }

    /// Search from `state.bisecting` for the first red merge. A search
    /// waiting on GitHub stays owed until the red verdict is `full_timeout` old.
    async fn bisect(
        &mut self,
        state: &mut BranchState,
        reading: &Reading,
    ) -> Result<(), ArchonError> {
        let bound = state.bisecting.clone().expect("a bisection is owed");
        match self.search(state, reading, &bound).await? {
            Search::Found(culprit) => state.culprit = Some(culprit),
            Search::Blameless => {}
            Search::Waiting if !self.waited_out(state) => return Ok(()),
            Search::Waiting => {
                tracing::warn!(repo = %self.watch.repo.name, "GitHub's checks never finished; the fix names no merge");
            }
        }
        state.bisecting = None;
        Ok(())
    }

    fn waited_out(&self, state: &BranchState) -> bool {
        let red_at = state.verdict_on_head().expect("a red head").at;
        let limit = self.ctx.config.integration.full_timeout.duration();
        let limit = chrono::Duration::from_std(limit).unwrap_or(chrono::Duration::MAX);
        archon_core::now() - red_at >= limit
    }

    /// The first red merge since the last green commit, `known_red` the
    /// earliest merge already known red.
    async fn search(
        &mut self,
        state: &mut BranchState,
        reading: &Reading,
        known_red: &str,
    ) -> Result<Search, ArchonError> {
        let merges = &reading.since_green;
        if merges.is_empty() {
            return Ok(Search::Blameless);
        }
        if let Some(stop) = self.baseline_not_green(state, reading).await? {
            return Ok(stop);
        }
        let known_bad = merges.iter().position(|m| m == known_red);
        let mut search = Bisect::new(merges.len(), known_bad);
        while let Some(index) = search.next() {
            match told(self.verify(state, &merges[index]).await?) {
                Ok(red) => search.record(index, red),
                Err(stop) => return Ok(stop),
            }
        }
        let commit = merges[search.first_bad().expect("the search ended")].clone();
        assert!(merges.contains(&commit), "the culprit is one of the merges");
        let subject = {
            let (source, commit) = (self.watch.source.clone(), commit.clone());
            blocking(move || source.subject(&commit)).await?
        };
        Ok(Search::Found(Culprit { commit, subject }))
    }

    /// `None` when the baseline is green: a recorded green commit is, and
    /// the release branch's fork point is verified first, as a red one
    /// leaves no merge to blame.
    async fn baseline_not_green(
        &mut self,
        state: &mut BranchState,
        reading: &Reading,
    ) -> Result<Option<Search>, ArchonError> {
        if reading.green.is_some() {
            return Ok(None);
        }
        let Some(base) = &reading.baseline else {
            return Ok(Some(Search::Blameless));
        };
        Ok(match told(self.verify(state, base).await?) {
            Ok(false) => None,
            Ok(true) => Some(Search::Blameless),
            Err(stop) => Some(stop),
        })
    }
}
