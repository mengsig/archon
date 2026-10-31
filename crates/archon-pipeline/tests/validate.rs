//! `PipelineSpec::validate`: one case per ARCHITECTURE §8.2 rule, plus the
//! structural promises the built-in pipelines keep.

mod support;

use std::collections::BTreeMap;

use archon_core::autonomy::{Autonomy, ProposalMode, ReportMode};
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
fn an_empty_or_blank_prompt_is_refused_with_stage_and_file() {
    for blank in ["", " \n\t "] {
        let error = validate(&stage("a", "work", ""), &[("a.md", blank)]).expect_err("blank");
        assert!(error.contains("stage \"a\""), "{error}");
        assert!(error.contains("\"a.md\" is empty"), "{error}");
    }
}

#[test]
fn a_bad_name_is_reported_together_with_missing_stages() {
    let upper = HEADER.replace("name = \"demo\"", "name = \"Upper-Case\"");
    let spec = support::spec_from(&upper, &[]);
    let error = spec.validate("Upper-Case").expect_err("bad").to_string();
    assert!(error.contains("[a-z0-9-]+"), "{error}");
    assert!(error.contains("at least one [[stage]]"), "{error}");
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
fn a_team_stage_may_take_its_roles_from_an_earlier_planner() {
    let planner = stage("plan", "work", "");
    let team = stage("build", "team", "roles_from = \"plan\"");
    let stages = [planner.clone(), team.clone()].concat();
    validate(&stages, &[]).expect("a team waiting for a plan validates");
    let spec = support::spec_from(&format!("{HEADER}{stages}"), &[]);
    let build = spec.stage("build").expect("declared");
    assert!(build.awaits_roles(), "no roles until the plan assigns them");
    assert_eq!(build.fan_out, 8, "room for the largest plan");
    assert_eq!(build.choice_limit(), 8);
    assert_eq!(
        spec.assigned_by("plan").map(|s| s.id.as_str()),
        Some("build")
    );
    assert_eq!(spec.summary().stages[1].roles_from.as_deref(), Some("plan"));
    let back = spec.manifest().expect("serializes");
    assert!(back.contains("roles_from = \"plan\""), "{back}");
    let frozen = support::spec_from(&back, &[]);
    assert_eq!(frozen.file, spec.file, "a frozen copy loads as it was");
    frozen.validate("demo").expect("and validates again");
}

#[test]
fn roles_from_names_one_earlier_planner_and_leaves_the_roles_to_it() {
    let planner = stage("plan", "work", "");
    let team = stage("build", "team", "roles_from = \"plan\"");
    let later = [team.clone(), planner.clone()].concat();
    assert_refused(
        &later,
        &[],
        "roles_from = \"plan\" must name an earlier stage",
    );
    let reviewer = stage("plan", "review", "gate = \"all_pass\"");
    assert_refused(
        &[reviewer, team.clone()].concat(),
        &[],
        "must name a work stage with one agent",
    );
    let twice =
        [planner.clone(), team.clone()].concat() + &stage("again", "team", "roles_from = \"plan\"");
    assert_refused(
        &twice,
        &[],
        "another team stage already takes its roles from \"plan\"",
    );
    let prompts = stage(
        "build",
        "team",
        "roles_from = \"plan\"\nrole_prompts = { a = \"x\" }",
    );
    assert_refused(
        &[planner.clone(), prompts].concat(),
        &[],
        "with roles_from the plan writes them",
    );
    let sized = stage("build", "team", "roles_from = \"plan\"\nfan_out = 3");
    assert_refused(
        &[planner.clone(), sized].concat(),
        &[],
        "fan_out = 3 is set by the plan",
    );
    assert_refused(
        &stage("plan", "work", "roles_from = \"x\""),
        &[],
        "only allowed on team stages",
    );
}

#[test]
fn a_stage_tier_is_at_least_its_min_tier() {
    validate(&stage("a", "work", "tier = \"fast\""), &[]).expect("a tier alone");
    validate(
        &stage("a", "work", "tier = \"strong\"\nmin_tier = \"balanced\""),
        &[],
    )
    .expect("a tier above the floor");
    assert_refused(
        &stage("a", "work", "tier = \"fast\"\nmin_tier = \"balanced\""),
        &[],
        "tier fast is below min_tier balanced",
    );
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
            // A swarm worker gets its task and the shared interfaces only.
            assert_eq!(
                names.iter().any(|n| n == "task.brief"),
                stage.roles_from.is_none(),
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
/// What every shipping prompt says of the pull request body it writes.
const PR_BODY_RULE: &str =
    "names no model or tool: no `Co-Authored-By` or \"Generated with\" line.";

#[test]
fn every_pipeline_that_ships_forbids_attribution_in_the_pr_body() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../pipelines");
    let catalog = Catalog::builtin();
    let mut shipping = Vec::new();
    for entry in std::fs::read_dir(&root).expect("the pipelines directory") {
        let dir = entry.expect("a directory entry").path();
        if !dir.join("pipeline.toml").is_file() {
            continue;
        }
        let name = dir.file_name().unwrap().to_str().unwrap().to_owned();
        let spec = catalog
            .get(&name)
            .expect("every pipeline directory is a built-in");
        let writers = spec.file.stages.iter().filter(|stage| {
            stage.kind == StageKind::Ship
                || spec.ship_record_stage().is_some_and(|r| r.id == stage.id)
        });
        for stage in writers {
            let prompt = std::fs::read_to_string(dir.join(&stage.prompt)).expect("its prompt");
            let prompt = prompt.split_whitespace().collect::<Vec<_>>().join(" ");
            assert!(
                prompt.contains(PR_BODY_RULE),
                "{name}/{}: the PR-body rule is missing",
                stage.prompt
            );
            shipping.push(name.clone());
        }
    }
    shipping.sort();
    assert_eq!(
        shipping,
        ["adaptive", "direct", "performance", "refactor", "swarm"]
    );
}

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
        ["adaptive", "direct", "swarm"],
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
        "full test suite once, and follow your contract's `wait_for` rule",
        "With no reviewer asked for, that run is the whole of this step",
        "the exact base and head commits (`{{base_commit}}` and `git rev-parse HEAD`",
        "never below one your contract names: medium for ordinary risk, high where the change's risk is medium or more",
        "never because a tree that passed was committed",
        "for any commit after a pass: Archon delivers only commits a passing reviewer read",
        "Delegate the missing passes as your contract defines, each a NEW agent at the new head",
        "after a fail one reads the whole change (`base` the change's start)",
        "any others only what changed since the head they passed",
        "Pass on your contract's bounds for reviewers",
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
        !build.contains("early-fix"),
        "reviewers still reading finish first"
    );
    assert!(!build.contains("edit nothing until all are back"));
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
    assert!(nudge.contains("at medium effort (the default); high for hard multi-file work"));
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
    assert!(
        prompt.contains("re-review of only what changed"),
        "{prompt}"
    );
    let integrate = &catalog.get("swarm").unwrap().prompts["integrate.md"];
    let integrate = integrate.split_whitespace().collect::<Vec<_>>().join(" ");
    assert!(
        integrate
            .contains("one new reviewer reads the whole change again, any others only the fix"),
        "{integrate}"
    );
}

#[test]
fn implementation_tests_of_blocking_fail_in_seconds() {
    let catalog = Catalog::builtin();
    for (pipeline, prompt) in [
        ("direct", "implement.md"),
        ("adaptive", "build.md"),
        ("refactor", "refactor.md"),
        ("performance", "optimize.md"),
    ] {
        let spec = catalog.get(pipeline).unwrap();
        let text = spec.prompts[prompt]
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        assert!(
            text.contains("A blocking or deadlock test must fail in seconds, never hang."),
            "{pipeline}"
        );
        assert!(
            !text.contains("daemon threads"),
            "{pipeline}: not every language has them"
        );
    }
}

#[test]
fn built_in_reviewers_apply_the_contracts_command_bounds() {
    let catalog = Catalog::builtin();
    for (pipeline, prompt) in [
        ("refactor", "review.md"),
        ("performance", "review.md"),
        ("design-doc", "critique.md"),
    ] {
        let spec = catalog.get(pipeline).unwrap();
        let stage = spec
            .file
            .stages
            .iter()
            .find(|stage| stage.prompt == prompt)
            .unwrap();
        assert_eq!(stage.kind, archon_core::catalog::StageKind::Review);
        let text = spec.prompts[prompt]
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        assert!(
            text.contains("your contract's command and reporting bounds"),
            "{pipeline}"
        );
    }
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

/// Every built-in prompt, the shared proposal and report included, diffs and
/// logs from the fork commit: a stale local base would add merged commits.
#[test]
fn every_builtin_prompt_diffs_from_the_fork_commit_not_the_local_base() {
    let fork = support::FORK_COMMIT;
    let catalog = Catalog::builtin();
    let knobs = Autonomy {
        proposal: ProposalMode::Lavish,
        report: ReportMode::WithPr,
        ..Autonomy::default()
    };
    let (mut rendered_prompts, mut diffs) = (Vec::new(), 0);
    for builtin in BUILTINS {
        let spec = catalog.get(builtin.name).expect("built-in loads");
        let spec = match autonomy::ships(spec) {
            true => autonomy::augment(spec, &knobs).expect("knobs apply"),
            false => spec.clone(),
        };
        for (name, text) in &spec.prompts {
            assert!(
                !text.contains("{{base_branch}}..."),
                "{}/{name}",
                builtin.name
            );
            assert!(
                !text.contains("{{base_branch}}..HEAD"),
                "{}/{name}",
                builtin.name
            );
            let mut vars: BTreeMap<String, String> = template::placeholders(text)
                .expect("parses")
                .into_iter()
                .map(|placeholder| (placeholder.clone(), format!("<{placeholder}>")))
                .collect();
            vars.insert("base_branch".into(), "next".into());
            vars.insert("base_commit".into(), fork.into());
            let rendered = template::render(text, &vars).expect("renders");
            for stale in ["next..", "merge-base next", "diff from `next`"] {
                assert!(
                    !rendered.contains(stale),
                    "{}/{name}: {stale}",
                    builtin.name
                );
            }
            diffs += rendered
                .matches(&format!("`git diff {fork}...HEAD`"))
                .count();
            rendered_prompts.push(name.clone());
        }
    }
    assert!(
        diffs >= 10,
        "the built-ins diff from the fork, {diffs} times"
    );
    for shared in [autonomy::PROPOSE_PROMPT, autonomy::REPORT_PROMPT] {
        assert!(
            rendered_prompts.iter().any(|name| name == shared),
            "{shared}"
        );
    }
}

/// A brief's own lenses win over the defaults: the scope lead numbers them
/// and each auditor applies its row.
#[test]
fn code_audit_auditors_take_their_lens_from_the_brief_before_the_defaults() {
    let catalog = Catalog::builtin();
    let spec = catalog.get("code-audit").expect("built-in");
    let flat = |name: &str| {
        spec.prompts[name]
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
    };
    let scope = flat("scope.md");
    assert!(scope.contains("Write `.archon/code-audit/lenses.md`: a table numbered from 1"));
    assert!(scope.contains("Lenses the brief names win"), "{scope}");
    assert!(scope.contains("Fill only the rows the brief leaves open"));
    let audit = flat("audit.md");
    assert!(
        audit.contains("Your lens is row {{fan.index}} of `lenses.md`"),
        "{audit}"
    );
    assert!(audit.contains("A lens the brief assigns wins over the defaults below"));
    assert!(
        !audit.contains("Apply only lens {{fan.index}}"),
        "no fixed lens"
    );
}
