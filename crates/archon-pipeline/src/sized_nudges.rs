//! The built-in stage nudges that name efforts, as the effort rubric words
//! them: served by archond and `archon pipelines list` while `[models]
//! effort_sizing` is on, so no catalog contradicts the rubric the
//! orchestrator and its leads read. Off, both serve it as 1.6.3 did.
use archon_core::catalog::{CatalogListing, PipelineSource};

/// (pipeline, stage, nudge) for the built-ins whose written nudge contradicts
/// the rubric; each keeps its tier and every other piece of guidance.
const SIZED_STAGE_NUDGES: [(&str, &str, &str); 3] = [
    (
        "direct",
        "implement",
        "The balanced tier at medium effort for a routine change; high for non-trivial \
         design, a subtle change or a risky diff; low for lookups, docs, config, formatting \
         and other mechanical edits. The agent escalates a \
         change that turns out risky: concurrency, locking, security or a migration; re-queue \
         that on adaptive.",
    ),
    (
        "adaptive",
        "build",
        "The strong tier at medium effort (the default); high for non-trivial design, hard \
         multi-file work or a risky diff; x_high for hard reasoning, such as root-causing a \
         regression, concurrency or security analysis, or a fix that failed twice: that the \
         operator prefers strong models is not one. The lead chooses its reviewer at runtime \
         within model policy. Its workers run on the balanced tier at medium, at most 2 at once \
         ([pipeline.workers]), not on the lead's model: give `build.workers` only when the parts \
         need another model or effort.",
    ),
    (
        "code-audit",
        "audit",
        "Strong tier, high effort; x_high for a security or concurrency lens. Independent \
         auditors, each with its own lens; another provider at the same tier adds independence \
         where the allowlist permits.",
    ),
];

/// `listing` with the built-in stage nudges worded by the rubric. A
/// pipeline a user or repository defines under a built-in's name is theirs.
pub fn sized_nudges(listing: &mut CatalogListing) {
    let builtins = listing
        .pipelines
        .iter_mut()
        .filter(|p| p.source == PipelineSource::Builtin);
    for pipeline in builtins {
        for (name, stage_id, sized) in SIZED_STAGE_NUDGES {
            if pipeline.name != name {
                continue;
            }
            let stage = pipeline.stages.iter_mut().find(|s| s.id == stage_id);
            let stage = stage.expect("a built-in pipeline keeps its stages");
            assert!(
                stage.nudge.is_some(),
                "{name}.{stage_id} is written with a nudge"
            );
            stage.nudge = Some(sized.to_owned());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use archon_core::model::EFFORT_RUBRIC;

    fn nudge<'a>(listing: &'a CatalogListing, name: &str, stage: &str) -> &'a str {
        let pipeline = listing.pipelines.iter().find(|p| p.name == name).unwrap();
        let stage = pipeline.stages.iter().find(|s| s.id == stage).unwrap();
        stage.nudge.as_deref().unwrap()
    }

    /// Off, the built-in nudges read as written, and those contradict the
    /// rubric: low only for a trivial edit, x_high only for a named problem.
    #[test]
    fn the_written_nudges_are_the_ones_the_rubric_replaces() {
        let written = crate::Catalog::builtin().listing();
        assert!(nudge(&written, "direct", "implement").contains("low only for docs"));
        let build = nudge(&written, "adaptive", "build");
        assert!(
            build.contains("x_high only for a reasoning problem"),
            "{build}"
        );
        for (name, stage, sized) in SIZED_STAGE_NUDGES {
            assert_ne!(
                nudge(&written, name, stage),
                sized,
                "{name}.{stage} changes"
            );
        }
    }

    /// On, no served stage nudge limits an effort the rubric grants, each
    /// names what the rubric does, and every other piece of guidance stays.
    #[test]
    fn the_served_nudges_agree_with_the_rubric() {
        assert!(EFFORT_RUBRIC.contains("`low` for lookups, listing, formatting, mechanical edits"));
        assert!(
            EFFORT_RUBRIC
                .contains("`x_high` for hard reasoning, such as root-causing a regression")
        );
        let mut sized = crate::Catalog::builtin().listing();
        sized_nudges(&mut sized);
        for pipeline in &sized.pipelines {
            for stage in &pipeline.stages {
                let Some(nudge) = stage.nudge.as_deref() else {
                    continue;
                };
                for limit in [
                    "low only for",
                    "x_high only for",
                    "high only for",
                    "never x_high",
                ] {
                    assert!(
                        !nudge.contains(limit),
                        "{}.{}: {nudge}",
                        pipeline.name,
                        stage.id
                    );
                }
            }
        }
        let implement = nudge(&sized, "direct", "implement");
        assert!(
            EFFORT_RUBRIC.contains("`medium` for routine changes")
                && EFFORT_RUBRIC.contains("`high` for non-trivial design, risky diffs")
        );
        assert!(
            implement.contains("medium effort for a routine change; high for non-trivial design"),
            "high by reasoning and risk, as the rubric: {implement}"
        );
        assert!(!implement.contains("larger"), "never by size alone");
        assert!(
            implement
                .contains("low for lookups, docs, config, formatting and other mechanical edits")
        );
        assert!(
            implement.contains("re-queue that on adaptive"),
            "the escalation stays"
        );
        let build = nudge(&sized, "adaptive", "build");
        assert!(build.contains("x_high for hard reasoning, such as root-causing a regression"));
        assert!(
            build.contains("`build.workers`"),
            "the worker guidance stays"
        );
        let audit = nudge(&sized, "code-audit", "audit");
        assert!(
            audit.contains("x_high for a security or concurrency lens"),
            "{audit}"
        );
    }

    /// A pipeline a repository defines under a built-in's name is served as
    /// written.
    #[test]
    fn a_pipeline_of_the_user_s_own_is_served_as_written() {
        let mut listing = crate::Catalog::builtin().listing();
        for pipeline in &mut listing.pipelines {
            pipeline.source = PipelineSource::Repo;
        }
        let written = listing.clone();
        sized_nudges(&mut listing);
        assert_eq!(listing, written);
    }
}
