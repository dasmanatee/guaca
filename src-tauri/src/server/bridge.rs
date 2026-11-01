//! The MCP server a Claude Code session reaches its crew through.
//!
//! A `claude` session keeps its own tools, and the one thing it cannot do with
//! them is talk to the agents beside it. This is the smallest MCP server that
//! closes that gap: the streamable-HTTP transport answered with plain JSON,
//! never a stream, and exactly one tool, `send_message`, sent as the turn that
//! started the session would have sent it.
//!
//! The URL names the agent and the run, and the token is the workspace's own.
//! A session is told nothing it could use to speak as anybody else: the run is
//! checked against the agent it belongs to, and a run that has ended has no
//! relay left to send through.

use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Value};

use super::{authorized, Serving};
use crate::domain::ids::{AgentId, RunId};
use crate::llm::tools;

/// What a client that sends no version of its own is answered with: the
/// revision that introduced the streamable-HTTP transport this speaks.
const PROTOCOL_VERSION: &str = "2025-03-26";

/// One JSON-RPC message from the session.
pub(super) async fn call(
    State(serving): State<Serving>,
    Path((agent_id, run_id)): Path<(AgentId, RunId)>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if let Err(response) = authorized(&serving, &headers, None) {
        return *response;
    }

    let Ok(request) = serde_json::from_slice::<Value>(&body) else {
        return error(Value::Null, -32700, "the body is not JSON");
    };
    let Some(method) = request.get("method").and_then(Value::as_str) else {
        return error(
            request.get("id").cloned().unwrap_or(Value::Null),
            -32600,
            "expected one JSON-RPC request with a method",
        );
    };
    // A notification wants no answer, and the transport says to acknowledge
    // one with an empty 202.
    let Some(id) = request.get("id").cloned() else {
        return StatusCode::ACCEPTED.into_response();
    };
    let params = request.get("params").cloned().unwrap_or(Value::Null);

    match method {
        "initialize" => {
            let version =
                params.get("protocolVersion").and_then(Value::as_str).unwrap_or(PROTOCOL_VERSION);
            result(
                id,
                json!({
                    "protocolVersion": version,
                    "capabilities": { "tools": {} },
                    "serverInfo": { "name": "guaca", "version": env!("CARGO_PKG_VERSION") },
                }),
            )
        }
        "ping" => result(id, json!({})),
        "tools/list" => result(id, json!({ "tools": [send_message()] })),
        "tools/call" => {
            let name = params.get("name").and_then(Value::as_str).unwrap_or_default();
            if name != tools::SEND_MESSAGE {
                return error(
                    id,
                    -32602,
                    &format!(
                        "there is no tool called {name:?} here; this server offers only {}",
                        tools::SEND_MESSAGE
                    ),
                );
            }
            let arguments = params.get("arguments").cloned().unwrap_or_else(|| json!({}));
            let (text, failed) =
                match serving.state.runtime.relay_send_message(agent_id, run_id, arguments) {
                    Ok(rendered) => (rendered, false),
                    Err(err) => (err.to_string(), true),
                };
            result(id, json!({ "content": [{ "type": "text", "text": text }], "isError": failed }))
        }
        _ => error(id, -32601, "method not found"),
    }
}

/// `send_message` as Guaca offers it to every other agent, less `files`.
///
/// The relay sends text only. Offered, `files` would be a parameter it can
/// only refuse, and a model shown it reasonably expects it to work.
fn send_message() -> Value {
    let spec = tools::spec(tools::SEND_MESSAGE).expect("send_message is one of Guaca's tools");
    let mut schema = spec.parameters;
    if let Some(properties) = schema.get_mut("properties").and_then(Value::as_object_mut) {
        properties.remove("files");
    }
    json!({ "name": spec.name, "description": spec.description, "inputSchema": schema })
}

fn result(id: Value, result: Value) -> Response {
    Json(json!({ "jsonrpc": "2.0", "id": id, "result": result })).into_response()
}

fn error(id: Value, code: i64, message: &str) -> Response {
    Json(json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } }))
        .into_response()
}
