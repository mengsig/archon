use super::*;

fn tags(named: &[&str]) -> Vec<String> {
    named.iter().map(|tag| (*tag).to_owned()).collect()
}

fn risk(title: &str, named: &[&str], review: ReviewLevel, size: Option<DiffSize>) -> ItemRisk {
    let tags = tags(named);
    classify(RiskInput {
        title,
        brief: "",
        tags: &tags,
        review,
        size,
    })
}

fn class(title: &str, named: &[&str], review: ReviewLevel, size: Option<DiffSize>) -> RiskClass {
    risk(title, named, review, size).class
}

fn sized(base: Effort, risk: &ItemRisk, cap: Effort, model: &str) -> SizedEffort {
    let facts = StageFacts::new(base, StageKind::Work, false);
    let model: ModelRef = model.parse().unwrap();
    size_effort(facts, (risk, ReviewLevel::Standard), cap, &model)
}

const CLAUDE: &str = "claude-code/claude-opus-5-5";

#[test]
fn the_class_follows_the_design_s_rules() {
    use ReviewLevel::{Standard, Strict};
    let large = Some(DiffSize::Large);
    assert_eq!(class("Add a button", &[], Standard, None), RiskClass::Ordinary);
    assert_eq!(class("docs: fix a typo", &[], Standard, None), RiskClass::Mechanical);
    assert_eq!(class("Tidy", &["changelog"], Standard, None), RiskClass::Mechanical);
    assert_eq!(class("Add a button", &[], Strict, None), RiskClass::Elevated);
    assert_eq!(class("Add a button", &[], Standard, large), RiskClass::Elevated);
    assert_eq!(class("Add a button", &[], Strict, large), RiskClass::Elevated);
    assert_eq!(class("Fix the release workflow", &[], Standard, None), RiskClass::Elevated);
    assert_eq!(class("Harden the security check", &[], Standard, None), RiskClass::Elevated);
    assert_eq!(class("Harden the security check", &[], Strict, None), RiskClass::Critical);
    assert_eq!(class("Fix a deadlock", &[], Standard, large), RiskClass::Critical);
    assert_eq!(class("Add a button", &["risk:critical"], Standard, None), RiskClass::Critical);
}

/// A mechanical tag or title does not hide a risk word: the work is risky.
#[test]
fn a_mechanical_tag_with_a_security_word_is_not_mechanical() {
    let found = risk("docs: rotate the credentials", &["docs"], ReviewLevel::Standard, None);
    assert_eq!(found.class, RiskClass::Elevated);
    assert_eq!(found.signals, ["domain:security"]);
}

#[test]
fn risk_words_are_read_from_the_brief_and_only_as_whole_words() {
    let none = tags(&[]);
    let input = |brief| RiskInput {
        title: "Change the cache",
        brief,
        tags: &none,
        review: ReviewLevel::Standard,
        size: None,
    };
    let migrated = classify(input("It needs a data migration of the old rows."));
    assert_eq!(migrated.signals, ["domain:migration"]);
    assert_eq!(classify(input("Fix the race in the pump.")).class, RiskClass::Elevated);
    let traced = classify(input("Add a trace line and an insecure-looking name: obsecure."));
    assert_eq!(traced.class, RiskClass::Ordinary, "{:?}", traced.signals);
}

#[test]
fn a_version_bump_s_title_does_not_make_it_release_work_but_a_tag_does() {
    use ReviewLevel::Standard;
    let bump = "chore(release): version bump to 1.6.4";
    assert_eq!(class(bump, &[], Standard, None), RiskClass::Mechanical);
    assert_eq!(class(bump, &["release"], Standard, None), RiskClass::Elevated);
}

#[test]
fn a_small_ordinary_item_says_so_and_the_tag_names_the_size() {
    let small = risk("Add a button", &[], ReviewLevel::Standard, Some(DiffSize::Small));
    assert_eq!(small.signals, ["size:small"]);
    assert_eq!(tagged_size(&tags(&["x", "diff:large"])), Some(DiffSize::Large));
    assert_eq!(tagged_size(&tags(&["diff:small"])), Some(DiffSize::Small));
    assert_eq!(tagged_size(&tags(&["large"])), None);
}

/// The design's table: one row per stage default, one column per class.
#[test]
fn the_effort_follows_the_class_table_for_each_stage_default() {
    use ReviewLevel::{Standard, Strict};
    let small = Some(DiffSize::Small);
    let large = Some(DiffSize::Large);
    let columns = [
        risk("docs: tidy", &[], Standard, None),
        risk("Add a button", &[], Standard, small),
        risk("Add a button", &[], Standard, None),
        risk("Add a button", &[], Strict, None),
        risk("Add a button", &[], Standard, large),
        risk("Fix the mutex", &[], Standard, None),
        risk("Fix the mutex", &[], Strict, None),
        risk("Fix the mutex", &[], Standard, large),
        risk("Add a button", &["risk:critical"], Standard, None),
    ];
    use Effort::{High, Low, Medium, XHigh};
    let rows = [
        (Medium, [Low, Low, Medium, High, High, High, XHigh, XHigh, XHigh]),
        (High, [Low, Medium, High, High, High, High, XHigh, XHigh, XHigh]),
    ];
    for (base, efforts) in rows {
        for (risk, effort) in columns.iter().zip(efforts) {
            let got = sized(base, risk, XHigh, CLAUDE);
            assert_eq!(got.effort, effort, "{base} default, {risk:?}: {}", got.reason);
            assert!(got.reason.starts_with(&format!("sizer: {} -> ", risk.class)));
        }
    }
}

#[test]
fn a_cap_lowers_the_target_and_the_reason_says_so() {
    let critical = risk("Fix the mutex", &["risk:critical"], ReviewLevel::Standard, None);
    let open = sized(Effort::Medium, &critical, Effort::XHigh, CLAUDE);
    assert_eq!((open.effort, open.reason.as_str()), (Effort::XHigh, "sizer: critical -> x_high"));
    let capped = sized(Effort::Medium, &critical, Effort::High, CLAUDE);
    assert_eq!(capped.effort, Effort::High);
    let why = "sizer: critical -> x_high; capped at high; consider a stronger tier";
    assert_eq!(capped.reason, why);
    assert_eq!(sizer_cap(Some(Effort::High), Effort::XHigh), Effort::High);
    assert_eq!(sizer_cap(Some(Effort::XHigh), Effort::High), Effort::High);
    assert_eq!(sizer_cap(None, Effort::High), Effort::High);
    assert_eq!(sizer_cap(Some(Effort::Max), Effort::XHigh), Effort::XHigh);
}

/// `x_high` is planned only on the engines that honour it.
#[test]
fn an_engine_that_does_not_honour_x_high_gets_high_with_the_reason() {
    let critical = risk("x", &["risk:critical"], ReviewLevel::Standard, None);
    for engine in [CLAUDE, "codex/gpt-6.2-sol"] {
        assert_eq!(sized(Effort::Medium, &critical, Effort::XHigh, engine).effort, Effort::XHigh);
    }
    let other = sized(Effort::Medium, &critical, Effort::XHigh, "openrouter/some-model");
    assert_eq!(other.effort, Effort::High);
    let why = "sizer: critical -> x_high; openrouter runs no more than high, so high";
    assert_eq!(other.reason, why);
    let elevated = risk("Fix the mutex", &[], ReviewLevel::Standard, None);
    let unchanged = sized(Effort::Medium, &elevated, Effort::XHigh, "openrouter/some-model");
    assert_eq!(unchanged.reason, "sizer: elevated -> high");
}

/// The table's parenthesis: a check of mechanical work under a strict review
/// thinks at medium. The classifier never pairs the two; the rule holds anyway.
#[test]
fn a_check_of_mechanical_work_under_a_strict_review_gets_medium() {
    let mechanical = ItemRisk {
        class: RiskClass::Mechanical,
        signals: vec!["mechanical".to_owned()],
    };
    let model: ModelRef = CLAUDE.parse().unwrap();
    let effort = |kind, reviewers, review| {
        let facts = StageFacts::new(Effort::High, kind, reviewers);
        size_effort(facts, (&mechanical, review), Effort::XHigh, &model).effort
    };
    use ReviewLevel::{Standard, Strict};
    assert_eq!(effort(StageKind::Review, false, Strict), Effort::Medium);
    assert_eq!(effort(StageKind::Test, false, Strict), Effort::Medium);
    assert_eq!(effort(StageKind::Work, true, Strict), Effort::Medium);
    assert_eq!(effort(StageKind::Work, false, Strict), Effort::Low);
    assert_eq!(effort(StageKind::Review, false, Standard), Effort::Low);
}

#[test]
fn a_small_step_never_goes_below_low_and_names_itself() {
    let small = risk("Add a button", &[], ReviewLevel::Standard, Some(DiffSize::Small));
    let stepped = sized(Effort::High, &small, Effort::XHigh, CLAUDE);
    assert_eq!((stepped.effort, stepped.reason.as_str()), (Effort::Medium, "sizer: ordinary -> medium (small)"));
    assert_eq!(sized(Effort::Low, &small, Effort::XHigh, CLAUDE).effort, Effort::Low);
}
