//! The repository's check tiers in every stage agent's contract (issue #82):
//! the fast check items run, and the full suite they leave to the
//! integration branch.

use archon_core::repo::Checks;

/// The contract's Checks section. Empty when the repository names neither
/// check, so a repository without tiers reads as before.
pub fn check_rules(checks: &Checks) -> String {
    if checks.is_empty() {
        return String::new();
    }
    let fast = match checks.fast.as_deref() {
        Some(fast) => format!(
            "This repository's fast check: `{fast}`. Run it before you commit, fix what it \
             reports, and name it among the checks you ran; reviewers run it as evidence."
        ),
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

#[cfg(test)]
mod tests {
    use super::*;

    fn checks(fast: Option<&str>, full: Option<&str>) -> Checks {
        Checks {
            fast: fast.map(str::to_owned),
            full: full.map(str::to_owned),
        }
    }

    #[test]
    fn the_section_names_the_fast_check_and_forbids_the_full_one() {
        let text = check_rules(&checks(Some("cargo test -p a"), Some("make ci")));
        assert!(text.starts_with("\n## Checks\n"), "{text}");
        assert!(text.contains("fast check: `cargo test -p a`"), "{text}");
        assert!(
            text.contains("Do not run the full suite (`make ci`)"),
            "{text}"
        );
        let unnamed = check_rules(&checks(Some("make quick"), None));
        assert!(unnamed.contains("Do not run the full suite:"), "{unnamed}");
    }

    #[test]
    fn a_repository_without_checks_gets_no_section() {
        assert_eq!(check_rules(&Checks::default()), "");
        let full_only = check_rules(&checks(None, Some("make ci")));
        assert!(
            full_only.contains("checks your change affects"),
            "{full_only}"
        );
        assert!(full_only.contains("(`make ci`)"), "{full_only}");
    }
}
