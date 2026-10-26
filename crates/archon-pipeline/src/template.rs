//! `{{placeholder}}` substitution for stage prompts (ARCHITECTURE §8.4).
//! Literal replacement: no logic, no escaping; `{{{{` yields `{{`.

use std::collections::BTreeMap;
use std::path::Path;

use archon_core::error::ArchonError;
use archon_core::id::{PipelineRunId, QueueItemId};
use archon_core::repo::ShipMode;

/// Fixed variable names; `previous.<stage-id>.summary` is also accepted for
/// any earlier stage id.
pub const KNOWN_VARIABLES: &[&str] = &[
    "task.title",
    "task.brief",
    "repo.name",
    "repo.path",
    "workspace",
    "branch",
    "base_branch",
    "base_commit",
    "ship.mode",
    "ship.auto_merge",
    "stage.id",
    "attempt",
    "fan.index",
    "fan.count",
    "run.id",
    "item.id",
    "feedback",
];

/// Set per slot by the driver, and only known in a team stage's prompt:
/// the slot's role, every role, and every worker branch, in roles order.
pub const TEAM_VARIABLES: &[&str] = &["team.role", "team.roles", "team.branches"];

/// The subset of [`KNOWN_VARIABLES`] fixed for a whole run; the daemon
/// supplies them (see [`RunVars`]). The rest are set per spawn by the driver.
pub const RUN_VARIABLES: &[&str] = &[
    "task.title",
    "task.brief",
    "repo.name",
    "repo.path",
    "workspace",
    "branch",
    "base_branch",
    "base_commit",
    "ship.mode",
    "ship.auto_merge",
    "run.id",
    "item.id",
];

const PREVIOUS_PREFIX: &str = "previous.";
const PREVIOUS_SUFFIX: &str = ".summary";

/// The earlier stage named by `previous.<stage-id>.summary`, if `name` has
/// that shape.
pub fn previous_stage(name: &str) -> Option<&str> {
    let stage = name
        .strip_prefix(PREVIOUS_PREFIX)?
        .strip_suffix(PREVIOUS_SUFFIX)?;
    (!stage.is_empty()).then_some(stage)
}

/// The variable name for an earlier stage's summary.
pub fn previous_variable(stage_id: &str) -> String {
    format!("{PREVIOUS_PREFIX}{stage_id}{PREVIOUS_SUFFIX}")
}

/// Run-wide values, typed so the daemon cannot forget one. Absent
/// repository fields render as empty strings (§8.4).
#[derive(Debug, Clone, Copy)]
pub struct RunVars<'a> {
    pub task_title: &'a str,
    pub task_brief: &'a str,
    pub repo_name: Option<&'a str>,
    pub repo_path: Option<&'a Path>,
    pub workspace: &'a Path,
    pub branch: &'a str,
    pub base_branch: &'a str,
    /// Where the branch forked from `base_branch`, the newer of its local and
    /// `origin` refs; the base's name when no fork is known.
    pub base_commit: &'a str,
    pub ship_mode: ShipMode,
    pub auto_merge: bool,
    pub run_id: &'a PipelineRunId,
    pub item_id: &'a QueueItemId,
}

impl RunVars<'_> {
    /// The `base_vars` map [`crate::RunDriver::new`] expects.
    pub fn to_map(&self) -> BTreeMap<String, String> {
        let ship_mode = match self.ship_mode {
            ShipMode::Pr => "pr",
            ShipMode::Local => "local",
        };
        let pairs = [
            ("task.title", self.task_title.to_owned()),
            ("task.brief", self.task_brief.to_owned()),
            ("repo.name", self.repo_name.unwrap_or_default().to_owned()),
            (
                "repo.path",
                self.repo_path.map(path_text).unwrap_or_default(),
            ),
            ("workspace", path_text(self.workspace)),
            ("branch", self.branch.to_owned()),
            ("base_branch", self.base_branch.to_owned()),
            ("base_commit", self.base_commit.to_owned()),
            ("ship.mode", ship_mode.to_owned()),
            ("ship.auto_merge", self.auto_merge.to_string()),
            ("run.id", self.run_id.to_string()),
            ("item.id", self.item_id.to_string()),
        ];
        let map: BTreeMap<String, String> =
            pairs.into_iter().map(|(k, v)| (k.to_owned(), v)).collect();
        debug_assert!(RUN_VARIABLES.iter().all(|k| map.contains_key(*k)));
        debug_assert_eq!(map.len(), RUN_VARIABLES.len());
        map
    }
}

fn path_text(path: &Path) -> String {
    path.display().to_string()
}

#[derive(Debug, PartialEq, Eq)]
enum Segment<'a> {
    Text(&'a str),
    Placeholder { name: &'a str, offset: usize },
}

/// Split a template into literal text and placeholders. `{{{{` is literal
/// `{{`; an opener without a closing `}}` is an error naming its offset.
fn segments(template: &str) -> Result<Vec<Segment<'_>>, ArchonError> {
    let mut out = Vec::new();
    let mut rest_start = 0;
    while let Some(found) = template[rest_start..].find("{{") {
        let open = rest_start + found;
        out.push(Segment::Text(&template[rest_start..open]));
        if template[open..].starts_with("{{{{") {
            out.push(Segment::Text("{{"));
            rest_start = open + 4;
            continue;
        }
        let name_start = open + 2;
        let Some(len) = template[name_start..].find("}}") else {
            return Err(ArchonError::Pipeline(format!(
                "unterminated `{{{{` at byte {open}"
            )));
        };
        let name = &template[name_start..name_start + len];
        check_placeholder_name(name, open)?;
        out.push(Segment::Placeholder { name, offset: open });
        rest_start = name_start + len + 2;
    }
    out.push(Segment::Text(&template[rest_start..]));
    debug_assert!(rest_start <= template.len());
    Ok(out)
}

fn check_placeholder_name(name: &str, offset: usize) -> Result<(), ArchonError> {
    let valid = !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_' | '-'));
    if valid {
        return Ok(());
    }
    Err(ArchonError::Pipeline(format!(
        "malformed placeholder `{{{{{name}}}}}` at byte {offset}: names use a-z, 0-9, `.`, `_` and `-` (write `{{{{{{{{` for a literal `{{{{`)"
    )))
}

/// Every placeholder name used in `template`, in order of first appearance.
pub fn placeholders(template: &str) -> Result<Vec<String>, ArchonError> {
    let mut names: Vec<String> = Vec::new();
    for segment in segments(template)? {
        if let Segment::Placeholder { name, .. } = segment
            && !names.iter().any(|n| n == name)
        {
            names.push(name.to_owned());
        }
    }
    Ok(names)
}

/// Substitute every placeholder; a name missing from `vars` is an error, so
/// a prompt can never ship with a raw `{{...}}` in it. Values are inserted
/// verbatim and never re-scanned, so a brief containing `{{` is safe.
pub fn render(template: &str, vars: &BTreeMap<String, String>) -> Result<String, ArchonError> {
    let mut out = String::with_capacity(template.len());
    for segment in segments(template)? {
        match segment {
            Segment::Text(text) => out.push_str(text),
            Segment::Placeholder { name, offset } => {
                let Some(value) = vars.get(name) else {
                    return Err(ArchonError::Pipeline(format!(
                        "unknown placeholder `{{{{{name}}}}}` at byte {offset}"
                    )));
                };
                out.push_str(value);
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    #[test]
    fn lists_placeholders_in_order_without_duplicates() {
        let names = placeholders("{{b}} and {{a.x}} then {{b}}").unwrap();
        assert_eq!(names, ["b", "a.x"]);
        assert!(placeholders("no placeholders } }} {").unwrap().is_empty());
    }

    #[test]
    fn renders_known_placeholders_literally() {
        let out = render(
            "Fix {{task.title}} in {{workspace}}.",
            &vars(&[("task.title", "bug"), ("workspace", "/w")]),
        )
        .unwrap();
        assert_eq!(out, "Fix bug in /w.");
    }

    #[test]
    fn values_are_never_rescanned() {
        let out = render("{{task.brief}}", &vars(&[("task.brief", "use {{x}} {{")])).unwrap();
        assert_eq!(out, "use {{x}} {{");
    }

    #[test]
    fn unknown_placeholder_is_an_error_naming_it_and_its_offset() {
        let err = render("ab {{nope}}", &vars(&[])).unwrap_err().to_string();
        assert!(err.contains("{{nope}}"), "{err}");
        assert!(err.contains("byte 3"), "{err}");
    }

    #[test]
    fn unterminated_opener_is_an_error_naming_the_offset() {
        let err = placeholders("hello {{task.title").unwrap_err().to_string();
        assert!(err.contains("unterminated"), "{err}");
        assert!(err.contains("byte 6"), "{err}");
        assert!(render("x {{", &vars(&[])).is_err());
    }

    #[test]
    fn quadruple_brace_is_a_literal_double_brace() {
        assert!(placeholders("{{{{not.a.var}}").unwrap().is_empty());
        let out = render("a {{{{b}} {{c}}", &vars(&[("c", "C")])).unwrap();
        assert_eq!(out, "a {{b}} C");
        assert_eq!(render("{{{{{{{{", &vars(&[])).unwrap(), "{{{{");
    }

    #[test]
    fn malformed_names_are_rejected() {
        for bad in ["{{}}", "{{ task.title }}", "{{Task}}", "{{a b}}"] {
            let err = placeholders(bad).unwrap_err().to_string();
            assert!(err.contains("malformed placeholder"), "{bad}: {err}");
        }
    }

    #[test]
    fn previous_stage_names_round_trip() {
        assert_eq!(previous_stage(&previous_variable("review")), Some("review"));
        assert_eq!(previous_stage("previous..summary"), None);
        assert_eq!(previous_stage("feedback"), None);
    }

    #[test]
    fn run_vars_cover_every_run_variable() {
        let run = PipelineRunId::new();
        let item = QueueItemId::new();
        let map = RunVars {
            task_title: "t",
            task_brief: "b",
            repo_name: None,
            repo_path: None,
            workspace: Path::new("/w"),
            branch: "archon/x",
            base_branch: "main",
            base_commit: "main",
            ship_mode: ShipMode::Local,
            auto_merge: false,
            run_id: &run,
            item_id: &item,
        }
        .to_map();
        assert_eq!(map["ship.mode"], "local");
        assert_eq!(map["repo.name"], "");
        assert_eq!(map["ship.auto_merge"], "false");
        for name in RUN_VARIABLES {
            assert!(KNOWN_VARIABLES.contains(name), "{name}");
        }
    }
}
