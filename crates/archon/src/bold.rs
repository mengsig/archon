//! The bold label an entry opens with, such as Archon's `**Archon:** …` and
//! `**Decision** (qi_7): …`, shows plain in the terminal. Only that leading
//! pair goes: nothing before it can escape it or open code, so it is always
//! Markdown bold. All other text, asterisks included, stays as written.

const MARK: &str = "**";

/// `text` without the marks of a `**label**` it starts with.
pub fn without_leading_bold(text: &str) -> String {
    let Some(label) = leading_bold(text) else {
        return text.to_owned();
    };
    let rest = &text[2 * MARK.len() + label.len()..];
    let plain = format!("{label}{rest}");
    assert_eq!(plain.len() + 2 * MARK.len(), text.len(), "only marks go");
    assert!(plain.starts_with(label) && !label.is_empty());
    plain
}

/// The label of a leading `**label**`: words, digits, spaces and `:` that
/// hug the marks, so no link, code or HTML can start inside it.
fn leading_bold(text: &str) -> Option<&str> {
    let body = text.strip_prefix(MARK)?;
    let end = body.find(MARK)?;
    let label = &body[..end];
    let after = body[end + MARK.len()..].chars().next();
    let hugs = !label.starts_with(char::is_whitespace) && !label.ends_with(char::is_whitespace);
    let inert = !label.is_empty()
        && label
            .chars()
            .all(|c| c.is_alphanumeric() || c == ' ' || c == ':');
    let closes = after.is_none_or(|c| c.is_whitespace() || (c.is_ascii_punctuation() && c != '*'));
    (hugs && inert && closes).then_some(label)
}

#[cfg(test)]
mod tests {
    use super::without_leading_bold;

    #[test]
    fn a_leading_bold_label_shows_plain() {
        let cases = [
            (
                "**Archon:** aetherd had stopped",
                "Archon: aetherd had stopped",
            ),
            ("**Decision** (qi_7): opus", "Decision (qi_7): opus"),
            ("**Q4:** Ship it?\n1. yes", "Q4: Ship it?\n1. yes"),
            ("**Outcome**: merged", "Outcome: merged"),
            ("**Done**", "Done"),
        ];
        for (text, plain) in cases {
            assert_eq!(without_leading_bold(text), plain, "{text:?}");
        }
    }

    #[test]
    fn everything_else_stays_as_written() {
        for text in [
            "run cargo test -- src/**/*.rs",
            "2 ** 10 is 1024",
            "a **b** later is kept",
            "** spaced **",
            "***x***",
            "**a*b**",
            "**`code`**",
            "**a\\**b",
            "**two\nlines**",
            "**word**s",
            "****",
            "**unclosed",
            "\\**escaped**",
            "**<https://example.com/**> tail",
            "**[a](b**)",
            "**<b>**",
            "**a&amp;**",
            "**a_b**",
            "```\n**x**\n```",
            "plain",
            "",
        ] {
            assert_eq!(without_leading_bold(text), text, "{text:?}");
        }
    }
}
