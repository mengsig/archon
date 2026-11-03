//! The "Evidence" section of a pull request body, between its markers.

use std::collections::BTreeMap;

use super::{Clipped, Evidence, Image, Item, Shown};
use crate::layout::{EVIDENCE_END, EVIDENCE_START, defused};

/// The characters one section may take; entries past them are left out.
pub const MAX_SECTION_CHARS: usize = 16_000;

/// What became of each screenshot file the delivery uploaded: its link, or
/// why it could not be attached.
pub type Hosted = BTreeMap<String, Result<String, String>>;

/// The evidence section for `evidence` of stage `by`, with `hosted` the
/// links of its uploaded files.
pub fn section(evidence: &Evidence, by: &str, hosted: &Hosted) -> String {
    assert!(!by.is_empty(), "a stage gave the evidence");
    let mut text = format!("{EVIDENCE_START}\n## Evidence\n\n");
    let mut dropped = evidence.dropped.clone();
    let mut shown = 0;
    for (n, item) in evidence.items.iter().enumerate() {
        let rendered = item_text(item, hosted);
        if text.chars().count() + rendered.chars().count() > MAX_SECTION_CHARS {
            dropped.push(format!(
                "entry {} ({}) left out: the section's {MAX_SECTION_CHARS} characters are spent",
                n + 1,
                defused(&item.caption)
            ));
            continue;
        }
        text.push_str(&rendered);
        shown += 1;
    }
    if shown == 0 {
        text.push_str(&format!("{}\n\n", no_evidence(evidence, by)));
    }
    if !dropped.is_empty() {
        text.push_str("**Not shown**\n\n");
        for why in &dropped {
            text.push_str(&format!("- {}\n", defused(why)));
        }
        text.push('\n');
    }
    let text = format!("{}\n{EVIDENCE_END}", text.trim_end());
    debug_assert_eq!(text.matches(EVIDENCE_START).count(), 1, "one section");
    text
}

/// The line that stands for evidence when no entry is shown.
fn no_evidence(evidence: &Evidence, by: &str) -> String {
    let reason = match (&evidence.none, evidence.given) {
        (Some(reason), _) => defused(reason),
        (None, false) => format!("`{by}` gave no `evidence`."),
        (None, true) => format!("`{by}` gave no entry that could be shown."),
    };
    format!("No before/after evidence: {reason}")
}

fn item_text(item: &Item, hosted: &Hosted) -> String {
    let caption = defused(&item.caption);
    let body = match &item.shown {
        Shown::Screenshot { before, after } => {
            screenshots(&caption, before.as_ref(), after.as_ref(), hosted)
        }
        Shown::Table { columns, rows } => table(columns, rows),
        Shown::Output { before, after } => outputs(before.as_ref(), after.as_ref()),
    };
    let mut text = format!("**{caption}**\n\n{body}\n");
    for note in &item.notes {
        text.push_str(&format!("_Note: {}._\n", defused(note)));
    }
    text.push('\n');
    text
}

fn screenshots(
    caption: &str,
    before: Option<&Image>,
    after: Option<&Image>,
    hosted: &Hosted,
) -> String {
    let alt = caption.replace(['[', ']'], "");
    match (before, after) {
        (Some(before), Some(after)) => format!(
            "| Before | After |\n| --- | --- |\n| {} | {} |",
            image(&format!("{alt}, before"), before, hosted),
            image(&format!("{alt}, after"), after, hosted)
        ),
        (Some(before), None) => format!("Before: {}", image(&alt, before, hosted)),
        (None, Some(after)) => image(&alt, after, hosted),
        (None, None) => unreachable!("a screenshot entry names an image"),
    }
}

/// An image's Markdown, or why it could not be attached.
fn image(alt: &str, image: &Image, hosted: &Hosted) -> String {
    let link = match image {
        Image::Link(url) => Ok(url.as_str()),
        Image::File(path) => match hosted.get(path) {
            Some(Ok(url)) => Ok(url.as_str()),
            Some(Err(why)) => Err(why.clone()),
            None => Err("it was not uploaded".to_owned()),
        },
    };
    let link = link.and_then(|url| match super::is_web_link(url) {
        true => Ok(url),
        false => Err(format!("{url:?} is not an `https://` link")),
    });
    match link {
        Ok(url) => format!("![{alt}]({url})"),
        Err(why) => format!(
            "_The screenshot could not be attached: {}_",
            defused(&why).replace('|', "\\|").replace('\n', " ")
        ),
    }
}

fn table(columns: &[String], rows: &[Vec<String>]) -> String {
    assert!(!columns.is_empty() && !rows.is_empty(), "a table to render");
    let line = |cells: &[String]| {
        let cells: Vec<String> = cells.iter().map(|c| cell(c)).collect();
        format!("| {} |", cells.join(" | "))
    };
    let mut text = format!("{}\n|{}\n", line(columns), " --- |".repeat(columns.len()));
    for row in rows {
        text.push_str(&format!("{}\n", line(row)));
    }
    text.trim_end().to_owned()
}

fn cell(text: &str) -> String {
    defused(text).replace('|', "\\|")
}

fn outputs(before: Option<&Clipped>, after: Option<&Clipped>) -> String {
    match (before, after) {
        (Some(before), Some(after)) => format!(
            "Before:\n\n{}\n\nAfter:\n\n{}",
            fenced(&before.text),
            fenced(&after.text)
        ),
        (Some(before), None) => format!("Before:\n\n{}", fenced(&before.text)),
        (None, Some(after)) => fenced(&after.text),
        (None, None) => unreachable!("an output entry has text"),
    }
}

/// `text` in a fence longer than any run of backticks inside it.
fn fenced(text: &str) -> String {
    let longest = text.split(|c| c != '`').map(str::len).max().unwrap_or(0);
    let fence = "`".repeat(longest.max(2) + 1);
    let fenced = format!("{fence}text\n{}\n{fence}", defused(text));
    debug_assert!(fenced.starts_with("```"));
    fenced
}
