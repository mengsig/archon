//! The design ledger (design.md §8.6): the approved intent of a live design
//! session, built from archond's own records, never from the agent's output.
//! It starts from the item's current approved intent and appends to it, so
//! the store's `check_preserved` holds across a Revise.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use archon_core::design::{
    DesignNote, DesignSession, FinishChoices, IMAGE_CAP_WARN_MARGIN, SessionState, Viewport,
};
use archon_core::error::ArchonError;
use archon_core::id::QueueItemId;
use archon_core::intent::{
    ApprovedIntent, IntentArtifact, IntentSource, ReferenceImage, Requirement, RequirementKind,
    Viewport as ImageViewport,
};
use archon_core::run::{OperatorGate, PipelineRun, StageRun, StageState};
use archon_queue::Store;
use base64::{Engine, engine::general_purpose::STANDARD};
use sha2::{Digest, Sha256};

use super::intent::{Pending, key};
use crate::context::DaemonContext;
use crate::design::WorktreeRef;

/// The most reference images an intent holds (`ApprovedIntent::validate`).
const MAX_IMAGES: usize = 128;

/// Marks a visual note whose final picture the operator did not include.
const WITHHELD: &str = "(visual; picture withheld by the operator)";

/// The requirement of a design session in which the operator asked for nothing.
const NO_CHANGE: &str =
    "The operator reviewed the running app in a design session and asked for no change";

/// One active note, as the ledger sees it.
pub(crate) struct LedgerNote {
    pub id: String,
    pub text: String,
    /// `(note id, its words)` of each note this one superseded.
    pub earlier: Vec<(String, String)>,
    /// The included final picture, if any.
    pub picture: Option<LedgerPicture>,
}

pub(crate) struct LedgerPicture {
    pub path: PathBuf,
    pub viewport: Viewport,
    pub caption: String,
}

fn sha_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// The draft intent: `base` (the item's current approved intent, kept whole)
/// plus one requirement per note. With no note, `no_change_id` names the
/// requirement of "no change"; without it nothing is approved.
pub(crate) fn build_draft(
    base: Option<&ApprovedIntent>,
    brief: &str,
    notes: &[LedgerNote],
    no_change_id: Option<&str>,
) -> Result<ApprovedIntent, ArchonError> {
    let mut draft = base.cloned().unwrap_or_else(|| ApprovedIntent {
        id: "design".into(),
        version: 1,
        operator_words: String::new(),
        source: IntentSource::Gate {
            item: "design".into(),
            gate: "design:1".into(),
        },
        artifacts: Vec::new(),
        images: Vec::new(),
        requirements: Vec::new(),
    });
    let brief = brief.trim();
    if notes.is_empty() {
        let Some(id) = no_change_id else {
            return Err(ArchonError::Invalid(
                "nothing to approve: the design session has no active note".into(),
            ));
        };
        if base.is_some() {
            draft.validate()?;
            return Ok(draft);
        }
        draft.operator_words = if brief.is_empty() {
            NO_CHANGE.to_owned()
        } else {
            brief.to_owned()
        };
        draft.requirements.push(Requirement {
            id: id.to_owned(),
            text: NO_CHANGE.to_owned(),
            kind: RequirementKind::Behaviour,
            images: Vec::new(),
            owner: "operator".into(),
        });
        draft.validate()?;
        return Ok(draft);
    }
    let mut words = vec![draft.operator_words.trim().to_owned()];
    if !brief.is_empty() && !draft.operator_words.contains(brief) {
        words.push(brief.to_owned());
    }
    for note in notes {
        let text = note.text.trim();
        if draft.requirements.iter().any(|r| r.id == note.id) {
            return Err(ArchonError::Invalid(format!(
                "requirement {} already exists in the approved intent; ids are never overwritten",
                note.id
            )));
        }
        let mut requirement = Requirement {
            id: note.id.clone(),
            text: text.to_owned(),
            kind: RequirementKind::Behaviour,
            images: Vec::new(),
            owner: "operator".into(),
        };
        for (earlier, said) in &note.earlier {
            requirement
                .text
                .push_str(&format!(" Earlier: {earlier} \"{}\"", said.trim()));
        }
        match &note.picture {
            Some(picture) => {
                let image_id = format!("{}-final", note.id);
                if draft.images.iter().any(|i| i.id == image_id) {
                    return Err(ArchonError::Invalid(format!(
                        "reference image {image_id} already exists in the approved intent; \
                         ids are never overwritten"
                    )));
                }
                let bytes = std::fs::read(&picture.path).map_err(|e| {
                    ArchonError::io(format!("read picture {}", picture.path.display()), e)
                })?;
                draft.images.push(ReferenceImage {
                    id: image_id.clone(),
                    path: picture.path.clone(),
                    sha256: sha_hex(&bytes),
                    // The viewport the picture was taken at is what the
                    // build must match. The session knows no theme or app
                    // state, so it records none: the caption ("captured
                    // from the preview at commit …") is where the picture
                    // came from, never a state a built image could show.
                    viewport: Some(ImageViewport {
                        width: picture.viewport.w,
                        height: picture.viewport.h,
                    }),
                    theme: None,
                    state: None,
                });
                requirement.kind = RequirementKind::Visual;
                requirement.images = vec![image_id];
            }
            None => requirement.text.push_str(&format!(" {WITHHELD}")),
        }
        draft.requirements.push(requirement);
        words.push(text.to_owned());
    }
    draft.operator_words = words
        .into_iter()
        .filter(|w| !w.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n");
    draft.validate()?;
    Ok(draft)
}

fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn mime_of(bytes: &[u8]) -> &'static str {
    if bytes.starts_with(&[0x89, b'P', b'N', b'G']) {
        "image/png"
    } else {
        "image/jpeg"
    }
}

/// Each new reference image's caption (where the picture came from), by id.
pub(crate) fn captions(notes: &[LedgerNote]) -> BTreeMap<String, String> {
    notes
        .iter()
        .filter_map(|n| {
            let picture = n.picture.as_ref()?;
            Some((
                format!("{}-final", n.id),
                format!(
                    "{}, {}x{}",
                    picture.caption, picture.viewport.w, picture.viewport.h
                ),
            ))
        })
        .collect()
}

/// The ledger page: self-contained HTML with the included final pictures as
/// `data:` images. `pictures` maps a reference image id to its bytes; any
/// other picture of the session is never shown. `captions` names where each
/// picture came from.
pub(crate) fn render_page(
    draft: &ApprovedIntent,
    pictures: &BTreeMap<String, Vec<u8>>,
    captions: &BTreeMap<String, String>,
) -> String {
    let mut html = String::from(
        "<!doctype html><html><head><meta charset=\"utf-8\"><title>Design ledger</title>\
         <style>body{font:16px/1.5 system-ui,sans-serif;max-width:60rem;margin:2rem auto;\
         padding:0 1rem}figure{margin:1rem 0}img{max-width:100%;border:1px solid #8884}\
         .warn{background:#fe9;padding:.5rem 1rem;border-radius:4px;color:#000}\
         li{margin:.75rem 0}</style></head><body><h1>Design ledger</h1>",
    );
    if draft.images.len() + IMAGE_CAP_WARN_MARGIN as usize >= MAX_IMAGES {
        html.push_str(&format!(
            "<p class=\"warn\">{} of {MAX_IMAGES} reference images used. The limit caps the \
             number of notes that can carry a picture.</p>",
            draft.images.len()
        ));
    }
    html.push_str(&format!(
        "<h2>What the operator asked</h2><p>{}</p><h2>Requirements</h2><ol>",
        escape(&draft.operator_words).replace('\n', "<br>")
    ));
    for requirement in &draft.requirements {
        html.push_str(&format!(
            "<li><strong>{}</strong> ({:?}): {}",
            escape(&requirement.id),
            requirement.kind,
            escape(&requirement.text)
        ));
        for id in &requirement.images {
            let Some(image) = draft.images.iter().find(|i| &i.id == id) else {
                continue;
            };
            let caption = escape(
                captions
                    .get(id)
                    .map(String::as_str)
                    .or(image.state.as_deref())
                    .unwrap_or(""),
            );
            match pictures.get(id) {
                Some(bytes) => html.push_str(&format!(
                    "<figure><img alt=\"{}\" src=\"data:{};base64,{}\"><figcaption>{}</figcaption></figure>",
                    escape(id),
                    mime_of(bytes),
                    STANDARD.encode(bytes),
                    caption
                )),
                None => html.push_str(&format!(
                    "<p>Reference image {} ({caption})</p>",
                    escape(id)
                )),
            }
        }
        html.push_str("</li>");
    }
    html.push_str("</ol></body></html>");
    html
}

/// The run's worktree, with the repository it belongs to.
pub(super) fn worktree_ref(store: &Store, run: &PipelineRun) -> Result<WorktreeRef, ArchonError> {
    let item = store.item(&run.item)?;
    let repo = item
        .repo
        .as_ref()
        .ok_or_else(|| ArchonError::Invalid("a design session needs a repository".into()))?;
    let repo = store.repo(&crate::resolve::repo_ref(repo))?;
    Ok(WorktreeRef {
        repo: repo.path,
        worktree: run.workspace.clone(),
        branch: run.branch.clone(),
    })
}

/// The CSS viewport of `preset`, as the preview browser sets it.
fn preset_viewport(preset: archon_core::design::ViewportPreset) -> Viewport {
    let (w, h) = preset.size();
    Viewport { w, h, preset }
}

/// The notes the confirmed `session` puts in the ledger, each with the words
/// of the notes it superseded and its included final picture.
fn ledger_notes(
    ctx: &DaemonContext,
    store: &Store,
    item: &QueueItemId,
    session: &DesignSession,
    active: &[DesignNote],
) -> Result<Vec<LedgerNote>, ArchonError> {
    let carried = crate::design::DesignService::review_choices(
        store,
        item,
        &session.id,
        session.finish_choices.as_ref(),
    )?;
    let choices: Option<&FinishChoices> = carried.as_ref();
    let head = session.finish_head.clone().unwrap_or_default();
    let all = store.design_notes(item)?;
    let mut out = Vec::new();
    for note in active.iter().filter(|n| !n.no_change) {
        let mut earlier: Vec<&DesignNote> = all
            .iter()
            .filter(|n| {
                n.superseded_by.as_deref() == Some(note.id.as_str())
                    || choices
                        .and_then(|c| c.superseded.get(&n.id))
                        .is_some_and(|by| by.as_deref() == Some(note.id.as_str()))
            })
            .collect();
        earlier.sort_by_key(|n| n.number);
        earlier.dedup_by_key(|n| n.number);
        let included = choices
            .and_then(|c| c.include.get(&note.id).copied())
            .or(note.final_included)
            .unwrap_or(true);
        let picture = match (&note.final_asset, included) {
            (Some(asset), true) => store.design_asset(item, asset)?.map(|asset| {
                let commit = note.captured_commit.as_deref().unwrap_or(&head);
                LedgerPicture {
                    path: ctx.design.asset_path(&asset),
                    // The viewport Finish took the picture at: its
                    // target's preset, else the note's (`capture_final`).
                    viewport: preset_viewport(
                        note.target
                            .as_ref()
                            .map_or(note.viewport.preset, |t| t.viewport.preset),
                    ),
                    caption: format!(
                        "captured from the preview at commit {} {}",
                        &commit[..commit.len().min(7)],
                        asset.created_at.format("%Y-%m-%d %H:%M UTC")
                    ),
                }
            }),
            _ => None,
        };
        out.push(LedgerNote {
            id: note.id.clone(),
            text: note.text.clone(),
            earlier: earlier
                .into_iter()
                .map(|n| (n.id.clone(), n.text.clone()))
                .collect(),
            picture,
        });
    }
    Ok(out)
}

/// The active notes the confirmed `session` puts in the ledger. A session whose
/// done notes are all "no change" (no commit, so never active) is approvable
/// with none: the ledger then adds nothing. Anything else without an active
/// note is refused.
pub(super) fn approvable_notes(
    ctx: &DaemonContext,
    store: &Store,
    worktree: &WorktreeRef,
    item: &QueueItemId,
    session: &DesignSession,
    head: &str,
) -> Result<Vec<DesignNote>, ArchonError> {
    let choices = crate::design::DesignService::review_choices(
        store,
        item,
        &session.id,
        session.finish_choices.as_ref(),
    )?;
    let active = ctx
        .design
        .active_notes(store, worktree, item, head, choices.as_ref())?;
    // The rule Finish and Confirm apply (`crate::design::approval`): with
    // nothing active, a done "no change" note or an earlier approval of
    // design notes makes the session approvable as it is.
    let approved = crate::design::approved_design(store, item)?;
    let notes = store.design_notes(item)?;
    match crate::design::approval(&active, &notes, &session.id, &approved) {
        crate::design::Approval::Nothing => Err(ArchonError::Invalid(
            "nothing to approve: the design session has no active note".into(),
        )),
        _ => Ok(active),
    }
}

/// Build, preserve and freeze the ledger of a design gate: the design twin of
/// `intent::prepare`. Returns the rendered page.
pub(super) fn prepare(
    ctx: &DaemonContext,
    store: &mut Store,
    run: &PipelineRun,
    gate: &OperatorGate,
    stages: &[StageRun],
) -> Result<PathBuf, ArchonError> {
    assert_eq!(gate.run, run.id, "freeze the gate's own run");
    if store.kv_get(&key(gate))?.is_some() {
        let saved = super::intent::pending(store, gate)?;
        super::intent::verify_page(&saved)?;
        return Ok(saved.page);
    }
    stages
        .iter()
        .find(|s| {
            s.stage_id == gate.stage_id
                && s.attempt == gate.attempt
                && s.state == StageState::Completed
        })
        .ok_or_else(|| ArchonError::NotFound("design stage".into()))?;
    let item = store.item(&run.item)?;
    let base = match item.intent.as_deref() {
        Some(binding) => Some(store.approved_intent(binding)?.ok_or_else(|| {
            ArchonError::Corrupt(format!("approved intent {} is missing", binding.id))
        })?),
        None => None,
    };
    let session = ctx
        .design
        .session_of(store, &run.id, &gate.stage_id, gate.attempt)?
        .filter(|s| s.state == SessionState::Confirmed)
        .ok_or_else(|| ArchonError::Invalid("the design session was not confirmed".into()))?;
    let head = session
        .finish_head
        .clone()
        .ok_or_else(|| ArchonError::Corrupt("a confirmed session has no finish head".into()))?;
    let worktree = worktree_ref(store, run)?;
    let active = approvable_notes(ctx, store, &worktree, &item.id, &session, &head)?;
    let notes = ledger_notes(ctx, store, &item.id, &session, &active)?;
    // A finish with no change names its requirement after the item's
    // counter, so it never takes an id an earlier note or session used.
    let no_change = store.design_next_note_id(&item.id)?;
    let mut record = build_draft(base.as_ref(), &item.brief, &notes, Some(&no_change))?;
    let base_images: Vec<String> = base
        .iter()
        .flat_map(|i| i.images.iter().map(|image| image.id.clone()))
        .collect();
    record.id = run.id.to_string();
    record.version = gate.attempt;
    record.source = IntentSource::Gate {
        item: run.item.to_string(),
        gate: format!("design:{}", gate.attempt),
    };
    let destination = ctx.paths.data_dir.join("approved-intent-artifacts");
    let mut pictures = BTreeMap::new();
    for image in record
        .images
        .iter_mut()
        .filter(|i| !base_images.contains(&i.id))
    {
        let bytes = std::fs::read(&image.path)
            .map_err(|e| ArchonError::io(format!("read picture {}", image.path.display()), e))?;
        image.path = crate::handlers::intent::preserve(&destination, &image.path, &image.sha256)?;
        pictures.insert(image.id.clone(), bytes);
    }
    record.validate()?;
    let html = render_page(&record, &pictures, &captions(&notes));
    let directory = ctx.paths.data_dir.join("design").join("ledger");
    std::fs::create_dir_all(&directory)
        .map_err(|e| ArchonError::io("create the design ledger directory", e))?;
    let page = directory.join(format!(
        "{}-{}-{}.html",
        run.id, gate.stage_id, gate.attempt
    ));
    crate::handlers::intent::save_bytes(&page, html.as_bytes())?;
    let page_sha256 = sha_hex(html.as_bytes());
    let preserved = crate::handlers::intent::preserve(&destination, &page, &page_sha256)?;
    record.artifacts.push(IntentArtifact {
        path: preserved,
        sha256: page_sha256.clone(),
    });
    record.validate()?;
    let saved = Pending {
        record,
        page: page.clone(),
        page_sha256,
    };
    store.kv_set(&key(gate), &serde_json::to_string(&saved)?)?;
    Ok(page)
}

/// Whether `page` is a ledger page archond wrote itself.
pub(super) fn is_ledger_page(ctx: &DaemonContext, page: &Path) -> bool {
    page.starts_with(ctx.paths.data_dir.join("design").join("ledger"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use archon_core::design::ViewportPreset;

    const PNG: &[u8] = &[0x89, b'P', b'N', b'G', 1, 2, 3];

    fn viewport() -> Viewport {
        Viewport {
            w: 1440,
            h: 900,
            preset: ViewportPreset::Desktop,
        }
    }

    fn picture(dir: &Path, name: &str) -> LedgerPicture {
        let path = dir.join(name);
        std::fs::write(&path, [PNG, name.as_bytes()].concat()).unwrap();
        LedgerPicture {
            path,
            viewport: viewport(),
            caption: "captured from the preview at commit 3f9c2a1 2026-11-21 10:00 UTC".into(),
        }
    }

    fn note(id: &str, text: &str, picture: Option<LedgerPicture>) -> LedgerNote {
        LedgerNote {
            id: id.into(),
            text: text.into(),
            earlier: Vec::new(),
            picture,
        }
    }

    fn inbox_intent() -> ApprovedIntent {
        let image = |id: &str, c: char| ReferenceImage {
            id: id.into(),
            path: PathBuf::from(format!("/data/{id}.png")),
            sha256: c.to_string().repeat(64),
            viewport: None,
            theme: None,
            state: None,
        };
        let requirement = |id: &str| Requirement {
            id: id.into(),
            text: "match it".into(),
            kind: RequirementKind::Visual,
            images: vec![id.into()],
            owner: "operator".into(),
        };
        ApprovedIntent {
            id: "inbox1".into(),
            version: 1,
            operator_words: "Build the page as pictured".into(),
            source: IntentSource::Inbox { seq: 4 },
            artifacts: vec![IntentArtifact {
                path: PathBuf::from("/data/a.html"),
                sha256: "b".repeat(64),
            }],
            images: vec![image("shot1", 'c'), image("shot2", 'd')],
            requirements: vec![requirement("shot1"), requirement("shot2")],
        }
    }

    #[test]
    fn a_visual_note_binds_its_final_picture_only() {
        let dir = tempfile::tempdir().unwrap();
        let notes = [note(
            "D2",
            "make it red",
            Some(picture(dir.path(), "d2.png")),
        )];
        let draft = build_draft(None, "Fix the button", &notes, None).unwrap();
        assert_eq!(draft.operator_words, "Fix the button\n\nmake it red");
        assert_eq!(draft.images.len(), 1);
        assert_eq!(draft.images[0].id, "D2-final");
        // The picture records the viewport it was taken at and nothing a
        // built image could never show: its caption is no app state.
        let image = &draft.images[0];
        assert_eq!(
            image.viewport,
            Some(ImageViewport {
                width: 1440,
                height: 900
            })
        );
        assert_eq!(
            (image.theme.as_deref(), image.state.as_deref()),
            (None, None)
        );
        let requirement = &draft.requirements[0];
        assert_eq!(requirement.kind, RequirementKind::Visual);
        assert_eq!(requirement.images, ["D2-final"]);
        assert_eq!(requirement.owner, "operator");
        draft.validate().unwrap();
        let json = serde_json::to_string(&draft).unwrap();
        assert!(!json.contains("before") && !json.contains("after"));
    }

    #[test]
    fn a_withheld_picture_gives_a_marked_behaviour() {
        let notes = [note("D1", "tighten the header", None)];
        let draft = build_draft(None, "Polish", &notes, None).unwrap();
        let requirement = &draft.requirements[0];
        assert_eq!(requirement.kind, RequirementKind::Behaviour);
        assert!(requirement.images.is_empty() && draft.images.is_empty());
        assert!(requirement.text.contains(WITHHELD));
        draft.validate().unwrap();
    }

    #[test]
    fn earlier_notes_of_any_session_are_cited() {
        let dir = tempfile::tempdir().unwrap();
        let mut d2 = note("D2", "make it red", Some(picture(dir.path(), "d2.png")));
        d2.earlier = vec![("D1".into(), "make it green".into())];
        let draft = build_draft(None, "Button", &[d2], None).unwrap();
        assert!(
            draft.requirements[0]
                .text
                .contains("Earlier: D1 \"make it green\"")
        );
        assert!(!draft.operator_words.contains("green"));
    }

    #[test]
    fn appending_to_an_inbox_intent_keeps_everything_it_had() {
        let dir = tempfile::tempdir().unwrap();
        let base = inbox_intent();
        let notes = [note(
            "D1",
            "make it red",
            Some(picture(dir.path(), "d1.png")),
        )];
        let draft = build_draft(Some(&base), "Build the page as pictured", &notes, None).unwrap();
        assert!(draft.operator_words.starts_with(&base.operator_words));
        assert_eq!(draft.images.len(), 3);
        assert_eq!(draft.requirements.len(), 3);
        for kept in &base.requirements {
            assert!(draft.requirements.contains(kept));
        }
        for kept in &base.images {
            assert!(draft.images.contains(kept));
        }
        assert_eq!(draft.artifacts, base.artifacts);
        draft.validate().unwrap();
    }

    #[test]
    fn a_colliding_id_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let mut base = inbox_intent();
        base.requirements[0].id = "D1".into();
        let notes = [note("D1", "again", None)];
        let refused = build_draft(Some(&base), "x", &notes, None).unwrap_err();
        assert!(refused.to_string().contains("already exists"), "{refused}");
        let mut base = inbox_intent();
        base.images[0].id = "D1-final".into();
        let notes = [note("D1", "again", Some(picture(dir.path(), "d1.png")))];
        assert!(build_draft(Some(&base), "x", &notes, None).is_err());
    }

    #[test]
    fn no_note_is_refused_unless_it_is_a_review_for_no_change() {
        assert!(build_draft(None, "x", &[], None).is_err());
        let base = inbox_intent();
        assert!(build_draft(Some(&base), "x", &[], None).is_err());
    }

    #[test]
    fn no_change_keeps_an_existing_intent_unchanged() {
        let base = inbox_intent();
        let draft = build_draft(Some(&base), "ignored", &[], Some("D3")).unwrap();
        assert_eq!(draft, base);
    }

    #[test]
    fn no_change_without_an_intent_is_one_behaviour_requirement() {
        let draft = build_draft(None, "Check the app", &[], Some("D1")).unwrap();
        assert_eq!(draft.requirements.len(), 1);
        let requirement = &draft.requirements[0];
        assert_eq!(requirement.id, "D1");
        assert_eq!(requirement.kind, RequirementKind::Behaviour);
        assert_eq!(
            requirement.text,
            "The operator reviewed the running app in a design session and asked for no change"
        );
        assert!(draft.images.is_empty());
        draft.validate().unwrap();
    }

    #[test]
    fn the_page_embeds_included_pictures_and_nothing_else() {
        let dir = tempfile::tempdir().unwrap();
        let notes = [
            note("D1", "make it <red>", Some(picture(dir.path(), "d1.png"))),
            note("D2", "wider", None),
        ];
        let draft = build_draft(None, "Button", &notes, None).unwrap();
        let bytes = std::fs::read(&draft.images[0].path).unwrap();
        let pictures = BTreeMap::from([("D1-final".to_owned(), bytes.clone())]);
        let page = render_page(&draft, &pictures, &captions(&notes));
        assert!(page.contains(&format!(
            "data:image/png;base64,{}",
            STANDARD.encode(&bytes)
        )));
        assert_eq!(page.matches("data:image/").count(), 1);
        assert!(page.contains("make it &lt;red&gt;"));
        assert!(page.contains("captured from the preview at commit 3f9c2a1"));
        assert!(!page.contains("class=\"warn\""));
    }

    #[test]
    fn the_page_warns_near_the_image_cap() {
        let mut draft = build_draft(None, "x", &[note("D1", "y", None)], None).unwrap();
        for n in 0..(MAX_IMAGES - 8) {
            draft.images.push(ReferenceImage {
                id: format!("i{n}"),
                path: PathBuf::from("/x"),
                sha256: "e".repeat(64),
                viewport: None,
                theme: None,
                state: None,
            });
        }
        assert!(render_page(&draft, &BTreeMap::new(), &BTreeMap::new()).contains("120 of 128"));
        draft.images.pop();
        assert!(
            !render_page(&draft, &BTreeMap::new(), &BTreeMap::new()).contains("class=\"warn\"")
        );
    }
}
