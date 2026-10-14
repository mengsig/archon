//! The MCP method handlers and the lazily opened daemon connection.
//! Every method but `tools/call` is answered on the read loop. Tool calls
//! run concurrently, each within [`CALL_BUDGET`] of the moment its line was
//! read, so a slow call cannot push the ones behind it past Aether's timeout.
//! A `notifications/cancelled` is only logged: the call still ends within its
//! budget, and the client ignores the late answer, as MCP specifies.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use archon_client::ArchonClient;
use archon_core::aether_ids::AetherAgentId;
use archon_core::error::ArchonError;
use archon_core::protocol::{ClientKind, ErrorReply, Reply, Request};
use serde_json::{Value, json};
use tokio::sync::Mutex;
use tokio::time::Instant;

use crate::jsonrpc::{
    INVALID_PARAMS, MAX_ID_BYTES, METHOD_NOT_FOUND, Message, Response, encoded_len, negotiate,
    truncate,
};
use crate::tools::ToolDefinition;
use crate::{MAX_CONCURRENT_CALLS, MAX_MESSAGE_BYTES, tools};

/// Everything one tool call may take, from the moment its line is read to
/// its answer, daemon autostart included. Under Aether's 20 s MCP timeout,
/// which would otherwise kill this server.
pub const CALL_BUDGET: Duration = Duration::from_secs(15);
const _: () = assert!(
    archon_core::github::REPLY_BUDGET.as_secs() + 3 <= CALL_BUDGET.as_secs(),
    "archond answers a github_reply, even one it stopped, within the call's budget"
);

/// Aether drops a larger `structuredContent` (its `MAX_STRUCTURED_CHARS`),
/// so sending one only spends the message budget.
const MAX_STRUCTURED_BYTES: usize = 30_000;
/// Room kept under `MAX_MESSAGE_BYTES` for the JSON-RPC envelope around a
/// tool result: `{"jsonrpc":"2.0","id":…,"result":…}` with a bounded id.
const ENVELOPE_BYTES: usize = 1024;
const _: () = assert!(
    ENVELOPE_BYTES >= MAX_ID_BYTES + 64,
    "the envelope holds any id"
);
/// Longest tool error text. Argument errors quote the caller's values, and
/// past this length the quote is noise to the model.
const MAX_TOOL_ERROR_BYTES: usize = 16 * 1024;
/// How much of an unknown method or tool name an error quotes back.
const MAX_ECHOED_NAME_BYTES: usize = 128;

const INSTRUCTIONS: &str = "Archon runs queued work through pipelines of Aether agents across \
    registered repositories. Plan with list_pipelines, model_policy and list_repos; call enqueue \
    once per unit of work, with a model_plan for the stages whose model you choose. Outcomes, blocked items and \
    operator answers arrive as triggers, so never poll; use get_item or list_queue to inspect, \
    and get_stage for a stage's full summary and findings.";

/// A failed method: the JSON-RPC error code and message.
type Failure = (i64, String);

/// What one line from the client asks of the server.
#[derive(Debug)]
pub(crate) enum Dispatch {
    /// Answered on the spot; `None` for notifications and responses.
    Now(Option<Response>),
    /// A `tools/call`, answered off the read loop by [`Server::call`].
    Call(Call),
}

/// One `tools/call` waiting for its answer.
#[derive(Debug)]
pub(crate) struct Call {
    id: Value,
    params: Value,
    /// [`CALL_BUDGET`] after the line was read.
    deadline: Instant,
}

/// Classify one line just read, answering it unless it is a tool call.
pub(crate) fn intake(line: &[u8]) -> Dispatch {
    let deadline = Instant::now() + CALL_BUDGET;
    let message = match Message::parse(line) {
        Ok(message) => message,
        Err(response) => return Dispatch::Now(Some(*response)),
    };
    let (id, method, params) = match message {
        Message::Request { id, method, params } => (id, method, params),
        Message::Notification { method } => {
            tracing::debug!(%method, "notification");
            return Dispatch::Now(None);
        }
        Message::Response => {
            tracing::debug!("ignored a response; this server sends no requests");
            return Dispatch::Now(None);
        }
    };
    let result = match method.as_str() {
        "tools/call" => {
            return Dispatch::Call(Call {
                id,
                params,
                deadline,
            });
        }
        "initialize" => initialize(&params),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(tools_list()),
        other => Err((
            METHOD_NOT_FOUND,
            format!(
                "method `{}` is not supported; this server implements initialize, ping, \
                 tools/list and tools/call",
                truncate(other, MAX_ECHOED_NAME_BYTES)
            ),
        )),
    };
    Dispatch::Now(Some(respond(id, result)))
}

/// The answer to a tool call refused because [`MAX_CONCURRENT_CALLS`] are
/// already running: a tool error, so the model knows to retry.
pub(crate) fn busy(call: Call) -> Response {
    let text = format!(
        "Archon is already running {MAX_CONCURRENT_CALLS} tool calls, so this one was not \
         started. Nothing was changed; retry shortly."
    );
    Response::ok(call.id, tool_error(&text, None))
}

fn respond(id: Value, result: Result<Value, Failure>) -> Response {
    match result {
        Ok(value) => Response::ok(id, value),
        Err((code, message)) => Response::error(id, code, message),
    }
}

/// The one connection to `archond`, opened on first use and re-opened after
/// the daemon goes away, so a restart costs at most the calls in flight.
#[derive(Debug)]
pub(crate) struct DaemonLink {
    socket: PathBuf,
    archon_bin: PathBuf,
    /// Locked only to (re)connect, never across a request: the client
    /// routes concurrent replies by request id.
    client: Mutex<Option<Arc<ArchonClient>>>,
}

impl DaemonLink {
    pub fn new(socket: &Path, archon_bin: &Path) -> Self {
        Self {
            socket: socket.to_owned(),
            archon_bin: archon_bin.to_owned(),
            client: Mutex::new(None),
        }
    }

    async fn call(&self, request: Request) -> Result<Result<Reply, ErrorReply>, ArchonError> {
        let client = self.client().await?;
        client.call(request).await
    }

    /// The live connection, opened now if there is none. Concurrent callers
    /// wait for one connection attempt instead of starting the daemon twice.
    async fn client(&self) -> Result<Arc<ArchonClient>, ArchonError> {
        let mut slot = self.client.lock().await;
        if slot.as_ref().is_some_and(|client| client.is_closed()) {
            tracing::info!("the archond connection closed; reconnecting");
            *slot = None;
        }
        if let Some(client) = slot.as_ref() {
            return Ok(Arc::clone(client));
        }
        let kind = ClientKind::ArchonMcp;
        let client = ArchonClient::connect_or_start(&self.socket, kind, &self.archon_bin).await?;
        let client = Arc::new(client);
        *slot = Some(Arc::clone(&client));
        Ok(client)
    }
}

#[derive(Debug)]
pub(crate) struct Server {
    daemon: DaemonLink,
}

impl Server {
    pub fn new(daemon: DaemonLink) -> Self {
        Self { daemon }
    }

    /// The response to one tool call, due by the call's deadline.
    pub async fn call(&self, call: Call) -> Response {
        let result = self.tools_call(&call.params, call.deadline).await;
        respond(call.id, result)
    }

    async fn tools_call(&self, params: &Value, deadline: Instant) -> Result<Value, Failure> {
        let Some(name) = params.get("name").and_then(Value::as_str) else {
            return Err((INVALID_PARAMS, "tools/call needs a string `name`".into()));
        };
        let Some(definition) = tools::definition(name) else {
            let known: Vec<&str> = tools::definitions().iter().map(|d| d.name).collect();
            let known = known.join(", ");
            let name = truncate(name, MAX_ECHOED_NAME_BYTES);
            return Err((
                INVALID_PARAMS,
                format!("Unknown tool: `{name}`; known: {known}"),
            ));
        };
        let arguments = params.get("arguments").cloned().unwrap_or(Value::Null);
        let result = match request_with_caller(name, arguments, params) {
            Ok(request) => self.forward(definition, request, deadline).await,
            Err(error) => tool_error(&error.0, None),
        };
        Ok(within_message_cap(name, result))
    }

    /// Send a validated request to the daemon and turn its answer, or its
    /// absence by `deadline`, into a tool result.
    async fn forward(
        &self,
        definition: &ToolDefinition,
        request: Request,
        deadline: Instant,
    ) -> Value {
        let name = definition.name;
        let check = (!definition.read_only).then(|| how_to_check(&request));
        let answer = tokio::time::timeout_at(deadline, self.daemon.call(request)).await;
        match answer {
            Ok(Ok(Ok(reply))) => tool_result(name, &reply),
            Ok(Ok(Err(refusal))) => {
                let structured = serde_json::to_value(&refusal).ok();
                tool_error(&refusal.message, structured)
            }
            Ok(Err(error)) => tool_error(&unavailable(name, check.as_deref(), &error), None),
            Err(_elapsed) => tool_error(&timed_out(name, check.as_deref()), None),
        }
    }
}

/// The request for one tool call, forwarded for the agent Aether says made
/// it: archond decides at the change itself whether that agent may (§18.4).
fn request_with_caller(
    name: &str,
    arguments: Value,
    params: &Value,
) -> Result<Request, tools::ToolError> {
    let mut request = tools::to_request(name, arguments)?;
    let caller = caller_identity(params)?;
    name_caller(name, &mut request, caller.as_ref())?;
    let Some(caller) = caller else {
        return Ok(request);
    };
    Ok(Request::ForAgent {
        caller,
        request: Box::new(request),
    })
}

/// Record the caller in the requests that keep it, refusing the ones that
/// cannot work without one.
fn name_caller(
    name: &str,
    request: &mut Request,
    caller: Option<&AetherAgentId>,
) -> Result<(), tools::ToolError> {
    let (named, required) = match request {
        Request::MessageOrchestrator { caller, .. } | Request::RecordChatAnswer { caller, .. } => {
            (caller, true)
        }
        // Without it the question is still asked, with no recorded asker.
        Request::AskOperator { caller, .. } => (caller, false),
        _ => return Ok(()),
    };
    assert!(named.is_none(), "a tool argument never names the caller");
    if required && caller.is_none() {
        return Err(tools::ToolError(format!(
            "{name} requires Aether caller metadata; set send_caller_meta = true on the archon MCP server and restart the relevant Aether daemon"
        )));
    }
    *named = caller.cloned();
    Ok(())
}

/// The agent Aether says made this call, when it sent caller metadata.
fn caller_identity(params: &Value) -> Result<Option<AetherAgentId>, tools::ToolError> {
    let Some(raw) = params.pointer("/_meta/aether~1agent_id") else {
        return Ok(None);
    };
    let invalid = |why: String| tools::ToolError(format!("invalid Aether caller identity: {why}"));
    let raw = raw
        .as_str()
        .ok_or_else(|| invalid(format!("{raw} is not a string")))?;
    let caller = AetherAgentId::parse(raw).map_err(|e| invalid(e.to_string()))?;
    Ok(Some(caller))
}

/// The result, or an error saying it is too large: one message over Aether's
/// cap would make it drop this server and withdraw every Archon tool.
fn within_message_cap(tool: &str, result: Value) -> Value {
    let limit = MAX_MESSAGE_BYTES - ENVELOPE_BYTES;
    let size = encoded_len(&result);
    if size <= limit {
        return result;
    }
    let error = tool_error(
        &format!(
            "The result of `{tool}` is {size} bytes, over the {limit}-byte MCP message limit, \
             so it was not sent. {}",
            how_to_ask_for_less(tool)
        ),
        None,
    );
    assert!(encoded_len(&error) <= limit, "the refusal itself fits");
    error
}

/// How to narrow a call of `tool` whose result was too large.
fn how_to_ask_for_less(tool: &str) -> &'static str {
    match tool {
        "list_queue" => "Ask for less: list_queue takes `limit` and filters.",
        "get_stage" => {
            "Ask for less: get_stage takes a smaller `limit` per page, with `offset` for the \
             next one, and `slot` for one fan-out slot."
        }
        _ => "Ask for less by narrowing the call's arguments.",
    }
}

fn initialize(params: &Value) -> Result<Value, Failure> {
    let Some(requested) = params.get("protocolVersion").and_then(Value::as_str) else {
        return Err((
            INVALID_PARAMS,
            "initialize needs a string `protocolVersion`".into(),
        ));
    };
    let client = params.pointer("/clientInfo/name").and_then(Value::as_str);
    tracing::info!(requested, client, "mcp initialize");
    Ok(json!({
        "protocolVersion": negotiate(requested),
        "capabilities": {"tools": {"listChanged": false}},
        "serverInfo": {"name": "archon", "title": "Archon", "version": env!("CARGO_PKG_VERSION")},
        "instructions": INSTRUCTIONS,
    }))
}

fn tools_list() -> Value {
    let tools: Vec<Value> = tools::definitions().iter().map(|d| d.to_mcp()).collect();
    json!({ "tools": tools })
}

fn tool_result(tool: &str, reply: &Reply) -> Value {
    let text = tools::render(tool, reply);
    // Adjacently tagged, so always an object, as `structuredContent` must be.
    let structured = serde_json::to_value(reply).ok();
    debug_assert!(structured.as_ref().is_some_and(Value::is_object));
    content(text, structured, false)
}

fn tool_error(text: &str, structured: Option<Value>) -> Value {
    let text = truncate(text, MAX_TOOL_ERROR_BYTES).into_owned();
    content(text, structured, true)
}

fn content(text: String, structured: Option<Value>, is_error: bool) -> Value {
    let mut result = json!({
        "content": [{"type": "text", "text": text}],
        "isError": is_error,
    });
    // The text is complete on its own, so an oversized structure is dropped.
    let structured = structured.filter(|s| s.is_object() && encoded_len(s) <= MAX_STRUCTURED_BYTES);
    if let Some(structured) = structured {
        result["structuredContent"] = structured;
    }
    result
}

fn unavailable(tool: &str, check: Option<&str>, error: &ArchonError) -> String {
    let mut text = format!(
        "The Archon daemon is unavailable, so `{tool}` could not be completed: {error}\nThis is \
         not a problem with your arguments. Retry later; if it persists, the operator must start \
         the daemon with `archon daemon start` and check it with `archon daemon status`."
    );
    if let Some(check) = check {
        text.push_str(&format!(" If the daemon went away mid-call, {check}."));
    }
    text
}

/// `check` is `None` for a read-only tool, which is always safe to retry.
fn timed_out(tool: &str, check: Option<&str>) -> String {
    let secs = CALL_BUDGET.as_secs();
    match check {
        None => format!("The Archon daemon did not answer `{tool}` within {secs}s. Retry shortly."),
        Some(check) => format!(
            "The Archon daemon did not answer `{tool}` within {secs}s and may still carry the \
             call out: {check}."
        ),
    }
}

/// How to learn whether a change whose answer never came took effect.
fn how_to_check(request: &Request) -> String {
    match request {
        Request::ForAgent { request, .. } => how_to_check(request),
        Request::MessageOrchestrator { .. } => "the message may already be recorded; check the item's archon log before repeating it, and keep working on your stage".into(),
        Request::GithubReply { thread, .. } => thread.check_before_retry(),
        Request::RecordChatAnswer { .. } => "the answer may already be recorded; a repeat is then refused as already answered, which is the confirmation".into(),
        _ => "the change may already have been applied, so check with list_queue or get_item \
              before repeating it (an enqueue repeated with the same idempotency_key is always \
              safe)"
            .into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The request archond is asked to run for `agt_1`.
    fn for_agent_1(request: Request) -> Request {
        let Request::ForAgent { caller, request } = request else {
            panic!("a named caller travels with the request, got {request:?}");
        };
        assert_eq!(caller.as_str(), "agt_1");
        *request
    }

    #[test]
    fn messaging_uses_only_out_of_band_caller_identity() {
        let args = json!({"text": "hello", "kind": "relay"});
        assert!(request_with_caller("message_orchestrator", args.clone(), &json!({})).is_err());
        let params = json!({"_meta": {"aether/agent_id": "agt_1"}});
        let request = request_with_caller("message_orchestrator", args, &params).unwrap();
        assert!(
            matches!(for_agent_1(request), Request::MessageOrchestrator { caller: Some(id), .. } if id.as_str() == "agt_1")
        );
        let spoof = json!({"text": "hello", "_meta": {"aether/agent_id": "agt_2"}});
        assert!(request_with_caller("message_orchestrator", spoof, &params).is_err());
    }

    #[test]
    fn recording_a_chat_answer_takes_its_caller_out_of_band_only() {
        let question = archon_core::id::QuestionId::new().to_string();
        let args = json!({"question": question, "answer": "yes"});
        let refused = request_with_caller("record_chat_answer", args.clone(), &json!({}));
        assert!(
            refused
                .unwrap_err()
                .0
                .contains("record_chat_answer requires Aether caller")
        );
        let params = json!({"_meta": {"aether/agent_id": "agt_1"}});
        let request = request_with_caller("record_chat_answer", args, &params).unwrap();
        assert!(
            matches!(for_agent_1(request), Request::RecordChatAnswer { caller: Some(id), .. } if id.as_str() == "agt_1")
        );
        let spoof = json!({"question": question, "answer": "yes", "caller": "agt_2"});
        assert!(request_with_caller("record_chat_answer", spoof, &params).is_err());
    }

    #[test]
    fn asking_the_operator_records_only_the_out_of_band_caller() {
        let args = json!({"text": "Ship to prod?"});
        let unvouched = request_with_caller("ask_operator", args.clone(), &json!({})).unwrap();
        assert!(matches!(
            unvouched,
            Request::AskOperator { caller: None, .. }
        ));
        let params = json!({"_meta": {"aether/agent_id": "agt_1"}});
        let request = request_with_caller("ask_operator", args.clone(), &params).unwrap();
        assert!(
            matches!(for_agent_1(request), Request::AskOperator { caller: Some(id), .. } if id.as_str() == "agt_1")
        );
        let spoof = json!({"text": "Ship to prod?", "caller": "agt_2"});
        assert!(request_with_caller("ask_operator", spoof, &json!({})).is_err());
        let malformed = json!({"_meta": {"aether/agent_id": 7}});
        let refused = request_with_caller("ask_operator", args, &malformed).unwrap_err();
        assert!(refused.0.contains("invalid Aether caller identity"));
    }

    /// archond decides who may change Archon, so every call must reach it
    /// naming its caller, and unnamed exactly when Aether named nobody.
    #[test]
    fn every_call_names_the_agent_aether_vouches_for_and_no_other() {
        let params = json!({"_meta": {"aether/agent_id": "agt_1"}});
        let pause = json!({"scope": "global"});
        let named = request_with_caller("pause", pause.clone(), &params).unwrap();
        assert!(matches!(for_agent_1(named), Request::Pause(_)));
        let read = request_with_caller("list_repos", json!({}), &params).unwrap();
        assert_eq!(for_agent_1(read), Request::RepoList);

        let unnamed = request_with_caller("pause", pause.clone(), &json!({})).unwrap();
        assert!(matches!(unnamed, Request::Pause(_)), "{unnamed:?}");
        let malformed = json!({"_meta": {"aether/agent_id": 7}});
        let refused = request_with_caller("pause", pause, &malformed).unwrap_err();
        assert!(refused.0.contains("invalid Aether caller identity"));
    }

    #[test]
    fn a_forwarded_change_keeps_its_own_advice_on_checking_it() {
        let question = archon_core::id::QuestionId::new().to_string();
        let args = json!({"question": question, "answer": "yes"});
        let params = json!({"_meta": {"aether/agent_id": "agt_1"}});
        let request = request_with_caller("record_chat_answer", args, &params).unwrap();
        assert!(matches!(request, Request::ForAgent { .. }));
        let check = how_to_check(&request);
        assert!(check.contains("refused as already answered"), "{check}");
        assert_eq!(check, how_to_check(&for_agent_1(request)));
    }

    fn oversized_error(tool: &str) -> String {
        let huge = content("x".repeat(MAX_MESSAGE_BYTES), None, false);
        let refused = within_message_cap(tool, huge);
        assert_eq!(refused["isError"], true, "an oversized result is refused");
        let text = refused["content"][0]["text"]
            .as_str()
            .expect("text content");
        assert!(text.contains("MCP message limit"), "{text}");
        text.to_owned()
    }

    #[test]
    fn an_oversized_result_says_how_to_narrow_the_tool_that_was_called() {
        let stage = oversized_error("get_stage");
        assert!(
            stage.contains("`limit`") && stage.contains("`offset`") && stage.contains("`slot`"),
            "{stage}"
        );
        assert!(!stage.contains("list_queue"), "{stage}");
        let queue = oversized_error("list_queue");
        assert!(queue.contains("list_queue takes `limit`"), "{queue}");
        let item = oversized_error("get_item");
        assert!(
            !item.contains("list_queue") && item.contains("narrowing"),
            "{item}"
        );
        let small = content("fits".into(), None, false);
        assert_eq!(within_message_cap("get_stage", small.clone()), small);
    }
}
