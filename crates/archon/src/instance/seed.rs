//! `archon setup` in dedicated mode: seed the instance's `config.toml` from
//! the operator's Aether config, share their provider settings by link, and
//! copy their `voice.toml`. Nothing in the operator's directory is ever written.

use std::path::{Path, PathBuf};

use anyhow::Context as _;
use archon_core::config::CONFIG_FILE_NAME;
use toml::{Table, Value};

use super::{Instance, create_private_dir};
use crate::setup::{self, Registration};

pub mod calm;
pub mod voice;

/// Linked. Aether only reads `providers.toml`, saves `ui.toml` by rename (which
/// forks the link harmlessly); themes and installed voices share directories.
pub const LINKED: [&str; 4] = ["providers.toml", "ui.toml", "voice", "themes"];
/// Never linked: Aether saves it by rename under a lock file beside it, so a
/// link would fork silently on the first token refresh (ARCHITECTURE §4.7).
pub const CREDENTIALS: &str = "credentials.json";
/// Sections that bind the operator's ports or address the operator's agents.
const DROPPED: [&str; 2] = ["listen", "serve"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigOutcome {
    Seeded,
    Resynced,
    /// Seeded before and declares the archon server: left as it is.
    Unchanged,
    /// Seeded by an older Archon: its archon server was brought up to date.
    Repaired,
    /// Seeded before but without the archon server, which was appended.
    BlockAdded,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkState {
    Linked,
    /// The operator has the file; the instance has no link yet.
    Missing,
    /// The operator has no such file, so there is nothing to share.
    NoSource,
    /// The link's target is gone.
    Dangling,
    /// A link to somewhere other than the operator's file.
    Elsewhere(PathBuf),
    /// A real file (Aether replaced the link, or someone put it there).
    OwnFile,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Seeding {
    pub config: ConfigOutcome,
    pub links: Vec<(&'static str, LinkState)>,
    pub voice: voice::VoiceOutcome,
    pub calm: &'static str,
}

/// Seed (or with `resync`, re-seed) the instance config, its links and its
/// own `voice.toml`, which no resync replaces.
pub fn seed(
    instance: &Instance,
    operator_dir: &Path,
    archon_bin: &Path,
    resync: bool,
) -> anyhow::Result<Seeding> {
    anyhow::ensure!(
        !same_dir(operator_dir, &instance.config_dir),
        "Aether's config directory {} is Archon's own instance; unset AETHER_CONFIG_DIR",
        operator_dir.display()
    );
    assert!(operator_dir.is_absolute(), "links need an absolute target");
    create_private_dir(&instance.config_dir)?;
    let config = seed_config(instance, operator_dir, archon_bin, resync)?;
    let mut links = Vec::with_capacity(LINKED.len());
    for name in LINKED {
        links.push((name, ensure_link(operator_dir, &instance.config_dir, name)?));
    }
    assert_eq!(links.len(), LINKED.len(), "every shared file is reported");
    let voice = voice::seed(operator_dir, &instance.config_dir)?;
    Ok(Seeding {
        config,
        links,
        voice,
        calm: calm::seed(&instance.config_dir)?,
    })
}

fn seed_config(
    instance: &Instance,
    operator_dir: &Path,
    archon_bin: &Path,
    resync: bool,
) -> anyhow::Result<ConfigOutcome> {
    let target = instance.config_path();
    let source = operator_dir.join(CONFIG_FILE_NAME);
    assert_ne!(target, source, "the operator's config is never written");
    let exists = target
        .try_exists()
        .with_context(|| format!("checking {}", target.display()))?;
    if exists && !resync {
        return Ok(match setup::register(&target, archon_bin)? {
            Registration::Added => ConfigOutcome::BlockAdded,
            Registration::AlreadyPresent(_) if repair_registration(instance)? => {
                ConfigOutcome::Repaired
            }
            Registration::AlreadyPresent(_) => ConfigOutcome::Unchanged,
        });
    }
    if exists && registration_is_stale(instance)? {
        owe_reload(instance)?;
    }
    let operator_text = read_optional(&source)?;
    let mut server = match existing_archon_server(&target)? {
        Some(server) => server,
        None => fresh_archon_server(archon_bin)?,
    };
    setup::migrate_archon_server(&mut server)?;
    let text = seeded_text(&operator_text, &source, server)?;
    write_atomically(&target, &text)?;
    Ok(if exists {
        ConfigOutcome::Resynced
    } else {
        ConfigOutcome::Seeded
    })
}

/// Whether the instance config's `archon` server lacks caller naming, as an
/// older setup wrote it. `false` for one that does not parse: doctor reports.
pub fn registration_is_stale(instance: &Instance) -> anyhow::Result<bool> {
    let path = instance.config_path();
    let table: Table = match toml::from_str(&read_optional(&path)?) {
        Ok(table) => table,
        Err(error) => {
            tracing::warn!(path = %path.display(), %error, "instance config unreadable; not repaired");
            return Ok(false);
        }
    };
    Ok(setup::archon_server(&table).is_some_and(|server| !setup::names_callers(server)))
}

/// Bring a stale `archon` server entry up to date, keeping its other
/// settings. `true` when the file was rewritten: a reload is then owed.
pub fn repair_registration(instance: &Instance) -> anyhow::Result<bool> {
    if !registration_is_stale(instance)? {
        return Ok(false);
    }
    owe_reload(instance)?;
    let path = instance.config_path();
    let text = read_optional(&path)?;
    let mut table: Table = toml::from_str(&text)
        .with_context(|| format!("{} stopped parsing while it was repaired", path.display()))?;
    let server = mcp_servers_mut(&mut table)?
        .iter_mut()
        .find(|server| setup::is_archon_server(server))
        .context("the stale archon server is gone")?;
    setup::migrate_archon_server(server)?;
    let body = toml::to_string(&table).context("serializing the repaired Aether config")?;
    let repaired = format!("{}{body}", leading_comments(&text));
    let reparsed: Table =
        toml::from_str(&repaired).context("the repaired Aether config does not parse")?;
    anyhow::ensure!(
        setup::archon_server(&reparsed).is_some_and(setup::names_callers),
        "the repaired Aether config must name the archon server's callers"
    );
    write_atomically(&path, &repaired)?;
    Ok(true)
}

/// Record, before the registration on disk changes, that an aetherd which
/// answers has the old one loaded: `lifecycle::ensure_running` restarts it.
fn owe_reload(instance: &Instance) -> anyhow::Result<()> {
    let marker = instance.reload_marker_path();
    create_private_dir(&instance.archon_data_dir)?;
    std::fs::write(&marker, b"").with_context(|| format!("writing {}", marker.display()))
}

/// Whether the registration changed since Archon's aetherd last started.
pub fn reload_is_owed(instance: &Instance) -> anyhow::Result<bool> {
    let marker = instance.reload_marker_path();
    marker
        .try_exists()
        .with_context(|| format!("checking {}", marker.display()))
}

/// No aetherd runs with the old registration any more.
pub fn reload_settled(instance: &Instance) -> anyhow::Result<()> {
    let marker = instance.reload_marker_path();
    match std::fs::remove_file(&marker) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(anyhow::Error::new(e).context(format!("removing {}", marker.display()))),
    }
}

/// The comment block a config starts with, which serializing its table drops.
fn leading_comments(text: &str) -> String {
    text.lines()
        .take_while(|line| line.starts_with('#') || line.trim().is_empty())
        .map(|line| format!("{line}\n"))
        .collect()
}

/// The operator's config with the archon server as its only `archon`
/// entry, and without the sections only the operator's daemon may use.
pub fn seeded_text(
    operator_text: &str,
    source: &Path,
    archon_server: Value,
) -> anyhow::Result<String> {
    assert!(
        setup::is_archon_server(&archon_server),
        "the server to add is Archon's"
    );
    let mut table: Table = toml::from_str(operator_text).with_context(|| {
        format!(
            "{} is not valid TOML; not seeding from it",
            source.display()
        )
    })?;
    for section in DROPPED {
        table.remove(section);
    }
    let servers = mcp_servers_mut(&mut table)
        .with_context(|| format!("{}: cannot add the archon MCP server", source.display()))?;
    servers.retain(|server| !setup::is_archon_server(server));
    servers.push(archon_server);
    let body = toml::to_string(&table).context("serializing the seeded Aether config")?;
    let text = format!(
        "# Archon's own Aether instance, generated by `archon setup` from\n\
         # {}. Edit that file, then run `archon setup --resync`.\n\
         # Left out: [listen] and [serve], which belong to the operator's aetherd.\n\n{body}",
        source.display()
    );
    let reparsed: Table =
        toml::from_str(&text).context("the seeded Aether config does not parse")?;
    anyhow::ensure!(
        archon_server_count(&reparsed) == 1,
        "the seeded Aether config must declare exactly one archon server"
    );
    Ok(text)
}

fn mcp_servers_mut(table: &mut Table) -> anyhow::Result<&mut Vec<Value>> {
    let mcp = table
        .entry("mcp")
        .or_insert_with(|| Value::Table(Table::new()))
        .as_table_mut()
        .context("`mcp` is not a table")?;
    mcp.entry("servers")
        .or_insert_with(|| Value::Array(Vec::new()))
        .as_array_mut()
        .context("`mcp.servers` is not a list")
}

fn archon_server_count(table: &Table) -> usize {
    let servers = setup::mcp_servers(table);
    servers
        .iter()
        .filter(|s| setup::is_archon_server(s))
        .count()
}

/// The archon server block `archon setup` writes for this executable.
fn fresh_archon_server(archon_bin: &Path) -> anyhow::Result<Value> {
    let block: Table = toml::from_str(&setup::mcp_block(archon_bin))
        .context("the archon MCP block does not parse")?;
    let server = setup::archon_server(&block)
        .cloned()
        .context("the archon MCP block declares no archon server")?;
    assert_eq!(
        setup::mcp_servers(&block).len(),
        1,
        "the block declares one server"
    );
    Ok(server)
}

/// The archon server the instance config already declares, kept by a
/// resync so edits to it survive.
fn existing_archon_server(path: &Path) -> anyhow::Result<Option<Value>> {
    let text = read_optional(path)?;
    let table: Table = toml::from_str(&text).with_context(|| {
        format!(
            "{} is not valid TOML; remove it and run `archon setup`",
            path.display()
        )
    })?;
    Ok(setup::archon_server(&table).cloned())
}

/// A file's text; a missing file reads as empty.
fn read_optional(path: &Path) -> anyhow::Result<String> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(text),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(e) => Err(anyhow::Error::new(e).context(format!("reading {}", path.display()))),
    }
}

/// Replace `path` by rename, so a reader never sees half a file.
pub(crate) fn write_atomically(path: &Path, text: &str) -> anyhow::Result<()> {
    let dir = path.parent().context("the path has no directory")?;
    let name = path.file_name().context("the path has no file name")?;
    assert!(dir.is_dir(), "the directory is created first");
    let temp = dir.join(format!(".{}.{}.tmp", name.display(), std::process::id()));
    std::fs::write(&temp, text).with_context(|| format!("writing {}", temp.display()))?;
    if let Err(error) = std::fs::rename(&temp, path) {
        let left = match std::fs::remove_file(&temp) {
            Ok(()) => String::new(),
            Err(cleanup) => format!("; {} is left behind: {cleanup}", temp.display()),
        };
        return Err(
            anyhow::Error::new(error).context(format!("replacing {}{left}", path.display()))
        );
    }
    Ok(())
}

/// Link `dest_dir/name` to `source_dir/name` where the operator has the file
/// and the instance has nothing, or a link elsewhere. Real files are kept.
pub fn ensure_link(source_dir: &Path, dest_dir: &Path, name: &str) -> anyhow::Result<LinkState> {
    let source = source_dir.join(name);
    let dest = dest_dir.join(name);
    assert_ne!(source, dest, "a file is never linked to itself");
    let state = inspect_link(source_dir, dest_dir, name)?;
    match state {
        LinkState::Missing => {}
        LinkState::Elsewhere(_) => std::fs::remove_file(&dest)
            .with_context(|| format!("replacing the link {}", dest.display()))?,
        other => return Ok(other),
    }
    std::os::unix::fs::symlink(&source, &dest)
        .with_context(|| format!("linking {} to {}", dest.display(), source.display()))?;
    Ok(LinkState::Linked)
}

/// What `dest_dir/name` is relative to the operator's `source_dir/name`.
pub fn inspect_link(source_dir: &Path, dest_dir: &Path, name: &str) -> anyhow::Result<LinkState> {
    let source = source_dir.join(name);
    let dest = dest_dir.join(name);
    let source_exists = source
        .try_exists()
        .with_context(|| format!("checking {}", source.display()))?;
    let meta = match std::fs::symlink_metadata(&dest) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(if source_exists {
                LinkState::Missing
            } else {
                LinkState::NoSource
            });
        }
        Err(e) => return Err(anyhow::Error::new(e).context(format!("checking {}", dest.display()))),
    };
    if !meta.file_type().is_symlink() {
        return Ok(LinkState::OwnFile);
    }
    let target =
        std::fs::read_link(&dest).with_context(|| format!("reading {}", dest.display()))?;
    Ok(match (target == source, source_exists) {
        (false, _) => LinkState::Elsewhere(target),
        (true, true) => LinkState::Linked,
        (true, false) => LinkState::Dangling,
    })
}

fn same_dir(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
}

#[cfg(test)]
mod tests;
