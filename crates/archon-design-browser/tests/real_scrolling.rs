//! Scrolling in a real Chromium: every way an operator scrolls moves the page
//! (or the panel under the pointer), and the stream frame shown once it rests
//! is the picture Chrome itself shows. Skipped unless ARCHON_DESIGN_REAL_BROWSER=1.

use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use http_body_util::Full;
use hyper::{Request, Response, body::Incoming};
use hyper_util::rt::TokioIo;
use tokio::net::{TcpListener, TcpStream};

use archon_design_browser::{
    Browser, BrowserError, Input, KeyAction, LaunchSpec, MouseAction, MouseButton, TouchAction,
    TouchPoint, UpstreamGuard, Viewport, find_browser,
};

const DESKTOP: Viewport = Viewport {
    width: 800,
    height: 600,
    touch: false,
    dpr: 1.0,
};
const PHONE: Viewport = Viewport {
    width: 390,
    height: 844,
    touch: true,
    dpr: 1.0,
};

/// A sticky header, a fixed badge, six 300 px bands wider than the view and
/// an inner scrolling panel of twenty 40 px rows.
const PAGE: &str = "<!doctype html><html><head><meta name=viewport content=\"width=device-width\">\
<style>body{margin:0;font:16px sans-serif}\
header{position:sticky;top:0;height:60px;background:#c00;color:#fff;z-index:2}\
.band{height:300px;width:1600px}\
#panel{position:absolute;left:400px;top:100px;width:300px;height:200px;overflow:auto;background:#fff;z-index:1}\
#panel div{box-sizing:border-box;height:40px;border-bottom:1px solid #000}\
#badge{position:fixed;right:20px;bottom:20px;width:80px;height:40px;background:#000}</style></head>\
<body><header>Cargo header</header>\
<div id=b0 class=band style=background:#0a0></div><div id=b1 class=band style=background:#00a></div>\
<div id=b2 class=band style=background:#aa0></div><div id=b3 class=band style=background:#0aa></div>\
<div id=b4 class=band style=background:#a0a></div><div id=b5 class=band style=background:#555></div>\
<div id=panel>ROWS</div><div id=badge></div></body></html>";

#[derive(Debug)]
struct Loopback;

#[async_trait::async_trait]
impl UpstreamGuard for Loopback {
    async fn verify_peer(&self, peer: &TcpStream) -> Result<(), BrowserError> {
        assert!(peer.peer_addr().unwrap().ip().is_loopback());
        Ok(())
    }
}

async fn serve_page() -> u16 {
    let rows: String = (0..20)
        .map(|i| format!("<div id=r{i}>row {i}</div>"))
        .collect();
    let wide = Bytes::from(PAGE.replace("ROWS", &rows));
    // As wide as the view: a phone shows it unzoomed, so a swipe has room to scroll.
    let narrow = Bytes::from(
        PAGE.replace("ROWS", &rows)
            .replace("width:1600px", "width:auto")
            .replace("left:400px", "left:40px"),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        while let Ok((socket, _)) = listener.accept().await {
            let (wide, narrow) = (wide.clone(), narrow.clone());
            tokio::spawn(async move {
                let service = hyper::service::service_fn(move |request: Request<Incoming>| {
                    let html = if request.uri().path() == "/narrow" {
                        narrow.clone()
                    } else {
                        wide.clone()
                    };
                    let page = Response::builder()
                        .header("content-type", "text/html")
                        .body(Full::new(html))
                        .unwrap();
                    std::future::ready(Ok::<_, std::convert::Infallible>(page))
                });
                let served = hyper::server::conn::http1::Builder::new()
                    .serve_connection(TokioIo::new(socket), service)
                    .await;
                if let Err(error) = served {
                    assert!(error.is_incomplete_message(), "{error}");
                }
            });
        }
    });
    port
}

async fn open(viewport: Viewport, profile: &std::path::Path) -> Option<(Browser, String)> {
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
    let url = format!("http://127.0.0.1:{}/", serve_page().await);
    let spec = LaunchSpec {
        binary,
        profile_dir: profile.to_path_buf(),
        sandbox: std::env::var("ARCHON_DESIGN_SANDBOX_OFF").as_deref() != Ok("1"),
        preview_url: url.clone(),
        upstream_guard: Arc::new(Loopback),
        viewport,
        path_env: path,
        lang: "C.UTF-8".into(),
        tz: "UTC".into(),
    };
    let browser = match Browser::launch(spec).await {
        Ok(browser) => browser,
        Err(BrowserError::SandboxUnavailable { hint }) => {
            eprintln!("skipped: sandbox unavailable: {hint}");
            return None;
        }
        Err(error) => panic!("launch: {error}"),
    };
    browser.set_viewport(viewport).await.unwrap();
    browser.navigate(&url).await.unwrap();
    let _ = browser
        .wait_network_idle(Duration::from_millis(300), Duration::from_secs(10))
        .await;
    Some((browser, url))
}

/// Saves `bytes` under ARCHON_DESIGN_EVIDENCE_DIR, when it is set.
fn keep(name: &str, bytes: &[u8]) {
    if let Ok(dir) = std::env::var("ARCHON_DESIGN_EVIDENCE_DIR") {
        std::fs::write(std::path::Path::new(&dir).join(name), bytes).unwrap();
    }
}

/// The stream frame once the page rests: it must equal Chrome's own picture.
async fn settled_frame(b: &Browser, step: &str) -> Vec<u8> {
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut previous = b.viewport_screenshot().await.unwrap();
    loop {
        tokio::time::sleep(Duration::from_millis(200)).await;
        let frame = b.capture(None, 1.0).await.unwrap().bytes;
        let direct = b.viewport_screenshot().await.unwrap();
        if frame == direct && direct == previous {
            return frame;
        }
        if Instant::now() > deadline {
            keep(&format!("{step}-frame.jpg"), &frame);
            keep(&format!("{step}-chrome.jpg"), &direct);
            panic!("{step}: the settled frame never matched Chrome's own picture");
        }
        previous = direct;
    }
}

async fn element_at(b: &Browser, x: f64, y: f64) -> String {
    match b.resolve(x, y, 0).await {
        Ok(found) => found.selector.unwrap_or_default(),
        Err(error) => panic!("nothing found at ({x}, {y}): {error}"),
    }
}

fn differ(a: &[u8], b: &[u8], what: &str) {
    assert!(a != b, "{what}: the frame did not change");
}

fn wheel(x: f64, y: f64, dx: f64, dy: f64) -> Input {
    Input::Wheel {
        x,
        y,
        dx,
        dy,
        modifiers: 0,
    }
}

fn mouse(action: MouseAction, x: f64, y: f64) -> Input {
    Input::Mouse {
        action,
        x,
        y,
        button: MouseButton::Left,
        click_count: 1,
        modifiers: 0,
    }
}

fn key_press(key: &str) -> [Input; 2] {
    [KeyAction::Down, KeyAction::Up].map(|action| Input::Key {
        action,
        key: key.into(),
        code: key.into(),
        text: None,
        modifiers: 0,
    })
}

#[tokio::test]
async fn wheel_keys_and_scrollbar_scroll_and_the_settled_frame_is_chromes() {
    let root = tempfile::tempdir().unwrap();
    let Some((b, _)) = open(DESKTOP, &root.path().join("scroll")).await else {
        return;
    };
    let top = settled_frame(&b, "top").await;
    assert_eq!(element_at(&b, 550.0, 150.0).await, "div#r1");
    assert!(element_at(&b, 10.0, 10.0).await.contains("header"));

    // An inner overflow panel under the pointer scrolls; the page does not.
    b.input(&[wheel(550.0, 200.0, 0.0, 120.0)]).await.unwrap();
    let nested = settled_frame(&b, "nested").await;
    differ(&nested, &top, "nested");
    assert_ne!(
        element_at(&b, 550.0, 150.0).await,
        "div#r1",
        "the panel scrolled"
    );
    assert_eq!(
        element_at(&b, 100.0, 300.0).await,
        "div#b0",
        "the page did not"
    );

    // The page scrolls under its sticky header: the operator's bug.
    b.input(&[wheel(100.0, 400.0, 0.0, 450.0)]).await.unwrap();
    let scrolled = settled_frame(&b, "page").await;
    keep("scrolled-frame.jpg", &scrolled);
    differ(&scrolled, &nested, "page");
    assert_eq!(element_at(&b, 100.0, 300.0).await, "div#b2");
    assert!(element_at(&b, 10.0, 10.0).await.contains("header"));

    // Sideways, then back.
    b.input(&[wheel(100.0, 400.0, 300.0, 0.0)]).await.unwrap();
    differ(&settled_frame(&b, "sideways").await, &scrolled, "sideways");
    b.input(&[wheel(100.0, 400.0, -300.0, 0.0)]).await.unwrap();
    assert!(
        settled_frame(&b, "back").await == scrolled,
        "back where it was"
    );

    // Keys scroll once the page (nothing editable) has focus.
    b.input(&[
        mouse(MouseAction::Down, 100.0, 400.0),
        mouse(MouseAction::Up, 100.0, 400.0),
    ])
    .await
    .unwrap();
    b.input(&key_press("PageDown")).await.unwrap();
    settled_frame(&b, "page-down").await;
    assert_eq!(element_at(&b, 100.0, 300.0).await, "div#b4");
    b.input(&key_press("Home")).await.unwrap();
    settled_frame(&b, "home").await;
    assert_eq!(element_at(&b, 100.0, 300.0).await, "div#b0");
    b.input(&key_press("End")).await.unwrap();
    settled_frame(&b, "end").await;
    assert_eq!(element_at(&b, 100.0, 300.0).await, "div#b5");
    b.input(&key_press("Home")).await.unwrap();
    assert!(settled_frame(&b, "home-again").await == nested, "home");

    // Dragging the page's scrollbar thumb down.
    let drag = [
        mouse(MouseAction::Down, 792.0, 60.0),
        mouse(MouseAction::Move, 792.0, 160.0),
        mouse(MouseAction::Move, 792.0, 260.0),
        mouse(MouseAction::Up, 792.0, 260.0),
    ];
    b.input(&drag).await.unwrap();
    settled_frame(&b, "scrollbar").await;
    assert_ne!(
        element_at(&b, 100.0, 300.0).await,
        "div#b0",
        "the thumb moved the page"
    );
    b.close().await.unwrap();
}

#[tokio::test]
async fn a_touch_swipe_scrolls_a_phone_preview_and_the_settled_frame_is_chromes() {
    let root = tempfile::tempdir().unwrap();
    let Some((b, url)) = open(PHONE, &root.path().join("swipe")).await else {
        return;
    };
    // A page wider than the phone is shown zoomed out, as Chrome shows it.
    settled_frame(&b, "phone-zoomed-out").await;
    b.navigate(&format!("{url}narrow")).await.unwrap();
    let _ = b
        .wait_network_idle(Duration::from_millis(300), Duration::from_secs(10))
        .await;
    let top = settled_frame(&b, "phone-top").await;
    assert_eq!(element_at(&b, 100.0, 300.0).await, "div#b0");
    let finger = |action, y| Input::Touch {
        action,
        points: vec![TouchPoint { x: 200.0, y, id: 0 }],
    };
    let mut swipe = vec![finger(TouchAction::Start, 700.0)];
    swipe.extend((1..=10).map(|i| finger(TouchAction::Move, 700.0 - f64::from(i) * 50.0)));
    swipe.push(finger(TouchAction::End, 200.0));
    b.input(&swipe).await.unwrap();
    let swiped = settled_frame(&b, "phone-swipe").await;
    differ(&swiped, &top, "swipe");
    assert_ne!(
        element_at(&b, 100.0, 300.0).await,
        "div#b0",
        "the page scrolled"
    );
    assert!(element_at(&b, 10.0, 10.0).await.contains("header"));
    b.close().await.unwrap();
}
