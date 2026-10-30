//! The one way archond runs `gh` (ARCHITECTURE §8.7): in the registered
//! repository, never an agent's worktree, with no prompt and a time limit
//! that ends it and everything it started. The git `gh` runs itself is told
//! to run no hook or fsmonitor. Pull requests are read through one type,
//! checked against the branch or the number they were asked about.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use archon_core::error::ArchonError;
use archon_core::merge::{Check, CheckOutcome, PullRequestFacts};
use archon_core::pull_request::PullRequestRef;
use serde::Deserialize;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use archon_core::repo::{GH_HOST, Repo};

use super::dependencies::{is_plain_branch, is_plain_url};
use crate::gh_account::{self, Token};
use crate::worktree::daemon_git::no_program_settings;
use crate::worktree::stack::is_commit_id;

pub(crate) mod checks;

/// Most one `gh` command may take.
pub(crate) const TIMEOUT: Duration = Duration::from_secs(60);
/// Characters of a tool's own words kept in an error.
const MAX_DETAIL_CHARS: usize = 1_500;
/// Most pull requests one listing returns: a repository's open ones, or a
/// branch's of any state.
const MAX_OPEN: &str = "1000";
const MAX_OF_BRANCH: &str = "20";
/// Everything archond reads of a pull request.
const FIELDS: &str =
    "id,number,url,state,headRefName,headRefOid,baseRefName,body,isDraft,reviewDecision";

/// Everything a merge decision reads of a pull request (§18.6).
const MERGE_FIELDS: &str =
    "state,isDraft,headRefOid,baseRefName,mergeable,statusCheckRollup,mergedBy,autoMergeRequest";

/// What git or `gh` says when only a person can let the delivery through.
const NEEDS_OPERATOR: [&str; 14] = [
    "permission denied",
    "permission to",
    "authentication failed",
    "could not read username",
    "could not read password",
    "terminal prompts disabled",
    "repository not found",
    "gh auth login",
    "bad credentials",
    "http 401",
    "http 403",
    "resource not accessible",
    "could not resolve to a repository",
    "known github host",
];

/// A pull request as `gh` lists it.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PullRequest {
    /// GitHub's node id, which older `gh` may not give.
    #[serde(default)]
    pub id: Option<String>,
    pub number: u64,
    pub url: String,
    /// `OPEN`, `CLOSED` or `MERGED`.
    pub state: String,
    pub head_ref_name: String,
    #[serde(default)]
    pub head_ref_oid: Option<String>,
    pub base_ref_name: String,
    #[serde(default)]
    pub body: String,
    #[serde(default)]
    pub is_draft: bool,
    /// `CHANGES_REQUESTED`, `APPROVED`, `REVIEW_REQUIRED`, or empty.
    #[serde(default)]
    pub review_decision: Option<String>,
}

impl PullRequest {
    pub(crate) fn is_open(&self) -> bool {
        self.state == "OPEN"
    }

    /// Which pull request this is, by host, repository and number.
    pub(crate) fn identity(&self) -> PullRequestRef {
        let pr = PullRequestRef::from_url(&self.url).expect("checked when read");
        assert_eq!(pr.number, self.number, "checked when read");
        pr
    }

    /// GitHub's node id, when it gave a usable one.
    pub(crate) fn node_id(&self) -> Option<&str> {
        let id = self.id.as_deref().filter(|id| !id.trim().is_empty());
        id.filter(|id| {
            id.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"_-=".contains(&b))
        })
    }

    /// Refuse what GitHub could not have meant: a pull request of another
    /// branch, an unknown state or a URL that is not its own.
    fn check(&self, branch: Option<&str>) -> Result<(), ArchonError> {
        let named = PullRequestRef::from_url(&self.url);
        let shaped = self.number != 0
            && is_plain_url(&self.url)
            && named.is_some_and(|pr| pr.number == self.number)
            && is_plain_branch(&self.head_ref_name)
            && is_plain_branch(&self.base_ref_name)
            && matches!(self.state.as_str(), "OPEN" | "CLOSED" | "MERGED")
            && self.head_ref_oid.as_deref().is_none_or(is_commit_id)
            && branch.is_none_or(|branch| branch == self.head_ref_name);
        if !shaped {
            return Err(ArchonError::Protocol(format!(
                "GitHub listed a pull request that does not match its branch, state or URL: {} \
                 ({}, head `{}`)",
                self.url, self.state, self.head_ref_name
            )));
        }
        Ok(())
    }
}

/// A pull request as `gh pr view` shows it for a merge.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct MergeView {
    state: String,
    #[serde(default)]
    is_draft: bool,
    head_ref_oid: String,
    base_ref_name: String,
    #[serde(default)]
    mergeable: String,
    #[serde(default)]
    status_check_rollup: Option<Vec<Rollup>>,
    merged_by: Option<MergeActor>,
    auto_merge_request: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
struct MergeActor {
    login: String,
}

/// One entry of `statusCheckRollup`: a check run, or a commit status.
#[derive(Debug, Deserialize)]
struct Rollup {
    #[serde(rename = "__typename", default)]
    kind: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    context: String,
    #[serde(default)]
    status: String,
    #[serde(default)]
    conclusion: Option<String>,
    #[serde(default)]
    state: String,
}

impl Rollup {
    fn check(&self) -> Check {
        let status_context =
            self.kind == "StatusContext" || (self.kind.is_empty() && self.status.is_empty());
        let outcome = if status_context {
            match self.state.as_str() {
                "SUCCESS" => CheckOutcome::Green,
                "PENDING" | "EXPECTED" => CheckOutcome::Pending,
                _ => CheckOutcome::Failed,
            }
        } else if self.status != "COMPLETED" {
            CheckOutcome::Pending
        } else {
            match self.conclusion.as_deref() {
                Some("SUCCESS" | "NEUTRAL" | "SKIPPED") => CheckOutcome::Green,
                _ => CheckOutcome::Failed,
            }
        };
        let name = [&self.name, &self.context]
            .into_iter()
            .find(|n| !n.is_empty())
            .map_or_else(|| "an unnamed check".to_owned(), Clone::clone);
        Check { name, outcome }
    }
}

/// What GitHub says now of the pull request at `url`, for a merge decision.
pub(crate) async fn merge_facts(
    binary: &Path,
    repo: &Repo,
    url: &str,
) -> Result<PullRequestFacts, ArchonError> {
    if !is_plain_url(url) {
        return Err(ArchonError::Invalid(format!(
            "the recorded pull request URL {url:?} is not a plain URL; `archon queue show` names the run's pull request"
        )));
    }
    let args = ["pr", "view", url, "--json", MERGE_FIELDS];
    let raw = text(binary, repo, &args).await?;
    let view: MergeView = serde_json::from_str(&raw)
        .map_err(|e| ArchonError::Protocol(format!("invalid `gh pr view` response: {e}")))?;
    let known = matches!(view.state.as_str(), "OPEN" | "CLOSED" | "MERGED");
    if !known || !is_commit_id(&view.head_ref_oid) || !is_plain_branch(&view.base_ref_name) {
        return Err(ArchonError::Protocol(format!(
            "GitHub showed {url} with state {:?}, head {:?} and base {:?}",
            view.state, view.head_ref_oid, view.base_ref_name
        )));
    }
    let checks = view.status_check_rollup.unwrap_or_default();
    Ok(PullRequestFacts {
        merged_by: view.merged_by.map(|actor| actor.login),
        auto_merge: view.auto_merge_request.is_some(),
        state: view.state,
        is_draft: view.is_draft,
        head: view.head_ref_oid,
        base: view.base_ref_name,
        mergeable: view.mergeable,
        checks: checks.iter().map(Rollup::check).collect(),
    })
}

/// What a finished `gh` printed.
#[derive(Debug)]
pub(crate) struct Ran {
    pub ok: bool,
    pub stdout: String,
    pub stderr: String,
}

impl Ran {
    /// What it printed, with `token` redacted wherever it shows.
    fn redacted(self, token: &Token) -> Self {
        Self {
            ok: self.ok,
            stdout: token.redact(&self.stdout),
            stderr: token.redact(&self.stderr),
        }
    }

    /// The command's own account of a failure, for an error.
    pub(crate) fn detail(&self) -> String {
        let said = match self.stderr.trim() {
            "" => self.stdout.trim(),
            stderr => stderr,
        };
        said.chars().take(MAX_DETAIL_CHARS).collect()
    }
}

/// `BLOCKED:` and why, when `said` shows that only a person can help.
pub(crate) fn needs_operator(doing: &str, said: &str) -> Option<String> {
    let lower = said.to_lowercase();
    let blocked = NEEDS_OPERATOR.iter().any(|sign| lower.contains(sign));
    blocked.then(|| format!("BLOCKED: {doing} was refused for want of access: {said}"))
}

/// `gh <args>` in `repo` with `input` on its stdin, for at most [`TIMEOUT`],
/// as the repository's GitHub login when the operator named one.
pub(crate) async fn run(
    binary: &Path,
    repo: &Repo,
    args: &[&str],
    input: Option<&str>,
) -> Result<Ran, ArchonError> {
    let Some(token) = token(binary, repo).await? else {
        return spawn(binary, &repo.path, args, input, Auth::Ambient).await;
    };
    let ran = spawn(binary, &repo.path, args, input, Auth::As(&token)).await?;
    Ok(ran.redacted(&token))
}

/// The token of `repo`'s GitHub login, read now; `None` when it runs as
/// the daemon's own. A login `gh` cannot give, or an `origin` that is not
/// github.com's, is a `BLOCKED:` refusal.
pub(crate) async fn token(binary: &Path, repo: &Repo) -> Result<Option<Token>, ArchonError> {
    let Some(user) = &repo.gh_user else {
        return Ok(None);
    };
    let (path, owner) = (repo.path.clone(), user.clone());
    let reachable = tokio::task::spawn_blocking(move || gh_account::push_url(&path, &owner))
        .await
        .map_err(|e| ArchonError::io("reading `origin`", std::io::Error::other(e)))??;
    if let Err(why) = reachable {
        return Err(ArchonError::Invalid(format!("BLOCKED: {why}")));
    }
    let lookup = spawn(binary, &repo.path, &user.token_args(), None, Auth::Lookup).await?;
    if !lookup.ok {
        // Only stderr: what a failed lookup printed on stdout is not shown.
        let said: String = lookup
            .stderr
            .trim()
            .chars()
            .take(MAX_DETAIL_CHARS)
            .collect();
        return Err(ArchonError::Invalid(format!(
            "BLOCKED: `gh` has no token for {user}, the GitHub login `{}` runs as: {said}. Log \
             it in with `gh auth login --hostname {GH_HOST}`, or change the repository's \
             `--gh-user`",
            repo.name
        )));
    }
    let token = Token::read(user, &lookup.stdout)?;
    assert_eq!(token.user(), user, "the login asked for");
    Ok(Some(token))
}

/// Whose credentials a `gh` uses.
#[derive(Debug, Clone, Copy)]
enum Auth<'a> {
    /// The daemon's own login, as its environment and `gh` config say.
    Ambient,
    /// None: it reads a login's token from `gh`'s own config.
    Lookup,
    /// This token only.
    As(&'a Token),
}

/// `gh <args>` in `repo` with `input` on its stdin, for at most [`TIMEOUT`].
async fn spawn(
    binary: &Path,
    repo: &Path,
    args: &[&str],
    input: Option<&str>,
    auth: Auth<'_>,
) -> Result<Ran, ArchonError> {
    assert!(repo.is_absolute(), "gh runs in a registered repository");
    assert!(!args.is_empty(), "a gh command has arguments");
    let what = format!(
        "gh {}",
        args.iter().take(2).copied().collect::<Vec<_>>().join(" ")
    );
    let mut command = command(binary, repo, args, auth);
    command.stdin(if input.is_some() {
        Stdio::piped()
    } else {
        Stdio::null()
    });
    let mut child = command
        .spawn()
        .map_err(|e| ArchonError::io(format!("running {what}"), e))?;
    let group = child.id().map(|pid| pid as i32);
    let work = async {
        if let (Some(input), Some(mut stdin)) = (input, child.stdin.take()) {
            stdin.write_all(input.as_bytes()).await?;
        }
        child.wait_with_output().await
    };
    let Ok(output) = tokio::time::timeout(TIMEOUT, work).await else {
        kill_group(group);
        return Err(ArchonError::Unavailable(format!(
            "{what} timed out after {}s",
            TIMEOUT.as_secs()
        )));
    };
    let output = output.map_err(|e| ArchonError::io(what, e))?;
    Ok(Ran {
        ok: output.status.success(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    })
}

fn command(binary: &Path, repo: &Path, args: &[&str], auth: Auth<'_>) -> Command {
    let mut command = Command::new(binary);
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("GIT_") {
            command.env_remove(key);
        }
    }
    let settings = no_program_settings();
    for (n, (key, value)) in settings.iter().enumerate() {
        command
            .env(format!("GIT_CONFIG_KEY_{n}"), key)
            .env(format!("GIT_CONFIG_VALUE_{n}"), value);
    }
    command
        .env("GIT_CONFIG_COUNT", settings.len().to_string())
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GH_PROMPT_DISABLED", "1")
        .env("GH_NO_UPDATE_NOTIFIER", "1")
        .env_remove("GH_REPO")
        .args(args)
        .current_dir(repo)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .kill_on_drop(true);
    if !matches!(auth, Auth::Ambient) {
        for name in gh_account::AMBIENT {
            command.env_remove(name);
        }
    }
    if let Auth::As(token) = auth {
        command.envs(token.gh_env());
    }
    command
}

/// End `gh` and whatever it started, after it overran.
fn kill_group(group: Option<i32>) {
    let Some(pid) = group.and_then(rustix::process::Pid::from_raw) else {
        return;
    };
    // Already gone is fine: kill_on_drop reaps what is left.
    let _ = rustix::process::kill_process_group(pid, rustix::process::Signal::KILL);
}

/// `gh <args>`, which must succeed: its stdout. A failure is worth another
/// try unless it shows that only the operator can help.
pub(crate) async fn text(binary: &Path, repo: &Repo, args: &[&str]) -> Result<String, ArchonError> {
    let ran = run(binary, repo, args, None).await?;
    succeeded(ran, &format!("gh {}", args.join(" ")))
}

/// The stdout of `ran`, which was `doing`, or why it failed.
fn succeeded(ran: Ran, doing: &str) -> Result<String, ArchonError> {
    if ran.ok {
        return Ok(ran.stdout);
    }
    let said = ran.detail();
    assert!(!doing.is_empty(), "a failure says what failed");
    match needs_operator(doing, &said) {
        Some(blocked) => Err(ArchonError::Invalid(blocked)),
        None => Err(ArchonError::Unavailable(format!("{doing} failed: {said}"))),
    }
}

/// `gh pr list <filter>`, which is `doing`, each pull request checked.
async fn list(
    binary: &Path,
    repo: &Repo,
    filter: &[&str],
    branch: Option<&str>,
    doing: &str,
) -> Result<Vec<PullRequest>, ArchonError> {
    assert!(
        filter.contains(&"--limit"),
        "gh lists 30 unless told otherwise"
    );
    assert!(
        branch.is_none_or(|branch| filter.contains(&branch)),
        "only a listing of one branch is checked against it"
    );
    let args = [&["pr", "list"], filter, &["--json", FIELDS]].concat();
    let raw = succeeded(run(binary, repo, &args, None).await?, doing)?;
    let prs: Vec<PullRequest> = serde_json::from_str(&raw)
        .map_err(|e| ArchonError::Protocol(format!("invalid `gh pr list` response: {e}")))?;
    for pr in &prs {
        pr.check(branch)?;
    }
    Ok(prs)
}

/// The pull requests whose head is `branch`, newest first: open ones only,
/// or of every state.
pub(crate) async fn of_branch(
    binary: &Path,
    repo: &Repo,
    branch: &str,
    only_open: bool,
) -> Result<Vec<PullRequest>, ArchonError> {
    if !is_plain_branch(branch) {
        return Err(ArchonError::Invalid(format!(
            "{branch:?} is no plain branch name"
        )));
    }
    let state = if only_open { "open" } else { "all" };
    let filter = ["--head", branch, "--state", state, "--limit", MAX_OF_BRANCH];
    let doing = format!("looking up the pull request of `{branch}`");
    let prs = list(binary, repo, &filter, Some(branch), &doing).await?;
    if only_open && !prs.iter().all(PullRequest::is_open) {
        return Err(ArchonError::Protocol(format!(
            "GitHub listed a pull request of `{branch}` that is not open when asked for open ones"
        )));
    }
    Ok(prs)
}

/// The pull request of `branch`: its open one, else its newest.
pub(crate) async fn view(
    binary: &Path,
    repo: &Repo,
    branch: &str,
) -> Result<PullRequest, ArchonError> {
    let prs = of_branch(binary, repo, branch, false).await?;
    let found = prs.iter().find(|pr| pr.is_open()).or(prs.first());
    let Some(pr) = found else {
        return Err(ArchonError::Unavailable(format!(
            "GitHub lists no pull request for `{branch}`"
        )));
    };
    assert_eq!(pr.head_ref_name, branch, "checked by list");
    Ok(pr.clone())
}

/// The pull request `pr` names, of any state, asked for by its number.
pub(crate) async fn view_number(
    binary: &Path,
    repo: &Repo,
    pr: &PullRequestRef,
) -> Result<PullRequest, ArchonError> {
    let url = pr.url();
    assert!(is_plain_url(&url), "a pull request URL is plain");
    let args = ["pr", "view", &url, "--json", FIELDS];
    let raw = text(binary, repo, &args).await?;
    let viewed: PullRequest = serde_json::from_str(&raw)
        .map_err(|e| ArchonError::Protocol(format!("invalid `gh pr view` response: {e}")))?;
    viewed.check(None)?;
    if viewed.identity() != *pr {
        return Err(ArchonError::Protocol(format!(
            "GitHub showed {} when asked for {pr}",
            viewed.url
        )));
    }
    Ok(viewed)
}

/// What `gh pr view` shows of a pull request to deliver into: [`FIELDS`]
/// and where its head branch lives.
const HEADED_FIELDS: &str = "id,number,url,state,headRefName,headRefOid,baseRefName,body,isDraft,\
     reviewDecision,isCrossRepository,headRepository,headRepositoryOwner";

/// A pull request with the repository its head branch is in.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Headed {
    #[serde(flatten)]
    pub pr: PullRequest,
    /// Whether the head is in another repository than the base, a fork.
    pub is_cross_repository: bool,
    head_repository: Option<HeadRepository>,
    head_repository_owner: Option<HeadOwner>,
}

#[derive(Debug, Clone, Deserialize)]
struct HeadRepository {
    name: String,
}

#[derive(Debug, Clone, Deserialize)]
struct HeadOwner {
    login: String,
}

impl Headed {
    /// `owner/name` of the head's repository, lowercase; `None` once it is gone.
    pub(crate) fn head_repository(&self) -> Option<String> {
        let name = &self.head_repository.as_ref()?.name;
        let owner = &self.head_repository_owner.as_ref()?.login;
        let plain = |s: &str| {
            !s.is_empty()
                && s.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
        };
        (plain(owner) && plain(name)).then(|| format!("{owner}/{name}").to_ascii_lowercase())
    }
}

/// The pull request `selector`, its number or URL, names in `repo`, with
/// where its head lives.
pub(crate) async fn view_headed(
    binary: &Path,
    repo: &Repo,
    selector: &str,
) -> Result<Headed, ArchonError> {
    let number = !selector.is_empty() && selector.bytes().all(|b| b.is_ascii_digit());
    assert!(
        number || PullRequestRef::from_url(selector).is_some(),
        "a pull request number or URL"
    );
    let args = ["pr", "view", selector, "--json", HEADED_FIELDS];
    let raw = text(binary, repo, &args).await?;
    let viewed: Headed = serde_json::from_str(&raw)
        .map_err(|e| ArchonError::Protocol(format!("invalid `gh pr view` response: {e}")))?;
    viewed.pr.check(None)?;
    let asked = match number {
        true => viewed.pr.number.to_string() == selector,
        false => PullRequestRef::from_url(selector) == Some(viewed.pr.identity()),
    };
    if !asked {
        return Err(ArchonError::Protocol(format!(
            "GitHub showed {} when asked for {selector}",
            viewed.pr.url
        )));
    }
    Ok(viewed)
}

/// Whether `repo`'s `gh` user may push to `full_name`, an `owner/name`:
/// GitHub grants it from the write role up.
pub(crate) async fn can_push(
    binary: &Path,
    repo: &Repo,
    full_name: &str,
) -> Result<bool, ArchonError> {
    assert!(
        full_name.split('/').count() == 2 && !full_name.starts_with('/'),
        "an owner/name"
    );
    let args = ["repo", "view", full_name, "--json", "viewerPermission"];
    let raw = text(binary, repo, &args).await?;
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Viewed {
        viewer_permission: Option<String>,
    }
    let viewed: Viewed = serde_json::from_str(&raw)
        .map_err(|e| ArchonError::Protocol(format!("invalid `gh repo view` response: {e}")))?;
    let permission = viewed.viewer_permission.unwrap_or_default();
    Ok(matches!(
        permission.as_str(),
        "ADMIN" | "MAINTAIN" | "WRITE"
    ))
}

/// One pass's reading of GitHub: one listing of each repository's open pull
/// requests, then one lookup for each pull request asked about that it lacks.
#[derive(Debug, Default)]
pub(crate) struct Pass {
    open: HashMap<PathBuf, Vec<PullRequest>>,
    /// Repositories whose listing failed this pass, with why.
    unlisted: HashMap<PathBuf, String>,
}

impl Pass {
    /// The pull request of `branch` in `repo`, as [`view`] finds it.
    pub(crate) async fn pull_request(
        &mut self,
        binary: &Path,
        repo: &Repo,
        branch: &str,
    ) -> Result<PullRequest, ArchonError> {
        let listed = self.open_in(binary, repo).await?;
        if let Some(pr) = listed.iter().find(|pr| pr.head_ref_name == branch) {
            return Ok(pr.clone());
        }
        view(binary, repo, branch).await
    }

    /// The pull request `pr` names, whatever branch now carries it.
    pub(crate) async fn by_number(
        &mut self,
        binary: &Path,
        repo: &Repo,
        pr: &PullRequestRef,
    ) -> Result<PullRequest, ArchonError> {
        let listed = self.open_in(binary, repo).await?;
        if let Some(open) = listed.iter().find(|open| open.identity() == *pr) {
            return Ok(open.clone());
        }
        view_number(binary, repo, pr).await
    }

    /// The open pull requests of `repo`, listed once a pass.
    async fn open_in(&mut self, binary: &Path, repo: &Repo) -> Result<&[PullRequest], ArchonError> {
        if !self.open.contains_key(&repo.path) {
            let filter = ["--state", "open", "--limit", MAX_OPEN];
            let doing = "listing the open pull requests";
            match list(binary, repo, &filter, None, doing).await {
                Ok(listed) => {
                    self.unlisted.remove(&repo.path);
                    self.open.insert(repo.path.clone(), listed);
                }
                Err(error) => {
                    self.unlisted.insert(repo.path.clone(), error.to_string());
                    return Err(error);
                }
            }
        }
        let listed = self.open.get(&repo.path).expect("listed above");
        assert!(listed.iter().all(PullRequest::is_open), "an open listing");
        Ok(listed)
    }

    /// Why the listing of `repo` failed this pass, if it did.
    pub(crate) fn unlisted(&self, repo: &Path) -> Option<&str> {
        let why = self.unlisted.get(repo).map(String::as_str);
        assert!(
            why.is_none() || !self.open.contains_key(repo),
            "a repository is either listed or not"
        );
        why
    }
}

/// `kv`: `pr_settled:<host>/<owner>/<name>#<number>` → the `MERGED` or
/// `CLOSED` GitHub said that pull request ended as; it is not asked again.
const SETTLED_PREFIX: &str = "pr_settled:";

fn settled_key(pr: &PullRequestRef) -> String {
    format!("{SETTLED_PREFIX}{pr}")
}

/// How the pull request `pr` ended, if it did.
pub(crate) fn settled(
    store: &archon_queue::Store,
    pr: &PullRequestRef,
) -> Result<Option<String>, ArchonError> {
    let key = settled_key(pr);
    let state = store.kv_get(&key)?;
    if state
        .as_deref()
        .is_some_and(|s| !matches!(s, "MERGED" | "CLOSED"))
    {
        return Err(ArchonError::Corrupt(format!("kv {key}: {state:?}")));
    }
    Ok(state)
}

/// Remember `pr` when it ended, so no later pass asks about it.
pub(crate) fn settle(store: &mut archon_queue::Store, pr: &PullRequest) -> Result<(), ArchonError> {
    if pr.is_open() {
        return Ok(());
    }
    assert!(
        matches!(pr.state.as_str(), "MERGED" | "CLOSED"),
        "checked when read"
    );
    store.kv_set(&settled_key(&pr.identity()), &pr.state)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pr(state: &str, head: &str) -> PullRequest {
        PullRequest {
            id: None,
            number: 7,
            url: "https://github.com/o/r/pull/7".into(),
            state: state.into(),
            head_ref_name: head.into(),
            head_ref_oid: Some("a".repeat(40)),
            base_ref_name: "main".into(),
            body: String::new(),
            is_draft: false,
            review_decision: None,
        }
    }

    #[test]
    fn a_pull_request_of_another_branch_or_state_is_refused() {
        assert!(pr("OPEN", "a/b").check(Some("a/b")).is_ok());
        assert!(pr("MERGED", "a/b").check(None).is_ok());
        assert!(pr("OPEN", "a/c").check(Some("a/b")).is_err());
        assert!(pr("DRAFT", "a/b").check(None).is_err());
        let mut other = pr("OPEN", "a/b");
        other.url = "https://github.com/o/r/pull/8".into();
        assert!(
            other.check(None).is_err(),
            "the URL is another pull request's"
        );
    }

    #[test]
    fn checks_are_read_from_check_runs_and_commit_statuses() {
        let raw = r#"[
            {"__typename": "CheckRun", "name": "ci", "status": "COMPLETED", "conclusion": "SUCCESS"},
            {"__typename": "CheckRun", "name": "lint", "status": "COMPLETED", "conclusion": "SKIPPED"},
            {"__typename": "CheckRun", "name": "e2e", "status": "IN_PROGRESS", "conclusion": ""},
            {"__typename": "CheckRun", "name": "fmt", "status": "COMPLETED", "conclusion": "CANCELLED"},
            {"__typename": "StatusContext", "context": "deploy", "state": "PENDING"},
            {"__typename": "StatusContext", "context": "sign", "state": "ERROR"},
            {"__typename": "StatusContext", "context": "cla", "state": "SUCCESS"}
        ]"#;
        let rollup: Vec<Rollup> = serde_json::from_str(raw).unwrap();
        let outcomes: Vec<(String, CheckOutcome)> = rollup
            .iter()
            .map(|r| (r.check().name, r.check().outcome))
            .collect();
        use CheckOutcome::{Failed, Green, Pending};
        let expected = [
            ("ci", Green),
            ("lint", Green),
            ("e2e", Pending),
            ("fmt", Failed),
            ("deploy", Pending),
            ("sign", Failed),
            ("cla", Green),
        ];
        let expected: Vec<(String, CheckOutcome)> = expected
            .iter()
            .map(|(n, o)| ((*n).to_owned(), *o))
            .collect();
        assert_eq!(outcomes, expected);
    }

    fn registered(path: &Path) -> Repo {
        use archon_core::repo::{Attribution, Checks, RepoTiers, ShipMode};
        Repo {
            id: archon_core::id::RepoId::new(),
            name: "r".into(),
            path: path.to_owned(),
            ship: ShipMode::Local,
            base_branch: None,
            default_pipeline: None,
            autonomy_max: Default::default(),
            grant_merge: false,
            merge_waive_checks: Vec::new(),
            attribution: Attribution::None,
            gh_user: None,
            tiers: RepoTiers {
                checks: Checks::default(),
                ..RepoTiers::default()
            },
            confirmed_fast_check: None,
            confirmed_full_check: None,
            confirmed_release_check: None,
            added_at: archon_core::now(),
        }
    }

    /// A `gh` that prints `stdout` and succeeds.
    fn gh_printing(dir: &Path, stdout: &str) -> PathBuf {
        let script = dir.join("gh");
        let body = format!("#!/bin/sh\ncat <<'EOF'\n{stdout}\nEOF\n");
        archon_core::testing::write_script(&script, &body);
        script
    }

    #[tokio::test]
    async fn malformed_or_contradicting_gh_output_is_an_error_not_a_panic() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let repo = registered(&root);
        let closed = r#"[{"number":7,"url":"https://github.com/o/r/pull/7","state":"CLOSED","headRefName":"a/b","baseRefName":"main"}]"#;
        for said in ["not json at all", "[{\"number\":1}]", "{}", closed] {
            let binary = gh_printing(&root, said);
            let found = of_branch(&binary, &repo, "a/b", true).await;
            assert!(
                matches!(found, Err(ArchonError::Protocol(_))),
                "{said}: {found:?}"
            );
        }
        let binary = gh_printing(&root, "{}");
        let facts = merge_facts(&binary, &repo, "https://github.com/o/r/pull/7").await;
        assert!(matches!(facts, Err(ArchonError::Protocol(_))), "{facts:?}");
        let refused = merge_facts(&binary, &repo, "not a url").await;
        assert!(
            matches!(refused, Err(ArchonError::Invalid(_))),
            "{refused:?}"
        );
    }

    #[test]
    fn only_missing_access_blocks_for_the_operator() {
        let denied = "remote: Permission to o/r.git denied to someone.\nfatal: HTTP 403";
        let blocked = needs_operator("pushing `b` to `origin`", denied).unwrap();
        assert!(blocked.starts_with("BLOCKED: pushing `b` to `origin` was refused"));
        let network = "fatal: unable to access 'https://x/': Could not resolve host: x";
        assert_eq!(
            needs_operator("pushing", network),
            None,
            "worth another try"
        );
    }

    #[test]
    fn a_failure_is_told_in_the_tool_s_own_words() {
        let ran = |stdout: &str, stderr: &str| Ran {
            ok: false,
            stdout: stdout.into(),
            stderr: stderr.into(),
        };
        assert_eq!(ran("out\n", " err \n").detail(), "err");
        assert_eq!(
            ran(" out \n", " \n").detail(),
            "out",
            "gh prints some errors to stdout"
        );
        let long = ran("", &"x".repeat(MAX_DETAIL_CHARS * 2));
        assert_eq!(long.detail().chars().count(), MAX_DETAIL_CHARS);
    }

    const SECRET: &str = "gho_FAKEfakeFAKE0123456789";

    /// A `gh` that knows one login, `devfake`, logs each call and prints
    /// the account variables it was given.
    const FAKE_GH: &str = r#"#!/bin/sh
echo "$*" >> "$(dirname "$0")/calls"
case "$*" in
"auth token --hostname github.com --user devfake")
    [ -z "$GH_TOKEN$GITHUB_TOKEN$GH_HOST" ] || exit 9
    echo "@SECRET@"; exit 0 ;;
"auth token "*) echo "no oauth token found for github.com account" >&2; exit 1 ;;
"api echo") echo "HTTP 401: Bad credentials ($GH_TOKEN)" >&2; exit 1 ;;
esac
echo "GH_TOKEN=${GH_TOKEN-unset} GH_HOST=${GH_HOST-unset} GITHUB_TOKEN=${GITHUB_TOKEN-unset}"
"#;

    struct Fake {
        _dir: tempfile::TempDir,
        gh: PathBuf,
        repo: Repo,
    }

    impl Fake {
        fn new(origin: &str, gh_user: Option<&str>) -> Self {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path().canonicalize().unwrap();
            let gh = root.join("gh");
            archon_core::testing::write_script(&gh, &FAKE_GH.replace("@SECRET@", SECRET));
            let path = root.join("repo");
            std::fs::create_dir(&path).unwrap();
            let git = |args: &[&str]| {
                crate::worktree::daemon_git::DaemonGit::sealed_in(&path)
                    .args(args.iter().copied())
                    .text()
                    .unwrap()
            };
            git(&["init", "--quiet"]);
            git(&["remote", "add", "origin", origin]);
            let repo = Repo {
                id: archon_core::id::RepoId::new(),
                name: "r".into(),
                path,
                ship: archon_core::repo::ShipMode::Pr,
                base_branch: None,
                default_pipeline: None,
                autonomy_max: Default::default(),
                grant_merge: false,
                merge_waive_checks: Vec::new(),
                attribution: Default::default(),
                gh_user: gh_user.map(|user| user.parse().unwrap()),
                tiers: Default::default(),
                confirmed_fast_check: None,
                confirmed_full_check: None,
                confirmed_release_check: None,
                added_at: archon_core::now(),
            };
            Self {
                _dir: dir,
                gh,
                repo,
            }
        }

        fn calls(&self) -> String {
            std::fs::read_to_string(self.gh.with_file_name("calls")).unwrap_or_default()
        }
    }

    #[tokio::test]
    async fn every_gh_call_of_a_repository_with_a_login_carries_its_token_alone() {
        let fake = Fake::new("git@github.com:o/r.git", Some("devfake"));
        let said = text(&fake.gh, &fake.repo, &["pr", "list"]).await.unwrap();
        assert_eq!(
            said.trim(),
            "GH_TOKEN=[token] GH_HOST=github.com GITHUB_TOKEN=unset",
            "the token is set for gh and redacted from what it printed"
        );
        let calls = fake.calls();
        assert_eq!(
            calls,
            "auth token --hostname github.com --user devfake\npr list\n"
        );
        assert!(std::env::vars().all(|(_, value)| !value.contains(SECRET)));
    }

    #[tokio::test]
    async fn a_repository_without_a_login_runs_as_the_daemon_s() {
        let fake = Fake::new("https://github.com/o/r.git", None);
        let said = text(&fake.gh, &fake.repo, &["pr", "list"]).await.unwrap();
        assert!(!said.contains(SECRET), "{said}");
        assert_eq!(fake.calls(), "pr list\n", "no token is looked up");
    }

    #[tokio::test]
    async fn a_failure_never_shows_the_token() {
        let fake = Fake::new("https://github.com/o/r.git", Some("devfake"));
        let error = text(&fake.gh, &fake.repo, &["api", "echo"])
            .await
            .unwrap_err();
        let shown = format!("{error} {error:?}");
        assert!(!shown.contains(SECRET), "{shown}");
        assert!(
            shown.contains("BLOCKED") && shown.contains("[token]"),
            "{shown}"
        );
    }

    #[tokio::test]
    async fn a_login_gh_has_no_token_for_blocks_with_the_login_named() {
        let fake = Fake::new("https://github.com/o/r.git", Some("someone-else"));
        let error = text(&fake.gh, &fake.repo, &["pr", "list"])
            .await
            .unwrap_err();
        let shown = error.to_string();
        assert!(
            shown.contains("BLOCKED: `gh` has no token for someone-else"),
            "{shown}"
        );
        assert!(
            !fake.calls().contains("pr list"),
            "nothing ran as another login"
        );
    }

    #[tokio::test]
    async fn a_login_is_used_only_for_a_github_com_origin() {
        let fake = Fake::new("https://ghe.example.com/o/r.git", Some("devfake"));
        let error = text(&fake.gh, &fake.repo, &["pr", "list"])
            .await
            .unwrap_err();
        let shown = error.to_string();
        assert!(shown.contains("BLOCKED: `origin` is") && shown.contains("devfake"));
        assert_eq!(fake.calls(), "", "gh never ran: no token was read");
    }
}
