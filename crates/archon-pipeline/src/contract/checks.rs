//! The repository's check tiers in every stage agent's contract (issue #82):
//! the fast check items run, for the packages their change affects, and the
//! full suite they leave to the integration branch.

use archon_core::repo::Checks;
use archon_core::repo::affected::{self, Affected};

/// What the contract says of the fast check for one change.
#[derive(Debug, Clone, Copy, Default)]
pub struct FastCheck<'a> {
    /// What the change as committed affects, where it could be read.
    pub affected: Option<&'a Affected>,
    /// Archon runs the check again before the change ships.
    pub gated: bool,
    /// The integration branch the change's pull request is for; `None` for
    /// a release, a stacked change or one Archon could not place.
    pub integration: Option<&'a str>,
}

/// The contract's Checks section. Empty when the repository names neither
/// check and the change is not for an integration branch, so such a
/// repository reads as before.
pub fn check_rules(checks: &Checks, fast: FastCheck<'_>) -> String {
    if checks.is_empty() && fast.integration.is_none() {
        return String::new();
    }
    let mut rules = Vec::with_capacity(5);
    if let Some(template) = checks.fast.as_deref() {
        rules.push(fast_rule(template, fast));
    }
    if let Some(branch) = fast.integration {
        rules.push(integration_rule(branch));
    }
    if rules.is_empty() {
        rules.push("Run only the checks your change affects.".to_owned());
    }
    let full = match checks.full.as_deref() {
        Some(full) => format!(" (`{full}`)"),
        None => String::new(),
    };
    rules.push(format!(
        "For this item, do not run the full suite{full} unless the brief asks for it: \
         it runs on the integration branch after merges, off this item's path."
    ));
    if fast.integration.is_some() {
        rules.extend(INTEGRATION_LIMITS.iter().map(|rule| (*rule).to_owned()));
    }
    assert!(
        rules.iter().all(|rule| !rule.contains('\n')),
        "validated: one-line commands and branch names"
    );
    let bullets: String = rules.iter().map(|rule| format!("- {rule}\n")).collect();
    format!("\n## Checks\n\n{bullets}")
}

/// What a change for an integration branch adds: when the full suite still
/// runs, the tree its evidence names, and what its reviewers run.
const INTEGRATION_LIMITS: [&str; 3] = [
    "Run the full suite only for a release pull request (the integration branch into \
     the release branch), release preparation, or when the brief asks for it.",
    "Report the `git rev-parse HEAD^{tree}` of your last fast checks in place of a full \
     check.",
    "Reviewers do not rerun the full suite: they read the checks' evidence and run only \
     the focused checks the review needs. Tell each reviewer so.",
];

/// The fast checks of a change for the integration branch `branch`, beside
/// any fast check command the repository names.
fn integration_rule(branch: &str) -> String {
    assert!(!branch.trim().is_empty(), "a branch has a name");
    assert!(
        !branch.contains(['`', '\n']),
        "validated: a plain branch name"
    );
    format!(
        "This change's pull request is for the integration branch `{branch}`: run fast \
         checks, through the repository's CI slot script where it has one (such as `sh \
         scripts/ci-slot.sh <command>`): the format check, the linter, and the tests of the \
         crates or packages your change touches. Name each command among the checks you ran."
    )
}

/// The fast check's bullet: the command, what its placeholders take and
/// the command for the change as it stands, and whether ship runs it again.
fn fast_rule(template: &str, fast: FastCheck<'_>) -> String {
    assert!(!template.trim().is_empty(), "validated: a command");
    let mut rule = format!("This repository's fast check: `{template}`.");
    if affected::selects(template) {
        rule.push_str(
            " Its `{packages}` placeholders take what your change affects: in a Cargo \
             workspace the crates it changes and every workspace crate depending on them, \
             elsewhere the top-level directories it changes; all of them once it changes a \
             root manifest, lockfile, toolchain or CI file.",
        );
        rule.push_str(&current(template, fast.affected));
    }
    rule.push_str(
        " Run it for your final change before you commit, fix what it reports, and name the \
         command you ran among the checks you ran; reviewers run it as evidence.",
    );
    if fast.gated {
        rule.push_str(
            " Archon runs it again on the committed change before it ships, and refuses the \
             item if it fails.",
        );
    }
    rule
}

/// The command for the change as committed now, if it could be read.
fn current(template: &str, affected: Option<&Affected>) -> String {
    let Some(affected) = affected else {
        return String::new();
    };
    let whole = match &affected.whole {
        Some(path) => format!(" (`{path}` changed, so all of them)"),
        None => String::new(),
    };
    match affected::expand(template, &affected.units) {
        Ok(Some(command)) => format!(" For the change as committed now{whole}: `{command}`."),
        Ok(None) => " The change as committed now affects none.".to_owned(),
        Err(why) => format!(" It cannot run for the change as committed now: {why}."),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn checks(fast: Option<&str>, full: Option<&str>) -> Checks {
        Checks {
            fast: fast.map(str::to_owned),
            full: full.map(str::to_owned),
            release: None,
        }
    }

    fn units(units: &[&str], whole: Option<&str>) -> Affected {
        Affected {
            units: units.iter().map(|u| (*u).to_owned()).collect(),
            whole: whole.map(str::to_owned),
        }
    }

    #[test]
    fn the_section_names_the_fast_check_and_forbids_the_full_one() {
        let fast = FastCheck::default();
        let text = check_rules(&checks(Some("cargo test -p a"), Some("make ci")), fast);
        assert!(text.starts_with("\n## Checks\n"), "{text}");
        assert!(text.contains("fast check: `cargo test -p a`"), "{text}");
        assert!(
            text.contains("do not run the full suite (`make ci`)"),
            "{text}"
        );
        assert!(!text.contains("placeholders"), "{text}");
        assert!(!text.contains("Archon runs it again"), "{text}");
        let unnamed = check_rules(&checks(Some("make quick"), None), fast);
        assert!(
            unnamed.contains("do not run the full suite unless the brief asks for it:"),
            "{unnamed}"
        );
    }

    #[test]
    fn a_change_for_the_integration_branch_runs_fast_checks_and_leaves_the_full_suite() {
        let fast = FastCheck {
            integration: Some("next"),
            ..FastCheck::default()
        };
        let text = check_rules(&Checks::default(), fast);
        let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
        for said in [
            "pull request is for the integration branch `next`",
            "(such as `sh scripts/ci-slot.sh <command>`)",
            "the format check, the linter, and the tests of the crates or packages your change touches",
            "do not run the full suite unless the brief asks for it:",
            "Run the full suite only for a release pull request",
            "release preparation, or when the brief asks for it",
            "of your last fast checks in place of a full check",
            "Reviewers do not rerun the full suite: they read the checks' evidence",
        ] {
            assert!(flat.contains(said), "missing {said:?} in {text}");
        }
        let configured = check_rules(&checks(Some("make quick"), Some("make ci")), fast);
        assert!(
            configured.contains("fast check: `make quick`"),
            "{configured}"
        );
        assert!(
            configured.contains("integration branch `next`: run fast checks"),
            "a configured command keeps the format, lint and slot rule: {configured}"
        );
        assert!(
            configured.contains("Reviewers do not rerun"),
            "{configured}"
        );
        let elsewhere = check_rules(
            &checks(Some("make quick"), Some("make ci")),
            FastCheck::default(),
        );
        for integration_only in [
            "integration branch `",
            "only for a release pull request",
            "in place of a full check",
            "Reviewers do not rerun",
        ] {
            assert!(!elsewhere.contains(integration_only), "{elsewhere}");
        }
    }

    #[test]
    fn a_repository_without_checks_gets_no_section() {
        assert_eq!(check_rules(&Checks::default(), FastCheck::default()), "");
        let full_only = check_rules(&checks(None, Some("make ci")), FastCheck::default());
        assert!(
            full_only.contains("checks your change affects"),
            "{full_only}"
        );
        assert!(full_only.contains("(`make ci`)"), "{full_only}");
    }

    #[test]
    fn the_section_gives_the_command_for_the_change_as_committed() {
        let template = "cargo test {packages:-p}";
        let affected = units(&["app", "core"], None);
        let fast = FastCheck {
            affected: Some(&affected),
            gated: true,
            integration: None,
        };
        let text = check_rules(&checks(Some(template), None), fast);
        assert!(
            text.contains("every workspace crate depending on them"),
            "{text}"
        );
        assert!(
            text.contains("For the change as committed now: `cargo test -p app -p core`."),
            "{text}"
        );
        assert!(text.contains("Archon runs it again"), "{text}");
        let whole = units(&["a"], Some("Cargo.lock"));
        let fast = FastCheck {
            affected: Some(&whole),
            gated: false,
            integration: None,
        };
        let text = check_rules(&checks(Some(template), None), fast);
        assert!(
            text.contains("(`Cargo.lock` changed, so all of them)"),
            "{text}"
        );
        assert!(!text.contains("Archon runs it again"), "{text}");
        let none = units(&[], None);
        let fast = FastCheck {
            affected: Some(&none),
            gated: false,
            integration: None,
        };
        let text = check_rules(&checks(Some(template), None), fast);
        assert!(text.contains("as committed now affects none"), "{text}");
        let hostile = units(&["$(touch x)"], None);
        let fast = FastCheck {
            affected: Some(&hostile),
            gated: false,
            integration: None,
        };
        let text = check_rules(&checks(Some(template), None), fast);
        assert!(text.contains("It cannot run for the change"), "{text}");
        assert!(!text.contains("cargo test $("), "{text}");
    }
}
