//! The `archon` arm (ARCHITECTURE §15): a private `archond` with a fresh
//! `ARCHON_DATA_DIR`, every fixture registered with `--ship local`, the
//! prompt sent as one operator `Say`, then a wait until the orchestrator is
//! idle and every item has settled. Driven only through `archon-client`
//! and the documented `archon daemon run` command.
//!
//! The private daemon must listen on the default socket: Aether spawns the
//! orchestrator's `archon mcp` proxy with its own environment, and that
//! proxy dials the default socket. An operator daemon already there is
//! refused rather than hijacked.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{Context, bail, ensure};
use archon_aether::AetherClient;
use archon_aether::wire::EventQuery;
use archon_client::ArchonClient;
use archon_core::aether_ids::AetherAgentId;
use archon_core::autonomy::ReviewLevel;
use archon_core::error::ArchonError;
use archon_core::id::RepoId;
use archon_core::notify::{FeedEntry, FeedItem};
use archon_core::protocol::{ClientKind, ItemDetail, ItemQuery, Reply, RepoAdd, Request};
use archon_core::queue::{QueueItem, QueueState};
use archon_core::repo::ShipMode;
use archon_core::run::PipelineRun;
use serde::Deserialize;
use serde_json::Value;

use crate::arms::aether::{endpoint, fetch_events};
use crate::arms::{
    ArmOutcome, ArmSettings, PrivateSockets, TrialInput, archon_config_toml, archon_daemon_command,
};
use crate::grade::ResultSource;
use crate::metrics::{ArchonItem, Arm};
use crate::usage::{Usage, aether_usage};
use crate::workspace::{self, BASE_BRANCH, TrialLayout};

const POLL_INTERVAL: Duration = Duration::from_secs(5);
/// How long the fleet must stay quiet before the trial counts as finished;
/// covers trigger coalescing and the orchestrator's follow-up turn.
pub const SETTLE_WINDOW: Duration = Duration::from_secs(45);
const DAEMON_LISTEN_TIMEOUT: Duration = Duration::from_secs(30);
const READY_TIMEOUT: Duration = Duration::from_secs(180);
const SHUTDOWN_GRACE: Duration = Duration::from_secs(20);
/// Aether agent statuses in which the orchestrator still has work to do.
/// Why the bench cancels the items still unsettled at the deadline.
pub const TIMEOUT_REASON: &str = "archon-bench: trial timeout";
const BUSY_STATUSES: &[&str] = &["created", "ready", "running"];

/// Read only captured routing fields so legacy captures never acquire policy defaults.
pub fn read_archon_work(logs: &Path) -> anyhow::Result<Option<Vec<ArchonItem>>> {
    let path = logs.join("archon-items.json");
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    parse_archon_work(&text)
        .map(Some)
        .with_context(|| format!("parsing {}", path.display()))
}

fn parse_archon_work(text: &str) -> anyhow::Result<Vec<ArchonItem>> {
    #[derive(Deserialize)]
    struct Policy {
        review: Option<ReviewLevel>,
    }
    #[derive(Deserialize)]
    struct Item {
        pipeline: String,
        autonomy: Option<Policy>,
    }
    #[derive(Deserialize)]
    struct Run {
        pipeline: String,
    }
    #[derive(Deserialize)]
    struct Detail {
        item: Item,
        #[serde(default)]
        runs: Vec<Run>,
    }
    let details: Vec<Detail> = serde_json::from_str(text)?;
    details
        .into_iter()
        .map(|detail| {
            ensure!(
                !detail.item.pipeline.trim().is_empty(),
                "empty item pipeline"
            );
            ensure!(
                detail.runs.iter().all(|r| !r.pipeline.trim().is_empty()),
                "empty run pipeline"
            );
            Ok(ArchonItem {
                pipeline: detail.item.pipeline,
                run_pipelines: detail.runs.into_iter().map(|r| r.pipeline).collect(),
                review: detail.item.autonomy.and_then(|p| p.review),
            })
        })
        .collect()
}

pub async fn run_archon(input: &TrialInput<'_>) -> ArmOutcome {
    let sockets = match daemon_sockets(input.settings) {
        Ok(sockets) => sockets,
        Err(e) => return ArmOutcome::failed(format!("{e:#}"), ResultSource::Workspace),
    };
    if let Err(e) = ensure_socket_free(&sockets.archon).await {
        return ArmOutcome::failed(format!("{e:#}"), ResultSource::Workspace);
    }
    let mut daemon = match Daemon::start(input, &sockets.archon) {
        Ok(daemon) => daemon,
        Err(e) => return ArmOutcome::failed(format!("{e:#}"), ResultSource::Workspace),
    };
    let client = connect_when_listening(&sockets.archon, &mut daemon).await;
    let mut outcome = match &client {
        Ok(client) => match session(input, client).await {
            Ok(outcome) => outcome,
            Err(e) => ArmOutcome::failed(format!("{e:#}"), ResultSource::Branches(BTreeMap::new())),
        },
        Err(e) => ArmOutcome::failed(format!("{e:#}"), ResultSource::Branches(BTreeMap::new())),
    };
    if let Err(e) = daemon.stop(client.as_ref().ok(), &sockets.aether).await {
        let prior = outcome
            .error
            .take()
            .map(|p| format!("{p}; "))
            .unwrap_or_default();
        outcome.error = Some(format!("{prior}stopping archond: {e:#}"));
    }
    outcome
}

/// The validated sandbox sockets. The orchestrator's MCP proxy dials the
/// default socket, so the private archond must listen exactly there.
fn daemon_sockets(settings: &ArmSettings) -> anyhow::Result<&PrivateSockets> {
    let sockets = settings.private_sockets()?;
    let dialled = archon_client::default_socket();
    ensure!(
        sockets.archon == dialled,
        "the private archond would listen at {} but ARCHON_SOCKET resolves to {}",
        sockets.archon.display(),
        dialled.display()
    );
    Ok(sockets)
}

async fn ensure_socket_free(socket: &Path) -> anyhow::Result<()> {
    match ArchonClient::connect(socket, ClientKind::ArchonBench).await {
        Err(ArchonError::Unavailable(_)) => Ok(()),
        Ok(_) => bail!(
            "an archond already listens at {}; stop it (`archon daemon stop`) so the benchmark can \
             run its own isolated daemon there",
            socket.display()
        ),
        Err(e) => Err(e).context("probing the archon socket"),
    }
}

/// The bench-owned `archon daemon run` process.
pub(crate) struct Daemon {
    pub(crate) child: tokio::process::Child,
}

impl Daemon {
    fn start(input: &TrialInput<'_>, socket: &Path) -> anyhow::Result<Self> {
        let dir = &input.layout.archon;
        for sub in ["config", "data"] {
            std::fs::create_dir_all(dir.join(sub))?;
        }
        std::fs::write(
            dir.join("config/config.toml"),
            archon_config_toml(input.settings)?,
        )?;
        let spec = archon_daemon_command(input.settings, dir, socket);
        let log = std::fs::File::create(input.layout.logs.join("archond.log"))?;
        let child = spec
            .command()
            .stdout(log.try_clone()?)
            .stderr(log)
            .spawn()
            .with_context(|| format!("starting {}", spec.render()))?;
        Ok(Self { child })
    }

    /// Ask archond to shut down (killing it if it lingers), then cancel the
    /// trial's long-lived orchestrator in Aether. Cancelling first would make
    /// archond's supervisor restart the orchestrator as a fresh agent.
    async fn stop(
        &mut self,
        client: Option<&ArchonClient>,
        aether_socket: &Path,
    ) -> anyhow::Result<()> {
        let mut problems = Vec::new();
        let mut orchestrator = None;
        if let Some(client) = client {
            match orchestrator_agent(client).await {
                Ok(agent) => orchestrator = Some(agent),
                Err(e) => problems.push(format!("finding the orchestrator: {e:#}")),
            }
            if let Err(e) = client.request(Request::Shutdown).await {
                problems.push(format!("shutdown request: {e}"));
            }
        }
        self.exit_then_cancel(orchestrator.as_ref(), aether_socket, problems)
            .await
    }

    /// Wait for archond to exit (or kill it), and only then cancel the
    /// orchestrator, so no supervisor is left to replace it.
    pub(crate) async fn exit_then_cancel(
        &mut self,
        orchestrator: Option<&AetherAgentId>,
        aether_socket: &Path,
        mut problems: Vec<String>,
    ) -> anyhow::Result<()> {
        if tokio::time::timeout(SHUTDOWN_GRACE, self.child.wait())
            .await
            .is_err()
        {
            self.child.kill().await.context("killing archond")?;
            problems.push("archond ignored shutdown and was killed".to_owned());
        }
        assert!(
            self.child.try_wait()?.is_some(),
            "archond is gone before its orchestrator is cancelled"
        );
        if let Some(agent) = orchestrator
            && let Err(e) = cancel_orchestrator(agent, aether_socket).await
        {
            problems.push(format!("cancelling the orchestrator: {e:#}"));
        }
        if problems.is_empty() {
            return Ok(());
        }
        bail!(problems.join("; "))
    }
}

async fn orchestrator_agent(client: &ArchonClient) -> anyhow::Result<AetherAgentId> {
    let Reply::Orchestrator(info) = client.request(Request::OrchestratorStatus).await? else {
        bail!("orchestrator_status returned another reply kind");
    };
    Ok(info.agent)
}

async fn cancel_orchestrator(agent: &AetherAgentId, aether_socket: &Path) -> anyhow::Result<()> {
    let aether = AetherClient::connect(&endpoint(aether_socket)).await?;
    aether.cancel(agent, "archon-bench: trial finished").await?;
    Ok(())
}

async fn connect_when_listening(
    socket: &Path,
    daemon: &mut Daemon,
) -> anyhow::Result<ArchonClient> {
    let deadline = Instant::now() + DAEMON_LISTEN_TIMEOUT;
    loop {
        match ArchonClient::connect(socket, ClientKind::ArchonBench).await {
            Ok(client) => return Ok(client),
            Err(ArchonError::Unavailable(_)) if Instant::now() < deadline => {}
            Err(e) => return Err(e).context("archond never listened; see logs/archond.log"),
        }
        if let Some(status) = daemon.child.try_wait()? {
            bail!("archond exited with {status} before listening; see logs/archond.log");
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

async fn session(input: &TrialInput<'_>, client: &ArchonClient) -> anyhow::Result<ArmOutcome> {
    let setup = Instant::now();
    wait_until_ready(client).await?;
    let setup_secs = setup.elapsed().as_secs_f64();
    let repos = register_repos(input, client).await?;
    let baseline = last_feed_seq(client).await?;
    let started = Instant::now();
    client
        .request(Request::Say {
            text: input.task.file.prompt.clone(),
        })
        .await
        .context("sending the prompt with Say")?;
    let (timed_out, finished) = wait_until_settled(client, baseline, input.timeout).await?;
    let wall_secs = finished.duration_since(started).as_secs_f64();
    let feed = feed_after(client, baseline).await?;
    let details = item_details(client).await?;
    std::fs::write(
        input.layout.logs.join("archon-items.json"),
        serde_json::to_string_pretty(&details)?,
    )?;
    let usage = fleet_usage(client, input, &details).await?;
    Ok(ArmOutcome {
        usage: Some(input.settings.price(Arm::Archon, usage)),
        answer: answer_text(&feed, &details),
        operator_messages: operator_messages(&feed),
        wall_secs,
        setup_secs: Some(setup_secs),
        timed_out,
        error: None,
        source: ResultSource::Branches(result_branches(&details, &repos)),
        timeout_source: timed_out
            .then(|| {
                let committed = |repo: &str, branch: &str| has_commits(input.layout, repo, branch);
                timeout_branches(&details, &repos, committed)
            })
            .flatten()
            .map(ResultSource::Branches),
    })
}

async fn wait_until_ready(client: &ArchonClient) -> anyhow::Result<()> {
    let deadline = Instant::now() + READY_TIMEOUT;
    loop {
        let Reply::Status(status) = client.request(Request::Status).await? else {
            bail!("status returned another reply kind");
        };
        let linked = matches!(
            status.aether,
            archon_core::protocol::AetherLinkStatus::Connected { .. }
        );
        if linked && status.orchestrator.is_some() {
            return Ok(());
        }
        if Instant::now() >= deadline {
            bail!(
                "archond was not ready within {READY_TIMEOUT:?}: aether {:?}",
                status.aether
            );
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

/// Register every fixture (`--ship local`, base `main`); repo id → name.
async fn register_repos(
    input: &TrialInput<'_>,
    client: &ArchonClient,
) -> anyhow::Result<BTreeMap<RepoId, String>> {
    let mut repos = BTreeMap::new();
    for fixture in input.task.fixtures() {
        let request = Request::RepoAdd(RepoAdd {
            path: input.layout.repo(&fixture),
            name: Some(fixture.name.clone()),
            ship: Some(ShipMode::Local),
            base_branch: Some(BASE_BRANCH.to_owned()),
            default_pipeline: None,
            auto_merge: false,
        });
        let Reply::Repo(repo) = client.request(request).await? else {
            bail!("repo_add returned another reply kind");
        };
        assert_eq!(repo.name, fixture.name, "archond renamed the repository");
        repos.insert(repo.id, fixture.name);
    }
    Ok(repos)
}

async fn last_feed_seq(client: &ArchonClient) -> anyhow::Result<u64> {
    let request = Request::FeedHistory {
        before: None,
        limit: 1,
        verbose: true,
    };
    let Reply::Feed(items) = client.request(request).await? else {
        bail!("feed_history returned another reply kind");
    };
    Ok(items.iter().map(|i| i.seq).max().unwrap_or(0))
}

async fn feed_after(client: &ArchonClient, baseline: u64) -> anyhow::Result<Vec<FeedItem>> {
    let request = Request::FeedHistory {
        before: None,
        limit: 100_000,
        verbose: true,
    };
    let Reply::Feed(mut items) = client.request(request).await? else {
        bail!("feed_history returned another reply kind");
    };
    items.retain(|i| i.seq > baseline);
    items.sort_by_key(|i| i.seq);
    Ok(items)
}

/// Poll until [`Settle`] reports quiet for [`SETTLE_WINDOW`]; past the
/// timeout, cancel every unfinished item. Returns whether it timed out and
/// when the work ended: the start of the quiet window, which the wall time
/// must not include, or the timeout.
async fn wait_until_settled(
    client: &ArchonClient,
    baseline: u64,
    timeout: Duration,
) -> anyhow::Result<(bool, Instant)> {
    let deadline = Instant::now() + timeout;
    let mut settle = Settle::default();
    loop {
        let observed = observe(client, baseline).await?;
        if settle.update(&observed, Instant::now()) {
            let quiet_since = settle.quiet_since().expect("settled means quiet");
            return Ok((false, quiet_since));
        }
        if Instant::now() >= deadline {
            for item in observed.items.iter().filter(|i| !is_settled(&i.state)) {
                let reason = Some(TIMEOUT_REASON.to_owned());
                client
                    .request(Request::CancelItem {
                        item: item.id.clone(),
                        reason,
                    })
                    .await?;
            }
            return Ok((true, Instant::now()));
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

/// One look at the fleet.
#[derive(Debug, Clone, PartialEq)]
pub struct Observation {
    pub replied: bool,
    pub orchestrator_busy: bool,
    pub items: Vec<QueueItem>,
}

impl Observation {
    /// Nothing left to happen without the operator.
    pub fn quiet(&self) -> bool {
        !self.orchestrator_busy
            && (self.replied || !self.items.is_empty())
            && self.items.iter().all(|i| is_settled(&i.state))
    }
}

/// Terminal, or blocked on the operator (the bench never answers).
pub fn is_settled(state: &QueueState) -> bool {
    state.is_terminal() || matches!(state, QueueState::Blocked { .. })
}

/// Quiet must hold across a whole window, so a trigger-woken orchestrator
/// turn (which may enqueue follow-ups) is not mistaken for the end.
#[derive(Debug, Default)]
pub struct Settle {
    quiet_since: Option<Instant>,
}

impl Settle {
    pub fn update(&mut self, observed: &Observation, now: Instant) -> bool {
        if !observed.quiet() {
            self.quiet_since = None;
            return false;
        }
        let since = *self.quiet_since.get_or_insert(now);
        now.duration_since(since) >= SETTLE_WINDOW
    }

    /// When the current quiet stretch began, if the fleet is quiet.
    pub fn quiet_since(&self) -> Option<Instant> {
        self.quiet_since
    }
}

async fn observe(client: &ArchonClient, baseline: u64) -> anyhow::Result<Observation> {
    let Reply::Items(items) = client
        .request(Request::ListItems(ItemQuery::default()))
        .await?
    else {
        bail!("list_items returned another reply kind");
    };
    let Reply::Orchestrator(info) = client.request(Request::OrchestratorStatus).await? else {
        bail!("orchestrator_status returned another reply kind");
    };
    let feed = feed_after(client, baseline).await?;
    Ok(Observation {
        replied: feed
            .iter()
            .any(|i| matches!(i.entry, FeedEntry::Orchestrator { .. })),
        orchestrator_busy: BUSY_STATUSES.contains(&info.status.as_str()),
        items,
    })
}

async fn item_details(client: &ArchonClient) -> anyhow::Result<Vec<ItemDetail>> {
    let Reply::Items(items) = client
        .request(Request::ListItems(ItemQuery::default()))
        .await?
    else {
        bail!("list_items returned another reply kind");
    };
    let mut details = Vec::with_capacity(items.len());
    for item in items {
        let Reply::ItemDetail(detail) = client.request(Request::GetItem { item: item.id }).await?
        else {
            bail!("get_item returned another reply kind");
        };
        details.push(*detail);
    }
    Ok(details)
}

/// Aether usage of the orchestrator and every stage agent. Falls back to
/// the spend ledger's dollars when Aether reports nothing.
async fn fleet_usage(
    client: &ArchonClient,
    input: &TrialInput<'_>,
    details: &[ItemDetail],
) -> anyhow::Result<Usage> {
    let Reply::Orchestrator(info) = client.request(Request::OrchestratorStatus).await? else {
        bail!("orchestrator_status returned another reply kind");
    };
    let mut agents: Vec<AetherAgentId> = vec![info.agent];
    agents.extend(
        details
            .iter()
            .flat_map(|d| d.stages.iter().filter_map(|s| s.aether_agent.clone())),
    );
    let aether =
        AetherClient::connect(&endpoint(&input.settings.private_sockets()?.aether)).await?;
    let mut events: BTreeMap<u64, Value> = BTreeMap::new();
    for agent in agents {
        let query = EventQuery {
            agent: Some(agent),
            types: vec!["agent.usage_updated".into()],
            ..EventQuery::default()
        };
        for event in fetch_events(&aether, query).await? {
            let seq = event["seq"].as_u64().context("an event without seq")?;
            events.insert(seq, event);
        }
    }
    let events: Vec<Value> = events.into_values().collect();
    if let Some(usage) = aether_usage(&events)? {
        return Ok(usage);
    }
    let Reply::Status(status) = client.request(Request::Status).await? else {
        bail!("status returned another reply kind");
    };
    Ok(Usage {
        cost_usd: Some(status.spend_total_usd),
        ..Usage::default()
    })
}

/// Default-visible feed items, excluding the operator's own lines.
pub fn operator_messages(feed: &[FeedItem]) -> u32 {
    let count = feed
        .iter()
        .filter(|i| i.visible(false))
        .filter(|i| {
            !matches!(
                i.entry,
                FeedEntry::Operator { .. } | FeedEntry::Answer { .. }
            )
        })
        .count();
    u32::try_from(count).unwrap_or(u32::MAX)
}

/// What the operator reads: every visible feed text, then the summaries of
/// succeeded items (one `archon queue show` away).
pub fn answer_text(feed: &[FeedItem], details: &[ItemDetail]) -> String {
    let mut parts: Vec<String> = feed
        .iter()
        .filter(|i| i.visible(false))
        .filter_map(|i| match &i.entry {
            FeedEntry::Orchestrator { text } | FeedEntry::System { text } => Some(text.clone()),
            FeedEntry::Authority { action, result, .. } => Some(format!("{action}: {result}")),
            FeedEntry::Notification(n) => Some(n.text.clone()),
            FeedEntry::Question(q) => Some(q.text.clone()),
            FeedEntry::Report {
                title, location, ..
            } => Some(format!("{title}: {location}")),
            FeedEntry::Operator { .. } | FeedEntry::Answer { .. } => None,
        })
        .collect();
    for detail in details {
        if let QueueState::Succeeded { summary, .. } = &detail.item.state {
            parts.push(summary.clone());
        }
    }
    parts.join("\n\n")
}

/// Per repository name, the latest run branch of each succeeded item, in
/// the order the items finished; runs without a branch are left out.
pub fn result_branches(
    details: &[ItemDetail],
    repos: &BTreeMap<RepoId, String>,
) -> BTreeMap<String, Vec<String>> {
    let mut done: Vec<&ItemDetail> = details
        .iter()
        .filter(|d| matches!(d.item.state, QueueState::Succeeded { .. }))
        .collect();
    done.sort_by_key(|d| d.item.updated_at);
    let mut branches: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for detail in done {
        let Some(name) = repo_name(detail, repos) else {
            continue;
        };
        let QueueState::Succeeded { run, .. } = &detail.item.state else {
            unreachable!("filtered to succeeded items above");
        };
        // Answer-only pipelines (`ask`) succeed without a branch.
        if let Some(found) = detail
            .runs
            .iter()
            .find(|r| &r.id == run && !r.branch.is_empty())
        {
            branches
                .entry(name.clone())
                .or_default()
                .push(found.branch.clone());
        }
    }
    branches
}

/// [`result_branches`] plus, after them, the last committed branch of each
/// item the bench cancelled at the deadline. `None` when no timed-out item
/// has one, so there is nothing beyond the verdict to grade.
pub fn timeout_branches(
    details: &[ItemDetail],
    repos: &BTreeMap<RepoId, String>,
    committed: impl Fn(&str, &str) -> bool,
) -> Option<BTreeMap<String, Vec<String>>> {
    let mut late: Vec<(&ItemDetail, &String, &str)> = details
        .iter()
        .filter(|d| is_timed_out(&d.item.state))
        .filter_map(|d| {
            let name = repo_name(d, repos)?;
            Some((d, name, last_committed(&d.runs, |b| committed(name, b))?))
        })
        .collect();
    late.sort_by_key(|(d, _, _)| d.item.updated_at);
    let mut branches = result_branches(details, repos);
    let added = late.len();
    for (_, name, branch) in late {
        branches
            .entry(name.clone())
            .or_default()
            .push(branch.to_owned());
    }
    (added > 0).then_some(branches)
}

/// Whether `branch` of fixture `repo` holds commits past `bench-base`; a
/// missing branch holds none.
fn has_commits(layout: &TrialLayout, repo: &str, branch: &str) -> bool {
    assert!(!branch.is_empty(), "only named branches are checked");
    let range = format!("{}..{branch}", workspace::BASE_TAG);
    let dir = layout.workspace.join(repo);
    workspace::git(&dir, &["rev-list", "--count", &range])
        .is_ok_and(|count| count.trim().parse::<u64>().is_ok_and(|n| n > 0))
}

fn is_timed_out(state: &QueueState) -> bool {
    matches!(state, QueueState::Cancelled { reason: Some(r) } if r == TIMEOUT_REASON)
}

/// The newest run's branch that `committed` says holds work: a run records
/// its branch before its first commit, so a fresh retry may hold none.
fn last_committed(runs: &[PipelineRun], committed: impl Fn(&str) -> bool) -> Option<&str> {
    let mut named: Vec<&PipelineRun> = runs.iter().filter(|r| !r.branch.is_empty()).collect();
    named.sort_by_key(|r| std::cmp::Reverse(r.started_at));
    named
        .into_iter()
        .map(|r| r.branch.as_str())
        .find(|branch| committed(branch))
}

fn repo_name<'a>(detail: &ItemDetail, repos: &'a BTreeMap<RepoId, String>) -> Option<&'a String> {
    detail.item.repo.as_ref().and_then(|r| repos.get(r))
}
