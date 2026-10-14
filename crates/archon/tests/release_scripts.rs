//! The scripts the release workflow runs: `scripts/release-notes.sh` writes
//! the body of a draft release, `scripts/package-release.sh` packs an archive.

#[path = "common/release.rs"]
mod release;

use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use archon_core::testing::write_script;
use release::{ARCHIVE_MEMBERS, archive_member, archive_members};

const ROOT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../..");
const VERSION: &str = env!("CARGO_PKG_VERSION");
const INSTALL: &str =
    "curl -fsSL https://raw.githubusercontent.com/mengsig/archon/main/install.sh | sh";
const CHANGELOG: &str = "\
# Changelog

## 2.1.0-rc.1

A release candidate.

## 2.0.0


Second release.

```md
## 1.0.0 inside a code block is not a heading
```

### Fixed

- One thing.

## 1.9.0

## 1.0.0

First release.
";

/// Run the script on `changelog` in an empty directory, so that it reads
/// the file it is given and nothing of the checkout.
fn notes(args: &[&str], changelog: Option<&Path>) -> Output {
    let script = PathBuf::from(ROOT).join("scripts/release-notes.sh");
    let elsewhere = tempfile::tempdir().unwrap();
    let mut command = Command::new("sh");
    command.arg(script).args(args).current_dir(elsewhere.path());
    if let Some(changelog) = changelog {
        command.arg(changelog);
    }
    command.output().expect("running release-notes.sh")
}

fn written(text: &str) -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("CHANGELOG.md");
    std::fs::write(&path, text).unwrap();
    (dir, path)
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8(bytes.to_vec()).unwrap()
}

#[test]
fn the_notes_are_the_versions_section_then_the_install_command() {
    let (_dir, changelog) = written(CHANGELOG);
    let out = notes(&["2.0.0"], Some(&changelog));
    assert!(out.status.success(), "{out:?}");
    let expected = format!(
        "Second release.\n\n```md\n## 1.0.0 inside a code block is not a heading\n```\n\n\
         ### Fixed\n\n- One thing.\n\n### Install\n\n```sh\n{INSTALL}\n```\n\n\
         This installs the latest release. To install this one, put\n\
         `ARCHON_VERSION=v2.0.0` before `sh`. Other ways to install are in\n\
         [docs/install.md](https://github.com/mengsig/archon/blob/v2.0.0/docs/install.md).\n"
    );
    assert_eq!(text(&out.stdout), expected);

    // A tag names the same section, and so does the last one in the file.
    let tagged = notes(&["v2.0.0"], Some(&changelog));
    assert_eq!(text(&tagged.stdout), expected);
    let last = text(&notes(&["1.0.0"], Some(&changelog)).stdout);
    assert!(
        last.starts_with("First release.\n\n### Install\n"),
        "{last}"
    );
    let candidate = text(&notes(&["2.1.0-rc.1"], Some(&changelog)).stdout);
    assert!(
        candidate.starts_with("A release candidate.\n\n### Install\n"),
        "{candidate}"
    );
}

#[test]
fn a_missing_or_empty_section_fails_and_names_the_version() {
    let (dir, changelog) = written(CHANGELOG);
    for (version, named) in [
        ("3.0.0", "## 3.0.0"),
        ("1.9.0", "## 1.9.0"),
        ("2.0", "'2.0'"),
    ] {
        let out = notes(&[version], Some(&changelog));
        let stderr = text(&out.stderr);
        assert_eq!(out.status.code(), Some(1), "{version}: {out:?}");
        assert!(out.stdout.is_empty(), "{version}: no notes are printed");
        assert!(stderr.starts_with("error: "), "{version}: {stderr}");
        assert!(stderr.contains(named), "{version}: {stderr}");
    }
    let missing = dir.path().join("NEWS.md");
    let out = notes(&["2.0.0"], Some(&missing));
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    assert!(
        text(&out.stderr).contains("NEWS.md does not exist"),
        "{out:?}"
    );
    let usage = notes(&[], None);
    assert_eq!(usage.status.code(), Some(1), "{usage:?}");
    assert!(text(&usage.stderr).contains("usage:"), "{usage:?}");
}

#[test]
fn this_version_has_notes_and_they_install_as_the_readme_does() {
    let root = PathBuf::from(ROOT);
    let out = notes(&[VERSION], Some(&root.join("CHANGELOG.md")));
    let notes = text(&out.stdout);
    assert!(out.status.success(), "{out:?}");
    assert!(
        notes.contains(&format!("\n```sh\n{INSTALL}\n```\n")),
        "{notes}"
    );
    assert!(
        !notes.contains(&format!("## {VERSION}")) && !notes.starts_with('\n'),
        "the notes start with the section's text: {notes}"
    );
    let readme = std::fs::read_to_string(root.join("README.md")).unwrap();
    assert!(
        readme.contains(&format!("\n{INSTALL}\n")),
        "the README shows the same command"
    );
}

fn package(binary: &Path, archive: &Path) -> Output {
    let script = PathBuf::from(ROOT).join("scripts/package-release.sh");
    let mut packing = Command::new("sh");
    packing.arg(script).arg(binary).arg(archive);
    packing.output().expect("running package-release.sh")
}

#[test]
fn an_archive_holds_archon_and_the_licence_files_of_this_checkout() {
    let dir = tempfile::tempdir().unwrap();
    let (binary, archive) = (dir.path().join("built"), dir.path().join("a.tar.gz"));
    write_script(&binary, "#!/bin/sh\necho archon 9.9.9\n");
    let out = package(&binary, &archive);
    assert!(out.status.success(), "{out:?}");
    assert_eq!(archive_members(&archive), ARCHIVE_MEMBERS);
    assert_eq!(
        archive_member(&archive, "archon"),
        std::fs::read(&binary).unwrap()
    );
    for notice in ["LICENSE", "THIRD-PARTY-NOTICES"] {
        let here = std::fs::read(PathBuf::from(ROOT).join(notice)).unwrap();
        assert!(!here.is_empty(), "{notice} has text");
        assert_eq!(archive_member(&archive, notice), here, "{notice}");
    }
    let unpacked = tempfile::tempdir().unwrap();
    // `-p`: the modes as packed, not narrowed by this process's umask.
    let extracted = Command::new("tar")
        .arg("-xpzf")
        .arg(&archive)
        .arg("-C")
        .arg(unpacked.path())
        .status()
        .unwrap();
    assert!(extracted.success(), "the archive unpacks");
    let mode = |name: &str| {
        let meta = std::fs::metadata(unpacked.path().join(name)).unwrap();
        meta.permissions().mode() & 0o777
    };
    assert_eq!(mode("archon"), 0o755, "archon runs");
    for notice in ["LICENSE", "THIRD-PARTY-NOTICES"] {
        assert_eq!(
            mode(notice),
            0o644,
            "{notice} is readable whatever the checkout's umask"
        );
    }
}

#[test]
fn a_binary_that_cannot_run_is_not_packed() {
    let dir = tempfile::tempdir().unwrap();
    let (binary, archive) = (dir.path().join("built"), dir.path().join("a.tar.gz"));
    std::fs::write(&binary, "not executable").unwrap();
    for candidate in [binary, dir.path().join("absent")] {
        let out = package(&candidate, &archive);
        assert_eq!(out.status.code(), Some(1), "{out:?}");
        assert!(
            text(&out.stderr).contains("is not an executable file"),
            "{out:?}"
        );
        assert!(!archive.exists(), "no archive is written");
    }
}
