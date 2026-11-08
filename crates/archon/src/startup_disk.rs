//! Low-disk diagnosis before starting archond.

use archon_core::config::Config;
use std::path::Path;

/// Report the same disk floor the daemon uses, without collecting anything.
pub(crate) fn warn_startup_disk(config: &Config, bin: &Path) {
    let floor = config.limits.min_free_bytes();
    if floor == 0 {
        return;
    }
    let paths = archon_daemon::context::DaemonPaths::resolve(config, bin.to_owned());
    let probe: archon_daemon::disk::FreeSpaceProbe =
        std::sync::Arc::new(archon_daemon::disk::free_bytes);
    match archon_daemon::disk::lowest(&probe, &[paths.data_dir, paths.worktrees]) {
        Ok(reading) => {
            if let Some(message) = startup_disk_message(&reading, floor) {
                eprintln!("archon: warning: {message}");
            }
        }
        Err(error) => eprintln!("archon: warning: startup free space unknown: {error}"),
    }
}

fn startup_disk_message(reading: &archon_daemon::disk::DiskReading, floor: u64) -> Option<String> {
    if floor == 0 || reading.free_bytes >= floor {
        return None;
    }
    Some(format!(
        "{} bytes free at {}, below limits.min_free_gb ({} bytes); dispatch will be held. Run `archon gc` to reclaim space; if archond cannot start, run `archon gc --offline`.",
        reading.free_bytes,
        reading.path.display(),
        floor
    ))
}

#[cfg(test)]
mod tests {
    #[test]
    fn startup_disk_warning_obeys_the_floor_and_names_recovery() {
        let mut reading = archon_daemon::disk::DiskReading {
            path: "/worktrees".into(),
            free_bytes: 99,
        };
        let warning = super::startup_disk_message(&reading, 100).unwrap();
        assert!(warning.contains("99 bytes free at /worktrees"));
        assert!(warning.contains("100 bytes"));
        assert!(warning.contains("`archon gc`") && warning.contains("`archon gc --offline`"));
        assert!(super::startup_disk_message(&reading, 0).is_none());
        reading.free_bytes = 100;
        assert!(super::startup_disk_message(&reading, 100).is_none());
    }
}
