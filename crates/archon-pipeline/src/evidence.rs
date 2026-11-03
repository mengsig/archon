//! The evidence a delivered pull request shows of what its change does
//! (ARCHITECTURE §8.7): screenshots, tables and command output, before and
//! after where they apply. The record stage lists it under `evidence`;
//! everything is checked here, at the boundary, and whatever is left out is
//! kept with the reason, for the section to show.

mod render;

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub use render::{Hosted, MAX_SECTION_CHARS, section};

/// Entries kept from one result.
pub const MAX_ITEMS: usize = 10;
/// The longest caption kept, in characters.
pub const MAX_CAPTION_CHARS: usize = 200;
/// Columns and rows a table keeps.
pub const MAX_COLUMNS: usize = 8;
pub const MAX_ROWS: usize = 25;
/// The longest table cell kept, in characters.
pub const MAX_CELL_CHARS: usize = 120;
/// Lines and characters one output part keeps.
pub const MAX_OUTPUT_LINES: usize = 40;
pub const MAX_OUTPUT_CHARS: usize = 3_000;
/// The longest "no evidence" reason kept, in characters.
pub const MAX_REASON_CHARS: usize = 300;
/// Where in the run worktree a screenshot file may lie: directories a
/// delivery refuses to commit, so the file never reaches the branch.
pub const FILE_ROOTS: [&str; 2] = [".archon/evidence/", ".archon/reports/"];
/// The image types a screenshot file may have, by extension.
pub const IMAGE_EXTENSIONS: [&str; 5] = ["png", "jpg", "jpeg", "gif", "webp"];

/// A screenshot: a web link, or a file in the run worktree under one of
/// [`FILE_ROOTS`] that the delivery uploads.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "at", rename_all = "snake_case")]
pub enum Image {
    Link(String),
    File(String),
}

/// Text that may have been cut, with what the cut left out.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Clipped {
    pub text: String,
    pub cut: Option<String>,
}

/// What one entry shows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Shown {
    Screenshot {
        before: Option<Image>,
        after: Option<Image>,
    },
    /// Before and after are columns of their own, if anything.
    Table {
        columns: Vec<String>,
        rows: Vec<Vec<String>>,
    },
    /// A single output is `after`.
    Output {
        before: Option<Clipped>,
        after: Option<Clipped>,
    },
}

/// One entry kept, with what was cut from it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Item {
    pub caption: String,
    pub shown: Shown,
    pub notes: Vec<String>,
}

/// The evidence of one result.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Evidence {
    pub items: Vec<Item>,
    /// Why nothing meaningful can be shown, as the agent said.
    pub none: Option<String>,
    /// Each entry left out, with why.
    pub dropped: Vec<String>,
    /// Whether the result had an `evidence` key at all.
    pub given: bool,
}

impl Evidence {
    /// Evidence that says only why there is none.
    pub fn none_because(reason: &str) -> Self {
        assert!(!reason.trim().is_empty(), "a reason is given");
        Self {
            none: Some(one_line(reason, MAX_REASON_CHARS)),
            given: true,
            ..Self::default()
        }
    }

    /// The evidence of a stage's `structured` result: `evidence`, a list of
    /// entries or `{"items": [...], "none": "<why>"}`, then the screenshot
    /// links of the older `risk.captures`.
    pub fn read(structured: Option<&Value>) -> Self {
        let mut evidence = Self::default();
        let raw = structured.and_then(|data| data.get("evidence"));
        let entries = match raw {
            None | Some(Value::Null) => Vec::new(),
            Some(raw) => {
                evidence.given = true;
                evidence.entries_of(raw)
            }
        };
        let captures = structured
            .and_then(|data| data.get("risk"))
            .and_then(|risk| risk.get("captures"));
        let captures = legacy_captures(captures, &mut evidence.dropped);
        evidence.given |= !captures.is_empty();
        for (n, entry) in entries.iter().enumerate() {
            match read_item(entry) {
                Ok(item) => evidence.items.push(item),
                Err(why) => evidence.dropped.push(format!("entry {}: {why}", n + 1)),
            }
        }
        evidence.items.extend(captures);
        evidence.keep_at_most(MAX_ITEMS);
        debug_assert!(evidence.items.len() <= MAX_ITEMS);
        evidence
    }

    /// The entries `raw` lists, keeping its `none` reason.
    fn entries_of(&mut self, raw: &Value) -> Vec<Value> {
        match raw {
            Value::Array(entries) => entries.clone(),
            Value::Object(object) => {
                let reason = object.get("none").and_then(Value::as_str);
                self.none = reason
                    .filter(|r| !r.trim().is_empty())
                    .map(|r| one_line(r, MAX_REASON_CHARS));
                match object.get("items") {
                    None | Some(Value::Null) => Vec::new(),
                    Some(Value::Array(entries)) => entries.clone(),
                    Some(other) => {
                        self.dropped
                            .push(format!("`evidence.items` is {}, not a list", kind(other)));
                        Vec::new()
                    }
                }
            }
            other => {
                self.dropped.push(format!(
                    "`evidence` is {}, not a list of entries or `{{\"none\": \"<why>\"}}`",
                    kind(other)
                ));
                Vec::new()
            }
        }
    }

    fn keep_at_most(&mut self, max: usize) {
        if self.items.len() <= max {
            return;
        }
        let over = self.items.len() - max;
        self.items.truncate(max);
        self.dropped.push(format!(
            "{over} more entr{} left out: at most {max} are shown",
            if over == 1 { "y" } else { "ies" }
        ));
    }

    /// Every file a screenshot names, for the delivery to upload.
    pub fn files(&self) -> Vec<&str> {
        let mut files: Vec<&str> = Vec::new();
        for item in &self.items {
            let Shown::Screenshot { before, after } = &item.shown else {
                continue;
            };
            for image in [before, after].into_iter().flatten() {
                if let Image::File(path) = image
                    && !files.contains(&path.as_str())
                {
                    files.push(path);
                }
            }
        }
        files
    }
}

/// The `https://` links of `risk.captures`, as screenshots after the change.
fn legacy_captures(captures: Option<&Value>, dropped: &mut Vec<String>) -> Vec<Item> {
    let links = captures.and_then(Value::as_array).into_iter().flatten();
    let mut items = Vec::new();
    for (n, link) in links.enumerate() {
        let text = link.as_str().map(str::trim).unwrap_or("");
        if !is_web_link(text) {
            dropped.push(format!("`risk.captures` {}: {}", n + 1, not_a_link(link)));
            continue;
        }
        items.push(Item {
            caption: format!("capture {}", items.len() + 1),
            shown: Shown::Screenshot {
                before: None,
                after: Some(Image::Link(text.to_owned())),
            },
            notes: Vec::new(),
        });
    }
    items
}

fn not_a_link(value: &Value) -> String {
    match value.as_str() {
        Some(text) => format!("{} is not an `https://` link", shown_value(text)),
        None => format!("{} is not an `https://` link", kind(value)),
    }
}

/// One entry, or why it is left out.
fn read_item(entry: &Value) -> Result<Item, String> {
    let Some(object) = entry.as_object() else {
        return Err(format!("{} is not an object", kind(entry)));
    };
    let caption = object.get("caption").and_then(Value::as_str).unwrap_or("");
    if caption.trim().is_empty() {
        return Err("it has no `caption`".into());
    }
    let caption = one_line(caption, MAX_CAPTION_CHARS);
    let mut notes = Vec::new();
    let shown = match object.get("kind").and_then(Value::as_str) {
        Some("screenshot") => screenshot(entry, &mut notes)?,
        Some("table") => table(entry, &mut notes)?,
        Some("output") => output(entry, &mut notes)?,
        Some(other) => {
            return Err(format!(
                "unknown kind {}; kinds are `screenshot`, `table` and `output`",
                shown_value(other)
            ));
        }
        None => return Err("it has no `kind`".into()),
    };
    assert!(!caption.is_empty(), "every kept entry has a caption");
    Ok(Item {
        caption,
        shown,
        notes,
    })
}

fn screenshot(entry: &Value, notes: &mut Vec<String>) -> Result<Shown, String> {
    let mut part = |side: &str| -> Option<Image> {
        let raw = entry.get(side)?;
        match image(raw) {
            Ok(image) => Some(image),
            Err(why) => {
                notes.push(format!("`{side}` left out: {why}"));
                None
            }
        }
    };
    let (before, after) = (part("before"), part("after"));
    if before.is_none() && after.is_none() {
        let why = match notes.is_empty() {
            true => "a screenshot names no `before` or `after` image".to_owned(),
            false => notes.join("; "),
        };
        return Err(why);
    }
    Ok(Shown::Screenshot { before, after })
}

/// A screenshot as the agent named it: an `https://` link, or a relative
/// path to an image file under one of [`FILE_ROOTS`] of the run worktree.
fn image(raw: &Value) -> Result<Image, String> {
    let Some(text) = raw.as_str().map(str::trim) else {
        return Err(format!("{} is not a link or a file path", kind(raw)));
    };
    if is_web_link(text) {
        return Ok(Image::Link(text.to_owned()));
    }
    let shown = shown_value(text);
    if text.contains("://") || text.starts_with("data:") {
        return Err(format!(
            "{shown} is neither an `https://` link nor a file path"
        ));
    }
    if text.starts_with('/') || text.starts_with('~') || text.contains('\\') {
        return Err(format!(
            "{shown} is a local path; name a file under `{}` relative to the \
             worktree, which Archon uploads",
            FILE_ROOTS[0]
        ));
    }
    let clean = text.trim_start_matches("./");
    let escapes = clean.split('/').any(|part| part == ".." || part.is_empty());
    if escapes || !FILE_ROOTS.iter().any(|root| clean.starts_with(root)) {
        return Err(format!(
            "{shown} is not a file under `{}` or `{}` of the worktree",
            FILE_ROOTS[0], FILE_ROOTS[1]
        ));
    }
    let extension = clean.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    if !IMAGE_EXTENSIONS.contains(&extension.as_str()) {
        return Err(format!(
            "{shown} is not a PNG, JPEG, GIF or WebP image by its name"
        ));
    }
    Ok(Image::File(clean.to_owned()))
}

fn table(entry: &Value, notes: &mut Vec<String>) -> Result<Shown, String> {
    let columns = entry.get("columns").and_then(Value::as_array);
    let Some(columns) = columns.filter(|c| !c.is_empty()) else {
        return Err("a table has no `columns`".into());
    };
    let mut cut_cells = false;
    let mut cell = |value: &Value| {
        let text = match value {
            Value::String(text) => text.clone(),
            Value::Null => String::new(),
            other => other.to_string(),
        };
        cut_cells |= text.chars().count() > MAX_CELL_CHARS;
        one_line(&text, MAX_CELL_CHARS)
    };
    let width = columns.len().min(MAX_COLUMNS);
    if columns.len() > width {
        notes.push(format!(
            "{} column(s) past the {MAX_COLUMNS}th left out",
            columns.len() - width
        ));
    }
    let header: Vec<String> = columns.iter().take(width).map(&mut cell).collect();
    let raw_rows = entry.get("rows").and_then(Value::as_array);
    let Some(raw_rows) = raw_rows.filter(|r| !r.is_empty()) else {
        return Err("a table has no `rows`".into());
    };
    let mut rows = Vec::new();
    for (n, row) in raw_rows.iter().enumerate() {
        let Some(row) = row.as_array().filter(|row| row.len() == columns.len()) else {
            notes.push(format!(
                "row {} left out: it is not a list of {} cells",
                n + 1,
                columns.len()
            ));
            continue;
        };
        rows.push(row.iter().take(width).map(&mut cell).collect::<Vec<_>>());
    }
    if rows.is_empty() {
        return Err(format!(
            "no row of the table could be shown: {}",
            notes.join("; ")
        ));
    }
    clip_rows(&mut rows, notes);
    if cut_cells {
        notes.push(format!("cells cut to {MAX_CELL_CHARS} characters"));
    }
    assert!(
        rows.iter().all(|row| row.len() == header.len()),
        "a rectangle"
    );
    Ok(Shown::Table {
        columns: header,
        rows,
    })
}

fn clip_rows(rows: &mut Vec<Vec<String>>, notes: &mut Vec<String>) {
    if rows.len() <= MAX_ROWS {
        return;
    }
    notes.push(format!(
        "{} row(s) past the {MAX_ROWS}th left out",
        rows.len() - MAX_ROWS
    ));
    rows.truncate(MAX_ROWS);
}

fn output(entry: &Value, notes: &mut Vec<String>) -> Result<Shown, String> {
    let mut part = |side: &str, keys: &[&str]| -> Option<Clipped> {
        let text = keys.iter().find_map(|key| entry.get(*key)?.as_str())?;
        let clipped = clip_output(text);
        if let Some(cut) = &clipped.cut {
            notes.push(format!("`{side}` {cut}"));
        }
        Some(clipped).filter(|c| !c.text.trim().is_empty())
    };
    let before = part("before", &["before"]);
    let after = part("after", &["after", "text"]);
    if before.is_none() && after.is_none() {
        return Err("an output has no `text`, `before` or `after`".into());
    }
    Ok(Shown::Output { before, after })
}

/// `text` cut to [`MAX_OUTPUT_LINES`] lines and [`MAX_OUTPUT_CHARS`]
/// characters, saying what was cut.
pub fn clip_output(text: &str) -> Clipped {
    let text = text.trim_end();
    let lines: Vec<&str> = text.lines().collect();
    let mut kept = lines[..lines.len().min(MAX_OUTPUT_LINES)].join("\n");
    let mut cut = (lines.len() > MAX_OUTPUT_LINES).then(|| {
        format!(
            "cut to its first {MAX_OUTPUT_LINES} of {} lines",
            lines.len()
        )
    });
    if let Some((at, _)) = kept.char_indices().nth(MAX_OUTPUT_CHARS) {
        kept.truncate(at);
        cut = Some(format!("cut to its first {MAX_OUTPUT_CHARS} characters"));
    }
    debug_assert!(kept.chars().count() <= MAX_OUTPUT_CHARS);
    Clipped { text: kept, cut }
}

/// Whether `url` is an `https://` link with nothing in it that could break
/// out of a Markdown image.
pub fn is_web_link(url: &str) -> bool {
    let unsafe_char = |c: char| c.is_whitespace() || matches!(c, '(' | ')' | '<' | '>' | '"');
    url.len() > "https://".len() && url.starts_with("https://") && !url.contains(unsafe_char)
}

/// `text` on one line, cut to `max` characters with an ellipsis.
fn one_line(text: &str, max: usize) -> String {
    assert!(max > 1, "room for a character and the ellipsis");
    let line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    match line.char_indices().nth(max) {
        None => line,
        Some((at, _)) => {
            let kept: String = line[..at].chars().take(max - 1).collect();
            format!("{kept}…")
        }
    }
}

/// A value the agent gave, quoted for a reason and cut short.
fn shown_value(text: &str) -> String {
    format!("`{}`", one_line(&text.replace('`', "'"), 80))
}

fn kind(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "a list",
        Value::Object(_) => "an object",
    }
}

#[cfg(test)]
mod tests;
