//! Verifying the integration branch's head, and bisecting a red head over
//! the merges since the last green commit to the first red one.

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
const NOTHING_VERIFIES: &str =
    "nothing verifies it: set `[checks] full` or run GitHub CI on the branch";

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

/// Verify the head `reading` names, and bisect it when it is red.
pub(super) async fn head(
    ctx: &DaemonContext,
    watch: &Watch,
    state: &mut BranchState,
    reading: &Reading,
) -> Result<(), ArchonError> {
    let commit = reading.head.commit.clone();
    assert_eq!(state.head.as_ref(), Some(&commit), "the head was observed");
    let earlier_red = state.verdict.clone().filter(|v| !v.green);
    let mut session = Session {
        ctx,
        watch,
        head: &reading.head,
        checkout: None,
    };
    let verdict = |green, via: &str, excerpt| Verdict {
        commit: commit.clone(),
        green,
        at: archon_core::now(),
        via: via.to_owned(),
        excerpt,
    };
    match session.verify(state, &commit).await? {
        Outcome::Pending => Ok(()),
        Outcome::Unverifiable(why) => {
            state.set_problem(&why);
            Ok(())
        }
        Outcome::Green(via) => {
            super::turned_green(ctx, watch, state, verdict(true, via, String::new())).await
        }
        Outcome::Red { via, excerpt } => {
            state.verdict = Some(verdict(false, via, excerpt));
            state.culprit = session.culprit(state, reading, earlier_red).await?;
            Ok(())
        }
    }
}

/// One look's verifications, sharing the checkout once it is made.
struct Session<'a> {
    ctx: &'a DaemonContext,
    watch: &'a Watch,
    head: &'a Head,
    checkout: Option<Arc<Checkout>>,
}

impl Session<'_> {
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
        let binary = &self.ctx.config.github.binary;
        let read = github::commit_checks(binary, &self.watch.repo.path, github, commit).await;
        match read {
            Ok(CommitChecks::Passed) => Ok(Some(Outcome::Green(VIA_GITHUB))),
            Ok(CommitChecks::Failed(names)) => Ok(Some(Outcome::Red {
                via: VIA_GITHUB,
                excerpt: format!("failing GitHub checks: {}", names.join(", ")),
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
        let ran = self.run_at(&slot, commit, command).await;
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
        slot: &Slot,
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

    /// Whether `commit` is red; `None` when it could not be told.
    async fn probe(
        &mut self,
        state: &mut BranchState,
        commit: &str,
    ) -> Result<Option<bool>, ArchonError> {
        Ok(match self.verify(state, commit).await? {
            Outcome::Green(_) => Some(false),
            Outcome::Red { .. } => Some(true),
            Outcome::Pending | Outcome::Unverifiable(_) => None,
        })
    }

    /// The first red merge since the last green commit; `None` when it
    /// cannot be told. A culprit found for an earlier red head stands.
    async fn culprit(
        &mut self,
        state: &mut BranchState,
        reading: &Reading,
        earlier_red: Option<Verdict>,
    ) -> Result<Option<Culprit>, ArchonError> {
        let merges = &reading.since_green;
        let known = state.culprit.clone().filter(|c| merges.contains(&c.commit));
        if let (Some(known), Some(_)) = (known, &earlier_red) {
            return Ok(Some(known));
        }
        if merges.is_empty() || !self.baseline_green(state, reading).await? {
            return Ok(None);
        }
        let known_bad = earlier_red.and_then(|v| merges.iter().position(|m| *m == v.commit));
        let mut search = Bisect::new(merges.len(), known_bad);
        while let Some(index) = search.next() {
            let Some(red) = self.probe(state, &merges[index]).await? else {
                return Ok(None);
            };
            search.record(index, red);
        }
        let commit = merges[search.first_bad().expect("the search ended")].clone();
        let subject = {
            let (source, commit) = (self.watch.source.clone(), commit.clone());
            blocking(move || source.subject(&commit)).await?
        };
        Ok(Some(Culprit { commit, subject }))
    }

    /// A recorded green baseline is green; the release branch's fork point
    /// is verified first, as a red one leaves no merge to blame.
    async fn baseline_green(
        &mut self,
        state: &mut BranchState,
        reading: &Reading,
    ) -> Result<bool, ArchonError> {
        if reading.green.is_some() {
            return Ok(true);
        }
        let Some(base) = &reading.baseline else {
            return Ok(false);
        };
        Ok(self.probe(state, base).await? == Some(false))
    }
}
