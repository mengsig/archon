//! Images on this machine as message attachments: a file named by
//! `/image`, `archon say --image` or a paste. Each is checked as archond
//! checks it, so a file it would refuse never crosses the socket.

use std::path::{Path, PathBuf};

use archon_core::attachment::{AttachmentError, ImageAttachment, MAX_IMAGE_BYTES};

/// File extensions a pasted path must end in to be read as an image.
const IMAGE_EXTENSIONS: [&str; 5] = ["png", "jpg", "jpeg", "gif", "webp"];

/// Read an image file into an attachment. A file over the size limit is
/// refused on its length, before it is read. The error is one line.
pub fn read_image_file(path: &Path) -> Result<ImageAttachment, String> {
    let shown = path.display();
    let metadata = std::fs::metadata(path).map_err(|e| format!("could not read {shown}: {e}"))?;
    if !metadata.is_file() {
        return Err(format!("{shown} is not a file"));
    }
    let name = path.file_name().map(|name| name.to_string_lossy());
    if metadata.len() > MAX_IMAGE_BYTES as u64 {
        let refused = AttachmentError::TooLarge {
            label: name.as_deref().unwrap_or("file").to_owned(),
            size: metadata.len(),
        };
        return Err(refused.to_string());
    }
    let bytes = std::fs::read(path).map_err(|e| format!("could not read {shown}: {e}"))?;
    let image = ImageAttachment::encode(&bytes, name.as_deref()).map_err(|e| e.to_string())?;
    debug_assert_eq!(image.size_bytes(), bytes.len() as u64);
    Ok(image)
}

/// The path `/image` or `--image` names: `~/` is the home directory.
pub fn expand_home(path: &str, home: Option<&Path>) -> Option<PathBuf> {
    let path = path.trim();
    if path.is_empty() {
        return None;
    }
    match path.strip_prefix("~/") {
        Some(rest) => home.map(|home| home.join(rest)),
        None if path == "~" => home.map(Path::to_path_buf),
        None => Some(PathBuf::from(path)),
    }
}

/// The image file a paste names, when the paste is exactly one path to one.
/// A terminal may quote, escape or `file://` a dropped file; each is tried.
pub fn pasted_image_path(text: &str, home: Option<&Path>) -> Option<PathBuf> {
    let text = text.trim();
    if text.is_empty() || text.contains('\n') {
        return None;
    }
    path_spellings(text)
        .into_iter()
        .filter_map(|spelling| expand_home(&spelling, home))
        .find(|path| has_image_extension(path) && path.is_file())
}

fn has_image_extension(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            IMAGE_EXTENSIONS
                .iter()
                .any(|known| extension.eq_ignore_ascii_case(known))
        })
}

/// `text` as typed, then with quotes, backslash escapes or a `file://` URL undone.
fn path_spellings(text: &str) -> Vec<String> {
    let mut spellings = vec![text.to_owned()];
    for quote in ['\'', '"'] {
        let inner = text
            .strip_prefix(quote)
            .and_then(|rest| rest.strip_suffix(quote));
        spellings.extend(inner.map(str::to_owned));
    }
    if text.contains('\\') {
        spellings.push(unescaped(text));
    }
    spellings.extend(text.strip_prefix("file://").and_then(percent_decoded));
    spellings
}

/// Shell escaping undone: each backslash yields the character after it.
fn unescaped(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => out.extend(chars.next()),
            c => out.push(c),
        }
    }
    out
}

/// `%20` and its kin decoded; `None` for a broken escape or non-UTF-8 bytes.
fn percent_decoded(text: &str) -> Option<String> {
    let mut bytes = Vec::with_capacity(text.len());
    let mut rest = text.as_bytes();
    while let Some((&first, tail)) = rest.split_first() {
        if first != b'%' {
            bytes.push(first);
            rest = tail;
            continue;
        }
        let hex = std::str::from_utf8(tail.get(..2)?).ok()?;
        bytes.push(u8::from_str_radix(hex, 16).ok()?);
        rest = &tail[2..];
    }
    String::from_utf8(bytes).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    const PNG: &[u8] = b"\x89PNG\r\n\x1a\npixels";

    #[test]
    fn a_paste_is_an_image_path_however_the_terminal_spelled_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("Screenshot from today.PNG");
        std::fs::write(&path, PNG).unwrap();
        let plain = path.to_str().unwrap().to_owned();
        let spellings = [
            plain.clone(),
            format!("  {plain}\n"),
            format!("'{plain}'"),
            format!("\"{plain}\""),
            plain.replace(' ', "\\ "),
            format!("file://{}", plain.replace(' ', "%20")),
        ];
        for spelling in spellings {
            assert_eq!(
                pasted_image_path(&spelling, None).as_deref(),
                Some(path.as_path()),
                "{spelling:?}"
            );
        }
    }

    #[test]
    fn a_home_relative_path_is_read_from_home() {
        let home = tempfile::tempdir().unwrap();
        std::fs::write(home.path().join("shot.png"), PNG).unwrap();
        let found = pasted_image_path("~/shot.png", Some(home.path()));
        assert_eq!(found, Some(home.path().join("shot.png")));
        assert_eq!(pasted_image_path("~/shot.png", None), None);
        assert_eq!(expand_home("  ", Some(home.path())), None);
    }

    #[test]
    fn text_that_is_not_one_existing_image_path_stays_text() {
        let dir = tempfile::tempdir().unwrap();
        let image = dir.path().join("shot.png");
        let notes = dir.path().join("notes.txt");
        std::fs::write(&image, PNG).unwrap();
        std::fs::write(&notes, "words").unwrap();
        let image = image.to_str().unwrap();
        let stays_text = [
            String::new(),
            "look at this".to_owned(),
            notes.to_str().unwrap().to_owned(),
            dir.path().join("gone.png").to_str().unwrap().to_owned(),
            format!("{image}\n{image}"),
            format!("see {image}"),
            "file://%zz/shot.png".to_owned(),
        ];
        for text in stays_text {
            assert_eq!(pasted_image_path(&text, None), None, "{text:?}");
        }
    }

    #[test]
    fn a_file_is_read_by_name_and_a_wrong_one_says_why() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("shot.png");
        std::fs::write(&path, PNG).unwrap();
        let image = read_image_file(&path).unwrap();
        assert_eq!(image.name.as_deref(), Some("shot.png"));
        assert_eq!(image.size_bytes(), PNG.len() as u64);
        let missing = read_image_file(&dir.path().join("gone.png")).unwrap_err();
        assert!(missing.contains("gone.png"), "{missing}");
        assert!(
            read_image_file(dir.path())
                .unwrap_err()
                .contains("not a file")
        );
        let big = dir.path().join("big.png");
        let file = std::fs::File::create(&big).unwrap();
        file.set_len(MAX_IMAGE_BYTES as u64 + 1).unwrap();
        let refused = read_image_file(&big).unwrap_err();
        assert!(refused.contains("big.png is too large"), "{refused}");
        let notes = dir.path().join("notes.png");
        std::fs::write(&notes, "plain text").unwrap();
        let refused = read_image_file(&notes).unwrap_err();
        assert!(refused.contains("not a supported type"), "{refused}");
    }
}
