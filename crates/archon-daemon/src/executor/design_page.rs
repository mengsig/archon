//! The review page of a live design session's approval gate: a decision page
//! with a summary and the answers on top, one card per note (words, what was
//! pointed at, the before and after pictures, the commit and its files, the
//! status) and the full brief folded at the end. Self-contained HTML: the
//! pictures are `data:` images, no script, no network.

use archon_core::intent::{ApprovedIntent, RequirementKind};
use base64::{Engine, engine::general_purpose::STANDARD};

use archon_core::design::IMAGE_CAP_WARN_MARGIN;

/// The most reference images an intent holds (`ApprovedIntent::validate`).
pub(crate) const MAX_IMAGES: usize = 128;

/// The most the inlined pictures may add to the page. The page is preserved
/// with the approved intent, whose files stop at 32 MiB; pictures past this
/// budget are left out of the page (never out of the intent) and say so.
pub(crate) const PICTURE_BUDGET: usize = 24 * 1024 * 1024;

/// A picture on the page.
pub(crate) struct Pic {
    /// What it is called in alt text.
    pub label: String,
    /// Where it came from.
    pub caption: String,
    pub bytes: Vec<u8>,
}

/// A note's commit, as `git` reports it.
pub(crate) struct CommitFacts {
    pub short: String,
    pub subject: String,
    pub files: Vec<String>,
}

/// One note's card.
pub(crate) struct Card {
    pub id: String,
    pub text: String,
    /// `(note id, its words)` of each note this one superseded.
    pub earlier: Vec<(String, String)>,
    /// The requirement kind the intent records for it; `None` when it is no
    /// requirement (a note that changed nothing, failed or was withdrawn).
    pub kind: Option<RequirementKind>,
    /// `(label, css class)`.
    pub status: (String, &'static str),
    /// The element or area the operator pointed at.
    pub target: Option<String>,
    /// The agent's last reply on the note.
    pub reply: Option<String>,
    pub commit: Option<CommitFacts>,
    /// Why the commit's message and files are missing, if they are.
    pub commit_note: Option<String>,
    pub before: Option<Pic>,
    pub after: Option<Pic>,
    /// The operator chose, in the finish step, to leave the picture out.
    pub withheld: bool,
}

/// Everything the page shows.
pub(crate) struct Page<'a> {
    pub title: &'a str,
    pub brief: &'a str,
    pub draft: &'a ApprovedIntent,
    /// What the design stage's agent reported.
    pub agent_summary: &'a str,
    /// The checks the agent says it ran, each with its result.
    pub checks: &'a [String],
    pub cards: &'a [Card],
}

pub(crate) fn escape(text: &str) -> String {
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

const STYLE: &str = "\
:root{color-scheme:light dark}\
body{font:16px/1.5 system-ui,sans-serif;max-width:64rem;margin:0 auto;padding:1rem}\
h1{font-size:1.5rem;margin:.5rem 0}h2{font-size:1.2rem;margin:1.5rem 0 .5rem}\
.summary{border:1px solid #8886;border-radius:8px;padding:1rem;background:#8881}\
.answers{margin:.75rem 0 0;padding-left:1.25rem}\
.warn{background:#fe9;padding:.5rem 1rem;border-radius:4px;color:#000;margin:.75rem 0}\
.card{border:1px solid #8886;border-radius:8px;padding:1rem;margin:1rem 0}\
.head{display:flex;flex-wrap:wrap;gap:.5rem;align-items:center}\
.id{font-weight:700;font-size:1.1rem}\
.chip{border-radius:999px;padding:.05rem .6rem;font-size:.85rem;border:1px solid #8888}\
.ok{background:#cfe9d4;color:#0a3a14}.bad{background:#f6c9c9;color:#5a0b0b}\
.idle{background:#e4e4e4;color:#222}.kind{background:#dbe5fb;color:#112a66}\
.said{margin:.5rem 0;font-size:1.05rem}.meta{opacity:.8;margin:.25rem 0;font-size:.9rem}\
.pair{display:grid;grid-template-columns:repeat(auto-fit,minmax(15rem,1fr));gap:.75rem;margin:.75rem 0}\
figure{margin:0}figure img{display:block;width:100%;height:auto;border:1px solid #8886;border-radius:4px}\
figcaption{font-size:.85rem;margin-top:.25rem}\
.missing{border:1px dashed #8886;border-radius:4px;padding:1rem;font-size:.9rem}\
code{font:.9em ui-monospace,monospace;overflow-wrap:anywhere}\
ul.files{margin:.25rem 0;padding-left:1.25rem}\
details{margin:1rem 0;border:1px solid #8886;border-radius:8px;padding:.5rem 1rem}\
summary{cursor:pointer;font-weight:600}pre{white-space:pre-wrap;overflow-wrap:anywhere}\
@media(max-width:30rem){body{padding:.5rem}.card{padding:.75rem}}";

/// The inlined pictures counted against [`PICTURE_BUDGET`].
struct Budget {
    limit: usize,
    used: usize,
    left_out: usize,
}

impl Budget {
    fn figure(&mut self, pic: &Pic, role: &str) -> String {
        let encoded = pic.bytes.len().div_ceil(3) * 4;
        if self.used + encoded > self.limit {
            self.left_out += 1;
            return format!(
                "<div class=\"missing\">{role} picture left out to keep this page small; it is \
                 still part of the approved intent. {}</div>",
                escape(&pic.caption)
            );
        }
        self.used += encoded;
        format!(
            "<figure><img alt=\"{}\" src=\"data:{};base64,{}\"><figcaption><strong>{role}</strong> \
             {}</figcaption></figure>",
            escape(&pic.label),
            mime_of(&pic.bytes),
            STANDARD.encode(&pic.bytes),
            escape(&pic.caption)
        )
    }
}

fn kind_label(kind: &RequirementKind) -> &'static str {
    match kind {
        RequirementKind::Visual => "Visual",
        RequirementKind::Behaviour => "Behaviour",
        RequirementKind::Wording => "Wording",
        RequirementKind::Data => "Data",
    }
}

fn pictures(card: &Card, budget: &mut Budget) -> String {
    if card.withheld {
        return "<p class=\"missing\">Picture withheld by the operator in the finish step.</p>"
            .into();
    }
    if card.before.is_none() && card.after.is_none() {
        return "<p class=\"missing\">No picture was captured for this note.</p>".into();
    }
    let side = |pic: &Option<Pic>, role: &str, budget: &mut Budget| match pic {
        Some(pic) => budget.figure(pic, role),
        None => format!(
            "<div class=\"missing\">No {} picture.</div>",
            role.to_lowercase()
        ),
    };
    let before = side(&card.before, "Before", budget);
    let after = side(&card.after, "After", budget);
    format!("<div class=\"pair\">{before}{after}</div>")
}

fn commit_block(card: &Card) -> String {
    match (&card.commit, &card.commit_note) {
        (Some(commit), _) => {
            let files: String = commit
                .files
                .iter()
                .map(|f| format!("<li><code>{}</code></li>", escape(f)))
                .collect();
            format!(
                "<p class=\"meta\">Commit <code>{}</code> {}</p>\
                 <p class=\"meta\">{} file{} changed</p><ul class=\"files\">{files}</ul>",
                escape(&commit.short),
                escape(&commit.subject),
                commit.files.len(),
                if commit.files.len() == 1 { "" } else { "s" }
            )
        }
        (None, Some(why)) => format!("<p class=\"meta\">{}</p>", escape(why)),
        (None, None) => String::new(),
    }
}

fn card_html(card: &Card, budget: &mut Budget) -> String {
    let mut html = format!(
        "<section class=\"card\" id=\"{id}\"><div class=\"head\"><span class=\"id\">{id}</span>\
         <span class=\"chip {}\">{}</span>",
        card.status.1,
        escape(&card.status.0),
        id = escape(&card.id)
    );
    if let Some(kind) = &card.kind {
        html.push_str(&format!(
            "<span class=\"chip kind\">{}</span>",
            kind_label(kind)
        ));
    }
    html.push_str(&format!(
        "</div><p class=\"said\">{}</p>",
        escape(&card.text).replace('\n', "<br>")
    ));
    for (earlier, said) in &card.earlier {
        html.push_str(&format!(
            "<p class=\"meta\">Replaces {} \"{}\"</p>",
            escape(earlier),
            escape(said.trim())
        ));
    }
    if let Some(target) = &card.target {
        html.push_str(&format!(
            "<p class=\"meta\">Pointed at: {}</p>",
            escape(target)
        ));
    }
    html.push_str(&pictures(card, budget));
    html.push_str(&commit_block(card));
    if let Some(reply) = &card.reply {
        html.push_str(&format!(
            "<p class=\"meta\">Agent: {}</p>",
            escape(reply).replace('\n', "<br>")
        ));
    }
    html.push_str("</section>");
    html
}

fn summary_html(page: &Page) -> String {
    let changed = page.cards.iter().filter(|c| c.commit.is_some()).count();
    let pictured = page.cards.iter().filter(|c| c.after.is_some()).count();
    let unchanged = page
        .cards
        .iter()
        .filter(|c| c.commit.is_none() && c.status.1 == "ok")
        .count();
    let trouble = page.cards.iter().filter(|c| c.status.1 == "bad").count();
    let checks = if page.checks.is_empty() {
        "The agent reported no checks.".to_owned()
    } else {
        format!("Checks the agent ran: {}.", escape(&page.checks.join("; ")))
    };
    let mut text = format!(
        "{} note{} in this session: {changed} changed the app ({pictured} with an after \
         picture), {unchanged} needed no change, {trouble} failed or were withdrawn. {checks} \
         Approving records {} requirement{} and {} picture{} as the approved intent that the \
         finished work is checked against.",
        page.cards.len(),
        if page.cards.len() == 1 { "" } else { "s" },
        page.draft.requirements.len(),
        if page.draft.requirements.len() == 1 {
            ""
        } else {
            "s"
        },
        page.draft.images.len(),
        if page.draft.images.len() == 1 {
            ""
        } else {
            "s"
        },
    );
    if !page.agent_summary.trim().is_empty() {
        text.push_str(&format!(
            "</p><p class=\"meta\">The agent says: {}",
            escape(page.agent_summary.trim())
        ));
    }
    format!(
        "<div class=\"summary\"><p>{text}</p><ul class=\"answers\">\
         <li><strong>yes</strong>: approve (words after it go along)</li>\
         <li><strong>reject</strong>: cancel the item</li>\
         <li>Anything else is feedback: the design is redone with it</li></ul></div>"
    )
}

/// The page. Warns when the intent nears its image cap or when pictures were
/// left out of the page to keep it under [`PICTURE_BUDGET`].
pub(crate) fn render(page: &Page) -> String {
    render_within(page, PICTURE_BUDGET)
}

fn render_within(page: &Page, limit: usize) -> String {
    assert!(
        page.draft.images.len() <= MAX_IMAGES,
        "an intent never holds more than {MAX_IMAGES} images"
    );
    let mut budget = Budget {
        limit,
        used: 0,
        left_out: 0,
    };
    let cards: String = page
        .cards
        .iter()
        .map(|c| card_html(c, &mut budget))
        .collect();
    let mut warnings = String::new();
    if page.draft.images.len() + IMAGE_CAP_WARN_MARGIN as usize >= MAX_IMAGES {
        warnings.push_str(&format!(
            "<p class=\"warn\">{} of {MAX_IMAGES} reference images used. The limit caps the \
             number of notes that can carry a picture.</p>",
            page.draft.images.len()
        ));
    }
    if budget.left_out > 0 {
        warnings.push_str(&format!(
            "<p class=\"warn\">{} picture{} left out of this page to keep it under {} MiB. They \
             are still part of the approved intent.</p>",
            budget.left_out,
            if budget.left_out == 1 { "" } else { "s" },
            limit / (1024 * 1024)
        ));
    }
    format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\
         <title>Design review</title><style>{STYLE}</style></head><body>\
         <h1>Design review: {}</h1>{}{warnings}<h2>Notes</h2>{cards}\
         <details><summary>The full brief</summary><pre>{}</pre></details>\
         <details><summary>Everything recorded as the operator's words</summary>\
         <pre>{}</pre></details></body></html>",
        escape(page.title),
        summary_html(page),
        escape(page.brief),
        escape(&page.draft.operator_words)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use archon_core::intent::{IntentSource, Requirement};

    fn pic(role: &str, fill: u8, len: usize) -> Pic {
        let mut bytes = vec![0x89, b'P', b'N', b'G'];
        bytes.resize(len, fill);
        Pic {
            label: format!("{role} picture"),
            caption: "captured from the preview at commit 3f9c2a1".into(),
            bytes,
        }
    }

    fn card(id: &str, text: &str) -> Card {
        Card {
            id: id.into(),
            text: text.into(),
            earlier: Vec::new(),
            kind: Some(RequirementKind::Visual),
            status: ("Done".into(), "ok"),
            target: Some("\"Save\" on /home".into()),
            reply: None,
            commit: Some(CommitFacts {
                short: "293769a".into(),
                subject: "Restyle the dossier".into(),
                files: vec!["src/App.tsx".into(), "src/index.css".into()],
            }),
            commit_note: None,
            before: Some(pic("Before", 1, 40)),
            after: Some(pic("After", 2, 40)),
            withheld: false,
        }
    }

    fn draft(kinds: &[(&str, RequirementKind)]) -> ApprovedIntent {
        ApprovedIntent {
            id: "design".into(),
            version: 1,
            operator_words: "make it <red>".into(),
            source: IntentSource::Gate {
                item: "design".into(),
                gate: "design:1".into(),
            },
            artifacts: Vec::new(),
            images: Vec::new(),
            requirements: kinds
                .iter()
                .map(|(id, kind)| Requirement {
                    id: (*id).into(),
                    text: "t".into(),
                    kind: kind.clone(),
                    images: Vec::new(),
                    owner: "operator".into(),
                })
                .collect(),
        }
    }

    fn page_of(cards: &[Card], limit: usize) -> String {
        let draft = draft(&[("D1", RequirementKind::Visual)]);
        render_within(
            &Page {
                title: "Console polish",
                brief: "The whole brief & more",
                draft: &draft,
                agent_summary: "Two commits.",
                checks: &["npm test (pass)".to_owned()],
                cards,
            },
            limit,
        )
    }

    #[test]
    fn a_captured_note_shows_both_pictures_its_commit_and_its_files() {
        let page = page_of(&[card("D1", "make it <red>")], PICTURE_BUDGET);
        assert_eq!(page.matches("data:image/png;base64,").count(), 2);
        assert!(
            page.contains("<strong>Before</strong>") && page.contains("<strong>After</strong>")
        );
        assert!(page.contains("make it &lt;red&gt;"));
        assert!(page.contains("<code>293769a</code> Restyle the dossier"));
        assert!(page.contains("2 files changed") && page.contains("src/index.css"));
        assert!(page.contains("Pointed at: &quot;Save&quot; on /home"));
        assert!(!page.contains("withheld"), "nothing was withheld");
        assert!(!page.contains("class=\"warn\""));
    }

    #[test]
    fn a_picture_is_called_withheld_only_when_the_operator_withheld_it() {
        let mut withheld = card("D2", "wider");
        withheld.withheld = true;
        withheld.after = None;
        let mut none = card("D3", "taller");
        none.before = None;
        none.after = None;
        let page = page_of(&[card("D1", "a"), withheld, none], PICTURE_BUDGET);
        assert_eq!(page.matches("withheld by the operator").count(), 1);
        assert_eq!(page.matches("No picture was captured").count(), 1);
        assert_eq!(page.matches("data:image/png").count(), 2);
    }

    #[test]
    fn the_kind_shown_is_the_requirements_and_a_card_without_one_shows_none() {
        let mut behaviour = card("D2", "retry");
        behaviour.kind = Some(RequirementKind::Behaviour);
        let mut unchanged = card("D3", "nothing");
        unchanged.kind = None;
        let page = page_of(&[card("D1", "a"), behaviour, unchanged], PICTURE_BUDGET);
        assert_eq!(page.matches("chip kind\">Visual<").count(), 1);
        assert_eq!(page.matches("chip kind\">Behaviour<").count(), 1);
        assert_eq!(page.matches("chip kind").count(), 2);
    }

    #[test]
    fn the_brief_is_folded_at_the_end_and_the_summary_comes_first() {
        let page = page_of(&[card("D1", "a")], PICTURE_BUDGET);
        let brief = page
            .find("<details><summary>The full brief</summary>")
            .unwrap();
        assert!(!page[brief..].starts_with("<details open"));
        assert!(page.find("class=\"summary\"").unwrap() < page.find("class=\"card\"").unwrap());
        assert!(page.find("class=\"card\"").unwrap() < brief);
        assert!(page.contains("The whole brief &amp; more"));
        assert!(
            page[..brief].find("The whole brief").is_none(),
            "the brief is not at the top"
        );
        let summary =
            &page[page.find("class=\"summary\"").unwrap()..page.find("<h2>Notes").unwrap()];
        for needle in [
            "1 note",
            "npm test (pass)",
            "Approving records 1 requirement",
            "reject",
        ] {
            assert!(summary.contains(needle), "{needle}: {summary}");
        }
    }

    #[test]
    fn pictures_past_the_budget_are_left_out_with_a_warning() {
        let big = |id: &str| {
            let mut c = card(id, "x");
            c.before = None;
            c.after = Some(pic("After", 7, 3000));
            c
        };
        let page = page_of(&[big("D1"), big("D2"), big("D3")], 7000);
        assert_eq!(page.matches("data:image/png").count(), 1);
        assert_eq!(page.matches("left out to keep this page small").count(), 2);
        assert!(page.contains("class=\"warn\">2 pictures left out"));
    }

    #[test]
    fn the_page_is_self_contained_and_fits_a_phone() {
        let page = page_of(&[card("D1", "a")], PICTURE_BUDGET);
        assert!(page.contains("name=\"viewport\" content=\"width=device-width"));
        assert!(
            !page.contains("<script") && !page.contains("http://") && !page.contains("https://")
        );
        assert!(!page.contains("src=\"/") && !page.contains("url("));
    }
}
