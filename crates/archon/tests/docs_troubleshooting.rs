//! `docs/troubleshooting.md` quotes every blocker text archond posts. Each
//! quoted phrase is checked against its source and the doc, and every daemon
//! file that posts a `Blocker` must be covered, so the doc cannot fall behind.

use std::path::{Path, PathBuf};

const ROOT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../..");
const DOC: &str = "docs/troubleshooting.md";
const DAEMON_SRC: &str = "crates/archon-daemon/src";
const BLOCKER_TOKEN: &str = "NotificationKind::Blocker";

/// (source file, phrase): the phrase is literal text in the source and is
/// quoted verbatim in the doc.
const BLOCKERS: &[(&str, &str)] = &[
    (
        "crates/archon-daemon/src/lib.rs",
        "restarted every time. Everything else keeps running.",
    ),
    (
        "crates/archon-daemon/src/lib.rs",
        "switched off. Everything else keeps running.",
    ),
    (
        "crates/archon-core/src/crash.rs",
        "A panic archond isolated leaves it running",
    ),
    (
        "crates/archon-core/src/config_files.rs",
        "it is the config file of Archon before 1.0",
    ),
    (
        "crates/archon-daemon/src/dispatcher/disk_hold.rs",
        "Dispatch is held:",
    ),
    (
        "crates/archon-core/src/host_load.rs",
        "is above [limits] max_load_per_cpu",
    ),
    (
        "crates/archon-core/src/host_load.rs",
        "is below [limits] min_available_memory_gb",
    ),
    (
        "crates/archon-daemon/src/dispatcher.rs",
        "agents at once, but the caps now allow",
    ),
    ("crates/archon-daemon/src/dispatcher.rs", "Blocked:"),
    (
        "crates/archon-daemon/src/dispatcher.rs",
        "No new work starts; running agents finish.",
    ),
    (
        "crates/archon-daemon/src/handlers/waiting.rs",
        "Daily budget reached",
    ),
    (
        "crates/archon-daemon/src/handlers/waiting.rs",
        "Total budget reached",
    ),
    (
        "crates/archon-core/src/cost.rs",
        "the item's budget is used up",
    ),
    (
        "crates/archon-daemon/src/executor.rs",
        "cannot enforce the item's dollar budget",
    ),
    (
        "crates/archon-daemon/src/executor/spawn.rs",
        "and a budget:",
    ),
    (
        "crates/archon-daemon/src/executor/finish.rs",
        "Full detail: archon queue show",
    ),
    (
        "crates/archon-daemon/src/executor/start.rs",
        "queues it again once fixed",
    ),
    (
        "crates/archon-daemon/src/executor/recover.rs",
        "cannot resume the run:",
    ),
    (
        "crates/archon-daemon/src/executor/spawn.rs",
        "create_agent failed on a full disk",
    ),
    (
        "crates/archon-daemon/src/executor/spawn.rs",
        "refused at spawn",
    ),
    (
        "crates/archon-daemon/src/executor/descendants.rs",
        "so the run does not go on beside them",
    ),
    (
        "crates/archon-daemon/src/executor/descendants.rs",
        "still run in Aether after their cancel",
    ),
    (
        "crates/archon-daemon/src/executor/team.rs",
        "merging the team's branches failed",
    ),
    (
        "crates/archon-daemon/src/executor/review/rounds.rs",
        "the lead is still fixing it",
    ),
    (
        "crates/archon-daemon/src/executor/coordination.rs",
        "tries again on every maintenance pass",
    ),
    (
        "crates/archon-daemon/src/executor/ship/remote.rs",
        "has no `origin` remote to push to",
    ),
    (
        "crates/archon-daemon/src/executor/ship/existing.rs",
        "refused the push of",
    ),
    (
        "crates/archon-daemon/src/executor/ship/existing.rs",
        "Point `origin` back at",
    ),
    (
        "crates/archon-daemon/src/executor/gh.rs",
        "has no token for",
    ),
    (
        "crates/archon-daemon/src/executor/gh.rs",
        "was refused for want of access",
    ),
    (
        "crates/archon-daemon/src/executor/ship/auto_merge.rs",
        "Auto-merge could not be enabled for",
    ),
    (
        "crates/archon-daemon/src/executor/operator_merge.rs",
        "Not merged:",
    ),
    (
        "crates/archon-daemon/src/executor/ship/pr_checks.rs",
        "This is not a failure of the change; fix the GitHub Actions account or runners",
    ),
    (
        "crates/archon-daemon/src/executor/restack.rs",
        "`origin` rejected the push as not a fast-forward",
    ),
    ("crates/archon-daemon/src/integration/fix.rs", "is red at"),
    (
        "crates/archon-core/src/integration.rs",
        "merging it waits for the operator",
    ),
    (
        "crates/archon-daemon/src/pump/aetherd.rs",
        "Archon's own aetherd is not running, and archond could not start it",
    ),
    (
        "crates/archon-daemon/src/supervisor/revival.rs",
        "Archon revives it again at",
    ),
    (
        "crates/archon-daemon/src/supervisor/revival.rs",
        "Archon keeps reviving it, waiting longer each time",
    ),
    (
        "crates/archon-daemon/src/supervisor/stall.rs",
        "minutes without a message, tool result or usage update",
    ),
    (
        "crates/archon-daemon/src/supervisor.rs",
        "Archon will not start the orchestrator:",
    ),
    (
        "crates/archon-daemon/src/supervisor/mcp.rs",
        "Archon could not bring back the archon MCP server in Aether",
    ),
    (
        "crates/archon-daemon/src/supervisor/mcp.rs",
        "could not restart the orchestrator to give it Archon's tools back",
    ),
    (
        "crates/archon-daemon/src/supervisor/callers.rs",
        "Aether is not naming the agents that call Archon's tools",
    ),
    (
        "crates/archon-daemon/src/supervisor/retired.rs",
        "record of replaced orchestrators was unreadable",
    ),
    (
        "crates/archon-daemon/src/pump/watchdog.rs",
        "Cancelled agent",
    ),
    (
        "crates/archon/src/cmd/first_run.rs",
        "is on PATH, so agents cannot run yet",
    ),
    (
        "crates/archon/src/cmd/doctor.rs",
        "`archon aether login <provider>` if Archon needs it",
    ),
    (
        "crates/archon-daemon/src/handlers/waiting.rs",
        "Waiting for provider quota: every provider stage",
    ),
    (
        "crates/archon/src/cmd/instance.rs",
        "try `archon tui` again after Aether reconnects",
    ),
    (
        "crates/archon-client/src/connection.rs",
        "archond did not answer",
    ),
    (
        "crates/archon/src/cmd/update/restart.rs",
        "gave up the restart: stage agents still ran after",
    ),
    (
        "crates/archon/src/cmd/update/restart.rs",
        "see its startup log with",
    ),
];

/// Daemon files that name the kind without carrying a blocker text of their own.
const EXEMPT: &[(&str, &str)] = &[
    (
        "crates/archon-daemon/src/notification_center/posts.rs",
        "maps one kind enum to another",
    ),
    (
        "crates/archon-daemon/src/reports.rs",
        "filters feed entries by kind",
    ),
    (
        "crates/archon-daemon/src/triggers.rs",
        "an escalation is posted with the text the orchestrator wrote",
    ),
    (
        "crates/archon-daemon/src/integration/promote.rs",
        "its text is built by Promotion::describe in crates/archon-core/src/integration.rs",
    ),
];

fn read(relative: &str) -> String {
    let path = Path::new(ROOT).join(relative);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("reading {}: {e}", path.display()))
}

/// Joins Rust string continuations (`\` + newline + indentation) and
/// collapses every whitespace run, so source and markdown compare equal.
fn normalised(text: &str) -> String {
    let mut joined = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find("\\\n") {
        joined.push_str(&rest[..at]);
        rest = rest[at + 2..].trim_start();
    }
    joined.push_str(rest);
    joined.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The file without its `#[cfg(test)]` module, which runs to the end.
fn production_code(text: &str) -> &str {
    text.find("#[cfg(test)]").map_or(text, |at| &text[..at])
}

fn daemon_sources(dir: &Path, found: &mut Vec<PathBuf>) {
    let entries = std::fs::read_dir(dir).unwrap_or_else(|e| panic!("{}: {e}", dir.display()));
    for entry in entries {
        let path = entry.expect("a readable directory entry").path();
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if path.is_dir() && name != "tests" {
            daemon_sources(&path, found);
        } else if name.ends_with(".rs") && name != "tests.rs" {
            found.push(path);
        }
    }
}

fn relative(path: &Path) -> String {
    let root = Path::new(ROOT).join(DAEMON_SRC);
    let inside = path.strip_prefix(&root).expect("under the daemon sources");
    format!("{DAEMON_SRC}/{}", inside.display())
}

#[test]
fn every_blocker_phrase_is_in_its_source_and_the_doc() {
    assert!(BLOCKERS.len() >= 40, "the table lists the blocker texts");
    let doc = normalised(&read(DOC));
    let mut missing = Vec::new();
    for (source, phrase) in BLOCKERS {
        assert!(!phrase.trim().is_empty(), "{source}: an empty phrase");
        if !normalised(&read(source)).contains(phrase) {
            missing.push(format!("{source} does not contain {phrase:?}"));
        }
        if !doc.contains(phrase) {
            missing.push(format!("{DOC} does not quote {phrase:?}"));
        }
    }
    assert!(missing.is_empty(), "stale entries:\n{}", missing.join("\n"));
}

#[test]
fn every_daemon_file_that_posts_a_blocker_is_covered() {
    let mut files = Vec::new();
    daemon_sources(&Path::new(ROOT).join(DAEMON_SRC), &mut files);
    assert!(files.len() > 50, "the walk found the daemon sources");
    let mut uncovered = Vec::new();
    for file in files {
        let name = relative(&file);
        let text = std::fs::read_to_string(&file).expect("a readable source file");
        if !production_code(&text).contains(BLOCKER_TOKEN) {
            continue;
        }
        let listed = BLOCKERS.iter().any(|(source, _)| *source == name);
        let exempt = EXEMPT.iter().any(|(source, _)| *source == name);
        if !listed && !exempt {
            uncovered.push(name);
        }
    }
    assert!(
        uncovered.is_empty(),
        "these files post a Blocker; add their text to BLOCKERS and {DOC}, or to EXEMPT:\n{}",
        uncovered.join("\n")
    );
}

#[test]
fn every_exemption_names_a_file_that_still_posts_a_blocker() {
    assert!(!EXEMPT.is_empty(), "the exemptions are listed");
    for (source, reason) in EXEMPT {
        assert!(!reason.is_empty(), "{source}: an exemption says why");
        let text = read(source);
        assert!(
            production_code(&text).contains(BLOCKER_TOKEN),
            "{source} no longer names {BLOCKER_TOKEN}; drop its exemption"
        );
        assert!(
            BLOCKERS.iter().all(|(listed, _)| listed != source),
            "{source} is exempt and listed"
        );
    }
}
