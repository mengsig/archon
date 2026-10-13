//! Text for an initiative and its members.

use std::fmt::Write as _;

use archon_core::initiative::InitiativeDetail;

use super::{LINE_CHARS, item_line};

pub(super) fn detail(tool: &str, detail: &InitiativeDetail) -> String {
    let initiative = &detail.initiative;
    let verb = match tool {
        "enqueue_initiative" => "Enqueued ",
        "update_initiative" => "Updated ",
        _ => "",
    };
    let atomic = if initiative.atomic { ", atomic" } else { "" };
    let mut text = format!(
        "{verb}initiative {} [{}{atomic}]: {}\nContract:\n{}\n{} member(s):",
        initiative.id,
        initiative.state.as_str(),
        initiative.title,
        initiative.contract,
        detail.members.len()
    );
    for member in &detail.members {
        let _ = write!(text, "\n{}", item_line(member, LINE_CHARS));
    }
    text
}

#[cfg(test)]
mod tests {
    use archon_core::initiative::Initiative;

    use super::*;

    #[test]
    fn an_initiative_reply_names_its_id_and_contract() {
        let initiative =
            Initiative::new("Rename", "Field is `account_id`.", true, archon_core::now()).unwrap();
        let id = initiative.id.to_string();
        let shown = InitiativeDetail {
            initiative,
            members: Vec::new(),
        };
        let text = detail("enqueue_initiative", &shown);
        assert!(text.starts_with(&format!("Enqueued initiative {id} [open, atomic]: Rename")));
        assert!(text.contains("Field is `account_id`.") && text.ends_with("0 member(s):"));
    }
}
