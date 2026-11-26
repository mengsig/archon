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
    Browser, BrowserError, ClipRect, Input, KeyAction, LaunchSpec, MouseAction, MouseButton,
    TouchAction, TouchPoint, UpstreamGuard, Viewport, find_browser,
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

/// A phone page with a fixed top bar. WIDE is empty, or one unbroken line
/// that makes the page wider than the phone: what a long typed word does.
const FIXED: &str = "<!doctype html><html><head>\
<meta name=viewport content=\"width=device-width,initial-scale=1\">\
<style>body{margin:0;font:16px sans-serif}\
#topbar{position:fixed;top:0;left:0;right:0;height:56px;background:#c00;z-index:2}\
main{padding-top:56px}.band{height:300px}</style></head>\
<body><header id=topbar>Top bar</header><main>WIDE\
<div id=b0 class=band style=background:#0a0></div><div id=b1 class=band style=background:#00a></div>\
<div id=b2 class=band style=background:#aa0></div><div id=b3 class=band style=background:#0aa></div>\
<div id=b4 class=band style=background:#a0a></div><div id=b5 class=band style=background:#555></div>\
</main></body></html>";
/// One colour under the bar and no scrollbar: a frame that keeps the bar at the top
/// shows what the resting frame shows, wherever the page has scrolled to.
const PLAIN: &str =
    "<style>.band{background:#0a0!important;height:800px}html{scrollbar-width:none}</style>";
const NO_BAR: &str = "<style>#topbar{visibility:hidden}</style>";
const WIDE_LINE: &str = "<p id=wide>abcdefghijklmnopqrstuvwxyzabcdefghijklmnopqrstuvwxyzabcdefghijklmnopqrstuvwxyzabcdefghijklmnopqrstuvwxyz</p>";

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
    // Right to left: scrolled sideways, Chrome puts the view left of x = 0.
    let rtl = Bytes::from(
        PAGE.replace("ROWS", &rows)
            .replace("<html>", "<html dir=rtl>"),
    );
    let fixed = Bytes::from(FIXED.replace("WIDE", PLAIN));
    let fixed_bare = Bytes::from(FIXED.replace("WIDE", &format!("{PLAIN}{NO_BAR}")));
    let fixed_wide = Bytes::from(FIXED.replace("WIDE", WIDE_LINE));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        while let Ok((socket, _)) = listener.accept().await {
            let pages = (wide.clone(), narrow.clone(), rtl.clone());
            let (wide, narrow, rtl) = pages;
            let (fixed, fixed_wide, fixed_bare) =
                (fixed.clone(), fixed_wide.clone(), fixed_bare.clone());
            tokio::spawn(async move {
                let service = hyper::service::service_fn(move |request: Request<Incoming>| {
                    let html = match request.uri().path() {
                        "/narrow" => narrow.clone(),
                        "/rtl" => rtl.clone(),
                        "/fixed" => fixed.clone(),
                        "/fixed-wide" => fixed_wide.clone(),
                        "/fixed-bare" => fixed_bare.clone(),
                        _ => wide.clone(),
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

/// A phone shows the wide page zoomed out. A region drawn on its frame and an
/// element's box were cropped as if the page were at scale 1: from its corner.
#[tokio::test]
async fn a_zoomed_out_phone_is_cropped_where_the_frame_shows() {
    let root = tempfile::tempdir().unwrap();
    let Some((b, _)) = open(PHONE, &root.path().join("zoom")).await else {
        return;
    };
    let before = settled_frame(&b, "zoomed-out").await;
    let frame = b.capture(None, 1.0).await.unwrap();
    let zoom = frame.capture.screenshot_scale;
    assert!(zoom < 0.5, "the wide page is shown zoomed out: {zoom}");
    assert_eq!((frame.width, frame.height), (PHONE.width, PHONE.height));

    // The fourth band starts 960 px down the page, at 960 × zoom on the frame.
    let band = b.resolve(100.0, 300.0, 0).await.unwrap();
    assert_eq!(band.selector.as_deref(), Some("div#b3"));
    assert!(band.y <= 300.0 && 300.0 <= band.y + band.h, "{band:?}");
    assert!((band.y - 960.0 * zoom).abs() < 1.0 && (band.h - 300.0 * zoom).abs() < 1.0);
    let element = ClipRect {
        x: band.x,
        y: band.y,
        w: band.w,
        h: band.h,
    };
    let shot = b.capture(Some(element), 1.0).await.unwrap();
    assert_eq!(
        (shot.capture.y, shot.capture.h),
        (960, 300),
        "{:?}",
        shot.capture
    );
    assert!(
        (f64::from(shot.height) - band.h).abs() <= 1.0,
        "{}",
        shot.height
    );

    let region = ClipRect {
        x: 0.0,
        y: 300.0,
        w: 195.0,
        h: 100.0,
    };
    let crop = b.capture(Some(region), 1.0).await.unwrap();
    let page = (region.y / zoom, region.w / zoom, region.h / zoom);
    let area = (crop.capture.y, crop.capture.w, crop.capture.h);
    assert_eq!(area, (page.0 as u32, page.1 as u32, page.2 as u32));
    assert_eq!(crop.capture.screenshot_scale, zoom);
    assert!(crop.width.abs_diff(195) <= 1 && crop.height.abs_diff(100) <= 1);

    // Two patches of that band look alike and one of the band above does not;
    // shot from the page's corner, all three were the first band.
    let patch = |x, y| ClipRect {
        x,
        y,
        w: 100.0,
        h: 50.0,
    };
    let one = b.capture(Some(patch(0.0, 250.0)), 1.0).await.unwrap();
    let other = b.capture(Some(patch(250.0, 255.0)), 1.0).await.unwrap();
    let above = b.capture(Some(patch(0.0, 180.0)), 1.0).await.unwrap();
    let same = b
        .pictures_match((one.mime, &one.bytes), (other.mime, &other.bytes))
        .await;
    assert!(same.unwrap(), "two patches of one band");
    let alike = b
        .pictures_match((one.mime, &one.bytes), (above.mime, &above.bytes))
        .await;
    assert!(!alike.unwrap(), "a patch of the band above");

    // The crops leave the operator's view as it was.
    assert!(settled_frame(&b, "zoomed-out-after").await == before);
    b.close().await.unwrap();
}

/// A right-to-left page scrolled sideways reports a negative offset; the
/// frame was shot from x = 0 and showed the wrong part of the page.
#[tokio::test]
async fn a_right_to_left_page_scrolled_sideways_is_framed_as_chrome_shows_it() {
    let root = tempfile::tempdir().unwrap();
    let Some((b, url)) = open(DESKTOP, &root.path().join("rtl")).await else {
        return;
    };
    b.navigate(&format!("{url}rtl")).await.unwrap();
    let _ = b
        .wait_network_idle(Duration::from_millis(300), Duration::from_secs(10))
        .await;
    let start = settled_frame(&b, "rtl-start").await;
    b.input(&[wheel(100.0, 400.0, -350.0, 0.0)]).await.unwrap();
    let sideways = settled_frame(&b, "rtl-sideways").await;
    differ(&sideways, &start, "rtl sideways");
    // The header is as wide as the body, at the page's right: now only the
    // view's right part shows it; the bands run on under its left part.
    assert!(element_at(&b, 700.0, 10.0).await.contains("header"));
    assert_eq!(element_at(&b, 100.0, 10.0).await, "html");
    assert_eq!(element_at(&b, 100.0, 300.0).await, "div#b0");
    b.close().await.unwrap();
}

/// The phone preview's red "Must send a TouchStart first" toasts: a swipe
/// comes as many batches while the stream runs, and Chrome lifts the finger
/// on a zoom reset, or a lost batch cancels it. Nothing of that is an error,
/// and the next swipe scrolls.
#[tokio::test]
async fn a_swipe_in_batches_survives_a_lifted_finger_and_the_next_one_scrolls() {
    let root = tempfile::tempdir().unwrap();
    let Some((b, url)) = open(PHONE, &root.path().join("batches")).await else {
        return;
    };
    b.navigate(&format!("{url}narrow")).await.unwrap();
    let _ = b
        .wait_network_idle(Duration::from_millis(300), Duration::from_secs(10))
        .await;
    let _stream = b.start_screencast().await.unwrap();
    let finger = |action, y: f64| Input::Touch {
        action,
        points: vec![TouchPoint { x: 200.0, y, id: 3 }],
    };
    b.input(&[finger(TouchAction::Start, 700.0)]).await.unwrap();
    b.input(&[finger(TouchAction::Move, 650.0)]).await.unwrap();
    // A crop resets the page zoom, which lifts the finger in Chrome.
    let crop = ClipRect {
        x: 0.0,
        y: 0.0,
        w: 100.0,
        h: 100.0,
    };
    b.capture(Some(crop), 2.0).await.unwrap();
    b.input(&[finger(TouchAction::Move, 600.0)]).await.unwrap();
    b.input_lost();
    b.input(&[finger(TouchAction::Move, 550.0)]).await.unwrap();
    b.input(&[finger(TouchAction::Cancel, 550.0)])
        .await
        .unwrap();
    let before = element_at(&b, 100.0, 300.0).await;
    assert!(["div#b0", "div#b1"].contains(&before.as_str()), "{before}");
    let mut swipe = vec![finger(TouchAction::Start, 700.0)];
    for i in 1..=10 {
        swipe.push(finger(TouchAction::Move, 700.0 - f64::from(i) * 50.0));
    }
    swipe.push(finger(TouchAction::End, 200.0));
    for event in &swipe {
        b.input(std::slice::from_ref(event)).await.unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    settled_frame(&b, "phone-batched-swipe").await;
    assert_ne!(
        element_at(&b, 100.0, 300.0).await,
        before,
        "the next swipe scrolled the page"
    );
    b.close().await.unwrap();
}

fn finger(action: TouchAction, y: f64) -> Input {
    Input::Touch {
        action,
        points: vec![TouchPoint { x: 200.0, y, id: 0 }],
    }
}

/// Load `path` on the phone, resting, with the fixed bar's box at its start,
/// and give back that box and the resting frame.
async fn fixed_bar_page(b: &Browser, url: &str, path: &str) -> (ClipRect, Vec<u8>) {
    b.navigate(&format!("{url}{path}")).await.unwrap();
    let _ = b
        .wait_network_idle(Duration::from_millis(300), Duration::from_secs(10))
        .await;
    let rest = settled_frame(b, path).await;
    let bar = b.resolve_selector("#topbar", None).await.unwrap();
    let bar = bar.expect("the page has its fixed bar");
    assert_eq!(
        (bar.y, bar.h),
        (0.0, 56.0),
        "the bar starts at the top: {bar:?}"
    );
    assert!(element_at(b, 10.0, 10.0).await.contains("topbar"));
    (bar, rest)
}

/// The fixed bar's box and what is drawn at the view's top-left corner.
async fn bar_sample(b: &Browser) -> (ClipRect, String) {
    let bar = b.resolve_selector("#topbar", None).await.unwrap();
    (
        bar.expect("the fixed bar stays on the page"),
        element_at(b, 10.0, 10.0).await,
    )
}

/// Every frame the stream sends while it runs, until `stop` is set.
fn collect_frames(
    relay: &archon_design_browser::FrameRelay,
    stop: Arc<std::sync::atomic::AtomicBool>,
) -> tokio::task::JoinHandle<Vec<Vec<u8>>> {
    let mut frames = relay.subscribe();
    tokio::spawn(async move {
        let mut got = Vec::new();
        while !stop.load(std::sync::atomic::Ordering::SeqCst) {
            let Ok(next) = tokio::time::timeout(Duration::from_millis(200), frames.next()).await
            else {
                continue;
            };
            let frame = next.expect("the stream runs");
            assert_eq!((frame.width, frame.height), (PHONE.width, PHONE.height));
            frames.ack(frame.seq);
            got.push(frame.jpeg);
        }
        got
    })
}

/// One step of a scroll: the bar's box is at the view's top, the bar is what
/// is drawn there, and Chrome's own picture of the view is kept to check.
async fn sampled_step(b: &Browser, step: &str, shots: &mut Vec<Vec<u8>>) {
    // A finger's pace, so the stream sends frames while the scroll runs.
    tokio::time::sleep(Duration::from_millis(40)).await;
    let (bar, top) = bar_sample(b).await;
    assert_eq!(
        (bar.y, bar.w),
        (0.0, f64::from(PHONE.width)),
        "{step}: {bar:?}"
    );
    assert!(
        top.contains("topbar"),
        "{step}: {top} is drawn over the bar"
    );
    shots.push(b.viewport_screenshot().await.unwrap());
}

/// Each picture shows what the resting frame shows: the bar at the top.
async fn all_show_the_bar(b: &Browser, rest: &[u8], pictures: &[Vec<u8>], what: &str) {
    assert!(!pictures.is_empty(), "{what}: no pictures to check");
    for (n, picture) in pictures.iter().enumerate() {
        let same = b
            .pictures_match(("image/jpeg", rest), ("image/jpeg", picture))
            .await
            .unwrap();
        if !same {
            keep(&format!("fixed-{what}-{n}.jpg"), picture);
            panic!(
                "{what} {n} of {}: the bar is not drawn at the top",
                pictures.len()
            );
        }
    }
}

/// A phone page that fits the phone keeps its fixed bar at the view's top in
/// every stream frame and every step of a swipe, batch by batch, and of the wheel.
#[tokio::test]
async fn a_phone_page_that_fits_keeps_its_fixed_bar_at_every_step_of_a_scroll() {
    let root = tempfile::tempdir().unwrap();
    let Some((b, url)) = open(PHONE, &root.path().join("fixed")).await else {
        return;
    };
    let (bar, rest) = fixed_bar_page(&b, &url, "fixed").await;
    assert_eq!(bar.w, f64::from(PHONE.width), "the page fits the phone");
    let relay = b.start_screencast().await.unwrap();
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let collector = collect_frames(&relay, stop.clone());
    let started = Instant::now();
    let mut shots = Vec::new();
    for round in 0..4 {
        let moves = (1..=10).map(|i| finger(TouchAction::Move, 700.0 - f64::from(i) * 30.0));
        let swipe = std::iter::once(finger(TouchAction::Start, 700.0))
            .chain(moves)
            .chain(std::iter::once(finger(TouchAction::End, 400.0)));
        for step in swipe {
            b.input(&[step]).await.unwrap();
            sampled_step(&b, &format!("swipe {round}"), &mut shots).await;
        }
    }
    for _ in 0..4 {
        b.input(&[wheel(200.0, 400.0, 0.0, 200.0)]).await.unwrap();
        sampled_step(&b, "the wheel", &mut shots).await;
    }
    let ran = started.elapsed();
    stop.store(true, std::sync::atomic::Ordering::SeqCst);
    let frames = collector.await.unwrap();
    assert_eq!(shots.len(), 52, "every step was sampled");
    // The stream sends a frame at least every 250 ms; half that rate is the floor.
    let floor = usize::try_from(ran.as_millis() / 500).unwrap().max(1);
    let sent = frames.len();
    assert!(
        sent >= floor,
        "the stream ran throughout: {sent} frames in {ran:?}"
    );
    assert!(
        element_at(&b, 10.0, 300.0).await != "div#b0",
        "the page scrolled"
    );
    all_show_the_bar(&b, &rest, &frames, "stream-frame").await;
    all_show_the_bar(&b, &rest, &shots, "step").await;
    // The check sees a missing bar: the same page without it does not match.
    let bare = bare_page_frame(&b, &url).await;
    let same = b.pictures_match(("image/jpeg", &rest), ("image/jpeg", &bare));
    assert!(!same.await.unwrap(), "a frame without the bar matched");
    b.close().await.unwrap();
}

/// The fitting page with its bar hidden, resting.
async fn bare_page_frame(b: &Browser, url: &str) -> Vec<u8> {
    b.navigate(&format!("{url}fixed-bare")).await.unwrap();
    let _ = b
        .wait_network_idle(Duration::from_millis(300), Duration::from_secs(10))
        .await;
    let bare = settled_frame(b, "fixed-bare").await;
    assert!(!element_at(b, 10.0, 10.0).await.contains("topbar"));
    bare
}

/// A phone page wider than the phone lays out as on Chrome for Android: a scroll
/// moves the view inside a layout viewport widened to the page, and the bar off the top.
#[tokio::test]
async fn a_phone_page_wider_than_the_phone_scrolls_its_fixed_bar_as_chrome_android_does() {
    let root = tempfile::tempdir().unwrap();
    let Some((b, url)) = open(PHONE, &root.path().join("fixed-wide")).await else {
        return;
    };
    let (bar, _) = fixed_bar_page(&b, &url, "fixed-wide").await;
    assert!(
        bar.w > f64::from(PHONE.width) * 1.5,
        "the bar spans the widened layout: {bar:?}"
    );
    let zoom = b.capture(None, 1.0).await.unwrap().capture.screenshot_scale;
    assert_eq!(zoom, 1.0, "initial-scale=1 is kept, not zoomed out");
    // The layout viewport keeps the phone's shape at the page's width.
    let room = f64::from(PHONE.height) * bar.w / f64::from(PHONE.width) - f64::from(PHONE.height);
    let mut lowest: f64 = 0.0;
    for round in 0..4 {
        let mut swipe = vec![finger(TouchAction::Start, 700.0)];
        swipe.extend((1..=10).map(|i| finger(TouchAction::Move, 700.0 - f64::from(i) * 30.0)));
        swipe.push(finger(TouchAction::End, 400.0));
        b.input(&swipe).await.unwrap();
        keep(
            &format!("fixed-wide-swipe-{round}.jpg"),
            &settled_frame(&b, "fixed-wide-swipe").await,
        );
        let (moved, _) = bar_sample(&b).await;
        assert!(
            moved.y <= lowest,
            "round {round}: the bar went back down: {moved:?}"
        );
        assert!(
            moved.y >= -room - 1.0,
            "round {round}: past the layout viewport: {moved:?}"
        );
        lowest = moved.y;
    }
    assert!(
        (lowest + room).abs() <= 1.0,
        "the view reached the layout viewport's foot: {lowest} of {room}"
    );
    assert!(
        !element_at(&b, 10.0, 10.0).await.contains("topbar"),
        "the bar left the view"
    );
    // Chrome may put the view back where it was; the layout itself is as at first.
    b.reload().await.unwrap();
    let _ = b
        .wait_network_idle(Duration::from_millis(300), Duration::from_secs(10))
        .await;
    settled_frame(&b, "fixed-wide-reloaded").await;
    let (again, _) = bar_sample(&b).await;
    assert_eq!(
        (again.x, again.w, again.h),
        (bar.x, bar.w, bar.h),
        "after a reload: {again:?}"
    );
    b.close().await.unwrap();
}

const TABLET: Viewport = Viewport {
    width: 834,
    height: 1112,
    touch: true,
    dpr: 1.0,
};

/// Size changes 60 ms apart while the stream runs end on the last size. A resize
/// during a stream shot left Chrome drawing the old size and every frame withheld.
#[tokio::test]
async fn quick_size_changes_while_streaming_end_on_the_last_size() {
    let root = tempfile::tempdir().unwrap();
    let Some((b, url)) = open(DESKTOP, &root.path().join("rapid")).await else {
        return;
    };
    b.navigate(&format!("{url}narrow")).await.unwrap();
    let _ = b
        .wait_network_idle(Duration::from_millis(300), Duration::from_secs(10))
        .await;
    let relay = b.start_screencast().await.unwrap();
    let mut frames = relay.subscribe();
    let bursts: [&[Viewport]; 6] = [
        &[PHONE, DESKTOP, TABLET, PHONE],
        &[DESKTOP, TABLET, PHONE, DESKTOP],
        &[TABLET, PHONE, DESKTOP, TABLET],
        &[PHONE, DESKTOP, TABLET, PHONE, DESKTOP, TABLET],
        &[TABLET, DESKTOP, PHONE],
        &[PHONE, TABLET, DESKTOP],
    ];
    for (n, burst) in bursts.iter().enumerate() {
        for v in burst.iter() {
            // A shot of the view is in flight as the size changes, as a stream shot often is.
            let (shot, resized) = tokio::join!(b.viewport_screenshot(), async {
                tokio::task::yield_now().await;
                b.set_viewport(*v).await
            });
            shot.unwrap();
            resized.unwrap();
            tokio::time::sleep(Duration::from_millis(60)).await;
        }
        let last = *burst.last().unwrap();
        let start = Instant::now();
        let at_last =
            |f: &archon_design_browser::Frame| (f.width, f.height) == (last.width, last.height);
        loop {
            let left = Duration::from_secs(3).saturating_sub(start.elapsed());
            let got = tokio::time::timeout(left, frames.next()).await;
            let frame =
                got.unwrap_or_else(|_| panic!("burst {n}: no frame at its last size in 3 s"));
            let frame = frame.expect("the stream runs");
            frames.ack(frame.seq);
            if at_last(&frame) {
                break;
            }
        }
        assert_eq!(
            b.capture(None, 1.0).await.unwrap().width,
            last.width,
            "burst {n}"
        );
    }
    b.close().await.unwrap();
}
