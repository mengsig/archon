//! The contract's Initiative section: what a member's stage agents are told
//! about the initiative their item belongs to.

use archon_core::initiative::InitiativeBrief;

/// The shared contract and where every sibling's work is; empty outside an
/// initiative. Not written yet: no item is a member until one can be enqueued.
pub fn initiative_rules(initiative: Option<&InitiativeBrief>) -> String {
    assert!(initiative.is_none(), "initiatives cannot be enqueued yet");
    String::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_item_outside_an_initiative_is_told_nothing() {
        assert_eq!(initiative_rules(None), "");
    }
}
