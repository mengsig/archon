//! What an operator's inbox message says explicitly: the requirement list
//! they introduced and the files they named by path.

/// Case, whitespace and trailing punctuation do not change a quote.
pub(super) fn normalized(text: &str) -> String {
    let joined = text.split_whitespace().collect::<Vec<_>>().join(" ");
    joined
        .trim_end_matches(['.', ',', ';', ':', '!'])
        .to_lowercase()
}

/// Whether `line` introduces the requirement list, such as `Requirements:`
/// or `Approve these requirements:`.
fn is_header(line: &str) -> bool {
    let line = line.trim().to_lowercase();
    line.ends_with("requirements:") && list_item(&line).is_none()
}

/// The text of a list item led by `-`, `*`, `•`, `1.` or `1)` and a space.
fn list_item(line: &str) -> Option<&str> {
    let line = line.trim_start();
    let rest = match line.strip_prefix(['-', '*', '•']) {
        Some(rest) => rest,
        None => {
            let digits = line.bytes().take_while(u8::is_ascii_digit).count();
            let rest = line[digits..].strip_prefix(['.', ')'])?;
            (digits > 0).then_some(rest)?
        }
    };
    rest.strip_prefix([' ', '\t'])
}

fn indent(line: &str) -> usize {
    line.len() - line.trim_start().len()
}

/// The operator's requirement list: the items right after their one line that
/// ends in `requirements:`, each whole with its deeper-indented lines, [`normalized`].
/// `Ok(empty)` when no such line introduces a list; more than one is ambiguous.
pub(super) fn requirement_list(words: &str) -> Result<Vec<String>, String> {
    let lines: Vec<&str> = words.lines().collect();
    let headers: Vec<usize> = (0..lines.len()).filter(|&i| is_header(lines[i])).collect();
    let start = match headers.as_slice() {
        [] => return Ok(Vec::new()),
        [header] => header + 1,
        _ => return Err("the operator's words have more than one `requirements:` line".into()),
    };
    let mut items: Vec<String> = Vec::new();
    let mut level = None;
    for line in lines[start..].iter().filter(|l| !l.trim().is_empty()) {
        let at = indent(line);
        let item = list_item(line).filter(|_| level.is_none_or(|level| at == level));
        if let Some(text) = item {
            level = Some(at);
            items.push(text.to_owned());
            continue;
        }
        match (items.last_mut(), level) {
            (Some(open), Some(level)) if at > level => {
                open.push(' ');
                open.push_str(line.trim());
            }
            _ => break,
        }
    }
    let items: Vec<String> = items.iter().map(|item| normalized(item)).collect();
    assert!(items.len() < lines.len(), "the header line is no item");
    if items.iter().any(String::is_empty) {
        return Err("the operator's requirement list has an empty item".into());
    }
    Ok(items)
}

/// The paths the operator wrote out: whole words that are absolute or start
/// at `~/`, as written. A bare file name names no file.
pub fn named_paths(words: &str) -> Vec<&str> {
    let named: Vec<&str> = words
        .split(|c: char| c.is_whitespace() || "`\"'()<>[],".contains(c))
        .map(|word| word.trim_end_matches(['.', ':', ';', '!', '?']))
        .filter(|word| word.len() > 1 && (word.starts_with('/') || word.starts_with("~/")))
        .collect();
    assert!(named.iter().all(|path| !path.is_empty()));
    named
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_list_a_requirements_line_introduces_counts_each_item_whole() {
        let incidental = "Please investigate these options:\n- Add caching\n- Drop the index";
        assert_eq!(requirement_list(incidental), Ok(vec![]));
        let words = "Approve these requirements:\n- Add caching\n  only if profiling shows it is needed.\n\n- Errors name\n  - the file\n  - the line\nThanks\n- not part of it";
        let listed = requirement_list(words).unwrap();
        assert_eq!(
            listed,
            vec![
                "add caching only if profiling shows it is needed",
                "errors name - the file - the line",
            ]
        );
        let numbered = "Requirements:\n  1. One\n  2) Two\nAlso:\n- not listed";
        assert_eq!(requirement_list(numbered).unwrap(), vec!["one", "two"]);
        let twice = "Requirements:\n- a\nMore requirements:\n- b";
        assert!(requirement_list(twice).is_err());
    }

    #[test]
    fn only_paths_written_out_name_files() {
        let words = "Match `/srv/shots/mockup.png` and ~/design/page.html, not mockup.png.";
        assert_eq!(
            named_paths(words),
            vec!["/srv/shots/mockup.png", "~/design/page.html"]
        );
        assert!(named_paths("Build it / ship it").is_empty());
    }
}
