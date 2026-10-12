//! What the orchestrator reads about each tool: its description and the JSON
//! Schema of its arguments. Written for a model: when to call the tool, what
//! each field means, and which fields are required.

use archon_core::cost::{CostGroup, DEFAULT_DAYS};
use archon_core::mcp_tools::{TOOLS, ToolClass};
use archon_core::model::MAX_MODEL_REASON_CHARS;
use archon_core::protocol::{MAX_STAGE_PAGE_CHARS, STAGE_PAGE_CHARS};
use archon_core::queue::QueueState;
use serde_json::{Value, json};

use super::ToolDefinition;

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
    ]
}

fn message_orchestrator() -> ToolDefinition {
    define(
        "message_orchestrator",
        "Message orchestrator",
        "Send a non-blocking message to the orchestrator, recorded with your item and stage. Use relay for operator messages meant for the orchestrator, escalation for blockers outside your stage, or info. Returns a receipt immediately; keep working on your stage. Never wait for a reply or end your turn because you sent a message. Limited to 20 messages per agent. Requires Aether caller metadata (send_caller_meta = true).",
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
    json!({"type": "string", "description": "Guidance to add to the brief, such as \"the reviewer found X; do Y\". It is appended under a dated \"Amendment\" heading, recorded in the item's history, and read by every stage of the item's next run."})
}

/// An object schema that refuses undeclared fields.
fn object(properties: Value, required: &[&str]) -> Value {
    json!({
        "type": "object",
        "properties": properties,
        "required": required,
        "additionalProperties": false,
    })
}

fn repo_ref(what: &str) -> Value {
    json!({
        "type": "string",
        "description": format!(
            "{what} Registered repository name (as shown by list_repos) or its `repo_…` id."
        ),
    })
}

fn item_id(what: &str) -> Value {
    json!({
        "type": "string",
        "description": format!("{what} Queue item id (`qi_…`) from enqueue or list_queue."),
    })
}

fn priority() -> Value {
    json!({
        "type": "string",
        "enum": ["low", "normal", "high", "critical"],
        "description": "Dispatch order: higher priority starts first; equal priorities start oldest first.",
    })
}

fn budget() -> Value {
    json!({
        "type": "number",
        "exclusiveMinimum": 0,
        "description": "Dollar cap for this item's whole pipeline run. Omit to use the configured per-item default.",
    })
}

fn model_reasons() -> Value {
    json!({"type": "object", "description": "Optional one-line reason per stage. Unknown stage ids are refused. Replaces reasons; a new model_plan without reasons clears old ones.",
        "additionalProperties": {"type": "string", "maxLength": MAX_MODEL_REASON_CHARS}})
}

fn model_plan(what: &str) -> Value {
    let choice = json!({
        "type": "object",
        "properties": {
            "model": model_ref(),
            "effort": {
                "type": "string",
                "enum": ["low", "medium", "high", "x_high", "xhigh", "max"],
                "description": "Reasoning effort; must not exceed model_policy's max effort. \
                    `xhigh` is accepted as a spelling of `x_high`.",
            },
        },
        "required": ["model", "effort"],
        "additionalProperties": false,
    });
    json!({
        "type": "object",
        "description": format!(
            "{what} Keys are the pipeline's stage ids (see list_pipelines); every stage must be \
             listed. Each value is a list of {{model, effort}} choices: exactly one for an ordinary \
             stage; for a fan-out stage one per parallel agent (up to its fan_out; fewer are \
             cycled across the slots). Example: {{\"implement\": [{{\"model\": \
             \"claude-code/claude-opus-5-5\", \"effort\": \"high\"}}], \"ship\": [{{\"model\": \
             \"claude-code/claude-sonnet-5\", \"effort\": \"medium\"}}]}}."
        ),
        "minProperties": 1,
        "additionalProperties": {"type": "array", "minItems": 1, "items": choice},
    })
}

/// Both spellings `ModelRef` accepts: the `provider/model` string and
/// Aether's `{provider, model}` object.
fn model_ref() -> Value {
    json!({
        "description": "The model, allowed by model_policy: the exact `provider/model` id, e.g. \
            `claude-code/claude-opus-5-5`, or the same as an object {\"provider\": \
            \"claude-code\", \"model\": \"claude-opus-5-5\"}.",
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
        "description": "Autonomy knobs for this item, chosen with the decision guide in your \
            instructions; omitted knobs take the defaults autonomy_policy shows. A value above the \
            ceiling is refused with the values allowed. Example: {\"review\": \"strict\", \
            \"report\": \"with-pr\"}.",
        "properties": {
            "review": {"type": "string", "enum": ["strict", "standard", "none"], "description": "Review depth; picks the pipeline when `pipeline` is omitted: strict → strict-pr-review, standard → default-pr-review, none → quick-pr. With an explicit pipeline leave it out."},
            "proposal": {"type": "string", "enum": ["off", "lavish"], "description": "lavish: a propose stage first writes a proposal page, and the run waits until the operator approves, revises or rejects it."},
            "report": {"type": "string", "enum": ["off", "with-pr", "before-pr"], "description": "with-pr: a change report is written, published and linked from the PR, without waiting. before-pr: the same, but the operator approves it before the PR is opened."},
            "merge": {"type": "string", "enum": ["manual", "auto"], "description": "auto: the ship stage merges the PR once every gate passed. Only when the operator said so."},
            "tools": {"type": "string", "enum": ["guarded", "yolo"], "description": "yolo: the item's stage agents may delegate to subagents and also see Archon's read-only tools; nothing waits for approval. guarded is the norm."},
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
        "List the pipelines work can be enqueued into, with every stage: id, kind, fan_out (or a team stage's \
         roles), gate, loop target and per-stage nudge. Call it before enqueue: the model_plan must name every \
         stage id shown here. Pipeline nudges (tier, independent_reviewers, notes) are advice for \
         choosing models, not rules. Pass `repo` to include that repository's own pipelines and \
         overrides. Broken pipeline files are reported as problems and cannot be used.",
        object(
            json!({"repo": repo_ref("Include this repository's pipelines.")}),
            &[],
        ),
        false,
    )
}

fn list_repos() -> ToolDefinition {
    define(
        "list_repos",
        "List repositories",
        "List the registered repositories: name, id, path, ship mode, base branch and default \
         pipeline. Use a name (or id) wherever a tool takes `repo`. Register new repositories \
         with register_repo.",
        object(json!({}), &[]),
        false,
    )
}

fn model_policy() -> ToolDefinition {
    define(
        "model_policy",
        "Show model policy",
        "Show which models and efforts a model_plan may use: the allowed models (or any model \
         Aether lists), the denied models and the maximum effort. Pass `repo` for that \
         repository's effective policy; a repository can only narrow the global one. enqueue, \
         update_item and retry_item refuse choices outside it.",
        object(
            json!({"repo": repo_ref("Show this repository's effective policy.")}),
            &[],
        ),
        false,
    )
}

fn list_queue() -> ToolDefinition {
    define(
        "list_queue",
        "List queue items",
        "List queue items with id, state, pipeline, priority, repository and title, plus why an \
         item is waiting (derived waiting_reason code, message and next_action), blocked, failed or cancelled. All filters are optional and combine with AND. Use \
         get_item for one item's runs, stages, verdicts and spend.",
        object(
            json!({
                "states": {
                    "type": "array",
                    "items": {
                        "type": "string",
                        "enum": QueueState::KINDS,
                    },
                    "description": "Only items in one of these states. Omit for all states.",
                },
                "repo": repo_ref("Only items for this repository."),
                "pipeline": {"type": "string", "description": "Only items using this pipeline."},
                "tag": {"type": "string", "description": "Only items carrying this tag."},
                "limit": {"type": "integer", "minimum": 1, "description": "At most this many items."},
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
        "Show one queue item in detail: state, brief, model plan, every pipeline run (branch, \
         worktree, state) and every stage attempt (model, state, verdict, summary, cost), plus \
         total spend and derived waiting_reason (code, message, next_action). Use it to explain an outcome, find the PR or commit, or decide on a retry. \
         Long summaries are clipped here; get_stage reads them in full.",
        object(json!({"item": item_id("The item to show.")}), &["item"]),
        false,
    )
}

fn get_stage() -> ToolDefinition {
    define(
        "get_stage",
        "Get stage output",
        "Read what the stages of an item's pipeline run reported, unclipped: its latest run \
         unless `run` is given. Without `stage`, list every stage run: stage, attempt, slot, \
         state, verdict and cost. With \
         `stage`, show each matching stage run's error, whole summary and structured result \
         (a reviewer's findings with paths and lines) as pretty-printed JSON: the latest attempt \
         unless `attempt` is given, every fan-out slot unless `slot` is given. Use it to read \
         review findings, or when get_item or item_finished clipped a summary. Long output \
         comes in pages; a page that is not the last says the `offset` of the next one.",
        object(
            json!({
                "item": item_id("The item whose stages to read."),
                "run": {"type": "string", "description": "Pipeline run id (`prun_…`), as get_item shows it; omit for the item's latest run. Attempts restart at 1 in each run."},
                "stage": {"type": "string", "minLength": 1, "description": "Stage id, as get_item or the listing shows it. Omit to list the stage runs."},
                "attempt": {"type": "integer", "minimum": 1, "description": "Which attempt of the stage; omit for the latest. Needs `stage`."},
                "slot": {"type": "integer", "minimum": 0, "description": "Fan-out slot (0-based); omit for every slot. Needs `stage`."},
                "offset": {"type": "integer", "minimum": 0, "description": "Characters of each stage run's output to skip, as the previous page's footer says. Needs `stage`."},
                "limit": {"type": "integer", "minimum": 1, "maximum": MAX_STAGE_PAGE_CHARS, "description": format!("Characters of output per stage run and page; default {STAGE_PAGE_CHARS}. Needs `stage`.")},
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
        "Daemon overview: Aether connection, orchestrator, pause state, running agents against the \
         caps, active runs, queued and blocked counts, open questions, and spend today and in \
         total against the budgets.",
        object(json!({}), &[]),
        false,
    )
}

fn cost_report() -> ToolDefinition {
    let groups: Vec<&str> = CostGroup::ALL.iter().map(|g| g.name()).collect();
    let bound = "A date `YYYY-MM-DD`, or whole days back from today like `7d` or `2w`.";
    define(
        "cost_report",
        "Spend report",
        "Report spend summed by local calendar day, the days the daily budget counts, and grouped \
         `by` day, repo, pipeline, model, item or stage, with a total. Each amount is what the \
         budgets count (reported cost plus separate API-equivalent estimates when enabled), then \
         provider-reported billing, list-price estimates and unclassified cost; classification \
         does not alter budget totals. `(none)` gathers spend with no such owner: the \
         orchestrator's, and agents outside a stage. Use it to answer spend questions; \
         queue_status shows only today and the lifetime total.",
        object(
            json!({
                "since": {"type": "string", "minLength": 1, "description": format!("First day of the report. {bound} `7d` is the last 7 days with today. Default: {DEFAULT_DAYS}d.")},
                "until": {"type": "string", "minLength": 1, "description": format!("Last day of the report. {bound} `1d` ends yesterday. Default: today.")},
                "by": {"type": "string", "enum": groups, "description": "What to group spend by; default day."},
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
        "Observe stored stage attempts, grouped by stage, model and effort, sorted by stage then median completed cost. Shows n, completed n, median/p90 cost, median minutes and review/test verdict pass share. Completed n < 5 is a small sample. At most 60 groups. Costs follow the budget estimate setting; each fan-out slot counts separately. No recommendations. Review/test kinds use the run's frozen pipeline spec when available, otherwise the current catalog; unknown kinds retain attempts, cost and minutes but exclude verdicts.",
        object(
            json!({
                "repo": repo_ref("Only this repository."),
                "pipeline": {"type": "string", "minLength": 1, "description": "Only this pipeline."},
                "stage": {"type": "string", "minLength": 1, "description": "Only this stage id."},
                "since": {"type": "string", "minLength": 1, "description": "Attempt start on or after this local day: YYYY-MM-DD, 7d or 2w. Default 30d, including today."}
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
        "Register a git repository so work can be enqueued against it. Returns the registered \
         repository. Its autonomy ceiling is the default one, without merge=auto: only the \
         operator can raise it, from the CLI.",
        object(
            json!({
                "path": {"type": "string", "description": "Absolute path of the repository's top-level directory (the git work tree root)."},
                "name": {"type": "string", "description": "Short unique name used as `repo` in other tools: letters, digits, `-`, `_`, `.`; at most 64 characters; not starting with `.`. Defaults to the directory name."},
                "ship": {"type": "string", "enum": ["pr", "local"], "description": "How finished work leaves the repository: `pr` pushes the branch and opens a pull request (default); `local` commits on a local branch and never pushes."},
                "base_branch": {"type": "string", "description": "Branch new work forks from. Defaults to the repository's checked-out HEAD commit."},
                "default_pipeline": {"type": "string", "description": "Pipeline this repository's items use by default."},
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
        "Add one unit of work to the queue and return the queued item with its `qi_…` id. The \
         daemon runs it through `pipeline` in a fresh git worktree and branch once dependencies, \
         caps and budgets allow; this call does not wait for that. When the item finishes you \
         receive an item_finished trigger, so do not poll. Before calling, choose the pipeline \
         with list_pipelines, the models with model_policy, and the `autonomy` knobs within \
         autonomy_policy's ceiling; omit `pipeline` to let `autonomy.review` pick the review \
         family. Knobs may add `propose` and `report` stages; the model_plan may name them and \
         otherwise they take the first stage's model. Call once per independent unit of work; \
         use depends_on to order units. Set idempotency_key whenever the call might be retried: \
         a repeat with the same key returns the first item instead of a duplicate.",
        object(
            json!({
                "repo": repo_ref("Repository to work in. Required unless the pipeline has requires_repo = false (e.g. `ask`)."),
                "pipeline": {"type": "string", "description": "Pipeline name from list_pipelines. Omit it to use the review family `autonomy.review` names, or the repository's default pipeline; required without `repo`."},
                "title": {"type": "string", "description": "One line naming the work; shown in listings and used in the branch name."},
                "brief": {"type": "string", "description": "Complete, self-contained instructions for the stage agents: goal, relevant context, constraints and acceptance criteria. They see only this brief and the repository, not your conversation."},
                "priority": priority(),
                "depends_on": string_list("Items (`qi_…`) that must succeed before this one starts. If one fails or is cancelled, this item becomes blocked."),
                "tags": string_list("Free-form labels for filtering with list_queue."),
                "model_reasons": model_reasons(),
                "model_plan": model_plan("Model and effort for every stage."),
                "budget_usd": budget(),
                "idempotency_key": {"type": "string", "description": "Any unique string for this unit of work; a retried enqueue with the same key and arguments returns the original item."},
                "autonomy": autonomy(),
                "origin": {
                    "type": "object",
                    "description": "The GitHub issue or pull request that asked for this work, from its github trigger. Its item_finished trigger then names the thread, so you can post the outcome there with github_reply.",
                    "properties": {
                        "repository": {"type": "string", "description": "`owner/name`, as the trigger's `repository`."},
                        "number": {"type": "integer", "minimum": 1, "description": "The issue or pull request number."},
                    },
                    "required": ["repository", "number"],
                    "additionalProperties": false,
                },
            }),
            &["title", "brief", "model_plan"],
        ),
        false,
    )
}

fn update_item() -> ToolDefinition {
    define(
        "update_item",
        "Update queue item",
        "Change a queue item that has not succeeded. Pass `item` and at least one change. \
         `model_plan` and `tags` replace the current values; `add_depends_on` adds dependencies. \
         `model_plan` and `brief_append` can change only while no run is dispatching or \
         running: for an active item the call is refused, so cancel_item it and retry_item \
         with the new plan or amendment.",
        object(
            json!({
                "item": item_id("The item to change."),
                "priority": priority(),
                "model_reasons": model_reasons(),
                "model_plan": model_plan("Replacement plan for the whole pipeline, checked against model_policy."),
                "tags": string_list("Replacement tag list."),
                "add_depends_on": string_list("More items (`qi_…`) that must succeed before this one starts."),
                "budget_usd": budget(),
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
        "Cancel a queue item. A queued or blocked item never starts; a running item's stage \
         agents are cancelled and its run ends. Items that depend on it become blocked. Only \
         retry_item brings it back.",
        object(
            json!({
                "item": item_id("The item to cancel."),
                "reason": {"type": "string", "description": "Why, for the operator and the item's history."},
            }),
            &["item"],
        ),
        true,
    )
}

fn retry_item() -> ToolDefinition {
    define(
        "retry_item",
        "Retry queue item",
        "Re-queue a failed or cancelled item for a new pipeline run from its first stage. By \
         default the run reuses the item's worktree and branch, so it starts from the earlier \
         run's work. Optionally replace its model_plan, for example with a stronger model after \
         a failure, and add guidance with brief_append; read get_item first to see why it \
         failed. Succeeded items cannot be retried: enqueue follow-up work instead.",
        object(
            json!({
                "item": item_id("The failed or cancelled item."),
                "model_reasons": model_reasons(),
                "model_plan": model_plan("Replacement plan for the new run. Omit to keep the current one."),
                "fresh": {"type": "boolean", "description": "Start again from the repository's base branch head, on a new branch, instead of the earlier run's work. The old worktree is moved aside with everything in it, uncommitted work included, and the old branch is kept with its commits. Worktree gc deletes the set-aside worktree, uncommitted work included, worktrees.gc_after (default 7 days) after the item's last run. A pull request open from the old branch stays open and unchanged: the new run's ship stage opens a new one, so close the old one with a note. Default false."},
                "brief_append": brief_append(),
            }),
            &["item"],
        ),
        false,
    )
}

fn pause_target(action: &str) -> Value {
    object(
        json!({
            "scope": {"type": "string", "enum": ["global", "repo", "pipeline"], "description": format!("What to {action}: everything, one repository, or one pipeline.")},
            "name": {"type": "string", "description": "Repository name or id when scope is `repo`; pipeline name when scope is `pipeline`. Omit for `global`."},
        }),
        &["scope"],
    )
}

fn pause() -> ToolDefinition {
    define(
        "pause",
        "Pause dispatch",
        "Stop starting new work. scope `global` pauses everything; `repo` or `pipeline` pauses \
         only that slice and needs `name`. Stage agents already running finish their stage. Undo \
         with resume using the same scope and name.",
        pause_target("pause"),
        false,
    )
}

fn resume() -> ToolDefinition {
    define(
        "resume",
        "Resume dispatch",
        "Undo a pause: pass the same scope and name that were paused.",
        pause_target("resume"),
        false,
    )
}

fn notify() -> ToolDefinition {
    define(
        "notify",
        "Notify operator",
        "Post a line to the operator's chat. Your final reply to an operator message is shown \
         anyway; use notify for everything else the operator should see. Kinds: `decision` (a \
         choice you made that they should know about), `blocker` (work cannot proceed without \
         them), `outcome` (a result worth reporting beyond the automatic item outcome), \
         `progress` (hidden unless they ask for progress). For a question that needs an answer \
         use ask_operator instead of kind `question`.",
        object(
            json!({
                "kind": {"type": "string", "enum": ["question", "decision", "blocker", "outcome", "progress"], "description": "What sort of message this is."},
                "text": {"type": "string", "description": "The message, written for the operator."},
                "item": item_id("The item this concerns, if any."),
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
        "Ask the operator a question without blocking: it appears in their chat and you keep \
         working. The answer arrives later as an operator_answer trigger carrying the question \
         id, so do not wait or poll for it. Offer `options` when the answer is a choice.",
        object(
            json!({
                "text": {"type": "string", "description": "The question, with the context needed to answer it."},
                "item": item_id("The item this concerns, if any."),
                "options": string_list("Suggested answers, when the answer is a choice."),
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
        "Close a question you asked with ask_operator once the operator has plainly answered it \
         in chat and you are acting on that answer: it leaves `archon questions`, recorded as \
         answered in chat with their words. Only for your own ask_operator questions; a gate or \
         any other question is refused, because only the operator answers those. No \
         operator_answer trigger follows. Requires Aether caller metadata \
         (send_caller_meta = true).",
        object(
            json!({
                "question": {"type": "string", "description": "The question id (`q_…`) ask_operator returned."},
                "answer": {"type": "string", "description": "The operator's answer, in their words."},
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
         refused) for the operator to review. With Lavish available it opens as a review session \
         and the operator's feedback arrives later as a lavish_feedback trigger; otherwise the \
         operator is given the file path.",
        object(
            json!({
                "title": {"type": "string", "description": "Report title shown to the operator."},
                "path": {"type": "string", "description": "Absolute path of an existing `.html` or `.htm` file."},
                "item": item_id("The item the report belongs to, if any."),
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
        "Show the autonomy defaults and ceiling (globally, or for `repo`) and the operator \
         directives in force. Read it before choosing an item's autonomy knobs: enqueue refuses a \
         knob above the ceiling, and only the operator can raise it, from the CLI.",
        object(
            json!({"repo": repo_ref("Show this repository's effective defaults and ceiling.")}),
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
         with operator standing (\"merge docs fixes yourself\", \"propose first for anything \
         in auth/\"), so it survives your restarts. Every trigger restates the directives in \
         force, and the operator sees them in `archon status`. Record only what the operator \
         said, in their words. A directive guides your knob choices; it never raises the \
         autonomy ceiling.",
        object(
            json!({
                "text": {"type": "string", "description": "The instruction, at most 500 characters."},
                "expires_in_hours": {"type": "integer", "minimum": 1, "maximum": 720, "description": "When it lapses, if the operator gave a time frame (\"today\" is about 12). Omit for a lasting one."},
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
         them), or every one with `all: true` when the operator asked for that.",
        object(
            json!({
                "directive": {"type": "string", "description": "The `dir_…` id to clear."},
                "all": {"type": "boolean", "description": "Clear every directive. Only when the operator asked."},
            }),
            &[],
        ),
        false,
    )
}

fn github_reply() -> ToolDefinition {
    define(
        "github_reply",
        "Reply on GitHub",
        "Post a comment on a GitHub issue or pull request of a repository Archon watches: \
         answer a github trigger on its own thread, or report an item's outcome where it was \
         asked for. Refused for any other repository. Archon appends the reply marker that \
         keeps the comment from triggering the watcher again. The comment is public and cannot \
         be taken back, so never include secrets, operator-only details or local paths. The \
         operator's approvals may hold it until they allow it.",
        object(
            json!({
                "repository": {"type": "string", "description": "`owner/name`, as the github trigger's `repository`."},
                "number": {"type": "integer", "minimum": 1, "description": "The issue or pull request number."},
                "body": {"type": "string", "description": "GitHub-flavoured Markdown, at most 65000 characters."},
            }),
            &["repository", "number", "body"],
        ),
        false,
    )
}
