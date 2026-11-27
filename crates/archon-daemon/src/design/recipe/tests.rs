use super::*;
use archon_core::design::recipe::{RecipeStep, StepRun};
use tokio::net::TcpListener;

fn recipe(steps: Vec<RecipeStep>) -> PreviewRecipe {
    PreviewRecipe {
        source: "docs/dev-preview.md".into(),
        steps,
        env: vec!["DB_PASSWORD".into()],
        notes: None,
    }
}

fn step(name: &str, port: Option<u16>, ready: Option<String>) -> RecipeStep {
    RecipeStep {
        name: name.into(),
        command: format!("scripts/start-{name}"),
        dir: None,
        run: StepRun::Service,
        port,
        ready,
        port_pick: None,
    }
}

#[test]
fn a_recipe_is_saved_read_replaced_and_cleared_per_repository() {
    let mut store = Store::open_in_memory().unwrap();
    let (repo, other) = (RepoId::new(), RepoId::new());
    let item = QueueItemId::new();
    assert_eq!(load(&store, &repo).unwrap(), None);
    let first = save(
        &mut store,
        &repo,
        &item,
        recipe(vec![step("web", None, None)]),
    )
    .unwrap();
    assert_eq!(load(&store, &repo).unwrap(), Some(first.clone()));
    assert_eq!(load(&store, &other).unwrap(), None, "kept per repository");
    let updated = recipe(vec![
        step("db", Some(5432), None),
        step("web", Some(5173), None),
    ]);
    let second = save(&mut store, &repo, &item, updated.clone()).unwrap();
    assert_eq!(load(&store, &repo).unwrap().unwrap().recipe, updated);
    assert_ne!(first.recipe, second.recipe);
    assert!(clear(&mut store, &repo).unwrap());
    assert_eq!(load(&store, &repo).unwrap(), None);
    assert!(
        !clear(&mut store, &repo).unwrap(),
        "clearing twice is harmless"
    );
}

#[test]
fn an_invalid_or_secret_recipe_is_never_stored() {
    let mut store = Store::open_in_memory().unwrap();
    let repo = RepoId::new();
    let item = QueueItemId::new();
    for command in [
        "DB_PASSWORD=hunter2hunter2 npm start",
        "curl -uadmin:hunter2 http://127.0.0.1:8000/",
    ] {
        let mut secret = step("web", None, None);
        secret.command = command.into();
        let error = save(&mut store, &repo, &item, recipe(vec![secret])).unwrap_err();
        assert!(matches!(error, ArchonError::Invalid(_)), "{error:?}");
        assert!(!error.to_string().contains("hunter2"), "{error}");
    }
    assert!(save(&mut store, &repo, &item, recipe(vec![])).is_err());
    assert_eq!(load(&store, &repo).unwrap(), None);
    let key = archon_queue::store::keys::design_recipe(&repo);
    assert_eq!(
        store.kv_get(&key).unwrap(),
        None,
        "nothing reached the store"
    );
}

#[test]
fn a_corrupt_row_is_reported_not_ignored() {
    let mut store = Store::open_in_memory().unwrap();
    let repo = RepoId::new();
    let key = archon_queue::store::keys::design_recipe(&repo);
    store.kv_set(&key, "{not json").unwrap();
    assert!(matches!(load(&store, &repo), Err(ArchonError::Corrupt(_))));
}

#[test]
fn a_failed_revision_bump_saves_and_clears_nothing() {
    let mut store = Store::open_in_memory().unwrap();
    let repo = RepoId::new();
    let item = QueueItemId::new();
    let kept = save(
        &mut store,
        &repo,
        &item,
        recipe(vec![step("web", None, None)]),
    )
    .unwrap();
    let key = archon_queue::store::keys::design_recipe(&repo);
    let revision_key = archon_queue::store::keys::design_recipe_revision(&repo);
    let row = store.kv_get(&key).unwrap();
    for broken in ["not a number", &u64::MAX.to_string()] {
        store.kv_set(&revision_key, broken).unwrap();
        let replacement = recipe(vec![step("api", Some(8000), None)]);
        let saved = save(&mut store, &repo, &item, replacement);
        assert!(matches!(saved, Err(ArchonError::Corrupt(_))), "{saved:?}");
        let cleared = clear(&mut store, &repo);
        assert!(
            matches!(cleared, Err(ArchonError::Corrupt(_))),
            "{cleared:?}"
        );
        assert_eq!(
            store.kv_get(&key).unwrap(),
            row,
            "the recipe row is untouched"
        );
        assert_eq!(load(&store, &repo).unwrap(), Some(kept.clone()));
        let revision = store.kv_get(&revision_key).unwrap();
        assert_eq!(
            revision.as_deref(),
            Some(broken),
            "the revision is untouched"
        );
    }
}

#[test]
fn every_save_and_clear_moves_the_revision_once() {
    let mut store = Store::open_in_memory().unwrap();
    let repo = RepoId::new();
    let item = QueueItemId::new();
    assert_eq!(revision(&store, &repo).unwrap(), 0);
    assert!(!clear(&mut store, &repo).unwrap(), "nothing to clear");
    assert_eq!(revision(&store, &repo).unwrap(), 1, "an empty clear counts");
    save(
        &mut store,
        &repo,
        &item,
        recipe(vec![step("web", None, None)]),
    )
    .unwrap();
    assert_eq!(revision(&store, &repo).unwrap(), 2);
    assert!(clear(&mut store, &repo).unwrap());
    assert_eq!(revision(&store, &repo).unwrap(), 3);
}

/// Serves `status` to every connection on a loopback port.
async fn serving(status: u16) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            let mut request = [0u8; 1024];
            let _ = socket.read(&mut request).await;
            let answer = format!("HTTP/1.1 {status} X\r\nContent-Length: 0\r\n\r\n");
            let _ = socket.write_all(answer.as_bytes()).await;
        }
    });
    port
}

/// Answers every connection with `302 Found` to `location`.
async fn redirecting(location: String) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            let mut request = [0u8; 1024];
            let _ = socket.read(&mut request).await;
            let answer =
                format!("HTTP/1.1 302 Found\r\nLocation: {location}\r\nContent-Length: 0\r\n\r\n");
            let _ = socket.write_all(answer.as_bytes()).await;
        }
    });
    port
}

#[tokio::test]
async fn a_redirect_is_followed_on_loopback_to_its_final_status() {
    tokio::time::timeout(Duration::from_secs(10), async {
        let broken = serving(500).await;
        let fine = serving(200).await;
        let to_broken = redirecting(format!("http://127.0.0.1:{broken}/app/")).await;
        let error = check_stack(&format!("http://127.0.0.1:{to_broken}/"), None)
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("answered HTTP 500"), "{error}");
        let to_fine = redirecting(format!("//127.0.0.1:{fine}/login")).await;
        check_stack(&format!("http://127.0.0.1:{to_fine}/"), None)
            .await
            .unwrap();
        let away = redirecting("https://example.com/".into()).await;
        let error = check_stack(&format!("http://127.0.0.1:{away}/"), None)
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("off loopback"), "{error}");
        let itself = redirecting("next".into()).await;
        let error = check_stack(&format!("http://127.0.0.1:{itself}/a/b"), None)
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("redirects more than 5 times"), "{error}");
    })
    .await
    .expect("bounded stack check");
}

#[test]
fn a_relative_location_resolves_against_the_page() {
    let base = "http://127.0.0.1:5173/app/page?x=1";
    assert_eq!(
        redirect_target(base, "/login"),
        "http://127.0.0.1:5173/login"
    );
    assert_eq!(
        redirect_target(base, "next"),
        "http://127.0.0.1:5173/app/next"
    );
    assert_eq!(redirect_target(base, "//[::1]:80/"), "http://[::1]:80/");
    assert_eq!(
        redirect_target(base, "http://localhost:1/"),
        "http://localhost:1/"
    );
    let same_page = "http://127.0.0.1:5173/app/page?x=1";
    assert_eq!(
        redirect_target(base, "?ready=1"),
        "http://127.0.0.1:5173/app/page?ready=1"
    );
    assert_eq!(redirect_target(base, "#top"), same_page);
    assert_eq!(redirect_target(base, ""), same_page);
}

/// Writes `answer` to every connection, then closes it.
async fn answering(answer: Vec<u8>) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            let mut request = [0u8; 1024];
            let _ = socket.read(&mut request).await;
            let _ = socket.write_all(&answer).await;
        }
    });
    port
}

#[tokio::test]
async fn only_a_whole_final_answer_counts() {
    tokio::time::timeout(Duration::from_secs(10), async {
        let check = |port: u16| async move {
            check_stack(&format!("http://127.0.0.1:{port}/"), None)
                .await
                .map_err(|e| e.to_string())
        };
        let hints = b"HTTP/1.1 103 Early Hints\r\nLink: </a.css>\r\n\r\nHTTP/1.1 500 X\r\n\r\n";
        let error = check(answering(hints.to_vec()).await).await.unwrap_err();
        assert!(error.contains("answered HTTP 500"), "{error}");
        let hints_then_ok = b"HTTP/1.1 100 Continue\r\n\r\nHTTP/1.1 200 OK\r\n\r\n";
        check(answering(hints_then_ok.to_vec()).await)
            .await
            .unwrap();
        let cut_off = b"HTTP/1.1 302 Found\r\n".to_vec();
        let error = check(answering(cut_off).await).await.unwrap_err();
        assert!(error.contains("before its answer's head ended"), "{error}");
        let mut oversized = b"HTTP/1.1 302 Found\r\n".to_vec();
        oversized.extend(format!("X-Pad: {}\r\n", "a".repeat(MAX_HEAD)).bytes());
        oversized.extend(b"Location: /broken\r\n\r\n");
        let error = check(answering(oversized).await).await.unwrap_err();
        assert!(error.contains("response head over"), "{error}");
    })
    .await
    .expect("bounded stack check");
}

/// A loopback port nothing listens on.
async fn closed_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    listener.local_addr().unwrap().port()
}

#[tokio::test]
async fn the_stack_is_up_when_page_and_readiness_checks_answer() {
    tokio::time::timeout(Duration::from_secs(10), async {
        let page = serving(200).await;
        let api = serving(404).await;
        let db = serving(200).await;
        let url = format!("http://127.0.0.1:{page}/fast-check/?x=1#top");
        check_stack(&url, None).await.unwrap();
        let ready = Some(format!("http://localhost:{api}/health"));
        let r = recipe(vec![step("api", None, ready), step("db", Some(db), None)]);
        check_stack(&url, Some(&r)).await.unwrap();
    })
    .await
    .expect("bounded stack check");
}

#[tokio::test]
async fn a_half_started_stack_names_every_failure() {
    tokio::time::timeout(Duration::from_secs(10), async {
        let page = serving(502).await;
        let down = closed_port().await;
        let url = format!("http://127.0.0.1:{page}/");
        let ready = Some(format!("http://127.0.0.1:{down}/health"));
        let r = recipe(vec![
            step("api", None, ready),
            step("db", Some(down), None),
            step("web", None, None),
        ]);
        let error = check_stack(&url, Some(&r)).await.unwrap_err().to_string();
        assert!(
            error.contains("answered HTTP 502, a server error"),
            "{error}"
        );
        assert!(error.contains("step api: http://127.0.0.1:"), "{error}");
        assert!(error.contains(&format!("step db: nothing listens on loopback port {down}")));
        assert!(
            !error.contains("step web"),
            "a step without checks is not probed"
        );
    })
    .await
    .expect("bounded stack check");
}

#[tokio::test]
async fn a_server_that_is_not_http_is_refused() {
    tokio::time::timeout(Duration::from_secs(10), async {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let _ = socket.write_all(b"SSH-2.0-OpenSSH\r\n").await;
            }
        });
        let error = check_stack(&format!("http://127.0.0.1:{port}/"), None)
            .await
            .unwrap_err();
        assert!(
            error.to_string().contains("did not answer with HTTP"),
            "{error}"
        );
    })
    .await
    .expect("bounded stack check");
}

/// S1: the port a script picked for one run is not what the next one loads.
#[test]
fn a_saved_recipe_keeps_how_a_port_is_picked_not_its_value() {
    let mut store = Store::open_in_memory().unwrap();
    let repo = RepoId::new();
    let mut web = step(
        "web",
        Some(36519),
        Some("http://127.0.0.1:36519/fast-check/".into()),
    );
    web.port_pick = Some(archon_core::design::recipe::PortPick::Free);
    save(&mut store, &repo, &QueueItemId::new(), recipe(vec![web])).unwrap();
    let loaded = load(&store, &repo).unwrap().expect("saved");
    let step = &loaded.recipe.steps[0];
    assert_eq!((step.port, step.ready.as_deref()), (None, None));
    assert_eq!(
        step.port_pick,
        Some(archon_core::design::recipe::PortPick::Free)
    );
}
