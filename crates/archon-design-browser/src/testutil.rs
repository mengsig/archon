//! A scripted fake CDP peer for tests.

use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt, duplex};

use crate::Browser;

/// What a peer says to a method its test did not script: an unscrolled page, or `{}`.
fn unscripted(method: &str) -> Value {
    match method {
        "Page.getLayoutMetrics" => {
            json!({"cssVisualViewport": {"pageX": 0.0, "pageY": 0.0, "scale": 1.0}})
        }
        _ => json!({}),
    }
}

/// Methods seen by the peer.
pub(crate) type Log = Arc<Mutex<Vec<(String, Value)>>>;

/// Connect a browser to a fake peer. `answer(method, params)` gives the `result` (None → `{}`).
pub(crate) async fn fake_browser(
    answer: impl Fn(&str, &Value) -> Option<Value> + Send + 'static,
) -> (Browser, Log) {
    fake_browser_slow_shots(answer, None).await
}

/// Like [`fake_browser`], but a `Page.captureScreenshot` is answered only after
/// `shot_delay`, off the read loop, so overlapping screenshots can be seen:
/// the log gets a `"<answered>"` entry when each one is answered.
pub(crate) async fn fake_browser_slow_shots(
    answer: impl Fn(&str, &Value) -> Option<Value> + Send + 'static,
    shot_delay: Option<std::time::Duration>,
) -> (Browser, Log) {
    fake_browser_with(answer, shot_delay, None).await
}

/// Like [`fake_browser`], but a `Page.reload` or `Page.navigate` is followed by
/// the page's load event only after `load_after`.
pub(crate) async fn fake_browser_loading(
    answer: impl Fn(&str, &Value) -> Option<Value> + Send + 'static,
    load_after: std::time::Duration,
) -> (Browser, Log) {
    fake_browser_with(answer, None, Some(load_after)).await
}

async fn fake_browser_with(
    answer: impl Fn(&str, &Value) -> Option<Value> + Send + 'static,
    shot_delay: Option<std::time::Duration>,
    load_after: Option<std::time::Duration>,
) -> (Browser, Log) {
    let (a_w, mut peer_r) = duplex(1 << 22);
    let (peer_w, a_r) = duplex(1 << 22);
    let peer_w = Arc::new(tokio::sync::Mutex::new(peer_w));
    let log: Log = Arc::default();
    let l2 = log.clone();
    tokio::spawn(async move {
        let mut buf = Vec::new();
        let mut chunk = vec![0u8; 65536];
        while let Ok(n) = peer_r.read(&mut chunk).await {
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&chunk[..n]);
            while let Some(i) = buf.iter().position(|b| *b == 0) {
                let msg: Value = serde_json::from_slice(&buf[..i]).unwrap();
                buf.drain(..=i);
                let method = msg["method"].as_str().unwrap().to_string();
                l2.lock()
                    .unwrap()
                    .push((method.clone(), msg["params"].clone()));
                let result = match method.as_str() {
                    "Target.attachToTarget" => json!({"sessionId": "S1"}),
                    _ => answer(&method, &msg["params"]).unwrap_or_else(|| unscripted(&method)),
                };
                let mut out =
                    serde_json::to_vec(&json!({"id": msg["id"], "result": result})).unwrap();
                out.push(0);
                if method == "Target.setDiscoverTargets" {
                    let mut ev = serde_json::to_vec(&json!({"method": "Target.targetCreated", "params": {"targetInfo": {"targetId": "T1", "type": "page", "url": "about:blank"}}})).unwrap();
                    ev.push(0);
                    out.extend(ev);
                }
                if let (Some(d), "Page.reload" | "Page.navigate") = (load_after, method.as_str()) {
                    let w = peer_w.clone();
                    tokio::spawn(async move {
                        tokio::time::sleep(d).await;
                        let mut ev = serde_json::to_vec(&json!({"method": "Page.loadEventFired", "sessionId": "S1", "params": {}})).unwrap();
                        ev.push(0);
                        let _ = w.lock().await.write_all(&ev).await;
                    });
                }
                if let (Some(d), "Page.captureScreenshot") = (shot_delay, method.as_str()) {
                    let (w, l) = (peer_w.clone(), l2.clone());
                    tokio::spawn(async move {
                        tokio::time::sleep(d).await;
                        l.lock().unwrap().push(("<answered>".into(), Value::Null));
                        let _ = w.lock().await.write_all(&out).await;
                    });
                } else if peer_w.lock().await.write_all(&out).await.is_err() {
                    return;
                }
            }
        }
    });
    let b = Browser::connect(a_r, a_w).await.expect("connect");
    (b, log)
}

#[derive(Debug)]
pub(crate) struct AcceptUpstream;

#[async_trait::async_trait]
impl crate::UpstreamGuard for AcceptUpstream {
    async fn verify_peer(&self, _: &tokio::net::TcpStream) -> Result<(), crate::BrowserError> {
        Ok(())
    }
}
