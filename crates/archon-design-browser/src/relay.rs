//! Latest-frame relay: one held frame, at most 10 fps, next only after the subscriber's ack.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::Notify;
use tokio::time::Instant;

use crate::Frame;

const MIN_INTERVAL: Duration = Duration::from_millis(100);

#[derive(Debug, Default)]
pub(crate) struct RelayShared {
    latest: Mutex<Option<Frame>>,
    seq: AtomicU64,
    notify: Notify,
    closed: AtomicBool,
}

impl RelayShared {
    pub(crate) fn push(&self, jpeg: Vec<u8>, width: u32, height: u32) -> u64 {
        let seq = self.seq.fetch_add(1, Ordering::SeqCst) + 1;
        *self.latest.lock().unwrap() = Some(Frame {
            seq,
            jpeg,
            width,
            height,
        });
        self.notify.notify_waiters();
        seq
    }

    pub(crate) fn seq(&self) -> u64 {
        self.seq.load(Ordering::SeqCst)
    }

    pub(crate) fn close(&self) {
        self.closed.store(true, Ordering::SeqCst);
        self.notify.notify_waiters();
    }

    pub(crate) async fn wait_seq_after(&self, since: u64, max: Duration) -> bool {
        let deadline = Instant::now() + max;
        loop {
            let notified = self.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.seq() > since {
                return true;
            }
            if self.closed.load(Ordering::SeqCst) {
                return false;
            }
            if tokio::time::timeout_at(deadline, notified).await.is_err() {
                return self.seq() > since;
            }
        }
    }
}

/// Handle on the screencast's frames; subscribe to receive them.
#[derive(Debug, Clone)]
pub struct FrameRelay {
    pub(crate) shared: Arc<RelayShared>,
}

impl FrameRelay {
    /// A new subscriber with its own pacing state.
    pub fn subscribe(&self) -> FrameSubscriber {
        FrameSubscriber {
            shared: self.shared.clone(),
            last_seq: 0,
            in_flight: None,
            last_release: None,
        }
    }

    /// Sequence number of the newest frame (0 before any).
    pub fn latest_seq(&self) -> u64 {
        self.shared.seq()
    }
}

/// One consumer of frames.
#[derive(Debug)]
pub struct FrameSubscriber {
    shared: Arc<RelayShared>,
    last_seq: u64,
    in_flight: Option<u64>,
    last_release: Option<Instant>,
}

impl FrameSubscriber {
    /// The newest frame not yet delivered, once the previous one was acked and 100 ms passed.
    /// Cancel-safe. `None` when the relay is closed.
    pub async fn next(&mut self) -> Option<Frame> {
        loop {
            let notified = self.shared.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.shared.closed.load(Ordering::SeqCst) {
                return None;
            }
            if self.in_flight.is_none() {
                let frame = self
                    .shared
                    .latest
                    .lock()
                    .unwrap()
                    .as_ref()
                    .filter(|f| f.seq > self.last_seq)
                    .cloned();
                if let Some(f) = frame {
                    let wait = self
                        .last_release
                        .map(|t| MIN_INTERVAL.saturating_sub(t.elapsed()))
                        .unwrap_or_default();
                    if wait.is_zero() {
                        self.last_seq = f.seq;
                        self.in_flight = Some(f.seq);
                        self.last_release = Some(Instant::now());
                        return Some(f);
                    }
                    tokio::time::sleep(wait).await;
                    continue;
                }
            }
            notified.await;
        }
    }

    /// The subscriber has shown frame `seq`; the next may be released.
    pub fn ack(&mut self, seq: u64) {
        if self.in_flight.is_some_and(|s| seq >= s) {
            self.in_flight = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn relay() -> FrameRelay {
        FrameRelay {
            shared: Arc::new(RelayShared::default()),
        }
    }

    // T-I4
    #[tokio::test(start_paused = true)]
    async fn slow_subscriber_gets_only_latest() {
        let r = relay();
        let mut sub = r.subscribe();
        for i in 0..5u8 {
            r.shared.push(vec![i], 1, 1);
        }
        let f = sub.next().await.unwrap();
        assert_eq!((f.seq, f.jpeg), (5, vec![4]));
        // Not acked: nothing more, even with new frames.
        r.shared.push(vec![9], 1, 1);
        assert!(
            tokio::time::timeout(Duration::from_secs(2), sub.next())
                .await
                .is_err()
        );
        sub.ack(5);
        let t0 = Instant::now();
        let f = sub.next().await.unwrap();
        assert_eq!(f.seq, 6);
        assert!(t0.elapsed() <= Duration::from_millis(100));
        sub.ack(6);
        // Rate limit: 10 fps.
        r.shared.push(vec![1], 1, 1);
        let t1 = Instant::now();
        let f = sub.next().await.unwrap();
        assert_eq!(f.seq, 7);
        assert!(t1.elapsed() <= MIN_INTERVAL);
        assert_eq!(r.latest_seq(), 7);
    }

    #[tokio::test(start_paused = true)]
    async fn close_ends_next() {
        let r = relay();
        let mut sub = r.subscribe();
        r.shared.close();
        assert!(sub.next().await.is_none());
    }
}
