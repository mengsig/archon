use archon_core::intent::{
    ApprovedIntent, IntentBinding, IntentSource, Requirement, RequirementKind,
};
use archon_queue::Store;

fn record() -> ApprovedIntent {
    ApprovedIntent {
        id: "design".into(),
        version: 1,
        operator_words: "Use this design".into(),
        source: IntentSource::Inbox { seq: 42 },
        artifacts: vec![],
        images: vec![],
        requirements: vec![
            Requirement {
                id: "R1".into(),
                text: "Show title".into(),
                kind: RequirementKind::Wording,
                images: vec![],
                owner: "app".into(),
            },
            Requirement {
                id: "R2".into(),
                text: "Save changes".into(),
                kind: RequirementKind::Behaviour,
                images: vec![],
                owner: "app".into(),
            },
        ],
    }
}

#[test]
fn records_are_versioned_immutable_and_subsets_are_checked() {
    let mut store = Store::open_in_memory().unwrap();
    let first = record();
    let mut binding = IntentBinding {
        id: first.id.clone(),
        version: 1,
        requirements: vec!["R2".into()],
    };
    store.save_approved_intent(&first).unwrap();
    let read = store.approved_intent(&binding).unwrap().unwrap();
    assert_eq!(read, first);
    assert_eq!(read.selected_requirements(&binding).unwrap()[0].id, "R2");
    assert!(store.save_approved_intent(&first).is_err());
    assert!(store.kv_set("approved_intent:design:1", "{} ").is_err());
    assert!(store.kv_delete("approved_intent:design:1").is_err());
    binding.requirements = vec!["missing".into()];
    assert!(store.approved_intent(&binding).is_err());
    binding.requirements.clear();
    assert_eq!(
        store
            .approved_intent(&binding)
            .unwrap()
            .unwrap()
            .selected_requirements(&binding)
            .unwrap()
            .len(),
        2
    );
    let mut second = first.clone();
    second.version = 2;
    second.operator_words = "Revised".into();
    store.save_approved_intent(&second).unwrap();
    assert_eq!(store.approved_intent(&binding).unwrap(), Some(first));
    binding.version = 2;
    assert_eq!(store.approved_intent(&binding).unwrap(), Some(second));
}
