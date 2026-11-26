//! Page-derived strings are untrusted: strip control characters and cap the length.

/// Longest page-derived string kept.
pub const MAX_CHARS: usize = 200;

/// Strip control characters and keep at most [`MAX_CHARS`] characters.
pub fn clean(s: &str) -> String {
    s.chars()
        .filter(|c| !c.is_control())
        .take(MAX_CHARS)
        .collect()
}

/// Page text meant to be read whole, such as a dialog's: line breaks and tabs
/// stay, other control characters go, and at most `max_bytes` are kept.
/// Returns the text and whether any of it was cut.
pub fn page_text(s: &str, max_bytes: usize) -> (String, bool) {
    assert!(max_bytes >= 4, "a cap holds at least one character");
    let mut text = String::new();
    let mut cut = false;
    for c in s
        .chars()
        .filter(|c| matches!(c, '\n' | '\t') || !c.is_control())
    {
        if text.len() + c.len_utf8() > max_bytes {
            cut = true;
            break;
        }
        text.push(c);
    }
    assert!(text.len() <= max_bytes, "the text fits its cap");
    (text, cut)
}

/// Longest file name kept, in bytes.
pub const MAX_FILE_NAME_BYTES: usize = 120;

/// A file name safe to create in a private directory: the last path
/// component only, no control or reserved characters, no leading dots.
pub fn file_name(raw: &str) -> String {
    let last = raw.rsplit(['/', '\\']).next().unwrap_or_default();
    let mut name = String::new();
    for c in last.chars().filter(|c| !c.is_control()) {
        let c = if matches!(c, '<' | '>' | ':' | '"' | '|' | '?' | '*') {
            '_'
        } else {
            c
        };
        if name.len() + c.len_utf8() > MAX_FILE_NAME_BYTES {
            break;
        }
        name.push(c);
    }
    let name = name.trim().trim_start_matches('.').trim();
    let name = if name.is_empty() { "file" } else { name };
    assert!(
        !name.contains(['/', '\\', '\0']),
        "a file name has no separator"
    );
    assert!(name.len() <= MAX_FILE_NAME_BYTES, "a file name is capped");
    name.to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_names_keep_only_a_safe_last_component() {
        assert_eq!(file_name("../../etc/passwd"), "passwd");
        assert_eq!(file_name("C:\\Users\\me\\BAPLIE.edi"), "BAPLIE.edi");
        assert_eq!(file_name("..."), "file");
        assert_eq!(file_name(""), "file");
        assert_eq!(file_name(".bashrc"), "bashrc");
        assert_eq!(file_name("a\u{0}b\nc<d>.txt"), "abc_d_.txt");
        assert_eq!(file_name(" lashing report.csv "), "lashing report.csv");
        let long = format!("{}.edi", "é".repeat(100));
        let kept = file_name(&long);
        assert!(kept.len() <= MAX_FILE_NAME_BYTES && kept.starts_with('é'));
    }

    #[test]
    fn strips_controls_and_caps() {
        assert_eq!(clean("a\u{0}b\nc\u{1b}[31md\te"), "abc[31mde");
        let long = "é".repeat(500);
        assert_eq!(clean(&long).chars().count(), 200);
        assert_eq!(clean("plain text"), "plain text");
    }

    #[test]
    fn page_text_keeps_lines_and_says_when_it_cuts() {
        assert_eq!(page_text("a\nb\tc\u{7}", 64), ("a\nb\tc".into(), false));
        let long = "é".repeat(300);
        let (kept, cut) = page_text(&long, 401);
        assert!(cut);
        assert_eq!(kept.len(), 400, "cut on a character boundary");
    }
}
