//! What the orchestrator reads about each tool: its description and the JSON
//! Schema of its arguments. Written for a model: when to call the tool, what
//! each field means, and which fields are required.

use archon_core::cost::{CostGroup, DEFAULT_DAYS};
use archon_core::mcp_tools::{TOOLS, ToolClass};
use archon_core::model::MAX_MODEL_REASON_CHARS;
use archon_core::protocol::{MAX_ARTIFACT_PAGE_BYTES, MAX_STAGE_PAGE_CHARS, STAGE_PAGE_CHARS};
use archon_core::queue::{NOTIFY_TAG, QueueState};
use serde_json::{Value, json};

use super::ToolDefinition;

mod initiatives;
mod lessons;
mod steer;

/// Every tool, in `TOOLS` order.
pub(super) fn all() -> Vec<ToolDefinition> {
    vec![
        list_pipelines(),
        list_repos(),
        model_policy(),
        list_queue(),
        get_item(),
        get_stage(),
        queue_status(),
        cost_report(),
        model_evidence(),
        register_repo(),
        enqueue(),
        update_item(),
        cancel_item(),
        retry_item(),
        pause(),
        resume(),
        notify(),
        ask_operator(),
        record_chat_answer(),
        publish_report(),
        autonomy_policy(),
        set_directive(),
        clear_directive(),
        github_reply(),
        message_orchestrator(),
        initiatives::enqueue_initiative(),
        initiatives::update_initiative(),
        steer::steer_item(),
        lessons::record_lesson(),
        refresh_item(),
        publish_artifact(),
        list_artifacts(),
        read_artifact(),
    ]
}

fn message_orchestrator() -> ToolDefinition {
    define(
        "message_orchestrator",
        "Message orchestrator",
        "Send the orchestrator a message, recorded with your item and stage: `relay` for an \
         operator message meant for it, `escalation` for a blocker outside your stage, else \
         `info`. Returns at once: keep working; never wait for a reply or end your turn \
         over it. At most 20 per agent.",
        object(
            json!({
                "text": {"type": "string", "minLength": 1, "maxLength": archon_core::agent_message::MAX_MESSAGE_CHARS},
                "kind": {"type": "string", "enum": ["relay", "escalation", "info"]}
            }),
            &["text"],
        ),
        false,
    )
}

fn publish_artifact() -> ToolDefinition {
    define(
        "publish_artifact",
        "Publish artifact",
        "Stage agents: keep a file or directory of your workspace (evidence, logs, bench \
         results) in your item's size-bounded artifact store, readable by item id after the \
         worktree is gone. A name published again is replaced.",
        object(
            json!({
                "path": {"type": "string", "minLength": 1, "description": "Inside your workspace; no symlinks."},
                "name": {"type": "string", "minLength": 1, "description": "Name in the store. Default: the path's last component."}
            }),
            &["path"],
        ),
        false,
    )
}

fn list_artifacts() -> ToolDefinition {
    define(
        "list_artifacts",
        "List artifacts",
        "The files an item published with publish_artifact: sizes and the directory they sit in.",
        object(json!({"item": item_id("")}), &["item"]),
        false,
    )
}

fn read_artifact() -> ToolDefinition {
    define(
        "read_artifact",
        "Read artifact",
        "An item's published file as UTF-8 text, a page at a time; a page that is not the last \
         gives the next `offset`.",
        object(
            json!({
                "item": item_id(""),
                "name": {"type": "string", "minLength": 1, "description": "As listed by list_artifacts."},
                "offset": {"type": "integer", "minimum": 0, "description": "Bytes to skip."},
                "limit": {"type": "integer", "minimum": 1, "maximum": MAX_ARTIFACT_PAGE_BYTES, "description": format!("Bytes per page. Default and maximum: {MAX_ARTIFACT_PAGE_BYTES}.")}
            }),
            &["item", "name"],
        ),
        false,
    )
}

fn define(
    name: &'static str,
    title: &'static str,
    description: &'static str,
    input_schema: Value,
    destructive: bool,
) -> ToolDefinition {
    let listed = TOOLS.iter().find(|t| t.name == name);
    let Some(listed) = listed else {
        panic!("tool `{name}` is not in archon_core::mcp_tools::TOOLS");
    };
    assert_eq!(
        input_schema["type"], "object",
        "`{name}` arguments are an object"
    );
    assert!(
        !(listed.read_only() && destructive),
        "`{name}` cannot be both"
    );
    ToolDefinition {
        name,
        title,
        description,
        input_schema,
        read_only: listed.read_only(),
        destructive,
        open_world: listed.class == ToolClass::External,
    }
}

/// The `brief_append` property shared by `update_item` and `retry_item`.
fn brief_append() -> Value {
    json!({"type": "string", "description": "Added to the brief, e.g. \"the reviewer found X; do Y\"; every later stage reads it."})
}

/// An object schema that refuses undeclared fields.
fn object(properties: Value, required: &[&str]) -> Value {
    let mut schema = json!({
        "type": "object",
        "properties": properties,
        "additionalProperties": false,
    });
    if !required.is_empty() {
        schema["required"] = json!(required);
    }
    schema
}

fn repo_ref(what: &str) -> Value {
    json!({"type": "string", "description": what})
}

fn item_id(what: &str) -> Value {
    let described = format!("{what} `qi_…` id.");
    json!({"type": "string", "description": described.trim_start()})
}

fn priority() -> Value {
    json!({
        "type": "string",
        "enum": ["low", "normal", "high", "critical"],
    })
}

fn budget() -> Value {
    json!({
        "type": "number",
        "exclusiveMinimum": 0,
        "description": "Dollar cap for the run. Default: the configured one.",
    })
}

fn stage_timeouts() -> Value {
    json!({"type": "object", "description": "Stage id to minutes (≤ `[limits]`).",
        "additionalProperties": {"type": "integer", "minimum": 1}})
}

fn model_reasons() -> Value {
    json!({"type": "object", "description": "Why per stage id (not pipeline).",
        "additionalProperties": {"type": "string", "maxLength": MAX_MODEL_REASON_CHARS}})
}

fn model_plan(what: &str) -> Value {
    let choice = json!({
        "type": "object",
        "properties": {
            "model": model_ref(),
            "effort": {"type": "string", "enum": ["low", "medium", "high", "x_high", "xhigh", "max"]},
        },
        "required": ["model"],
        "additionalProperties": false,
    });
    json!({
        "type": "object",
        "description": format!(
            "{what} Stage id (list_pipelines) to {{model, effort}} lists within model_policy; \
             fan-out cycles fewer choices. \
             No effort = the stage default; above it needs a model_reasons entry (prefer a stronger \
             model at default effort). `<stage>.review`: reviewers a stage delegates."
        ),
        "minProperties": 1,
        "additionalProperties": {"type": "array", "minItems": 1, "items": choice},
    })
}

/// A replacement plan, where `enqueue` beside it already gives the shape.
fn new_model_plan(what: &str) -> Value {
    json!({"type": "object", "description": format!("{what} Shaped as in enqueue.")})
}

/// Both spellings `ModelRef` accepts: the `provider/model` string and
/// Aether's `{provider, model}` object.
fn model_ref() -> Value {
    json!({
        "description": "`provider/model`, e.g. `claude-code/claude-opus-5-5`, or {provider, model}.",
        "anyOf": [
            {"type": "string", "minLength": 3},
            {
                "type": "object",
                "properties": {
                    "provider": {"type": "string", "minLength": 1},
                    "model": {"type": "string", "minLength": 1},
                },
                "required": ["provider", "model"],
                "additionalProperties": false,
            },
        ],
    })
}

fn autonomy() -> Value {
    json!({
        "type": "object",
        "description": "A knob left out takes autonomy_policy's default. `propose` and `report` stages \
            take the first stage's model unless model_plan names them.",
        "properties": {
            "review": {"type": "string", "enum": ["strict", "standard", "none"], "description": "Without `pipeline`, `direct` runs. `adaptive` and `direct` delegate 3, 1 or 0 reviewers; another pipeline's gates fix it."},
            "proposal": {"type": "string", "enum": ["off", "lavish"], "description": "lavish: a proposal page first, and the run waits for the operator's verdict on it. For an ambiguous direction or a requested plan."},
            "report": {"type": "string", "enum": ["off", "with-pr", "before-pr"], "description": "with-pr: a change report linked from the PR. before-pr: the operator approves it before the PR opens. For a diff that is hard to review alone."},
            "merge": {"type": "string", "enum": ["manual", "auto"], "description": "auto: GitHub merges the PR once its checks allow."},
            "tools": {"type": "string", "enum": ["guarded", "yolo"], "description": "yolo: stage agents may delegate and see Archon's read-only tools; nothing waits for approval."},
        },
        "additionalProperties": false,
    })
}

fn string_list(description: &str) -> Value {
    json!({
        "type": "array",
        "items": {"type": "string", "minLength": 1},
        "description": description,
    })
}

fn list_pipelines() -> ToolDefinition {
    define(
        "list_pipelines",
        "List pipelines",
        "Pipelines to queue work into, a line each; name a `pipeline` for its stages' kind, \
         roles, gate, loop target and nudge (advice, not a rule). A broken pipeline file is \
         listed as a problem.",
        object(
            json!({
                "repo": repo_ref("Add this repository's pipelines."),
                "pipeline": {"type": "string"},
            }),
            &[],
        ),
        false,
    )
}

fn list_repos() -> ToolDefinition {
    define(
        "list_repos",
        "List repositories",
        "Registered repositories: name, id, path, ship mode, base branch, default pipeline. A `repo` argument takes the name or the `repo_…` id.",
        object(json!({}), &[]),
        false,
    )
}

fn model_policy() -> ToolDefinition {
    define(
        "model_policy",
        "Show model policy",
        "What a model_plan may use, anything else being refused: the allowed models (or any \
         model Aether lists), the denied models and the maximum effort. A repository only \
         narrows the global policy.",
        object(json!({"repo": repo_ref("Its effective policy.")}), &[]),
        false,
    )
}

fn list_queue() -> ToolDefinition {
    define(
        "list_queue",
        "List queue items",
        "Queue items: state, pipeline, priority, and why one waits, is blocked, failed or \
         cancelled. Filters combine with AND.",
        object(
            json!({
                "states": {
                    "type": "array",
                    "items": {
                        "type": "string",
                        "enum": QueueState::KINDS,
                    },
                },
                "repo": {"type": "string"},
                "pipeline": {"type": "string"},
                "tag": {"type": "string"},
                "limit": {"type": "integer", "minimum": 1},
            }),
            &[],
        ),
        false,
    )
}

fn get_item() -> ToolDefinition {
    define(
        "get_item",
        "Get queue item",
        "One item in full: state, brief, model plan, runs (branch, worktree), stage attempts \
         (model, verdict, summary, cost, with the split of a stage that delegated) and spend. \
         Use it to explain an outcome, find the PR or commit, or decide a retry. Long \
         summaries are clipped: get_stage reads them whole.",
        object(json!({"item": item_id("")}), &["item"]),
        false,
    )
}

fn get_stage() -> ToolDefinition {
    define(
        "get_stage",
        "Get stage output",
        "What an item's stages reported, unclipped, for its latest run unless `run`. \
         Without `stage` it lists the stage runs; with it, the error, whole summary and \
         structured result (a reviewer's findings with paths and lines) of the latest attempt \
         and every fan-out slot unless `attempt` or `slot` is given. A page that is not the \
         last gives the next `offset`.",
        object(
            json!({
                "item": item_id(""),
                "run": {"type": "string", "description": "`prun_…` id from get_item; attempts restart at 1 in each run."},
                "stage": {"type": "string", "minLength": 1},
                "attempt": {"type": "integer", "minimum": 1},
                "slot": {"type": "integer", "minimum": 0, "description": "From 0."},
                "offset": {"type": "integer", "minimum": 0, "description": "Characters to skip."},
                "limit": {"type": "integer", "minimum": 1, "maximum": MAX_STAGE_PAGE_CHARS, "description": format!("Characters per stage run and page; default {STAGE_PAGE_CHARS}.")},
            }),
            &["item"],
        ),
        false,
    )
}

fn queue_status() -> ToolDefinition {
    define(
        "queue_status",
        "Queue status",
        "Daemon overview: Aether link, orchestrator, pauses, agents against the caps, queue \
         counts, open questions, spend against the budgets, an upgrade under way (`/upgrade`), \
         differing versions.",
        object(json!({}), &[]),
        false,
    )
}

fn cost_report() -> ToolDefinition {
    let groups: Vec<&str> = CostGroup::ALL.iter().map(|g| g.name()).collect();
    define(
        "cost_report",
        "Spend report",
        "Spend by local day as budgets count it, grouped `by`, with a total. \
         `(none)` is spend with no such owner: the orchestrator's, and agents outside a stage.",
        object(
            json!({
                "since": {"type": "string", "minLength": 1, "description": format!("First day: `YYYY-MM-DD`, or days back as `7d` (the last 7 days, with today) or `2w`. Default: {DEFAULT_DAYS}d.")},
                "until": {"type": "string", "minLength": 1, "description": "Last day likewise; `1d` = yesterday. Default: today."},
                "by": {"type": "string", "enum": groups, "description": "Default: day."},
            }),
            &[],
        ),
        false,
    )
}

fn model_evidence() -> ToolDefinition {
    define(
        "model_evidence",
        "Model evidence",
        "What stored stage attempts cost and how they fared, by stage and (model, effort) for comparison: sample \
         size, median/p90 cost, minutes, pass share of review and test verdicts (max 60 groups). \
         Cost is what budgets count, delegated agents included; fan-out slots count apart.",
        object(
            json!({
                "repo": {"type": "string"},
                "pipeline": {"type": "string", "minLength": 1},
                "stage": {"type": "string", "minLength": 1},
                "since": {"type": "string", "minLength": 1, "description": "Attempts started since this local day: `YYYY-MM-DD`, `7d` or `2w`. Default: 30d."}
            }),
            &[],
        ),
        false,
    )
}

fn register_repo() -> ToolDefinition {
    define(
        "register_repo",
        "Register repository",
        "Register a git repository so work can be queued against it. Only the operator sets \
         merge=auto or GitHub watching (`/watch <name>` in chat).",
        object(
            json!({
                "path": {"type": "string", "description": "Absolute path of its top-level directory."},
                "name": {"type": "string", "description": "Unique; max 64 of letters, digits, `-_.`. Default: the directory name."},
                "ship": {"type": "string", "enum": ["pr", "local"], "description": "`pr` (default): push, open a pull request. `local`: commit on a local branch, never push."},
                "base_branch": {"type": "string", "description": "New work forks from it. Default: the checked-out HEAD commit."},
                "default_pipeline": {"type": "string"},
            }),
            &["path"],
        ),
        false,
    )
}

fn enqueue() -> ToolDefinition {
    define(
        "enqueue",
        "Enqueue work",
        "Queue one unit of work; returns its item (`qi_…`) at once. The daemon runs it through \
         `pipeline` in a fresh worktree and branch when dependencies, caps and budgets allow. \
         item_finished or items_finished reports it: do not poll.",
        object(
            json!({
                "repo": repo_ref("Left out only for a pipeline with requires_repo = false (`ask`)."),
                "pipeline": {"type": "string", "description": "Default: `direct` if `autonomy.review` is set, else the repository's, else `direct`."},
                "title": {"type": "string", "description": "One line; names the branch too."},
                "brief": {"type": "string", "description": "All the stage agents get besides the repository: the request's own words, then goal, context, constraints, acceptance criteria."},
                "priority": priority(),
                "depends_on": string_list("Items (`qi_…`) that must succeed first, else this one is blocked. Its agents are told their branches and pull requests."),
                "tags": string_list(&format!("Labels for list_queue. `{NOTIFY_TAG}` wakes you when the item succeeds too.")),
                "model_reasons": model_reasons(),
                "model_plan": model_plan("Model and effort for the stages you have a reason to choose; Archon fills the rest."),
                "budget_usd": budget(),
                "stage_timeouts": stage_timeouts(),
                "idempotency_key": {"type": "string", "description": "A repeat with the same key and arguments returns the first item."},
                "autonomy": autonomy(),
                "origin": {
                    "type": "object",
                    "description": "The GitHub thread that asked, from its github trigger; item_finished names it for github_reply.",
                    "properties": {
                        "repository": {"type": "string", "description": "`owner/name`."},
                        "number": {"type": "integer", "minimum": 1},
                    },
                    "required": ["repository", "number"],
                    "additionalProperties": false,
                },
            }),
            &["title", "brief"],
        ),
        false,
    )
}

fn update_item() -> ToolDefinition {
    define(
        "update_item",
        "Update queue item",
        "Change an item that has not succeeded. On a running item `model_plan` and \
         `brief_append` are steered into its run without cancelling it; while it is just \
         starting they are refused: resend shortly.",
        object(
            json!({
                "item": item_id(""),
                "priority": priority(),
                "model_reasons": model_reasons(),
                "model_plan": new_model_plan("Replaces the whole plan."),
                "tags": string_list("Replaces the tags."),
                "add_depends_on": string_list("More items that must succeed first."),
                "budget_usd": budget(),
                "stage_timeouts": stage_timeouts(),
                "brief_append": brief_append(),
            }),
            &["item"],
        ),
        false,
    )
}

fn cancel_item() -> ToolDefinition {
    define(
        "cancel_item",
        "Cancel queue item",
        "Cancel an item: its agents stop and its run ends; dependents are blocked. Only \
         retry_item brings it back.",
        object(
            json!({"item": item_id(""), "reason": {"type": "string"}}),
            &["item"],
        ),
        true,
    )
}

fn retry_item() -> ToolDefinition {
    define(
        "retry_item",
        "Retry queue item",
        "Queue a finished item again from its first stage. The run continues in the item's \
         worktree and branch unless `fresh`. A succeeded item's retry delivers into its pull \
         request again and counts against the run that passed: enqueue new work instead.",
        object(
            json!({
                "item": item_id(""),
                "model_reasons": model_reasons(),
                "model_plan": new_model_plan("Replaces the plan for the new run."),
                "fresh": {"type": "boolean", "description": "New branch from the base head, when the earlier work is a dead end or the base moved on. The old branch stays and its worktree, uncommitted work included, is set aside until worktree gc. A new pull request is opened and the old one stays open: close it with a note."},
                "brief_append": brief_append(),
            }),
            &["item"],
        ),
        false,
    )
}

fn refresh_item() -> ToolDefinition {
    define(
        "refresh_item",
        "Refresh item onto its base",
        "Merge fresh base into a stale or conflicting item's branch and same pull request. \
         Review reads from the new merge base, conflict fixes included.",
        object(json!({"item": item_id("")}), &["item"]),
        false,
    )
}

fn pause_target() -> Value {
    object(
        json!({
            "scope": {"type": "string", "enum": ["global", "repo", "pipeline"]},
            "name": {"type": "string", "description": "The repository or pipeline; left out for `global`."},
        }),
        &["scope"],
    )
}

fn pause() -> ToolDefinition {
    define(
        "pause",
        "Pause dispatch",
        "Stop starting new work, everywhere or per repository or pipeline; running agents finish \
         their stage. resume undoes it.",
        pause_target(),
        false,
    )
}

fn resume() -> ToolDefinition {
    define(
        "resume",
        "Resume dispatch",
        "Undo a pause (same scope and name).",
        pause_target(),
        false,
    )
}

fn notify() -> ToolDefinition {
    define(
        "notify",
        "Notify operator",
        "Post a line to the operator's chat, for what your final reply does not show. `decision`: a choice of yours they should know. `blocker`: work \
         cannot go on without them. `outcome`: more than the automatic item outcome. \
         `progress`: hidden unless asked for. A question goes through ask_operator, not \
         `question`.",
        object(
            json!({
                "kind": {"type": "string", "enum": ["question", "decision", "blocker", "outcome", "progress"]},
                "text": {"type": "string"},
                "item": item_id(""),
            }),
            &["kind", "text"],
        ),
        false,
    )
}

fn ask_operator() -> ToolDefinition {
    define(
        "ask_operator",
        "Ask operator",
        "Ask the operator without blocking: it shows in their chat, and the answer comes as an \
         operator_answer trigger with the question id; do not wait or poll. Aether's \
         ask_user only when you cannot go on without the answer.",
        object(
            json!({
                "text": {"type": "string"},
                "item": item_id(""),
                "options": string_list("Suggested answers."),
            }),
            &["text"],
        ),
        false,
    )
}

fn record_chat_answer() -> ToolDefinition {
    define(
        "record_chat_answer",
        "Record chat answer",
        "Close a question you asked with ask_operator once the operator plainly answered it in \
         chat and you act on that: it leaves `archon questions`, recorded with their words. \
         Only your own question: a gate or another agent's is refused. No operator_answer trigger follows.",
        object(
            json!({
                "question": {"type": "string", "description": "`q_…` id from ask_operator."},
                "answer": {"type": "string", "description": "In the operator's words."},
            }),
            &["question", "answer"],
        ),
        false,
    )
}

fn publish_report() -> ToolDefinition {
    define(
        "publish_report",
        "Publish report",
        "Publish an HTML report file (`.html` or `.htm`; other files, Markdown included, are \
         refused) for the operator to review, and return where it is. With Lavish it opens as a \
         review session whose feedback comes as a lavish_feedback trigger.",
        object(
            json!({
                "title": {"type": "string"},
                "path": {"type": "string", "description": "Absolute path of an existing `.html`/`.htm` file."},
                "item": item_id(""),
            }),
            &["title", "path"],
        ),
        false,
    )
}

fn autonomy_policy() -> ToolDefinition {
    define(
        "autonomy_policy",
        "Show autonomy policy",
        "The autonomy defaults and ceiling, and the operator directives in force. A knob above \
         the ceiling is refused.",
        object(
            json!({"repo": repo_ref("Its effective defaults and ceiling.")}),
            &[],
        ),
        false,
    )
}

fn set_directive() -> ToolDefinition {
    define(
        "set_directive",
        "Record operator directive",
        "Record a standing instruction the operator gave you, in chat or in a GitHub request \
         with operator standing (\"propose first for anything in auth/\"): only what they \
         said, in their words. It survives restarts and shows in `archon status`. It never raises the autonomy ceiling.",
        object(
            json!({
                "text": {"type": "string", "description": "At most 500 characters."},
                "expires_in_hours": {"type": "integer", "minimum": 1, "maximum": 720, "description": "When it lapses, if the operator gave a time frame (\"today\" is about 12)."},
            }),
            &["text"],
        ),
        false,
    )
}

fn clear_directive() -> ToolDefinition {
    define(
        "clear_directive",
        "Clear operator directive",
        "Clear a directive the operator withdrew: one by its `dir_…` id (autonomy_policy lists \
         them), or every one with `all: true` when they asked for that.",
        object(
            json!({"directive": {"type": "string"}, "all": {"type": "boolean"}}),
            &[],
        ),
        false,
    )
}

fn github_reply() -> ToolDefinition {
    define(
        "github_reply",
        "Reply on GitHub",
        "Comment on an issue or pull request of a repository Archon watches; any other is \
         refused. Marked so it does not trigger the watcher. It is public and \
         cannot be taken back: no secrets, operator-only details or local paths. The operator's \
         approvals may hold it.",
        object(
            json!({
                "repository": {"type": "string", "description": "`owner/name`, as in the trigger."},
                "number": {"type": "integer", "minimum": 1},
                "body": {"type": "string", "description": "GitHub Markdown, at most 65000 characters."},
            }),
            &["repository", "number", "body"],
        ),
        false,
    )
}
