//! Compare smudged LFS content without executing a configured filter.

use std::ffi::OsStr;
use std::fs::File;
use std::io::{self, Read};
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path};

use archon_core::error::ArchonError;
use rustix::fs::{Mode, OFlags, open, openat};
use rustix::io::Errno;
use sha2::{Digest, Sha256};

use crate::worktree::daemon_git::Config;
use crate::worktree::shadow::Shadow;

// The v1 spec requires the entire pointer, including extensions, to be < 1024 bytes.
const POINTER_LIMIT: u64 = 1024;
const VERSION: &str = "version https://git-lfs.github.com/spec/v1\n";

#[derive(Debug, PartialEq, Eq)]
struct Pointer {
    oid: [u8; 32],
    size: u64,
}

impl Pointer {
    fn parse(blob: &[u8]) -> Option<Self> {
        if blob.len() as u64 >= POINTER_LIMIT {
            return None;
        }
        let text = std::str::from_utf8(blob).ok()?;
        let rest = text.strip_prefix(VERSION)?.strip_suffix('\n')?;
        let (mut oid, mut size, mut previous) = (None, None, "");
        for line in rest.split('\n') {
            let (key, value) = line.split_once(' ')?;
            if key.is_empty()
                || key <= previous
                || key == "version"
                || !key
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'.' || b == b'-')
                || value.is_empty()
                || value.starts_with(' ')
                || value.contains('\r')
            {
                return None;
            }
            previous = key;
            match key {
                "oid" => {
                    let hex = value.strip_prefix("sha256:")?;
                    if hex.len() != 64
                        || !hex
                            .bytes()
                            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
                    {
                        return None;
                    }
                    let mut bytes = [0; 32];
                    for (byte, pair) in bytes.iter_mut().zip(hex.as_bytes().as_chunks::<2>().0) {
                        *byte = u8::from_str_radix(std::str::from_utf8(pair).ok()?, 16).ok()?;
                    }
                    oid = Some(bytes);
                }
                "size" => {
                    if !value.bytes().all(|b| b.is_ascii_digit())
                        || (value.len() > 1 && value.starts_with('0'))
                    {
                        return None;
                    }
                    size = Some(value.parse().ok()?);
                }
                _ => {}
            }
        }
        Some(Self {
            oid: oid?,
            size: size?,
        })
    }
}

/// Raw records name the HEAD blob and both modes, independently of attributes and
/// filename quoting. Only a content modification of a regular file may be waived.
pub(super) fn dirty_paths(shadow: &Shadow, tree: &Path, raw: &[u8]) -> Result<String, ArchonError> {
    if raw.is_empty() {
        return Ok(String::new());
    }
    let raw = raw
        .strip_suffix(&[0])
        .ok_or_else(|| ArchonError::Protocol("git diff-files omitted its final NUL".into()))?;
    let mut records = raw.split(|b| *b == 0);
    let mut dirty = Vec::new();
    while let Some(header) = records.next() {
        let path = records
            .next()
            .ok_or_else(|| ArchonError::Protocol("git diff-files omitted a path".into()))?;
        let header = std::str::from_utf8(header).map_err(|_| {
            ArchonError::Protocol("git diff-files printed a non-UTF-8 header".into())
        })?;
        let fields: Vec<_> = header.split(' ').collect();
        let [old, new, oid, _, status] = fields.as_slice() else {
            return Err(ArchonError::Protocol(format!(
                "invalid git diff-files header: {header:?}"
            )));
        };
        let regular = matches!(*old, ":100644" | ":100755") && &old[1..] == *new;
        let path = Path::new(OsStr::from_bytes(path));
        if regular && *status == "M" && matches_blob(shadow, tree, path, oid)? {
            continue;
        }
        let name = path.to_string_lossy();
        dirty.push(if name.chars().any(char::is_control) {
            format!("{name:?}")
        } else {
            name.into_owned()
        });
    }
    Ok(dirty.join("\n"))
}

fn matches_blob(shadow: &Shadow, tree: &Path, path: &Path, oid: &str) -> Result<bool, ArchonError> {
    let git = || shadow.git(None, Config::Sealed);
    let size = git()?.args(["cat-file", "-s", oid]).text()?;
    let size: u64 = size.parse().map_err(|_| {
        ArchonError::Protocol(format!("git cat-file printed invalid size {size:?}"))
    })?;
    if size >= POINTER_LIMIT {
        return Ok(false);
    }
    // Object ids are immutable: the size check bounds this unfiltered blob read.
    let blob = git()?.args(["cat-file", "blob", oid]).output()?;
    if !blob.status.success() {
        return Err(blob.failure());
    }
    let Some(pointer) = Pointer::parse(&blob.stdout) else {
        return Ok(false);
    };
    match matches_file(tree, path, &pointer, &blob.stdout) {
        Ok(clean) => Ok(clean),
        Err(e)
            if matches!(
                Errno::from_io_error(&e),
                Some(Errno::NOENT | Errno::NOTDIR | Errno::LOOP)
            ) =>
        {
            Ok(false)
        }
        Err(e) => Err(ArchonError::io(
            format!("checking LFS content at {}", tree.join(path).display()),
            e,
        )),
    }
}

/// Directory descriptors also refuse a symlink replacing a parent directory;
/// NONBLOCK prevents a concurrently planted FIFO from hanging the daemon.
fn open_file(tree: &Path, path: &Path) -> io::Result<File> {
    let dir_flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    let mut dir = open(tree, dir_flags, Mode::empty())?;
    let mut components = path.components().peekable();
    while let Some(component) = components.next() {
        let Component::Normal(name) = component else {
            return Err(io::Error::other("git path must be relative without '..'"));
        };
        let flags = if components.peek().is_some() {
            dir_flags
        } else {
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC
        };
        dir = openat(&dir, name, flags, Mode::empty())?;
    }
    Ok(File::from(dir))
}

fn matches_file(tree: &Path, path: &Path, pointer: &Pointer, blob: &[u8]) -> io::Result<bool> {
    let mut file = open_file(tree, path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Ok(false);
    }
    let length = metadata.len();
    if length != pointer.size && length != blob.len() as u64 {
        return Ok(false);
    }
    let mut hash = Sha256::new();
    let mut buffer = [0; 64 * 1024];
    let mut read = 0u64;
    let mut is_pointer = length == blob.len() as u64;
    // Read at most the observed length plus one, even if another writer grows it.
    let mut bounded = (&mut file).take(length.saturating_add(1));
    loop {
        let n = match bounded.read(&mut buffer) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        if is_pointer {
            is_pointer = blob.get(read as usize..read as usize + n) == Some(&buffer[..n]);
        }
        hash.update(&buffer[..n]);
        read += n as u64;
    }
    Ok(read == length
        && (is_pointer || (read == pointer.size && hash.finalize().as_slice() == pointer.oid)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pointer() -> String {
        format!("{VERSION}oid sha256:{}\nsize 3\n", "a".repeat(64))
    }

    #[test]
    fn parses_v1_and_sorted_extra_keys() {
        assert_eq!(
            Pointer::parse(pointer().as_bytes()),
            Some(Pointer {
                oid: [0xaa; 32],
                size: 3
            })
        );
        let extra = pointer().replace("oid sha256:", "ext-0-test sha256:abc\noid sha256:")
            + "z.extra preserved value\n";
        assert!(Pointer::parse(extra.as_bytes()).is_some());
        assert!(Pointer::parse(pointer().replace("size 3", "size 0").as_bytes()).is_some());
    }

    #[test]
    fn refuses_malformed_pointers() {
        let valid = pointer();
        for text in [
            String::new(),
            valid.replace("/v1", "/v2"),
            valid.replace("sha256:", "sha1:"),
            valid.replace(&"a".repeat(64), &"a".repeat(63)),
            valid.replace(&"a".repeat(64), &"a".repeat(65)),
            valid.replace(&"a".repeat(64), &"A".repeat(64)),
            valid.replace(&"a".repeat(64), &"g".repeat(64)),
            valid.replace("size 3", "size -3"),
            valid.replace("size 3", "size +3"),
            valid.replace("size 3", "size 03"),
            valid.replace("size 3", "size 18446744073709551616"),
            valid.replace("size 3\n", ""),
            valid.replace("size 3", "size  3"),
            valid.replace('\n', "\r\n"),
            valid.trim_end().to_owned(),
            format!("{valid}\n"),
            format!("{valid}size 3\n"),
            format!("{valid}aaa unsorted\n"),
            format!("{valid}z_bad value\n"),
            format!("{valid}z empty\rvalue\n"),
            format!("oid sha256:{}\n{VERSION}size 3\n", "a".repeat(64)),
        ] {
            assert!(Pointer::parse(text.as_bytes()).is_none(), "{text:?}");
        }
        assert!(Pointer::parse(&[0xff]).is_none());
    }

    #[test]
    fn bounds_the_entire_pointer() {
        let mut text = pointer();
        text.push_str("z ");
        text.push_str(&"x".repeat(1023 - text.len() - 1));
        text.push('\n');
        assert!(Pointer::parse(text.as_bytes()).is_some());
        text.insert(text.len() - 1, 'x');
        assert_eq!(text.len(), 1024);
        assert!(Pointer::parse(text.as_bytes()).is_none());
    }

    #[test]
    fn streams_content_and_accepts_only_the_exact_unsmudged_pointer() {
        let dir = tempfile::tempdir().unwrap();
        let blob = format!(
            "{VERSION}oid sha256:cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0\nsize 1000000\n"
        );
        let pointer = Pointer::parse(blob.as_bytes()).unwrap();
        let path = Path::new("asset");
        std::fs::write(dir.path().join(path), vec![b'a'; 1_000_000]).unwrap();
        assert!(matches_file(dir.path(), path, &pointer, blob.as_bytes()).unwrap());
        std::fs::write(dir.path().join(path), blob.as_bytes()).unwrap();
        assert!(matches_file(dir.path(), path, &pointer, blob.as_bytes()).unwrap());
        std::fs::write(
            dir.path().join(path),
            blob.replace("size 1000000", "size 1000001"),
        )
        .unwrap();
        assert!(!matches_file(dir.path(), path, &pointer, blob.as_bytes()).unwrap());
    }
}
