//! `archon initiatives` through the built binary, against a fake archond
//! that records what it was asked: an empty list says how to start one,
//! create and update read their files, and ids may be given by their start.

mod common;

use std::io::{BufRead as _, BufReader, Write as _};
use std::os::unix::net::{UnixListener, UnixStream};
use std::process::Stdio;
use std::sync::{Arc, Mutex};

use archon_core::id::InitiativeId;
use archon_core::initiative::{ContractChange, Initiative, InitiativeDetail, InitiativeState};
use archon_core::protocol::{
    ClientFrame, ErrorCode, ErrorReply, PROTOCOL_VERSION, Reply, Request, ServerFrame, Welcome,
};
use common::Env;

const FIRST: &str = "ini_01K00000000000000000000001";
const SECOND: &str = "ini_01M00000000000000000000002";

#[derive(Clone)]
struct Fake {
    initiatives: Vec<Initiative>,
    received: Arc<Mutex<Vec<Request>>>,
}

fn initiative(id: &str, title: &str) -> Initiative {
    let now = archon_core::now();
    Initiative {
        id: InitiativeId::parse(id).unwrap(),
        title: title.into(),
        contract: "Every API uses account_id.".into(),
        atomic: false,
        state: InitiativeState::Open,
        created_at: now,
        updated_at: now,
    }
}

fn detail(initiative: Initiative) -> Reply {
    Reply::Initiative(Box::new(InitiativeDetail {
        initiative,
        members: vec![],
    }))
}

impl Fake {
    fn start(env: &Env, initiatives: Vec<Initiative>) -> Self {
        let listener = UnixListener::bind(env.path("archond.sock")).unwrap();
        let fake = Self {
            initiatives,
            received: Arc::default(),
        };
        let serving = fake.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                serving.serve(stream.unwrap());
            }
        });
        fake
    }

    fn named(&self, id: &InitiativeId) -> Result<Reply, ErrorReply> {
        let found = self.initiatives.iter().find(|i| &i.id == id);
        found.cloned().map(detail).ok_or_else(|| ErrorReply {
            code: ErrorCode::NotFound,
            message: format!("initiative {id}"),
        })
    }

    fn reply(&self, request: &Request) -> Result<Reply, ErrorReply> {
        match request {
            Request::Hello { .. } => Ok(Reply::Welcome(Welcome {
                protocol_version: PROTOCOL_VERSION,
                daemon_version: "fake".into(),
                pid: std::process::id(),
            })),
            Request::ListInitiatives => Ok(Reply::Initiatives(self.initiatives.clone())),
            Request::EnqueueInitiative(asked) => {
                let made = Initiative::new(
                    &asked.title,
                    &asked.contract,
                    asked.atomic,
                    archon_core::now(),
                );
                Ok(detail(made.unwrap()))
            }
            Request::GetInitiative { initiative }
            | Request::UpdateInitiative { initiative, .. }
            | Request::CancelInitiative { initiative }
            | Request::RetryInitiative { initiative } => self.named(initiative),
            other => Err(ErrorReply {
                code: ErrorCode::Internal,
                message: format!("the fake archond does not serve {other:?}"),
            }),
        }
    }

    fn serve(&self, stream: UnixStream) {
        let mut writer = stream.try_clone().unwrap();
        for line in BufReader::new(stream).lines() {
            let Ok(line) = line else { return };
            let frame: ClientFrame = serde_json::from_str(&line).unwrap();
            if !matches!(frame.request, Request::Hello { .. }) {
                self.received.lock().unwrap().push(frame.request.clone());
            }
            let reply = ServerFrame::Reply {
                id: frame.id,
                result: self.reply(&frame.request),
            };
            if writeln!(writer, "{}", serde_json::to_string(&reply).unwrap()).is_err() {
                return;
            }
        }
    }

    /// The last request other than a lookup of the list.
    fn last_change(&self) -> Request {
        let received = self.received.lock().unwrap();
        let mut changes = received
            .iter()
            .filter(|r| !matches!(r, Request::ListInitiatives));
        changes.next_back().cloned().expect("a request was made")
    }
}

fn stdout(out: &std::process::Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
fn an_empty_list_says_how_to_start_one() {
    let env = Env::new();
    Fake::start(&env, vec![]);
    let out = env.run(&["initiatives"]);
    assert_eq!(
        stdout(&out),
        "No initiatives. Start one with `archon initiatives create <file>`.\n"
    );
    let listed: serde_json::Value =
        serde_json::from_slice(&env.run(&["--json", "initiatives"]).stdout).unwrap();
    assert_eq!(listed, serde_json::json!([]), "JSON stays a plain list");
}

#[test]
fn create_reads_its_request_from_a_file() {
    let env = Env::new();
    let fake = Fake::start(&env, vec![]);
    let file = env.path("rename.json");
    let request = serde_json::json!({
        "title": "Rename account field",
        "contract": "Every API uses account_id.",
        "atomic": true,
        "members": [{"repo": "api", "brief": "Rename the field."}],
        "idempotency_key": "account-v2",
    });
    std::fs::write(&file, request.to_string()).unwrap();
    let out = env.run(&["initiatives", "create", file.to_str().unwrap()]);
    assert!(
        stdout(&out).contains("[open] Rename account field"),
        "{out:?}"
    );
    let Request::EnqueueInitiative(sent) = fake.last_change() else {
        panic!("expected an enqueue");
    };
    assert_eq!(sent.title, "Rename account field");
    assert!(sent.atomic);
    assert_eq!(sent.members.len(), 1);
    assert_eq!(sent.members[0].repo.0, "api");
    assert_eq!(sent.idempotency_key.as_deref(), Some("account-v2"));
}

#[test]
fn ids_may_be_given_by_their_start_and_update_appends_or_replaces() {
    let env = Env::new();
    let fake = Fake::start(
        &env,
        vec![initiative(FIRST, "first"), initiative(SECOND, "second")],
    );
    let shown = stdout(&env.run(&["initiatives", "show", "01k"]));
    assert!(
        shown.starts_with(&format!("{FIRST} [open] first\n")),
        "{shown}"
    );

    let mut update = env
        .command(&["initiatives", "update", "INI_01M", "--append-file", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = update.stdin.take().unwrap();
    stdin.write_all(b"Also rename the CLI flag.").unwrap();
    drop(stdin);
    assert!(update.wait_with_output().unwrap().status.success());
    let Request::UpdateInitiative { initiative, change } = fake.last_change() else {
        panic!("expected an update");
    };
    assert_eq!(initiative.as_str(), SECOND);
    assert_eq!(
        change,
        ContractChange::Append("Also rename the CLI flag.".into())
    );

    let revised = env.path("revised.md");
    std::fs::write(&revised, "Every API uses customer_id.").unwrap();
    let argv = ["initiatives", "update", FIRST, "--contract-file"];
    env.run(&[&argv[..], &[revised.to_str().unwrap()]].concat());
    let Request::UpdateInitiative { change, .. } = fake.last_change() else {
        panic!("expected an update");
    };
    assert_eq!(
        change,
        ContractChange::Replace("Every API uses customer_id.".into())
    );
    for (argv, says) in [
        (&["initiatives", "update", FIRST][..], "--append-file"),
        (
            &[
                "initiatives",
                "update",
                FIRST,
                "--append-file",
                "-",
                "--contract-file",
                "-",
            ][..],
            "cannot be used with",
        ),
    ] {
        let out = env.command(argv).output().unwrap();
        assert_eq!(out.status.code(), Some(2), "{out:?}");
        let error = String::from_utf8_lossy(&out.stderr);
        assert!(error.contains(says), "{error}");
    }

    for (start, code, says) in [
        ("01", "invalid_argument", "ambiguous"),
        ("01Z", "not_found", "initiative starting with ini_01Z"),
    ] {
        let argv = ["--json", "initiatives", "cancel", start];
        let out = env.command(&argv).output().unwrap();
        assert_eq!(out.status.code(), Some(1), "{out:?}");
        let error: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(error["error"]["code"], code, "{error}");
        let message = error["error"]["message"].as_str().unwrap();
        assert!(message.contains(says), "{message}");
    }
}
