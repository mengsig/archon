use archon_core::risk::RiskLevel;
use serde_json::json;

use super::*;

fn call(level: RiskLevel, why: &str) -> RiskCall {
    RiskCall {
        level,
        why: why.into(),
    }
}

#[test]
fn a_proposal_reads_its_call_the_relayed_review_and_only_web_captures() {
    let result = json!({"risk": {"level": "Medium", "why": " touches the parser ",
        "reviewer": {"level": "high", "why": "it changes the wire format"},
        "captures": ["https://example.com/a.png", "/home/me/shot.png", "https://x/a b", 7]}});
    let read = Proposal::read(Some(&result));
    assert_eq!(read.call, Ok(call(RiskLevel::Medium, "touches the parser")));
    assert_eq!(
        read.reviewer,
        Some(call(RiskLevel::High, "it changes the wire format"))
    );
    assert_eq!(
        read.captures,
        ["https://example.com/a.png"],
        "no local path"
    );
    assert_eq!(
        Proposal::read(Some(&json!({}))).call,
        Err("no level was proposed".into())
    );
    let bad = Proposal::read(Some(&json!({"risk": {"level": "risky", "why": "w"}})));
    assert!(bad.call.unwrap_err().contains("\"risky\""));
    let silent = Proposal::read(Some(&json!({"risk": {"level": "low"}})));
    assert!(silent.call.unwrap_err().contains("`risk.why`"));
}

#[test]
fn a_reviewer_s_call_stands_and_no_proposal_is_high() {
    let mut proposal = Proposal::read(Some(&json!({"risk": {"level": "low", "why": "docs"}})));
    let kept = Assessment::settle("implement", &proposal, None);
    assert_eq!(kept.level, RiskLevel::Low);
    assert!(kept.decided.contains("no reviewer"), "{}", kept.decided);
    let review = call(RiskLevel::Medium, "it changes a default");
    let raised = Assessment::settle("implement", &proposal, Some(("stage `review`", &review)));
    assert_eq!(raised.level, RiskLevel::Medium);
    assert!(
        raised.decided.contains("changed it to `medium`"),
        "{}",
        raised.decided
    );
    assert!(raised.why.contains("docs") && raised.why.contains("a default"));
    proposal.reviewer = Some(call(RiskLevel::Low, "agreed"));
    let confirmed = Assessment::settle("implement", &proposal, None);
    assert!(
        confirmed.decided.contains("its reviewer confirmed"),
        "{}",
        confirmed.decided
    );
    let later = Assessment::settle("implement", &proposal, Some(("stage `review`", &review)));
    assert_eq!(
        later.level,
        RiskLevel::Medium,
        "a later review stage reads last"
    );
    let none = Proposal::read(None);
    let high = Assessment::settle("implement", &none, None);
    assert_eq!(high.level, RiskLevel::High);
    assert!(high.decided.contains("applies `high`"), "{}", high.decided);
    let unproposed = Assessment::settle("implement", &none, Some(("its reviewer", &review)));
    assert_eq!(
        unproposed.level,
        RiskLevel::High,
        "a reviewer never lowers it"
    );
    assert!(
        unproposed
            .decided
            .contains("its reviewer called it `medium`"),
        "{}",
        unproposed.decided
    );
    assert!(unproposed.why.contains("a default"), "{}", unproposed.why);
}

#[test]
fn commits_onto_a_pull_request_never_lower_its_label() {
    let proposal = Proposal::read(Some(&json!({"risk": {"level": "low", "why": "a typo"}})));
    let low = Assessment::settle("implement", &proposal, None);
    let kept = low.clone().onto(Some(RiskLevel::High));
    assert_eq!(kept.level, RiskLevel::High);
    assert!(
        kept.decided.contains("already labelled `high`"),
        "{}",
        kept.decided
    );
    assert_eq!(low.clone().onto(None), low);
    let medium = Assessment::settle(
        "implement",
        &Proposal::read(Some(&json!({"risk": {"level": "medium", "why": "w"}}))),
        None,
    );
    assert_eq!(medium.clone().onto(Some(RiskLevel::Low)), medium);
}

#[test]
fn the_highest_call_of_a_review_stage_counts() {
    let a = json!({"risk": {"level": "low", "why": "fine"}});
    let b = json!({"risk": {"level": "high", "why": "locks"}});
    let found = highest_call([Some(&a), None, Some(&b), Some(&json!({"risk": 3}))]);
    assert_eq!(found, Some(call(RiskLevel::High, "locks")));
    assert_eq!(highest_call([None, Some(&json!({}))]), None);
}

#[test]
fn numstat_reads_files_renames_and_binaries() {
    let raw = "3\t1\tsrc/a.rs\0-\t-\tdocs/logo.png\0\
               2\t0\t\0old/name.rs\0crates/x/src/new.rs\0";
    let diff = DiffStat::from_numstat(raw).unwrap();
    let paths: Vec<&str> = diff.files.iter().map(|f| f.path.as_str()).collect();
    assert_eq!(paths, ["src/a.rs", "docs/logo.png", "crates/x/src/new.rs"]);
    assert_eq!(diff.files[1].added, None, "a binary file");
    assert!(DiffStat::from_numstat("").unwrap().files.is_empty());
    assert!(DiffStat::from_numstat("garbage\0").is_err());
}

fn assessment(level: RiskLevel) -> Assessment {
    Assessment {
        level,
        decided: "`implement` proposed it.".into(),
        why: "Only the guide changed; `make ci` passed.".into(),
        captures: vec!["https://example.com/tui.png".into()],
    }
}

#[test]
fn the_section_states_the_meaning_the_reason_and_the_evidence() {
    let diff = DiffStat::from_numstat("10\t2\tdocs/guide.md\0").unwrap();
    let checks = ["make ci".to_owned()];
    let evidence = Evidence {
        diff: Some(&diff),
        against: "`main`",
        checks: &checks,
        review: Some("REVIEW: pass, by a/1 (agent agt_r)"),
        label: "`risk: low` is set.",
    };
    let text = section(&assessment(RiskLevel::Low), evidence);
    assert!(
        text.starts_with("<!-- archon-risk -->\n## Risk: low\n"),
        "{text}"
    );
    assert!(text.ends_with("<!-- /archon-risk -->"));
    for part in [
        RiskLevel::Low.meaning(),
        "`implement` proposed it.",
        "`make ci` passed",
        "Diff: 1 file(s), +10 −2, against `main`.",
        "Areas: `docs` (1).",
        "Kind: only tests and docs, by path.",
        "Checks run: `make ci`",
        "Review: REVIEW: pass",
        "Label: `risk: low` is set.",
        "![capture 1](https://example.com/tui.png)",
    ] {
        assert!(text.contains(part), "{part} in {text}");
    }
    let unread = Evidence {
        diff: None,
        checks: &[],
        review: None,
        ..evidence
    };
    let text = section(&assessment(RiskLevel::High), unread);
    assert!(text.contains("Diff: could not be read") && text.contains("none listed"));
    assert!(text.contains("no review is on record"), "{text}");
}

#[test]
fn many_files_are_counted_and_code_is_told_apart_from_tests() {
    let raw: String = (0..30)
        .map(|n| format!("1\t1\tcrates/c{n}/src/lib.rs\0"))
        .chain(["1\t0\tcrates/c0/tests/it.rs\0".to_owned()])
        .collect();
    let diff = DiffStat::from_numstat(&raw).unwrap();
    let evidence = Evidence {
        diff: Some(&diff),
        against: "`main`",
        checks: &[],
        review: None,
        label: "set",
    };
    let text = section(&assessment(RiskLevel::Medium), evidence);
    assert!(
        text.contains("and 6 more`") || text.contains("and 6 more."),
        "{text}"
    );
    assert!(text.contains("and 18 more"), "{text}");
    assert!(
        text.contains("touches code beyond tests and docs"),
        "{text}"
    );
}

#[test]
fn the_section_replaces_its_old_self_and_leads_the_body() {
    let first = section(
        &assessment(RiskLevel::Low),
        Evidence {
            diff: None,
            against: "`main`",
            checks: &[],
            review: None,
            label: "set",
        },
    );
    let body = with_section("What and why.\n", &first);
    assert!(
        body.starts_with(&first) && body.ends_with("\n\nWhat and why.\n"),
        "{body}"
    );
    let second = first.replace("## Risk: low", "## Risk: high");
    let again = with_section(&body, &second);
    assert_eq!(again, format!("{second}\n\nWhat and why.\n"));
    assert_eq!(with_section("", &first), format!("{first}\n"));
    let around = format!("Intro.\n{first}\nOutro.");
    assert_eq!(
        with_section(&around, &second),
        format!("{second}\n\nIntro.\n\nOutro.\n")
    );
    let broken = "<!-- archon-risk -->\nhalf";
    assert!(with_section(broken, &first).ends_with(&format!("\n\n{broken}\n")));
}
