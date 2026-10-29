//! The long-lived services a stage declares for a later stage of its run:
//! one PID per `.archon/services/<name>.pid` file in the run's worktree. A
//! stage end spares each, its process group and its descendants; the run's
//! end reaps them with everything else.

use std::collections::BTreeSet;
use std::io::ErrorKind;
use std::path::Path;

use archon_core::error::ArchonError;

/// Where a stage declares its services, relative to the run's worktree.
pub const SERVICES_DIR: &str = ".archon/services";

/// The most PID files read; a stage serving more is refused the rest.
const MAX_FILES: usize = 64;

/// A PID file is a number and a newline; anything longer is not one.
const MAX_FILE_BYTES: u64 = 64;

/// The PIDs declared under `workspace`. Malformed or oversized files, links
/// and pid 1 are skipped with a warning, so what they name is reaped as before.
pub fn declared(workspace: &Path) -> Result<BTreeSet<i32>, ArchonError> {
    assert!(workspace.is_absolute(), "run workspaces are absolute");
    let dir = workspace.join(SERVICES_DIR);
    let context = || format!("reading {}", dir.display());
    let meta = match std::fs::symlink_metadata(&dir) {
        Ok(meta) => meta,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(BTreeSet::new()),
        Err(e) => return Err(ArchonError::io(context(), e)),
    };
    if !meta.is_dir() {
        tracing::warn!(dir = %dir.display(), "not a directory, so no service is kept");
        return Ok(BTreeSet::new());
    }
    let mut files = Vec::new();
    for entry in std::fs::read_dir(&dir).map_err(|e| ArchonError::io(context(), e))? {
        let path = entry.map_err(|e| ArchonError::io(context(), e))?.path();
        if path.extension().is_some_and(|ext| ext == "pid") {
            files.push(path);
        }
    }
    files.sort();
    if files.len() > MAX_FILES {
        tracing::warn!(dir = %dir.display(), count = files.len(), "only the first {MAX_FILES} service files are kept");
        files.truncate(MAX_FILES);
    }
    let pids: BTreeSet<i32> = files.iter().filter_map(|file| pid_in(file)).collect();
    assert!(pids.len() <= MAX_FILES, "one PID per file read");
    assert!(pids.iter().all(|&pid| pid > 1), "pid 1 is never kept");
    Ok(pids)
}

/// The PID `file` holds, or `None` with a warning when it holds none.
fn pid_in(file: &Path) -> Option<i32> {
    let skip = |why: &str| {
        tracing::warn!(file = %file.display(), "service file skipped: {why}");
        None
    };
    let meta = match std::fs::symlink_metadata(file) {
        Ok(meta) => meta,
        Err(e) => return skip(&e.to_string()),
    };
    if !meta.is_file() || meta.len() > MAX_FILE_BYTES {
        return skip("not a small regular file");
    }
    let text = match std::fs::read_to_string(file) {
        Ok(text) => text,
        Err(e) => return skip(&e.to_string()),
    };
    match text.trim().parse::<i32>() {
        Ok(pid) if pid > 1 => Some(pid),
        _ => skip("not one PID above 1"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, name: &str, text: &str) {
        std::fs::write(dir.join(name), text).unwrap();
    }

    #[test]
    fn reads_one_pid_per_pid_file_and_skips_the_rest() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace = tmp.path().canonicalize().unwrap();
        assert!(declared(&workspace).unwrap().is_empty(), "none declared");
        let dir = workspace.join(SERVICES_DIR);
        std::fs::create_dir_all(&dir).unwrap();
        write(&dir, "web.pid", "4242\n");
        write(&dir, "db.pid", "  77 ");
        write(&dir, "notes.txt", "99");
        write(&dir, "init.pid", "1");
        write(&dir, "word.pid", "pid 5");
        write(&dir, "two.pid", "5\n6\n");
        write(&dir, "huge.pid", &"7".repeat(100));
        std::os::unix::fs::symlink(dir.join("web.pid"), dir.join("link.pid")).unwrap();
        std::fs::create_dir(dir.join("sub.pid")).unwrap();
        assert_eq!(declared(&workspace).unwrap(), BTreeSet::from([77, 4242]));
    }

    #[test]
    fn a_services_path_that_is_no_directory_keeps_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace = tmp.path().canonicalize().unwrap();
        std::fs::create_dir_all(workspace.join(".archon")).unwrap();
        write(&workspace, SERVICES_DIR, "4242");
        assert!(declared(&workspace).unwrap().is_empty());
        let elsewhere = workspace.join("elsewhere");
        std::fs::create_dir(&elsewhere).unwrap();
        write(&elsewhere, "web.pid", "4242");
        std::fs::remove_file(workspace.join(SERVICES_DIR)).unwrap();
        std::os::unix::fs::symlink(&elsewhere, workspace.join(SERVICES_DIR)).unwrap();
        assert!(
            declared(&workspace).unwrap().is_empty(),
            "a link leads elsewhere"
        );
    }

    #[test]
    fn only_the_first_files_by_name_are_read() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace = tmp.path().canonicalize().unwrap();
        let dir = workspace.join(SERVICES_DIR);
        std::fs::create_dir_all(&dir).unwrap();
        for n in 0..(MAX_FILES + 5) {
            write(&dir, &format!("s{n:03}.pid"), &format!("{}", 1000 + n));
        }
        let pids = declared(&workspace).unwrap();
        assert_eq!(pids.len(), MAX_FILES);
        assert!(pids.contains(&1000) && !pids.contains(&(1000 + MAX_FILES as i32)));
    }
}
