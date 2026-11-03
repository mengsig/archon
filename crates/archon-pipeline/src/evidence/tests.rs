use archon_core::risk::RiskLevel;
use serde_json::{Value, json};

use super::*;
use crate::layout::{compose, summary_section};
use crate::risk::{self, Assessment, DiffStat};

fn read(evidence: Value) -> Evidence {
    Evidence::read(Some(&json!({ "evidence": evidence })))
}

fn screenshot(before: &str, after: &str) -> Value {
    json!({"kind": "screenshot", "caption": "Queue screen", "before": before, "after": after})
}

#[test]
fn every_kind_is_read_with_its_before_and_after() {
    let evidence = read(json!([
        screenshot(".archon/evidence/before.png", "https://example.com/after.png"),
        {"kind": "table", "caption": "p50", "columns": ["metric", "before", "after"],
         "rows": [["p50 ms", 120, "80"]]},
        {"kind": "output", "caption": "cargo test", "text": "test result: ok. 12 passed"},
        {"kind": "output", "caption": "exit", "before": "1", "after": "0"},
    ]));
    assert!(
        evidence.given && evidence.dropped.is_empty(),
        "{evidence:?}"
    );
    assert_eq!(evidence.items.len(), 4);
    assert_eq!(
        evidence.items[0].shown,
        Shown::Screenshot {
            before: Some(Image::File(".archon/evidence/before.png".into())),
            after: Some(Image::Link("https://example.com/after.png".into())),
        }
    );
    let Shown::Table { rows, .. } = &evidence.items[1].shown else {
        panic!("a table")
    };
    assert_eq!(
        rows,
        &[vec!["p50 ms".to_owned(), "120".into(), "80".into()]]
    );
    let Shown::Output { before, after } = &evidence.items[2].shown else {
        panic!("an output")
    };
    assert!(before.is_none() && after.as_ref().unwrap().text.contains("12 passed"));
    assert_eq!(evidence.files(), [".archon/evidence/before.png"]);
}

#[test]
fn local_paths_and_other_links_are_dropped_with_why() {
    for (path, why) in [
        ("/home/me/shot.png", "is a local path"),
        ("~/shot.png", "is a local path"),
        (
            "file:///home/me/shot.png",
            "neither an `https://` link nor a file path",
        ),
        (
            "http://example.com/a.png",
            "neither an `https://` link nor a file path",
        ),
        (
            ".archon/evidence/../../etc/passwd.png",
            "not a file under `.archon/evidence/`",
        ),
        (
            ".archon/shot.png",
            "not a file under `.archon/evidence/` or `.archon/reports/`",
        ),
        ("src/shot.png", "not a file under `.archon/evidence/`"),
        (".archon/evidence/notes.txt", "not a PNG, JPEG, GIF or WebP"),
        (
            "https://x/a b.png",
            "neither an `https://` link nor a file path",
        ),
    ] {
        let evidence = read(json!([screenshot(path, "https://example.com/after.png")]));
        let notes = &evidence.items[0].notes;
        assert!(
            notes.len() == 1 && notes[0].starts_with("`before` left out") && notes[0].contains(why),
            "{path}: {notes:?}"
        );
        let alone = read(json!([{"kind": "screenshot", "caption": "c", "before": path}]));
        assert!(alone.items.is_empty(), "{path}");
        assert!(
            alone.dropped[0].starts_with("entry 1: `before` left out"),
            "{alone:?}"
        );
    }
    let kept = read(json!([screenshot(
        "./.archon/reports/a.PNG",
        "https://e.com/b.webp"
    )]));
    assert_eq!(kept.files(), [".archon/reports/a.PNG"]);
}

#[test]
fn unknown_kinds_and_shapeless_entries_are_named() {
    let evidence = read(json!([
        {"kind": "video", "caption": "demo"},
        {"caption": "no kind"},
        {"kind": "table", "caption": "  "},
        "just text",
        {"kind": "table", "caption": "empty", "columns": ["a"], "rows": []},
        {"kind": "output", "caption": "silent", "text": "   "},
    ]));
    assert!(evidence.items.is_empty());
    let dropped = evidence.dropped.join("\n");
    for said in [
        "entry 1: unknown kind `video`; kinds are `screenshot`, `table` and `output`",
        "entry 2: it has no `kind`",
        "entry 3: it has no `caption`",
        "entry 4: a string is not an object",
        "entry 5: a table has no `rows`",
        "entry 6: an output has no `text`, `before` or `after`",
    ] {
        assert!(dropped.contains(said), "missing {said:?} in {dropped}");
    }
    let odd = read(json!("see the screenshots"));
    assert!(odd.dropped[0].contains("`evidence` is a string"), "{odd:?}");
}

#[test]
fn oversized_tables_and_outputs_are_cut_and_say_so() {
    let columns: Vec<String> = (0..10).map(|n| format!("c{n}")).collect();
    let row: Vec<String> = (0..10).map(|_| "x".repeat(200)).collect();
    let rows: Vec<Vec<String>> = (0..30).map(|_| row.clone()).collect();
    let long: String = (0..100).map(|n| format!("line {n}\n")).collect();
    let evidence = read(json!([
        {"kind": "table", "caption": "wide", "columns": columns,
         "rows": rows.iter().cloned().chain([vec!["short".to_owned()]]).collect::<Vec<_>>()},
        {"kind": "output", "caption": "log", "text": long},
    ]));
    let Shown::Table { columns, rows } = &evidence.items[0].shown else {
        panic!("a table")
    };
    assert_eq!((columns.len(), rows.len()), (MAX_COLUMNS, MAX_ROWS));
    assert!(rows[0][0].chars().count() == MAX_CELL_CHARS && rows[0][0].ends_with('…'));
    let notes = evidence.items[0].notes.join("\n");
    for said in [
        "2 column(s) past the 8th left out",
        "row 31 left out: it is not a list of 10 cells",
        "5 row(s) past the 25th left out",
        "cells cut to 120 characters",
    ] {
        assert!(notes.contains(said), "missing {said:?} in {notes}");
    }
    let Shown::Output { after, .. } = &evidence.items[1].shown else {
        panic!("an output")
    };
    assert_eq!(
        after.as_ref().unwrap().text.lines().count(),
        MAX_OUTPUT_LINES
    );
    assert_eq!(
        evidence.items[1].notes,
        ["`after` cut to its first 40 of 100 lines"]
    );
    let wide = clip_output(&"y".repeat(MAX_OUTPUT_CHARS + 5));
    assert_eq!(wide.text.chars().count(), MAX_OUTPUT_CHARS);
    assert!(wide.cut.unwrap().contains("3000 characters"));
}

#[test]
fn entries_past_the_cap_are_counted_and_old_captures_still_show() {
    let many: Vec<Value> = (0..12)
        .map(|n| json!({"kind": "output", "caption": format!("o{n}"), "text": "ok"}))
        .collect();
    assert_eq!(
        read(json!(many)).dropped,
        ["2 more entries left out: at most 10 are shown"]
    );
    let legacy = Evidence::read(Some(&json!({"risk": {"level": "low", "why": "w",
        "captures": ["https://example.com/a.png", "/home/me/b.png", 7]}})));
    assert!(legacy.given);
    assert_eq!(legacy.items.len(), 1);
    assert_eq!(legacy.items[0].caption, "capture 1");
    assert!(legacy.dropped[0].contains("`/home/me/b.png` is not an `https://` link"));
    assert!(legacy.dropped[1].contains("a number is not an `https://` link"));
}

#[test]
fn the_section_renders_before_and_after_side_by_side() {
    let evidence = read(json!([
        screenshot(".archon/evidence/before.png", ".archon/evidence/after.png"),
        {"kind": "table", "caption": "p50 | p99", "columns": ["metric", "before", "after"],
         "rows": [["a|b", "1", "2"]]},
        {"kind": "output", "caption": "fenced", "text": "```\ninner <!-- /archon-evidence -->\n```"},
    ]));
    let mut hosted = Hosted::new();
    let before = "https://github.com/o/r/blob/abc/1.png?raw=true";
    hosted.insert(".archon/evidence/before.png".into(), Ok(before.into()));
    hosted.insert(".archon/evidence/after.png".into(), Err("HTTP 403".into()));
    let text = section(&evidence, "implement", &hosted);
    assert!(text.starts_with("<!-- archon-evidence -->\n## Evidence\n\n**Queue screen**"));
    assert!(text.ends_with("\n<!-- /archon-evidence -->"), "{text}");
    for said in [
        "| Before | After |\n| --- | --- |\n",
        &format!(
            "| ![Queue screen, before]({before}) | _The screenshot could not be attached: HTTP 403_ |"
        ),
        "| metric | before | after |\n| --- | --- | --- |\n| a\\|b | 1 | 2 |",
        "````text\n```\ninner <! -- /archon-evidence -->\n```\n````",
    ] {
        assert!(text.contains(said), "missing {said:?} in {text}");
    }
    assert_eq!(
        text.matches("<!-- /archon-evidence -->").count(),
        1,
        "{text}"
    );
    assert_eq!(
        section(&evidence, "implement", &hosted),
        text,
        "the same each time"
    );
    let unhosted = section(&evidence, "implement", &Hosted::new());
    assert!(unhosted.contains("_The screenshot could not be attached: it was not uploaded_"));
}

#[test]
fn no_evidence_is_said_with_its_reason_and_dropped_entries_are_listed() {
    let none = read(json!({"none": "a docs-only change; nothing runs differently"}));
    let text = section(&none, "implement", &Hosted::new());
    assert!(
        text.contains("No before/after evidence: a docs-only change; nothing runs differently"),
        "{text}"
    );
    let missing = section(&Evidence::read(Some(&json!({}))), "build", &Hosted::new());
    assert!(missing.contains("No before/after evidence: `build` gave no `evidence`."));
    let bad = read(json!([{"kind": "video", "caption": "demo"}]));
    let text = section(&bad, "build", &Hosted::new());
    assert!(text.contains("No before/after evidence: `build` gave no entry that could be shown."));
    assert!(
        text.contains("**Not shown**\n\n- entry 1: unknown kind `video`"),
        "{text}"
    );
    let promoted = Evidence::none_because("each change shows its own");
    assert!(
        section(&promoted, "Archon", &Hosted::new())
            .contains("evidence: each change shows its own")
    );
}

#[test]
fn entries_past_the_section_budget_are_left_out_and_named() {
    let big = "z".repeat(MAX_OUTPUT_CHARS);
    let entries: Vec<Value> = (0..10)
        .map(|n| json!({"kind": "output", "caption": format!("big {n}"), "before": big, "after": big}))
        .collect();
    let text = section(&read(json!(entries)), "implement", &Hosted::new());
    assert!(
        text.chars().count() < MAX_SECTION_CHARS + 2_000,
        "{}",
        text.len()
    );
    assert!(text.contains("entry 3 (big 2) left out: the section's 16000 characters are spent"));
    assert!(text.contains("**big 1**") && !text.contains("**big 2**"));
}

/// The three bodies the pull request of this change shows as its evidence:
/// `cargo test -p archon-pipeline sample_bodies -- --nocapture` prints them.
#[test]
fn sample_bodies_lead_with_the_level_the_summary_and_the_evidence() {
    let samples = [
        (
            RiskLevel::Medium,
            "made the queue screen show each item's risk. before: no level → now: a coloured `risk:` chip per row.",
            json!([screenshot(
                ".archon/evidence/queue-before.png",
                ".archon/evidence/queue-after.png"
            )]),
            "crates/archon-remote/web/queue.js",
            "web/queue.js",
        ),
        (
            RiskLevel::Medium,
            "made the store's queue listing use an index. before: 240 ms for 10k items → now: 9 ms.",
            json!([{"kind": "table", "caption": "`archon queue` on 10 000 items, median of 5 runs",
                 "columns": ["metric", "before", "after"],
                 "rows": [["list time", "240 ms", "9 ms"], ["rows scanned", "10 000", "37"], ["peak RSS", "41 MB", "39 MB"]]}]),
            "crates/archon-queue/src/store.rs",
            "src/store.rs",
        ),
        (
            RiskLevel::Low,
            "documented the evidence section. before: undocumented → now: in docs/PIPELINES.md.",
            json!({"none": "docs only; nothing runs differently"}),
            "docs/PIPELINES.md",
            "docs/PIPELINES.md",
        ),
    ];
    for (level, summary, evidence, file, short) in samples {
        let evidence = read(evidence);
        let mut hosted = Hosted::new();
        for (n, path) in evidence.files().into_iter().enumerate() {
            let url = format!(
                "https://github.com/o/r/blob/3f2a9c1e/{}.png?raw=true",
                n + 1
            );
            hosted.insert(path.to_owned(), Ok(url));
        }
        let diff = DiffStat::from_numstat(&format!("12\t3\t{file}\0")).unwrap();
        let assessment = Assessment {
            level,
            decided: format!("`build` proposed `{level}`, and its reviewer confirmed it."),
            why: format!("Touches only `{short}`; `make ci` passed."),
        };
        let facts = risk::Evidence {
            diff: Some(&diff),
            against: "`next`",
            checks: &["make ci".to_owned()],
            review: Some("REVIEW: pass, by build/1 (agent agt_r)"),
            label: &format!("`risk: {level}` is set."),
        };
        let top = [
            summary_section(level, "build", Some(summary)),
            section(&evidence, "build", &hosted),
        ];
        let technical = "## Technical detail\n\nApproach, key files and checks, in a few lines.";
        let risk = risk::section(&assessment, facts);
        let body = compose(technical, &[&top[0], &top[1]], &[&risk]);
        println!("----- sample: {summary}\n{body}");
        let at = |part: &str| {
            body.find(part)
                .unwrap_or_else(|| panic!("{part} in {body}"))
        };
        assert!(body.starts_with(&format!("<!-- archon-summary -->\nRisk level: {level}\n")));
        assert!(at("## What changed") < at("## Evidence"));
        assert!(at("## Evidence") < at("## Technical detail"));
        assert!(at("## Technical detail") < at(&format!("## Risk: {level}")));
        assert!(at(&format!("## Risk: {level}")) < at("<details>"));
        let again = compose(&body, &[&top[0], &top[1]], &[&risk]);
        assert_eq!(again, body, "re-rendering changes nothing");
    }
}

#[test]
fn table_cells_escape_trailing_backslashes_and_backslashes_before_pipes() {
    let evidence = read(json!([{
        "kind": "table", "caption": "Escaped cells", "columns": ["path\\", "value"],
        "rows": [["ends\\", "a\\|b"], ["two\\\\", "plain|pipe"]]
    }]));
    let text = section(&evidence, "implement", &Hosted::new());
    assert!(text.contains("| path\\\\ | value |"), "{text}");
    assert!(text.contains("| ends\\\\ | a\\\\\\|b |"), "{text}");
    assert!(text.contains("| two\\\\\\\\ | plain\\|pipe |"), "{text}");
}
