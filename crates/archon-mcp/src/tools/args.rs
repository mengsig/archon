//! Tool arguments to daemon requests. Each field is decoded on its own so an
//! error names the argument at fault; undeclared arguments are refused with
//! the list of accepted ones, taken from the tool's schema.

use std::num::NonZeroU64;
use std::path::PathBuf;

use archon_core::cost::{CostGroup, CostQuery, DayBound};
use archon_core::error::ArchonError;
use archon_core::github::{GithubRepo, GithubThread};
use archon_core::id::QueueItemId;
use archon_core::model::{ModelPlan, ModelReasons, check_reason_lengths};
use archon_core::model_evidence::ModelEvidenceQuery;
use archon_core::protocol::{
    EnqueueRequest, ItemPatch, ItemQuery, PauseTarget, RepoAdd, RepoRef, Request, StageQuery,
};
use archon_core::queue::QueueState;
use archon_core::repo::Repo;
use serde::de::DeserializeOwned;
use serde_json::{Map, Value};

use super::{ToolDefinition, ToolError, definition};

mod initiatives;
mod lessons;
mod steer;

pub(super) fn to_request(tool: &str, arguments: Value) -> Result<Request, ToolError> {
    let Some(definition) = definition(tool) else {
        return Err(ToolError(format!("unknown tool `{tool}`")));
    };
    let mut args = Args::new(definition, arguments)?;
    let request = match tool {
        "message_orchestrator" => {
            let text = args.required_text("text")?;
            archon_core::agent_message::check_text(&text).map_err(|e| args.error("text", e))?;
            Request::MessageOrchestrator {
                text,
                kind: args.optional("kind")?.unwrap_or_default(),
                caller: None,
            }
        }
        "list_pipelines" => Request::ListPipelines {
            repo: args.optional("repo")?,
        },
        "list_repos" => Request::RepoList,
        "model_policy" => Request::ModelPolicy {
            repo: args.optional("repo")?,
        },
        "list_queue" => list_queue(&mut args)?,
        "get_item" => Request::GetItem {
            item: args.required("item")?,
        },
        "get_stage" => get_stage(&mut args)?,
        "queue_status" => Request::Status,
        "cost_report" => cost_report(&mut args)?,
        "model_evidence" => Request::ModelEvidence(ModelEvidenceQuery {
            repo: args.optional("repo")?,
            pipeline: args.optional_text("pipeline")?,
            stage: args.optional_text("stage")?,
            since: day_bound(&mut args, "since")?,
        }),
        "register_repo" => register_repo(&mut args)?,
        "enqueue" => Request::Enqueue(enqueue(&mut args)?),
        "update_item" => update_item(&mut args)?,
        "cancel_item" => Request::CancelItem {
            item: args.required("item")?,
            reason: args.optional_text("reason")?,
        },
        "retry_item" => Request::RetryItem {
            item: args.required("item")?,
            model_plan: args.optional_plan("model_plan")?,
            model_reasons: args.optional_reasons()?,
            fresh: args.optional("fresh")?.unwrap_or(false),
            brief_append: args.optional_text("brief_append")?,
        },
        "pause" => Request::Pause(pause_target(&mut args)?),
        "resume" => Request::Resume(pause_target(&mut args)?),
        "notify" => Request::Notify {
            kind: args.required("kind")?,
            text: args.required_text("text")?,
            item: args.optional("item")?,
        },
        "ask_operator" => ask_operator(&mut args)?,
        "record_chat_answer" => Request::RecordChatAnswer {
            question: args.required("question")?,
            answer: args.required_text("answer")?,
            caller: None,
        },
        "publish_report" => publish_report(&mut args)?,
        "autonomy_policy" => Request::Autonomy {
            repo: args.optional("repo")?,
        },
        "set_directive" => set_directive(&mut args)?,
        "clear_directive" => clear_directive(&mut args)?,
        "github_reply" => github_reply(&mut args)?,
        "enqueue_initiative" => initiatives::enqueue_initiative(&mut args)?,
        "update_initiative" => initiatives::update_initiative(&mut args)?,
        "steer_item" => steer::steer_item(&mut args)?,
        "record_lesson" => lessons::record_lesson(&mut args)?,
        other => unreachable!("`{other}` has a definition but no mapping"),
    };
    args.finish()?;
    Ok(request)
}

/// The arguments of one call, consumed field by field.
struct Args {
    tool: &'static str,
    fields: Map<String, Value>,
}

impl Args {
    /// Refuses anything but an object and any undeclared argument, so a
    /// misspelt optional field is reported rather than silently ignored.
    fn new(definition: &'static ToolDefinition, arguments: Value) -> Result<Self, ToolError> {
        let tool = definition.name;
        let fields = match arguments {
            Value::Null => Map::new(),
            Value::Object(fields) => fields,
            other => {
                return Err(ToolError(format!(
                    "{tool}: arguments must be a JSON object, got {}",
                    json_type(&other)
                )));
            }
        };
        let accepted: Vec<&str> = definition.properties().collect();
        let unknown: Vec<&str> = fields
            .keys()
            .map(String::as_str)
            .filter(|key| !accepted.contains(key))
            .collect();
        if !unknown.is_empty() {
            let accepted = match accepted.is_empty() {
                true => "none".to_owned(),
                false => accepted.join(", "),
            };
            return Err(ToolError(format!(
                "{tool}: unknown argument(s) {}; accepted: {accepted}",
                quoted(&unknown)
            )));
        }
        let missing: Vec<&str> = definition
            .required()
            .filter(|key| fields.get(*key).is_none_or(Value::is_null))
            .collect();
        if !missing.is_empty() {
            return Err(ToolError(format!(
                "{tool}: missing required argument(s) {}",
                quoted(&missing)
            )));
        }
        Ok(Self { tool, fields })
    }

    fn error(&self, key: &str, why: impl std::fmt::Display) -> ToolError {
        ToolError(format!("{}: `{key}` {why}", self.tool))
    }

    /// A refusal of the arguments together, not of one of them.
    fn refusal(&self, why: impl std::fmt::Display) -> ToolError {
        ToolError(format!("{}: {why}", self.tool))
    }

    /// Absent and `null` are both "not given".
    fn optional<T: DeserializeOwned>(&mut self, key: &str) -> Result<Option<T>, ToolError> {
        match self.fields.remove(key) {
            None | Some(Value::Null) => Ok(None),
            Some(value) => serde_json::from_value(value)
                .map(Some)
                .map_err(|e| self.error(key, format_args!("is invalid: {e}"))),
        }
    }

    fn required<T: DeserializeOwned>(&mut self, key: &str) -> Result<T, ToolError> {
        self.optional(key)?
            .ok_or_else(|| self.error(key, "is required"))
    }

    fn optional_text(&mut self, key: &str) -> Result<Option<String>, ToolError> {
        let Some(text) = self.optional::<String>(key)? else {
            return Ok(None);
        };
        match text.trim().is_empty() {
            true => Err(self.error(key, "must not be empty; omit it instead")),
            false => Ok(Some(text)),
        }
    }

    fn required_text(&mut self, key: &str) -> Result<String, ToolError> {
        self.optional_text(key)?
            .ok_or_else(|| self.error(key, "is required and must not be empty"))
    }

    /// A plan naming at least one stage, each with at least one choice.
    fn optional_plan(&mut self, key: &str) -> Result<Option<ModelPlan>, ToolError> {
        if let Some(field) = self.fields.get(key).and_then(undeclared_plan_field) {
            return Err(self.error(key, format_args!("has an unknown field {field}")));
        }
        let Some(plan) = self.optional::<ModelPlan>(key)? else {
            return Ok(None);
        };
        if plan.0.is_empty() {
            return Err(self.error(key, "must name every stage of the pipeline; it is empty"));
        }
        if let Some((stage, _)) = plan.0.iter().find(|(_, choices)| choices.is_empty()) {
            return Err(self.error(key, format_args!("lists no model for stage {stage:?}")));
        }
        Ok(Some(plan))
    }

    fn optional_reasons(&mut self) -> Result<Option<ModelReasons>, ToolError> {
        let reasons = self.optional::<ModelReasons>("model_reasons")?;
        if let Some(reasons) = &reasons {
            check_reason_lengths(reasons).map_err(|e| self.error("model_reasons", e))?;
        }
        Ok(reasons)
    }

    fn optional_budget(&mut self, key: &str) -> Result<Option<f64>, ToolError> {
        let budget = self.optional::<f64>(key)?;
        match budget {
            Some(usd) if !(usd.is_finite() && usd > 0.0) => Err(self.error(
                key,
                format_args!("must be a positive dollar amount, got {usd}"),
            )),
            _ => Ok(budget),
        }
    }

    fn optional_path(&mut self, key: &str) -> Result<Option<PathBuf>, ToolError> {
        let path = self.optional::<PathBuf>(key)?;
        match &path {
            Some(p) if !p.is_absolute() => Err(self.error(
                key,
                format_args!(
                    "must be an absolute path, got {:?}",
                    p.display().to_string()
                ),
            )),
            _ => Ok(path),
        }
    }

    /// A list of non-empty strings; `[]` when absent.
    fn text_list(&mut self, key: &str) -> Result<Vec<String>, ToolError> {
        let list = self.optional::<Vec<String>>(key)?.unwrap_or_default();
        match list.iter().any(|entry| entry.trim().is_empty()) {
            true => Err(self.error(key, "must not contain empty strings")),
            false => Ok(list),
        }
    }

    /// Every argument was declared (checked in `new`), so one left over is a
    /// schema property this module forgot to map: a bug, reported loudly.
    fn finish(self) -> Result<(), ToolError> {
        let Some(key) = self.fields.keys().next() else {
            return Ok(());
        };
        debug_assert!(false, "`{}` declares `{key}` but never reads it", self.tool);
        Err(ToolError(format!(
            "{}: internal error: argument `{key}` is declared but not handled",
            self.tool
        )))
    }
}

/// The first field of a model choice, or of its `{provider, model}` object,
/// that the schema does not declare; serde alone would ignore it.
fn undeclared_plan_field(plan: &Value) -> Option<String> {
    let stages = plan.as_object()?.values().filter_map(Value::as_array);
    for choice in stages.flatten().filter_map(Value::as_object) {
        if let Some(key) = choice
            .keys()
            .find(|k| !matches!(k.as_str(), "model" | "effort"))
        {
            return Some(format!("`{key}`; a choice is {{model, effort}}"));
        }
        let model = choice.get("model").and_then(Value::as_object);
        let mut keys = model.into_iter().flat_map(Map::keys);
        if let Some(key) = keys.find(|k| !matches!(k.as_str(), "provider" | "model")) {
            return Some(format!("`{key}`; a model object is {{provider, model}}"));
        }
    }
    None
}

fn register_repo(args: &mut Args) -> Result<Request, ToolError> {
    let path = args.optional_path("path")?;
    let path = path.ok_or_else(|| args.error("path", "is required"))?;
    let name = args.optional_text("name")?;
    if let Some(name) = name.as_deref().filter(|n| !Repo::valid_name(n)) {
        return Err(args.error(
            "name",
            format_args!(
                "{name:?} is not a valid repository name: use letters, digits, `-`, `_` and `.`, \
                 at most 64 characters, not starting with `.`"
            ),
        ));
    }
    Ok(Request::RepoAdd(RepoAdd {
        path,
        name,
        ship: args.optional("ship")?,
        base_branch: args.optional_text("base_branch")?,
        default_pipeline: args.optional_text("default_pipeline")?,
        auto_merge: false,
    }))
}

fn enqueue(args: &mut Args) -> Result<EnqueueRequest, ToolError> {
    let title = args.required_text("title")?;
    if title.contains('\n') {
        return Err(args.error("title", "must be a single line; put the details in `brief`"));
    }
    let model_plan = args.optional_plan("model_plan")?.unwrap_or_default();
    let request = EnqueueRequest {
        repo: args.optional("repo")?,
        pipeline: args.optional_text("pipeline")?,
        title,
        brief: args.required_text("brief")?,
        priority: args.optional("priority")?.unwrap_or_default(),
        depends_on: args.optional("depends_on")?.unwrap_or_default(),
        tags: args.text_list("tags")?,
        model_plan,
        model_reasons: args.optional_reasons()?,
        budget_usd: args.optional_budget("budget_usd")?,
        idempotency_key: args.optional_text("idempotency_key")?,
        autonomy: args.optional("autonomy")?.unwrap_or_default(),
        origin: args.optional("origin")?,
    };
    assert!(!request.title.trim().is_empty() && !request.brief.trim().is_empty());
    Ok(request)
}

fn github_reply(args: &mut Args) -> Result<Request, ToolError> {
    let repository = args.required_text("repository")?;
    let repository = GithubRepo::parse(&repository).map_err(|e| args.error("repository", e))?;
    let number: NonZeroU64 = args.required("number")?;
    let body = args.required_text("body")?;
    assert!(!body.trim().is_empty(), "required_text refuses blank text");
    Ok(Request::GithubReply {
        thread: GithubThread { repository, number },
        body,
    })
}

fn set_directive(args: &mut Args) -> Result<Request, ToolError> {
    let text = args.required_text("text")?;
    let expires_in_hours = args.optional::<u32>("expires_in_hours")?;
    if expires_in_hours.is_some_and(|hours| !(1..=720).contains(&hours)) {
        return Err(args.error("expires_in_hours", "must be between 1 and 720"));
    }
    assert!(!text.trim().is_empty(), "required_text refuses blank text");
    Ok(Request::SetDirective {
        text,
        expires_in_hours,
    })
}

/// One directive by id, or every one with `all`; never both or neither, so
/// a forgotten id cannot wipe them all.
fn clear_directive(args: &mut Args) -> Result<Request, ToolError> {
    let directive = args.optional("directive")?;
    let all = args.optional::<bool>("all")?.unwrap_or(false);
    match (directive, all) {
        (Some(_), true) => Err(args.error("all", "must be omitted when `directive` names one")),
        (None, false) => Err(ToolError(
            "clear_directive: pass `directive` (a `dir_…` id) or `all: true`".into(),
        )),
        (directive, _) => Ok(Request::ClearDirective { directive }),
    }
}

fn cost_report(args: &mut Args) -> Result<Request, ToolError> {
    let since = day_bound(args, "since")?;
    let until = day_bound(args, "until")?;
    let by = args.optional::<CostGroup>("by")?.unwrap_or_default();
    Ok(Request::CostReport(CostQuery { since, until, by }))
}

fn day_bound(args: &mut Args, key: &str) -> Result<Option<DayBound>, ToolError> {
    let Some(text) = args.optional_text(key)? else {
        return Ok(None);
    };
    match DayBound::parse(&text) {
        Ok(bound) => Ok(Some(bound)),
        Err(ArchonError::Invalid(why)) => Err(args.error(key, format_args!("is invalid: {why}"))),
        Err(other) => Err(args.error(key, other)),
    }
}

fn list_queue(args: &mut Args) -> Result<Request, ToolError> {
    let states = args.text_list("states")?;
    if let Some(bad) = states
        .iter()
        .find(|s| !QueueState::KINDS.contains(&s.as_str()))
    {
        return Err(args.error(
            "states",
            format_args!(
                "contains {bad:?}; expected any of {}",
                QueueState::KINDS.join(", ")
            ),
        ));
    }
    let limit = args.optional::<usize>("limit")?;
    if limit == Some(0) {
        return Err(args.error("limit", "must be at least 1; omit it for no limit"));
    }
    Ok(Request::ListItemSummaries(ItemQuery {
        states,
        repo: args.optional("repo")?,
        pipeline: args.optional_text("pipeline")?,
        tag: args.optional_text("tag")?,
        limit,
    }))
}

fn get_stage(args: &mut Args) -> Result<Request, ToolError> {
    let query = StageQuery {
        item: args.required("item")?,
        run: args.optional("run")?,
        stage: args.optional_text("stage")?,
        attempt: args.optional("attempt")?,
        slot: args.optional("slot")?,
        offset: args.optional("offset")?.unwrap_or(0),
        limit: args.optional("limit")?,
    };
    query
        .check()
        .map_err(|why| ToolError(format!("{}: {why}", args.tool)))?;
    assert!(
        query.stage.is_some() || query.offset == 0,
        "check refused it"
    );
    Ok(Request::GetStage(query))
}

fn update_item(args: &mut Args) -> Result<Request, ToolError> {
    let item: QueueItemId = args.required("item")?;
    let tags = args.optional::<Vec<String>>("tags")?;
    if tags.iter().flatten().any(|tag| tag.trim().is_empty()) {
        return Err(args.error("tags", "must not contain empty strings"));
    }
    let patch = ItemPatch {
        priority: args.optional("priority")?,
        model_plan: args.optional_plan("model_plan")?,
        model_reasons: args.optional_reasons()?,
        tags,
        add_depends_on: args.optional("add_depends_on")?.unwrap_or_default(),
        budget_usd: args.optional_budget("budget_usd")?,
        brief_append: args.optional_text("brief_append")?,
    };
    if patch == ItemPatch::default() {
        return Err(ToolError(
            "update_item: nothing to change; pass at least one of priority, model_plan, model_reasons, tags, \
             add_depends_on, budget_usd, brief_append"
                .into(),
        ));
    }
    Ok(Request::UpdateItem { item, patch })
}

fn pause_target(args: &mut Args) -> Result<PauseTarget, ToolError> {
    let scope: String = args.required("scope")?;
    let name = args.optional_text("name")?;
    match (scope.as_str(), name) {
        ("global", None) => Ok(PauseTarget::Global { hard: false }),
        ("global", Some(_)) => Err(args.error("name", "must be omitted when scope is `global`")),
        ("repo", Some(name)) => Ok(PauseTarget::Repo {
            name: RepoRef(name),
        }),
        ("pipeline", Some(name)) => Ok(PauseTarget::Pipeline { name }),
        ("repo" | "pipeline", None) => {
            Err(args.error("name", format_args!("is required when scope is `{scope}`")))
        }
        (other, _) => Err(args.error(
            "scope",
            format_args!("is {other:?}; expected one of global, repo, pipeline"),
        )),
    }
}

fn ask_operator(args: &mut Args) -> Result<Request, ToolError> {
    Ok(Request::AskOperator {
        text: args.required_text("text")?,
        item: args.optional("item")?,
        options: args.text_list("options")?,
        caller: None,
    })
}

fn publish_report(args: &mut Args) -> Result<Request, ToolError> {
    let title = args.required_text("title")?;
    let path = args.optional_path("path")?;
    let path = path.ok_or_else(|| args.error("path", "is required"))?;
    Ok(Request::PublishReport {
        title,
        path,
        item: args.optional("item")?,
    })
}

fn json_type(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
    }
}

fn quoted(keys: &[&str]) -> String {
    keys.iter()
        .map(|k| format!("`{k}`"))
        .collect::<Vec<_>>()
        .join(", ")
}
