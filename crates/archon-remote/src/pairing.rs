//! One-time pairing codes: eight Crockford base32 symbols (40 bits), open
//! for [`PAIRING_TTL`], consumed by the first right guess, closed after
//! [`MAX_FAILURES`] wrong ones, and guessed at most [`GUESSES_PER_WINDOW`]
//! times per [`GUESS_WINDOW`] whatever the peer.

use std::time::{Duration, Instant};

use archon_core::Timestamp;
use archon_core::error::ArchonError;

pub const PAIRING_TTL: Duration = Duration::from_secs(5 * 60);
pub const MAX_FAILURES: u32 = 5;
pub const GUESS_WINDOW: Duration = Duration::from_secs(10);
pub const GUESSES_PER_WINDOW: u32 = 3;
const CODE_LEN: usize = 8;
const ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

/// The open pairing. The code lives only here, in memory.
#[derive(Debug)]
pub struct Pairing {
    code: String,
    opened: Instant,
    expires_at: Timestamp,
    failures: u32,
}

/// Why a claim was refused, each with its HTTP status in `http`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    Closed,
    Expired,
    InvalidCode,
    RateLimited { retry_after: Duration },
}

/// Guesses made in the current window, across every pairing.
#[derive(Debug, Default)]
pub struct GuessLimit {
    window_start: Option<Instant>,
    guesses: u32,
}

impl GuessLimit {
    /// Count one guess at `now`, or say how long to wait.
    pub fn take(&mut self, now: Instant) -> Result<(), Duration> {
        let start = *self.window_start.get_or_insert(now);
        let elapsed = now.saturating_duration_since(start);
        if elapsed >= GUESS_WINDOW {
            self.window_start = Some(now);
            self.guesses = 0;
        }
        if self.guesses >= GUESSES_PER_WINDOW {
            let wait = GUESS_WINDOW.saturating_sub(elapsed);
            return Err(wait.max(Duration::from_secs(1)));
        }
        self.guesses += 1;
        assert!(
            self.guesses <= GUESSES_PER_WINDOW,
            "the window never overfills"
        );
        Ok(())
    }
}

impl Pairing {
    /// A new pairing with a fresh random code.
    pub fn open(now: Instant) -> Result<Self, ArchonError> {
        let mut bytes = [0u8; 5];
        getrandom::fill(&mut bytes).map_err(|e| {
            ArchonError::Unavailable(format!("no randomness for a pairing code: {e}"))
        })?;
        let bits = bytes.iter().fold(0u64, |acc, b| (acc << 8) | u64::from(*b));
        let code: String = (0..CODE_LEN)
            .rev()
            .map(|i| char::from(ALPHABET[((bits >> (i * 5)) & 31) as usize]))
            .collect();
        assert_eq!(code.len(), CODE_LEN, "40 bits make eight symbols");
        let ttl = chrono::Duration::from_std(PAIRING_TTL).expect("the TTL fits");
        Ok(Self {
            code,
            opened: now,
            expires_at: archon_core::now() + ttl,
            failures: 0,
        })
    }

    /// The code as the operator reads it: `ABCD-EFGH`.
    pub fn display_code(&self) -> String {
        format!("{}-{}", &self.code[..4], &self.code[4..])
    }

    pub fn expires_at(&self) -> Timestamp {
        self.expires_at
    }

    pub fn expired(&self, now: Instant) -> bool {
        now.saturating_duration_since(self.opened) >= PAIRING_TTL
    }

    /// Judge `typed` against the code. `Ok` consumes the pairing; the
    /// caller drops it then, and also on `Closed` from too many failures.
    pub fn claim(
        &mut self,
        typed: &str,
        now: Instant,
        limit: &mut GuessLimit,
    ) -> Result<(), Refusal> {
        if self.expired(now) {
            return Err(Refusal::Expired);
        }
        let Some(typed) = normalize_code(typed) else {
            return Err(Refusal::InvalidCode);
        };
        limit
            .take(now)
            .map_err(|retry_after| Refusal::RateLimited { retry_after })?;
        if constant_time_eq(typed.as_bytes(), self.code.as_bytes()) {
            return Ok(());
        }
        self.failures += 1;
        if self.failures >= MAX_FAILURES {
            return Err(Refusal::Closed);
        }
        Err(Refusal::InvalidCode)
    }
}

/// The eight symbols of a typed code: case, dashes and spaces ignored, and
/// the Crockford look-alikes (`O`, `I`, `L`) read as digits.
pub fn normalize_code(typed: &str) -> Option<String> {
    let symbols: String = typed
        .chars()
        .filter(|c| !matches!(c, '-' | ' '))
        .map(|c| match c.to_ascii_uppercase() {
            'O' => '0',
            'I' | 'L' => '1',
            other => other,
        })
        .collect();
    let valid = symbols.len() == CODE_LEN && symbols.bytes().all(|b| ALPHABET.contains(&b));
    valid.then_some(symbols)
}

/// Compares without an early exit, so timing says nothing of a prefix.
pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_are_eight_crockford_symbols_and_differ() {
        let now = Instant::now();
        let a = Pairing::open(now).unwrap();
        let b = Pairing::open(now).unwrap();
        assert_eq!(a.code.len(), 8);
        assert!(a.code.bytes().all(|c| ALPHABET.contains(&c)));
        assert_ne!(a.code, b.code, "40 random bits do not repeat");
        assert_eq!(a.display_code().len(), 9);
    }

    #[test]
    fn typed_codes_ignore_case_dashes_and_look_alikes() {
        assert_eq!(normalize_code("abcd-efgh").as_deref(), Some("ABCDEFGH"));
        assert_eq!(
            normalize_code("o1l2 i3x4").as_deref(),
            Some("0112 13X4".replace(' ', "").as_str())
        );
        assert_eq!(normalize_code("ABCDEFG"), None);
        assert_eq!(
            normalize_code("ABCDEFGU"),
            None,
            "U is not a Crockford symbol"
        );
    }

    #[test]
    fn the_right_code_is_accepted_and_wrong_ones_close_the_pairing() {
        let now = Instant::now();
        let mut pairing = Pairing::open(now).unwrap();
        let code = pairing.code.clone();
        let wrong = if code.starts_with('0') {
            "11111111"
        } else {
            "00000000"
        };
        for attempt in 1..MAX_FAILURES {
            // A fresh window each time, so only the failure count matters.
            let mut limit = GuessLimit::default();
            let at = now + Duration::from_secs(u64::from(attempt));
            assert_eq!(
                pairing.claim(wrong, at, &mut limit),
                Err(Refusal::InvalidCode)
            );
        }
        let mut limit = GuessLimit::default();
        assert_eq!(pairing.claim(wrong, now, &mut limit), Err(Refusal::Closed));
        let mut fresh = Pairing::open(now).unwrap();
        let code = fresh.code.clone();
        assert_eq!(fresh.claim(&code.to_lowercase(), now, &mut limit), Ok(()));
        assert_ne!(code, "");
    }

    #[test]
    fn guesses_are_rate_limited_and_expired_pairings_refused() {
        let now = Instant::now();
        let mut pairing = Pairing::open(now).unwrap();
        let mut limit = GuessLimit::default();
        for _ in 0..GUESSES_PER_WINDOW {
            limit.take(now).unwrap();
        }
        let refused = pairing.claim("ZZZZZZZZ", now, &mut limit);
        assert!(
            matches!(refused, Err(Refusal::RateLimited { .. })),
            "{refused:?}"
        );
        assert!(limit.take(now + GUESS_WINDOW).is_ok(), "a new window opens");
        let late = now + PAIRING_TTL;
        assert_eq!(
            pairing.claim("ZZZZZZZZ", late, &mut limit),
            Err(Refusal::Expired)
        );
        assert_eq!(
            pairing.failures, 0,
            "refused guesses do not count as failures"
        );
    }
}
