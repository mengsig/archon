//! T-I5: a real Chromium. Skipped unless ARCHON_DESIGN_REAL_BROWSER=1 and a browser is found.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use bytes::Bytes;
use http_body_util::Full;
use hyper::{Request, Response, body::Incoming};
use hyper_util::rt::TokioIo;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use archon_design_browser::{
    Browser, BrowserError, BrowserEvent, Input, LaunchSpec, MouseAction, MouseButton,
    UpstreamGuard, Viewport, find_browser,
};

#[derive(Debug)]
struct TestUpstream {
    accepts: AtomicU32,
    allow: bool,
}

impl TestUpstream {
    fn allowing() -> Arc<Self> {
        Arc::new(Self {
            accepts: AtomicU32::new(0),
            allow: true,
        })
    }
}

#[async_trait::async_trait]
impl UpstreamGuard for TestUpstream {
    async fn verify_peer(&self, peer: &TcpStream) -> Result<(), BrowserError> {
        assert!(peer.peer_addr().unwrap().ip().is_loopback());
        self.accepts.fetch_add(1, Ordering::Relaxed);
        if self.allow {
            Ok(())
        } else {
            Err(BrowserError::Launch(
                "fixture denied the preview socket".into(),
            ))
        }
    }
}

type TestResponse = Response<Full<Bytes>>;

fn page_body(html: impl Into<Bytes>) -> TestResponse {
    Response::builder()
        .header("content-type", "text/html")
        .body(Full::new(html.into()))
        .unwrap()
}

async fn serve(handler: impl Fn(Request<Incoming>) -> TestResponse + Send + Sync + 'static) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let handler = Arc::new(handler);
    tokio::spawn(async move {
        while let Ok((socket, _)) = listener.accept().await {
            let handler = handler.clone();
            tokio::spawn(async move {
                let service = hyper::service::service_fn(move |request| {
                    std::future::ready(Ok::<_, std::convert::Infallible>(handler(request)))
                });
                if let Err(error) = hyper::server::conn::http1::Builder::new()
                    .serve_connection(TokioIo::new(socket), service)
                    .with_upgrades()
                    .await
                {
                    assert!(error.is_incomplete_message(), "{error}");
                }
            });
        }
    });
    port
}

fn websocket(mut request: Request<Incoming>) -> TestResponse {
    let key = request.headers()["sec-websocket-key"].as_bytes();
    let accept = tokio_tungstenite::tungstenite::handshake::derive_accept_key(key);
    let upgrade = hyper::upgrade::on(&mut request);
    tokio::spawn(async move {
        let mut socket = TokioIo::new(upgrade.await.unwrap());
        socket.write_all(&[0x81, 2, b'o', b'k']).await.unwrap();
        tokio::time::sleep(Duration::from_secs(1)).await;
    });
    Response::builder()
        .status(101)
        .header("upgrade", "websocket")
        .header("connection", "upgrade")
        .header("sec-websocket-accept", accept)
        .body(Full::default())
        .unwrap()
}

fn browser_options() -> Option<(std::path::PathBuf, String)> {
    if std::env::var("ARCHON_DESIGN_REAL_BROWSER").as_deref() != Ok("1") {
        eprintln!("skipped: ARCHON_DESIGN_REAL_BROWSER=1 not set");
        return None;
    }
    let path = std::env::var("PATH").unwrap_or_default();
    let home = std::path::PathBuf::from(std::env::var("HOME").unwrap_or_default());
    let Ok(binary) = find_browser(None, &path, &home) else {
        eprintln!("skipped: no browser found");
        return None;
    };
    Some((binary, path))
}

async fn launch(spec: LaunchSpec) -> Option<Browser> {
    match Browser::launch(spec).await {
        Ok(browser) => Some(browser),
        Err(BrowserError::SandboxUnavailable { hint }) => {
            eprintln!("skipped: sandbox unavailable: {hint}");
            None
        }
        Err(error) => panic!("launch: {error}"),
    }
}

async fn click_other_preview(browser: &Browser) {
    let (x, y) = browser
        .center_of("#other")
        .await
        .unwrap()
        .expect("preview link");
    assert!(x.is_finite() && y.is_finite());
    let mut events = browser.events();
    browser
        .input(
            &[MouseAction::Down, MouseAction::Up].map(|action| Input::Mouse {
                action,
                x,
                y,
                button: MouseButton::Left,
                click_count: 1,
                modifiers: 0,
            }),
        )
        .await
        .unwrap();
    let blocked = tokio::time::timeout(Duration::from_secs(5), async {
        while let Ok(event) = events.recv().await {
            if matches!(event, BrowserEvent::BlockedHost { host } if host == "localhost") {
                return true;
            }
        }
        false
    })
    .await
    .unwrap();
    assert!(blocked);
}

#[tokio::test]
async fn real_browser_is_confined() {
    let Some((binary, path)) = browser_options() else {
        return;
    };
    let root = tempfile::tempdir().unwrap();
    let profile = root.path().join("ds_test");
    let hits = Arc::new(AtomicU32::new(0));
    let other = serve({
        let hits = hits.clone();
        move |_| {
            hits.fetch_add(1, Ordering::Relaxed);
            page_body("other origin")
        }
    })
    .await;
    let page = serve(move |request| match request.uri().path() {
        "/socket" => websocket(request),
        "/local-redirect" => Response::builder()
            .status(302)
            .header("location", "/x?y=1")
            .body(Full::default())
            .unwrap(),
        "/off-origin-redirect" => Response::builder()
            .status(302)
            .header("location", format!("http://127.0.0.1:{other}/"))
            .body(Full::default())
            .unwrap(),
        _ => page_body(format!(
            "<html><body><button data-testid=save>Save changes</button>\
             <a id=other href='http://localhost:{other}/'>Other preview</a>\
             <script>\
             fetch('http://127.0.0.1:{other}/', {{mode:'no-cors'}}).catch(() => {{}});\
             const socket = new WebSocket('ws://' + location.host + '/socket');\
             socket.onmessage = (m) => document.querySelector('button').textContent = m.data;\
             </script></body></html>"
        )),
    })
    .await;
    let guard = TestUpstream::allowing();
    let spec = LaunchSpec {
        binary,
        profile_dir: profile.clone(),
        sandbox: std::env::var("ARCHON_DESIGN_SANDBOX_OFF").as_deref() != Ok("1"),
        preview_url: format!("http://localhost:{page}/"),
        upstream_guard: guard.clone(),
        viewport: Viewport {
            width: 800,
            height: 600,
            touch: false,
            dpr: 1.0,
        },
        path_env: path,
        lang: "C.UTF-8".into(),
        tz: "UTC".into(),
    };
    let Some(b) = launch(spec).await else { return };

    let pid = b.pid().unwrap();
    let environ = std::fs::read(format!("/proc/{pid}/environ")).unwrap_or_default();
    let environ = String::from_utf8_lossy(&environ);
    assert!(
        !environ.contains("ARCHON_") && !environ.contains("AETHER_"),
        "{environ}"
    );

    let mut ev = b.events();
    b.navigate(&format!("http://localhost:{page}/local-redirect"))
        .await
        .unwrap();
    assert!(
        b.wait_network_idle(Duration::from_millis(500), Duration::from_secs(20))
            .await
            .unwrap()
    );
    assert_eq!(b.current_path().await.unwrap(), "/x");
    assert!(b.probe(&format!("http://localhost:{page}/x")).await);

    let websocket_text = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let resolved = b.resolve(20.0, 20.0, 0).await.unwrap();
            if resolved.role_name.as_deref() == Some("button \"ok\"") {
                break resolved;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("same-origin websocket did not deliver its message");
    assert_eq!(
        websocket_text.selector.as_deref(),
        Some("[data-testid=\"save\"]")
    );
    assert_eq!(websocket_text.path, "/x");

    let pic = b.capture(None, 1.0).await.unwrap();
    assert!(!pic.bytes.is_empty() && pic.bytes.len() <= 560 * 1024);
    assert!(guard.accepts.load(Ordering::Relaxed) >= 4);

    for disallowed in [
        format!("http://127.0.0.1:{page}/"),
        format!("http://127.0.0.1:{other}/"),
        "about:blank".into(),
    ] {
        assert!(matches!(
            b.navigate(&disallowed).await,
            Err(BrowserError::Navigation { .. })
        ));
    }
    click_other_preview(&b).await;
    let off_origin_redirect = format!("http://localhost:{page}/off-origin-redirect");
    assert!(b.navigate(&off_origin_redirect).await.is_err());
    assert_eq!(hits.load(Ordering::Relaxed), 0);
    assert!(
        std::iter::from_fn(|| ev.try_recv().ok()).any(|event| matches!(
            event, BrowserEvent::BlockedHost { host } if host == "127.0.0.1"
        ))
    );

    b.close().await.unwrap();
    assert!(!profile.exists(), "profile still there");
}

#[tokio::test]
async fn rejected_preview_socket_receives_no_bytes() {
    let Some((binary, path)) = browser_options() else {
        return;
    };
    let fixture = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/", fixture.local_addr().unwrap());
    let root = tempfile::tempdir().unwrap();
    let guard = Arc::new(TestUpstream {
        accepts: AtomicU32::new(0),
        allow: false,
    });
    let spec = LaunchSpec {
        binary,
        profile_dir: root.path().join("guard"),
        sandbox: std::env::var("ARCHON_DESIGN_SANDBOX_OFF").as_deref() != Ok("1"),
        preview_url: url.clone(),
        upstream_guard: guard.clone(),
        viewport: Viewport {
            width: 800,
            height: 600,
            touch: false,
            dpr: 1.0,
        },
        path_env: path,
        lang: "C.UTF-8".into(),
        tz: "UTC".into(),
    };
    let Some(browser) = launch(spec).await else {
        return;
    };
    assert!(!browser.probe(&url).await);
    assert!(browser.navigate(&url).await.is_err());
    let checks = guard.accepts.load(Ordering::Relaxed);
    assert!(
        checks >= 2,
        "expected both the health check and Chromium to verify the peer"
    );
    for _ in 0..checks {
        tokio::time::timeout(Duration::from_secs(2), async {
            let (mut socket, _) = fixture.accept().await.unwrap();
            assert_eq!(socket.read(&mut [0]).await.unwrap(), 0);
        })
        .await
        .unwrap();
    }
    browser.close().await.unwrap();
}

/// LOW-crop: a selector shared by a nav's links (`nav > a`) resolved to the
/// first link, so the final picture of "New task" showed "Tasks"; and a crop
/// with its margin reached past the page's edge.
#[tokio::test]
async fn a_shared_selector_finds_the_recorded_element_and_crops_stay_on_the_page() {
    let Some((binary, path)) = browser_options() else {
        return;
    };
    let root = tempfile::tempdir().unwrap();
    let phone = Viewport {
        width: 390,
        height: 844,
        touch: true,
        dpr: 1.0,
    };
    let page = serve(|request| {
        if request.uri().path() == "/moved" {
            page_body("<html><head><meta name=viewport content=\"width=device-width\"></head>\
         <body style=\"margin:0\"><nav style=\"position:relative;height:60px\">\
         <a href=\"/\" style=\"position:absolute;left:10px;top:20px;width:50px;height:20px\">Home</a>\
         <a href=\"/tasks\" style=\"position:absolute;left:100px;top:20px;width:50px;height:20px\">Tasks</a>\
         <a href=\"/new\" style=\"position:absolute;left:200px;top:20px;width:80px;height:20px\">New task</a>\
         </nav></body></html>")
        } else {
            page_body("<html><head><meta name=viewport content=\"width=device-width\"></head>\
         <body style=\"margin:0\"><nav style=\"position:relative;height:60px\">\
         <a href=\"/tasks\" style=\"position:absolute;left:200px;top:20px;width:50px;height:20px\">Tasks</a>\
         <a href=\"/new\" style=\"position:absolute;left:300px;top:20px;width:80px;height:20px\">New task</a>\
         </nav></body></html>")
        }
    }).await;
    let spec = LaunchSpec {
        binary,
        profile_dir: root.path().join("ds_zoom"),
        sandbox: std::env::var("ARCHON_DESIGN_SANDBOX_OFF").as_deref() != Ok("1"),
        preview_url: format!("http://127.0.0.1:{page}/"),
        upstream_guard: TestUpstream::allowing(),
        viewport: phone,
        path_env: path,
        lang: "C.UTF-8".into(),
        tz: "UTC".into(),
    };
    let Some(b) = launch(spec).await else { return };

    b.set_viewport(phone).await.unwrap();
    b.navigate(&format!("http://127.0.0.1:{page}/"))
        .await
        .unwrap();
    let _ = b
        .wait_network_idle(Duration::from_millis(300), Duration::from_secs(10))
        .await;
    let recorded = archon_design_browser::ClipRect {
        x: 300.0,
        y: 20.0,
        w: 80.0,
        h: 20.0,
    };
    let rect = b
        .resolve_selector("nav > a", Some(recorded))
        .await
        .unwrap()
        .expect("the link is found");
    let near = |a: f64, b: f64| (a - b).abs() < 1.0;
    assert!(near(rect.x, 300.0) && near(rect.w, 80.0), "{rect:?}");
    let first = b.resolve_selector("nav > a", None).await.unwrap().unwrap();
    assert!(
        near(first.x, 200.0),
        "without a record, the first: {first:?}"
    );
    // A 24 px margin around the link reaches past the 390 px page: cut there.
    let grown = archon_design_browser::ClipRect {
        x: rect.x - 24.0,
        y: rect.y - 20.0,
        w: rect.w + 48.0,
        h: rect.h + 40.0,
    };
    let pic = b.capture(Some(grown), 2.0).await.unwrap();
    assert_eq!(
        pic.width,
        ((390.0 - grown.x) * 2.0) as u32,
        "cut at the page's edge"
    );
    // A pick names the link by its address, not its place among siblings.
    let picked = b.resolve(340.0, 30.0, 0).await.unwrap();
    let selector = picked.selector.expect("a selector");
    assert!(selector.ends_with("a[href=\"/new\"]"), "{selector}");
    // So a link the design adds before it leaves the note on its element.
    b.navigate(&format!("http://127.0.0.1:{page}/moved"))
        .await
        .unwrap();
    let _ = b
        .wait_network_idle(Duration::from_millis(300), Duration::from_secs(10))
        .await;
    let after = b
        .resolve_selector(&selector, Some(recorded))
        .await
        .unwrap()
        .expect("still found");
    assert!(near(after.x, 200.0) && near(after.w, 80.0), "{after:?}");
    b.close().await.unwrap();
}

/// A whole-page final picture at 2x came out as the 1x page tiled 2x2 when the
/// stream's polling shot (scale 1) was in flight with it. With the live stream
/// and a scale-1 loop running, every whole-page 2x shot must be one copy of the
/// page: the same bytes as the element-crop path (which was always right) takes
/// of the same, static page.
#[tokio::test]
async fn whole_page_pictures_at_2x_are_not_tiled_while_the_stream_runs() {
    let Some((binary, path)) = browser_options() else {
        return;
    };
    let root = tempfile::tempdir().unwrap();
    let desktop = Viewport {
        width: 1440,
        height: 900,
        touch: false,
        dpr: 1.0,
    };
    // Four quadrants of four colours: a tiled picture repeats them.
    let quad = |l: u32, t: u32, c: &str| {
        format!(
            "<div style=\"position:absolute;left:{l}px;top:{t}px;width:720px;height:450px;background:{c}\"></div>"
        )
    };
    let body = format!(
        "<html><body style=\"margin:0\">{}{}{}{}</body></html>",
        quad(0, 0, "#f00"),
        quad(720, 0, "#0f0"),
        quad(0, 450, "#00f"),
        quad(720, 450, "#ff0"),
    );
    let page = serve(move |_| page_body(body.clone())).await;
    let spec = LaunchSpec {
        binary,
        profile_dir: root.path().join("ds_tile"),
        sandbox: std::env::var("ARCHON_DESIGN_SANDBOX_OFF").as_deref() != Ok("1"),
        preview_url: format!("http://127.0.0.1:{page}/"),
        upstream_guard: TestUpstream::allowing(),
        viewport: desktop,
        path_env: path,
        lang: "C.UTF-8".into(),
        tz: "UTC".into(),
    };
    let Some(b) = launch(spec).await else { return };
    let b = Arc::new(b);
    b.set_viewport(desktop).await.unwrap();
    b.navigate(&format!("http://127.0.0.1:{page}/"))
        .await
        .unwrap();
    let _ = b
        .wait_network_idle(Duration::from_millis(300), Duration::from_secs(10))
        .await;
    let whole = archon_design_browser::ClipRect {
        x: 0.0,
        y: 0.0,
        w: 1440.0,
        h: 900.0,
    };
    let reference = b.capture(Some(whole), 2.0).await.unwrap();
    assert_eq!((reference.width, reference.height), (2880, 1800));
    let _relay = b.start_screencast().await.unwrap();
    let poller = {
        let b = b.clone();
        tokio::spawn(async move {
            loop {
                let _ = b.capture(None, 1.0).await;
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
    };
    for i in 0..20 {
        if i % 5 == 0 {
            b.set_viewport(desktop).await.unwrap();
        }
        let pic = b.capture(None, 2.0).await.unwrap();
        assert_eq!((pic.width, pic.height), (2880, 1800), "shot {i}");
        assert!(
            pic.bytes == reference.bytes,
            "shot {i} differs from one copy of the page (tiled?)"
        );
    }
    poller.abort();
    let _ = poller.await;
    drop(_relay);
    let Ok(b) = std::sync::Arc::try_unwrap(b) else {
        panic!("the poller still holds the browser");
    };
    b.close().await.unwrap();
}
