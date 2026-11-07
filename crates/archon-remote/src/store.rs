//! `remote.json` in archond's data directory: whether the listener is
//! enabled, the origin allowlist and the paired devices. Mode 0600, and
//! replaced whole through a private temporary file, so it is never torn.
//!
//! Format 1 has no device roles, and its devices are operators. Format 2
//! names each device's role. A file is written in format 1 while every
//! device is an operator, so an older archond still reads it, and one it
//! cannot read never makes an observer an operator.

use std::io::Write as _;
use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};

use archon_core::error::ArchonError;
use archon_core::protocol::DeviceRole;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::devices::Device;

pub const FILE_NAME: &str = "remote.json";
/// The format of a file whose devices are all operators.
const FORMAT_WITHOUT_ROLES: u64 = 1;
/// The format that names each device's role.
const FORMAT_WITH_ROLES: u64 = 2;

/// The file's content; its `version` is the format, chosen on save.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Stored {
    pub enabled: bool,
    pub public_origins: Vec<String>,
    pub devices: Vec<Device>,
}

impl Stored {
    /// The oldest format that holds every device's role.
    fn format(&self) -> u64 {
        match self.devices.iter().all(|d| d.role == DeviceRole::Operator) {
            true => FORMAT_WITHOUT_ROLES,
            false => FORMAT_WITH_ROLES,
        }
    }
}

/// Read the file; a missing one is the default (off, nothing paired). A
/// file others can read is tightened, since it holds credential hashes.
pub fn load(path: &Path) -> Result<Stored, ArchonError> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Stored::default()),
        Err(e) => return Err(ArchonError::io(format!("reading {}", path.display()), e)),
    };
    tighten(path)?;
    let invalid = |why: String| {
        ArchonError::Config(format!(
            "{} is not valid ({why}); move it away to reset remote access",
            path.display()
        ))
    };
    let mut file: Map<String, Value> =
        serde_json::from_str(&text).map_err(|e| invalid(e.to_string()))?;
    let format = file.remove("version");
    match format.as_ref().and_then(Value::as_u64) {
        Some(format @ (FORMAT_WITHOUT_ROLES | FORMAT_WITH_ROLES)) => {
            check_roles(&file, format).map_err(invalid)?;
        }
        _ => {
            let found = format.map_or_else(|| "no version".to_owned(), |v| format!("version {v}"));
            return Err(ArchonError::Config(format!(
                "{} has {found}, this archond reads formats \
                 {FORMAT_WITHOUT_ROLES} and {FORMAT_WITH_ROLES}",
                path.display()
            )));
        }
    }
    serde_json::from_value(Value::Object(file)).map_err(|e| invalid(e.to_string()))
}

/// Format 1 names no role, and format 2 names every device's.
fn check_roles(file: &Map<String, Value>, format: u64) -> Result<(), String> {
    let devices = file.get("devices").and_then(Value::as_array);
    for device in devices.into_iter().flatten() {
        let named = device.as_object().is_some_and(|d| d.contains_key("role"));
        match (format, named) {
            (FORMAT_WITHOUT_ROLES, true) => return Err("format 1 names no device role".into()),
            (FORMAT_WITH_ROLES, false) => return Err("format 2 names each device's role".into()),
            _ => {}
        }
    }
    Ok(())
}

/// `stored` as the file holds it, in [`Stored::format`].
fn encode(stored: &Stored) -> Result<Vec<u8>, ArchonError> {
    let encoding =
        |e: serde_json::Error| ArchonError::Invalid(format!("encoding {FILE_NAME}: {e}"));
    let format = stored.format();
    let Value::Object(mut file) = serde_json::to_value(stored).map_err(encoding)? else {
        unreachable!("a struct encodes as an object");
    };
    if format == FORMAT_WITHOUT_ROLES {
        let devices = file.get_mut("devices").and_then(Value::as_array_mut);
        for device in devices.into_iter().flatten() {
            let role = device.as_object_mut().and_then(|d| d.remove("role"));
            assert_eq!(
                role,
                Some(Value::from("operator")),
                "format 1 holds operators only"
            );
        }
    }
    let previous = file.insert("version".to_owned(), Value::from(format));
    assert!(previous.is_none(), "the format is not a stored field");
    serde_json::to_vec_pretty(&file).map_err(encoding)
}

/// Replace the file with `stored`, created 0600 before any byte is written.
pub fn save(path: &Path, stored: &Stored) -> Result<(), ArchonError> {
    let dir = path
        .parent()
        .ok_or_else(|| ArchonError::Invalid(format!("{} has no directory", path.display())))?;
    let temporary = temporary_path(path);
    let json = encode(stored)?;
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
        let issued =
            Device::issue("Phone".into(), DeviceRole::Observer, archon_core::now()).unwrap();
        let stored = Stored {
            enabled: true,
            public_origins: vec!["https://box.ts.net".into()],
            devices: vec![issued.device],
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

    /// A file as archond 1.4 writes it, with one paired device.
    const FORMAT_1: &str = r#"{
  "version": 1,
  "enabled": true,
  "public_origins": ["https://box.ts.net"],
  "devices": [{
    "id": "dev_0123456789abcdef01234567",
    "name": "Phone",
    "secret_sha256": "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08",
    "paired_at": "2026-10-01T12:00:00Z",
    "last_seen_at": null
  }]
}"#;

    #[test]
    fn a_format_1_file_loads_as_operators_and_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE_NAME);
        write_private(&path, FORMAT_1.as_bytes()).unwrap();
        let stored = load(&path).unwrap();
        assert_eq!(stored.devices.len(), 1);
        assert_eq!(stored.devices[0].role, DeviceRole::Operator);
        save(&path, &stored).unwrap();
        let written: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        let original: Value = serde_json::from_str(FORMAT_1).unwrap();
        assert_eq!(written, original, "rewritten unchanged, still format 1");
        assert_eq!(load(&path).unwrap(), stored);
    }

    #[test]
    fn an_observer_is_written_in_format_2_and_each_format_keeps_its_rule() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE_NAME);
        write_private(&path, FORMAT_1.as_bytes()).unwrap();
        let mut stored = load(&path).unwrap();
        stored.devices[0].role = DeviceRole::Observer;
        save(&path, &stored).unwrap();
        let written: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(written["version"], 2);
        assert_eq!(written["devices"][0]["role"], "observer");
        assert_eq!(load(&path).unwrap(), stored);
        let refusals = [
            (
                FORMAT_1.replace("\"name\"", "\"role\": \"observer\", \"name\""),
                "names no",
            ),
            (
                FORMAT_1.replace("\"version\": 1", "\"version\": 2"),
                "each device",
            ),
            (
                FORMAT_1.replace("\"version\": 1", "\"version\": 3"),
                "version 3",
            ),
            (FORMAT_1.replace("\"version\": 1,", ""), "no version"),
        ];
        for (text, why) in refusals {
            write_private(&path, text.as_bytes()).unwrap();
            let refused = load(&path).unwrap_err().to_string();
            assert!(refused.contains(why), "{refused}");
        }
    }
}
