//! Browser features a headless preview hands to the operator, in a real
//! Chromium: a file upload and a drop, a download, a confirm() dialog, a
//! new-tab link and a native select. Skipped unless ARCHON_DESIGN_REAL_BROWSER=1.

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use http_body_util::Full;
use hyper::{Request, Response, body::Incoming};
use hyper_util::rt::TokioIo;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::broadcast;

use archon_design_browser::{
    Browser, BrowserError, BrowserEvent, Input, LaunchSpec, MouseAction, MouseButton,
    UpstreamGuard, Viewport, find_browser,
};

const CSV: &str = "bay,row,tier\n01,02,82\n";

/// Each control at a fixed place; the page reports what happened in its query string.
const PAGE: &str = "<!doctype html><body style='margin:0;font:14px sans-serif'>
<style>body>*{position:absolute;left:10px;width:200px;height:30px;margin:0}</style>
<input id=f type=file accept='.edi,.csv' multiple style='top:10px'>
<a id=dl href='/report' download style='top:60px'>Download</a>
<button id=c style='top:110px'>Delete</button>
<a id=tab href='/second' target=_blank style='top:160px'>Open in new tab</a>
<select id=s style='top:210px'><option value=a>Alpha</option><option value=b>Beta</option>
<optgroup label=More><option value=c>Gamma</option></optgroup></select>
<div id=drop style='top:260px;height:60px;background:#ddd'>Drop here</div>
<script>
const report = (key, value) => history.replaceState({}, '', '?' + key + '=' + encodeURIComponent(value));
const names = (files) => [...files].map((f) => f.name + ':' + f.size).join(',');
f.onchange = () => report('files', names(f.files));
c.onclick = () => report('confirm', confirm('Delete the lashing plan?'));
s.onchange = () => report('select', s.value);
drop.ondragover = (e) => e.preventDefault();
drop.ondrop = (e) => { e.preventDefault(); report('dropped', names(e.dataTransfer.files)); };
</script>";

#[derive(Debug)]
struct Loopback;

#[async_trait::async_trait]
impl UpstreamGuard for Loopback {
    async fn verify_peer(&self, peer: &TcpStream) -> Result<(), BrowserError> {
        assert!(peer.peer_addr().unwrap().ip().is_loopback());
        Ok(())
    }
}

fn respond(path: &str) -> Response<Full<Bytes>> {
    let (kind, body, attachment) = match path {
        "/report" => ("text/csv", CSV.to_owned(), true),
        "/second" => ("text/html", "<body>Second page</body>".to_owned(), false),
        _ => ("text/html", PAGE.to_owned(), false),
    };
    let mut response = Response::builder().header("content-type", kind);
    if attachment {
        response = response.header(
            "content-disposition",
            "attachment; filename=\"lashing.csv\"",
        );
    }
    response.body(Full::new(Bytes::from(body))).unwrap()
}

async fn serve() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        while let Ok((socket, _)) = listener.accept().await {
            tokio::spawn(async move {
                let service = hyper::service::service_fn(|request: Request<Incoming>| {
                    std::future::ready(Ok::<_, std::convert::Infallible>(respond(
                        request.uri().path(),
                    )))
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

struct Fixture {
    browser: Browser,
    events: broadcast::Receiver<BrowserEvent>,
    root: tempfile::TempDir,
}

async fn open() -> Option<Fixture> {
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
    let url = format!("http://127.0.0.1:{}/", serve().await);
    let root = tempfile::tempdir().unwrap();
    let viewport = Viewport {
        width: 480,
        height: 400,
        touch: false,
        dpr: 1.0,
    };
    let spec = LaunchSpec {
        binary,
        profile_dir: root.path().join("profile"),
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
    let events = browser.events();
    assert!(
        browser
            .navigate_settled(&url, Duration::from_secs(10))
            .await
            .unwrap()
    );
    Some(Fixture {
        browser,
        events,
        root,
    })
}

async fn click(browser: &Browser, x: f64, y: f64) {
    let events = [MouseAction::Down, MouseAction::Up].map(|action| Input::Mouse {
        action,
        x,
        y,
        button: MouseButton::Left,
        click_count: 1,
        modifiers: 0,
    });
    browser.input(&events).await.unwrap();
}

async fn next_event<T>(
    events: &mut broadcast::Receiver<BrowserEvent>,
    mut pick: impl FnMut(BrowserEvent) -> Option<T>,
) -> T {
    let wait = async {
        loop {
            if let Some(found) = pick(events.recv().await.expect("browser events end")) {
                return found;
            }
        }
    };
    tokio::time::timeout(Duration::from_secs(10), wait)
        .await
        .expect("the event came")
}

async fn wait_location(browser: &Browser, want: &str) {
    let mut last = String::new();
    for _ in 0..100 {
        last = browser.current_location().await.unwrap_or_default();
        if last == want {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("location {last:?}, wanted {want:?}");
}

#[tokio::test]
async fn a_file_chooser_takes_the_operators_files_and_a_drop_delivers_them() {
    let Some(Fixture {
        browser,
        mut events,
        root,
    }) = open().await
    else {
        return;
    };
    let file = root.path().join("BAPLIE.edi");
    std::fs::write(&file, "UNB+UNOA:2'").unwrap();
    click(&browser, 60.0, 25.0).await;
    let (node, multiple, accept) = next_event(&mut events, |event| match event {
        BrowserEvent::FileChooser {
            node,
            multiple,
            accept,
        } => Some((node, multiple, accept)),
        _ => None,
    })
    .await;
    assert!(multiple);
    assert_eq!(accept, ".edi,.csv");
    browser
        .set_files(node, std::slice::from_ref(&file))
        .await
        .unwrap();
    wait_location(&browser, "/?files=BAPLIE.edi%3A11").await;

    // A drop onto the file input is checked against its accept; elsewhere is not.
    let on_input = browser.accept_at(60.0, 25.0).await.unwrap();
    assert_eq!(on_input.as_deref(), Some(".edi,.csv"));
    assert_eq!(browser.accept_at(60.0, 290.0).await.unwrap(), None);
    browser.drop_files(60.0, 290.0, &[file]).await.unwrap();
    wait_location(&browser, "/?dropped=BAPLIE.edi%3A11").await;
    assert_eq!(browser.containment_reason(), None);
    browser.close().await.unwrap();
}

#[tokio::test]
async fn a_download_is_kept_for_the_operator() {
    let Some(Fixture {
        browser,
        mut events,
        ..
    }) = open().await
    else {
        return;
    };
    click(&browser, 40.0, 70.0).await;
    let (name, path, bytes) = next_event(&mut events, |event| match event {
        BrowserEvent::Downloaded { name, path, bytes } => Some((name, path, bytes)),
        BrowserEvent::DownloadFailed { reason, .. } => panic!("download failed: {reason}"),
        _ => None,
    })
    .await;
    assert_eq!(name, "lashing.csv");
    assert_eq!(bytes, CSV.len() as u64);
    assert_eq!(std::fs::read_to_string(path).unwrap(), CSV);
    browser.close().await.unwrap();
}

#[tokio::test]
async fn a_confirm_dialog_shows_its_text_and_takes_the_operators_answer() {
    let Some(Fixture {
        browser,
        mut events,
        ..
    }) = open().await
    else {
        return;
    };
    // The click's release is answered only once the dialog is.
    {
        let clicked = click(&browser, 60.0, 125.0);
        tokio::pin!(clicked);
        let shown = next_event(&mut events, |event| match event {
            BrowserEvent::Dialog { kind, message, .. } => Some((kind, message)),
            _ => None,
        });
        let (kind, message) = tokio::select! {
            () = &mut clicked => panic!("the click ended before the dialog opened"),
            shown = shown => shown,
        };
        assert_eq!(
            (kind.as_str(), message.as_str()),
            ("confirm", "Delete the lashing plan?")
        );
        let phone = Viewport {
            width: 390,
            height: 844,
            touch: true,
            dpr: 3.0,
        };
        let resized = tokio::time::timeout(Duration::from_secs(2), browser.set_viewport(phone));
        let refused = resized
            .await
            .expect("a resize under a dialog is refused at once");
        assert!(
            matches!(refused, Err(BrowserError::DialogOpen)),
            "{refused:?}"
        );
        browser.answer_dialog(false, None).await.unwrap();
        clicked.await;
        browser.set_viewport(phone).await.unwrap();
    }
    wait_location(&browser, "/?confirm=false").await;
    drop(events);
    browser.close().await.unwrap();
}

#[tokio::test]
async fn a_new_tab_link_opens_in_the_preview_and_back_returns() {
    let Some(Fixture {
        browser,
        mut events,
        ..
    }) = open().await
    else {
        return;
    };
    click(&browser, 40.0, 170.0).await;
    let url = next_event(&mut events, |event| match event {
        BrowserEvent::NewTab { url } => Some(url),
        _ => None,
    })
    .await;
    assert!(url.ends_with("/second"), "{url}");
    wait_location(&browser, "/second").await;
    assert!(browser.go_back().await.unwrap());
    wait_location(&browser, "/").await;
    assert_eq!(browser.containment_reason(), None);
    browser.close().await.unwrap();
}

#[tokio::test]
async fn a_native_select_is_answered_by_the_operator() {
    let Some(Fixture {
        browser,
        mut events,
        ..
    }) = open().await
    else {
        return;
    };
    click(&browser, 60.0, 225.0).await;
    let picker = next_event(&mut events, |event| match event {
        BrowserEvent::Picker(picker) => Some(picker),
        _ => None,
    })
    .await;
    assert_eq!(picker.control, "select");
    let labels: Vec<_> = picker
        .options
        .iter()
        .map(|o| (o.label.as_str(), o.group.as_str()))
        .collect();
    assert_eq!(labels, [("Alpha", ""), ("Beta", ""), ("Gamma", "More")]);
    assert!(picker.options[0].selected);
    assert!(browser.answer_picker(picker.id, Some("2")).await.unwrap());
    wait_location(&browser, "/?select=c").await;
    assert!(
        !browser.answer_picker(picker.id, Some("1")).await.unwrap(),
        "an answer is used once"
    );
    browser.close().await.unwrap();
}
