//! Files between the operator's device and the preview: uploads staged in the
//! session's private directory for a file chooser or a drop, and downloads the
//! page finished, offered back in parts. Every name and size from the client
//! is checked here; no client path is ever used on the host.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use archon_core::ArchonError;
use archon_design_browser as adb;
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};

/// Largest file the operator may hand the page.
pub(crate) const MAX_FILE_BYTES: u64 = 64 * 1024 * 1024;
/// Most bytes one session's uploads may hold on disk together.
pub(crate) const MAX_SESSION_BYTES: u64 = 256 * 1024 * 1024;
/// Most files in one transfer.
pub(crate) const MAX_FILES: u32 = 32;
/// Largest decoded part of an upload or a download.
pub(crate) const MAX_PART_BYTES: usize = 512 * 1024;
/// Most transfers one session stages.
const MAX_TRANSFERS: usize = 64;
/// Downloads offered at once; older ones are forgotten.
const MAX_DOWNLOADS: usize = 32;

const MIB: u64 = 1024 * 1024;

#[derive(Debug)]
struct Staged {
    name: String,
    path: PathBuf,
    size: u64,
    written: u64,
}

/// The file chooser the page has open.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Chooser {
    pub id: u64,
    pub node: i64,
    pub multiple: bool,
}

#[derive(Debug, Clone)]
struct Download {
    id: u64,
    name: String,
    path: PathBuf,
    size: u64,
}

/// One part of an upload, as the client sent it.
#[derive(Debug)]
pub(crate) struct Part<'a> {
    pub transfer: &'a str,
    pub file: u32,
    pub name: &'a str,
    pub size: u64,
    pub offset: u64,
    pub data: &'a [u8],
}

/// A session's uploads, open chooser and downloads.
#[derive(Debug, Default)]
pub(crate) struct Transfers {
    staged: HashMap<String, Vec<Staged>>,
    /// Bytes reserved on disk: each file's whole size from its first part.
    total: u64,
    chooser: Option<Chooser>,
    next_chooser: u64,
    downloads: Vec<Download>,
    next_download: u64,
}

fn invalid(message: impl Into<String>) -> ArchonError {
    ArchonError::Invalid(message.into())
}

/// A transfer id is the client's: letters, digits, `-` and `_`, at most 40.
pub(crate) fn valid_transfer(id: &str) -> bool {
    (1..=40).contains(&id.len())
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// The decoded part, refused before decoding when it cannot fit.
pub(crate) fn decode_part(data: &str) -> Result<Vec<u8>, ArchonError> {
    use base64::Engine;
    if data.len() > MAX_PART_BYTES.div_ceil(3) * 4 {
        return Err(invalid(format!(
            "a file part is larger than {} KiB",
            MAX_PART_BYTES / 1024
        )));
    }
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(data)
        .map_err(|error| invalid(format!("a file part is not base64: {error}")))?;
    assert!(
        bytes.len() <= MAX_PART_BYTES,
        "the encoded cap bounds the decoded part"
    );
    Ok(bytes)
}

fn check_part(part: &Part<'_>) -> Result<(), ArchonError> {
    if !valid_transfer(part.transfer) {
        return Err(invalid(
            "a transfer id is 1 to 40 letters, digits, '-' or '_'",
        ));
    }
    if part.file >= MAX_FILES {
        return Err(invalid(format!(
            "the preview takes at most {MAX_FILES} files at once"
        )));
    }
    if part.size > MAX_FILE_BYTES {
        return Err(invalid(format!(
            "{} is larger than {} MiB, the most the preview takes",
            adb::file_name(part.name),
            MAX_FILE_BYTES / MIB
        )));
    }
    let len = part.data.len() as u64;
    if part.data.len() > MAX_PART_BYTES
        || part
            .offset
            .checked_add(len)
            .is_none_or(|end| end > part.size)
    {
        return Err(invalid("a file part does not fit its file"));
    }
    Ok(())
}

async fn create_private_dir(dir: &Path) -> Result<(), ArchonError> {
    let mut builder = tokio::fs::DirBuilder::new();
    builder.recursive(true).mode(0o700);
    builder
        .create(dir)
        .await
        .map_err(|error| ArchonError::io(format!("creating {}", dir.display()), error))
}

impl Transfers {
    /// The page opened a file chooser; it replaces any earlier one.
    pub(crate) fn open_chooser(&mut self, node: i64, multiple: bool) -> u64 {
        self.next_chooser += 1;
        self.chooser = Some(Chooser {
            id: self.next_chooser,
            node,
            multiple,
        });
        assert!(self.next_chooser > 0, "chooser ids start at 1");
        self.next_chooser
    }

    /// Close chooser `id`; refused when another one is open now.
    pub(crate) fn take_chooser(&mut self, id: u64) -> Result<Chooser, ArchonError> {
        match self.chooser {
            Some(chooser) if chooser.id == id => {
                self.chooser = None;
                Ok(chooser)
            }
            _ => Err(invalid("that file chooser is no longer open in the app")),
        }
    }

    /// Write one part under `root`, the session's private uploads directory.
    pub(crate) async fn write_part(
        &mut self,
        root: &Path,
        part: Part<'_>,
    ) -> Result<(), ArchonError> {
        check_part(&part)?;
        if !self.staged.contains_key(part.transfer) && self.staged.len() >= MAX_TRANSFERS {
            return Err(invalid(
                "this session already holds as many uploads as it can",
            ));
        }
        let files = self.staged.entry(part.transfer.to_owned()).or_default();
        let index = part.file as usize;
        if index > files.len() {
            return Err(invalid("a transfer's files come in order"));
        }
        if index == files.len() {
            if part.offset != 0 {
                return Err(invalid("a file starts at offset 0"));
            }
            if self.total + part.size > MAX_SESSION_BYTES {
                return Err(invalid(format!(
                    "this preview session already holds {} MiB of files; it takes {} MiB at most",
                    self.total / MIB,
                    MAX_SESSION_BYTES / MIB
                )));
            }
            let name = adb::file_name(part.name);
            let dir = root.join(part.transfer).join(part.file.to_string());
            create_private_dir(&dir).await?;
            let path = dir.join(&name);
            assert!(
                path.starts_with(root) && path.parent() == Some(dir.as_path()),
                "an upload stays in its directory"
            );
            self.total += part.size;
            files.push(Staged {
                name,
                path,
                size: part.size,
                written: 0,
            });
        }
        let staged = &mut files[index];
        if staged.size != part.size
            || staged.name != adb::file_name(part.name)
            || staged.written != part.offset
        {
            return Err(invalid("a file part does not continue its file"));
        }
        let mut options = tokio::fs::OpenOptions::new();
        options.append(true).mode(0o600);
        if part.offset == 0 {
            options.create_new(true);
        }
        let mut file = options
            .open(&staged.path)
            .await
            .map_err(|error| ArchonError::io("opening an upload", error))?;
        file.write_all(part.data)
            .await
            .map_err(|error| ArchonError::io("writing an upload", error))?;
        staged.written += part.data.len() as u64;
        assert!(
            staged.written <= staged.size,
            "an upload never outgrows its declared size"
        );
        Ok(())
    }

    /// The complete files of `transfer`, in order.
    pub(crate) fn files(&self, transfer: &str) -> Result<Vec<PathBuf>, ArchonError> {
        let files = self
            .staged
            .get(transfer)
            .filter(|files| !files.is_empty())
            .ok_or_else(|| invalid("no files were sent for that transfer"))?;
        if let Some(partial) = files.iter().find(|f| f.written != f.size) {
            return Err(invalid(format!("{} was not sent in full", partial.name)));
        }
        Ok(files.iter().map(|f| f.path.clone()).collect())
    }

    /// Offer a finished download; the oldest is forgotten past the cap.
    pub(crate) fn add_download(&mut self, name: String, path: PathBuf, size: u64) -> u64 {
        assert!(
            size <= adb::MAX_DOWNLOAD_BYTES,
            "the browser caps downloads"
        );
        self.next_download += 1;
        if self.downloads.len() >= MAX_DOWNLOADS {
            self.downloads.remove(0);
        }
        let id = self.next_download;
        self.downloads.push(Download {
            id,
            name,
            path,
            size,
        });
        id
    }

    /// Up to [`MAX_PART_BYTES`] of download `id` from `offset`: its name, size and bytes.
    pub(crate) async fn read_download(
        &self,
        id: u64,
        offset: u64,
    ) -> Result<(String, u64, Vec<u8>), ArchonError> {
        let download = self
            .downloads
            .iter()
            .find(|d| d.id == id)
            .cloned()
            .ok_or_else(|| ArchonError::NotFound("that download".into()))?;
        if offset > download.size {
            return Err(invalid("the offset is past the end of the download"));
        }
        let len = (download.size - offset).min(MAX_PART_BYTES as u64) as usize;
        let mut file = tokio::fs::File::open(&download.path)
            .await
            .map_err(|error| ArchonError::io("opening a download", error))?;
        file.seek(std::io::SeekFrom::Start(offset))
            .await
            .map_err(|error| ArchonError::io("reading a download", error))?;
        let mut bytes = vec![0; len];
        file.read_exact(&mut bytes)
            .await
            .map_err(|error| ArchonError::io("reading a download", error))?;
        Ok((download.name, download.size, bytes))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn part<'a>(
        transfer: &'a str,
        file: u32,
        name: &'a str,
        size: u64,
        offset: u64,
        data: &'a [u8],
    ) -> Part<'a> {
        Part {
            transfer,
            file,
            name,
            size,
            offset,
            data,
        }
    }

    #[tokio::test]
    async fn parts_make_a_sanitised_file_in_the_private_directory() {
        let root = tempfile::tempdir().unwrap();
        let mut transfers = Transfers::default();
        let name = "../../home/me/BAPLIE.edi";
        transfers
            .write_part(root.path(), part("t1", 0, name, 6, 0, b"UNB"))
            .await
            .unwrap();
        assert!(
            transfers.files("t1").is_err(),
            "a partial file is not offered"
        );
        transfers
            .write_part(root.path(), part("t1", 0, name, 6, 3, b"+UN"))
            .await
            .unwrap();
        let files = transfers.files("t1").unwrap();
        assert_eq!(files, [root.path().join("t1/0/BAPLIE.edi")]);
        assert_eq!(std::fs::read(&files[0]).unwrap(), b"UNB+UN");
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&files[0]).unwrap().permissions().mode();
        assert_eq!(mode & 0o077, 0, "only archond's user reads an upload");
    }

    #[tokio::test]
    async fn parts_out_of_order_or_off_their_file_are_refused() {
        let root = tempfile::tempdir().unwrap();
        let mut transfers = Transfers::default();
        let wrote = |r: Result<(), ArchonError>| r.is_ok();
        assert!(!wrote(
            transfers
                .write_part(root.path(), part("t", 1, "a", 1, 0, b"x"))
                .await
        ));
        assert!(!wrote(
            transfers
                .write_part(root.path(), part("t", 0, "a", 4, 2, b"x"))
                .await
        ));
        assert!(wrote(
            transfers
                .write_part(root.path(), part("t", 0, "a", 4, 0, b"xy"))
                .await
        ));
        assert!(
            !wrote(
                transfers
                    .write_part(root.path(), part("t", 0, "a", 4, 0, b"xy"))
                    .await
            ),
            "a part is not written twice"
        );
        assert!(
            !wrote(
                transfers
                    .write_part(root.path(), part("t", 0, "b", 4, 2, b"zw"))
                    .await
            ),
            "the name stays"
        );
        assert!(
            !wrote(
                transfers
                    .write_part(root.path(), part("t", 0, "a", 4, 2, b"zwv"))
                    .await
            ),
            "the size holds"
        );
        assert!(!wrote(
            transfers
                .write_part(root.path(), part("../t", 0, "a", 1, 0, b"x"))
                .await
        ));
        assert!(!wrote(
            transfers
                .write_part(root.path(), part("t", MAX_FILES, "a", 1, 0, b"x"))
                .await
        ));
    }

    #[tokio::test]
    async fn file_and_session_limits_hold() {
        let root = tempfile::tempdir().unwrap();
        let mut transfers = Transfers::default();
        let too_big = transfers
            .write_part(
                root.path(),
                part("t", 0, "big.edi", MAX_FILE_BYTES + 1, 0, b""),
            )
            .await;
        let message = too_big.unwrap_err().to_string();
        assert!(
            message.contains("big.edi is larger than 64 MiB"),
            "{message}"
        );
        for file in 0..4 {
            transfers
                .write_part(root.path(), part("t", file, "f", MAX_FILE_BYTES, 0, b""))
                .await
                .unwrap();
        }
        let over = transfers
            .write_part(root.path(), part("t", 4, "f", 1, 0, b""))
            .await;
        assert!(over.unwrap_err().to_string().contains("256 MiB at most"));
        assert!(decode_part(&"A".repeat(MAX_PART_BYTES / 3 * 4 + 8)).is_err());
        assert_eq!(decode_part("eHk=").unwrap(), b"xy");
    }

    #[test]
    fn only_the_open_chooser_is_answered() {
        let mut transfers = Transfers::default();
        let first = transfers.open_chooser(7, false);
        let second = transfers.open_chooser(8, true);
        assert!(transfers.take_chooser(first).is_err());
        assert_eq!(transfers.take_chooser(second).unwrap().node, 8);
        assert!(
            transfers.take_chooser(second).is_err(),
            "a chooser is answered once"
        );
    }

    #[tokio::test]
    async fn downloads_are_read_in_parts_and_the_oldest_forgotten() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("guid");
        let body = vec![7u8; MAX_PART_BYTES + 10];
        std::fs::write(&path, &body).unwrap();
        let mut transfers = Transfers::default();
        let id = transfers.add_download("plan.csv".into(), path.clone(), body.len() as u64);
        let (name, size, first) = transfers.read_download(id, 0).await.unwrap();
        assert_eq!(
            (name.as_str(), size, first.len()),
            ("plan.csv", body.len() as u64, MAX_PART_BYTES)
        );
        let (_, _, rest) = transfers
            .read_download(id, MAX_PART_BYTES as u64)
            .await
            .unwrap();
        assert_eq!(rest.len(), 10);
        assert!(transfers.read_download(id, size + 1).await.is_err());
        for _ in 0..MAX_DOWNLOADS {
            transfers.add_download("x".into(), path.clone(), 1);
        }
        assert!(
            transfers.read_download(id, 0).await.is_err(),
            "the oldest is forgotten"
        );
    }
}
