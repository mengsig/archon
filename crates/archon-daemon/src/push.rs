//! Push notifications (`[notify]`): an event the operator must act on runs
//! the configured command, or POSTs to the webhook, with the event as JSON.
//! Events carry an id, a title and one line Archon writes, never brief text
//! or secrets. Delivery runs detached: a failing notifier is logged and
//! never touches the item that caused it.

use std::collections::HashMap;
use std::process::Stdio;
use std::time::{Duration, Instant};

use archon_core::Timestamp;
use archon_core::id::QueueItemId;
use archon_core::notify::Question;
use archon_core::push::{NotifyConfig, PushKind};
use archon_core::queue::{QueueItem, QueueState};
use archon_core::run::GateSubject;
use serde::Serialize;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use crate::context::DaemonContext;
use crate::executor::clip;

/// A notifier that has not finished by then is killed.
const DELIVERY_TIMEOUT: Duration = Duration::from_secs(30);

/// Titles are clipped to this; one line is Archon's own and short.
const MAX_TITLE_CHARS: usize = 200;

/// The JSON a notifier receives on stdin or as the webhook body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PushEvent {
    pub kind: PushKind,
    /// The item it concerns; a question about no item names the question,
    /// a budget hold `budget`.
    pub id: String,
    pub title: String,
    pub line: String,
    pub at: Timestamp,
}

impl PushEvent {
    fn new(kind: PushKind, id: impl Into<String>, title: &str, line: String) -> Self {
        let id = id.into();
        assert!(!id.is_empty(), "a push event names what it is about");
        assert!(!line.trim().is_empty(), "a push event says something");
        Self {
            kind,
            id,
            title: clip(title, MAX_TITLE_CHARS),
            line,
            at: archon_core::now(),
        }
    }
}

/// When each kind was last sent, for `[notify] min_gap_secs`.
#[derive(Debug, Default)]
pub struct Pusher {
    last: std::sync::Mutex<HashMap<PushKind, Instant>>,
}

impl Pusher {
    /// Whether an event of `kind` may go out at `now`; records it if so.
    fn admit(&self, kind: PushKind, gap: Duration, now: Instant) -> bool {
        let mut last = self
            .last
            .lock()
            .expect("the push rate table is never poisoned");
        if let Some(at) = last.get(&kind)
            && now.saturating_duration_since(*at) < gap
        {
            return false;
        }
        last.insert(kind, now);
        assert!(last.contains_key(&kind), "an admitted kind is recorded");
        true
    }
}

/// Send `event` when `[notify]` asks for its kind and its rate allows.
pub fn send(ctx: &DaemonContext, event: PushEvent) {
    let config = &ctx.config.notify;
    if !config.sends(event.kind) {
        return;
    }
    let kind = event.kind.as_str();
    let gap = Duration::from_secs(config.min_gap_secs);
    if !ctx.push.admit(event.kind, gap, Instant::now()) {
        tracing::info!(kind, id = %event.id, "push notification skipped: sent one of this kind lately");
        return;
    }
    let delivery = Delivery::of(config, &event);
    let id = event.id;
    tokio::spawn(async move {
        match delivery.run().await {
            Ok(()) => tracing::debug!(kind, %id, "push notification sent"),
            Err(error) => tracing::warn!(kind, %id, %error, "push notification failed"),
        }
    });
}

/// A gate was asked as question `number`.
pub fn gate(ctx: &DaemonContext, item: &QueueItem, subject: GateSubject, number: u64) {
    assert!(number > 0, "a gate's question is recorded first");
    let line = format!("Q{number}: the {} waits for your decision", subject.noun());
    send(
        ctx,
        PushEvent::new(PushKind::Gate, item.id.as_str(), &item.title, line),
    );
}

/// A question other than a gate's waits for the operator.
pub async fn question(ctx: &DaemonContext, question: &Question) {
    assert!(question.number > 0, "a recorded question is numbered");
    if !ctx.config.notify.sends(PushKind::Question) {
        return;
    }
    let line = format!("Q{} waits for your answer", question.number);
    let event = match &question.item {
        Some(item) => {
            let title = item_title(ctx, item).await;
            PushEvent::new(PushKind::Question, item.as_str(), &title, line)
        }
        None => PushEvent::new(PushKind::Question, question.id.as_str(), "Archon", line),
    };
    send(ctx, event);
}

/// A finished item that failed or needs the operator.
pub fn finished(ctx: &DaemonContext, item: &QueueItem, needs_operator: bool) {
    assert!(
        item.state.is_terminal(),
        "only finished items are announced"
    );
    let (kind, line) = match &item.state {
        QueueState::Failed { .. } => (PushKind::Failure, "failed"),
        QueueState::Blocked { .. } => (PushKind::Blocked, "is blocked and needs you"),
        _ if needs_operator => (PushKind::Blocked, "ended needing you"),
        _ => return,
    };
    let line = format!("{} {line}", item.id);
    send(
        ctx,
        PushEvent::new(kind, item.id.as_str(), &item.title, line),
    );
}

/// A dependency failure blocked `item` before it ran.
pub fn blocked_by_dependency(ctx: &DaemonContext, item: &QueueItem) {
    assert!(
        matches!(item.state, QueueState::Blocked { .. }),
        "the item is blocked"
    );
    let line = format!(
        "{} is blocked: a dependency failed or was cancelled",
        item.id
    );
    send(
        ctx,
        PushEvent::new(PushKind::Blocked, item.id.as_str(), &item.title, line),
    );
}

/// A delivered pull request's checks ended `state` (`failed`, `not_started`).
pub async fn pr_checks(ctx: &DaemonContext, item: &QueueItemId, url: &str, state: &str) {
    assert!(!url.is_empty(), "checks belong to a pull request");
    if !ctx.config.notify.sends(PushKind::PrChecks) {
        return;
    }
    let ended = if state == "failed" {
        "failed"
    } else {
        "never started"
    };
    let line = format!("the checks of {url} {ended}");
    let title = item_title(ctx, item).await;
    send(
        ctx,
        PushEvent::new(PushKind::PrChecks, item.as_str(), &title, line),
    );
}

/// A budget hold stops new work.
pub fn budget(ctx: &DaemonContext, scope: &str, limit: &str) {
    assert!(matches!(scope, "daily" | "total"), "a known budget scope");
    let line = format!("the {scope} budget of {limit} is reached; no new work starts");
    send(
        ctx,
        PushEvent::new(PushKind::Budget, "budget", "Archon", line),
    );
}

/// The item's title, or its id when the store cannot say (the push is a
/// courtesy; its failure must not fail the caller).
async fn item_title(ctx: &DaemonContext, id: &QueueItemId) -> String {
    match ctx.store.lock().await.item(id) {
        Ok(item) => item.title,
        Err(error) => {
            tracing::warn!(%id, %error, "push notification without the item's title");
            id.to_string()
        }
    }
}

/// One notifier run: a program, its arguments and what it reads on stdin.
#[derive(Debug)]
struct Delivery {
    program: String,
    args: Vec<String>,
    stdin: Vec<u8>,
    /// Off for curl, whose diagnostics may quote the webhook URL and its token.
    log_stderr: bool,
}

impl Delivery {
    fn of(config: &NotifyConfig, event: &PushEvent) -> Self {
        let json = serde_json::to_string(event).expect("a push event serializes");
        if let Some((program, args)) = config.command.split_first() {
            return Self {
                program: program.clone(),
                args: args.to_vec(),
                stdin: json.into_bytes(),
                log_stderr: true,
            };
        }
        let url = config.webhook.as_deref().expect("a sink is configured");
        // The URL goes on stdin, not argv, so no `ps` shows a secret token.
        Self {
            program: "curl".into(),
            args: vec!["-q".into(), "--config".into(), "-".into()],
            stdin: curl_config(url, &json).into_bytes(),
            log_stderr: false,
        }
    }

    async fn run(self) -> Result<(), String> {
        let program = self.program.clone();
        match tokio::time::timeout(DELIVERY_TIMEOUT, self.run_untimed()).await {
            Ok(result) => result.map_err(|why| format!("{program}: {why}")),
            Err(_) => Err(format!("{program}: no exit within {DELIVERY_TIMEOUT:?}")),
        }
    }

    async fn run_untimed(self) -> Result<(), String> {
        let mut child = Command::new(&self.program)
            .args(&self.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| format!("cannot start: {e}"))?;
        let mut stdin = child.stdin.take().expect("stdin is piped");
        match stdin.write_all(&self.stdin).await {
            // A command that takes its message from argv need not read stdin.
            Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => {}
            Err(e) => return Err(format!("writing the event: {e}")),
            Ok(()) => {}
        }
        drop(stdin);
        let output = child
            .wait_with_output()
            .await
            .map_err(|e| format!("waiting: {e}"))?;
        if output.status.success() {
            return Ok(());
        }
        if !self.log_stderr {
            return Err(format!("{} (its stderr is not logged)", output.status));
        }
        let stderr = String::from_utf8_lossy(&output.stderr);
        Err(format!("{}: {}", output.status, clip(stderr.trim(), 500)))
    }
}

/// A `curl --config` that POSTs `json` to `url` and fails on HTTP errors.
fn curl_config(url: &str, json: &str) -> String {
    assert!(url.starts_with("http"), "the webhook is an http(s) URL");
    assert!(!json.contains('\n'), "compact JSON is one line");
    format!(
        "url = \"{}\"\nrequest = \"POST\"\nheader = \"Content-Type: application/json\"\n\
         data-binary = \"{}\"\nproto = \"=http,https\"\ngloboff\nfail\nsilent\nshow-error\nmax-time = 20\n",
        curl_quote(url),
        curl_quote(json)
    )
}

/// Inside a quoted `curl --config` value, `\` and `"` are escaped.
fn curl_quote(text: &str) -> String {
    text.replace('\\', "\\\\").replace('"', "\\\"")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_kind_is_rate_limited_on_its_own() {
        let pusher = Pusher::default();
        let gap = Duration::from_secs(60);
        let start = Instant::now();
        assert!(pusher.admit(PushKind::Gate, gap, start));
        assert!(!pusher.admit(PushKind::Gate, gap, start + Duration::from_secs(59)));
        assert!(pusher.admit(PushKind::Question, gap, start + Duration::from_secs(1)));
        assert!(pusher.admit(PushKind::Gate, gap, start + gap));
        assert!(pusher.admit(PushKind::Budget, Duration::ZERO, start));
        assert!(pusher.admit(PushKind::Budget, Duration::ZERO, start));
    }

    #[test]
    fn curl_config_keeps_quotes_and_backslashes_literal() {
        let json = r#"{"title":"say \"hi\" \\n"}"#;
        let config = curl_config("https://hooks.example/a?t=1", json);
        assert!(
            config.contains(r#"url = "https://hooks.example/a?t=1""#),
            "{config}"
        );
        assert!(
            config.contains(r#"data-binary = "{\"title\":\"say \\\"hi\\\" \\\\n\"}""#),
            "{config}"
        );
        assert_eq!(config.lines().count(), 10, "one option a line: {config}");
    }
}
