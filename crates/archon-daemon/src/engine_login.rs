//! The engine-login hold (ARCHITECTURE §7.3): an engine whose stage agent
//! failed on its login takes no new agent, and its failed stages wait on it,
//! until a probe finds it logged in again. Other engines go on. The holds
//! are kept in the store, so a restart keeps holding them.

use std::collections::BTreeMap;
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use archon_core::Timestamp;
use archon_core::error::ArchonError;
use archon_queue::Store;
use serde::{Deserialize, Serialize};

/// The store key of the holds: each held engine's failure and start.
pub const HOLDS_KEY: &str = "engine_login_holds";

/// One hold as the store keeps it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct StoredHold {
    error: String,
    held_at: Timestamp,
}

/// The first probe of a held engine comes this long after the hold starts.
pub const FIRST_PROBE: Duration = Duration::from_secs(60);

/// The wait between probes doubles up to this.
pub const MAX_PROBE: Duration = Duration::from_secs(30 * 60);

/// A hold this soon after the engine's last one lifted is a relapse: the
/// probe found it logged in, yet its woken agents failed on the login again.
/// It goes on with the longer wait, and the operator is not told again.
pub const RELAPSE_WINDOW: Duration = Duration::from_secs(10 * 60);

/// Why and since when one engine is held, and when it is probed next.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EngineHold {
    pub engine: String,
    /// The login failure that started the hold.
    pub error: String,
    /// When the hold started; held spawns get back the time since.
    pub held_at: Timestamp,
    /// The wait before the next probe.
    pub interval: Duration,
    pub next_probe: Instant,
    /// It started right after the engine's last hold lifted.
    pub relapse: bool,
}

/// The engine holds shared by settles, spawns and the dispatcher. The first
/// probe's wait is injected so tests probe at once.
pub struct EngineLoginWatch {
    first_probe: Duration,
    state: Mutex<WatchState>,
}

#[derive(Debug, Default)]
struct WatchState {
    held: BTreeMap<String, EngineHold>,
    /// When each engine's last hold lifted, and the wait it had reached.
    lifted: BTreeMap<String, (Instant, Duration)>,
}

impl std::fmt::Debug for EngineLoginWatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EngineLoginWatch")
            .field("state", &*self.lock())
            .finish_non_exhaustive()
    }
}

impl EngineLoginWatch {
    pub fn new(first_probe: Duration) -> Self {
        assert!(first_probe <= MAX_PROBE, "the first probe within the cap");
        Self {
            first_probe,
            state: Mutex::new(WatchState::default()),
        }
    }

    /// Probing a minute after a hold starts, as archond does.
    pub fn system() -> Self {
        Self::new(FIRST_PROBE)
    }

    fn lock(&self) -> MutexGuard<'_, WatchState> {
        self.state.lock().expect("no engine watch holder panics")
    }

    /// Hold again the engines the store says were held when archond stopped,
    /// announced already, with their first probe a wait from `now`; returns
    /// how many. An unreadable record holds nothing and is dropped, loudly.
    pub fn restore(&self, store: &mut Store, now: Instant) -> Result<usize, ArchonError> {
        let Some(raw) = store.kv_get(HOLDS_KEY)? else {
            return Ok(0);
        };
        let stored: BTreeMap<String, StoredHold> = match serde_json::from_str(&raw) {
            Ok(stored) => stored,
            Err(error) => {
                tracing::error!(%error, "the engine-login holds are unreadable; none is restored");
                store.kv_delete(HOLDS_KEY)?;
                return Ok(0);
            }
        };
        let mut state = self.lock();
        for (engine, hold) in stored {
            assert!(!engine.is_empty(), "a stored hold names its engine");
            let restored = EngineHold {
                engine: engine.clone(),
                error: hold.error,
                held_at: hold.held_at,
                interval: self.first_probe,
                next_probe: now + self.first_probe,
                relapse: false,
            };
            state.held.insert(engine, restored);
        }
        Ok(state.held.len())
    }

    /// Keep the holds as they are now in the store.
    pub fn save(&self, store: &mut Store) -> Result<(), ArchonError> {
        let stored: BTreeMap<String, StoredHold> = self
            .lock()
            .held
            .values()
            .map(|hold| {
                let kept = StoredHold {
                    error: hold.error.clone(),
                    held_at: hold.held_at,
                };
                (hold.engine.clone(), kept)
            })
            .collect();
        if stored.is_empty() {
            return store.kv_delete(HOLDS_KEY);
        }
        store.kv_set(HOLDS_KEY, &serde_json::to_string(&stored)?)
    }

    /// Hold `engine`, whose agent failed on its login with `error`. Returns
    /// the hold this starts; `None` when the engine is held already.
    pub fn report(&self, engine: &str, error: &str, now: Instant) -> Option<EngineHold> {
        assert!(!engine.is_empty(), "a model names its engine");
        assert!(!error.trim().is_empty(), "a login failure says what failed");
        let mut state = self.lock();
        if state.held.contains_key(engine) {
            return None;
        }
        let relapsed = state
            .lifted
            .get(engine)
            .filter(|(at, _)| now.saturating_duration_since(*at) < RELAPSE_WINDOW)
            .map(|(_, reached)| *reached);
        let interval = relapsed.map_or(self.first_probe, longer);
        let hold = EngineHold {
            engine: engine.to_owned(),
            error: error.to_owned(),
            held_at: archon_core::now(),
            interval,
            next_probe: now + interval,
            relapse: relapsed.is_some(),
        };
        state.held.insert(engine.to_owned(), hold.clone());
        Some(hold)
    }

    /// Whether `engine` is held now.
    pub fn is_held(&self, engine: &str) -> bool {
        self.lock().held.contains_key(engine)
    }

    /// Whether any engine is held now.
    pub fn any_held(&self) -> bool {
        !self.lock().held.is_empty()
    }

    /// Every hold, by engine.
    pub fn holds(&self) -> Vec<EngineHold> {
        self.lock().held.values().cloned().collect()
    }

    /// The held engines whose probe is due at `now`.
    pub fn due(&self, now: Instant) -> Vec<String> {
        let state = self.lock();
        let due: Vec<String> = state
            .held
            .values()
            .filter(|hold| hold.next_probe <= now)
            .map(|hold| hold.engine.clone())
            .collect();
        debug_assert!(due.len() <= state.held.len());
        due
    }

    /// A probe found `engine` still unable to run: wait twice as long, up to
    /// [`MAX_PROBE`], before the next.
    pub fn still_out(&self, engine: &str, now: Instant) {
        let mut state = self.lock();
        let Some(hold) = state.held.get_mut(engine) else {
            return;
        };
        hold.interval = longer(hold.interval);
        hold.next_probe = now + hold.interval;
        assert!(hold.interval <= MAX_PROBE, "the wait is bounded");
    }

    /// A probe found `engine` logged in: lift its hold, which is returned;
    /// `None` when it was not held.
    pub fn lift(&self, engine: &str, now: Instant) -> Option<EngineHold> {
        let mut state = self.lock();
        let hold = state.held.remove(engine)?;
        state.lifted.insert(engine.to_owned(), (now, hold.interval));
        assert!(!state.held.contains_key(engine), "the hold is lifted");
        Some(hold)
    }
}

/// The next wait after `interval`: twice as long, at least a second and at
/// most [`MAX_PROBE`]. A zero wait, as tests use, stays zero.
fn longer(interval: Duration) -> Duration {
    if interval.is_zero() {
        return interval;
    }
    let next = interval.saturating_mul(2).min(MAX_PROBE);
    assert!(next >= interval.min(MAX_PROBE), "the wait never shrinks");
    next
}

#[cfg(test)]
mod tests {
    use super::*;

    const LOGIN: &str = "model error from claude-code/opus: Auth: token expired";

    #[test]
    fn a_login_failure_holds_its_engine_once_and_no_other() {
        let watch = EngineLoginWatch::system();
        let now = Instant::now();
        let hold = watch.report("claude-code", LOGIN, now).expect("starts");
        assert_eq!((hold.interval, hold.relapse), (FIRST_PROBE, false));
        assert!(watch.report("claude-code", LOGIN, now).is_none(), "held");
        assert!(watch.is_held("claude-code"));
        assert!(!watch.is_held("codex"), "other engines go on");
        assert!(watch.due(now).is_empty(), "not probed at once");
        assert_eq!(watch.due(now + FIRST_PROBE), ["claude-code"]);
    }

    #[test]
    fn the_wait_between_probes_doubles_up_to_its_cap() {
        let watch = EngineLoginWatch::system();
        let mut now = Instant::now();
        watch.report("codex", LOGIN, now);
        let mut waits = Vec::new();
        for _ in 0..8 {
            now += Duration::from_secs(3600);
            watch.still_out("codex", now);
            waits.push(watch.holds()[0].interval.as_secs() / 60);
        }
        assert_eq!(waits, [2, 4, 8, 16, 30, 30, 30, 30]);
        assert!(watch.due(now).is_empty(), "the next probe lies ahead");
    }

    #[test]
    fn a_lifted_hold_that_fails_again_soon_is_a_relapse_with_the_longer_wait() {
        let watch = EngineLoginWatch::system();
        let start = Instant::now();
        watch.report("claude-code", LOGIN, start);
        watch.still_out("claude-code", start);
        let lifted = watch.lift("claude-code", start).expect("held");
        assert_eq!(lifted.interval, 2 * FIRST_PROBE);
        assert!(!watch.any_held() && watch.lift("claude-code", start).is_none());
        let again = watch.report("claude-code", LOGIN, start).unwrap();
        assert_eq!((again.interval, again.relapse), (4 * FIRST_PROBE, true));
        watch.lift("claude-code", start);
        let later = start + RELAPSE_WINDOW;
        let fresh = watch.report("claude-code", LOGIN, later).unwrap();
        assert_eq!((fresh.interval, fresh.relapse), (FIRST_PROBE, false));
    }
}
