//! `archon setup`: register the `archon` MCP server in Aether's
//! `config.toml` (ARCHITECTURE §4.7). Appends a marked block only when no
//! server named `archon` exists; never rewrites the operator's file.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use anyhow::Context as _;
use archon_core::mcp_tools::{SERVER_NAME, TOOLS};
use toml::{Table, Value};

pub const BLOCK_MARKER: &str = "# Added by `archon setup`. Remove this block to unregister Archon.";

/// The TOML block to append for this executable.
pub fn mcp_block(archon_bin: &Path) -> String {
    assert!(
        archon_bin.is_absolute(),
        "Aether needs an absolute command path"
    );
    let classes = TOOLS
        .iter()
        .map(|t| format!("{} = \"{}\"", t.name, t.class.aether_name()))
        .collect::<Vec<_>>()
        .join(", ");
    let mut block = String::new();
    let command = toml_string(&archon_bin.display().to_string());
    // Writing to a String cannot fail.
    let _ = write!(
        block,
        "\n{BLOCK_MARKER}\n[[mcp.servers]]\nname = \"{SERVER_NAME}\"\ncommand = {command}\nargs = [\"mcp\"]\nsend_caller_meta = true\nclasses = {{ {classes} }}\n"
    );
    block
}

/// The `[[mcp.servers]]` entry named `archon` in an Aether config.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArchonServer {
    /// `None` when the entry has no `command` (e.g. a `url` server).
    pub command: Option<String>,
    /// Aether names the agent behind each call (`send_caller_meta = true`).
    pub names_callers: bool,
}

/// What [`register`] found or did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Registration {
    Added,
    AlreadyPresent(ArchonServer),
}

/// The server named `archon` that Aether's config declares, if any.
pub fn find_archon_server(config_text: &str) -> Result<Option<ArchonServer>, toml::de::Error> {
    let table: Table = toml::from_str(config_text)?;
    Ok(archon_server(&table).map(|server| ArchonServer {
        command: server
            .get("command")
            .and_then(Value::as_str)
            .map(str::to_owned),
        names_callers: names_callers(server),
    }))
}

/// Whether an `archon` server entry has Aether name the agent behind each
/// call. archond lets a call change Archon only for a caller it can name.
pub fn names_callers(server: &Value) -> bool {
    server.get("send_caller_meta").and_then(Value::as_bool) == Some(true)
}

/// The `[[mcp.servers]]` entries of a parsed Aether config; none when it
/// declares no such list.
pub fn mcp_servers(table: &Table) -> &[Value] {
    table
        .get("mcp")
        .and_then(|mcp| mcp.get("servers"))
        .and_then(Value::as_array)
        .map_or(&[], Vec::as_slice)
}

/// The first `[[mcp.servers]]` entry named `archon`, if any.
pub fn archon_server(table: &Table) -> Option<&Value> {
    mcp_servers(table).iter().find(|s| is_archon_server(s))
}

pub fn is_archon_server(server: &Value) -> bool {
    server.get("name").and_then(Value::as_str) == Some(SERVER_NAME)
}

/// Repair only the fields Archon requires; retain the operator's server settings.
pub fn migrate_archon_server(server: &mut Value) -> anyhow::Result<()> {
    let table = server
        .as_table_mut()
        .context("archon MCP server is not a table")?;
    table.insert("send_caller_meta".into(), Value::Boolean(true));
    let classes = table
        .entry("classes")
        .or_insert_with(|| Value::Table(Table::new()))
        .as_table_mut()
        .context("archon MCP server classes is not a table")?;
    for tool in TOOLS {
        classes
            .entry(tool.name)
            .or_insert_with(|| Value::String(tool.class.aether_name().into()));
    }
    // The bridge tool must be classified as a mutation even in older entries.
    classes.insert(
        "message_orchestrator".into(),
        Value::String("mutation".into()),
    );
    Ok(())
}

/// Aether's `config.toml`, resolved as Aether resolves it.
pub fn aether_config_path() -> PathBuf {
    let dir = std::env::var_os("AETHER_CONFIG_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            dirs::config_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join("aether")
        });
    dir.join("config.toml")
}

/// Append the block unless a server named `archon` exists. A config that
/// does not parse, or would not parse with the block appended (an inline
/// `mcp.servers = [...]` cannot take a `[[mcp.servers]]` table), is left untouched.
pub fn register(config_path: &Path, archon_bin: &Path) -> anyhow::Result<Registration> {
    let existing = match std::fs::read_to_string(config_path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => {
            return Err(anyhow::Error::new(e).context(format!("reading {}", config_path.display())));
        }
    };
    let found = find_archon_server(&existing).with_context(|| {
        format!(
            "{} is not valid TOML; not modifying it",
            config_path.display()
        )
    })?;
    if let Some(server) = found {
        return Ok(Registration::AlreadyPresent(server));
    }
    let block = mcp_block(archon_bin);
    if !matches!(
        find_archon_server(&format!("{existing}{block}")),
        Ok(Some(_))
    ) {
        return Err(archon_core::ArchonError::Config(format!(
            "{} declares `mcp.servers` in a form an appended [[mcp.servers]] table would break; \
             not modifying it. Add this server to that list by hand:\n{block}",
            config_path.display()
        ))
        .into());
    }
    if let Some(parent) = config_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(config_path)
        .with_context(|| format!("opening {}", config_path.display()))?;
    std::io::Write::write_all(&mut file, block.as_bytes())
        .with_context(|| format!("appending to {}", config_path.display()))?;
    Ok(Registration::Added)
}

fn toml_string(text: &str) -> String {
    toml::Value::String(text.to_owned()).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn block_parses_as_aether_config_and_is_detected() {
        let block = mcp_block(Path::new("/usr/local/bin/archon"));
        let server = find_archon_server(&block).unwrap().unwrap();
        assert_eq!(server.command.as_deref(), Some("/usr/local/bin/archon"));
        let table: toml::Table = toml::from_str(&block).unwrap();
        let server = &table["mcp"]["servers"][0];
        assert_eq!(server["args"][0].as_str(), Some("mcp"));
        assert_eq!(server["send_caller_meta"].as_bool(), Some(true));
        assert_eq!(
            server["classes"]["message_orchestrator"].as_str(),
            Some("mutation")
        );
        assert_eq!(server["classes"]["enqueue"].as_str(), Some("mutation"));
        assert_eq!(server["classes"]["get_item"].as_str(), Some("read_only"));
        assert_eq!(server["classes"]["github_reply"].as_str(), Some("external"));
    }

    #[test]
    fn register_appends_once_and_keeps_existing_content() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("aether").join("config.toml");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "[network]\ndefault = \"allowed\"\n").unwrap();
        let bin = Path::new("/opt/archon");
        assert_eq!(register(&path, bin).unwrap(), Registration::Added);
        let again = register(&path, bin).unwrap();
        assert_eq!(
            again,
            Registration::AlreadyPresent(ArchonServer {
                command: Some("/opt/archon".into()),
                names_callers: true,
            })
        );
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.starts_with("[network]"));
        assert_eq!(text.matches(BLOCK_MARKER).count(), 1);
    }

    #[test]
    fn register_refuses_to_touch_invalid_toml() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "[[broken").unwrap();
        assert!(register(&path, Path::new("/opt/archon")).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "[[broken");
    }

    #[test]
    fn archon_server_is_found_in_either_list_form() {
        let tables = [
            "[[mcp.servers]]\nname = \"files\"\n\n[[mcp.servers]]\nname = \"archon\"\n",
            "[mcp]\nservers = [{ name = \"files\" }, { name = \"archon\" }]\n",
        ];
        for text in tables {
            let table: Table = toml::from_str(text).unwrap();
            assert_eq!(mcp_servers(&table).len(), 2, "{text}");
            let server = archon_server(&table).expect(text);
            assert!(is_archon_server(server));
        }
        let malformed: Table = toml::from_str("[mcp]\nservers = \"archon\"\n").unwrap();
        assert!(mcp_servers(&malformed).is_empty());
        assert_eq!(archon_server(&Table::new()), None);
    }

    #[test]
    fn other_servers_are_not_mistaken_for_archon() {
        let text = "[[mcp.servers]]\nname = \"files\"\ncommand = \"npx\"\n";
        assert_eq!(find_archon_server(text).unwrap(), None);
        assert_eq!(find_archon_server("").unwrap(), None);
    }

    #[test]
    fn register_refuses_an_append_that_would_break_the_config() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let inline = "# mine\n[mcp]\nservers = [{ name = \"files\", command = \"npx\" }]\n";
        std::fs::write(&path, inline).unwrap();
        let error = register(&path, Path::new("/opt/archon")).unwrap_err();
        assert!(
            format!("{error:#}").contains("[[mcp.servers]]"),
            "{error:#}"
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), inline);
    }

    #[test]
    fn register_keeps_comments_and_a_missing_final_newline_intact() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let original = "# keep me\n[network]\ndefault = \"allowed\"  # inline note";
        std::fs::write(&path, original).unwrap();
        assert_eq!(
            register(&path, Path::new("/opt/archon")).unwrap(),
            Registration::Added
        );
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.starts_with(&format!("{original}\n")), "{text}");
        let table: toml::Table = toml::from_str(&text).unwrap();
        assert_eq!(table["network"]["default"].as_str(), Some("allowed"));
    }
}
