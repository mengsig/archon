//! CDP over a pipe: NUL-delimited JSON, request ids, event fan-out.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio::task::JoinHandle;

use crate::BrowserError;

/// Largest frame accepted from the browser.
pub(crate) const MAX_FRAME: usize = 64 * 1024 * 1024;
/// Method name of the synthetic event sent once the pipe ends.
pub(crate) const CLOSED_EVENT: &str = "Archon.closed";

type Reply = Result<Value, BrowserError>;
type Pending = Arc<Mutex<Option<HashMap<u64, oneshot::Sender<Reply>>>>>;

struct PendingCall<'a> {
    cdp: &'a Cdp,
    id: u64,
}

impl Drop for PendingCall<'_> {
    fn drop(&mut self) {
        self.cdp.forget(self.id);
    }
}

/// A CDP event.
#[derive(Debug, Clone)]
pub(crate) struct CdpEvent {
    pub method: String,
    pub params: Value,
    pub session_id: Option<String>,
}

#[derive(Debug)]
pub(crate) struct Cdp {
    tx: mpsc::Sender<Vec<u8>>,
    pending: Pending,
    next_id: AtomicU64,
    events: broadcast::Sender<CdpEvent>,
    tasks: Vec<JoinHandle<()>>,
}

fn fail_all(pending: &Pending, make: impl Fn() -> BrowserError) {
    let taken = pending.lock().unwrap().take();
    for (_, tx) in taken.into_iter().flatten() {
        let _ = tx.send(Err(make()));
    }
}

impl Cdp {
    pub(crate) fn new<R, W>(reader: R, writer: W, max_frame: usize) -> Cdp
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let pending: Pending = Arc::new(Mutex::new(Some(HashMap::new())));
        let (events, _) = broadcast::channel(64);
        let (tx, mut rx) = mpsc::channel::<Vec<u8>>(64);

        let wtask = tokio::spawn(async move {
            let mut w = writer;
            while let Some(mut msg) = rx.recv().await {
                msg.push(0);
                if w.write_all(&msg).await.is_err() || w.flush().await.is_err() {
                    break;
                }
            }
        });

        let p = pending.clone();
        let ev = events.clone();
        let rtask = tokio::spawn(async move {
            let mut r = reader;
            let mut buf: Vec<u8> = Vec::new();
            let mut chunk = vec![0u8; 64 * 1024];
            let mut fatal: Option<String> = None;
            'outer: loop {
                let n = match r.read(&mut chunk).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => n,
                };
                buf.extend_from_slice(&chunk[..n]);
                let mut start = 0;
                while let Some(off) = buf[start..].iter().position(|b| *b == 0) {
                    let frame = &buf[start..start + off];
                    start += off + 1;
                    if frame.len() > max_frame {
                        fatal = Some(format!("CDP frame larger than {max_frame} bytes refused"));
                        break 'outer;
                    }
                    if frame.is_empty() {
                        continue;
                    }
                    handle_frame(frame, &p, &ev);
                }
                buf.drain(..start);
                if buf.len() > max_frame {
                    fatal = Some(format!("CDP frame larger than {max_frame} bytes refused"));
                    break;
                }
            }
            match fatal {
                Some(m) => fail_all(&p, || BrowserError::Protocol(m.clone())),
                None => fail_all(&p, || BrowserError::Closed),
            }
            let _ = ev.send(CdpEvent {
                method: CLOSED_EVENT.into(),
                params: Value::Null,
                session_id: None,
            });
        });

        Cdp {
            tx,
            pending,
            next_id: AtomicU64::new(1),
            events,
            tasks: vec![wtask, rtask],
        }
    }

    pub(crate) fn subscribe(&self) -> broadcast::Receiver<CdpEvent> {
        self.events.subscribe()
    }

    pub(crate) async fn call(
        &self,
        session: Option<&str>,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value, BrowserError> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (otx, orx) = oneshot::channel();
        match self.pending.lock().unwrap().as_mut() {
            Some(m) => {
                if m.len() >= 256 {
                    return Err(BrowserError::Protocol("too many pending CDP calls".into()));
                }
                m.insert(id, otx);
            }
            None => return Err(BrowserError::Closed),
        }
        let _pending_call = PendingCall { cdp: self, id };
        let mut msg = json!({"id": id, "method": method, "params": params});
        if let Some(s) = session {
            msg["sessionId"] = Value::String(s.into());
        }
        let bytes = serde_json::to_vec(&msg).map_err(|e| BrowserError::Protocol(e.to_string()))?;
        let call = async {
            self.tx
                .send(bytes)
                .await
                .map_err(|_| BrowserError::Closed)?;
            orx.await.map_err(|_| BrowserError::Closed)?
        };
        tokio::time::timeout(timeout, call)
            .await
            .map_err(|_| BrowserError::Timeout(method.into()))?
    }

    fn forget(&self, id: u64) {
        if let Some(m) = self.pending.lock().unwrap().as_mut() {
            m.remove(&id);
        }
    }

    pub(crate) fn abort(&self) {
        for t in &self.tasks {
            t.abort();
        }
        fail_all(&self.pending, || BrowserError::Closed);
    }
}

impl Drop for Cdp {
    fn drop(&mut self) {
        self.abort();
    }
}

fn handle_frame(frame: &[u8], pending: &Pending, events: &broadcast::Sender<CdpEvent>) {
    let Ok(v) = serde_json::from_slice::<Value>(frame) else {
        return;
    };
    if let Some(id) = v.get("id").and_then(Value::as_u64) {
        let tx = pending.lock().unwrap().as_mut().and_then(|m| m.remove(&id));
        if let Some(tx) = tx {
            let r = match v.get("error") {
                Some(e) => Err(BrowserError::Protocol(
                    e.get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("CDP error")
                        .to_string(),
                )),
                None => Ok(v.get("result").cloned().unwrap_or(Value::Null)),
            };
            let _ = tx.send(r);
        }
    } else if let Some(m) = v.get("method").and_then(Value::as_str) {
        let _ = events.send(CdpEvent {
            method: m.to_string(),
            params: v.get("params").cloned().unwrap_or(Value::Null),
            session_id: v.get("sessionId").and_then(Value::as_str).map(String::from),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{DuplexStream, duplex};

    const T: Duration = Duration::from_secs(5);

    /// Our side's reader/writer plus the fake peer's halves.
    fn pair() -> (Cdp, DuplexStream, DuplexStream) {
        let (a_w, peer_r) = duplex(1 << 20);
        let (peer_w, a_r) = duplex(1 << 20);
        (Cdp::new(a_r, a_w, 1024), peer_r, peer_w)
    }

    async fn read_msg(r: &mut DuplexStream) -> Value {
        let mut b = Vec::new();
        loop {
            let c = r.read_u8().await.unwrap();
            if c == 0 {
                return serde_json::from_slice(&b).unwrap();
            }
            b.push(c);
        }
    }

    #[tokio::test]
    async fn cancelled_call_releases_pending_entry() {
        let (cdp, mut peer, _writer) = pair();
        let cdp = Arc::new(cdp);
        let caller = cdp.clone();
        let task =
            tokio::spawn(async move { caller.call(None, "Page.enable", json!({}), T).await });
        let message = read_msg(&mut peer).await;
        assert!(message["id"].as_u64().is_some());
        assert_eq!(cdp.pending.lock().unwrap().as_ref().unwrap().len(), 1);
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert!(cdp.pending.lock().unwrap().as_ref().unwrap().is_empty());
    }

    #[tokio::test]
    async fn dropping_cdp_closes_its_pipe_tasks() {
        let (cdp, mut peer, _writer) = pair();
        drop(cdp);
        let mut byte = [0];
        let n = tokio::time::timeout(T, peer.read(&mut byte))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(n, 0);
    }

    // T-U1
    #[tokio::test]
    async fn ids_replies_split_reads_and_events() {
        let (cdp, mut peer_r, mut peer_w) = pair();
        let mut ev = cdp.subscribe();
        let cdp = Arc::new(cdp);
        let c1 = cdp.clone();
        let a = tokio::spawn(async move { c1.call(None, "A.one", json!({"x": 1}), T).await });
        let c2 = cdp.clone();
        let b = tokio::spawn(async move { c2.call(Some("S1"), "B.two", json!({}), T).await });
        let m1 = read_msg(&mut peer_r).await;
        let m2 = read_msg(&mut peer_r).await;
        let (m_a, m_b) = if m1["method"] == "A.one" {
            (m1, m2)
        } else {
            (m2, m1)
        };
        assert_eq!(m_a["params"]["x"], 1);
        assert_eq!(m_b["sessionId"], "S1");
        assert_ne!(m_a["id"], m_b["id"]);
        // Reply out of order, an event in between, split across writes.
        let mut out = Vec::new();
        out.extend(
            serde_json::to_vec(
                &json!({"method":"Page.loadEventFired","params":{"t":1},"sessionId":"S1"}),
            )
            .unwrap(),
        );
        out.push(0);
        out.extend(serde_json::to_vec(&json!({"id": m_b["id"], "result": {"ok": "b"}})).unwrap());
        out.push(0);
        out.extend(
            serde_json::to_vec(&json!({"id": m_a["id"], "error": {"code": -1, "message": "boom"}}))
                .unwrap(),
        );
        out.push(0);
        for part in out.chunks(7) {
            peer_w.write_all(part).await.unwrap();
            peer_w.flush().await.unwrap();
            tokio::task::yield_now().await;
        }
        assert_eq!(b.await.unwrap().unwrap()["ok"], "b");
        assert!(matches!(a.await.unwrap(), Err(BrowserError::Protocol(m)) if m == "boom"));
        let e = ev.recv().await.unwrap();
        assert_eq!(
            (e.method.as_str(), e.session_id.as_deref()),
            ("Page.loadEventFired", Some("S1"))
        );
        // Fan-out: a second subscriber sees later events.
        let mut ev2 = cdp.subscribe();
        peer_w
            .write_all(b"{\"method\":\"X.y\",\"params\":{}}\0")
            .await
            .unwrap();
        assert_eq!(ev.recv().await.unwrap().method, "X.y");
        assert_eq!(ev2.recv().await.unwrap().method, "X.y");
    }

    #[tokio::test]
    async fn oversize_frame_refused() {
        let (cdp, _peer_r, mut peer_w) = pair();
        let cdp = Arc::new(cdp);
        let c = cdp.clone();
        let call = tokio::spawn(async move { c.call(None, "A.b", json!({}), T).await });
        tokio::time::sleep(Duration::from_millis(50)).await;
        peer_w.write_all(&vec![b'a'; 4096]).await.unwrap();
        assert!(
            matches!(call.await.unwrap(), Err(BrowserError::Protocol(m)) if m.contains("refused"))
        );
        assert!(matches!(
            cdp.call(None, "A.c", json!({}), T).await,
            Err(BrowserError::Closed)
        ));
    }

    #[tokio::test]
    async fn exit_mid_request_fails_pending() {
        let (cdp, mut peer_r, peer_w) = pair();
        let mut ev = cdp.subscribe();
        let cdp = Arc::new(cdp);
        let c = cdp.clone();
        let call = tokio::spawn(async move { c.call(None, "A.b", json!({}), T).await });
        let _ = read_msg(&mut peer_r).await;
        drop(peer_w);
        assert!(matches!(call.await.unwrap(), Err(BrowserError::Closed)));
        assert_eq!(ev.recv().await.unwrap().method, CLOSED_EVENT);
    }

    #[tokio::test]
    async fn call_times_out() {
        let (cdp, _peer_r, _peer_w) = pair();
        let r = cdp
            .call(None, "A.b", json!({}), Duration::from_millis(50))
            .await;
        assert!(matches!(r, Err(BrowserError::Timeout(_))));
    }
}
