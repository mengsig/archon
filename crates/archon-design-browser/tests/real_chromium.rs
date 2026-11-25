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

/// S4 review: a page DPR of 2 with clip scale 1 makes the same 2880x1800
/// pixels as the preview's DPR 1 with clip scale 2, but a resolution query
/// lays it out otherwise. Each picture records which it was.
#[tokio::test]
async fn a_picture_records_the_page_dpr_apart_from_the_screenshot_scale() {
    let Some((binary, path)) = browser_options() else {
        return;
    };
    let root = tempfile::tempdir().unwrap();
    let at = |dpr| Viewport {
        width: 1440,
        height: 900,
        touch: false,
        dpr,
    };
    let body = "<html><head><style>body{margin:0}#card{width:420px;height:200px;background:#0a0}\
        @media (min-resolution: 2dppx){#card{width:850px}}</style></head>\
        <body><div id=\"card\"></div></body></html>";
    let page = serve(move |_| page_body(body)).await;
    let spec = LaunchSpec {
        binary,
        profile_dir: root.path().join("ds_dpr"),
        sandbox: std::env::var("ARCHON_DESIGN_SANDBOX_OFF").as_deref() != Ok("1"),
        preview_url: format!("http://127.0.0.1:{page}/"),
        upstream_guard: TestUpstream::allowing(),
        viewport: at(1.0),
        path_env: path,
        lang: "C.UTF-8".into(),
        tz: "UTC".into(),
    };
    let Some(b) = launch(spec).await else { return };
    let mut shots = Vec::new();
    for (dpr, scale) in [(1.0, 2.0), (2.0, 1.0)] {
        b.set_viewport(at(dpr)).await.unwrap();
        b.navigate(&format!("http://127.0.0.1:{page}/"))
            .await
            .unwrap();
        let _ = b
            .wait_network_idle(Duration::from_millis(300), Duration::from_secs(10))
            .await;
        let card = b.resolve_selector("#card", None).await.unwrap().unwrap();
        shots.push((card.w, b.capture(None, scale).await.unwrap()));
    }
    let [(preview_card, preview), (replayed_card, replayed)] = &shots[..] else {
        unreachable!("two shots were taken");
    };
    assert_eq!((preview.width, preview.height), (2880, 1800));
    assert_eq!((replayed.width, replayed.height), (2880, 1800));
    assert_eq!(
        (*preview_card, *replayed_card),
        (420.0, 850.0),
        "the layouts differ"
    );
    assert_eq!(preview.capture.device_scale_factor, 1.0);
    assert_eq!(preview.capture.screenshot_scale, 2.0);
    assert_eq!(replayed.capture.device_scale_factor, 2.0);
    assert_eq!(replayed.capture.screenshot_scale, 1.0);
    assert_ne!(preview.capture, replayed.capture);
    b.close().await.unwrap();
}

/// S4 review: the preview's phone viewport turns touch on with mobile, so a
/// `pointer: coarse` page lays out its phone card and the capture says mobile.
#[tokio::test]
async fn a_phone_picture_is_taken_with_touch_and_records_it_as_mobile() {
    let Some((binary, path)) = browser_options() else {
        return;
    };
    let root = tempfile::tempdir().unwrap();
    let at = |touch| Viewport {
        width: 390,
        height: 844,
        touch,
        dpr: 1.0,
    };
    let body = "<html><head><style>body{margin:0}#card{width:140px;height:80px;background:#0a0}\
        @media (pointer: coarse){#card{width:340px}}</style></head>\
        <body><div id=\"card\"></div></body></html>";
    let page = serve(move |_| page_body(body)).await;
    let spec = LaunchSpec {
        binary,
        profile_dir: root.path().join("ds_touch"),
        sandbox: std::env::var("ARCHON_DESIGN_SANDBOX_OFF").as_deref() != Ok("1"),
        preview_url: format!("http://127.0.0.1:{page}/"),
        upstream_guard: TestUpstream::allowing(),
        viewport: at(false),
        path_env: path,
        lang: "C.UTF-8".into(),
        tz: "UTC".into(),
    };
    let Some(b) = launch(spec).await else { return };
    let mut shots = Vec::new();
    for touch in [false, true] {
        b.set_viewport(at(touch)).await.unwrap();
        b.navigate(&format!("http://127.0.0.1:{page}/"))
            .await
            .unwrap();
        let _ = b
            .wait_network_idle(Duration::from_millis(300), Duration::from_secs(10))
            .await;
        let card = b.resolve_selector("#card", None).await.unwrap().unwrap();
        shots.push((card.w, b.capture(None, 2.0).await.unwrap()));
    }
    let [(desk_card, desk), (phone_card, phone)] = &shots[..] else {
        unreachable!("two shots were taken");
    };
    assert_eq!(
        (*desk_card, *phone_card),
        (140.0, 340.0),
        "touch changes the layout"
    );
    assert_eq!((desk.width, desk.height), (phone.width, phone.height));
    assert!(!desk.capture.mobile);
    assert!(
        phone.capture.mobile,
        "a touch picture records mobile, which means touch"
    );
    b.close().await.unwrap();
}

#[tokio::test]
async fn js_navigation_to_a_blob_stops_before_disclosure() {
    assert_disclosure_stops("location.assign(fixture())", &["main-frame document"]).await;
}

#[tokio::test]
async fn js_popup_stops_before_disclosure() {
    assert_disclosure_stops("window.open(fixture(), '_blank')", &["new window"]).await;
}

#[tokio::test]
async fn opaque_subframe_stops_before_disclosure() {
    assert_disclosure_stops(
        "const child = document.createElement('iframe');
         child.style = 'position:fixed;inset:0;width:100%;height:100%;border:0';
         child.src = 'data:text/html,' + encodeURIComponent(otherDocument);
         document.body.append(child)",
        &["subframe"],
    )
    .await;
}

#[tokio::test]
async fn same_origin_subframes_allow_disclosure() {
    let Some((binary, path_env)) = browser_options() else {
        return;
    };
    let port = serve(|request| match request.uri().path() {
        "/child" => page_body("<!doctype html><p>HTTP child</p>"),
        _ => page_body(
            "<!doctype html>
             <button id=status style='width:200px;height:60px'>Waiting for children</button>
             <script>
               const loaded = new Set();
               function ready(frame, expected) {
                 if (frame.contentWindow.location.href.endsWith(expected)) {
                   loaded.add(expected);
                 }
                 if (loaded.size === 3) {
                   document.getElementById('status').textContent = 'All children loaded';
                 }
               }
             </script>
             <iframe name=http src='/child' onload=\"ready(this, '/child')\"></iframe>
             <iframe name=blank onload=\"ready(this, 'about:blank')\"></iframe>
             <iframe name=inline srcdoc='<p>Inline child</p>' onload=\"ready(this, 'about:srcdoc')\"></iframe>",
        ),
    })
    .await;
    let Some((_root, browser)) = open_disclosure_fixture(binary, path_env, port).await else {
        return;
    };
    let result = browser.resolve(40.0, 40.0, 0).await.unwrap();
    assert_eq!(
        result.role_name.as_deref(),
        Some("button \"All children loaded\"")
    );
    let picture = browser.capture(None, 1.0).await.unwrap();
    assert!(!picture.bytes.is_empty());
    assert!(browser.containment_reason().is_none());
    browser.close().await.unwrap();
}

#[tokio::test]
async fn sandboxed_srcdoc_subframe_stops_before_disclosure() {
    assert_disclosure_stops(
        "const child = document.createElement('iframe');
         child.sandbox = '';
         child.srcdoc = otherDocument;
         document.body.append(child)",
        &[
            "Chromium created an isolated subframe",
            "a subframe document was outside the preview origin",
            "a subframe storage origin was outside the preview origin",
            "Chromium could not verify a subframe storage origin",
        ],
    )
    .await;
}

#[tokio::test]
async fn sandboxed_http_subframe_stops_before_disclosure() {
    assert_disclosure_stops(
        "const child = document.createElement('iframe');
         child.style = 'position:fixed;inset:0;width:100%;height:100%;border:0';
         child.src = '/opaque';
         document.body.append(child)",
        &["subframe"],
    )
    .await;
}

async fn disclosure_fixture(script: &'static str) -> u16 {
    serve(move |request| {
        if request.uri().path() == "/child" {
            return page_body("<body style='background:lime'>Same-origin frame</body>
                <script>history.replaceState({}, '', '#ready')</script>");
        }
        if request.uri().path() == "/opaque" {
            let mut response = page_body("<body style='background:red'>Opaque origin</body>");
            response.headers_mut().insert(
                "content-security-policy",
                http::HeaderValue::from_static("sandbox allow-scripts"),
            );
            return response;
        }
        page_body(format!(
            "<!doctype html>
             <body style='margin:0;background:blue;color:white'>
             <div data-testid=preview style='padding:30px'>Registered preview</div>
             <iframe src='/child' style='position:absolute;top:100px;left:100px'></iframe>
             <script>
               history.replaceState({{}}, '', '/?tab=design#ready');
               const permissions = ['geolocation', 'clipboard-read', 'clipboard-write', 'notifications'];
               Promise.all(permissions.map(name => navigator.permissions.query({{name}})))
                 .then(results => document.querySelector('[data-testid=preview]').textContent =
                     results.map(result => result.state).join(' '));
               const otherDocument = '<body style=background:red><button id=outside>Outside preview</button>';
               const fixture = () => URL.createObjectURL(new Blob([otherDocument], {{type:'text/html'}}));
               document.addEventListener('click', () => {{ {script} }}, {{once:true}});
             </script>"
        ))
    })
    .await
}

async fn open_disclosure_fixture(
    binary: std::path::PathBuf,
    path_env: String,
    port: u16,
) -> Option<(tempfile::TempDir, Browser)> {
    assert_ne!(port, 0);
    let preview_url = format!("http://127.0.0.1:{port}/");
    let root = tempfile::tempdir().unwrap();
    let spec = LaunchSpec {
        binary,
        profile_dir: root.path().join("disclosure"),
        sandbox: std::env::var("ARCHON_DESIGN_SANDBOX_OFF").as_deref() != Ok("1"),
        preview_url: preview_url.clone(),
        upstream_guard: TestUpstream::allowing(),
        viewport: Viewport {
            width: 460,
            height: 320,
            touch: false,
            dpr: 1.0,
        },
        path_env,
        lang: "C.UTF-8".into(),
        tz: "UTC".into(),
    };
    let browser = launch(spec).await?;
    assert!(
        browser
            .navigate_settled(&preview_url, Duration::from_secs(5))
            .await
            .unwrap()
    );
    Some((root, browser))
}

async fn assert_disclosure_stops(script: &'static str, expected_reasons: &[&str]) {
    let Some((binary, path_env)) = browser_options() else {
        return;
    };
    let port = disclosure_fixture(script).await;
    let Some((_root, browser)) = open_disclosure_fixture(binary, path_env, port).await else {
        return;
    };
    let mut events = browser.events();
    let mut feed = browser.start_screencast().await.unwrap().subscribe();
    let baseline = browser.capture(None, 1.0).await.unwrap();
    let main = browser.resolve(40.0, 40.0, 0).await.unwrap();
    assert_eq!(main.selector.as_deref(), Some("[data-testid=\"preview\"]"));
    assert_eq!(
        main.role_name.as_deref(),
        Some("div \"denied denied denied denied\"")
    );
    assert_eq!(
        browser.current_location().await.unwrap(),
        "/?tab=design#ready"
    );
    assert_eq!(browser.current_path().await.unwrap(), "/");
    let frame = tokio::time::timeout(Duration::from_secs(5), feed.next())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(frame.jpeg, baseline.bytes);
    feed.ack(frame.seq);

    let (_, (), ()) = tokio::time::timeout(Duration::from_secs(8), async {
        tokio::join!(
            click_fixture(&browser),
            check_racing_reads(&browser, &baseline.bytes, &main),
            async {
                while let Some(frame) = feed.next().await {
                    assert_eq!(frame.jpeg, baseline.bytes, "unverified stream frame");
                    feed.ack(frame.seq);
                }
            }
        )
    })
    .await
    .expect("disclosures did not stop");

    assert_containment_reported(&browser, &mut events, expected_reasons).await;
    assert!(feed.next().await.is_none());
    assert_preview_process_stopped(browser.pid().unwrap()).await;
    browser.close().await.unwrap();
}

async fn assert_containment_reported(
    browser: &Browser,
    events: &mut tokio::sync::broadcast::Receiver<BrowserEvent>,
    expected: &[&str],
) {
    let reason = browser
        .containment_reason()
        .expect("missing containment reason");
    assert!(expected.iter().any(|why| reason.contains(why)), "{reason}");
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            match events.recv().await.unwrap() {
                BrowserEvent::ContainmentStopped { reason: reported } => {
                    assert_eq!(reported, reason);
                    break;
                }
                BrowserEvent::Exited => panic!("containment was reported as a crash"),
                _ => {}
            }
        }
    })
    .await
    .expect("missing containment event");
    assert!(matches!(
        browser.capture(None, 1.0).await,
        Err(BrowserError::Containment(_))
    ));
    assert!(matches!(
        browser.resolve(40.0, 40.0, 0).await,
        Err(BrowserError::Containment(_))
    ));
}

async fn assert_preview_process_stopped(pid: u32) {
    let stat = format!("/proc/{pid}/stat");
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            match std::fs::read_to_string(&stat) {
                Ok(stat) => {
                    let (_, fields) = stat.rsplit_once(") ").expect("process stat fields");
                    if fields.starts_with("Z ") {
                        return;
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return,
                Err(error) => panic!("could not verify preview process exit: {error}"),
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("containment did not stop the preview process");
}

async fn click_fixture(browser: &Browser) {
    let result = browser
        .input(
            &[MouseAction::Down, MouseAction::Up].map(|action| Input::Mouse {
                action,
                x: 40.0,
                y: 40.0,
                button: MouseButton::Left,
                click_count: 1,
                modifiers: 0,
            }),
        )
        .await;
    assert!(
        matches!(result, Ok(()) | Err(BrowserError::Containment(_))),
        "{result:?}"
    );
}

async fn check_racing_reads(
    browser: &Browser,
    baseline: &[u8],
    main: &archon_design_browser::Resolved,
) {
    while browser.containment_reason().is_none() {
        let (picture, element) =
            tokio::join!(browser.capture(None, 1.0), browser.resolve(40.0, 40.0, 0));
        match picture {
            Ok(picture) => assert_eq!(picture.bytes, baseline, "unverified screenshot"),
            Err(
                BrowserError::DisclosureUnavailable
                | BrowserError::Containment(_)
                | BrowserError::Closed,
            ) => {}
            Err(error) => panic!("capture: {error}"),
        }
        match element {
            Ok(element) => assert_eq!(&element, main, "unverified DOM result"),
            Err(
                BrowserError::DisclosureUnavailable
                | BrowserError::Containment(_)
                | BrowserError::Closed,
            ) => {}
            Err(error) => panic!("resolve: {error}"),
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}
