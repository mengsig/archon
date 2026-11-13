//! Markdown bold in the chat: a terminal shows `**text**` as plain `text`.
//! Only well-formed pairs go; literal asterisks (`2 ** 10`, `src/**/*.rs`),
//! code spans and fenced blocks are kept exactly as written.

const MARK: &str = "**";

/// `text` with each `**bold**` pair's marks dropped, outside code.
pub fn without_bold_marks(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut fence: Option<&str> = None;
    for line in text.split_inclusive('\n') {
        let opens = fence_of(line);
        match (fence, opens) {
            (Some(open), Some(close)) if closes(open, close) => {
                fence = None;
                out.push_str(line);
            }
            (Some(_), _) => out.push_str(line),
            (None, Some(open)) => {
                fence = Some(open);
                out.push_str(line);
            }
            (None, None) => out.push_str(&line_without_bold_marks(line)),
        }
    }
    debug_assert!(out.len() <= text.len(), "only marks are dropped");
    debug_assert_eq!(out.matches('`').count(), text.matches('`').count());
    out
}

/// The fence run a line opens or closes with: three or more ``` ` ``` or `~`.
fn fence_of(line: &str) -> Option<&str> {
    let start = line.trim_start_matches(' ');
    let first = start.chars().next().filter(|c| matches!(c, '`' | '~'))?;
    let run = &start[..start.len() - start.trim_start_matches(first).len()];
    (run.len() >= 3).then_some(run)
}

/// A closing fence uses the opening one's character and is at least as long.
fn closes(open: &str, close: &str) -> bool {
    assert!(
        open.len() >= 3 && close.len() >= 3,
        "fences are runs of three"
    );
    close.starts_with(&open[..1]) && close.len() >= open.len()
}

fn line_without_bold_marks(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut rest = line;
    while let Some(c) = rest.chars().next() {
        if c == '`' {
            let span = code_span(rest);
            out.push_str(span);
            rest = &rest[span.len()..];
            continue;
        }
        if let Some(inner) = bold_inner(rest, out.ends_with('*')) {
            out.push_str(inner);
            rest = &rest[inner.len() + 2 * MARK.len()..];
            continue;
        }
        out.push(c);
        rest = &rest[c.len_utf8()..];
    }
    assert!(rest.is_empty(), "the whole line is copied");
    out
}

/// The code span `rest` starts with: a backtick run up to the next run of
/// the same length, or the opening run alone when none closes it.
fn code_span(rest: &str) -> &str {
    assert!(rest.starts_with('`'), "a code span starts with a backtick");
    let ticks = rest.len() - rest.trim_start_matches('`').len();
    let mut at = ticks;
    while let Some(found) = rest[at..].find('`') {
        let start = at + found;
        let run = rest[start..].len() - rest[start..].trim_start_matches('`').len();
        if run == ticks {
            return &rest[..start + run];
        }
        at = start + run;
    }
    &rest[..ticks]
}

/// The text of the `**bold**` pair `rest` starts with: marks that hug
/// non-blank text with no other asterisk or backtick, on one line.
fn bold_inner(rest: &str, after_star: bool) -> Option<&str> {
    let body = rest.strip_prefix(MARK)?;
    if after_star || body.starts_with(|c: char| c.is_whitespace() || c == '*') {
        return None;
    }
    let end = body.find(MARK)?;
    let inner = &body[..end];
    let after = &body[end + MARK.len()..];
    let hugs = !inner.ends_with(char::is_whitespace) && !after.starts_with('*');
    let plain = !inner.contains(['*', '`', '\n']);
    (hugs && plain).then_some(inner)
}

#[cfg(test)]
mod tests {
    use super::without_bold_marks;

    #[test]
    fn bold_pairs_lose_their_marks() {
        let cases = [
            (
                "**Archon:** aetherd had stopped",
                "Archon: aetherd had stopped",
            ),
            ("**Decision** (qi_7): opus", "Decision (qi_7): opus"),
            ("a **b** and **c d**.", "a b and c d."),
        ];
        for (text, plain) in cases {
            assert_eq!(without_bold_marks(text), plain, "{text:?}");
        }
    }

    #[test]
    fn literal_asterisks_stay() {
        for text in [
            "run cargo test -- src/**/*.rs",
            "2 ** 10 is 1024",
            "** not bold **",
            "***x***",
            "a ** b **c",
            "**unclosed",
        ] {
            assert_eq!(without_bold_marks(text), text, "{text:?}");
        }
    }

    #[test]
    fn code_keeps_its_marks() {
        for text in [
            "see `**x**` and ``a`**b**``",
            "```\n**x**\n```\n",
            "~~~\n**x**\n```\n**y**\n~~~\n",
            "````\n```\n**x**\n````\n",
        ] {
            assert_eq!(without_bold_marks(text), text, "{text:?}");
        }
        let after = "```\n**x**\n```\n**y** done";
        assert_eq!(without_bold_marks(after), "```\n**x**\n```\ny done");
        assert_eq!(without_bold_marks("`open **y**"), "`open y");
    }
}
