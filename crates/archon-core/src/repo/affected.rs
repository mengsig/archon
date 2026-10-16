//! The units a change affects (issue #82, task AR-I2), which a fast check
//! runs for through its `{packages}` placeholders. In a Cargo workspace a
//! unit is a crate: the changed ones and every workspace crate depending on
//! them. Elsewhere it is a changed top-level directory.

use std::collections::{BTreeMap, BTreeSet};
use std::ops::Range;
use std::path::Path;

use crate::ArchonError;

/// Root files whose change affects every unit: manifests, lockfiles,
/// toolchain pins and CI definitions.
const ROOT_TRIGGERS: &[&str] = &[
    "Cargo.toml",
    "Cargo.lock",
    "rust-toolchain",
    "rust-toolchain.toml",
    "Makefile",
    "package.json",
    "package-lock.json",
    "pnpm-lock.yaml",
    "yarn.lock",
    "pyproject.toml",
    "poetry.lock",
    "uv.lock",
    "go.mod",
    "go.sum",
    ".gitlab-ci.yml",
    ".travis.yml",
    "Jenkinsfile",
    "azure-pipelines.yml",
];

/// Root directories whose change affects every unit: build and CI config,
/// and Archon's own, which holds the check itself.
const TRIGGER_DIRS: &[&str] = &[".github/", ".circleci/", ".cargo/", ".archon/"];

/// What a change affects.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Affected {
    /// Sorted and distinct: crate names, or top-level directories.
    pub units: Vec<String>,
    /// The changed path that made it every unit, if one did.
    pub whole: Option<String>,
}

/// The units `changed` (paths relative to `root`, `/`-separated) affects in
/// the tree at `root`, read from disk.
pub fn affected(root: &Path, changed: &[String]) -> Result<Affected, ArchonError> {
    assert!(root.is_absolute(), "checks run in absolute work trees");
    let whole = changed.iter().find(|path| triggers_whole(path)).cloned();
    let units = match workspace(root)? {
        Some(members) => crates(&members, changed, whole.is_some()),
        None => directories(root, changed, whole.is_some())?,
    };
    debug_assert!(units.windows(2).all(|w| w[0] < w[1]), "sorted, distinct");
    Ok(Affected { units, whole })
}

fn triggers_whole(path: &str) -> bool {
    ROOT_TRIGGERS.contains(&path) || TRIGGER_DIRS.iter().any(|dir| path.starts_with(dir))
}

/// One crate of the workspace.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Member {
    /// Relative to the root; empty for a root package.
    dir: String,
    name: String,
    /// The directories of its path dependencies, relative to the root.
    deps: Vec<String>,
}

/// The crates whose files `changed` touches, and every crate depending on
/// one of them; every crate with `whole`.
fn crates(members: &[Member], changed: &[String], whole: bool) -> Vec<String> {
    assert!(!members.is_empty(), "a workspace has a crate");
    let mut hit: BTreeSet<usize> = match whole {
        true => (0..members.len()).collect(),
        false => changed.iter().flat_map(|p| touched(members, p)).collect(),
    };
    let at: BTreeMap<&str, usize> = members
        .iter()
        .enumerate()
        .map(|(i, m)| (m.dir.as_str(), i))
        .collect();
    let mut queue: Vec<usize> = hit.iter().copied().collect();
    while let Some(changed) = queue.pop() {
        for (i, member) in members.iter().enumerate() {
            let depends = member
                .deps
                .iter()
                .any(|d| at.get(d.as_str()) == Some(&changed));
            if depends && hit.insert(i) {
                queue.push(i);
            }
        }
    }
    let names: BTreeSet<String> = hit.into_iter().map(|i| members[i].name.clone()).collect();
    names.into_iter().collect()
}

/// The crate whose directory holds `path`: the deepest one. A path no crate
/// holds touches the crates whose path dependencies hold it, as an excluded
/// one.
fn touched(members: &[Member], path: &str) -> Vec<usize> {
    assert!(!path.starts_with('/'), "changed paths are relative");
    let mut found: Option<usize> = None;
    for (i, member) in members.iter().enumerate() {
        let deeper = found.is_none_or(|f| member.dir.len() > members[f].dir.len());
        if holds(&member.dir, path) && deeper {
            found = Some(i);
        }
    }
    if let Some(owner) = found {
        return vec![owner];
    }
    let depending = members.iter().enumerate();
    let depending = depending.filter(|(_, m)| m.deps.iter().any(|d| holds(d, path)));
    depending.map(|(i, _)| i).collect()
}

/// Whether the directory `dir` (relative; empty for the root) holds `path`.
fn holds(dir: &str, path: &str) -> bool {
    dir.is_empty()
        || path
            .strip_prefix(dir)
            .is_some_and(|rest| rest.starts_with('/'))
}

/// The top-level directories `changed` touches that still exist; all of
/// them with `whole`. Hidden directories are no units.
fn directories(root: &Path, changed: &[String], whole: bool) -> Result<Vec<String>, ArchonError> {
    let mut present = BTreeSet::new();
    let entries = std::fs::read_dir(root)
        .map_err(|e| ArchonError::io(format!("listing {}", root.display()), e))?;
    for entry in entries {
        let entry = entry.map_err(|e| ArchonError::io(format!("listing {}", root.display()), e))?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.starts_with('.') && entry.path().is_dir() {
            present.insert(name);
        }
    }
    if whole {
        return Ok(present.into_iter().collect());
    }
    let touched: BTreeSet<String> = changed
        .iter()
        .filter_map(|path| path.split_once('/').map(|(top, _)| top.to_owned()))
        .filter(|top| present.contains(top))
        .collect();
    assert!(touched.is_subset(&present), "only existing directories");
    Ok(touched.into_iter().collect())
}

/// The crates of the Cargo workspace or package at `root`; `None` when it
/// has no root `Cargo.toml`.
fn workspace(root: &Path) -> Result<Option<Vec<Member>>, ArchonError> {
    let Some(top) = manifest(root, "")? else {
        return Ok(None);
    };
    let section = top.get("workspace").and_then(toml::Value::as_table);
    let mut found = Discovery {
        root,
        shared: section.map(shared_paths).unwrap_or_default(),
        excluded: listed(section, "exclude"),
        seen: BTreeSet::from([String::new()]),
        members: Vec::new(),
    };
    found.members.extend(member(&top, "", &found.shared));
    for pattern in listed(section, "members") {
        for dir in expand_glob(root, &pattern)? {
            found.admit(dir)?;
        }
    }
    // Cargo makes every path dependency under the root a member too.
    let mut next = 0;
    while next < found.members.len() {
        for dir in found.members[next].deps.clone() {
            found.admit(dir)?;
        }
        next += 1;
    }
    let members = found.members;
    assert!(
        members.iter().all(|m| !m.name.is_empty()),
        "a package has a name"
    );
    Ok((!members.is_empty()).then_some(members))
}

/// The members found so far, and what admits another.
struct Discovery<'a> {
    root: &'a Path,
    shared: BTreeMap<String, String>,
    excluded: Vec<String>,
    /// Every directory considered, members or not.
    seen: BTreeSet<String>,
    members: Vec<Member>,
}

impl Discovery<'_> {
    /// Admit the package in `dir` unless it was considered or is excluded.
    fn admit(&mut self, dir: String) -> Result<(), ArchonError> {
        assert!(!dir.starts_with('/'), "member directories are relative");
        let out = self
            .excluded
            .iter()
            .any(|ex| dir == *ex || dir.starts_with(&format!("{ex}/")));
        if out || !self.seen.insert(dir.clone()) {
            return Ok(());
        }
        if let Some(found) = manifest(self.root, &dir)? {
            self.members.extend(member(&found, &dir, &self.shared));
        }
        debug_assert!(self.seen.contains(&dir));
        Ok(())
    }
}

/// The `[workspace]` list under `key`, without trailing slashes.
fn listed(section: Option<&toml::Table>, key: &str) -> Vec<String> {
    let list = section
        .and_then(|ws| ws.get(key))
        .and_then(toml::Value::as_array);
    let names = list.into_iter().flatten().filter_map(toml::Value::as_str);
    names
        .map(|name| name.trim_end_matches('/').to_owned())
        .collect()
}

/// `<root>/<dir>/Cargo.toml`, parsed; `None` when missing.
fn manifest(root: &Path, dir: &str) -> Result<Option<toml::Table>, ArchonError> {
    let path = root.join(dir).join("Cargo.toml");
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(ArchonError::io(format!("reading {}", path.display()), e)),
    };
    let table = text
        .parse::<toml::Table>()
        .map_err(|e| ArchonError::Config(format!("{}: {e}", path.display())))?;
    Ok(Some(table))
}

/// The package `manifest` in `dir` declares, if any, with its path
/// dependencies; `shared` resolves `workspace = true` ones.
fn member(manifest: &toml::Table, dir: &str, shared: &BTreeMap<String, String>) -> Option<Member> {
    let package = manifest.get("package")?.as_table()?;
    let name = package.get("name")?.as_str()?.to_owned();
    let mut tables: Vec<&toml::Table> = vec![manifest];
    let targets = manifest.get("target").and_then(toml::Value::as_table);
    tables.extend(
        targets
            .into_iter()
            .flat_map(|t| t.values())
            .filter_map(toml::Value::as_table),
    );
    let mut deps = Vec::new();
    for table in tables {
        for kind in ["dependencies", "dev-dependencies", "build-dependencies"] {
            let Some(listed) = table.get(kind).and_then(toml::Value::as_table) else {
                continue;
            };
            for (key, spec) in listed {
                let Some(spec) = spec.as_table() else {
                    continue;
                };
                let local = spec.get("path").and_then(toml::Value::as_str);
                let inherited = spec.get("workspace").and_then(toml::Value::as_bool) == Some(true);
                let dep = match local {
                    Some(path) => joined(dir, path),
                    None if inherited => shared.get(key).cloned(),
                    None => None,
                };
                deps.extend(dep);
            }
        }
    }
    Some(Member {
        dir: dir.to_owned(),
        name,
        deps,
    })
}

/// `[workspace.dependencies]` entries with a path: name → directory.
fn shared_paths(workspace: &toml::Table) -> BTreeMap<String, String> {
    let listed = workspace
        .get("dependencies")
        .and_then(toml::Value::as_table);
    let mut paths = BTreeMap::new();
    for (name, spec) in listed.into_iter().flatten() {
        let path = spec
            .as_table()
            .and_then(|s| s.get("path"))
            .and_then(toml::Value::as_str);
        if let Some(dir) = path.and_then(|p| joined("", p)) {
            paths.insert(name.clone(), dir);
        }
    }
    paths
}

/// `relative` from `base`, both under the root, normalised; `None` when it
/// leaves the root or is absolute.
fn joined(base: &str, relative: &str) -> Option<String> {
    if relative.starts_with('/') {
        return None;
    }
    let mut parts: Vec<&str> = base.split('/').filter(|p| !p.is_empty()).collect();
    for part in relative.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            name => parts.push(name),
        }
    }
    Some(parts.join("/"))
}

/// The directories under `root` that `pattern` names; a component may hold
/// `*`, which matches any run of characters.
fn expand_glob(root: &Path, pattern: &str) -> Result<Vec<String>, ArchonError> {
    let mut found = vec![String::new()];
    for part in pattern.split('/').filter(|p| !p.is_empty() && *p != ".") {
        let mut next = Vec::new();
        for dir in &found {
            if !part.contains('*') {
                next.push(joined(dir, part).unwrap_or_default());
                continue;
            }
            let at = root.join(dir);
            let entries = std::fs::read_dir(&at)
                .map_err(|e| ArchonError::io(format!("listing {}", at.display()), e))?;
            for entry in entries {
                let entry =
                    entry.map_err(|e| ArchonError::io(format!("listing {}", at.display()), e))?;
                let name = entry.file_name().to_string_lossy().into_owned();
                if entry.path().is_dir() && wildcard(part, &name) {
                    next.push(joined(dir, &name).unwrap_or_default());
                }
            }
        }
        found = next;
    }
    found.retain(|dir| !dir.is_empty());
    found.sort();
    Ok(found)
}

/// Whether `name` matches `pattern`, whose `*` matches any run of characters.
fn wildcard(pattern: &str, name: &str) -> bool {
    let pieces: Vec<&str> = pattern.split('*').collect();
    assert!(pieces.len() >= 2, "only a pattern with `*` is matched");
    let (first, last) = (pieces[0], pieces[pieces.len() - 1]);
    if !name.starts_with(first) || name.len() < first.len() + last.len() || !name.ends_with(last) {
        return false;
    }
    let mut rest = &name[first.len()..name.len() - last.len()];
    for piece in &pieces[1..pieces.len() - 1] {
        match rest.find(piece) {
            Some(at) => rest = &rest[at + piece.len()..],
            None => return false,
        }
    }
    true
}

/// One `{packages}` or `{packages:<flag>}` in a check command.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Placeholder {
    range: Range<usize>,
    flag: Option<String>,
}

const PLACEHOLDER: &str = "{packages";

/// The command's placeholders, or why one is malformed.
fn placeholders(command: &str) -> Result<Vec<Placeholder>, String> {
    let mut found = Vec::new();
    let mut from = 0;
    while let Some(at) = command[from..].find(PLACEHOLDER).map(|i| i + from) {
        let rest = &command[at + PLACEHOLDER.len()..];
        let Some(close) = rest.find('}') else {
            return Err(format!("`{PLACEHOLDER}` at byte {at} has no closing `}}`"));
        };
        let flag = match &rest[..close] {
            "" => None,
            inner => match inner.strip_prefix(':') {
                Some(flag) if !flag.trim().is_empty() && !flag.contains('{') => {
                    Some(flag.to_owned())
                }
                _ => {
                    return Err(format!(
                        "`{{packages{inner}}}` is neither `{{packages}}` nor `{{packages:<flag>}}`"
                    ));
                }
            },
        };
        let end = at + PLACEHOLDER.len() + close + 1;
        found.push(Placeholder {
            range: at..end,
            flag,
        });
        from = end;
    }
    Ok(found)
}

/// Why `command`'s placeholders are malformed, if they are.
pub fn check_placeholders(command: &str) -> Result<(), String> {
    placeholders(command).map(|_| ())
}

/// Whether `command` runs for the affected units only.
pub fn selects(command: &str) -> bool {
    placeholders(command).is_ok_and(|found| !found.is_empty())
}

/// `command` for `units`: `{packages}` becomes the units and
/// `{packages:-p}` becomes `-p <unit>` for each. `None` when it selects and
/// no unit is affected: nothing to run. A unit that is no plain name refuses.
pub fn expand(command: &str, units: &[String]) -> Result<Option<String>, String> {
    let found = placeholders(command)?;
    if found.is_empty() {
        return Ok(Some(command.to_owned()));
    }
    if units.is_empty() {
        return Ok(None);
    }
    if let Some(unsafe_unit) = units.iter().find(|unit| !plain(unit)) {
        return Err(format!(
            "the unit {unsafe_unit:?} is no plain name (letters, digits, `_`, `-`, `.`, `+`, \
             not leading `-` or `.`), so no command may take it"
        ));
    }
    let mut text = String::with_capacity(command.len() + units.len() * 16);
    let mut copied = 0;
    for placeholder in &found {
        assert!(placeholder.range.start >= copied, "placeholders in order");
        text.push_str(&command[copied..placeholder.range.start]);
        let words: Vec<String> = units
            .iter()
            .map(|unit| match &placeholder.flag {
                Some(flag) => format!("{flag} {unit}"),
                None => unit.clone(),
            })
            .collect();
        text.push_str(&words.join(" "));
        copied = placeholder.range.end;
    }
    text.push_str(&command[copied..]);
    Ok(Some(text))
}

/// Whether `unit` reads as the same one word in any shell context, quoted,
/// unquoted or `eval`ed, and as no option: units come from the item's tree.
fn plain(unit: &str) -> bool {
    let inner = |c: char| c.is_ascii_alphanumeric() || "_-.+".contains(c);
    let first = unit.chars().next();
    first.is_some_and(|c| c.is_ascii_alphanumeric() || c == '_') && unit.chars().all(inner)
}

#[cfg(test)]
mod tests;
