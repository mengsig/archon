//! Preserve local image references in a proposal page.
use archon_core::error::ArchonError;
use archon_core::intent::{ApprovedIntent, ReferenceImage};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

pub(super) fn extract(
    html: &str,
    page: &Path,
    workspace: &Path,
    destination: &Path,
    record: &mut ApprovedIntent,
) -> Result<String, ArchonError> {
    let mut urls = image_urls(html)?;
    urls.extend(css_image_urls(html)?);
    let mut replacements = Vec::new();
    for url in urls.into_iter().filter(|url| !url.starts_with("data:")) {
        let range = html
            .substr_range(url)
            .expect("parsed URLs borrow the source HTML");
        let source = local_path(url, page, workspace)?;
        let name = preserve_local_image(&source, destination, record)?;
        let suffix = url.find(['?', '#']).map_or("", |start| &url[start..]);
        replacements.push((range, format!("./{name}{suffix}")));
    }
    replacements.sort_unstable_by_key(|(range, _)| (range.start, std::cmp::Reverse(range.end)));
    let mut rewritten = String::new();
    let mut position = 0;
    for (range, replacement) in replacements {
        if range.end <= position {
            continue;
        }
        if range.start < position {
            return Err(ArchonError::Invalid(
                "overlapping proposal image URLs".into(),
            ));
        }
        assert!(range.start < range.end, "parsed image URLs are nonempty");
        assert!(
            range.end <= html.len(),
            "a replacement lies within the HTML"
        );
        rewritten.push_str(&html[position..range.start]);
        rewritten.push_str(&replacement);
        position = range.end;
    }
    rewritten.push_str(&html[position..]);
    Ok(rewritten)
}

fn preserve_local_image(
    source: &Path,
    destination: &Path,
    record: &mut ApprovedIntent,
) -> Result<String, ArchonError> {
    let metadata = std::fs::metadata(source)
        .map_err(|e| ArchonError::io(format!("inspect image {}", source.display()), e))?;
    if !metadata.is_file() || metadata.len() == 0 || metadata.len() > 32 * 1024 * 1024 {
        return Err(ArchonError::Invalid(format!(
            "image {} exceeds the size limit or is empty",
            source.display()
        )));
    }
    let bytes = std::fs::read(source)
        .map_err(|e| ArchonError::io(format!("read image {}", source.display()), e))?;
    if bytes.is_empty() || bytes.len() > 32 * 1024 * 1024 {
        return Err(ArchonError::Invalid(format!(
            "image {} exceeds the size limit or is empty",
            source.display()
        )));
    }
    let hash = Sha256::digest(&bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    if let Some(image) = record.images.iter().find(|image| image.sha256 == hash) {
        return Ok(crate::handlers::intent::preserved_filename(
            &image.path,
            &hash,
        ));
    }
    let path = crate::handlers::intent::preserve(destination, source, &hash)?;
    let name = crate::handlers::intent::preserved_filename(source, &hash);
    assert_eq!(
        path.file_name().and_then(|name| name.to_str()),
        Some(name.as_str())
    );
    assert!(path.starts_with(destination));
    let id = next_id(record);
    record.images.push(ReferenceImage {
        id,
        path,
        sha256: hash,
        viewport: None,
        theme: None,
        state: None,
    });
    Ok(name)
}

fn css_image_urls(html: &str) -> Result<Vec<&str>, ArchonError> {
    let mut urls = Vec::new();
    let mut rest = html;
    while let Some(start) = rest.to_ascii_lowercase().find("url(") {
        rest = &rest[start + 4..];
        let end = rest
            .find(')')
            .ok_or_else(|| ArchonError::Invalid("unterminated CSS URL".into()))?;
        let value = rest[..end].trim().trim_matches(['\'', '"']);
        let path = value
            .split(['?', '#'])
            .next()
            .unwrap_or_default()
            .to_ascii_lowercase();
        if [".png", ".jpg", ".jpeg", ".gif", ".webp", ".svg"]
            .iter()
            .any(|ext| path.ends_with(ext))
        {
            urls.push(value);
        }
        rest = &rest[end + 1..];
    }
    Ok(urls)
}

fn next_id(record: &ApprovedIntent) -> String {
    let mut index = 1;
    loop {
        let id = format!("page_image{index}");
        if !record.images.iter().any(|image| image.id == id) {
            return id;
        }
        index += 1;
    }
}

fn local_path(url: &str, page: &Path, workspace: &Path) -> Result<PathBuf, ArchonError> {
    if url.is_empty() || url.contains(':') || url.starts_with("//") {
        return Err(ArchonError::Invalid(format!(
            "proposal image URL {url:?} must name a local file"
        )));
    }
    let raw = url.split(['?', '#']).next().unwrap_or_default();
    let decoded = decode_path(&raw.replace("&amp;", "&"))?;
    let parent = page
        .parent()
        .ok_or_else(|| ArchonError::Invalid("proposal page needs a directory".into()))?;
    let source = parent
        .join(decoded)
        .canonicalize()
        .map_err(|e| ArchonError::io(format!("resolve image {url:?}"), e))?;
    let root = workspace
        .canonicalize()
        .map_err(|e| ArchonError::io("resolve proposal workspace", e))?;
    if !source.starts_with(&root) || !source.is_file() {
        return Err(ArchonError::Invalid(format!(
            "proposal image {url:?} is outside the workspace or not a file"
        )));
    }
    Ok(source)
}

fn decode_path(raw: &str) -> Result<String, ArchonError> {
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let pair = bytes
                .get(i + 1..i + 3)
                .ok_or_else(|| ArchonError::Invalid("incomplete image URL escape".into()))?;
            let hex = std::str::from_utf8(pair)
                .map_err(|_| ArchonError::Invalid("invalid image URL escape".into()))?;
            out.push(
                u8::from_str_radix(hex, 16)
                    .map_err(|_| ArchonError::Invalid("invalid image URL escape".into()))?,
            );
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).map_err(|_| ArchonError::Invalid("image URL must be UTF-8".into()))
}

fn image_urls(html: &str) -> Result<Vec<&str>, ArchonError> {
    let mut urls = Vec::new();
    let mut rest = html;
    while let Some(start) = rest.find('<') {
        rest = &rest[start + 1..];
        if rest.starts_with("!--") {
            let end = rest
                .find("-->")
                .ok_or_else(|| ArchonError::Invalid("unterminated HTML comment".into()))?;
            rest = &rest[end + 3..];
            continue;
        }
        let end =
            tag_end(rest).ok_or_else(|| ArchonError::Invalid("unterminated HTML tag".into()))?;
        let tag = &rest[..end];
        rest = &rest[end + 1..];
        let name_end = tag
            .find(|c: char| c.is_ascii_whitespace() || c == '/')
            .unwrap_or(tag.len());
        let name = &tag[..name_end];
        if name.eq_ignore_ascii_case("script") || name.eq_ignore_ascii_case("style") {
            let closing = format!("</{}", name.to_ascii_lowercase());
            let end = rest
                .to_ascii_lowercase()
                .find(&closing)
                .ok_or_else(|| ArchonError::Invalid(format!("unterminated {name} element")))?;
            rest = &rest[end..];
            continue;
        }
        if !name.eq_ignore_ascii_case("img") && !name.eq_ignore_ascii_case("source") {
            continue;
        }
        for (name, value) in attributes(&tag[name_end..])? {
            if name.eq_ignore_ascii_case("src") && !value.is_empty() {
                urls.push(value);
            } else if name.eq_ignore_ascii_case("srcset") {
                urls.extend(srcset(value)?);
            }
        }
    }
    Ok(urls)
}

fn tag_end(text: &str) -> Option<usize> {
    let mut quote = None;
    for (index, c) in text.char_indices() {
        match (quote, c) {
            (Some(q), c) if q == c => quote = None,
            (None, '\'' | '"') => quote = Some(c),
            (None, '>') => return Some(index),
            _ => {}
        }
    }
    None
}

fn attributes(mut text: &str) -> Result<Vec<(&str, &str)>, ArchonError> {
    let mut result = Vec::new();
    while !text
        .trim_start_matches(|c: char| c.is_ascii_whitespace() || c == '/')
        .is_empty()
    {
        text = text.trim_start_matches(|c: char| c.is_ascii_whitespace() || c == '/');
        let n = text
            .find(|c: char| c.is_ascii_whitespace() || c == '=' || c == '/')
            .unwrap_or(text.len());
        if n == 0 {
            return Err(ArchonError::Invalid("malformed image tag attribute".into()));
        }
        let name = &text[..n];
        text = text[n..].trim_start();
        if !text.starts_with('=') {
            continue;
        }
        text = text[1..].trim_start();
        let (value, remaining) = attribute_value(text)?;
        result.push((name, value));
        text = remaining;
    }
    Ok(result)
}

fn attribute_value(text: &str) -> Result<(&str, &str), ArchonError> {
    let first = text
        .chars()
        .next()
        .ok_or_else(|| ArchonError::Invalid("missing image tag value".into()))?;
    if first == '\'' || first == '"' {
        let end = text[1..]
            .find(first)
            .ok_or_else(|| ArchonError::Invalid("unterminated image tag value".into()))?
            + 1;
        return Ok((&text[1..end], &text[end + 1..]));
    }
    let end = text
        .find(|c: char| c.is_ascii_whitespace())
        .unwrap_or(text.len());
    Ok((text[..end].trim_end_matches('/'), &text[end..]))
}

fn srcset(mut value: &str) -> Result<Vec<&str>, ArchonError> {
    let mut urls = Vec::new();
    while !value
        .trim_start_matches(|c: char| c.is_ascii_whitespace() || c == ',')
        .is_empty()
    {
        value = value.trim_start_matches(|c: char| c.is_ascii_whitespace() || c == ',');
        let data = value.starts_with("data:");
        let end = if data {
            let first = value
                .find(',')
                .ok_or_else(|| ArchonError::Invalid("malformed srcset data URL".into()))?;
            value[first + 1..]
                .find(|c: char| c.is_ascii_whitespace() || c == ',')
                .map_or(value.len(), |n| first + 1 + n)
        } else {
            value
                .find(|c: char| c.is_ascii_whitespace() || c == ',')
                .unwrap_or(value.len())
        };
        let url = value[..end].trim_end_matches(',');
        if url.is_empty() {
            return Err(ArchonError::Invalid("empty srcset image".into()));
        }
        if !data {
            urls.push(url);
        }
        value = &value[end..];
        if value.starts_with(',') {
            value = &value[1..];
            continue;
        }
        value = match value.find(',') {
            Some(comma) => &value[comma + 1..],
            None => "",
        };
    }
    Ok(urls)
}

#[cfg(test)]
mod tests {
    use super::*;
    use archon_core::intent::{IntentSource, Requirement, RequirementKind};

    fn record() -> ApprovedIntent {
        ApprovedIntent {
            id: "test".into(),
            version: 1,
            operator_words: "Do this".into(),
            source: IntentSource::Inbox { seq: 1 },
            artifacts: vec![],
            images: vec![],
            requirements: vec![Requirement {
                id: "R1".into(),
                text: "Show title".into(),
                kind: RequirementKind::Wording,
                images: vec![],
                owner: "app".into(),
            }],
        }
    }

    #[test]
    fn local_and_srcset_images_are_saved_even_when_not_listed() {
        let dir = tempfile::tempdir().unwrap();
        let page_dir = dir.path().join("pages");
        std::fs::create_dir(&page_dir).unwrap();
        std::fs::write(page_dir.join("one image.png"), b"one").unwrap();
        std::fs::write(dir.path().join("two.png"), b"two").unwrap();
        let html = "<script>if (x < 3) {}</script><IMG src='one%20image.png'><picture><source srcset='../two.png 2x'></picture>";
        let mut intent = record();
        extract(
            html,
            &page_dir.join("index.html"),
            dir.path(),
            &dir.path().join("saved"),
            &mut intent,
        )
        .unwrap();
        assert_eq!(intent.images.len(), 2);
        assert_eq!(std::fs::read(&intent.images[0].path).unwrap(), b"one");
        assert_eq!(std::fs::read(&intent.images[1].path).unwrap(), b"two");
    }

    #[test]
    fn remote_and_escaping_images_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let page = dir.path().join("index.html");
        let mut intent = record();
        assert!(
            extract(
                "<img src='https://example.com/a.png'>",
                &page,
                dir.path(),
                dir.path(),
                &mut intent
            )
            .is_err()
        );
        assert!(local_path("../outside.png", &page, dir.path()).is_err());
        assert!(image_urls("<img src='unclosed>").is_err());
        assert_eq!(
            css_image_urls(".hero { background: url('../cover.png') }").unwrap(),
            vec!["../cover.png"]
        );
        assert_eq!(
            srcset("data:image/png;base64,aW1hZ2U= 1x, second.png 2x").unwrap(),
            vec!["second.png"]
        );
    }
}
