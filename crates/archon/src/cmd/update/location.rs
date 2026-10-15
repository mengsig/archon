//! Whether this archon sits where an update may replace it, and the
//! replacement itself: staged beside the binary, then renamed over it.

use std::ffi::OsStr;
use std::fs::DirEntry;
use std::io::ErrorKind;
use std::os::unix::fs::MetadataExt as _;
use std::path::Path;
use std::time::{Duration, SystemTime};

use anyhow::Context as _;

use super::release::{BINARIES, installer_command};

/// Files cargo leaves at the top of a build directory.
const BUILD_DIR_MARKERS: [&str; 2] = ["CACHEDIR.TAG", ".rustc_info.json"];
const STAGING_PREFIX: &str = ".archon-update.";
/// Longer than any update runs: a staging directory untouched for this long
/// belongs to an update that was killed, not to one that is still running.
const STALE_AFTER: Duration = Duration::from_secs(60 * 60);

/// Why the archon at `exe` is not the installer's to replace, with the
/// command that updates it instead; `None` when an update may replace it.
pub fn refusal(exe: &Path, cargo_bin: Option<&Path>) -> Option<String> {
    assert!(exe.is_absolute(), "{} is not absolute", exe.display());
    let dir = exe.parent()?;
    if exe.file_name() != Some(OsStr::new(BINARIES[0])) {
        return Some(format!(
            "{} is not named `{}`, so `archon update` cannot tell which file to replace. \
             Install a release with `{}`.",
            exe.display(),
            BINARIES[0],
            installer_command()
        ));
    }
    if cargo_bin.is_some_and(|bin| bin == dir) {
        return Some(format!(
            "{} was installed by cargo, which `archon update` does not replace. Update it from \
             a checkout with `cargo install --locked --path crates/archon`, or install a \
             release with `{}`.",
            exe.display(),
            installer_command()
        ));
    }
    // target/<profile>/archon or target/<triple>/<profile>/archon.
    let build_dir = dir.ancestors().skip(1).take(2).find(|dir| {
        BUILD_DIR_MARKERS
            .iter()
            .any(|name| dir.join(name).is_file())
    })?;
    Some(format!(
        "{} is a build in {}, which `archon update` does not replace. Rebuild it with \
         `cargo build --release`, or install a release with `{}`.",
        exe.display(),
        build_dir.display(),
        installer_command()
    ))
}

pub use archon_core::paths::cargo_bin_dir;

/// A private directory beside the binaries, so the staged files are on their
/// filesystem and a rename over them is atomic. Removed when dropped.
pub fn stage_in(dir: &Path) -> anyhow::Result<tempfile::TempDir> {
    assert!(dir.is_dir(), "{} is not a directory", dir.display());
    let staging = tempfile::Builder::new()
        .prefix(STAGING_PREFIX)
        .tempdir_in(dir);
    match staging {
        Ok(staging) => Ok(staging),
        Err(error) if error.kind() == ErrorKind::PermissionDenied => Err(anyhow::anyhow!(
            "{} is not writable by you, so `archon update` cannot replace the archon in it. \
             Update it the way it was installed, or install a release you own with `{}`.",
            dir.display(),
            installer_command()
        )),
        Err(error) => Err(error).with_context(|| format!("staging in {}", dir.display())),
    }
}

/// Remove the staging directories that killed updates of this user left in
/// `dir`. `staging` is this update's own: it names the user and is kept.
pub fn remove_stale(dir: &Path, staging: &Path, now: SystemTime) -> anyhow::Result<()> {
    assert_eq!(
        staging.parent(),
        Some(dir),
        "{} is not staged in {}",
        staging.display(),
        dir.display()
    );
    let listing = || format!("listing {}", dir.display());
    let owner = std::fs::symlink_metadata(staging)
        .with_context(|| format!("reading {}", staging.display()))?
        .uid();
    for entry in std::fs::read_dir(dir).with_context(listing)? {
        let entry = entry.with_context(listing)?;
        let path = entry.path();
        if path == staging || !is_stale(&entry, owner, now)? {
            continue;
        }
        match std::fs::remove_dir_all(&path) {
            Ok(()) => {}
            // Another update that started at the same moment removed it.
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => {
                let doing = format!("removing the stale staging directory {}", path.display());
                return Err(error).context(doing);
            }
        }
    }
    Ok(())
}

/// Whether `entry` is a staging directory of the user `owner` that no
/// running update can still be using.
fn is_stale(entry: &DirEntry, owner: u32, now: SystemTime) -> anyhow::Result<bool> {
    let name = entry.file_name();
    if !name
        .as_encoded_bytes()
        .starts_with(STAGING_PREFIX.as_bytes())
    {
        return Ok(false);
    }
    // Not followed: a symbolic link with a staging name is never removed.
    let metadata = match entry.metadata() {
        Ok(metadata) => metadata,
        // A running update removed its own staging directory just now.
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(false),
        Err(error) => {
            return Err(error).with_context(|| format!("reading {}", entry.path().display()));
        }
    };
    if !metadata.is_dir() || metadata.uid() != owner {
        return Ok(false);
    }
    let modified = metadata
        .modified()
        .with_context(|| format!("reading the age of {}", entry.path().display()))?;
    // A time in the future is no proof of age, so that directory is kept.
    Ok(now
        .duration_since(modified)
        .is_ok_and(|age| age > STALE_AFTER))
}

/// Rename each staged binary over the installed one. Every binary is
/// checked first, so a missing one fails before anything is replaced.
pub fn replace(staged: &Path, dir: &Path, binaries: &[&str]) -> anyhow::Result<()> {
    assert!(!binaries.is_empty(), "a release ships at least one binary");
    for binary in binaries {
        let from = staged.join(binary);
        anyhow::ensure!(from.is_file(), "{} was not staged", from.display());
    }
    for binary in binaries {
        let (from, to) = (staged.join(binary), dir.join(binary));
        std::fs::rename(&from, &to).with_context(|| format!("replacing {}", to.display()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt as _;

    use super::*;

    #[test]
    fn a_cargo_install_is_refused_with_the_cargo_command() {
        let home = tempfile::tempdir().unwrap();
        let bin = home.path().join("bin");
        std::fs::create_dir(&bin).unwrap();
        let refused = refusal(&bin.join("archon"), Some(&bin)).unwrap();
        assert!(refused.contains("installed by cargo"), "{refused}");
        assert!(
            refused.contains("cargo install --locked --path crates/archon"),
            "{refused}"
        );
        assert!(refused.contains(&installer_command()), "{refused}");
    }

    #[test]
    fn a_build_directory_is_refused_and_an_installer_directory_is_not() {
        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("target");
        let release = target.join("x86_64-unknown-linux-musl/release");
        std::fs::create_dir_all(&release).unwrap();
        std::fs::write(target.join("CACHEDIR.TAG"), "").unwrap();
        let refused = refusal(&release.join("archon"), None).unwrap();
        assert!(refused.contains("is a build in"), "{refused}");
        assert!(refused.contains("cargo build --release"), "{refused}");

        let debug = target.join("debug");
        std::fs::create_dir(&debug).unwrap();
        assert!(refusal(&debug.join("archon"), None).is_some());

        // Deeper than cargo's own layout is somebody's directory, not a build.
        let local = target.join("tmp/home/bin");
        std::fs::create_dir_all(&local).unwrap();
        let cargo = root.path().join("cargo/bin");
        assert_eq!(refusal(&local.join("archon"), Some(&cargo)), None);
        assert_eq!(refusal(&local.join("archon"), None), None);
        let renamed = refusal(&local.join("archon-old"), None).unwrap();
        assert!(renamed.contains("is not named `archon`"), "{renamed}");
    }

    #[test]
    fn a_directory_that_is_not_writable_is_refused_before_any_download() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("bin");
        std::fs::create_dir(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o555)).unwrap();
        let outcome = stage_in(&dir);
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        // Root writes anywhere, so there the directory is simply usable.
        let uid = std::process::Command::new("id").arg("-u").output().unwrap();
        if uid.stdout.trim_ascii() == b"0" {
            return;
        }
        let refused = outcome.unwrap_err().to_string();
        assert!(refused.contains("is not writable by you"), "{refused}");
        assert!(refused.contains(&installer_command()), "{refused}");
    }

    #[test]
    fn staged_binaries_replace_the_installed_ones_and_staging_leaves_nothing() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path();
        std::fs::write(dir.join("archon"), "old").unwrap();
        let staging = stage_in(dir).unwrap();
        let error = replace(staging.path(), dir, &["archon"]).unwrap_err();
        assert!(error.to_string().contains("was not staged"), "{error}");
        assert_eq!(std::fs::read_to_string(dir.join("archon")).unwrap(), "old");

        std::fs::write(staging.path().join("archon"), "new").unwrap();
        replace(staging.path(), dir, &["archon"]).unwrap();
        drop(staging);
        assert_eq!(std::fs::read_to_string(dir.join("archon")).unwrap(), "new");
        let left: Vec<_> = std::fs::read_dir(dir).unwrap().collect();
        assert_eq!(left.len(), 1, "only the binary remains: {left:?}");
    }

    fn names_in(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        names
    }

    fn set_modified(path: &Path, time: SystemTime) {
        let opened = std::fs::File::open(path).unwrap();
        opened.set_modified(time).unwrap();
    }

    #[test]
    fn only_old_staging_directories_are_removed_as_stale() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path();
        std::fs::write(dir.join("archon"), "installed").unwrap();
        let staging = stage_in(dir).unwrap();
        let own = staging.path().file_name().unwrap().to_str().unwrap();
        let (killed, running) = (".archon-update.killed", ".archon-update.running");
        let (ahead, unrelated) = (".archon-update.ahead", "other");
        for name in [killed, running, ahead, unrelated] {
            std::fs::create_dir(dir.join(name)).unwrap();
        }
        std::fs::write(dir.join(killed).join("archive.tar.gz"), "half").unwrap();
        std::fs::write(dir.join(".archon-update.file"), "not a directory").unwrap();
        std::os::unix::fs::symlink(unrelated, dir.join(".archon-update.link")).unwrap();
        let now = SystemTime::now();
        let old = now - STALE_AFTER - Duration::from_secs(60);
        for name in [killed, unrelated, ".archon-update.file", own] {
            set_modified(&dir.join(name), old);
        }
        set_modified(&dir.join(ahead), now + STALE_AFTER * 2);

        remove_stale(dir, staging.path(), now).unwrap();
        let mut kept = vec![running, ahead, unrelated, "archon", own];
        kept.extend([".archon-update.file", ".archon-update.link"]);
        kept.sort_unstable();
        assert_eq!(names_in(dir), kept, "only the old staging directory goes");
        assert_eq!(
            std::fs::read_to_string(dir.join("archon")).unwrap(),
            "installed"
        );
    }
}
