//! The operator's stream: latest-wins frames with the client's acks, at most
//! 10 a second, and the session's events.

use std::sync::Arc;
use std::time::{Duration, Instant};

use archon_core::design::MAX_SERIALIZED_FRAME_BYTES;
use archon_core::error::ArchonError;
use archon_core::protocol::{DesignEvent, ErrorReply, Reply, ServerFrame};
use archon_remote::DesignSubscription;
use base64::Engine as _;
use tokio::sync::{mpsc, watch};

use crate::context::DaemonContext;

/// Shortest gap between two frames to one subscriber: 10 a second.
const FRAME_GAP: Duration = Duration::from_millis(100);
/// How long a frame waits for the previous one's ack before the next goes.
const ACK_WAIT: Duration = Duration::from_secs(2);

pub(crate) struct AckState {
    pub acked: watch::Sender<u64>,
}

impl super::DesignService {
    /// `design_frame_ack`: late acks of a gone stream are ignored.
    pub(crate) fn frame_ack(&self, sub: u64, seq: u64) {
        let Some(device) = super::current_device() else {
            return;
        };
        let state = self
            .acks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&(device.id, sub))
            .cloned();
        if let Some(state) = state {
            state.acked.send_if_modified(|current| {
                if seq > *current {
                    *current = seq;
                    true
                } else {
                    false
                }
            });
        }
    }
}

pub async fn stream(
    ctx: Arc<DaemonContext>,
    sub: DesignSubscription,
    frames: mpsc::Sender<ServerFrame>,
) {
    let service = ctx.design.clone();
    let opened = async {
        if !service.enabled() {
            return Err(ArchonError::Invalid(
                "design sessions are turned off".into(),
            ));
        }
        service.current(&ctx, &sub.session).await
    }
    .await;
    let reply = |result: Result<Reply, ErrorReply>| ServerFrame::Reply {
        id: sub.request,
        result,
    };
    let session = match opened {
        Ok(session) => session,
        Err(error) => {
            let _ = frames.send(reply(Err(ErrorReply::from(&error)))).await;
            return;
        }
    };
    let subscribed = Reply::Subscribed {
        id: sub.id,
        last_seq: 0,
    };
    if frames.send(reply(Ok(subscribed))).await.is_err() {
        return;
    }
    let (ack_tx, mut ack_rx) = watch::channel(0u64);
    // Counted before any await; dropped, even when the task is aborted,
    // it gives the subscriber back and removes the ack entry.
    let _guard = Subscribed::new(
        service.clone(),
        session.id.clone(),
        (sub.device.id.clone(), sub.id),
        ack_tx,
    );
    if let Err(error) = service.subscriber_joined(&ctx, &session.id).await {
        tracing::warn!(%error, session = %session.id, "design: the preview did not start for a subscriber");
        let _ = send(
            &frames,
            sub.id,
            DesignEvent::Toast {
                message: format!("The preview did not start: {error}"),
            },
        )
        .await;
    }
    run(&ctx, &sub, &session.id, &frames, &mut ack_rx).await;
}

/// One attached subscriber: its count and its ack entry live as long as it.
struct Subscribed {
    service: Arc<super::DesignService>,
    session: archon_core::design::DesignSessionId,
    key: (String, u64),
}

impl Subscribed {
    fn new(
        service: Arc<super::DesignService>,
        session: archon_core::design::DesignSessionId,
        key: (String, u64),
        ack: watch::Sender<u64>,
    ) -> Self {
        service
            .acks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(key.clone(), Arc::new(AckState { acked: ack }));
        service.subscriber_counted(&session);
        Self {
            service,
            session,
            key,
        }
    }
}

impl Drop for Subscribed {
    fn drop(&mut self) {
        self.service.subscriber_left(&self.session);
        self.service
            .acks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&self.key);
    }
}

async fn send(frames: &mpsc::Sender<ServerFrame>, sub: u64, event: DesignEvent) -> Result<(), ()> {
    frames
        .send(ServerFrame::Design { sub, event })
        .await
        .map_err(|_| ())
}

async fn run(
    ctx: &Arc<DaemonContext>,
    sub: &DesignSubscription,
    session: &archon_core::design::DesignSessionId,
    frames: &mpsc::Sender<ServerFrame>,
    ack_rx: &mut watch::Receiver<u64>,
) {
    let service = &ctx.design;
    let rt = service.runtime(session);
    let mut events = rt.events.subscribe();
    let mut live = rt.frames.subscribe();
    // Subscribing marks the current frame seen; a late joiner still gets it.
    live.mark_changed();
    service.emit_session(ctx, session).await;
    let mut last_seq = 0u64;
    let mut last_sent = Instant::now() - FRAME_GAP;
    let mut pending_ack: Option<u64> = None;
    loop {
        tokio::select! {
            changed = live.changed() => {
                if changed.is_err() {
                    return;
                }
                // Latest wins: whatever arrived while this waited is skipped.
                let gap = last_sent.elapsed();
                if gap < FRAME_GAP {
                    tokio::time::sleep(FRAME_GAP - gap).await;
                }
                if let Some(seq) = pending_ack {
                    let waited = tokio::time::timeout(ACK_WAIT, async {
                        while *ack_rx.borrow() < seq {
                            if ack_rx.changed().await.is_err() {
                                break;
                            }
                        }
                    })
                    .await;
                    let _ = waited;
                }
                let Some(frame) = live.borrow_and_update().clone() else {
                    continue;
                };
                if frame.seq <= last_seq {
                    continue;
                }
                let event = DesignEvent::Frame {
                    seq: frame.seq,
                    jpeg_b64: base64::engine::general_purpose::STANDARD.encode(&frame.jpeg),
                    w: frame.width,
                    h: frame.height,
                    at: archon_core::now(),
                };
                let outgoing = ServerFrame::Design { sub: sub.id, event };
                let size = serde_json::to_vec(&outgoing).map_or(usize::MAX, |b| b.len());
                if size > MAX_SERIALIZED_FRAME_BYTES {
                    rt.st().counters.frames_dropped += 1;
                    last_seq = frame.seq;
                    continue;
                }
                if frames.send(outgoing).await.is_err() {
                    return;
                }
                last_seq = frame.seq;
                last_sent = Instant::now();
                pending_ack = Some(frame.seq);
                let mut st = rt.st();
                st.counters.frames_sent += 1;
                if let Some(at) = st.pending_input.take() {
                    st.counters.latencies_ms.push(at.elapsed().as_millis() as u64);
                }
            }
            event = events.recv() => match event {
                Ok(event) => {
                    let ended = matches!(event, DesignEvent::Ended { .. });
                    if send(frames, sub.id, event).await.is_err() || ended {
                        return;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
            },
            _ = frames.closed() => return,
        }
    }
}
