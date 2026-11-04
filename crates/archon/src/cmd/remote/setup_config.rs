//! `[remote] mounted_at` and `trusted_origins` in Archon's config.toml, as
//! `archon remote setup` writes them: only the two keys change, the rest
//! of the file is kept as written, and a file it cannot edit safely is left
//! alone with the lines to add by hand.

use std::path::Path;

use anyhow::{Context as _, bail};

const MOUNTED_AT: &str = "mounted_at";
const TRUSTED_ORIGINS: &str = "trusted_origins";

/// Write `mounted_at = prefix` and `trusted_origins = trusted` under
/// `[remote]` in `path`; `false` when the file already says so.
pub(super) fn ensure_mounted(
    path: &Path,
    prefix: &str,
    trusted: &[String],
) -> anyhow::Result<bool> {
    assert!(prefix.starts_with('/'), "a mount prefix is absolute");
    assert!(!trusted.is_empty(), "a mount trusts at least one origin");
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    let before: toml::Table = toml::from_str(&text)
        .with_context(|| format!("{} is not valid TOML; not changing it", path.display()))?;
    if says_mounted(&before, prefix, trusted) {
        return Ok(false);
    }
    let edited = edit(&text, prefix, trusted);
    let after: Option<toml::Table> = toml::from_str(&edited).ok();
    let safe = after.is_some_and(|after| {
        says_mounted(&after, prefix, trusted) && without_mount(after) == without_mount(before)
    });
    if !safe {
        bail!(
            "{} declares [remote] in a form setup cannot edit safely; not changing it. Put \
             these under [remote] by hand, then run this again:\n{}",
            path.display(),
            lines(prefix, trusted)
        );
    }
    write_atomically(path, &edited)?;
    Ok(true)
}

fn says_mounted(table: &toml::Table, prefix: &str, trusted: &[String]) -> bool {
    let Some(remote) = table.get("remote").and_then(toml::Value::as_table) else {
        return false;
    };
    let mounted = remote.get(MOUNTED_AT).and_then(toml::Value::as_str) == Some(prefix);
    let origins: Option<Vec<&str>> = remote
        .get(TRUSTED_ORIGINS)
        .and_then(toml::Value::as_array)
        .map(|list| list.iter().filter_map(toml::Value::as_str).collect());
    mounted && origins.is_some_and(|o| o == trusted.iter().map(String::as_str).collect::<Vec<_>>())
}

/// The table with the two mount keys taken out, to check nothing else moved.
fn without_mount(mut table: toml::Table) -> toml::Table {
    if let Some(remote) = table.get_mut("remote").and_then(toml::Value::as_table_mut) {
        remote.remove(MOUNTED_AT);
        remote.remove(TRUSTED_ORIGINS);
        if remote.is_empty() {
            table.remove("remote");
        }
    }
    table
}

fn lines(prefix: &str, trusted: &[String]) -> String {
    let origins = toml::Value::Array(trusted.iter().cloned().map(toml::Value::String).collect());
    format!(
        "{MOUNTED_AT} = {}\n{TRUSTED_ORIGINS} = {origins}\n",
        toml::Value::String(prefix.to_owned())
    )
}

/// `text` with the two keys put first under its `[remote]` header, any
/// earlier values of them dropped; or a `[remote]` table appended.
fn edit(text: &str, prefix: &str, trusted: &[String]) -> String {
    let all: Vec<&str> = text.lines().collect();
    let Some(header) = all.iter().position(|line| is_header(line, "[remote]")) else {
        let gap = if text.is_empty() || text.ends_with("\n\n") {
            ""
        } else if text.ends_with('\n') {
            "\n"
        } else {
            "\n\n"
        };
        return format!("{text}{gap}[remote]\n{}", lines(prefix, trusted));
    };
    let end = all[header + 1..]
        .iter()
        .position(|line| line.trim_start().starts_with('['))
        .map_or(all.len(), |offset| header + 1 + offset);
    let mut out: Vec<String> = all[..=header].iter().map(|l| (*l).to_owned()).collect();
    out.extend(lines(prefix, trusted).lines().map(str::to_owned));
    let mut depth = 0i32;
    for line in &all[header + 1..end] {
        let key = line.split('=').next().unwrap_or_default().trim();
        if depth > 0 || key == MOUNTED_AT || key == TRUSTED_ORIGINS {
            // A value may run over several lines, as a multi-line array does.
            depth += brackets(line);
            continue;
        }
        out.push((*line).to_owned());
    }
    out.extend(all[end..].iter().map(|l| (*l).to_owned()));
    let mut edited = out.join("\n");
    edited.push('\n');
    edited
}

fn is_header(line: &str, header: &str) -> bool {
    let code = line.split('#').next().unwrap_or_default().trim();
    code == header
}

/// Opening minus closing brackets outside strings, to follow a value.
fn brackets(line: &str) -> i32 {
    let mut quoted = false;
    let mut depth = 0;
    for c in line.chars() {
        match c {
            '"' => quoted = !quoted,
            '[' if !quoted => depth += 1,
            ']' if !quoted => depth -= 1,
            '#' if !quoted => break,
            _ => {}
        }
    }
    depth
}

/// Replace `path` with `text` through a sibling file, keeping its mode.
fn write_atomically(path: &Path, text: &str) -> anyhow::Result<()> {
    let parent = path.parent().context("config.toml has a directory")?;
    std::fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    let temporary = parent.join(".config.toml.setup");
    std::fs::write(&temporary, text).with_context(|| format!("writing {}", temporary.display()))?;
    if let Ok(meta) = std::fs::metadata(path) {
        std::fs::set_permissions(&temporary, meta.permissions())
            .with_context(|| format!("keeping the mode of {}", path.display()))?;
    }
    std::fs::rename(&temporary, path).with_context(|| format!("replacing {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const ORIGIN: &str = "https://box.ts.net";

    fn written(before: &str) -> (bool, String) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, before).unwrap();
        let changed = ensure_mounted(&path, "/archon", &[ORIGIN.to_owned()]).unwrap();
        (changed, std::fs::read_to_string(&path).unwrap())
    }

    #[test]
    fn the_keys_go_under_remote_and_a_second_write_changes_nothing() {
        let before = "# mine\n[aether]\nmode = \"shared\"\n\n[remote]\naddress = \"127.0.0.1:8378\" # kept\n\n[limits]\nx = 1\n";
        let (changed, after) = written(before);
        assert!(changed);
        assert!(after.contains("[remote]\nmounted_at = \"/archon\"\ntrusted_origins = [\"https://box.ts.net\"]\naddress"), "{after}");
        assert!(
            after.starts_with("# mine\n") && after.contains("# kept"),
            "{after}"
        );
        let (again, same) = written(&after);
        assert!(!again, "an applied mount is not written twice");
        assert_eq!(same, after);
    }

    #[test]
    fn old_values_are_replaced_and_a_missing_table_is_appended() {
        let before = "[remote]\ntrusted_origins = [\n  \"https://old.ts.net\",\n]\nmounted_at = \"/old\"\nmax_connections = 8\n";
        let (changed, after) = written(before);
        assert!(changed);
        assert!(
            !after.contains("old.ts.net") && !after.contains("/old"),
            "{after}"
        );
        assert!(after.contains("max_connections = 8"), "{after}");
        let (_, appended) = written("[aether]\nmode = \"shared\"");
        assert!(appended.ends_with("\n\n[remote]\nmounted_at = \"/archon\"\ntrusted_origins = [\"https://box.ts.net\"]\n"), "{appended}");
    }

    #[test]
    fn a_form_that_cannot_be_edited_safely_is_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let inline = "remote = { address = \"127.0.0.1:8378\" }\n";
        std::fs::write(&path, inline).unwrap();
        let refused = ensure_mounted(&path, "/archon", &[ORIGIN.to_owned()]).unwrap_err();
        assert!(refused.to_string().contains("by hand"), "{refused}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), inline);
    }
}
