//! The `accept` attribute of a file input, checked against the files the
//! operator staged. The browser's own check does not run for files set from
//! outside, so the preview applies it before handing files over.

use archon_core::ArchonError;

/// Extension to MIME type, for a file whose type the operator's browser did
/// not give.
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
    ("tif", "image/tiff"),
    ("tiff", "image/tiff"),
    ("heic", "image/heic"),
    ("txt", "text/plain"),
    ("csv", "text/csv"),
    ("tsv", "text/tab-separated-values"),
    ("json", "application/json"),
    ("xml", "application/xml"),
    ("html", "text/html"),
    ("md", "text/markdown"),
    ("pdf", "application/pdf"),
    ("rtf", "application/rtf"),
    ("doc", "application/msword"),
    ("xls", "application/vnd.ms-excel"),
    ("ppt", "application/vnd.ms-powerpoint"),
    (
        "docx",
        "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
    ),
    (
        "xlsx",
        "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
    ),
    (
        "pptx",
        "application/vnd.openxmlformats-officedocument.presentationml.presentation",
    ),
    ("odt", "application/vnd.oasis.opendocument.text"),
    ("ods", "application/vnd.oasis.opendocument.spreadsheet"),
    ("zip", "application/zip"),
    ("gz", "application/gzip"),
    ("tar", "application/x-tar"),
    ("mp3", "audio/mpeg"),
    ("wav", "audio/wav"),
    ("ogg", "audio/ogg"),
    ("mp4", "video/mp4"),
    ("webm", "video/webm"),
    ("mov", "video/quicktime"),
];

/// Longest media type kept from the client.
const MAX_MEDIA_TYPE: usize = 255;

/// A staged file as `accept` sees it.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Picked<'a> {
    pub name: &'a str,
    /// The type the operator's browser gave, normalised; empty when none.
    pub media_type: &'a str,
}

/// The client's media type in lower case without parameters; empty when it
/// is not a `type/subtype`.
pub(crate) fn media_type(raw: &str) -> String {
    let bare = raw.split(';').next().unwrap_or_default().trim();
    let valid = bare.len() <= MAX_MEDIA_TYPE
        && bare.split_once('/').is_some_and(|(kind, sub)| {
            let token =
                |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_graphic() && b != b'/');
            token(kind) && token(sub)
        });
    if valid {
        bare.to_ascii_lowercase()
    } else {
        String::new()
    }
}

fn mime_of(name: &str) -> Option<&'static str> {
    let (stem, ext) = name.rsplit_once('.')?;
    if stem.is_empty() {
        return None;
    }
    let ext = ext.to_ascii_lowercase();
    MIME_BY_EXTENSION
        .iter()
        .find(|(known, _)| *known == ext)
        .map(|(_, mime)| *mime)
}

fn mime_matches(token: &str, mime: &str) -> bool {
    match token.strip_suffix("/*") {
        Some(kind) => mime.split('/').next() == Some(kind),
        None => mime == token,
    }
}

fn token_matches(token: &str, file: Picked<'_>) -> bool {
    if token.starts_with('.') {
        let name = file.name.to_ascii_lowercase();
        return name.len() > token.len() && name.ends_with(token);
    }
    let declared = (!file.media_type.is_empty()).then_some(file.media_type);
    [declared, mime_of(file.name)]
        .into_iter()
        .flatten()
        .any(|mime| mime_matches(token, mime))
}

fn tokens(accept: &str) -> Vec<String> {
    accept
        .split(',')
        .map(|t| t.trim().to_ascii_lowercase())
        .filter(|t| !t.is_empty())
        .collect()
}

/// Whether `file` is allowed by `accept`; empty accepts anything.
pub(crate) fn accepts(accept: &str, file: Picked<'_>) -> bool {
    let tokens = tokens(accept);
    tokens.is_empty() || tokens.iter().any(|t| token_matches(t, file))
}

/// The refusal when any of `files` is one `accept` does not allow.
pub(crate) fn check(accept: &str, files: &[Picked<'_>]) -> Result<(), ArchonError> {
    assert!(!files.is_empty(), "a transfer has files");
    if files.iter().all(|file| accepts(accept, *file)) {
        return Ok(());
    }
    let list = tokens(accept).join(", ");
    assert!(!list.is_empty(), "an empty accept refuses nothing");
    Err(ArchonError::Invalid(format!(
        "That file type is not one the app accepts here ({list})."
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn named(name: &str) -> Picked<'_> {
        Picked {
            name,
            media_type: "",
        }
    }

    #[test]
    fn extensions_match_without_regard_to_case() {
        assert!(accepts(".txt,.csv", named("plan.CSV")));
        assert!(accepts(" .txt , .csv ", named("a.txt")));
        assert!(!accepts(".txt,.csv", named("evil.exe")));
        assert!(
            !accepts(".txt", named("txt")),
            "a bare name has no extension"
        );
        assert!(
            !accepts(".txt", named(".txt")),
            "a dotfile has no extension"
        );
    }

    #[test]
    fn a_compound_extension_matches_the_whole_suffix() {
        assert!(accepts(".tar.gz", named("stowage.TAR.GZ")));
        assert!(!accepts(".tar.gz", named("stowage.gz")));
    }

    #[test]
    fn mime_types_and_wildcards_match_through_the_extension() {
        assert!(accepts("text/csv", named("plan.csv")));
        assert!(!accepts("text/csv", named("plan.txt")));
        assert!(accepts("image/*", named("logo.PNG")));
        assert!(accepts("image/*", named("scan.tif")));
        assert!(!accepts("image/*", named("song.mp3")));
        assert!(accepts("audio/*,video/*", named("clip.webm")));
        assert!(!accepts("image/png", named("unknown.xyz")));
    }

    #[test]
    fn mime_types_match_the_type_the_operators_browser_gave() {
        let docx = "application/vnd.openxmlformats-officedocument.wordprocessingml.document";
        let report = Picked {
            name: "report.docx",
            media_type: docx,
        };
        assert!(accepts(docx, report));
        let raw = Picked {
            name: "scan.raw",
            media_type: "image/x-raw",
        };
        assert!(accepts("image/*", raw));
        assert!(!accepts("text/*", raw));
    }

    #[test]
    fn a_client_media_type_is_kept_only_when_well_formed() {
        assert_eq!(media_type("Text/CSV; charset=utf-8"), "text/csv");
        assert_eq!(media_type(""), "");
        assert_eq!(media_type("text"), "");
        assert_eq!(media_type("text/csv/x"), "");
        assert_eq!(media_type(&format!("a/{}", "b".repeat(300))), "");
    }

    #[test]
    fn empty_accept_takes_anything() {
        assert!(accepts("", named("evil.exe")));
        assert!(accepts(" , ", named("evil.exe")));
    }

    #[test]
    fn the_refusal_names_what_is_accepted() {
        let files = [named("a.csv"), named("evil.exe")];
        let error = check(".txt, .CSV", &files).unwrap_err().to_string();
        assert!(
            error.contains("That file type is not one the app accepts here (.txt, .csv)."),
            "{error}"
        );
        assert!(check(".txt, .csv", &files[..1]).is_ok());
    }
}
