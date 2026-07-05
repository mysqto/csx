//! Model Context Protocol (MCP) server logic.
//!
//! This module implements the *decision* half of an MCP server: it parses
//! JSON-RPC 2.0 request objects and produces JSON-RPC response objects for the
//! three methods csx exposes —
//!
//! * `initialize` — the capability handshake,
//! * `tools/list` — the catalog of callable tools, and
//! * `tools/call` — invoking one of three tools:
//!   * `search_sessions` (args: `query` plus an optional `scope`),
//!   * `get_session` (args: `id`), and
//!   * `ask_sessions` (args: `question` plus an optional `scope`).
//!
//! The handler is a pure function over a [`Db`] and an [`AskEngine`]: it never
//! touches stdin/stdout, so every branch — the handshake, the tool catalog,
//! each tool's happy path, and every error (unknown method, unknown tool, bad
//! arguments, a failing search or answer) — is unit-tested against a temp
//! database and a fake answer engine.
//!
//! The real line-delimited stdio transport that pumps this handler lives in
//! [`crate::mcp_shim`] and is excluded from coverage.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::cli::AskEngine;
use crate::daemon::RequestScope;
use crate::db::{Db, Scope, SearchOpts};

/// The MCP protocol version this server advertises in the `initialize`
/// response. Matches the value clients send in their `initialize` params.
pub const PROTOCOL_VERSION: &str = "2024-11-05";

/// Server name reported in the `initialize` handshake.
pub const SERVER_NAME: &str = "csx";

/// Server version reported in the `initialize` handshake.
pub const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Default number of hits/sessions a tool returns when the request omits a
/// limit.
fn default_limit() -> usize {
    10
}

/// A JSON-RPC 2.0 request (or notification, when `id` is absent).
///
/// The `id` field distinguishes a request (which expects a response) from a
/// notification (which does not); `params` is method-specific and parsed
/// lazily by the handler.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct Request {
    /// Protocol tag; always `"2.0"` for well-formed peers.
    pub jsonrpc: String,
    /// Correlation id. Absent for notifications.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<Value>,
    /// The invoked method name.
    pub method: String,
    /// Method parameters, method-specific.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

/// A JSON-RPC 2.0 response: exactly one of `result` or `error` is set.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct Response {
    /// Protocol tag; always `"2.0"`.
    pub jsonrpc: String,
    /// Correlation id echoed from the request (null when unknown).
    pub id: Value,
    /// The success payload, when the call succeeded.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    /// The failure payload, when the call failed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<RpcError>,
}

/// A JSON-RPC 2.0 error object.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct RpcError {
    /// Numeric error code (JSON-RPC standard codes where applicable).
    pub code: i64,
    /// Human-readable error message.
    pub message: String,
}

/// JSON-RPC: the method does not exist / is not supported.
pub const METHOD_NOT_FOUND: i64 = -32601;
/// JSON-RPC: invalid method parameters.
pub const INVALID_PARAMS: i64 = -32602;
/// JSON-RPC: internal error (a tool failed to execute).
pub const INTERNAL_ERROR: i64 = -32603;

impl Response {
    /// Build a success response echoing `id`.
    pub fn success(id: Value, result: Value) -> Self {
        Response {
            jsonrpc: "2.0".to_string(),
            id,
            result: Some(result),
            error: None,
        }
    }

    /// Build an error response echoing `id`, with `code` and `message`.
    pub fn error(id: Value, code: i64, message: impl Into<String>) -> Self {
        Response {
            jsonrpc: "2.0".to_string(),
            id,
            result: None,
            error: Some(RpcError {
                code,
                message: message.into(),
            }),
        }
    }

    /// Serialize to a single newline-terminated line for the stdio transport.
    pub fn to_line(&self) -> String {
        let body = serde_json::to_string(self).unwrap_or_else(|_| {
            r#"{"jsonrpc":"2.0","id":null,"error":{"code":-32603,"message":"serialize failed"}}"#
                .to_string()
        });
        format!("{body}\n")
    }
}

/// Arguments for the `search_sessions` tool.
#[derive(Debug, Clone, Deserialize, Serialize, Default, PartialEq)]
pub struct SearchArgs {
    /// The full-text (or trigram, when `code`) query string.
    pub query: String,
    /// Optional scope filters, ANDed together.
    #[serde(default)]
    pub scope: RequestScope,
    /// Use the trigram (substring / code) index instead of full text.
    #[serde(default)]
    pub code: bool,
    /// Maximum number of hits to return.
    #[serde(default = "default_limit")]
    pub limit: usize,
}

/// Arguments for the `get_session` tool.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct GetSessionArgs {
    /// The session identifier to fetch.
    pub id: String,
}

/// Arguments for the `ask_sessions` tool.
#[derive(Debug, Clone, Deserialize, Serialize, Default, PartialEq)]
pub struct AskArgs {
    /// The natural-language question to answer from the transcripts.
    pub question: String,
    /// Optional scope restricting which transcripts are consulted.
    #[serde(default)]
    pub scope: RequestScope,
    /// Maximum number of passages to retrieve as grounding context.
    #[serde(default = "default_limit")]
    pub limit: usize,
}

/// The static tool catalog returned by `tools/list`.
///
/// Each entry advertises a tool's name, a one-line description, and its JSON
/// Schema `inputSchema` so a client can validate arguments before calling.
pub fn tool_catalog() -> Value {
    json!([
        {
            "name": "search_sessions",
            "description": "Full-text and hybrid search over the developer's AI-coding session transcripts, scoped by account, repo, tool, branch, and time.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "Search query text." },
                    "scope": scope_schema(),
                    "code": { "type": "boolean", "description": "Use the substring/code (trigram) index." },
                    "limit": { "type": "integer", "description": "Maximum hits to return." }
                },
                "required": ["query"]
            }
        },
        {
            "name": "get_session",
            "description": "Fetch the full ordered message transcript of a single session by its id.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "id": { "type": "string", "description": "Session identifier." }
                },
                "required": ["id"]
            }
        },
        {
            "name": "ask_sessions",
            "description": "Answer a natural-language question grounded in the developer's own session transcripts, returning the answer and the sessions cited as evidence.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "question": { "type": "string", "description": "The question to answer." },
                    "scope": scope_schema(),
                    "limit": { "type": "integer", "description": "Maximum grounding passages to retrieve." }
                },
                "required": ["question"]
            }
        }
    ])
}

/// The JSON Schema fragment describing the optional `scope` object shared by the
/// search and ask tools.
fn scope_schema() -> Value {
    json!({
        "type": "object",
        "description": "Optional scope filters; all fields optional and ANDed together.",
        "properties": {
            "account_uuid": { "type": "string" },
            "org_uuid": { "type": "string" },
            "profile": { "type": "string" },
            "tool": { "type": "string" },
            "repo_id": { "type": "string" },
            "cwd": { "type": "string" },
            "branch": { "type": "string" },
            "session_id": { "type": "string" },
            "role": { "type": "string" },
            "tool_name": { "type": "string" },
            "since": { "type": "integer" },
            "until": { "type": "integer" }
        }
    })
}

/// Handle one parsed JSON-RPC message.
///
/// Returns `Some(response)` for a request that expects a reply, and `None` for
/// a notification (a message with no `id`, e.g. `notifications/initialized`),
/// which per JSON-RPC must not be answered.
///
/// The handler never returns an `Err`: a tool failure is folded into a
/// JSON-RPC error response so the transport always has exactly one line (or
/// nothing, for a notification) to emit.
pub fn handle_message(db: &Db, ask: &dyn AskEngine, req: &Request) -> Option<Response> {
    // A notification has no id and must not be answered.
    let id = req.id.clone()?;

    let resp = match req.method.as_str() {
        "initialize" => Response::success(id, initialize_result()),
        "tools/list" => Response::success(id, json!({ "tools": tool_catalog() })),
        "tools/call" => handle_tools_call(db, ask, id, req.params.as_ref()),
        other => Response::error(id, METHOD_NOT_FOUND, format!("method not found: {other}")),
    };
    Some(resp)
}

/// The `initialize` result: protocol version, advertised capabilities, and
/// server identity.
fn initialize_result() -> Value {
    json!({
        "protocolVersion": PROTOCOL_VERSION,
        "capabilities": { "tools": {} },
        "serverInfo": { "name": SERVER_NAME, "version": SERVER_VERSION }
    })
}

/// Dispatch a `tools/call` request to the named tool.
fn handle_tools_call(db: &Db, ask: &dyn AskEngine, id: Value, params: Option<&Value>) -> Response {
    let params = match params {
        Some(p) => p,
        None => return Response::error(id, INVALID_PARAMS, "missing params"),
    };
    let name = match params.get("name").and_then(Value::as_str) {
        Some(n) => n,
        None => return Response::error(id, INVALID_PARAMS, "missing tool name"),
    };
    // `arguments` is optional; treat an absent object as empty.
    let arguments = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));

    match name {
        "search_sessions" => call_search(db, id, arguments),
        "get_session" => call_get_session(db, id, arguments),
        "ask_sessions" => call_ask(db, ask, id, arguments),
        other => Response::error(id, INVALID_PARAMS, format!("unknown tool: {other}")),
    }
}

/// Wrap tool output text into the MCP `tools/call` result envelope: a single
/// text content block plus a structured payload.
fn tool_result(text: String, structured: Value) -> Value {
    json!({
        "content": [ { "type": "text", "text": text } ],
        "structuredContent": structured,
        "isError": false
    })
}

/// Parse tool arguments into `T`, mapping a schema mismatch to an
/// invalid-params error response.
fn parse_args<T: for<'de> Deserialize<'de>>(
    id: &Value,
    arguments: Value,
) -> std::result::Result<T, Response> {
    serde_json::from_value::<T>(arguments)
        .map_err(|e| Response::error(id.clone(), INVALID_PARAMS, format!("bad arguments: {e}")))
}

/// Execute `search_sessions`.
fn call_search(db: &Db, id: Value, arguments: Value) -> Response {
    let args: SearchArgs = match parse_args(&id, arguments) {
        Ok(a) => a,
        Err(resp) => return resp,
    };
    let scope: Scope = args.scope.to_scope();
    let opts = SearchOpts {
        code: args.code,
        limit: args.limit,
    };
    match db.search(&args.query, &scope, &opts) {
        Ok(hits) => {
            let structured = json!({
                "hits": hits.iter().map(|h| json!({
                    "session_id": h.session_id,
                    "tool": h.tool,
                    "repo_id": h.repo_id,
                    "project_name": h.project_name,
                    "ts": h.ts,
                    "snippet": h.snippet,
                    "score": h.score,
                })).collect::<Vec<_>>()
            });
            let text = format!("{} matching session message(s).", hits.len());
            Response::success(id, tool_result(text, structured))
        }
        Err(e) => Response::error(id, INTERNAL_ERROR, format!("search failed: {e}")),
    }
}

/// Execute `get_session`.
fn call_get_session(db: &Db, id: Value, arguments: Value) -> Response {
    let args: GetSessionArgs = match parse_args(&id, arguments) {
        Ok(a) => a,
        Err(resp) => return resp,
    };
    match db.session_messages(&args.id) {
        Ok(messages) => {
            let structured = json!({
                "session_id": args.id,
                "messages": messages.iter().map(|m| json!({
                    "seq": m.seq,
                    "ts": m.ts,
                    "role": m.role,
                    "tool_name": m.tool_name,
                    "body": m.body,
                })).collect::<Vec<_>>()
            });
            let text = format!("Session {} has {} message(s).", args.id, messages.len());
            Response::success(id, tool_result(text, structured))
        }
        Err(e) => Response::error(id, INTERNAL_ERROR, format!("get_session failed: {e}")),
    }
}

/// Execute `ask_sessions`.
fn call_ask(db: &Db, ask: &dyn AskEngine, id: Value, arguments: Value) -> Response {
    let args: AskArgs = match parse_args(&id, arguments) {
        Ok(a) => a,
        Err(resp) => return resp,
    };
    let scope: Scope = args.scope.to_scope();
    match ask.ask(db, &args.question, &scope, args.limit) {
        Ok(answer) => {
            let structured = json!({
                "answer": answer.text,
                "citations": answer.citations,
            });
            Response::success(id, tool_result(answer.text.clone(), structured))
        }
        Err(e) => Response::error(id, INTERNAL_ERROR, format!("ask failed: {e}")),
    }
}

/// Parse one line of input into a [`Request`], mapping a JSON error into a
/// JSON-RPC parse-error response (with a null id, since the id is unknown).
pub fn parse_request(line: &str) -> std::result::Result<Request, Response> {
    serde_json::from_str::<Request>(line)
        .map_err(|e| Response::error(Value::Null, -32700, format!("parse error: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::Answer;
    use crate::error::{Error, Result};
    use crate::model::{MessageRecord, Role, SessionMeta, Tool};

    // ---- fakes -----------------------------------------------------------

    /// An [`AskEngine`] that echoes a canned answer and records the last call,
    /// or fails when constructed to.
    struct FakeAsk {
        reply: Option<Answer>,
        seen: std::cell::RefCell<Option<(String, usize)>>,
    }
    impl FakeAsk {
        fn ok(text: &str, citations: &[&str]) -> Self {
            FakeAsk {
                reply: Some(Answer {
                    text: text.to_string(),
                    citations: citations.iter().map(|s| s.to_string()).collect(),
                }),
                seen: std::cell::RefCell::new(None),
            }
        }
        fn failing() -> Self {
            FakeAsk {
                reply: None,
                seen: std::cell::RefCell::new(None),
            }
        }
    }
    impl AskEngine for FakeAsk {
        fn ask(&self, _db: &Db, question: &str, _scope: &Scope, limit: usize) -> Result<Answer> {
            *self.seen.borrow_mut() = Some((question.to_string(), limit));
            match &self.reply {
                Some(a) => Ok(a.clone()),
                None => Err(Error::other("engine unavailable")),
            }
        }
    }

    // ---- fixtures --------------------------------------------------------

    fn seed_db() -> Db {
        let db = Db::open(":memory:").unwrap();
        let src = db
            .upsert_source(&crate::db::SourceRow {
                tool: Some("claude-code".into()),
                ..Default::default()
            })
            .unwrap();
        db.upsert_session(
            src,
            &SessionMeta {
                session_id: "s-1".into(),
                tool: Tool::ClaudeCode,
                repo_id: Some("repo-1".into()),
                project_path: Some("/proj".into()),
                project_name: Some("proj".into()),
                git_branch: Some("main".into()),
                account: None,
                first_ts: 100,
                last_ts: 200,
            },
            2,
            None,
        )
        .unwrap();
        db.insert_message(
            src,
            &MessageRecord {
                session_id: "s-1".into(),
                tool: Tool::ClaudeCode,
                seq: 0,
                ts: 100,
                role: Role::User,
                tool_name: None,
                uuid: None,
                text: "the database query was slow".into(),
                cwd: None,
            },
        )
        .unwrap();
        db.insert_message(
            src,
            &MessageRecord {
                session_id: "s-1".into(),
                tool: Tool::ClaudeCode,
                seq: 1,
                ts: 200,
                role: Role::Assistant,
                tool_name: None,
                uuid: None,
                text: "added a database index to fix it".into(),
                cwd: None,
            },
        )
        .unwrap();
        db
    }

    fn req(id: Value, method: &str, params: Value) -> Request {
        Request {
            jsonrpc: "2.0".into(),
            id: Some(id),
            method: method.into(),
            params: Some(params),
        }
    }

    fn call_tool(name: &str, arguments: Value) -> Request {
        req(
            json!(1),
            "tools/call",
            json!({ "name": name, "arguments": arguments }),
        )
    }

    // ---- initialize ------------------------------------------------------

    #[test]
    fn initialize_handshake_reports_protocol_and_server() {
        let db = seed_db();
        let ask = FakeAsk::ok("x", &[]);
        let request = req(
            json!(0),
            "initialize",
            json!({ "protocolVersion": PROTOCOL_VERSION, "capabilities": {} }),
        );
        let resp = handle_message(&db, &ask, &request).unwrap();
        assert_eq!(resp.id, json!(0));
        assert!(resp.error.is_none());
        let result = resp.result.unwrap();
        assert_eq!(result["protocolVersion"], json!(PROTOCOL_VERSION));
        assert_eq!(result["serverInfo"]["name"], json!(SERVER_NAME));
        assert_eq!(result["serverInfo"]["version"], json!(SERVER_VERSION));
        assert!(result["capabilities"]["tools"].is_object());
    }

    #[test]
    fn notification_gets_no_response() {
        let db = seed_db();
        let ask = FakeAsk::ok("x", &[]);
        let notif = Request {
            jsonrpc: "2.0".into(),
            id: None,
            method: "notifications/initialized".into(),
            params: None,
        };
        assert!(handle_message(&db, &ask, &notif).is_none());
    }

    #[test]
    fn unknown_method_is_method_not_found() {
        let db = seed_db();
        let ask = FakeAsk::ok("x", &[]);
        let request = req(json!(7), "does/not/exist", json!({}));
        let resp = handle_message(&db, &ask, &request).unwrap();
        assert_eq!(resp.id, json!(7));
        let err = resp.error.unwrap();
        assert_eq!(err.code, METHOD_NOT_FOUND);
        assert!(err.message.contains("does/not/exist"));
        assert!(resp.result.is_none());
    }

    // ---- tools/list ------------------------------------------------------

    #[test]
    fn tools_list_shape_lists_three_tools() {
        let db = seed_db();
        let ask = FakeAsk::ok("x", &[]);
        let request = req(json!(2), "tools/list", json!({}));
        let resp = handle_message(&db, &ask, &request).unwrap();
        let result = resp.result.unwrap();
        let tools = result["tools"].as_array().unwrap();
        let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert_eq!(
            names,
            vec!["search_sessions", "get_session", "ask_sessions"]
        );
        // Each tool advertises a description and an object input schema.
        for t in tools {
            assert!(t["description"].as_str().unwrap().len() > 10);
            assert_eq!(t["inputSchema"]["type"], json!("object"));
        }
        // The search tool requires a query.
        assert_eq!(tools[0]["inputSchema"]["required"], json!(["query"]));
        assert_eq!(tools[1]["inputSchema"]["required"], json!(["id"]));
        assert_eq!(tools[2]["inputSchema"]["required"], json!(["question"]));
    }

    // ---- tools/call: search_sessions ------------------------------------

    #[test]
    fn search_sessions_happy_path_returns_hits() {
        let db = seed_db();
        let ask = FakeAsk::ok("x", &[]);
        let request = call_tool("search_sessions", json!({ "query": "database" }));
        let resp = handle_message(&db, &ask, &request).unwrap();
        assert!(resp.error.is_none());
        let result = resp.result.unwrap();
        assert_eq!(result["isError"], json!(false));
        let hits = result["structuredContent"]["hits"].as_array().unwrap();
        assert!(!hits.is_empty());
        assert_eq!(hits[0]["session_id"], json!("s-1"));
        assert_eq!(hits[0]["tool"], json!("claude-code"));
        assert!(result["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("session message"));
    }

    #[test]
    fn search_sessions_honors_scope() {
        let db = seed_db();
        let ask = FakeAsk::ok("x", &[]);
        // A scope that matches nothing yields zero hits (not an error).
        let request = call_tool(
            "search_sessions",
            json!({ "query": "database", "scope": { "tool": "codex" } }),
        );
        let resp = handle_message(&db, &ask, &request).unwrap();
        let result = resp.result.unwrap();
        assert!(result["structuredContent"]["hits"]
            .as_array()
            .unwrap()
            .is_empty());
    }

    #[test]
    fn search_sessions_bad_args_is_invalid_params() {
        let db = seed_db();
        let ask = FakeAsk::ok("x", &[]);
        // `query` must be a string; a number is a schema mismatch.
        let request = call_tool("search_sessions", json!({ "query": 123 }));
        let resp = handle_message(&db, &ask, &request).unwrap();
        let err = resp.error.unwrap();
        assert_eq!(err.code, INVALID_PARAMS);
        assert!(err.message.contains("bad arguments"));
    }

    #[test]
    fn search_sessions_search_error_is_internal_error() {
        let db = seed_db();
        let ask = FakeAsk::ok("x", &[]);
        // An unbalanced FTS quote is a query syntax error from SQLite.
        let request = call_tool("search_sessions", json!({ "query": "\"unterminated" }));
        let resp = handle_message(&db, &ask, &request).unwrap();
        let err = resp.error.unwrap();
        assert_eq!(err.code, INTERNAL_ERROR);
        assert!(err.message.contains("search failed"));
    }

    // ---- tools/call: get_session ----------------------------------------

    #[test]
    fn get_session_happy_path_returns_messages() {
        let db = seed_db();
        let ask = FakeAsk::ok("x", &[]);
        let request = call_tool("get_session", json!({ "id": "s-1" }));
        let resp = handle_message(&db, &ask, &request).unwrap();
        assert!(resp.error.is_none());
        let result = resp.result.unwrap();
        assert_eq!(result["structuredContent"]["session_id"], json!("s-1"));
        let msgs = result["structuredContent"]["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0]["seq"], json!(0));
        assert_eq!(msgs[0]["role"], json!("user"));
        assert!(msgs[0]["body"].as_str().unwrap().contains("slow"));
        assert!(result["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("2 message"));
    }

    #[test]
    fn get_session_unknown_id_returns_empty_transcript() {
        let db = seed_db();
        let ask = FakeAsk::ok("x", &[]);
        let request = call_tool("get_session", json!({ "id": "nope" }));
        let resp = handle_message(&db, &ask, &request).unwrap();
        let result = resp.result.unwrap();
        assert!(result["structuredContent"]["messages"]
            .as_array()
            .unwrap()
            .is_empty());
    }

    #[test]
    fn get_session_bad_args_is_invalid_params() {
        let db = seed_db();
        let ask = FakeAsk::ok("x", &[]);
        // Missing required `id`.
        let request = call_tool("get_session", json!({}));
        let resp = handle_message(&db, &ask, &request).unwrap();
        let err = resp.error.unwrap();
        assert_eq!(err.code, INVALID_PARAMS);
    }

    // ---- tools/call: ask_sessions ---------------------------------------

    #[test]
    fn ask_sessions_happy_path_returns_answer_and_citations() {
        let db = seed_db();
        let ask = FakeAsk::ok("It was a missing index [1].", &["s-1"]);
        let request = call_tool(
            "ask_sessions",
            json!({ "question": "why was the query slow", "limit": 5 }),
        );
        let resp = handle_message(&db, &ask, &request).unwrap();
        assert!(resp.error.is_none());
        let result = resp.result.unwrap();
        assert_eq!(
            result["structuredContent"]["answer"],
            json!("It was a missing index [1].")
        );
        assert_eq!(result["structuredContent"]["citations"], json!(["s-1"]));
        assert_eq!(
            result["content"][0]["text"],
            json!("It was a missing index [1].")
        );
        // The engine saw the question and the requested limit.
        let (q, limit) = ask.seen.borrow().clone().unwrap();
        assert_eq!(q, "why was the query slow");
        assert_eq!(limit, 5);
    }

    #[test]
    fn ask_sessions_defaults_limit_when_omitted() {
        let db = seed_db();
        let ask = FakeAsk::ok("ok", &[]);
        let request = call_tool("ask_sessions", json!({ "question": "anything" }));
        let resp = handle_message(&db, &ask, &request).unwrap();
        assert!(resp.error.is_none());
        let (_, limit) = ask.seen.borrow().clone().unwrap();
        assert_eq!(limit, default_limit());
    }

    #[test]
    fn ask_sessions_engine_failure_is_internal_error() {
        let db = seed_db();
        let ask = FakeAsk::failing();
        let request = call_tool("ask_sessions", json!({ "question": "why" }));
        let resp = handle_message(&db, &ask, &request).unwrap();
        let err = resp.error.unwrap();
        assert_eq!(err.code, INTERNAL_ERROR);
        assert!(err.message.contains("ask failed"));
    }

    #[test]
    fn ask_sessions_bad_args_is_invalid_params() {
        let db = seed_db();
        let ask = FakeAsk::ok("x", &[]);
        // Missing required `question`.
        let request = call_tool("ask_sessions", json!({ "scope": {} }));
        let resp = handle_message(&db, &ask, &request).unwrap();
        assert_eq!(resp.error.unwrap().code, INVALID_PARAMS);
    }

    // ---- tools/call: dispatch errors ------------------------------------

    #[test]
    fn tools_call_unknown_tool_is_invalid_params() {
        let db = seed_db();
        let ask = FakeAsk::ok("x", &[]);
        let request = call_tool("frobnicate", json!({}));
        let resp = handle_message(&db, &ask, &request).unwrap();
        let err = resp.error.unwrap();
        assert_eq!(err.code, INVALID_PARAMS);
        assert!(err.message.contains("unknown tool: frobnicate"));
    }

    #[test]
    fn tools_call_missing_params_is_invalid_params() {
        let db = seed_db();
        let ask = FakeAsk::ok("x", &[]);
        let request = Request {
            jsonrpc: "2.0".into(),
            id: Some(json!(3)),
            method: "tools/call".into(),
            params: None,
        };
        let resp = handle_message(&db, &ask, &request).unwrap();
        let err = resp.error.unwrap();
        assert_eq!(err.code, INVALID_PARAMS);
        assert!(err.message.contains("missing params"));
    }

    #[test]
    fn tools_call_missing_name_is_invalid_params() {
        let db = seed_db();
        let ask = FakeAsk::ok("x", &[]);
        let request = req(json!(4), "tools/call", json!({ "arguments": {} }));
        let resp = handle_message(&db, &ask, &request).unwrap();
        let err = resp.error.unwrap();
        assert_eq!(err.code, INVALID_PARAMS);
        assert!(err.message.contains("missing tool name"));
    }

    #[test]
    fn tools_call_absent_arguments_defaults_to_empty() {
        // get_session with no `arguments` -> parsed as empty object -> missing
        // required `id` -> invalid params (not a panic).
        let db = seed_db();
        let ask = FakeAsk::ok("x", &[]);
        let request = req(json!(5), "tools/call", json!({ "name": "get_session" }));
        let resp = handle_message(&db, &ask, &request).unwrap();
        assert_eq!(resp.error.unwrap().code, INVALID_PARAMS);
    }

    // ---- Response / parsing helpers -------------------------------------

    #[test]
    fn response_to_line_is_newline_terminated_json() {
        let resp = Response::success(json!(1), json!({ "ok": true }));
        let line = resp.to_line();
        assert!(line.ends_with('\n'));
        let parsed: Response = serde_json::from_str(line.trim_end()).unwrap();
        assert_eq!(parsed, resp);
    }

    #[test]
    fn error_response_serializes_without_result_field() {
        let resp = Response::error(json!(1), METHOD_NOT_FOUND, "nope");
        let line = resp.to_line();
        assert!(!line.contains("\"result\""));
        assert!(line.contains("\"error\""));
    }

    #[test]
    fn parse_request_accepts_valid_json_rpc() {
        let req = parse_request(r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#).unwrap();
        assert_eq!(req.method, "tools/list");
        assert_eq!(req.id, Some(json!(1)));
    }

    #[test]
    fn parse_request_rejects_garbage_with_parse_error() {
        let resp = parse_request("not json").unwrap_err();
        let err = resp.error.unwrap();
        assert_eq!(err.code, -32700);
        assert_eq!(resp.id, Value::Null);
    }

    #[test]
    fn parse_request_round_trips_a_notification() {
        let req =
            parse_request(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#).unwrap();
        assert!(req.id.is_none());
        assert_eq!(req.method, "notifications/initialized");
    }
}
