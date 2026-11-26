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
    }
}

#[test]
fn a_recipe_is_saved_read_replaced_and_cleared_per_repository() {
    let mut store = Store::open_in_memory().unwrap();
    let (repo, other) = (RepoId::new(), RepoId::new());
    let item = QueueItemId::new();
    assert_eq!(load(&store, &repo).unwrap(), None);
    let first = save(&mut store, &repo, &item, recipe(vec![step("web", None, None)])).unwrap();
    assert_eq!(load(&store, &repo).unwrap(), Some(first.clone()));
    assert_eq!(load(&store, &other).unwrap(), None, "kept per repository");
    let updated = recipe(vec![step("db", Some(5432), None), step("web", None, None)]);
    let second = save(&mut store, &repo, &item, updated.clone()).unwrap();
    assert_eq!(load(&store, &repo).unwrap().unwrap().recipe, updated);
    assert_ne!(first.recipe, second.recipe);
    assert!(clear(&mut store, &repo).unwrap());
    assert_eq!(load(&store, &repo).unwrap(), None);
    assert!(!clear(&mut store, &repo).unwrap(), "clearing twice is harmless");
}

#[test]
fn an_invalid_or_secret_recipe_is_never_stored() {
    let mut store = Store::open_in_memory().unwrap();
    let repo = RepoId::new();
    let item = QueueItemId::new();
    let mut secret = step("web", None, None);
    secret.command = "DB_PASSWORD=hunter2hunter2 npm start".into();
    let error = save(&mut store, &repo, &item, recipe(vec![secret])).unwrap_err();
    assert!(matches!(error, ArchonError::Invalid(_)), "{error:?}");
    assert!(!error.to_string().contains("hunter2"), "{error}");
    assert!(save(&mut store, &repo, &item, recipe(vec![])).is_err());
    assert_eq!(load(&store, &repo).unwrap(), None);
    let key = archon_queue::store::keys::design_recipe(&repo);
    assert_eq!(store.kv_get(&key).unwrap(), None, "nothing reached the store");
}

#[test]
fn a_corrupt_row_is_reported_not_ignored() {
    let mut store = Store::open_in_memory().unwrap();
    let repo = RepoId::new();
    let key = archon_queue::store::keys::design_recipe(&repo);
    store.kv_set(&key, "{not json").unwrap();
    assert!(matches!(load(&store, &repo), Err(ArchonError::Corrupt(_))));
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
        assert!(error.contains("answered HTTP 502, a server error"), "{error}");
        assert!(error.contains("step api: http://127.0.0.1:"), "{error}");
        assert!(error.contains(&format!("step db: nothing listens on loopback port {down}")));
        assert!(!error.contains("step web"), "a step without checks is not probed");
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
        assert!(error.to_string().contains("did not answer with HTTP"), "{error}");
    })
    .await
    .expect("bounded stack check");
}
