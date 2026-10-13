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
    builtin!("adaptive", ["build.md", "ship.md"]),
    builtin!("quick-pr", ["implement.md", "ship.md"]),
    builtin!(
        "default-pr-review",
        ["implement.md", "review.md", "ship.md"]
    ),
    builtin!("bugfix", ["reproduce.md", "fix.md", "review.md", "ship.md"]),
    builtin!("docs", ["write.md", "verify.md", "ship.md"]),
    builtin!(
        "dependency-upgrade",
        ["survey.md", "upgrade.md", "verify.md", "ship.md"]
    ),
    builtin!(
        "refactor",
        [
            "baseline.md",
            "refactor.md",
            "verify.md",
            "review.md",
            "ship.md"
        ]
    ),
    builtin!(
        "strict-pr-review",
        ["implement.md", "test.md", "review.md", "ship.md"]
    ),
    builtin!(
        "team-pr",
        ["plan.md", "build.md", "e2e.md", "review.md", "ship.md"]
    ),
    builtin!(
        "performance",
        [
            "measure.md",
            "optimize.md",
            "verify.md",
            "review.md",
            "ship.md"
        ]
    ),
    builtin!(
        "test-coverage",
        ["analyze.md", "write.md", "verify.md", "ship.md"]
    ),
    builtin!("agent-testing", ["plan.md", "exercise.md", "report.md"]),
    builtin!("code-audit", ["scope.md", "audit.md", "report.md"]),
    builtin!("e2e-testing", ["setup.md", "e2e.md", "report.md"]),
    builtin!("ask", ["answer.md"]),
    builtin!("errand", ["errand.md"]),
    builtin!("design-doc", ["research.md", "critique.md", "report.md"]),
];
