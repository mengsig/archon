//! T-I5: a real Chromium. Skipped unless ARCHON_DESIGN_REAL_BROWSER=1 and a browser is found.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::time::Duration;

use archon_design_browser::{
    Browser, BrowserError, BrowserEvent, LaunchSpec, Viewport, find_browser,
};

static HITS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

fn serve(body: &'static str) -> u16 {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for s in l.incoming() {
            let Ok(mut s) = s else { break };
            let mut buf = [0u8; 4096];
            let _ = s.read(&mut buf);
            if body == "fetched-ok" {
                HITS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
            let _ = write!(
                s,
                "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nAccess-Control-Allow-Origin: *\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
        }
    });
    port
}

#[tokio::test]
async fn real_browser_is_confined() {
    if std::env::var("ARCHON_DESIGN_REAL_BROWSER").as_deref() != Ok("1") {
        eprintln!("skipped: ARCHON_DESIGN_REAL_BROWSER=1 not set");
        return;
    }
    let path = std::env::var("PATH").unwrap_or_default();
    let home = std::path::PathBuf::from(std::env::var("HOME").unwrap_or_default());
    let Ok(binary) = find_browser(None, &path, &home) else {
        eprintln!("skipped: no browser found");
        return;
    };
    let root = tempfile::tempdir().unwrap();
    let profile = root.path().join("ds_test");
    let spec = LaunchSpec {
        binary,
        profile_dir: profile.clone(),
        sandbox: std::env::var("ARCHON_DESIGN_SANDBOX_OFF").as_deref() != Ok("1"),
        allow_hosts: vec![],
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
    let b = match Browser::launch(spec).await {
        Ok(b) => b,
        Err(BrowserError::SandboxUnavailable { hint }) => {
            eprintln!("skipped: sandbox unavailable: {hint}");
            return;
        }
        Err(e) => panic!("launch: {e}"),
    };
    let pid = b.pid().unwrap();
    let environ = std::fs::read(format!("/proc/{pid}/environ")).unwrap_or_default();
    let environ = String::from_utf8_lossy(&environ);
    assert!(
        !environ.contains("ARCHON_") && !environ.contains("AETHER_"),
        "{environ}"
    );

    let other = serve("fetched-ok");
    let page = serve(Box::leak(
        format!(
            "<html><body><button data-testid=save class=primary>Save changes</button><script>\
             window.res={{}};\
             const t=(k,u)=>fetch(u,{{mode:'no-cors'}}).then(()=>res[k]='ok',()=>res[k]='fail');\
             t('loop','http://127.0.0.1:{other}/');\
             t('ext','http://example.com/');t('meta','http://169.254.169.254/');t('priv','http://10.0.0.1/');\
             try{{const w=new WebSocket('ws://10.0.0.1:9/');w.onerror=()=>res.ws='fail';w.onopen=()=>res.ws='ok';}}catch(e){{res.ws='fail'}}\
             </script></body></html>"
        )
        .into_boxed_str(),
    ));
    let mut ev = b.events();
    b.navigate(&format!("http://127.0.0.1:{page}/x?y=1"))
        .await
        .unwrap();
    assert!(
        b.wait_network_idle(Duration::from_millis(500), Duration::from_secs(20))
            .await
            .unwrap()
    );
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert!(
        HITS.load(std::sync::atomic::Ordering::SeqCst) >= 1,
        "loopback fetch to another port failed"
    );
    assert_eq!(b.current_path().await.unwrap(), "/x");

    // Read the results through resolve's sibling path: the button's box proves the page rendered.
    let r = b.resolve(20.0, 20.0, 0).await.unwrap();
    assert_eq!(
        r.role_name.as_deref(),
        Some("button \"Save changes\""),
        "{r:?}"
    );
    assert_eq!(r.selector.as_deref(), Some("[data-testid=\"save\"]"));
    assert_eq!(r.path, "/x");

    let pic = b.capture(None, 1.0).await.unwrap();
    assert!(!pic.bytes.is_empty() && pic.bytes.len() <= 560 * 1024);

    let mut blocked = Vec::new();
    while let Ok(e) = ev.try_recv() {
        if let BrowserEvent::BlockedHost { host } = e {
            blocked.push(host);
        }
    }
    for h in ["example.com", "169.254.169.254", "10.0.0.1"] {
        assert!(
            blocked.iter().any(|b| b == h),
            "{h} not blocked: {blocked:?}"
        );
    }
    assert!(!blocked.iter().any(|h| h == "127.0.0.1"));

    b.close().await.unwrap();
    assert!(!profile.exists(), "profile still there");
}
