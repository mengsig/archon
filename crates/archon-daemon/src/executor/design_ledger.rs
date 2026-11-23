//! The design ledger (design.md §8.6): the approved intent of a live design
//! session, built from archond's own records, never from the agent's output.
//! It starts from the item's current approved intent and appends to it, so
//! the store's `check_preserved` holds across a Revise.

use std::path::{Path, PathBuf};

use archon_core::design::{
    DesignAsset, DesignAssetId, DesignNote, DesignSession, FinishChoices, NoteKind, NoteStatus,
    ReplyAuthor, SessionState, Target, Viewport, undone_by,
};
use archon_core::error::ArchonError;
use archon_core::id::QueueItemId;
use archon_core::intent::{
    ApprovedIntent, IntentArtifact, IntentSource, ReferenceImage, Requirement, RequirementKind,
    Viewport as ImageViewport,
};
use archon_core::run::{OperatorGate, PipelineRun, StageRun, StageState};
use archon_queue::Store;
use sha2::{Digest, Sha256};

use super::design_page::{self, Card, CommitFacts, Outcome, Page, Pic};
use super::intent::{Pending, key};
use crate::context::DaemonContext;
use crate::design::WorktreeRef;

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
    /// The picture the ledger binds: the final one, else the newest after
    /// capture; none only when the operator left it out.
    pub picture: Option<LedgerPicture>,
    /// The operator chose, in the finish step, to leave the picture out.
    pub withheld: bool,
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
            None if note.withheld => requirement.text.push_str(&format!(" {WITHHELD}")),
            None => {}
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

/// The picture a note takes into the ledger, and whether the operator left
/// it out. The final picture counts, else the newest after capture, so a
/// captured picture is never lost for want of a final one. Only a note with a
/// final picture can be withheld: that is what the review's switch is for.
fn chosen_picture(
    note: &DesignNote,
    choices: Option<&FinishChoices>,
) -> (Option<DesignAssetId>, bool) {
    let asset = note
        .final_asset
        .clone()
        .or_else(|| note.after_asset.clone());
    let chose = choices.and_then(|c| c.include.get(&note.id).copied());
    let left_out = note.final_asset.is_some()
        && chose.map_or(note.final_included == Some(false), |include| !include);
    assert!(
        asset.is_some() || !left_out,
        "only a picture that exists can be withheld"
    );
    if left_out {
        (None, true)
    } else {
        (asset, false)
    }
}

fn ledger_picture(
    ctx: &DaemonContext,
    note: &DesignNote,
    asset: &DesignAsset,
    head: &str,
) -> LedgerPicture {
    let commit = asset
        .commit
        .as_deref()
        .or(note.captured_commit.as_deref())
        .unwrap_or(head);
    LedgerPicture {
        path: ctx.design.asset_path(asset),
        // The viewport the picture was taken at: its target's preset, else
        // the note's (`capture_final`).
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
        let (asset, withheld) = chosen_picture(note, choices);
        let picture = match asset {
            Some(id) => store
                .design_asset(item, &id)?
                .map(|asset| ledger_picture(ctx, note, &asset, &head)),
            None => None,
        };
        out.push(LedgerNote {
            id: note.id.clone(),
            text: note.text.clone(),
            earlier: earlier
                .into_iter()
                .map(|n| (n.id.clone(), n.text.clone()))
                .collect(),
            picture,
            withheld,
        });
    }
    Ok(out)
}

/// What the operator pointed at, in words.
fn describe_target(target: Option<&Target>, kind: NoteKind) -> Option<String> {
    let Some(target) = target else {
        return (kind == NoteKind::Page).then(|| "the whole page".to_owned());
    };
    let clip = |text: &str| -> String {
        let text = text.trim();
        match text.char_indices().nth(80) {
            Some((end, _)) => format!("{}...", &text[..end]),
            None => text.to_owned(),
        }
    };
    let what = if target.region {
        format!(
            "an area of {} by {} pixels",
            target.rect.w.round(),
            target.rect.h.round()
        )
    } else if let Some(name) = target.role_name.as_deref().filter(|n| !n.trim().is_empty()) {
        format!("\"{}\"", clip(name))
    } else if let Some(component) = &target.component {
        format!("the {} component", component.name)
    } else if let Some(selector) = &target.selector {
        format!("the element {}", clip(selector))
    } else {
        "an element".to_owned()
    };
    Some(format!("{what} on {}", target.path))
}

/// `(label, css class)` of a note's status.
fn status_of(
    note: &DesignNote,
    undone: bool,
    superseded_by: Option<&str>,
) -> (String, &'static str) {
    if undone {
        return ("Undone".into(), "idle");
    }
    if let Some(by) = superseded_by {
        return (format!("Superseded by {by}"), "idle");
    }
    match note.status {
        NoteStatus::Done if note.no_change => ("No change needed".into(), "ok"),
        NoteStatus::Done => ("Done".into(), "ok"),
        NoteStatus::Failed => ("Failed".into(), "bad"),
        NoteStatus::Cancelled => ("Cancelled".into(), "bad"),
        NoteStatus::NeedsReply => ("Waiting for the operator".into(), "idle"),
        NoteStatus::Queued | NoteStatus::Sent | NoteStatus::Working => {
            ("Not finished".into(), "idle")
        }
    }
}

/// What `note` came to, for the page's counts.
fn outcome_of(note: &DesignNote, undone: bool, superseded_by: Option<&str>) -> Outcome {
    if undone || superseded_by.is_some() {
        return Outcome::Replaced;
    }
    match note.status {
        NoteStatus::Done if note.no_change => Outcome::NoChange,
        NoteStatus::Done => Outcome::Changed,
        NoteStatus::Failed | NoteStatus::Cancelled => Outcome::Trouble,
        _ => Outcome::Open,
    }
}

/// A picture's bytes. A missing file leaves the card without it: the page
/// says so, and the gate is never blocked by a picture it only displays.
fn read_pic(path: &Path, label: String, caption: String) -> Option<Pic> {
    match std::fs::read(path) {
        Ok(bytes) => Some(Pic {
            label,
            caption,
            bytes,
        }),
        Err(error) => {
            tracing::warn!(%error, path = %path.display(), "design review page: picture unreadable");
            None
        }
    }
}

/// The commit's one-line message and files, or why they are missing.
fn commit_of(
    ctx: &DaemonContext,
    worktree: &WorktreeRef,
    commit: &str,
) -> (Option<CommitFacts>, Option<String>) {
    use crate::worktree::design_git::{self, NoteGit};
    let short = commit[..commit.len().min(7)].to_owned();
    let facts = ctx.design.shadow(worktree).and_then(|shadow| {
        design_git::commit_facts(
            &NoteGit {
                shadow: &shadow,
                repo: &worktree.repo,
                worktree: &worktree.worktree,
                branch: &worktree.branch,
            },
            commit,
        )
    });
    match facts {
        Ok((subject, files)) => (
            Some(CommitFacts {
                short,
                subject,
                files,
            }),
            None,
        ),
        Err(error) => (
            None,
            Some(format!("Commit {short}: details unavailable ({error})")),
        ),
    }
}

/// The cards of the review page: every note of the session (and each active
/// note of an earlier one), in note order. `notes` are the ledger's notes.
fn page_cards(
    ctx: &DaemonContext,
    store: &Store,
    worktree: &WorktreeRef,
    session: &DesignSession,
    notes: &[LedgerNote],
    draft: &ApprovedIntent,
) -> Result<Vec<Card>, ArchonError> {
    let all = store.design_notes(&session.item)?;
    let undone = undone_by(&all);
    let choices = crate::design::DesignService::review_choices(
        store,
        &session.item,
        &session.id,
        session.finish_choices.as_ref(),
    )?;
    let mut shown: Vec<&DesignNote> = all
        .iter()
        .filter(|n| n.kind != NoteKind::Revert)
        .filter(|n| n.session == session.id || notes.iter().any(|l| l.id == n.id))
        .collect();
    shown.sort_by_key(|n| n.number);
    let mut cards = Vec::new();
    for note in shown {
        let ledger = notes.iter().find(|l| l.id == note.id);
        // A note in the ledger is in force, whatever else was said of it.
        let by = match ledger {
            Some(_) => None,
            None => note.superseded_by.clone().or_else(|| {
                choices
                    .as_ref()
                    .and_then(|c| c.superseded.get(&note.id).cloned().flatten())
            }),
        };
        let (commit, commit_note) = match note.commit.as_deref() {
            Some(c) => commit_of(ctx, worktree, c),
            None => (None, None),
        };
        let is_undone = undone.contains_key(&note.id);
        let status = status_of(note, is_undone, by.as_deref());
        let outcome = outcome_of(note, is_undone, by.as_deref());
        let facts = (commit, commit_note);
        let state = (status, outcome);
        cards.push(card_of(ctx, store, note, ledger, draft, state, facts)?);
    }
    Ok(cards)
}

/// The card of `note`; `facts` are its commit's facts, or why they are missing.
fn card_of(
    ctx: &DaemonContext,
    store: &Store,
    note: &DesignNote,
    ledger: Option<&LedgerNote>,
    draft: &ApprovedIntent,
    state: ((String, &'static str), Outcome),
    facts: (Option<CommitFacts>, Option<String>),
) -> Result<Card, ArchonError> {
    let after = ledger.and_then(|l| l.picture.as_ref()).and_then(|p| {
        let caption = format!("{}, {}x{}", p.caption, p.viewport.w, p.viewport.h);
        read_pic(&p.path, format!("{} after the change", note.id), caption)
    });
    let before = match &note.before_asset {
        Some(id) => store.design_asset(&note.item, id)?.and_then(|a| {
            let caption = format!(
                "taken before the change, {}x{}, {}",
                a.width,
                a.height,
                a.created_at.format("%Y-%m-%d %H:%M UTC")
            );
            let label = format!("{} before the change", note.id);
            read_pic(&ctx.design.asset_path(&a), label, caption)
        }),
        None => None,
    };
    Ok(Card {
        id: note.id.clone(),
        text: note.text.clone(),
        earlier: ledger.map(|l| l.earlier.clone()).unwrap_or_default(),
        kind: draft
            .requirements
            .iter()
            .find(|r| r.id == note.id)
            .map(|r| r.kind.clone()),
        status: state.0,
        outcome: state.1,
        unchanged: archon_core::design::no_visible_change(note).map(str::to_owned),
        target: describe_target(note.target.as_ref(), note.kind),
        reply: note
            .thread
            .iter()
            .rev()
            .find(|e| e.from == ReplyAuthor::Agent)
            .map(|e| e.text.clone()),
        commit: facts.0,
        commit_note: facts.1,
        before,
        after,
        withheld: ledger.is_some_and(|l| l.withheld),
    })
}

/// The checks the run's stages reported (the latest to list any) and what the
/// design stage's agent said.
fn reported(stages: &[StageRun], gate: &OperatorGate) -> (Vec<String>, String) {
    let done = |s: &&StageRun| s.state == StageState::Completed;
    let checks = stages
        .iter()
        .filter(done)
        .rev()
        .find_map(|s| {
            let list = s.structured.as_ref()?.get("checks")?.as_array()?;
            Some(
                list.iter()
                    .filter_map(|c| c.as_str().map(str::to_owned))
                    .collect::<Vec<_>>(),
            )
        })
        .unwrap_or_default();
    let said = stages
        .iter()
        .filter(done)
        .find(|s| s.stage_id == gate.stage_id && s.attempt == gate.attempt)
        .and_then(|s| s.summary.clone())
        .unwrap_or_default();
    (checks, said)
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
    for image in record
        .images
        .iter_mut()
        .filter(|i| !base_images.contains(&i.id))
    {
        image.path = crate::handlers::intent::preserve(&destination, &image.path, &image.sha256)?;
    }
    record.validate()?;
    let cards = page_cards(ctx, store, &worktree, &session, &notes, &record)?;
    let (checks, agent_summary) = reported(stages, gate);
    let page_of = |with_before| Page {
        title: &item.title,
        brief: &item.brief,
        draft: &record,
        agent_summary: &agent_summary,
        checks: &checks,
        cards: &cards,
        with_before,
    };
    // The operator sees the whole page; the copy kept with the approved
    // intent leaves the before pictures out, as they are never sent to a
    // later model.
    let html = design_page::render(&page_of(true));
    let kept_html = design_page::render(&page_of(false));
    let directory = ctx.paths.data_dir.join("design").join("ledger");
    std::fs::create_dir_all(&directory)
        .map_err(|e| ArchonError::io("create the design ledger directory", e))?;
    let page = directory.join(format!(
        "{}-{}-{}.html",
        run.id, gate.stage_id, gate.attempt
    ));
    crate::handlers::intent::save_bytes(&page, html.as_bytes())?;
    let page_sha256 = sha_hex(html.as_bytes());
    let kept = directory.join(format!(
        "{}-{}-{}-kept.html",
        run.id, gate.stage_id, gate.attempt
    ));
    crate::handlers::intent::save_bytes(&kept, kept_html.as_bytes())?;
    let kept_sha256 = sha_hex(kept_html.as_bytes());
    let preserved = crate::handlers::intent::preserve(&destination, &kept, &kept_sha256)?;
    record.artifacts.push(IntentArtifact {
        path: preserved,
        sha256: kept_sha256,
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
    use std::collections::BTreeMap;

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
            withheld: false,
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
        let mut withheld = note("D1", "tighten the header", None);
        withheld.withheld = true;
        let notes = [withheld];
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

    fn dnote(number: u64) -> DesignNote {
        let at = chrono::DateTime::<chrono::Utc>::from_timestamp(0, 0).unwrap();
        DesignNote {
            item: QueueItemId::new(),
            id: format!("D{number}"),
            number,
            session: archon_core::design::DesignSessionId::new(),
            kind: NoteKind::Change,
            text: String::new(),
            details_text: None,
            send_picture: true,
            target: None,
            viewport: viewport(),
            status: NoteStatus::Done,
            fail_reason: None,
            base_commit: None,
            commit: None,
            no_change: false,
            reverts: None,
            resent_from: None,
            superseded_by: None,
            thread: Vec::new(),
            client_key: String::new(),
            delivery: 0,
            before_asset: None,
            after_asset: None,
            final_asset: None,
            final_included: None,
            captured_commit: None,
            created_by_device: None,
            created_at: at,
            updated_at: at,
        }
    }

    fn choice(note: &str, include: bool) -> FinishChoices {
        FinishChoices {
            include: BTreeMap::from([(note.to_owned(), include)]),
            superseded: BTreeMap::new(),
        }
    }

    #[test]
    fn a_captured_after_picture_is_used_when_there_is_no_final_one() {
        let mut d6 = dnote(6);
        d6.after_asset = Some(DesignAssetId::new());
        let (asset, withheld) = chosen_picture(&d6, None);
        assert_eq!((asset, withheld), (d6.after_asset.clone(), false));
        // An old client sent `false` for a card that had no picture to switch.
        let (asset, withheld) = chosen_picture(&d6, Some(&choice("D6", false)));
        assert_eq!((asset, withheld), (d6.after_asset.clone(), false));
    }

    #[test]
    fn the_final_picture_wins_and_only_a_real_choice_withholds_it() {
        let mut d5 = dnote(5);
        d5.after_asset = Some(DesignAssetId::new());
        d5.final_asset = Some(DesignAssetId::new());
        let (asset, withheld) = chosen_picture(&d5, None);
        assert_eq!((asset, withheld), (d5.final_asset.clone(), false));
        let (asset, withheld) = chosen_picture(&d5, Some(&choice("D5", true)));
        assert_eq!((asset, withheld), (d5.final_asset.clone(), false));
        let (asset, withheld) = chosen_picture(&d5, Some(&choice("D5", false)));
        assert_eq!((asset, withheld), (None, true));
        // An earlier session's note keeps what its own review recorded.
        d5.final_included = Some(false);
        assert_eq!(chosen_picture(&d5, None), (None, true));
        d5.final_included = Some(true);
        assert!(!chosen_picture(&d5, None).1);
    }

    #[test]
    fn a_note_without_a_picture_is_not_marked_withheld() {
        let draft = build_draft(None, "x", &[note("D1", "y", None)], None).unwrap();
        assert!(!draft.requirements[0].text.contains("withheld"));
        assert_eq!(
            chosen_picture(&dnote(1), Some(&choice("D1", false))),
            (None, false)
        );
    }

    #[test]
    fn the_target_is_put_in_words() {
        use archon_core::design::{ComponentRef, Rect};
        let mut target = Target {
            path: "/overview".into(),
            selector: Some("div.a > canvas".into()),
            role_name: Some("Match confidence 90%".into()),
            component: Some(ComponentRef {
                name: "Meter".into(),
                framework: "react".into(),
            }),
            rect: Rect {
                x: 1.0,
                y: 2.0,
                w: 143.4,
                h: 92.0,
            },
            viewport: viewport(),
            styles: BTreeMap::new(),
            region: false,
            steps: Box::default(),
        };
        assert_eq!(
            describe_target(Some(&target), NoteKind::Change).unwrap(),
            "\"Match confidence 90%\" on /overview"
        );
        target.role_name = None;
        assert_eq!(
            describe_target(Some(&target), NoteKind::Change).unwrap(),
            "the Meter component on /overview"
        );
        target.region = true;
        assert_eq!(
            describe_target(Some(&target), NoteKind::Change).unwrap(),
            "an area of 143 by 92 pixels on /overview"
        );
        assert_eq!(
            describe_target(None, NoteKind::Page).as_deref(),
            Some("the whole page")
        );
        assert_eq!(describe_target(None, NoteKind::Change), None);
    }

    #[test]
    fn a_status_names_what_became_of_the_note() {
        let mut n = dnote(1);
        assert_eq!(status_of(&n, false, None), ("Done".into(), "ok"));
        n.no_change = true;
        assert_eq!(status_of(&n, false, None).0, "No change needed");
        n.status = NoteStatus::Failed;
        assert_eq!(status_of(&n, false, None), ("Failed".into(), "bad"));
        n.status = NoteStatus::Cancelled;
        assert_eq!(status_of(&n, false, None).0, "Cancelled");
        assert_eq!(status_of(&n, true, None).0, "Undone");
        assert_eq!(status_of(&n, false, Some("D3")).0, "Superseded by D3");
    }

    #[test]
    fn every_note_is_exactly_one_outcome_for_the_summary() {
        let mut n = dnote(1);
        n.commit = Some("a".repeat(40));
        assert_eq!(outcome_of(&n, false, None), Outcome::Changed);
        // Its commit may be unreadable; that does not make it a no-change note.
        n.commit = None;
        assert_eq!(outcome_of(&n, false, None), Outcome::Changed);
        assert_eq!(outcome_of(&n, true, None), Outcome::Replaced, "undone");
        assert_eq!(outcome_of(&n, false, Some("D3")), Outcome::Replaced);
        n.no_change = true;
        assert_eq!(outcome_of(&n, false, None), Outcome::NoChange);
        assert_eq!(outcome_of(&n, true, None), Outcome::Replaced);
        n.status = NoteStatus::Failed;
        assert_eq!(outcome_of(&n, false, None), Outcome::Trouble);
        n.status = NoteStatus::Cancelled;
        assert_eq!(outcome_of(&n, false, None), Outcome::Trouble);
        n.status = NoteStatus::Working;
        assert_eq!(outcome_of(&n, false, None), Outcome::Open);
    }
}
