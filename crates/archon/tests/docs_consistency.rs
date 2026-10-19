//! The reference docs state facts the code owns: the built-in pipelines, the
//! Aether protocol version and the accepted configuration keys. Each is
//! checked against its source, so a change to one fails until the docs follow.

use std::path::Path;

use archon_aether::wire::{MIN_PROTOCOL_VERSION, PROTOCOL_VERSION};
use archon_pipeline::builtin::BUILTINS;

const ROOT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../..");
/// Sources of the structs `Config` and `RepoConfig` are built from.
const CONFIG_SOURCES: [&str; 3] = [
    "crates/archon-core/src/config.rs",
    "crates/archon-core/src/authority.rs",
    "crates/archon-core/src/push.rs",
];
const NUMBER_WORDS: [&str; 20] = [
    "Zero",
    "One",
    "Two",
    "Three",
    "Four",
    "Five",
    "Six",
    "Seven",
    "Eight",
    "Nine",
    "Ten",
    "Eleven",
    "Twelve",
    "Thirteen",
    "Fourteen",
    "Fifteen",
    "Sixteen",
    "Seventeen",
    "Eighteen",
    "Nineteen",
];

fn read(relative: &str) -> String {
    std::fs::read_to_string(Path::new(ROOT).join(relative))
        .unwrap_or_else(|e| panic!("reading {relative}: {e}"))
}

/// The lines of `text` from the heading line starting with `heading` to the next heading.
fn section(text: &str, heading: &str) -> String {
    let mut lines = text.lines().skip_while(|line| !line.starts_with(heading));
    assert!(lines.next().is_some(), "no heading {heading:?}");
    lines
        .take_while(|line| !line.starts_with('#'))
        .collect::<Vec<_>>()
        .join("\n")
}

fn assert_names_every_builtin(label: &str, text: &str) {
    for builtin in BUILTINS {
        let cell = format!("| `{}` ", builtin.name);
        let grouped = format!("`{}`", builtin.name);
        assert!(
            text.contains(&cell) || text.contains(&grouped),
            "{label} does not name the built-in pipeline `{}`",
            builtin.name
        );
    }
}

#[test]
fn the_pipeline_tables_name_every_builtin() {
    assert!(BUILTINS.len() >= 11, "the built-in list is not empty");
    assert_names_every_builtin("README.md", &read("README.md"));
    let pipelines = read("docs/PIPELINES.md");
    assert_names_every_builtin(
        "docs/PIPELINES.md",
        &section(&pipelines, "## Built-in pipelines"),
    );
    let architecture = read("docs/ARCHITECTURE.md");
    assert_names_every_builtin(
        "docs/ARCHITECTURE.md section 8.5",
        &section(&architecture, "### 8.5 "),
    );
}

#[test]
fn the_readme_counts_the_builtins() {
    let word = NUMBER_WORDS[BUILTINS.len()];
    let phrase = format!("{word} built-in pipelines");
    assert!(
        read("README.md").contains(&phrase),
        "README.md should say {phrase:?}"
    );
}

#[test]
fn the_architecture_cites_the_protocol_versions() {
    let architecture = read("docs/ARCHITECTURE.md");
    let intro = section(&architecture, "## 4. Aether integration");
    let current = format!("protocol version **{PROTOCOL_VERSION}**");
    assert!(intro.contains(&current), "section 4 should say {current:?}");
    let range = format!("down to {MIN_PROTOCOL_VERSION}");
    assert!(intro.contains(&range), "section 4 should say {range:?}");
    assert!(
        architecture.contains(&format!("\"protocol_version\":{PROTOCOL_VERSION},")),
        "the section 4.2 hello shows protocol {PROTOCOL_VERSION}"
    );
    assert!(
        architecture.contains(&format!(
            "Aether protocol {MIN_PROTOCOL_VERSION}..{PROTOCOL_VERSION};"
        )),
        "the handshake refusal text states {MIN_PROTOCOL_VERSION}..{PROTOCOL_VERSION}"
    );
}

/// The field names of every `deny_unknown_fields` struct in `source`.
fn config_keys(source: &str) -> Vec<(String, String)> {
    let mut keys = Vec::new();
    let mut lines = source.lines();
    while let Some(line) = lines.next() {
        if !line.contains("deny_unknown_fields") {
            continue;
        }
        let Some(name) = lines
            .by_ref()
            .find_map(|l| l.strip_prefix("pub struct ").map(str::to_owned))
        else {
            continue;
        };
        let name = name.trim_end_matches(" {").to_owned();
        for field in lines.by_ref().take_while(|l| *l != "}") {
            let Some(rest) = field.strip_prefix("    pub ") else {
                continue;
            };
            let key = rest.split(':').next().unwrap_or_default().trim();
            keys.push((name.clone(), key.to_owned()));
        }
    }
    keys
}

#[test]
fn every_accepted_config_key_is_in_the_configuration_reference() {
    let reference = read("docs/configuration.md");
    let mut all = Vec::new();
    for source in CONFIG_SOURCES {
        all.extend(config_keys(&read(source)));
    }
    assert!(all.len() > 100, "the scan found the config structs");
    let missing: Vec<String> = all
        .iter()
        .filter(|(_, key)| {
            !reference.contains(&format!("{key} ="))
                && !reference.contains(&format!("`{key}`"))
                && !reference.contains(&format!("[{key}]"))
                && !reference.contains(&format!("[{key}."))
                && !reference.contains(&format!(".{key}]"))
        })
        .map(|(owner, key)| format!("{owner}.{key}"))
        .collect();
    assert!(
        missing.is_empty(),
        "docs/configuration.md misses accepted keys: {missing:?}"
    );
}
