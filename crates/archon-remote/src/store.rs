//! `remote.json` in archond's data directory: whether the listener is
//! enabled, the origin allowlist and the paired devices. Mode 0600, and
//! replaced whole through a private temporary file, so it is never torn.

use std::io::Write as _;
use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};

use archon_core::error::ArchonError;
use serde::{Deserialize, Serialize};

use crate::devices::Device;

pub const FILE_NAME: &str = "remote.json";
const VERSION: u32 = 1;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Stored {
    pub version: u32,
    pub enabled: bool,
    pub public_origins: Vec<String>,
    pub devices: Vec<Device>,
}

/// Read the file; a missing one is the default (off, nothing paired). A
/// file others can read is tightened, since it holds credential hashes.
pub fn load(path: &Path) -> Result<Stored, ArchonError> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Stored {
                version: VERSION,
                ..Stored::default()
            });
        }
        Err(e) => return Err(ArchonError::io(format!("reading {}", path.display()), e)),
    };
    tighten(path)?;
    let stored: Stored = serde_json::from_str(&text).map_err(|e| {
        ArchonError::Config(format!(
            "{} is not valid ({e}); move it away to reset remote access",
            path.display()
        ))
    })?;
    if stored.version != VERSION {
        return Err(ArchonError::Config(format!(
            "{} has version {}, this archond reads {VERSION}",
            path.display(),
            stored.version
        )));
    }
    Ok(stored)
}

/// Replace the file with `stored`, created 0600 before any byte is written.
pub fn save(path: &Path, stored: &Stored) -> Result<(), ArchonError> {
    assert_eq!(
        stored.version, VERSION,
        "only the current version is written"
    );
    let dir = path
        .parent()
        .ok_or_else(|| ArchonError::Invalid(format!("{} has no directory", path.display())))?;
    let temporary = temporary_path(path);
    let json = serde_json::to_vec_pretty(stored)
        .map_err(|e| ArchonError::Invalid(format!("encoding {FILE_NAME}: {e}")))?;
    let written = write_private(&temporary, &json);
    if let Err(error) = written {
        let _removed = std::fs::remove_file(&temporary);
        return Err(error);
    }
    std::fs::rename(&temporary, path)
        .map_err(|e| ArchonError::io(format!("replacing {}", path.display()), e))?;
    // The rename is durable only once the directory is.
    std::fs::File::open(dir)
        .and_then(|d| d.sync_all())
        .map_err(|e| ArchonError::io(format!("syncing {}", dir.display()), e))
}

fn temporary_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_owned();
    name.push(format!(".{}.tmp", std::process::id()));
    path.with_file_name(name)
}

fn write_private(path: &Path, bytes: &[u8]) -> Result<(), ArchonError> {
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .map_err(|e| ArchonError::io(format!("creating {}", path.display()), e))?;
    // An existing temporary file keeps its mode through `create`.
    file.set_permissions(std::fs::Permissions::from_mode(0o600))
        .map_err(|e| ArchonError::io(format!("protecting {}", path.display()), e))?;
    file.write_all(bytes)
        .and_then(|()| file.sync_all())
        .map_err(|e| ArchonError::io(format!("writing {}", path.display()), e))
}

fn tighten(path: &Path) -> Result<(), ArchonError> {
    let mode = std::fs::metadata(path)
        .map_err(|e| ArchonError::io(format!("inspecting {}", path.display()), e))?
        .permissions()
        .mode();
    if mode & 0o077 == 0 {
        return Ok(());
    }
    tracing::warn!(path = %path.display(), mode = format!("{:o}", mode & 0o777), "tightening remote.json to 0600");
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .map_err(|e| ArchonError::io(format!("protecting {}", path.display()), e))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn saved_state_round_trips_with_mode_0600() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE_NAME);
        let fresh = load(&path).unwrap();
        assert!(!fresh.enabled && fresh.devices.is_empty());
        let issued = Device::issue("Phone".into(), archon_core::now()).unwrap();
        let stored = Stored {
            enabled: true,
            public_origins: vec!["https://box.ts.net".into()],
            devices: vec![issued.device],
            ..fresh
        };
        save(&path, &stored).unwrap();
        assert_eq!(load(&path).unwrap(), stored);
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains(issued.credential.split_once('.').unwrap().1));
    }

    #[test]
    fn a_readable_file_is_tightened_and_a_bad_one_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE_NAME);
        save(&path, &load(&path).unwrap()).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        load(&path).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        std::fs::write(&path, "{\"version\":1,\"bogus\":true}").unwrap();
        let refused = load(&path).unwrap_err().to_string();
        assert!(refused.contains("move it away"), "{refused}");
    }
}
