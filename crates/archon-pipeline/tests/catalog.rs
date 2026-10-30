//! Discovery and precedence (ARCHITECTURE §8.1) against temp user and
//! repository directories: built-in < user < repository, whole-directory
//! replacement, and a broken override making its name unavailable.

mod sim;
mod support;

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use archon_core::catalog::{Gate, PipelineSource, StageKind, Tier, WorkspaceMode};
use archon_core::error::ArchonError;
use archon_pipeline::catalog::load_dir;
use archon_pipeline::{Catalog, CatalogSources};
use sim::{Agent, Sim, spawned};

const EXAMPLE: &str = "bugfix-tdd";

/// A temp root with `user/` (the user pipelines dir) and `repo/` (a
/// repository whose pipelines live in `.archon/pipelines`).
struct Dirs {
    _root: tempfile::TempDir,
    user: PathBuf,
    repo: PathBuf,
}

impl Dirs {
    fn new() -> Self {
        let root = tempfile::tempdir().expect("tempdir");
        let user = root.path().join("config/archon/pipelines");
        let repo = root.path().join("repo");
        fs::create_dir_all(&user).expect("user dir");
        fs::create_dir_all(repo.join(".archon/pipelines")).expect("repo dir");
        Self {
            _root: root,
            user,
            repo,
        }
    }

    fn repo_pipelines(&self) -> PathBuf {
        self.repo.join(".archon/pipelines")
    }

    fn sources(&self) -> CatalogSources {
        CatalogSources {
            user_dir: Some(self.user.clone()),
            repo_root: Some(self.repo.clone()),
            disabled: Vec::new(),
        }
    }

    fn discover(&self) -> Catalog {
        Catalog::discover(&self.sources()).expect("discover")
    }
}

fn examples_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/examples/pipelines")
}

fn example_dir() -> PathBuf {
    examples_dir().join(EXAMPLE)
}

/// The built-ins under every documented example, as a user who copied them all.
fn examples() -> Catalog {
    let sources = CatalogSources {
        user_dir: Some(examples_dir()),
        repo_root: None,
        disabled: Vec::new(),
    };
    let catalog = Catalog::discover(&sources).expect("discover");
    assert!(catalog.problems().is_empty(), "{:#?}", catalog.problems());
    catalog
}

/// Copy the documented example into `parent/<EXAMPLE>`, as a user would.
fn drop_example(parent: &Path) -> PathBuf {
    let target = parent.join(EXAMPLE);
    fs::create_dir_all(&target).expect("create");
    for entry in fs::read_dir(example_dir()).expect("example exists") {
        let entry = entry.expect("entry");
        fs::copy(entry.path(), target.join(entry.file_name())).expect("copy");
    }
    target
}

/// A one-stage `ask`-shaped pipeline named `name` whose description is
/// `marker`, so tests can tell which source won.
fn write_pipeline(parent: &Path, name: &str, marker: &str) -> PathBuf {
    let dir = parent.join(name);
    fs::create_dir_all(&dir).expect("create");
    let toml = format!(
        "[pipeline]\nname = \"{name}\"\ndescription = \"{marker}\"\nversion = 1\n\
         workspace = \"repo-readonly\"\nrequires_repo = false\n\n\
         [[stage]]\nid = \"answer\"\nkind = \"work\"\nprompt = \"answer.md\"\n"
    );
    fs::write(dir.join("pipeline.toml"), toml).expect("write toml");
    fs::write(
        dir.join("answer.md"),
        "Answer {{task.title}} in {{workspace}}.",
    )
    .expect("write prompt");
    dir
}

fn description(catalog: &Catalog, name: &str) -> String {
    catalog
        .get(name)
        .expect("available")
        .file
        .pipeline
        .description
        .clone()
}

#[test]
fn builtins_load_without_problems() {
    let catalog = Catalog::builtin();
    assert!(catalog.problems().is_empty(), "{:#?}", catalog.problems());
    let names: Vec<&str> = catalog.names().collect();
    assert_eq!(
        names.len(),
        archon_pipeline::builtin::BUILTINS.len(),
        "every built-in is registered exactly once"
    );
    assert_eq!(
        names,
        [
            "adaptive",
            "agent-testing",
            "ask",
            "code-audit",
            "design-doc",
            "direct",
            "e2e-testing",
            "errand",
            "performance",
            "refactor",
            "swarm",
        ]
    );
    let missing_dirs = CatalogSources {
        user_dir: Some(PathBuf::from("/nonexistent/archon/pipelines")),
        repo_root: Some(PathBuf::from("/nonexistent/repo")),
        disabled: Vec::new(),
    };
    let discovered = Catalog::discover(&missing_dirs).expect("missing dirs are empty sources");
    assert_eq!(discovered.listing(), catalog.listing());
}

/// A stage that builds and tests the product gets room for a slow stack, and
/// `direct`'s single agent as long as `adaptive`'s lead.
#[test]
fn built_in_stages_that_build_get_room_to_finish() {
    let catalog = Catalog::builtin();
    let minutes = |pipeline: &str, stage: &str| {
        let spec = catalog.get(pipeline).expect("a built-in loads");
        let stage = spec
            .stage(stage)
            .unwrap_or_else(|| panic!("{pipeline}/{stage}"));
        stage.timeout_minutes
    };
    assert_eq!(minutes("direct", "implement"), minutes("adaptive", "build"));
    assert_eq!(minutes("direct", "implement"), 180);
    assert_eq!(minutes("e2e-testing", "setup"), 120);
    assert_eq!(minutes("swarm", "plan"), 120);
    let builds = [
        ("agent-testing", "plan"),
        ("code-audit", "scope"),
        ("refactor", "review"),
        ("swarm", "build"),
        ("performance", "review"),
        ("refactor", "verify"),
    ];
    for (pipeline, stage) in builds {
        let got = minutes(pipeline, stage);
        assert!(got >= 90, "{pipeline}/{stage} has {got} minutes");
    }
}

/// "Confirm nothing is left running" invited a pattern kill that took the
/// operator's daemons down: prompts confirm only what their scripts recorded.
#[test]
fn built_in_prompts_stop_only_what_their_scripts_recorded() {
    let catalog = Catalog::builtin();
    let spec = catalog.get("e2e-testing").expect("e2e-testing loads");
    let setup = spec.prompts["setup.md"].replace("\n   ", " ");
    for said in [
        "by the PIDs, process groups or Compose project `start.sh` recorded",
        "never stops anything by name or pattern (`pkill`, `killall`, `pgrep -f`)",
        "confirm that none of the processes or containers `start.sh` recorded is left running",
    ] {
        assert!(setup.contains(said), "missing {said:?} in {setup}");
    }
    for name in catalog.names() {
        let spec = catalog.get(name).expect("a listed built-in loads");
        for (file, prompt) in &spec.prompts {
            let prompt = prompt.replace("\n   ", " ");
            assert!(!prompt.contains("nothing is left running"), "{name}/{file}");
            assert!(!prompt.contains("nothing is running"), "{name}/{file}");
        }
    }
}

#[test]
fn errand_is_a_scratch_repository_optional_report_pipeline() {
    let catalog = Catalog::builtin();
    let spec = catalog.get("errand").expect("errand available");
    let summary = spec.summary();
    assert_eq!(summary.source, PipelineSource::Builtin);
    assert_eq!(summary.workspace, WorkspaceMode::Scratch);
    assert!(!summary.requires_repo);
    assert_eq!(summary.nudges.tier, Some(Tier::Balanced));
    assert_eq!(summary.stages.len(), 1);
    let stage = &summary.stages[0];
    assert_eq!(stage.id, "errand");
    assert_eq!(stage.kind, StageKind::Report);
    assert_eq!(stage.gate, Gate::None);
    assert_eq!(stage.fan_out, 1);
    assert_eq!(stage.timeout_minutes, 45);
    assert!(stage.loops_to.is_none());
}

#[test]
fn a_dropped_in_user_pipeline_is_discovered_and_runs() {
    let dirs = Dirs::new();
    let path = drop_example(&dirs.user);
    let catalog = dirs.discover();
    assert!(catalog.problems().is_empty(), "{:#?}", catalog.problems());
    let spec = catalog.get(EXAMPLE).expect("user pipeline available");
    assert_eq!(spec.source, PipelineSource::User);
    assert_eq!(spec.dir.as_deref(), Some(path.as_path()));
    let summary = catalog
        .listing()
        .pipelines
        .into_iter()
        .find(|p| p.name == EXAMPLE)
        .expect("listed");
    assert_eq!(
        summary.stage("review").expect("review").loops_to.as_deref(),
        Some("fix")
    );

    // The dropped-in pipeline drives like any built-in.
    let spec = Arc::new(spec.clone());
    let plan = support::plan_for(&spec, &["claude-code/opus", "codex/astra"]);
    let mut sim = Sim::new(spec, plan);
    sim.start();
    sim.settle("reproduce", 0, Agent::Done("test_empty_window fails"));
    let reviews = sim.settle("fix", 0, Agent::Done("fixed window end"));
    assert_eq!(spawned(&reviews).len(), 2);
    sim.settle("review", 0, Agent::Pass);
    let back = sim.settle("review", 1, Agent::Fail("regression in resize"));
    assert_eq!(spawned(&back), [("fix".to_owned(), 2, 0)]);
}

#[test]
fn precedence_is_builtin_then_user_then_repo() {
    let dirs = Dirs::new();
    write_pipeline(&dirs.user, "direct", "user direct");
    write_pipeline(&dirs.user, "ask", "user ask");
    write_pipeline(&dirs.repo_pipelines(), "ask", "repo ask");
    let catalog = dirs.discover();
    assert_eq!(
        description(&catalog, "direct"),
        "user direct",
        "user replaces built-in"
    );
    assert_eq!(
        catalog.get("direct").expect("user").source,
        PipelineSource::User
    );
    assert_eq!(
        description(&catalog, "ask"),
        "repo ask",
        "repo replaces user"
    );
    assert_eq!(
        catalog.get("ask").expect("repo").source,
        PipelineSource::Repo
    );
    assert_eq!(
        catalog.get("refactor").expect("built-in").source,
        PipelineSource::Builtin
    );

    let outside_repo = CatalogSources {
        repo_root: None,
        ..dirs.sources()
    };
    let catalog = Catalog::discover(&outside_repo).expect("discover");
    assert_eq!(
        description(&catalog, "ask"),
        "user ask",
        "repo pipelines only apply inside the repo"
    );
}

#[test]
fn an_override_replaces_the_whole_directory() {
    let dirs = Dirs::new();
    let dir = write_pipeline(&dirs.user, "direct", "one stage only");
    let catalog = dirs.discover();
    let spec = catalog.get("direct").expect("override");
    assert_eq!(
        spec.file.stages.len(),
        1,
        "no stages inherited from the built-in"
    );
    assert_eq!(spec.prompts.keys().collect::<Vec<_>>(), ["answer.md"]);
    assert_eq!(spec.dir.as_deref(), Some(dir.as_path()));
}

#[test]
fn a_broken_override_makes_the_name_unavailable_instead_of_falling_back() {
    let dirs = Dirs::new();
    let dir = write_pipeline(&dirs.user, "direct", "broken");
    fs::remove_file(dir.join("answer.md")).expect("remove prompt");
    let catalog = dirs.discover();
    let error = catalog.get("direct").expect_err("unavailable");
    assert!(matches!(error, ArchonError::Pipeline(_)), "{error}");
    assert!(
        error.to_string().contains("broken") && error.to_string().contains("answer.md"),
        "{error}"
    );
    let listing = catalog.listing();
    assert!(
        listing.pipelines.iter().all(|p| p.name != "direct"),
        "no built-in fallback"
    );
    let problem = &listing.problems[0];
    assert_eq!(
        (problem.name.as_str(), problem.source),
        ("direct", PipelineSource::User)
    );
    assert_eq!(problem.path.as_deref(), Some(dir.as_path()));
}

#[test]
fn a_valid_repo_override_supersedes_a_broken_user_one() {
    let dirs = Dirs::new();
    let user = dirs.user.join("ask");
    fs::create_dir_all(&user).expect("create");
    fs::write(user.join("pipeline.toml"), "this is not toml [").expect("write");
    write_pipeline(&dirs.repo_pipelines(), "ask", "repo ask");
    let catalog = dirs.discover();
    assert_eq!(description(&catalog, "ask"), "repo ask");
    assert_eq!(
        catalog.problems().len(),
        1,
        "the broken user file is still reported"
    );
    assert_eq!(catalog.problems()[0].source, PipelineSource::User);
}

#[test]
fn problems_name_the_path_and_reason() {
    let dirs = Dirs::new();
    write_pipeline(&dirs.user, "renamed", "x");
    fs::rename(dirs.user.join("renamed"), dirs.user.join("other-name")).expect("rename");
    let bad_toml = dirs.user.join("typo");
    fs::create_dir_all(&bad_toml).expect("create");
    fs::write(
        bad_toml.join("pipeline.toml"),
        "[pipeline]\nname = \"typo\"\nfan_out = 2\n",
    )
    .expect("write");
    let catalog = dirs.discover();
    let problems = catalog.problems();
    assert_eq!(problems.len(), 2, "{problems:#?}");
    let renamed = problems
        .iter()
        .find(|p| p.name == "other-name")
        .expect("name mismatch reported");
    assert!(
        renamed.error.contains("must equal its directory name"),
        "{}",
        renamed.error
    );
    let typo = problems
        .iter()
        .find(|p| p.name == "typo")
        .expect("parse error reported");
    assert!(typo.error.contains("pipeline.toml"), "{}", typo.error);
    assert!(typo.path.as_ref().is_some_and(|p| p.ends_with("typo")));
    assert!(matches!(
        catalog.get("nothing-here"),
        Err(ArchonError::NotFound(_))
    ));
}

/// A name 1.1.0 retired says what replaces it, unless the operator has a
/// pipeline of that name of their own.
#[test]
fn a_retired_built_in_names_its_replacement_unless_one_of_that_name_exists() {
    let dirs = Dirs::new();
    let catalog = dirs.discover();
    for (name, replacement) in archon_pipeline::builtin::RETIRED {
        let error = catalog.get(name).expect_err("retired").to_string();
        assert!(error.contains("Archon 1.1.0 removed"), "{error}");
        assert!(error.ends_with(replacement), "{error}");
    }
    write_pipeline(&dirs.user, "bugfix", "the operator's own");
    let catalog = dirs.discover();
    assert_eq!(description(&catalog, "bugfix"), "the operator's own");
}

#[test]
fn non_pipeline_entries_are_ignored() {
    let dirs = Dirs::new();
    fs::create_dir_all(dirs.user.join("notes")).expect("dir without pipeline.toml");
    fs::write(dirs.user.join("README.md"), "loose file").expect("file");
    write_pipeline(&dirs.user, ".hidden", "hidden");
    let catalog = dirs.discover();
    assert!(catalog.problems().is_empty(), "{:#?}", catalog.problems());
    assert_eq!(
        catalog.names().count(),
        archon_pipeline::builtin::BUILTINS.len(),
        "only the built-ins"
    );
}

#[test]
fn disabled_pipelines_are_hidden_with_a_clear_error() {
    let dirs = Dirs::new();
    let sources = CatalogSources {
        disabled: vec!["direct".into()],
        ..dirs.sources()
    };
    let catalog = Catalog::discover(&sources).expect("discover");
    assert!(
        catalog
            .listing()
            .pipelines
            .iter()
            .all(|p| p.name != "direct")
    );
    let error = catalog.get("direct").expect_err("disabled").to_string();
    assert!(error.contains("disabled"), "{error}");
}

#[test]
fn load_dir_refuses_prompts_outside_the_pipeline_directory() {
    let dirs = Dirs::new();
    let dir = write_pipeline(&dirs.user, "escape", "x");
    fs::write(dirs.user.join("secret.md"), "top secret").expect("write");
    let toml = fs::read_to_string(dir.join("pipeline.toml")).expect("read");
    fs::write(
        dir.join("pipeline.toml"),
        toml.replace("answer.md", "../secret.md"),
    )
    .expect("write");
    let error = load_dir(&dir, PipelineSource::User)
        .expect_err("refused")
        .to_string();
    assert!(error.contains("plain file name"), "{error}");
}

#[test]
fn load_dir_refuses_oversized_files() {
    let dirs = Dirs::new();
    let dir = write_pipeline(&dirs.user, "huge", "x");
    let big = "x".repeat(archon_pipeline::catalog::MAX_FILE_BYTES as usize + 1);
    fs::write(dir.join("answer.md"), big).expect("write");
    let error = load_dir(&dir, PipelineSource::User)
        .expect_err("refused")
        .to_string();
    assert!(error.contains("larger than"), "{error}");
}

#[cfg(unix)]
#[test]
fn load_dir_refuses_symlinks_out_of_the_pipeline_directory() {
    use std::os::unix::fs::symlink;
    let dirs = Dirs::new();
    let secret = dirs.repo.join("id_token");
    fs::write(&secret, "token = \"hunter2\"\n").expect("write");

    let prompt = write_pipeline(&dirs.user, "prompt-link", "x");
    fs::remove_file(prompt.join("answer.md")).expect("remove");
    symlink(&secret, prompt.join("answer.md")).expect("symlink");
    let error = load_dir(&prompt, PipelineSource::User)
        .expect_err("refused")
        .to_string();
    assert!(error.contains("outside its pipeline directory"), "{error}");

    let manifest = write_pipeline(&dirs.user, "manifest-link", "x");
    fs::remove_file(manifest.join("pipeline.toml")).expect("remove");
    symlink(&secret, manifest.join("pipeline.toml")).expect("symlink");
    let catalog = dirs.discover();
    let problem = &catalog.problems()[0];
    assert_eq!(problem.name, "manifest-link");
    assert!(problem.error.contains("outside"), "{}", problem.error);
    assert!(!problem.error.contains("hunter2"), "{}", problem.error);
}

/// `name` must be unavailable with one user problem naming `needle`.
fn assert_broken_user_override(catalog: &Catalog, name: &str, needle: &str) {
    let error = catalog.get(name).expect_err("no built-in fallback");
    assert!(error.to_string().contains("is broken"), "{error}");
    let problem = &catalog.problems()[0];
    assert_eq!(catalog.problems().len(), 1, "{:#?}", catalog.problems());
    assert_eq!(
        (problem.name.as_str(), problem.source),
        (name, PipelineSource::User)
    );
    assert!(problem.error.contains(needle), "{}", problem.error);
}

#[cfg(unix)]
#[test]
fn a_dangling_manifest_symlink_is_reported_instead_of_falling_back() {
    use std::os::unix::fs::symlink;
    let dirs = Dirs::new();
    let dir = write_pipeline(&dirs.user, "adaptive", "override");
    fs::remove_file(dir.join("pipeline.toml")).expect("remove");
    symlink(dirs.repo.join("moved.toml"), dir.join("pipeline.toml")).expect("symlink");
    assert_broken_user_override(&dirs.discover(), "adaptive", "pipeline.toml");
}

#[test]
fn a_manifest_that_is_not_a_file_is_reported() {
    let dirs = Dirs::new();
    let dir = dirs.user.join("direct");
    fs::create_dir_all(dir.join("pipeline.toml")).expect("directory named pipeline.toml");
    assert_broken_user_override(&dirs.discover(), "direct", "pipeline.toml");
}

#[cfg(unix)]
#[test]
fn an_unsearchable_pipeline_directory_is_reported() {
    use std::os::unix::fs::PermissionsExt;
    let dirs = Dirs::new();
    let dir = write_pipeline(&dirs.user, "direct", "override");
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o000)).expect("chmod");
    // Root ignores permissions, so there is nothing to observe.
    let enforced = fs::metadata(dir.join("pipeline.toml")).is_err();
    let catalog = dirs.discover();
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).expect("chmod back");
    if enforced {
        assert_broken_user_override(&catalog, "direct", "pipeline.toml");
    }
}

#[cfg(unix)]
#[test]
fn symlinks_within_a_pipeline_directory_are_followed() {
    use std::os::unix::fs::symlink;
    let dirs = Dirs::new();
    let real = write_pipeline(&dirs.repo, "linked", "real");
    fs::rename(real.join("answer.md"), real.join("shared.md")).expect("rename");
    symlink("shared.md", real.join("answer.md")).expect("symlink");
    // A dotfiles-style link to the whole directory is fine too.
    symlink(&real, dirs.user.join("linked")).expect("symlink dir");
    let spec = load_dir(&dirs.user.join("linked"), PipelineSource::User).expect("loads");
    assert_eq!(
        spec.prompts["answer.md"],
        "Answer {{task.title}} in {{workspace}}."
    );
}

#[test]
fn load_dir_refuses_too_many_stages_before_reading_prompts() {
    let dirs = Dirs::new();
    let dir = dirs.user.join("long");
    fs::create_dir_all(&dir).expect("create");
    let mut toml = String::from(
        "[pipeline]\nname = \"long\"\ndescription = \"d\"\nversion = 1\n\
         workspace = \"scratch\"\nrequires_repo = false\n",
    );
    let count = archon_pipeline::spec::MAX_STAGES + 1;
    for i in 0..count {
        toml.push_str(&format!(
            "[[stage]]\nid = \"s{i}\"\nkind = \"work\"\nprompt = \"p{i}.md\"\n"
        ));
    }
    fs::write(dir.join("pipeline.toml"), toml).expect("write");
    let error = load_dir(&dir, PipelineSource::User)
        .expect_err("refused")
        .to_string();
    assert!(
        error.contains(&format!("{count} stages is too many")),
        "{error}"
    );
}

/// Rewrite `dir`'s one-stage manifest (see `write_pipeline`) with `minutes`.
fn set_timeout(dir: &Path, minutes: u32) {
    let manifest = dir.join("pipeline.toml");
    let toml = fs::read_to_string(&manifest).expect("read toml");
    fs::write(&manifest, format!("{toml}timeout_minutes = {minutes}\n")).expect("write toml");
}

#[test]
fn load_dir_keeps_a_week_long_timeout_and_refuses_one_past_the_maximum() {
    let dirs = Dirs::new();
    let week = 7 * 24 * 60;
    let dir = write_pipeline(&dirs.user, "e2e-testing", "long e2e");
    set_timeout(&dir, week);
    let spec = load_dir(&dir, PipelineSource::User).expect("a week loads");
    assert_eq!(spec.file.stages[0].timeout_minutes, week, "never clamped");

    let over = archon_pipeline::spec::MAX_TIMEOUT_MINUTES + 1;
    let dir = write_pipeline(&dirs.user, "too-long", "x");
    set_timeout(&dir, over);
    let error = load_dir(&dir, PipelineSource::User)
        .expect_err("refused")
        .to_string();
    assert!(
        error.contains("stage \"answer\"") && error.contains(&format!("got {over}")),
        "{error}"
    );
}

#[test]
fn the_team_pr_example_builds_as_a_team_then_loops_e2e_and_review_back_to_build() {
    let catalog = examples();
    let spec = catalog.get("team-pr").expect("team-pr loads");
    assert_eq!(spec.source, PipelineSource::User);
    let summary = spec.summary();
    let stages: Vec<(&str, StageKind)> = summary
        .stages
        .iter()
        .map(|s| (s.id.as_str(), s.kind))
        .collect();
    assert_eq!(
        stages,
        [
            ("plan", StageKind::Work),
            ("build", StageKind::Team),
            ("e2e", StageKind::Test),
            ("review", StageKind::Review)
        ]
    );
    let build = summary.stage("build").expect("build");
    assert_eq!(build.roles, ["backend", "frontend"]);
    for gated in ["e2e", "review"] {
        let stage = summary.stage(gated).expect("gated stage");
        assert_eq!(stage.gate, Gate::AllPass, "{gated}");
        assert_eq!(stage.fan_out, 1, "{gated}: one agent, no fan-out");
        assert_eq!(stage.loops_to.as_deref(), Some("build"), "{gated}");
        assert!(stage.max_loops >= 1 && stage.max_loops <= 3, "{gated}");
    }
    let notes = summary.nudges.notes.as_deref().expect("notes");
    assert!(notes.contains("backend and a frontend"), "{notes}");
    let listed = catalog.listing();
    assert!(listed.pipelines.iter().any(|p| p.name == "team-pr"));
}

/// `direct` is one agent and nothing else: archond ships what it records,
/// and the item's review level, not a stage, says whether anyone reviews it.
#[test]
fn direct_is_one_work_stage_that_archond_ships() {
    let catalog = Catalog::builtin();
    let spec = catalog.get("direct").expect("direct loads");
    let summary = spec.summary();
    let stages: Vec<(&str, StageKind)> = summary
        .stages
        .iter()
        .map(|s| (s.id.as_str(), s.kind))
        .collect();
    assert_eq!(stages, [("implement", StageKind::Work)]);
    assert_eq!(summary.workspace, WorkspaceMode::Worktree);
    assert_eq!(summary.nudges.tier, Some(Tier::Balanced));
    let record = spec.ship_record_stage().expect("archond ships it");
    assert_eq!(record.id, "implement");
    assert!(spec.holds_review(record), "its one agent holds the review");
    let duty = spec.review_duty(record).expect("the holder has a duty");
    assert_eq!(
        (duty.reviewers, duty.may_skip),
        (0, false),
        "none by default"
    );
}

#[test]
fn design_doc_researches_and_critiques_before_reporting_without_shipping() {
    let catalog = Catalog::builtin();
    let spec = catalog.get("design-doc").expect("design-doc loads");
    let summary = spec.summary();
    assert_eq!(summary.workspace, WorkspaceMode::Worktree);
    assert!(summary.requires_repo);
    assert_eq!(summary.nudges.tier, Some(Tier::Strong));
    assert!(summary.nudges.independent_reviewers);
    let stages: Vec<(&str, StageKind)> = summary
        .stages
        .iter()
        .map(|s| (s.id.as_str(), s.kind))
        .collect();
    assert_eq!(
        stages,
        [
            ("research", StageKind::Work),
            ("critique", StageKind::Review),
            ("report", StageKind::Report)
        ]
    );
    let critique = summary.stage("critique").expect("critique");
    assert_eq!(critique.fan_out, 1);
    assert_eq!(critique.gate, Gate::AllPass);
    assert_eq!(critique.loops_to.as_deref(), Some("research"));
    assert_eq!(critique.max_loops, 2);
    assert_eq!(summary.stage("report").expect("report").gate, Gate::None);
}
