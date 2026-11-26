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

pub async fn stream(
    ctx: Arc<DaemonContext>,
    sub: DesignSubscription,
    mut acked: watch::Receiver<u64>,
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
    assert_eq!(
        session.id, sub.session,
        "the subscription opens its named session"
    );
    assert_eq!(sub.device.role, archon_core::protocol::DeviceRole::Operator);
    let subscribed = Reply::Subscribed {
        id: sub.id,
        last_seq: 0,
    };
    if frames.send(reply(Ok(subscribed))).await.is_err() {
        return;
    }
    service.subscriber_counted(&session.id);
    let _guard = Subscribed {
        service: service.clone(),
        session: session.id.clone(),
    };
    let rt = service.runtime(&session.id);
    let events = rt.events.subscribe();
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
    run(&ctx, &sub, rt, events, &frames, &mut acked).await;
}

/// Give back the subscriber count even if the connection aborts this task.
struct Subscribed {
    service: Arc<super::DesignService>,
    session: archon_core::design::DesignSessionId,
}

impl Drop for Subscribed {
    fn drop(&mut self) {
        self.service.subscriber_left(&self.session);
    }
}

async fn send(frames: &mpsc::Sender<ServerFrame>, sub: u64, event: DesignEvent) -> Result<(), ()> {
    frames
        .send(ServerFrame::Design { sub, event })
        .await
        .map_err(|_| ())
}

/// The session as it stands, then any dialog still waiting for an answer:
/// an operator who left while one was open would see a frozen page.
async fn send_state(
    ctx: &DaemonContext,
    sub: &DesignSubscription,
    rt: &super::browser::Runtime,
    frames: &mpsc::Sender<ServerFrame>,
) -> bool {
    let event = match ctx.design.session_event(ctx, &sub.session).await {
        Ok(event) => event,
        Err(error) => {
            tracing::warn!(%error, session = %sub.session, "cannot resume design stream");
            DesignEvent::Ended {
                reason: "The preview state could not be read. Reopen the item.".into(),
            }
        }
    };
    let ended = matches!(event, DesignEvent::Ended { .. });
    if send(frames, sub.id, event).await.is_err() || ended {
        return false;
    }
    let Some(dialog) = rt.browser().and_then(|browser| browser.open_dialog()) else {
        return true;
    };
    send(frames, sub.id, super::browser::dialog_event(dialog))
        .await
        .is_ok()
}

async fn run(
    ctx: &Arc<DaemonContext>,
    sub: &DesignSubscription,
    rt: Arc<super::browser::Runtime>,
    mut events: tokio::sync::broadcast::Receiver<DesignEvent>,
    frames: &mpsc::Sender<ServerFrame>,
    ack_rx: &mut watch::Receiver<u64>,
) {
    let mut live = rt.frames.subscribe();
    // Subscribing marks the current frame seen; a late joiner still gets it.
    live.mark_changed();
    if !send_state(ctx, sub, &rt, frames).await {
        return;
    }
    let mut window = FrameWindow {
        seen: 0,
        sent: 0,
        last_sent: Instant::now() - FRAME_GAP,
    };
    loop {
        tokio::select! {
            changed = live.changed() => {
                if changed.is_err() {
                    return;
                }
                if window.send_latest(sub.id, &rt, &mut live, ack_rx, frames).await.is_err() {
                    return;
                }
            }
            event = events.recv() => match event {
                Ok(event) => {
                    let ended = matches!(event, DesignEvent::Ended { .. });
                    if send(frames, sub.id, event).await.is_err() || ended {
                        return;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                    if !send_state(ctx, sub, &rt, frames).await { return; }
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
            },
            _ = frames.closed() => return,
        }
    }
}

struct FrameWindow {
    seen: u64,
    sent: u64,
    last_sent: Instant,
}

impl FrameWindow {
    async fn send_latest(
        &mut self,
        sub: u64,
        rt: &super::browser::Runtime,
        live: &mut watch::Receiver<Option<Arc<archon_design_browser::Frame>>>,
        ack: &mut watch::Receiver<u64>,
        frames: &mpsc::Sender<ServerFrame>,
    ) -> Result<(), ()> {
        tokio::time::sleep_until((self.last_sent + FRAME_GAP).into()).await;
        // A slow subscriber resumes at the newest frame after ACK_WAIT.
        if self.sent > 0
            && let Ok(Err(_closed)) =
                tokio::time::timeout(ACK_WAIT, ack.wait_for(|seq| *seq >= self.sent)).await
        {
            return Err(());
        }
        let Some(frame) = live.borrow_and_update().clone() else {
            return Ok(());
        };
        if frame.seq <= self.seen {
            return Ok(());
        }
        self.seen = frame.seq;
        let event = DesignEvent::Frame {
            seq: frame.seq,
            jpeg_b64: base64::engine::general_purpose::STANDARD.encode(&frame.jpeg),
            w: frame.width,
            h: frame.height,
            at: archon_core::now(),
        };
        let outgoing = ServerFrame::Design { sub, event };
        let size = serde_json::to_vec(&outgoing)
            .map_err(|error| tracing::error!(%error, "serialize design frame"))?
            .len();
        if size > MAX_SERIALIZED_FRAME_BYTES {
            rt.st().counters.frames_dropped += 1;
            return Ok(());
        }
        frames.send(outgoing).await.map_err(|_| ())?;
        self.sent = frame.seq;
        self.last_sent = Instant::now();
        record_sent(rt);
        Ok(())
    }
}

fn record_sent(rt: &super::browser::Runtime) {
    let mut state = rt.st();
    state.counters.frames_sent += 1;
    if let Some(at) = state.pending_input.take() {
        let samples = &mut state.counters.latencies_ms;
        if samples.len() >= 4096 {
            samples.drain(..2048);
        }
        samples.push(at.elapsed().as_millis() as u64);
    }
}
