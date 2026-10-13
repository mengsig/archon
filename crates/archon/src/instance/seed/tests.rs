use std::collections::BTreeMap;

use super::*;
use crate::setup::find_archon_server;

mod calm_tests;
mod voice_tests;

const OPERATOR_CONFIG: &str = r#"# the operator's own notes
[approval]
default = "ask"

[network]
default = "allowed"

[listen]
address = "127.0.0.1:7777"

[[mcp.servers]]
name = "files"
command = "npx"

[[mcp.servers]]
name = "archon"
command = "/old/archon"
args = ["mcp"]

[routing.levels.low]
model = { provider = "claude-code", model = "sonnet" }
"#;

struct Dirs {
    _root: tempfile::TempDir,
    operator: PathBuf,
    instance: Instance,
}

fn dirs() -> Dirs {
    let root = tempfile::tempdir().unwrap();
    let base = root.path().canonicalize().unwrap();
    let operator = base.join("operator-aether");
    std::fs::create_dir_all(&operator).unwrap();
    let instance = Instance::from_archon_dirs(
        base.join("archon-config"),
        base.join("archon-data"),
        base.join("run").join("archond.sock"),
    );
    Dirs {
        _root: root,
        operator,
        instance,
    }
}

/// Every file under `dir` with its contents, links as their targets.
fn snapshot(dir: &Path) -> BTreeMap<PathBuf, String> {
    let mut files = BTreeMap::new();
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        let meta = std::fs::symlink_metadata(&path).unwrap();
        let text = if meta.is_dir() {
            format!("dir {:?}", snapshot(&path))
        } else {
            std::fs::read_to_string(&path).unwrap_or_default()
        };
        files.insert(path, text);
    }
    files
}

fn server(command: &str) -> Value {
    let block: Table = toml::from_str(&crate::setup::mcp_block(Path::new(command))).unwrap();
    block["mcp"]["servers"][0].clone()
}

#[test]
fn the_seed_keeps_operator_settings_and_holds_one_archon_server() {
    let source = Path::new("/home/op/.config/aether/config.toml");
    let text = seeded_text(OPERATOR_CONFIG, source, server("/opt/archon")).unwrap();
    let table: Table = toml::from_str(&text).unwrap();
    assert_eq!(table["approval"]["default"].as_str(), Some("ask"));
    assert_eq!(table["network"]["default"].as_str(), Some("allowed"));
    assert_eq!(
        table["routing"]["levels"]["low"]["model"]["model"].as_str(),
        Some("sonnet")
    );
    assert!(
        !table.contains_key("listen"),
        "the operator's port stays theirs"
    );
    let names: Vec<_> = table["mcp"]["servers"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["files", "archon"]);
    let archon = find_archon_server(&text).unwrap().unwrap();
    assert_eq!(archon.command.as_deref(), Some("/opt/archon"));
    assert!(text.contains(&source.display().to_string()), "{text}");
}

#[test]
fn an_empty_operator_config_seeds_only_the_archon_server() {
    let text = seeded_text("", Path::new("/c/config.toml"), server("/opt/archon")).unwrap();
    let table: Table = toml::from_str(&text).unwrap();
    assert_eq!(archon_server_count(&table), 1);
    assert_eq!(table.len(), 1, "{text}");
}

#[test]
fn invalid_operator_toml_is_refused_and_nothing_is_written() {
    let dirs = dirs();
    std::fs::write(dirs.operator.join("config.toml"), "[[broken").unwrap();
    let error = seed(
        &dirs.instance,
        &dirs.operator,
        Path::new("/opt/archon"),
        false,
    )
    .unwrap_err();
    assert!(format!("{error:#}").contains("not valid TOML"), "{error:#}");
    assert!(!dirs.instance.config_path().exists());
    let inline = "[mcp]\nservers = \"none\"\n";
    let error = seeded_text(inline, Path::new("/c"), server("/opt/archon")).unwrap_err();
    assert!(format!("{error:#}").contains("not a list"), "{error:#}");
}

#[test]
fn setup_links_shared_files_never_credentials_and_leaves_the_operator_alone() {
    let dirs = dirs();
    std::fs::write(dirs.operator.join("config.toml"), OPERATOR_CONFIG).unwrap();
    std::fs::write(dirs.operator.join("providers.toml"), "[anthropic]\n").unwrap();
    std::fs::write(dirs.operator.join(CREDENTIALS), "{}").unwrap();
    std::fs::create_dir(dirs.operator.join("voice")).unwrap();
    std::fs::create_dir(dirs.operator.join("themes")).unwrap();
    std::fs::write(
        dirs.operator.join("themes/quiet.toml"),
        "name = \"quiet\"\n",
    )
    .unwrap();
    let before = snapshot(&dirs.operator);

    let bin = Path::new("/opt/archon");
    let first = seed(&dirs.instance, &dirs.operator, bin, false).unwrap();
    assert_eq!(first.config, ConfigOutcome::Seeded);
    let states: BTreeMap<_, _> = first.links.into_iter().collect();
    assert_eq!(states["providers.toml"], LinkState::Linked);
    assert_eq!(states["voice"], LinkState::Linked);
    assert_eq!(states["themes"], LinkState::Linked);
    assert_eq!(states["ui.toml"], LinkState::NoSource);
    let link = dirs.instance.config_dir.join("providers.toml");
    assert_eq!(
        std::fs::read_link(&link).unwrap(),
        dirs.operator.join("providers.toml")
    );
    assert!(!dirs.instance.config_dir.join(CREDENTIALS).exists());

    let seeded = std::fs::read_to_string(dirs.instance.config_path()).unwrap();
    let second = seed(&dirs.instance, &dirs.operator, bin, false).unwrap();
    assert_eq!(second.config, ConfigOutcome::Unchanged);
    assert!(
        second
            .links
            .iter()
            .all(|(_, s)| matches!(s, LinkState::Linked | LinkState::NoSource))
    );
    assert_eq!(
        std::fs::read_to_string(dirs.instance.config_path()).unwrap(),
        seeded
    );
    assert_eq!(
        snapshot(&dirs.operator),
        before,
        "the operator's directory is untouched"
    );
}

#[test]
fn resync_follows_the_operator_and_keeps_the_archon_block() {
    let dirs = dirs();
    let operator_config = dirs.operator.join("config.toml");
    std::fs::write(&operator_config, "[network]\ndefault = \"denied\"\n").unwrap();
    seed(
        &dirs.instance,
        &dirs.operator,
        Path::new("/opt/archon"),
        false,
    )
    .unwrap();
    // An edit to the instance's archon server survives the resync.
    let path = dirs.instance.config_path();
    let edited = std::fs::read_to_string(&path)
        .unwrap()
        .replace("/opt/archon", "/custom/archon");
    std::fs::write(&path, edited).unwrap();
    std::fs::write(&operator_config, "[network]\ndefault = \"allowed\"\n").unwrap();

    let resynced = seed(
        &dirs.instance,
        &dirs.operator,
        Path::new("/opt/archon"),
        true,
    )
    .unwrap();
    assert_eq!(resynced.config, ConfigOutcome::Resynced);
    let text = std::fs::read_to_string(&path).unwrap();
    let table: Table = toml::from_str(&text).unwrap();
    assert_eq!(table["network"]["default"].as_str(), Some("allowed"));
    let archon = find_archon_server(&text).unwrap().unwrap();
    assert_eq!(archon.command.as_deref(), Some("/custom/archon"));
    assert_eq!(archon_server_count(&table), 1);
}

#[test]
fn a_seeded_config_without_the_block_gets_it_appended() {
    let dirs = dirs();
    seed(
        &dirs.instance,
        &dirs.operator,
        Path::new("/opt/archon"),
        false,
    )
    .unwrap();
    std::fs::write(
        dirs.instance.config_path(),
        "[network]\ndefault = \"allowed\"\n",
    )
    .unwrap();
    let again = seed(
        &dirs.instance,
        &dirs.operator,
        Path::new("/opt/archon"),
        false,
    )
    .unwrap();
    assert_eq!(again.config, ConfigOutcome::BlockAdded);
    let text = std::fs::read_to_string(dirs.instance.config_path()).unwrap();
    assert!(find_archon_server(&text).unwrap().is_some());
}

#[test]
fn links_are_repaired_but_real_files_are_kept() {
    let dirs = dirs();
    let dest = &dirs.instance.config_dir;
    std::fs::create_dir_all(dest).unwrap();
    std::fs::write(dirs.operator.join("providers.toml"), "").unwrap();
    std::fs::write(dirs.operator.join("ui.toml"), "").unwrap();
    std::os::unix::fs::symlink("/elsewhere/providers.toml", dest.join("providers.toml")).unwrap();
    std::fs::write(dest.join("ui.toml"), "theme = \"dawn\"\n").unwrap();
    assert!(matches!(
        inspect_link(&dirs.operator, dest, "providers.toml").unwrap(),
        LinkState::Elsewhere(_)
    ));

    assert_eq!(
        ensure_link(&dirs.operator, dest, "providers.toml").unwrap(),
        LinkState::Linked
    );
    assert_eq!(
        ensure_link(&dirs.operator, dest, "ui.toml").unwrap(),
        LinkState::OwnFile
    );
    assert_eq!(
        std::fs::read_to_string(dest.join("ui.toml")).unwrap(),
        "theme = \"dawn\"\n"
    );
    std::fs::remove_file(dirs.operator.join("providers.toml")).unwrap();
    assert_eq!(
        inspect_link(&dirs.operator, dest, "providers.toml").unwrap(),
        LinkState::Dangling
    );
}

#[test]
fn the_operators_directory_can_never_be_the_instance() {
    let dirs = dirs();
    let own = dirs.instance.config_dir.clone();
    std::fs::create_dir_all(&own).unwrap();
    let error = seed(&dirs.instance, &own, Path::new("/opt/archon"), false).unwrap_err();
    assert!(error.to_string().contains("AETHER_CONFIG_DIR"), "{error}");
    assert!(!dirs.instance.config_path().exists());
}

#[test]
fn optional_theme_link_never_replaces_an_instance_owned_directory() {
    let dirs = dirs();
    let source = dirs.operator.join("themes");
    let dest = dirs.instance.config_dir.join("themes");
    std::fs::create_dir_all(&dest).unwrap();
    std::fs::write(dest.join("custom.toml"), "own theme").unwrap();
    std::fs::create_dir(&source).unwrap();
    std::fs::write(source.join("custom.toml"), "operator theme").unwrap();
    let before = snapshot(&dirs.operator);
    let seeding = seed(
        &dirs.instance,
        &dirs.operator,
        Path::new("/opt/archon"),
        false,
    )
    .unwrap();
    assert_eq!(
        seeding.links.into_iter().collect::<BTreeMap<_, _>>()["themes"],
        LinkState::OwnFile
    );
    assert_eq!(
        std::fs::read_to_string(dest.join("custom.toml")).unwrap(),
        "own theme"
    );
    assert_eq!(snapshot(&dirs.operator), before);
}

#[test]
fn resync_migrates_a_stale_server_idempotently_preserving_custom_settings() {
    let dirs = dirs();
    let bin = Path::new("/opt/archon");
    seed(&dirs.instance, &dirs.operator, bin, false).unwrap();
    let path = dirs.instance.config_path();
    std::fs::write(
        &path,
        r#"[[mcp.servers]]
name = "archon"
command = "/custom/archon"
args = ["mcp", "--custom"]
send_caller_meta = false
env = { CUSTOM = "kept" }
classes = { enqueue = "custom", extra_tool = "read", message_orchestrator = "read" }
"#,
    )
    .unwrap();
    let mut previous = None;
    for _ in 0..2 {
        seed(&dirs.instance, &dirs.operator, bin, true).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        let table: Table = toml::from_str(&text).unwrap();
        let server = crate::setup::archon_server(&table).unwrap();
        assert_eq!(server["send_caller_meta"].as_bool(), Some(true));
        assert_eq!(
            server["classes"]["message_orchestrator"].as_str(),
            Some("mutation")
        );
        assert_eq!(server["classes"]["enqueue"].as_str(), Some("custom"));
        assert_eq!(server["classes"]["extra_tool"].as_str(), Some("read"));
        assert_eq!(server["command"].as_str(), Some("/custom/archon"));
        assert_eq!(server["args"][1].as_str(), Some("--custom"));
        assert_eq!(server["env"]["CUSTOM"].as_str(), Some("kept"));
        if let Some(previous) = previous {
            assert_eq!(text, previous);
        }
        previous = Some(text);
    }
}

/// What `archon setup` wrote before archond needed its callers named.
const OLD_INSTANCE_CONFIG: &str = r#"# Archon's own Aether.
# Seeded by an older archon.

[approval]
default = "ask"

[[mcp.servers]]
name = "files"
command = "npx"

[[mcp.servers]]
name = "archon"
command = "/opt/archon"
args = ["mcp"]
env = { ARCHON_LOG = "debug" }
classes = { queue_status = "read", message_orchestrator = "read" }
"#;

fn old_instance() -> Dirs {
    let dirs = dirs();
    std::fs::create_dir_all(&dirs.instance.config_dir).unwrap();
    std::fs::write(dirs.instance.config_path(), OLD_INSTANCE_CONFIG).unwrap();
    dirs
}

#[test]
fn a_registration_without_caller_naming_is_repaired_keeping_what_the_operator_set() {
    let dirs = old_instance();
    assert!(registration_is_stale(&dirs.instance).unwrap());
    assert!(!reload_is_owed(&dirs.instance).unwrap());

    assert!(repair_registration(&dirs.instance).unwrap(), "rewritten");
    let text = std::fs::read_to_string(dirs.instance.config_path()).unwrap();
    let server = find_archon_server(&text).unwrap().unwrap();
    assert!(server.names_callers, "{text}");
    assert_eq!(server.command.as_deref(), Some("/opt/archon"));
    assert!(text.starts_with("# Archon's own Aether.\n# Seeded by an older archon.\n"));
    let table: Table = toml::from_str(&text).unwrap();
    assert_eq!(table["approval"]["default"].as_str(), Some("ask"));
    assert_eq!(setup::mcp_servers(&table).len(), 2, "{text}");
    let archon = setup::archon_server(&table).unwrap();
    assert_eq!(archon["env"]["ARCHON_LOG"].as_str(), Some("debug"));
    let classes = archon["classes"].as_table().unwrap();
    assert_eq!(classes["message_orchestrator"].as_str(), Some("mutation"));
    assert_eq!(classes.len(), archon_core::mcp_tools::TOOLS.len());

    assert!(
        reload_is_owed(&dirs.instance).unwrap(),
        "aetherd must load it"
    );
    assert!(!registration_is_stale(&dirs.instance).unwrap());
    assert!(!repair_registration(&dirs.instance).unwrap(), "once");
    assert_eq!(
        std::fs::read_to_string(dirs.instance.config_path()).unwrap(),
        text
    );
    reload_settled(&dirs.instance).unwrap();
    reload_settled(&dirs.instance).unwrap();
    assert!(!reload_is_owed(&dirs.instance).unwrap());
}

#[test]
fn a_current_or_unreadable_config_is_never_rewritten() {
    let dirs = dirs();
    std::fs::write(dirs.operator.join("config.toml"), OPERATOR_CONFIG).unwrap();
    seed(
        &dirs.instance,
        &dirs.operator,
        Path::new("/opt/archon"),
        false,
    )
    .unwrap();
    for text in [
        std::fs::read_to_string(dirs.instance.config_path()).unwrap(),
        "[[mcp.servers]\nname = \"archon\"\n".to_owned(),
        "[approval]\ndefault = \"ask\"\n".to_owned(),
    ] {
        std::fs::write(dirs.instance.config_path(), &text).unwrap();
        assert!(!registration_is_stale(&dirs.instance).unwrap(), "{text}");
        assert!(!repair_registration(&dirs.instance).unwrap(), "{text}");
        let after = std::fs::read_to_string(dirs.instance.config_path()).unwrap();
        assert_eq!(after, text);
        assert!(!reload_is_owed(&dirs.instance).unwrap(), "{text}");
    }
}

#[test]
fn setup_repairs_an_old_registration_with_or_without_resync() {
    for resync in [false, true] {
        let dirs = old_instance();
        std::fs::write(dirs.operator.join("config.toml"), OPERATOR_CONFIG).unwrap();
        let seeding = seed(
            &dirs.instance,
            &dirs.operator,
            Path::new("/opt/archon"),
            resync,
        );
        let expected = match resync {
            true => ConfigOutcome::Resynced,
            false => ConfigOutcome::Repaired,
        };
        assert_eq!(seeding.unwrap().config, expected);
        let text = std::fs::read_to_string(dirs.instance.config_path()).unwrap();
        assert!(find_archon_server(&text).unwrap().unwrap().names_callers);
        assert!(reload_is_owed(&dirs.instance).unwrap(), "resync={resync}");
    }
}
