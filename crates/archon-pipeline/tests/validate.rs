//! `PipelineSpec::validate`: one case per ARCHITECTURE §8.2 rule, plus the
//! structural promises the built-in pipelines keep.

mod support;

use archon_core::catalog::{Gate, StageKind};
use archon_pipeline::builtin::BUILTINS;
use archon_pipeline::{Catalog, autonomy, template};

const HEADER: &str = r#"
[pipeline]
name = "demo"
description = "d"
version = 1
"#;

/// Validate `HEADER` + `stages` under directory name `demo`.
fn validate(stages: &str, prompts: &[(&str, &str)]) -> Result<(), String> {
    let spec = support::spec_from(&format!("{HEADER}{stages}"), prompts);
    spec.validate("demo").map_err(|e| e.to_string())
}

fn stage(id: &str, kind: &str, extra: &str) -> String {
    format!("[[stage]]\nid = \"{id}\"\nkind = \"{kind}\"\nprompt = \"{id}.md\"\n{extra}\n")
}

fn assert_refused(stages: &str, prompts: &[(&str, &str)], needle: &str) {
    let error = validate(stages, prompts).expect_err(needle);
    assert!(error.contains(needle), "expected {needle:?} in {error:?}");
}

#[test]
fn a_well_formed_pipeline_validates() {
    let stages = [
        stage("build", "work", ""),
        stage("check", "test", "gate = \"all_pass\""),
        stage(
            "review",
            "review",
            "fan_out = 3\ngate = \"majority\"\non_fail = { goto = \"build\", max_loops = 10 }",
        ),
        stage("ship", "ship", ""),
    ]
    .concat();
    let prompts = [(
        "ship.md",
        "{{previous.build.summary}} {{previous.review.summary}} {{{{literal}}",
    )];
    validate(&stages, &prompts).expect("valid");
}

#[test]
fn name_must_equal_directory_and_be_an_id() {
    let spec = support::spec_from(&format!("{HEADER}{}", stage("a", "work", "")), &[]);
    let error = spec.validate("other").expect_err("mismatch").to_string();
    assert!(
        error.contains("must equal its directory name \"other\""),
        "{error}"
    );

    let upper = HEADER.replace("name = \"demo\"", "name = \"Demo\"");
    let spec = support::spec_from(&format!("{upper}{}", stage("a", "work", "")), &[]);
    let error = spec.validate("Demo").expect_err("bad id").to_string();
    assert!(error.contains("[a-z0-9-]+"), "{error}");
}

#[test]
fn only_schema_version_one_exists() {
    let v2 = HEADER.replace("version = 1", "version = 2");
    let spec = support::spec_from(&format!("{v2}{}", stage("a", "work", "")), &[]);
    let error = spec.validate("demo").expect_err("v2").to_string();
    assert!(error.contains("version 2 is not supported"), "{error}");
}

#[test]
fn stage_ids_are_unique_and_well_formed() {
    assert_refused(
        &[stage("a", "work", ""), stage("a", "work", "")].concat(),
        &[],
        "more than one stage",
    );
    assert_refused(&stage("Build_1", "work", ""), &[], "id must match");
    assert_refused("", &[], "at least one [[stage]]");
}

#[test]
fn prompt_files_must_exist_and_stay_in_the_directory() {
    let mut spec = support::spec_from(&format!("{HEADER}{}", stage("a", "work", "")), &[]);
    spec.prompts.clear();
    let error = spec.validate("demo").expect_err("missing").to_string();
    assert!(error.contains("\"a.md\" does not exist"), "{error}");

    let escaping = "[[stage]]\nid = \"a\"\nkind = \"work\"\nprompt = \"../a.md\"\n";
    assert_refused(escaping, &[], "plain file name");
}

#[test]
fn goto_must_name_an_earlier_stage_within_the_loop_bound() {
    let gated = |goto: &str, loops: u32| {
        format!("gate = \"all_pass\"\non_fail = {{ goto = \"{goto}\", max_loops = {loops} }}")
    };
    let later = [stage("a", "review", &gated("b", 1)), stage("b", "work", "")].concat();
    assert_refused(&later, &[], "must name an earlier stage");
    let itself = stage("a", "review", &gated("a", 1));
    assert_refused(&itself, &[], "must name an earlier stage");
    let unknown = stage("a", "review", &gated("zzz", 1));
    assert_refused(&unknown, &[], "names no stage");
    let too_many = [
        stage("a", "work", ""),
        stage("b", "review", &gated("a", 11)),
    ]
    .concat();
    assert_refused(&too_many, &[], "at most 10");
}

#[test]
fn fan_out_is_bounded_and_refused_for_writers() {
    assert_refused(
        &stage("a", "review", "fan_out = 0"),
        &[],
        "between 1 and 16",
    );
    assert_refused(
        &stage("a", "review", "fan_out = 17"),
        &[],
        "between 1 and 16",
    );
    assert_refused(
        &stage("a", "work", "fan_out = 2"),
        &[],
        "fan_out > 1 is refused",
    );
    assert_refused(
        &stage("a", "ship", "fan_out = 2"),
        &[],
        "fan_out > 1 is refused",
    );
    validate(&stage("a", "report", "fan_out = 16"), &[]).expect("readers may fan out");
}

#[test]
fn gates_only_on_review_test_and_report() {
    assert_refused(
        &stage("a", "work", "gate = \"all_pass\""),
        &[],
        "a gate is only allowed",
    );
    assert_refused(
        &stage("a", "ship", "gate = \"any\""),
        &[],
        "a gate is only allowed",
    );
    for kind in ["review", "test", "report"] {
        validate(&stage("a", kind, "gate = \"any\""), &[]).expect(kind);
    }
}

#[test]
fn review_and_test_stages_need_a_gate() {
    for kind in ["review", "test"] {
        assert_refused(&stage("a", kind, ""), &[], "verdict is ignored");
        assert_refused(
            &stage("a", kind, "gate = \"none\""),
            &[],
            "a review or test stage needs gate",
        );
    }
    let looping = [
        stage("build", "work", ""),
        stage(
            "review",
            "review",
            "on_fail = { goto = \"build\", max_loops = 2 }",
        ),
    ]
    .concat();
    assert_refused(&looping, &[], "verdict is ignored");
    let crash_retry = [
        stage("build", "work", ""),
        stage(
            "pages",
            "report",
            "on_fail = { goto = \"build\", max_loops = 1 }",
        ),
    ]
    .concat();
    validate(&crash_retry, &[]).expect("an ungated report may loop on a failed agent");
}

#[test]
fn propose_and_report_stages_need_a_writable_workspace() {
    for (mode, allowed) in [
        ("repo-readonly", false),
        ("scratch", true),
        ("worktree", true),
    ] {
        let header = HEADER.replace(
            "version = 1",
            &format!("version = 1\nworkspace = \"{mode}\""),
        );
        for kind in ["propose", "report"] {
            let spec = support::spec_from(&format!("{header}{}", stage("a", kind, "")), &[]);
            match spec.validate("demo") {
                Ok(()) => assert!(allowed, "{kind} in {mode} must be refused"),
                Err(e) => {
                    assert!(!allowed, "{kind} in {mode}: {e}");
                    assert!(e.to_string().contains("need workspace"), "{e}");
                }
            }
        }
    }
}

#[test]
fn at_most_one_ship_stage_and_it_is_last() {
    let two = [stage("a", "ship", ""), stage("b", "ship", "")].concat();
    assert_refused(&two, &[], "at most one ship stage");
    let early = [stage("a", "ship", ""), stage("b", "work", "")].concat();
    assert_refused(&early, &[], "must be the last stage");
    let readonly = HEADER.replace("version = 1", "version = 1\nworkspace = \"repo-readonly\"");
    let spec = support::spec_from(&format!("{readonly}{}", stage("a", "ship", "")), &[]);
    let error = spec
        .validate("demo")
        .expect_err("ship needs a worktree")
        .to_string();
    assert!(error.contains("needs workspace = \"worktree\""), "{error}");
}

#[test]
fn placeholders_must_be_known() {
    let one = stage("a", "work", "");
    assert_refused(
        &one,
        &[("a.md", "{{task.titel}}")],
        "unknown placeholder {{task.titel}}",
    );
    assert_refused(
        &one,
        &[("a.md", "{{previous.a.summary}}")],
        "unknown placeholder",
    );
    assert_refused(
        &one,
        &[("a.md", "unterminated {{task.title")],
        "unterminated",
    );
    let two = [stage("a", "work", ""), stage("b", "work", "")].concat();
    validate(&two, &[("b.md", "{{previous.a.summary}}")]).expect("earlier stage summary is known");
    assert_refused(
        &two,
        &[("a.md", "{{previous.b.summary}}")],
        "unknown placeholder",
    );
    for name in template::KNOWN_VARIABLES {
        validate(&one, &[("a.md", &format!("{{{{{name}}}}}"))]).expect(name);
    }
}

#[test]
fn a_worktree_pipeline_requires_a_repo_and_timeouts_are_positive() {
    let no_repo = HEADER.replace("version = 1", "version = 1\nrequires_repo = false");
    let spec = support::spec_from(&format!("{no_repo}{}", stage("a", "work", "")), &[]);
    assert!(spec.validate("demo").is_err());
    assert_refused(
        &stage("a", "work", "timeout_minutes = 0"),
        &[],
        "timeout_minutes",
    );
}

#[test]
fn timeouts_up_to_thirty_days_validate_and_longer_ones_are_refused() {
    let max = archon_pipeline::spec::MAX_TIMEOUT_MINUTES;
    assert_eq!(max, 30 * 24 * 60);
    for minutes in [1, 24 * 60 + 1, 7 * 24 * 60, max] {
        validate(
            &stage("a", "work", &format!("timeout_minutes = {minutes}")),
            &[],
        )
        .unwrap_or_else(|e| panic!("{minutes} minutes: {e}"));
    }
    for minutes in [max + 1, u32::MAX] {
        assert_refused(
            &stage("a", "work", &format!("timeout_minutes = {minutes}")),
            &[],
            &format!("timeout_minutes must be between 1 and {max} (30 days), got {minutes}"),
        );
    }
}

#[test]
fn a_team_stage_implies_one_agent_per_role() {
    let team = [
        stage("build", "team", "roles = [\"api\", \"ui\"]"),
        stage("ship", "ship", ""),
    ]
    .concat();
    let prompts = [(
        "build.md",
        "{{team.role}} of {{team.roles}}: {{team.branches}}",
    )];
    validate(&team, &prompts).expect("a team stage validates");
    let spec = support::spec_from(&format!("{HEADER}{team}"), &prompts);
    let build = spec.stage("build").expect("declared");
    assert_eq!(build.fan_out, 2, "fan_out is implied by the roles");
    assert_eq!(spec.summary().stages[0].roles, ["api", "ui"]);
    let back = spec.manifest().expect("serializes");
    assert!(back.contains("roles = [\"api\", \"ui\"]"), "{back}");

    let explicit = stage("build", "team", "roles = [\"api\", \"ui\"]\nfan_out = 2");
    validate(&explicit, &[]).expect("fan_out may repeat the role count");
    let wrong = stage("build", "team", "roles = [\"api\", \"ui\"]\nfan_out = 3");
    assert_refused(
        &wrong,
        &[],
        "fan_out = 3 must equal the number of roles (2)",
    );
}

#[test]
fn team_roles_are_bounded_unique_and_slug_safe() {
    let team = |roles: &str| stage("build", "team", &format!("roles = [{roles}]"));
    assert_refused(
        &stage("build", "team", ""),
        &[],
        "needs 2 to 8 roles, found 0",
    );
    assert_refused(&team("\"api\""), &[], "needs 2 to 8 roles, found 1");
    let nine: Vec<String> = (0..9).map(|n| format!("\"r{n}\"")).collect();
    assert_refused(&team(&nine.join(", ")), &[], "found 9");
    assert_refused(&team("\"api\", \"api\""), &[], "listed more than once");
    for bad in ["Api", "a--b", "-a", "a-", "a/b", "", &"x".repeat(33)] {
        let stages = team(&format!("\"{bad}\", \"ui\""));
        assert_refused(&stages, &[], "must be 1 to 32 of [a-z0-9-]");
    }
    validate(&team("\"api-v2\", \"9\""), &[]).expect("digits and inner dashes are fine");
}

#[test]
fn roles_belong_to_team_stages_only() {
    assert_refused(
        &stage("build", "work", "roles = [\"api\", \"ui\"]"),
        &[],
        "only allowed on team stages",
    );
    let prompts = "roles = [\"api\", \"ui\"]\nrole_prompts = { db = \"x\" }";
    assert_refused(
        &stage("build", "team", prompts),
        &[],
        "names \"db\", which is not in roles",
    );
    let prompts = "roles = [\"api\", \"ui\"]\nrole_prompts = { ui = \"Own the CSS.\" }";
    validate(&stage("build", "team", prompts), &[]).expect("a role prompt for a role");
    for name in template::TEAM_VARIABLES {
        let text = format!("{{{{{name}}}}}");
        assert_refused(
            &stage("a", "work", ""),
            &[("a.md", &text)],
            "only a team stage's prompt may use",
        );
    }
}

#[test]
fn a_team_stage_has_no_gate_and_needs_a_worktree() {
    let roles = "roles = [\"api\", \"ui\"]";
    assert_refused(
        &stage("build", "team", &format!("{roles}\ngate = \"all_pass\"")),
        &[],
        "a gate is only allowed on review, test or report stages",
    );
    for mode in ["scratch", "repo-readonly"] {
        let header = HEADER.replace(
            "version = 1",
            &format!("version = 1\nworkspace = \"{mode}\"\nrequires_repo = true"),
        );
        let spec = support::spec_from(&format!("{header}{}", stage("b", "team", roles)), &[]);
        let error = spec.validate("demo").expect_err(mode).to_string();
        assert!(error.contains("needs workspace = \"worktree\""), "{error}");
    }
}

#[test]
fn team_roles_and_worker_directories_are_unique_across_a_pipeline() {
    let reused = [
        stage("one", "team", "roles = [\"api\", \"ui\"]"),
        stage("two", "team", "roles = [\"api\", \"db\"]"),
    ]
    .concat();
    assert_refused(
        &reused,
        &[],
        "role \"api\" is used by team stages \"one\" and \"two\"",
    );
    let clash = [
        stage("a-b", "team", "roles = [\"c\", \"x\"]"),
        stage("a", "team", "roles = [\"b-c\", \"y\"]"),
    ]
    .concat();
    assert_refused(&clash, &[], "both put a worker in directory \"a-b-c\"");
}

#[test]
fn every_builtin_validates_and_keeps_its_promises() {
    let catalog = Catalog::builtin();
    assert!(catalog.problems().is_empty(), "{:#?}", catalog.problems());
    for builtin in BUILTINS {
        let spec = catalog.get(builtin.name).expect("built-in loads");
        let stages = &spec.file.stages;
        for stage in stages {
            let text = &spec.prompts[&stage.prompt];
            let names = template::placeholders(text).expect("parses");
            assert!(
                names.iter().any(|n| n == "task.brief"),
                "{}/{}: brief",
                builtin.name,
                stage.id
            );
            if matches!(stage.kind, StageKind::Review | StageKind::Test) {
                assert_ne!(
                    stage.gate,
                    Gate::None,
                    "{}/{}: review and test stages gate",
                    builtin.name,
                    stage.id
                );
            }
            let uses = |name: &str| names.iter().any(|n| n == name);
            if stage.kind == StageKind::Ship {
                assert!(
                    uses("ship.mode") && uses("ship.auto_merge"),
                    "{}: ship follows the repo's mode and merge opt-in",
                    builtin.name
                );
            }
            if stage.kind == StageKind::Team {
                assert!(
                    uses("team.role") && uses("team.roles"),
                    "{}/{}: each role knows its part and its peers",
                    builtin.name,
                    stage.id
                );
            } else if stage.fan_out > 1 {
                assert!(
                    uses("fan.index") && uses("fan.count"),
                    "{}/{}: each slot knows its share of the work",
                    builtin.name,
                    stage.id
                );
            }
        }
        // A change-report revision re-enters the first work stage.
        if autonomy::ships(spec) {
            let first_work = stages
                .iter()
                .find(|s| s.kind == StageKind::Work)
                .expect("a shipping built-in has work to ship");
            let names = template::placeholders(&spec.prompts[&first_work.prompt]).expect("parses");
            assert!(
                names.iter().any(|n| n == "feedback"),
                "{}/{}: a revision target must show {{{{feedback}}}}",
                builtin.name,
                first_work.id
            );
        }
        for stage in stages.iter().filter_map(|s| s.on_fail.as_ref()) {
            let target = spec.stage(&stage.goto).expect("goto target");
            let names = template::placeholders(&spec.prompts[&target.prompt]).expect("parses");
            assert!(
                names.iter().any(|n| n == "feedback"),
                "{}/{}: a loop target must show {{{{feedback}}}}",
                builtin.name,
                target.id
            );
        }
        let summary = spec.summary();
        let files: Vec<&str> = builtin.files.iter().map(|(n, _)| *n).skip(1).collect();
        for stage in &summary.stages {
            assert!(files.contains(&spec.stage(&stage.id).expect("stage").prompt.as_str()));
        }
        assert_eq!(
            files.len(),
            stages.len(),
            "{}: no unused prompt files",
            builtin.name
        );
    }
}

/// archond ships what one agent recorded, so `[pipeline.ship]` names a
/// single-agent stage of a worktree pipeline after which nothing commits.
#[test]
fn a_daemon_ship_names_the_last_stage_that_can_commit() {
    let direct = include_str!("../../../pipelines/direct/pipeline.toml");
    let check = |text: &str| -> Result<(), String> {
        let spec = support::spec_from(text, &[]);
        spec.validate("direct").map_err(|e| e.to_string())
    };
    check(direct).expect("the built-in declaration is valid");
    let ship_agent = "\n[[stage]]\nid = \"deliver\"\nkind = \"ship\"\nprompt = \"implement.md\"\n\
                      timeout_minutes = 5\n";
    let later_work = "\n[[stage]]\nid = \"polish\"\nkind = \"work\"\nprompt = \"implement.md\"\n\
                      timeout_minutes = 5\n";
    let fanned = direct.replace(
        "kind = \"work\"",
        "kind = \"review\"\nfan_out = 2\ngate = \"all_pass\"",
    );
    for (text, why) in [
        (
            direct.replace(
                "[pipeline.ship]\nstage = \"implement\"",
                "[pipeline.ship]\nstage = \"nowhere\"",
            ),
            "[pipeline.ship] stage \"nowhere\" names no stage",
        ),
        (
            format!("{direct}{ship_agent}"),
            "cannot be combined with ship stage \"deliver\"",
        ),
        (
            format!("{direct}{later_work}"),
            "must come after stage \"polish\", which can still commit",
        ),
        (
            fanned.replace(
                "[pipeline.review]\nlevel = \"none\"\nstage = \"implement\"\n",
                "",
            ),
            "must be a work, review or test stage with one agent",
        ),
    ] {
        let refused = check(&text).unwrap_err();
        assert!(refused.contains(why), "{why:?}: {refused}");
    }
    let scratch = direct
        .replace("workspace = \"worktree\"", "workspace = \"scratch\"")
        .replace("requires_repo = true", "requires_repo = false");
    let refused = check(&scratch).unwrap_err();
    assert!(
        refused.contains("needs workspace = \"worktree\""),
        "{refused}"
    );
}

#[test]
fn adaptive_declares_the_review_its_lead_holds_and_round_trips() {
    use archon_core::autonomy::ReviewLevel;

    let catalog = Catalog::builtin();
    let spec = catalog.get("adaptive").unwrap();
    assert_eq!(spec.file.stages.len(), 1, "the lead, and archond ships");
    let build = spec.stage("build").unwrap();
    assert_eq!(build.kind, StageKind::Work);
    assert!(build.allow_delegation);
    assert_eq!(build.fan_out, 1);
    assert!(spec.holds_review(build));
    assert_eq!(
        spec.ship_record_stage().map(|s| s.id.as_str()),
        Some("build")
    );
    assert_eq!(autonomy::review_level(spec), ReviewLevel::Standard);
    assert!(autonomy::review_is_chosen(spec), "an item picks its level");
    let text = toml::to_string(&spec.file).unwrap();
    let back: archon_pipeline::spec::PipelineFile = toml::from_str(&text).unwrap();
    assert_eq!(back, spec.file);
    let holders: Vec<&str> = catalog
        .names()
        .filter(|name| catalog.get(name).unwrap().file.pipeline.review.is_some())
        .collect();
    assert_eq!(
        holders,
        ["adaptive", "direct"],
        "the rest gate their review"
    );
}

/// What `adaptive` tells its lead decides what a run costs and what is
/// delivered: the lead prices each child and records what archond ships.
#[test]
fn adaptive_prices_what_it_delegates_and_records_what_ships() {
    let catalog = Catalog::builtin();
    let spec = catalog.get("adaptive").unwrap();
    let flat = |text: &str| text.split_whitespace().collect::<Vec<_>>().join(" ");
    let build = flat(&spec.prompts["build.md"]);
    for said in [
        "saves more than briefing it, integrating it and the reading it repeats",
        "Do a bounded local read yourself",
        "no `Co-Authored-By` or \"Generated with\" line",
        "Delegate the reviewers your contract asks for on the committed change with `wait: false`",
        "never see each other's findings",
        "full test suite once, and edit nothing until all are back",
        "With no reviewer asked for, that run is the whole of this step",
        "the exact base and head commits (`git merge-base {{base_branch}} HEAD`",
        "medium for ordinary risk, high where security or an invariant is at stake",
        "never because a tree that passed was committed",
        "for any commit after a pass: Archon delivers only commits a passing reviewer read",
        "It reads that delta (`base` the reviewed commit) and the callers it affects",
        "Use leaf workers unless a bounded subproblem benefits from another level allowed by the contract",
        "Continue while fixes make progress within the item's time and spending limits; \
         escalate repeated findings or a stalled approach with evidence",
        "An unresolved blocking finding, a failed check",
        "Advice left open fails nothing",
        "Archon delivers your commits as they are. Do not push.",
        "the ship record and, where reviewers were asked for, the review record",
    ] {
        assert!(build.contains(said), "build.md lacks {said:?}");
    }
    assert!(!build.contains("Open findings"), "only blocking ones fail");
    assert!(
        !build.contains("Each worker is a leaf"),
        "another level may pay"
    );
    assert!(
        !build.contains("at most two"),
        "progress, not a count, bounds fixing"
    );
    assert!(!spec.prompts.contains_key("ship.md"), "no agent ships");
    let nudge = spec.stage("build").unwrap().nudge.as_deref().unwrap();
    assert!(nudge.contains("at high effort; medium for a bounded change"));
    assert!(nudge.contains("x_high or max only for a reasoning problem you can name"));
}

/// `direct` escalates on what a change risks and reaches into, not on its
/// size, and its reviewers return the verdict Archon checks.
#[test]
fn direct_escalates_on_risk_not_size() {
    let catalog = Catalog::builtin();
    let prompt = &catalog.get("direct").unwrap().prompts["implement.md"];
    let prompt = prompt.split_whitespace().collect::<Vec<_>>().join(" ");
    assert!(!prompt.contains("few hundred lines"), "{prompt}");
    assert!(prompt.contains("Size alone is no risk"), "{prompt}");
    assert!(
        prompt.contains("the verdict your contract shows"),
        "{prompt}"
    );
}

/// A declared review is a claim Archon enforces at one agent's result, so
/// only a work stage can hold it; the level is the default an item overrides.
#[test]
fn a_declared_review_names_one_work_stage_at_any_level() {
    let adaptive = include_str!("../../../pipelines/adaptive/pipeline.toml");
    let check = |text: &str| -> Result<(), String> {
        toml::from_str::<archon_pipeline::spec::PipelineFile>(text).map_err(|e| e.to_string())?;
        let spec = support::spec_from(text, &[]);
        spec.validate("adaptive").map_err(|e| e.to_string())
    };
    check(adaptive).expect("the built-in declaration is valid");
    let level = "level = \"standard\"";
    let stage = "[pipeline.review]\nlevel = \"standard\"\nstage = \"build\"";
    assert!(
        adaptive.contains(stage),
        "the declaration is in the built-in"
    );
    for chosen in ["none", "strict"] {
        let text = adaptive.replace(level, &format!("level = \"{chosen}\""));
        check(&text).unwrap_or_else(|why| panic!("{chosen} is a level: {why}"));
    }
    check(&adaptive.replace("allow_delegation = true", "allow_delegation = false"))
        .expect("the holder is granted delegation for its reviewers");
    let gated = "\n[[stage]]\nid = \"check\"\nkind = \"test\"\nprompt = \"build.md\"\n\
                 gate = \"all_pass\"\ntimeout_minutes = 5\n";
    for (text, why) in [
        (adaptive.replace(level, "level = \"thorough\""), "thorough"),
        (
            adaptive.replace(
                stage,
                "[pipeline.review]\nlevel = \"standard\"\nstage = \"review\"",
            ),
            "[pipeline.review] stage \"review\" names no stage",
        ),
        (
            format!(
                "{}{gated}",
                adaptive.replace(
                    stage,
                    "[pipeline.review]\nlevel = \"standard\"\nstage = \"check\""
                )
            ),
            "[pipeline.review] stage \"check\" must be a work stage",
        ),
        (
            adaptive.replace(stage, &format!("{stage}\nreviewer = \"x\"")),
            "unknown field",
        ),
    ] {
        let refused = check(&text).unwrap_err();
        assert!(refused.contains(why), "{why:?}: {refused}");
    }
}
