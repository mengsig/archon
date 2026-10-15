//! Text for an item's artifact store.

use std::fmt::Write as _;

use archon_core::protocol::{ArtifactFile, ArtifactListing, ArtifactPage};

pub(super) fn listing(listing: &ArtifactListing) -> String {
    let mut text = String::new();
    if !listing.published.is_empty() {
        let _ = writeln!(text, "Published to {}:", listing.item);
        for file in &listing.published {
            let _ = writeln!(text, "  {}", file_line(file));
        }
    }
    if listing.files.is_empty() {
        let _ = write!(
            text,
            "{} has no artifacts ({}).",
            listing.item,
            listing.dir.display()
        );
        return text;
    }
    let _ = writeln!(
        text,
        "Artifacts of {} in {} ({} bytes in all):",
        listing.item,
        listing.dir.display(),
        listing.total_bytes
    );
    for file in &listing.files {
        let _ = writeln!(text, "  {}", file_line(file));
    }
    if listing.omitted > 0 {
        let _ = writeln!(text, "  … and {} more", listing.omitted);
    }
    text.trim_end().to_owned()
}

fn file_line(file: &ArtifactFile) -> String {
    format!("{} ({} bytes)", file.name, file.bytes)
}

pub(super) fn page(page: &ArtifactPage) -> String {
    let end = page.offset + page.text.len() as u64;
    let more = match page.next_offset {
        Some(next) => format!("more: read again with offset {next}"),
        None => "end of file".to_owned(),
    };
    format!(
        "{} of {} ({}), bytes {}..{} of {}; {more}\n{}",
        page.name,
        page.item,
        page.path.display(),
        page.offset,
        end,
        page.total_bytes,
        page.text
    )
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use archon_core::id::QueueItemId;

    use super::*;

    fn file(name: &str, bytes: u64) -> ArtifactFile {
        ArtifactFile {
            name: name.into(),
            bytes,
        }
    }

    #[test]
    fn a_listing_names_the_directory_files_and_what_was_just_published() {
        let item = QueueItemId::new();
        let text = listing(&ArtifactListing {
            item: item.clone(),
            dir: PathBuf::from("/data/artifacts/x"),
            files: vec![file("bench/out.json", 12)],
            omitted: 3,
            total_bytes: 12,
            published: vec![file("bench/out.json", 12)],
        });
        assert!(text.contains("Published to"), "{text}");
        assert!(text.contains("/data/artifacts/x"), "{text}");
        assert!(text.contains("bench/out.json (12 bytes)"), "{text}");
        assert!(text.contains("and 3 more"), "{text}");
    }

    #[test]
    fn a_page_says_where_the_next_one_starts() {
        let page = page(&ArtifactPage {
            item: QueueItemId::new(),
            name: "log.txt".into(),
            path: PathBuf::from("/data/artifacts/x/log.txt"),
            offset: 10,
            total_bytes: 100,
            text: "abc".into(),
            next_offset: Some(13),
        });
        assert!(page.contains("bytes 10..13 of 100"), "{page}");
        assert!(page.contains("offset 13"), "{page}");
    }
}
