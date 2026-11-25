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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_controls_and_caps() {
        assert_eq!(clean("a\u{0}b\nc\u{1b}[31md\te"), "abc[31mde");
        let long = "é".repeat(500);
        assert_eq!(clean(&long).chars().count(), 200);
        assert_eq!(clean("plain text"), "plain text");
    }
}
