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

fn example_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../docs/examples/pipelines")
        .join(EXAMPLE)
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
            "bugfix",
            "code-audit",
            "default-pr-review",
            "dependency-upgrade",
            "design-doc",
            "docs",
            "e2e-testing",
            "errand",
            "performance",
            "quick-pr",
            "refactor",
            "strict-pr-review",
            "team-pr",
            "test-coverage",
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
    write_pipeline(&dirs.user, "quick-pr", "user quick-pr");
    write_pipeline(&dirs.user, "ask", "user ask");
    write_pipeline(&dirs.repo_pipelines(), "ask", "repo ask");
    let catalog = dirs.discover();
    assert_eq!(
        description(&catalog, "quick-pr"),
        "user quick-pr",
        "user replaces built-in"
    );
    assert_eq!(
        catalog.get("quick-pr").expect("user").source,
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
        catalog.get("strict-pr-review").expect("built-in").source,
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
    let dir = write_pipeline(&dirs.user, "quick-pr", "one stage only");
    let catalog = dirs.discover();
    let spec = catalog.get("quick-pr").expect("override");
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
    let dir = write_pipeline(&dirs.user, "quick-pr", "broken");
    fs::remove_file(dir.join("answer.md")).expect("remove prompt");
    let catalog = dirs.discover();
    let error = catalog.get("quick-pr").expect_err("unavailable");
    assert!(matches!(error, ArchonError::Pipeline(_)), "{error}");
    assert!(
        error.to_string().contains("broken") && error.to_string().contains("answer.md"),
        "{error}"
    );
    let listing = catalog.listing();
    assert!(
        listing.pipelines.iter().all(|p| p.name != "quick-pr"),
        "no built-in fallback"
    );
    let problem = &listing.problems[0];
    assert_eq!(
        (problem.name.as_str(), problem.source),
        ("quick-pr", PipelineSource::User)
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
        disabled: vec!["quick-pr".into()],
        ..dirs.sources()
    };
    let catalog = Catalog::discover(&sources).expect("discover");
    assert!(
        catalog
            .listing()
            .pipelines
            .iter()
            .all(|p| p.name != "quick-pr")
    );
    let error = catalog.get("quick-pr").expect_err("disabled").to_string();
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
    let dir = write_pipeline(&dirs.user, "default-pr-review", "override");
    fs::remove_file(dir.join("pipeline.toml")).expect("remove");
    symlink(dirs.repo.join("moved.toml"), dir.join("pipeline.toml")).expect("symlink");
    assert_broken_user_override(&dirs.discover(), "default-pr-review", "pipeline.toml");
}

#[test]
fn a_manifest_that_is_not_a_file_is_reported() {
    let dirs = Dirs::new();
    let dir = dirs.user.join("quick-pr");
    fs::create_dir_all(dir.join("pipeline.toml")).expect("directory named pipeline.toml");
    assert_broken_user_override(&dirs.discover(), "quick-pr", "pipeline.toml");
}

#[cfg(unix)]
#[test]
fn an_unsearchable_pipeline_directory_is_reported() {
    use std::os::unix::fs::PermissionsExt;
    let dirs = Dirs::new();
    let dir = write_pipeline(&dirs.user, "quick-pr", "override");
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o000)).expect("chmod");
    // Root ignores permissions, so there is nothing to observe.
    let enforced = fs::metadata(dir.join("pipeline.toml")).is_err();
    let catalog = dirs.discover();
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).expect("chmod back");
    if enforced {
        assert_broken_user_override(&catalog, "quick-pr", "pipeline.toml");
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
fn team_pr_builds_as_a_team_then_loops_e2e_and_review_back_to_build() {
    let catalog = Catalog::builtin();
    let summary = catalog.get("team-pr").expect("team-pr loads").summary();
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
            ("review", StageKind::Review),
            ("ship", StageKind::Ship)
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

#[test]
fn bugfix_has_a_single_independent_review_and_two_fix_loops() {
    let catalog = Catalog::builtin();
    let summary = catalog.get("bugfix").expect("bugfix loads").summary();
    let stages: Vec<(&str, StageKind)> = summary
        .stages
        .iter()
        .map(|s| (s.id.as_str(), s.kind))
        .collect();
    assert_eq!(
        stages,
        [
            ("reproduce", StageKind::Work),
            ("fix", StageKind::Work),
            ("review", StageKind::Review),
            ("ship", StageKind::Ship)
        ]
    );
    let review = summary.stage("review").expect("review");
    assert_eq!(review.gate, Gate::AllPass);
    assert_eq!(review.fan_out, 1);
    assert_eq!(review.loops_to.as_deref(), Some("fix"));
    assert_eq!(review.max_loops, 2);
    assert_eq!(summary.nudges.tier, Some(Tier::Balanced));
    assert!(summary.nudges.independent_reviewers);
}

#[test]
fn design_doc_researches_and_critiques_before_reporting_without_shipping() {
    let catalog = Catalog::builtin();
    let spec = catalog.get("design-doc").expect("design-doc loads");
    let summary = spec.summary();
    assert_eq!(summary.workspace, WorkspaceMode::Worktree);
    assert!(summary.requires_repo);
    assert_eq!(summary.nudges.tier, Some(Tier::Strongest));
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
