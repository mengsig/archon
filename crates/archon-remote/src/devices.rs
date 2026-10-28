//! Paired devices and their credentials. A device holds `<id>.<secret>` in
//! an HttpOnly cookie; archond keeps only the secret's SHA-256.

use archon_core::Timestamp;
use archon_core::error::ArchonError;
use base64::Engine as _;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use crate::pairing::constant_time_eq;

pub const MAX_NAME_CHARS: usize = 64;
/// Devices kept at once; pairing another first needs a revoke.
pub const MAX_DEVICES: usize = 64;
const SECRET_BYTES: usize = 32;
const ID_BYTES: usize = 12;
const ID_PREFIX: &str = "dev_";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Device {
    pub id: String,
    pub name: String,
    /// Lowercase hex SHA-256 of the secret's bytes.
    pub secret_sha256: String,
    pub paired_at: Timestamp,
    #[serde(default)]
    pub last_seen_at: Option<Timestamp>,
}

/// A credential just issued: the device and the cookie value it carries.
#[derive(Debug)]
pub struct Issued {
    pub device: Device,
    pub credential: String,
}

impl Device {
    /// A new device named `name` with a fresh secret.
    pub fn issue(name: String, now: Timestamp) -> Result<Issued, ArchonError> {
        assert!(!name.is_empty(), "names are normalized first");
        let mut secret = [0u8; SECRET_BYTES];
        let mut id = [0u8; ID_BYTES];
        getrandom::fill(&mut secret)
            .and_then(|()| getrandom::fill(&mut id))
            .map_err(|e| {
                ArchonError::Unavailable(format!("no randomness for a device secret: {e}"))
            })?;
        let secret = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(secret);
        let id = format!("{ID_PREFIX}{}", hex(&id));
        let device = Self {
            secret_sha256: digest(&secret),
            id,
            name,
            paired_at: now,
            last_seen_at: None,
        };
        let credential = format!("{}.{secret}", device.id);
        Ok(Issued { device, credential })
    }

    /// Whether `secret` is this device's.
    fn holds(&self, secret: &str) -> bool {
        constant_time_eq(digest(secret).as_bytes(), self.secret_sha256.as_bytes())
    }
}

fn digest(secret: &str) -> String {
    hex(&Sha256::digest(secret.as_bytes()))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// The device a cookie value names, if its secret matches.
pub fn authenticate<'a>(devices: &'a [Device], credential: &str) -> Option<&'a Device> {
    let (id, secret) = credential.split_once('.')?;
    if !id.starts_with(ID_PREFIX) || secret.len() < SECRET_BYTES {
        return None;
    }
    let device = devices.iter().find(|d| d.id == id)?;
    device.holds(secret).then_some(device)
}

/// The name as stored and shown: whitespace runs become one space, and a
/// name with a control, invisible or direction-changing character is refused.
pub fn normalize_name(name: &str) -> Result<String, String> {
    if let Some(bad) = name.chars().find(|c| invisible(*c)) {
        return Err(format!(
            "a device name cannot hold U+{:04X}; use visible characters",
            u32::from(bad)
        ));
    }
    let name = name.split_whitespace().collect::<Vec<_>>().join(" ");
    let chars = name.chars().count();
    if chars == 0 || chars > MAX_NAME_CHARS {
        return Err(format!("a device name is 1 to {MAX_NAME_CHARS} characters"));
    }
    Ok(name)
}

/// Characters that display as nothing or reorder what follows them. Plain
/// whitespace other than a space is folded by the caller, except controls.
fn invisible(c: char) -> bool {
    let code = u32::from(c);
    (c.is_control() && c != ' ')
        || matches!(code,
            0x00AD | 0x034F | 0x061C | 0x115F | 0x1160 | 0x17B4 | 0x17B5 | 0x180B..=0x180F
            | 0x200B..=0x200F | 0x202A..=0x202E | 0x2060..=0x206F | 0x2800 | 0x3164
            | 0xFE00..=0xFE0F | 0xFEFF | 0xFFA0 | 0xFFF0..=0xFFFB
            | 0xE000..=0xF8FF | 0xE0000..=0xE0FFF | 0xF0000..=0x10FFFF)
        || (code & 0xFFFE) == 0xFFFE
        || (0xFDD0..=0xFDEF).contains(&code)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_credential_authenticates_only_its_own_device() {
        let now = archon_core::now();
        let a = Device::issue("Phone".into(), now).unwrap();
        let b = Device::issue("Tablet".into(), now).unwrap();
        let devices = vec![a.device.clone(), b.device.clone()];
        assert_eq!(
            authenticate(&devices, &a.credential).map(|d| &d.id),
            Some(&a.device.id)
        );
        let (id, _) = a.credential.split_once('.').unwrap();
        let (_, other_secret) = b.credential.split_once('.').unwrap();
        assert!(authenticate(&devices, &format!("{id}.{other_secret}")).is_none());
        assert!(authenticate(&devices, id).is_none());
        assert!(
            authenticate(&devices[1..], &a.credential).is_none(),
            "a revoked device fails"
        );
        assert!(
            !a.device
                .secret_sha256
                .contains(a.credential.split_once('.').unwrap().1)
        );
    }

    #[test]
    fn names_fold_whitespace_and_refuse_invisible_characters() {
        assert_eq!(
            normalize_name("  Robin's\u{00A0}  iPhone ").unwrap(),
            "Robin's iPhone"
        );
        assert_eq!(normalize_name("Phone 📱").unwrap(), "Phone 📱");
        for bad in [
            "",
            "   ",
            "a\u{202E}b",
            "a\u{200B}b",
            "a\tb",
            "a\nb",
            "a\u{FE0F}",
        ] {
            assert!(normalize_name(bad).is_err(), "{bad:?} was accepted");
        }
        assert!(normalize_name(&"x".repeat(65)).is_err());
        assert!(normalize_name(&"x".repeat(64)).is_ok());
    }
}
