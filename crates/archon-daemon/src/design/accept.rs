//! The `accept` attribute of a file input, checked against the names of the
//! files the operator staged. The browser's own check does not run for files
//! set from outside, so the preview applies it before handing files over.

use archon_core::ArchonError;

/// Extension to MIME type, for the types an operator commonly hands an app.
const MIME_BY_EXTENSION: &[(&str, &str)] = &[
    ("png", "image/png"),
    ("jpg", "image/jpeg"),
    ("jpeg", "image/jpeg"),
    ("gif", "image/gif"),
    ("webp", "image/webp"),
    ("svg", "image/svg+xml"),
    ("bmp", "image/bmp"),
    ("ico", "image/x-icon"),
    ("avif", "image/avif"),
    ("txt", "text/plain"),
    ("csv", "text/csv"),
    ("tsv", "text/tab-separated-values"),
    ("json", "application/json"),
    ("xml", "application/xml"),
    ("html", "text/html"),
    ("md", "text/markdown"),
    ("pdf", "application/pdf"),
    ("zip", "application/zip"),
    ("mp3", "audio/mpeg"),
    ("wav", "audio/wav"),
    ("ogg", "audio/ogg"),
    ("mp4", "video/mp4"),
    ("webm", "video/webm"),
    ("mov", "video/quicktime"),
];

fn extension(name: &str) -> Option<String> {
    let (stem, ext) = name.rsplit_once('.')?;
    (!stem.is_empty() && !ext.is_empty()).then(|| ext.to_ascii_lowercase())
}

fn mime_of(name: &str) -> Option<&'static str> {
    let ext = extension(name)?;
    MIME_BY_EXTENSION
        .iter()
        .find(|(known, _)| *known == ext)
        .map(|(_, mime)| *mime)
}

fn token_matches(token: &str, name: &str) -> bool {
    if let Some(ext) = token.strip_prefix('.') {
        return extension(name).is_some_and(|actual| actual == ext);
    }
    let Some(mime) = mime_of(name) else {
        return false;
    };
    match token.strip_suffix("/*") {
        Some(kind) => mime.split('/').next() == Some(kind),
        None => mime == token,
    }
}

/// Whether a file called `name` is allowed by `accept`; empty accepts anything.
pub(crate) fn accepts(accept: &str, name: &str) -> bool {
    let tokens: Vec<String> = accept
        .split(',')
        .map(|t| t.trim().to_ascii_lowercase())
        .filter(|t| !t.is_empty())
        .collect();
    tokens.is_empty() || tokens.iter().any(|t| token_matches(t, name))
}

/// The refusal for the first of `names` that `accept` does not allow.
pub(crate) fn check(accept: &str, names: &[String]) -> Result<(), ArchonError> {
    assert!(!names.is_empty(), "a transfer has files");
    if names.iter().all(|name| accepts(accept, name)) {
        return Ok(());
    }
    let list = accept
        .split(',')
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .collect::<Vec<_>>()
        .join(", ");
    Err(ArchonError::Invalid(format!(
        "That file type is not one the app accepts here ({list})."
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extensions_match_without_regard_to_case() {
        assert!(accepts(".txt,.csv", "plan.CSV"));
        assert!(accepts(" .txt , .csv ", "a.txt"));
        assert!(!accepts(".txt,.csv", "evil.exe"));
        assert!(!accepts(".txt", "txt"), "a bare name has no extension");
        assert!(!accepts(".txt", ".txt"), "a dotfile has no extension");
    }

    #[test]
    fn mime_types_and_wildcards_match_through_the_extension() {
        assert!(accepts("text/csv", "plan.csv"));
        assert!(!accepts("text/csv", "plan.txt"));
        assert!(accepts("image/*", "logo.PNG"));
        assert!(!accepts("image/*", "song.mp3"));
        assert!(accepts("audio/*,video/*", "clip.webm"));
        assert!(!accepts("image/png", "unknown.xyz"));
    }

    #[test]
    fn empty_accept_takes_anything() {
        assert!(accepts("", "evil.exe"));
        assert!(accepts(" , ", "evil.exe"));
    }

    #[test]
    fn the_refusal_names_what_is_accepted() {
        let names = ["a.csv".to_owned(), "evil.exe".to_owned()];
        let error = check(".txt, .csv", &names).unwrap_err().to_string();
        assert!(
            error.contains("That file type is not one the app accepts here (.txt, .csv)."),
            "{error}"
        );
        assert!(check(".txt, .csv", &names[..1]).is_ok());
    }
}
