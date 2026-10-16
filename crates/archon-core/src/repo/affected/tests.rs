use std::path::Path;

use super::*;

fn write(root: &Path, path: &str, text: &str) {
    let file = root.join(path);
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    std::fs::write(file, text).unwrap();
}

fn package(name: &str, deps: &str) -> String {
    format!("[package]\nname = \"{name}\"\nversion = \"0.1.0\"\n\n{deps}")
}

/// `core` ← `lib` ← `app`, `tool` alone; `bench` is a glob-less member,
/// `crates/skip` is excluded.
fn fixture() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(
        root,
        "Cargo.toml",
        "[workspace]\nmembers = [\"crates/*\", \"bench\"]\nexclude = [\"crates/skip\"]\n\n\
         [workspace.dependencies]\ncore = { path = \"crates/core\" }\n",
    );
    write(root, "crates/core/Cargo.toml", &package("core", ""));
    let lib = "[dependencies]\ncore = { workspace = true }\n";
    write(root, "crates/lib/Cargo.toml", &package("lib", lib));
    let app =
        "[target.'cfg(unix)'.dev-dependencies]\nmylib = { package = \"lib\", path = \"../lib\" }\n";
    write(root, "crates/app/Cargo.toml", &package("app", app));
    write(
        root,
        "crates/tool/Cargo.toml",
        &package("tool", "[dependencies]\nserde = \"1\"\n"),
    );
    write(root, "crates/skip/Cargo.toml", &package("skip", ""));
    write(root, "bench/Cargo.toml", &package("bench-runner", ""));
    write(root, "docs/guide.md", "# guide\n");
    dir
}

fn units(root: &Path, changed: &[&str]) -> Affected {
    let changed: Vec<String> = changed.iter().map(|p| (*p).to_owned()).collect();
    affected(root, &changed).unwrap()
}

#[test]
fn a_leaf_crate_yields_only_itself() {
    let dir = fixture();
    let found = units(
        dir.path(),
        &["crates/tool/src/main.rs", "crates/app/src/lib.rs"],
    );
    assert_eq!(found.units, ["app", "tool"]);
    assert_eq!(found.whole, None);
    assert_eq!(
        units(dir.path(), &["bench/src/main.rs"]).units,
        ["bench-runner"]
    );
}

#[test]
fn a_library_yields_itself_and_every_dependent() {
    let dir = fixture();
    let found = units(dir.path(), &["crates/core/src/lib.rs"]);
    assert_eq!(
        found.units,
        ["app", "core", "lib"],
        "through workspace = true and a rename"
    );
    assert_eq!(
        units(dir.path(), &["crates/lib/Cargo.toml"]).units,
        ["app", "lib"]
    );
}

#[test]
fn lockfile_toolchain_and_ci_changes_yield_the_whole_workspace() {
    let dir = fixture();
    let every = ["app", "bench-runner", "core", "lib", "tool"];
    for trigger in [
        "Cargo.lock",
        "rust-toolchain.toml",
        "Cargo.toml",
        ".github/workflows/ci.yml",
    ] {
        let found = units(dir.path(), &["crates/tool/src/main.rs", trigger]);
        assert_eq!(found.units, every, "{trigger}");
        assert_eq!(found.whole.as_deref(), Some(trigger));
    }
}

#[test]
fn files_outside_every_crate_and_excluded_crates_affect_none() {
    let dir = fixture();
    let found = units(
        dir.path(),
        &["docs/guide.md", "README.md", "crates/skip/src/lib.rs"],
    );
    assert!(found.units.is_empty(), "{found:?}");
    assert_eq!(found.whole, None);
}

#[test]
fn a_root_package_owns_what_no_member_does() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let deps =
        "[dependencies]\nhelper = { path = \"helper\" }\n\n[workspace]\nmembers = [\"helper\"]\n";
    write(root, "Cargo.toml", &package("top", deps));
    write(root, "helper/Cargo.toml", &package("helper", ""));
    assert_eq!(units(root, &["src/main.rs"]).units, ["top"]);
    assert_eq!(units(root, &["helper/src/lib.rs"]).units, ["helper", "top"]);
}

#[test]
fn without_cargo_a_unit_is_a_changed_top_level_directory() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    for path in ["web/index.js", "api/main.py", "lib/x.py", ".hidden/x"] {
        write(root, path, "");
    }
    let found = units(
        root,
        &["web/index.js", "api/main.py", "README.md", "gone/x"],
    );
    assert_eq!(
        found.units,
        ["api", "web"],
        "a removed directory is no unit"
    );
    let whole = units(root, &["package.json"]);
    assert_eq!(whole.units, ["api", "lib", "web"]);
    assert_eq!(whole.whole.as_deref(), Some("package.json"));
}

#[test]
fn a_broken_member_manifest_is_an_error_naming_it() {
    let dir = fixture();
    write(dir.path(), "crates/tool/Cargo.toml", "[package\n");
    let error = affected(dir.path(), &["crates/tool/x".to_owned()]).unwrap_err();
    assert!(
        error.to_string().contains("crates/tool/Cargo.toml"),
        "{error}"
    );
}

#[test]
fn placeholders_expand_to_quoted_units() {
    let units = vec!["archon-core".to_owned(), "it's".to_owned()];
    let command = "cargo clippy {packages:-p} && cargo test {packages:-p} && ls {packages}";
    let expanded = expand(command, &units).unwrap().unwrap();
    assert_eq!(
        expanded,
        "cargo clippy -p archon-core -p 'it'\\''s' && cargo test -p archon-core -p 'it'\\''s' \
         && ls archon-core 'it'\\''s'"
    );
    assert!(selects(command));
    assert_eq!(expand(command, &[]).unwrap(), None, "nothing to run");
    assert_eq!(
        expand("make quick", &[]).unwrap().as_deref(),
        Some("make quick")
    );
    assert!(!selects("make quick"));
}

#[test]
fn malformed_placeholders_are_refused() {
    for bad in [
        "x {packages",
        "x {packages:}",
        "x {packagesy}",
        "x {packages:{y}",
    ] {
        assert!(check_placeholders(bad).is_err(), "{bad}");
        assert!(expand(bad, &["a".to_owned()]).is_err(), "{bad}");
    }
    assert!(check_placeholders("echo ${HOME} {} {package}").is_ok());
}

#[test]
fn wildcards_match_any_run_of_characters() {
    assert!(wildcard("*", "anything"));
    assert!(wildcard("archon-*", "archon-core"));
    assert!(wildcard("a*c*e", "abcde"));
    assert!(!wildcard("a*c*e", "abde"));
    assert!(!wildcard("ab*ba", "aba"));
    assert_eq!(
        joined("crates/app", "../lib").as_deref(),
        Some("crates/lib")
    );
    assert_eq!(joined("", "../outside"), None);
}
