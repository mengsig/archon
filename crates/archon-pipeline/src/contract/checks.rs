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
}

/// The contract's Checks section. Empty when the repository names neither
/// check, so a repository without tiers reads as before.
pub fn check_rules(checks: &Checks, fast: FastCheck<'_>) -> String {
    if checks.is_empty() {
        return String::new();
    }
    let fast = match checks.fast.as_deref() {
        Some(template) => fast_rule(template, fast),
        None => "Run only the checks your change affects.".to_owned(),
    };
    let full = match checks.full.as_deref() {
        Some(full) => format!(" (`{full}`)"),
        None => String::new(),
    };
    assert!(
        !fast.contains('\n') && !full.contains('\n'),
        "validated: one-line commands"
    );
    format!(
        "\n## Checks\n\n\
         - {fast}\n\
         - Do not run the full suite{full}: it runs on the integration branch after \
         merges, off this item's path.\n"
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
            text.contains("Do not run the full suite (`make ci`)"),
            "{text}"
        );
        assert!(!text.contains("placeholders"), "{text}");
        assert!(!text.contains("Archon runs it again"), "{text}");
        let unnamed = check_rules(&checks(Some("make quick"), None), fast);
        assert!(unnamed.contains("Do not run the full suite:"), "{unnamed}");
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
        };
        let text = check_rules(&checks(Some(template), None), fast);
        assert!(text.contains("as committed now affects none"), "{text}");
        let hostile = units(&["$(touch x)"], None);
        let fast = FastCheck {
            affected: Some(&hostile),
            gated: false,
        };
        let text = check_rules(&checks(Some(template), None), fast);
        assert!(text.contains("It cannot run for the change"), "{text}");
        assert!(!text.contains("cargo test $("), "{text}");
    }
}
