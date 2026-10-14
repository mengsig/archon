//! Built-in pipelines, compiled in from the repository's `pipelines/`
//! directory (ARCHITECTURE §8.1). Adding a built-in means adding its files
//! here; user and repository pipelines need no code at all.

#[derive(Debug)]
pub struct BuiltinPipeline {
    pub name: &'static str,
    /// `(file name, contents)`, `pipeline.toml` first.
    pub files: &'static [(&'static str, &'static str)],
}

impl BuiltinPipeline {
    pub fn file(&self, name: &str) -> Option<&'static str> {
        self.files
            .iter()
            .find(|(n, _)| *n == name)
            .map(|(_, text)| *text)
    }
}

macro_rules! builtin {
    ($name:literal, [$($file:literal),+ $(,)?]) => {
        BuiltinPipeline {
            name: $name,
            files: &[
                ("pipeline.toml", include_str!(concat!("../../../pipelines/", $name, "/pipeline.toml"))),
                $(($file, include_str!(concat!("../../../pipelines/", $name, "/", $file))),)+
            ],
        }
    };
}

pub const BUILTINS: &[BuiltinPipeline] = &[
    builtin!("adaptive", ["build.md"]),
    builtin!("direct", ["implement.md"]),
    builtin!(
        "refactor",
        ["baseline.md", "refactor.md", "verify.md", "review.md"]
    ),
    builtin!(
        "performance",
        ["measure.md", "optimize.md", "verify.md", "review.md"]
    ),
    builtin!("agent-testing", ["plan.md", "exercise.md", "report.md"]),
    builtin!("code-audit", ["scope.md", "audit.md", "report.md"]),
    builtin!("e2e-testing", ["setup.md", "e2e.md", "report.md"]),
    builtin!("ask", ["answer.md"]),
    builtin!("errand", ["errand.md"]),
    builtin!("design-doc", ["research.md", "critique.md", "report.md"]),
];

/// The pipeline an enqueue that names none runs: it takes every review level.
pub const DEFAULT_PIPELINE: &str = "direct";

/// Built-ins 1.1.0 removed, each with what replaces it.
pub const RETIRED: &[(&str, &str)] = &[
    ("quick-pr", "direct with review none"),
    ("default-pr-review", "direct with review standard"),
    ("strict-pr-review", "direct with review strict"),
    (
        "team-pr",
        "adaptive, or docs/examples/pipelines/team-pr as a user pipeline",
    ),
    ("bugfix", "direct"),
    ("docs", "direct"),
    ("test-coverage", "direct"),
    ("dependency-upgrade", "direct"),
];

/// What replaces `name` when it is a built-in 1.1.0 removed.
pub fn retired(name: &str) -> Option<&'static str> {
    RETIRED
        .iter()
        .find(|(retired, _)| *retired == name)
        .map(|(_, replacement)| *replacement)
}
