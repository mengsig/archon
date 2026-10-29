//! What finished work may leave running: Docker Compose stacks in the
//! worktrees of items that ended, and `lavish-axi poll` processes for
//! reports archond no longer follows. Each check names the command that
//! clears what it found.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use archon_client::ArchonClient;
use archon_core::config::Config;
use archon_core::id::QueueItemId;
use archon_core::protocol::{ErrorCode, Reply, ReportState, Request};

use super::registry::COMMAND_TIMEOUT;
use super::{Check, Level, check};

type Archond = anyhow::Result<Option<ArchonClient>>;

/// Compose projects with containers, running or stopped, from an Archon
/// worktree whose item ended, was pruned, or whose state archond cannot tell.
pub(super) async fn check_compose(config: &Config, worktrees: &Path, archond: &Archond) -> Check {
    assert!(worktrees.is_absolute(), "the worktree root is absolute");
    let Some(listing) = archon_daemon::compose::list(&config.worktrees.docker).await else {
        return check("compose stacks", Level::Ok, "docker is not available");
    };
    let selection = archon_daemon::compose::select(&listing, &[worktrees.to_path_buf()]);
    let mut stale = Vec::new();
    for (project, found) in &selection.inside {
        let Some(item) = item_of(&found.dir, worktrees) else {
            continue;
        };
        if let Some(why) = left_behind(archond, &item).await {
            stale.push(format!(
                "{project} ({item} {why}): {}",
                archon_daemon::compose::down_command(project)
            ));
        }
    }
    if stale.is_empty() {
        return check("compose stacks", Level::Ok, "none left by finished items");
    }
    // Stopped containers count: a restart policy starts them with Docker.
    let detail = format!("left by finished items: {}", stale.join("; "));
    check("compose stacks", Level::Warn, detail)
}

/// The item whose worktree `dir` lies in: the first `qi_` directory below
/// `worktrees`.
fn item_of(dir: &Path, worktrees: &Path) -> Option<QueueItemId> {
    let below = dir.strip_prefix(worktrees).ok()?;
    below
        .components()
        .filter_map(|c| c.as_os_str().to_str())
        .find_map(|name| QueueItemId::parse(name).ok())
}

/// Why `item`'s stacks should no longer run; `None` while it is live.
async fn left_behind(archond: &Archond, item: &QueueItemId) -> Option<&'static str> {
    let Ok(Some(client)) = archond else {
        return Some("unknown: archond is not running");
    };
    let call = client.call(Request::GetItem { item: item.clone() });
    match tokio::time::timeout(COMMAND_TIMEOUT, call).await {
        Ok(Ok(Ok(Reply::ItemDetail(detail)))) => {
            assert_eq!(&detail.item.id, item, "archond answers for the item asked");
            detail.item.state.is_terminal().then_some("ended")
        }
        Ok(Ok(Err(refusal))) if refusal.code == ErrorCode::NotFound => Some("was pruned"),
        _ => Some("unknown: archond did not say"),
    }
}

/// One running `lavish-axi poll`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Poll {
    pub pid: u32,
    pub html: PathBuf,
    /// Its parent is an `archon daemon`.
    pub by_archond: bool,
}

/// `lavish-axi poll` processes of Archon's beyond one per report archond
/// follows: started by archond, or polling a page under its data directory.
pub(super) async fn check_lavish_polls(data_dir: &Path, archond: &Archond) -> Check {
    let polls: Vec<Poll> = running_polls()
        .into_iter()
        .filter(|poll| poll.by_archond || poll.html.starts_with(data_dir))
        .collect();
    if polls.is_empty() {
        return check("lavish polls", Level::Ok, "none running");
    }
    let Some(live) = followed_reports(archond).await else {
        let detail = format!("{} running; archond did not list its reports", polls.len());
        return check("lavish polls", Level::Warn, detail);
    };
    let strays = strays(&polls, &live);
    if strays.is_empty() {
        let detail = format!("{} running, one per followed report", polls.len());
        return check("lavish polls", Level::Ok, detail);
    }
    let pids: Vec<String> = strays.iter().map(u32::to_string).collect();
    let detail = format!(
        "{} beyond the {} reports archond follows: kill {}",
        strays.len(),
        live.len(),
        pids.join(" ")
    );
    check("lavish polls", Level::Warn, detail)
}

/// The pages of the open reports archond follows; empty when it is not
/// running, `None` when it did not answer.
async fn followed_reports(archond: &Archond) -> Option<BTreeSet<PathBuf>> {
    let Ok(Some(client)) = archond else {
        return Some(BTreeSet::new());
    };
    let call = client.call(Request::ListReports);
    let Ok(Ok(Ok(Reply::Reports(reports)))) = tokio::time::timeout(COMMAND_TIMEOUT, call).await
    else {
        return None;
    };
    let open = reports.into_iter().filter(|r| r.state == ReportState::Open);
    Some(open.map(|r| r.path).collect())
}

/// The pids of polls beyond the first of each page in `live`, oldest kept.
pub(super) fn strays(polls: &[Poll], live: &BTreeSet<PathBuf>) -> Vec<u32> {
    let mut by_page: BTreeMap<&Path, Vec<u32>> = BTreeMap::new();
    for poll in polls {
        by_page.entry(&poll.html).or_default().push(poll.pid);
    }
    let mut strays = Vec::new();
    for (page, mut pids) in by_page {
        pids.sort_unstable();
        let keep = usize::from(live.contains(page));
        strays.extend(pids.into_iter().skip(keep));
    }
    strays.sort_unstable();
    assert!(strays.len() <= polls.len(), "only running polls are strays");
    strays
}

/// The page a process polls, from its arguments: `… lavish-axi poll <html>`.
pub(super) fn polled_page(args: &[String]) -> Option<PathBuf> {
    let at = args.iter().position(|arg| {
        Path::new(arg)
            .file_name()
            .is_some_and(|n| n == "lavish-axi")
    })?;
    match &args[at + 1..] {
        [command, html, ..] if command == "poll" && Path::new(html).is_absolute() => {
            Some(PathBuf::from(html))
        }
        _ => None,
    }
}

/// Every `lavish-axi poll` running now; none off Linux.
fn running_polls() -> Vec<Poll> {
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    let mut polls = Vec::new();
    for entry in entries.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|n| n.parse::<u32>().ok())
        else {
            continue;
        };
        let Some(html) = polled_page(&cmdline(pid)) else {
            continue;
        };
        let by_archond = parent_of(pid).is_some_and(|parent| is_archond(&cmdline(parent)));
        polls.push(Poll {
            pid,
            html,
            by_archond,
        });
    }
    polls
}

/// A process's arguments; empty once it is gone.
fn cmdline(pid: u32) -> Vec<String> {
    let Ok(raw) = std::fs::read(format!("/proc/{pid}/cmdline")) else {
        return Vec::new();
    };
    raw.split(|byte| *byte == 0)
        .filter(|arg| !arg.is_empty())
        .map(|arg| String::from_utf8_lossy(arg).into_owned())
        .collect()
}

/// The parent pid, the field after the parenthesised name in `stat`.
fn parent_of(pid: u32) -> Option<u32> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let after_name = &stat[stat.rfind(')')? + 1..];
    after_name.split_whitespace().nth(1)?.parse().ok()
}

/// `archon daemon …`, as archond runs.
pub(super) fn is_archond(args: &[String]) -> bool {
    let Some(binary) = args.first() else {
        return false;
    };
    let named = Path::new(binary).file_name().is_some_and(|n| n == "archon");
    named && args.get(1).is_some_and(|arg| arg == "daemon")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(line: &str) -> Vec<String> {
        line.split(' ').map(str::to_owned).collect()
    }

    #[test]
    fn a_poll_is_told_by_its_arguments() {
        let node = args("node /n/bin/lavish-axi poll /d/orchestrator/pages/merged-pr-69.html");
        let page = PathBuf::from("/d/orchestrator/pages/merged-pr-69.html");
        assert_eq!(polled_page(&node), Some(page));
        assert_eq!(polled_page(&args("/n/bin/lavish-axi /d/page.html")), None);
        assert_eq!(polled_page(&args("lavish-axi poll relative.html")), None);
        assert_eq!(polled_page(&args("lavish-axi poll")), None);
        assert!(is_archond(&args("/usr/bin/archon daemon run")));
        assert!(!is_archond(&args("/usr/bin/archon chat")));
    }

    #[test]
    fn strays_are_polls_of_unfollowed_pages_and_duplicates() {
        let poll = |pid, html: &str| Poll {
            pid,
            html: PathBuf::from(html),
            by_archond: true,
        };
        let polls = [
            poll(30, "/d/live.html"),
            poll(12, "/d/live.html"),
            poll(40, "/d/old.html"),
            poll(50, "/d/other-live.html"),
        ];
        let live = BTreeSet::from([
            PathBuf::from("/d/live.html"),
            PathBuf::from("/d/other-live.html"),
        ]);
        assert_eq!(strays(&polls, &live), vec![30, 40]);
        assert_eq!(strays(&polls, &BTreeSet::new()), vec![12, 30, 40, 50]);
    }

    #[test]
    fn a_stack_belongs_to_the_item_whose_directory_holds_it() {
        let item = QueueItemId::new();
        let root = Path::new("/d/worktrees");
        let dir = root.join("app").join(item.as_str()).join("services");
        assert_eq!(item_of(&dir, root), Some(item));
        assert_eq!(item_of(Path::new("/d/worktrees/app/scratch"), root), None);
        assert_eq!(item_of(Path::new("/home/op/app"), root), None);
    }
}
