//! Short item ids and `archon log` filters through the built binary, against
//! a fake archond holding a few items and a feed longer than one page.

mod common;

use std::io::{BufRead as _, BufReader, Write as _};
use std::os::unix::net::{UnixListener, UnixStream};
use std::process::Stdio;
use std::sync::{Arc, Mutex};

use archon_core::id::{FeedItemId, NotificationId, QueueItemId, queue_item_prefix};
use archon_core::notify::{FeedEntry, FeedItem, Notification, NotificationKind};
use archon_core::protocol::{
    ClientFrame, ErrorCode, ErrorReply, ItemDetail, PROTOCOL_VERSION, Reply, Request, ServerFrame,
    Welcome,
};
use archon_core::queue::{QueueItem, QueueState};
use common::Env;

const FIRST: &str = "qi_01K00000000000000000000001";
const SECOND: &str = "qi_01K00000000000000000000002";
const THIRD: &str = "qi_01M00000000000000000000003";
/// Longer than one `FeedHistory` page, so a sparse filter pages back.
const FEED_LEN: u64 = 1_500;
/// The seqs of the third item's outcome notices; every other entry is noise.
const OUTCOMES: [u64; 2] = [3, 1_400];
/// Entries a subscription delivers after the history, before the fake
/// closes it; the third item's outcome notices among them.
const LATER: std::ops::RangeInclusive<u64> = FEED_LEN + 1..=FEED_LEN + 4;
const LATER_OUTCOMES: [u64; 1] = [FEED_LEN + 3];

struct Fake {
    items: Vec<QueueItem>,
    feed: Vec<FeedItem>,
    /// The `after` of every `SubscribeFeed` received.
    subscribed_after: Mutex<Vec<Option<u64>>>,
}

fn item(id: &str, title: &str) -> QueueItem {
    let now = archon_core::now();
    QueueItem {
        onto: None,
        id: QueueItemId::parse(id).unwrap(),
        repo: None,
        pipeline: "ask".into(),
        title: title.into(),
        brief: "the brief".into(),
        priority: archon_core::queue::Priority::Normal,
        dependencies: vec![],
        tags: vec![],
        model_plan: archon_core::model::ModelPlan::default(),
        model_reasons: None,
        budget_usd: None,
        autonomy: archon_core::autonomy::Autonomy::default(),
        origin: None,
        stage_timeouts: Default::default(),
        fresh_retries: 0,
        routed_provider: None,
        provider_switch: None,
        initiative: None,
        risk: None,
        state: QueueState::Queued,
        created_at: now,
        updated_at: now,
    }
}

fn entry(seq: u64, third: &QueueItemId) -> FeedItem {
    let (kind, item) = match seq {
        s if OUTCOMES.contains(&s) || LATER_OUTCOMES.contains(&s) => {
            (NotificationKind::Outcome, Some(third.clone()))
        }
        s if s % 2 == 0 => (NotificationKind::Progress, Some(third.clone())),
        _ => (NotificationKind::Outcome, None),
    };
    FeedItem {
        id: FeedItemId::new(),
        seq,
        at: archon_core::now(),
        entry: FeedEntry::Notification(Notification {
            id: NotificationId::new(),
            kind,
            text: format!("entry {seq}"),
            item,
            run: None,
            at: archon_core::now(),
        }),
    }
}

impl Fake {
    fn new() -> Self {
        let items = vec![
            item(FIRST, "one"),
            item(SECOND, "two"),
            item(THIRD, "three"),
        ];
        let third = items[2].id.clone();
        let feed = (1..=FEED_LEN).map(|seq| entry(seq, &third)).collect();
        Self {
            items,
            feed,
            subscribed_after: Mutex::new(Vec::new()),
        }
    }

    fn resolve(&self, text: &str) -> Result<Reply, ErrorReply> {
        let refused = |code, message: String| ErrorReply { code, message };
        let prefix = queue_item_prefix(text)
            .map_err(|e| refused(ErrorCode::InvalidArgument, e.to_string()))?;
        let found: Vec<&QueueItem> = self
            .items
            .iter()
            .filter(|i| i.id.as_str().starts_with(&prefix))
            .collect();
        match found[..] {
            [] => Err(refused(ErrorCode::NotFound, format!("{prefix} not found"))),
            [one] => Ok(Reply::Item(one.clone())),
            _ => Err(refused(
                ErrorCode::InvalidArgument,
                format!("invalid argument: {prefix} is ambiguous; it starts {FIRST}, {SECOND}"),
            )),
        }
    }

    fn history(&self, before: Option<u64>, limit: usize) -> Reply {
        let limit = limit.min(1_000);
        let older: Vec<&FeedItem> = self
            .feed
            .iter()
            .filter(|e| before.is_none_or(|b| e.seq < b))
            .collect();
        let page = older[older.len().saturating_sub(limit)..].iter();
        Reply::Feed(page.map(|e| (*e).clone()).collect())
    }

    fn reply(&self, request: &Request) -> Result<Reply, ErrorReply> {
        Ok(match request {
            Request::Hello { .. } => Reply::Welcome(Welcome {
                protocol_version: PROTOCOL_VERSION,
                daemon_version: "fake".into(),
                pid: std::process::id(),
            }),
            Request::ResolveItem { prefix } => return self.resolve(prefix),
            Request::GetItem { item } => Reply::ItemDetail(Box::new(ItemDetail {
                default_efforts: Default::default(),
                steers: Vec::new(),
                waiting_on: None,
                waiting_reason: None,
                item: self.items.iter().find(|i| &i.id == item).unwrap().clone(),
                runs: vec![],
                stages: vec![],
                spent_usd: 0.0,
                gates: vec![],
                delegations: Default::default(),
                switched_to: None,
                merges: Vec::new(),
            })),
            Request::RepoList => Reply::Repos(vec![]),
            Request::ListQuestions { .. } => Reply::Questions(vec![]),
            Request::FeedHistory { before, limit, .. } => self.history(*before, *limit),
            other => {
                return Err(ErrorReply {
                    code: ErrorCode::Internal,
                    message: format!("the fake archond does not serve {other:?}"),
                });
            }
        })
    }

    /// Answers a subscription, sends the `LATER` entries, then hangs up.
    fn subscribe(&self, id: u64, after: Option<u64>, writer: &mut UnixStream) {
        self.subscribed_after.lock().unwrap().push(after);
        let third = self.items[2].id.clone();
        let reply = ServerFrame::Reply {
            id,
            result: Ok(Reply::Subscribed {
                id: 1,
                last_seq: FEED_LEN,
            }),
        };
        let frames = LATER.map(|seq| ServerFrame::Feed {
            sub: 1,
            item: entry(seq, &third),
        });
        for frame in std::iter::once(reply).chain(frames) {
            writeln!(writer, "{}", serde_json::to_string(&frame).unwrap()).unwrap();
        }
        writer.shutdown(std::net::Shutdown::Both).unwrap();
    }

    fn serve(&self, stream: UnixStream) {
        let mut writer = stream.try_clone().unwrap();
        for line in BufReader::new(stream).lines() {
            let Ok(line) = line else { return };
            let frame: ClientFrame = serde_json::from_str(&line).unwrap();
            if let Request::SubscribeFeed { after, .. } = frame.request {
                return self.subscribe(frame.id, after, &mut writer);
            }
            let reply = ServerFrame::Reply {
                id: frame.id,
                result: self.reply(&frame.request),
            };
            let text = serde_json::to_string(&reply).unwrap();
            if writeln!(writer, "{text}").is_err() {
                return;
            }
        }
    }
}

fn start_fake(env: &Env) -> Arc<Fake> {
    let listener = UnixListener::bind(env.path("archond.sock")).unwrap();
    let fake = Arc::new(Fake::new());
    let served = fake.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let fake = served.clone();
            std::thread::spawn(move || fake.serve(stream.unwrap()));
        }
    });
    fake
}

/// `archon <args>` that must fail; its stderr.
fn refusal(env: &Env, args: &[&str]) -> String {
    let out = env.command(args).output().unwrap();
    assert_eq!(out.status.code(), Some(1), "archon {args:?}: {out:?}");
    String::from_utf8(out.stderr).unwrap()
}

#[test]
fn a_short_id_names_one_item_and_a_bad_one_is_explained() {
    let env = Env::new();
    start_fake(&env);
    let whole_lower = THIRD.to_ascii_lowercase();
    for text in ["01m", "qi_01M", "01M00000000000000000000003", &whole_lower] {
        let out = String::from_utf8(env.run(&["queue", "show", text]).stdout).unwrap();
        assert!(
            out.starts_with(&format!("{THIRD}  three\n")),
            "{text}: {out}"
        );
    }
    let ambiguous = refusal(&env, &["queue", "show", "01k"]);
    assert!(ambiguous.contains("qi_01K is ambiguous"), "{ambiguous}");
    assert!(
        ambiguous.contains(FIRST) && ambiguous.contains(SECOND),
        "{ambiguous}"
    );
    let unknown = refusal(&env, &["queue", "show", "7z"]);
    assert!(unknown.contains("qi_7Z not found"), "{unknown}");
    let malformed = refusal(&env, &["log", "--item", "01-k"]);
    assert!(
        malformed.contains("is not a queue item id or its start"),
        "{malformed}"
    );
}

fn seqs(value: &serde_json::Value) -> Vec<u64> {
    let entries = value.as_array().expect("a JSON array of entries");
    entries.iter().map(|e| e["seq"].as_u64().unwrap()).collect()
}

#[test]
fn log_narrows_to_an_item_and_kinds_across_pages() {
    let env = Env::new();
    start_fake(&env);
    let json = |args: &[&str]| -> serde_json::Value {
        let out = env.run(&[&["--json", "log"], args].concat());
        serde_json::from_slice(&out.stdout).unwrap()
    };
    assert_eq!(seqs(&json(&["--limit", "3"])), [1_498, 1_499, 1_500]);
    let outcomes = json(&["--item", "01m", "--kind", "outcome"]);
    assert_eq!(seqs(&outcomes), OUTCOMES, "both pages are searched");
    assert_eq!(
        seqs(&json(&[
            "--item", "01m", "--kind", "outcome", "--limit", "1"
        ])),
        [1_400]
    );
    let progress = json(&["--item", "01m", "--kind", "progress", "--limit", "2"]);
    assert_eq!(seqs(&progress), [1_498, 1_500]);
    let text = String::from_utf8(
        env.run(&["log", "--item", "01m", "--kind", "outcome"])
            .stdout,
    )
    .unwrap();
    let lines: Vec<&str> = text.lines().filter(|l| l.contains("entry ")).collect();
    assert_eq!(lines.len(), 2, "{text}");
    assert!(
        lines[0].contains("entry 3") && lines[1].contains("entry 1400"),
        "{text}"
    );
}

#[test]
fn log_follow_continues_after_the_history_through_the_filter() {
    let env = Env::new();
    let fake = start_fake(&env);
    let args = [
        "--json", "log", "--item", "01m", "--kind", "outcome", "--follow",
    ];
    let out = env.command(&args).output().unwrap();
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert_eq!(out.status.code(), Some(1), "the fake hangs up: {stderr}");
    assert!(stderr.is_empty(), "{stderr}");
    assert_eq!(*fake.subscribed_after.lock().unwrap(), [Some(FEED_LEN)]);
    let lines = String::from_utf8(out.stdout).unwrap();
    let mut values: Vec<serde_json::Value> = lines
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let error = values.pop().unwrap();
    assert!(
        error["error"]["message"]
            .as_str()
            .unwrap()
            .contains("the feed stopped")
    );
    let printed: Vec<u64> = values
        .iter()
        .map(|value| value["seq"].as_u64().unwrap())
        .collect();
    assert_eq!(printed, [OUTCOMES[0], OUTCOMES[1], LATER_OUTCOMES[0]]);
}

/// `archon log --follow | head`: once the reader closes the pipe, archon
/// stops and exits cleanly instead of panicking.
#[test]
fn log_follow_ends_cleanly_when_its_reader_goes_away() {
    let env = Env::new();
    let fake = start_fake(&env);
    for json in [true, false] {
        let args = ["log", "--item", "01m", "--kind", "outcome", "--follow"];
        let args = match json {
            true => [&["--json"][..], &args].concat(),
            false => args.to_vec(),
        };
        let mut child = env
            .command(&args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        drop(child.stdout.take());
        let out = child.wait_with_output().unwrap();
        let stderr = String::from_utf8(out.stderr).unwrap();
        assert_eq!(out.status.code(), Some(0), "json {json}: {stderr}");
        assert!(!stderr.contains("panicked"), "{stderr}");
    }
    let subscribed = fake.subscribed_after.lock().unwrap();
    assert!(subscribed.is_empty(), "it stopped at its first print");
}

/// `archon --json log | head`: the history, printed as one JSON array, ends
/// as cleanly when the reader has closed the pipe.
#[test]
fn log_json_ends_cleanly_when_its_reader_goes_away() {
    let env = Env::new();
    let _fake = start_fake(&env);
    let args = ["--json", "log", "--item", "01m", "--kind", "outcome"];
    let mut child = env
        .command(&args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    drop(child.stdout.take());
    let out = child.wait_with_output().unwrap();
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert_eq!(out.status.code(), Some(0), "{stderr}");
    assert!(!stderr.contains("panicked"), "{stderr}");
}
