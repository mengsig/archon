//! Shared contract and sibling locations for each initiative member.

use archon_core::initiative::{InitiativeBrief, MAX_CONTRACT_BYTES};

pub fn initiative_rules(initiative: Option<&InitiativeBrief>) -> String {
    let Some(initiative) = initiative else {
        return String::new();
    };
    assert!(!initiative.contract.trim().is_empty());
    assert!(initiative.contract.len() <= MAX_CONTRACT_BYTES);
    let mut out = format!(
        "\n## Initiative\n\n{}\n\nShared contract:\n{}\n\nSibling facts (JSON):\n",
        initiative.title, initiative.contract
    );
    for sibling in &initiative.siblings {
        let fact = serde_json::to_string(sibling).expect("string facts serialize");
        assert!(!fact.contains('\n'));
        out.push_str(&format!("- {fact}\n"));
    }
    if initiative.atomic {
        out.push_str("Keep this PR a draft; Archon marks all member PRs ready after every member passes. Do not merge or enable auto-merge.\n");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn outside_an_initiative_there_are_no_rules() {
        assert!(initiative_rules(None).is_empty());
    }
}
