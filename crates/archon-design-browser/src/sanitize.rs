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
}
