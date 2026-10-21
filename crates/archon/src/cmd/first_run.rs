//! The first-run offer of a bare `archon` on a terminal: set Archon up,
//! register the current git checkout, then run the doctor. Every step asks
//! first and a "no" skips it; `ARCHON_NO_WIZARD=1` skips the whole offer.

use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};

use anyhow::Context as _;
use archon_core::config::{AetherMode, Config};
use archon_core::protocol::RepoAdd;
use archon_core::repo::ShipMode;

use super::{doctor, instance, repo};
use crate::connect::connect_running;
use crate::instance::Instance;
use crate::views;

const DECLINED_FILE: &str = "declined-repos";

pub const SKIP_ENV: &str = "ARCHON_NO_WIZARD";

const NOT_SET_UP_HINT: &str = "Archon is not set up yet. Run `archon setup`, then `archon` again (or run `archon` on a terminal to be guided).";

/// Whether Archon's own Aether has no config yet (dedicated mode only).
fn needs_setup(config: &Config) -> anyhow::Result<bool> {
    if config.aether.mode != AetherMode::Dedicated {
        return Ok(false);
    }
    Ok(!Instance::resolve()?.config_path().is_file())
}

/// The one next command for a run with no terminal; `None` when set up.
pub fn non_interactive_hint(config: &Config) -> anyhow::Result<Option<&'static str>> {
    Ok(needs_setup(config)?.then_some(NOT_SET_UP_HINT))
}

/// Ask `question`; an empty answer takes `default_yes`, end of input is "no".
fn ask(
    input: &mut impl BufRead,
    out: &mut impl Write,
    question: &str,
    default_yes: bool,
) -> anyhow::Result<bool> {
    let choices = if default_yes { "[Y/n]" } else { "[y/N]" };
    write!(out, "{question} {choices} ")?;
    out.flush()?;
    let mut line = String::new();
    if input.read_line(&mut line)? == 0 {
        writeln!(out)?;
        return Ok(false);
    }
    Ok(match line.trim().to_ascii_lowercase().as_str() {
        "" => default_yes,
        "y" | "yes" => true,
        _ => false,
    })
}

/// A checkout with an `origin` remote ships pull requests; one without keeps
/// its work on a local branch.
fn ship_for(has_origin: bool) -> ShipMode {
    match has_origin {
        true => ShipMode::Pr,
        false => ShipMode::Local,
    }
}

fn ship_name(ship: ShipMode) -> &'static str {
    match ship {
        ShipMode::Pr => "pr",
        ShipMode::Local => "local",
    }
}

fn git_output(dir: &Path, args: &[&str]) -> Option<String> {
    let output = std::process::Command::new("git")
        .current_dir(dir)
        .args(args)
        .output()
        .ok()?;
    let text = String::from_utf8(output.stdout).ok()?;
    (output.status.success() && !text.trim().is_empty()).then(|| text.trim().to_owned())
}

/// The top level of the git checkout holding `dir`, with whether it has an
/// `origin` remote.
fn checkout_of(dir: &Path) -> Option<(PathBuf, bool)> {
    let top = PathBuf::from(git_output(dir, &["rev-parse", "--show-toplevel"])?);
    let origin = git_output(&top, &["remote", "get-url", "origin"]).is_some();
    Some((top, origin))
}

/// Whether the daemon, if it runs, already lists `top`. A daemon that is
/// not running lists nothing, so the checkout is offered.
async fn is_registered(top: &Path) -> bool {
    let Ok(client) = connect_running().await else {
        return false;
    };
    match views::fetch_repos(&client).await {
        Ok(repos) => repos.iter().any(|r| r.path == top),
        Err(_) => false,
    }
}

/// Whether the terminal UI may open: `false` when the user declined setup.
pub async fn offer(config: &Config) -> anyhow::Result<bool> {
    if std::env::var_os(SKIP_ENV).is_some() {
        return Ok(true);
    }
    let stdin = std::io::stdin();
    let (mut input, mut out) = (stdin.lock(), std::io::stdout());
    let mut changed = false;
    if needs_setup(config)? {
        let question = "Archon is not set up yet. Set up its own Aether now \
                        (writes its config under ~/.config/archon)?";
        if !ask(&mut input, &mut out, question, true)? {
            println!("Skipped. `archon setup` does it later; then run `archon` again.");
            return Ok(false);
        }
        instance::setup(false, false, false).await?;
        changed = true;
    }
    if doctor::find_program(Path::new("claude")).is_none()
        && doctor::find_program(Path::new("codex")).is_none()
    {
        println!(
            "Neither `claude` nor `codex` is on PATH, so agents cannot run yet. \
             Install one and log in by running it once, then run `archon` again."
        );
    }
    changed |= offer_repo(&mut input, &mut out).await?;
    if changed {
        println!("Checking the setup:");
        doctor::run(false).await?;
    }
    Ok(true)
}

/// Checkouts whose registration the user declined, one path per line, in
/// Archon's data directory (never in the repository).
fn declined_path() -> PathBuf {
    archon_core::paths::data_dir().join(DECLINED_FILE)
}

fn is_declined(file: &Path, top: &Path) -> anyhow::Result<bool> {
    assert!(top.is_absolute(), "declined checkouts are absolute paths");
    match std::fs::read_to_string(file) {
        Ok(text) => Ok(text.lines().any(|line| Path::new(line) == top)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error).with_context(|| format!("reading {}", file.display())),
    }
}

fn remember_declined(file: &Path, top: &Path) -> anyhow::Result<()> {
    use std::io::Write as _;
    assert!(top.is_absolute(), "declined checkouts are absolute paths");
    anyhow::ensure!(
        !top.to_string_lossy().contains('\n'),
        "{} has a newline in its name",
        top.display()
    );
    let dir = file
        .parent()
        .context("the declined list has no directory")?;
    crate::instance::create_private_dir(&std::path::absolute(dir)?)?;
    let mut list = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(file)
        .with_context(|| format!("opening {}", file.display()))?;
    writeln!(list, "{}", top.display()).with_context(|| format!("writing {}", file.display()))
}

async fn offer_repo(input: &mut impl BufRead, out: &mut impl Write) -> anyhow::Result<bool> {
    let Some((top, has_origin)) = checkout_of(&std::env::current_dir()?) else {
        return Ok(false);
    };
    assert!(top.is_absolute(), "git reports an absolute top level");
    let declined = declined_path();
    match is_declined(&declined, &top) {
        Ok(true) => return Ok(false),
        Ok(false) => {}
        Err(error) => eprintln!("archon: the declined-checkouts list is unreadable: {error:#}"),
    }
    if is_registered(&top).await {
        return Ok(false);
    }
    let ship = ship_for(has_origin);
    let why = match has_origin {
        true => "it has an origin remote",
        false => "it has no origin remote",
    };
    let question = format!(
        "Register {} so Archon can work on it (ship: {}, because {why})?",
        top.display(),
        ship_name(ship)
    );
    if !ask(input, out, &question, false)? {
        match remember_declined(&declined, &top) {
            Ok(()) => println!(
                "Not registered, and not asked again here. `archon repo add .` registers it later."
            ),
            Err(error) => println!(
                "Not registered (could not remember that: {error:#}). `archon repo add .` registers it later."
            ),
        }
        return Ok(false);
    }
    let add = RepoAdd {
        path: top,
        name: None,
        ship: Some(ship),
        base_branch: None,
        default_pipeline: None,
        auto_merge: false,
        github_watch: false,
    };
    repo::add_repo(add, false).await?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn answer(text: &str, default_yes: bool) -> bool {
        ask(
            &mut Cursor::new(text.to_owned()),
            &mut Vec::new(),
            "ok?",
            default_yes,
        )
        .unwrap()
    }

    #[test]
    fn only_a_yes_agrees_when_the_default_is_no() {
        assert!(answer("y\n", false));
        assert!(answer("YES\n", false));
        assert!(!answer("\n", false));
        assert!(!answer("maybe\n", false));
    }

    #[test]
    fn an_empty_answer_takes_the_default_and_closed_input_declines() {
        assert!(answer("\n", true));
        assert!(!answer("n\n", true));
        assert!(!answer("", true), "end of input is never a yes");
    }

    #[test]
    fn the_prompt_shows_which_answer_is_the_default() {
        let mut out = Vec::new();
        ask(&mut Cursor::new("\n"), &mut out, "Go?", false).unwrap();
        assert_eq!(String::from_utf8(out).unwrap(), "Go? [y/N] ");
    }

    #[test]
    fn ship_mode_follows_the_origin_remote() {
        assert_eq!(ship_for(true), ShipMode::Pr);
        assert_eq!(ship_for(false), ShipMode::Local);
    }

    #[test]
    fn a_declined_checkout_is_remembered_by_path_and_others_are_still_offered() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("state").join(DECLINED_FILE);
        let (one, two) = (Path::new("/work/one"), Path::new("/work/one-more"));
        assert!(!is_declined(&file, one).unwrap(), "no file yet");
        remember_declined(&file, one).unwrap();
        assert!(is_declined(&file, one).unwrap());
        assert!(!is_declined(&file, two).unwrap(), "a prefix is not a match");
        remember_declined(&file, two).unwrap();
        assert!(is_declined(&file, one).unwrap() && is_declined(&file, two).unwrap());
    }

    #[test]
    fn the_hint_names_the_one_next_command() {
        assert!(NOT_SET_UP_HINT.contains("`archon setup`"));
        assert!(!NOT_SET_UP_HINT.contains('\n'));
    }
}
