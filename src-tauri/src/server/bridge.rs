//! The MCP server a Claude Code session reaches Guaca through.
//!
//! A `claude` session keeps its own tools, and what it cannot do with them is
//! anything with its crew: message a peer, read the directory, make an agent,
//! keep a schedule, write to the notebook, ask the operator. This is the
//! smallest MCP server that closes that gap: the streamable-HTTP transport
//! answered with plain JSON, never a stream, offering exactly the tools the
//! turn that started the session would have offered a model, and running each
//! one through the same dispatch every other agent's calls take.
//!
//! The URL names the agent and the run, and the token is the bridge's own,
//! minted when the server starts and refused everywhere else. The workspace's
//! token is refused here in turn, so nothing a session holds opens the rest of
//! the server and nothing that opens the rest of the server is needed here. A
//! session is told nothing it could use to speak as anybody else: the run is
//! checked against the agent it belongs to, and a run that has ended has no
//! relay left to call through.

use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Value};

use super::{bearer, same, Serving};
use crate::domain::ids::{AgentId, RunId};
use crate::llm::openrouter::ToolSpec;

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
    if !bearer(&headers).is_some_and(|token| same(token, &serving.bridge_token)) {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({ "err": {
                "kind": "unauthorized",
                "message": "this route takes the token Guaca gives a Claude Code session for its \
                            turn, not the workspace's. Only a session Guaca started can call it",
            }})),
        )
            .into_response();
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
        "tools/list" => match serving.state.runtime.relay_tools(agent_id, run_id).await {
            Ok(offered) => result(
                id,
                json!({ "tools": offered.into_iter().map(mcp_tool).collect::<Vec<_>>() }),
            ),
            // The program lists tools as the session starts, while the turn's
            // relay is open; a list asked for by anything else is nobody's.
            Err(err) => error(id, -32602, &err.to_string()),
        },
        "tools/call" => {
            let name = params.get("name").and_then(Value::as_str).unwrap_or_default();
            let arguments = params.get("arguments").cloned().unwrap_or_else(|| json!({}));
            match serving.state.runtime.relay_call(agent_id, run_id, name, arguments).await {
                Ok(answer) => {
                    let mut content = vec![json!({ "type": "text", "text": answer.text })];
                    if let Some(block) = answer.image.as_deref().and_then(image_block) {
                        content.push(block);
                    }
                    result(id, json!({ "content": content, "isError": answer.failed }))
                }
                // A tool this server does not have is the protocol's error, as
                // a client expects; everything a tool itself says, a refusal
                // included, is a result the model reads.
                Err(err @ crate::runtime::RelayError::NoSuchTool { .. }) => {
                    error(id, -32602, &err.to_string())
                }
                Err(err) => result(
                    id,
                    json!({ "content": [{ "type": "text", "text": err.to_string() }], "isError": true }),
                ),
            }
        }
        _ => error(id, -32601, "method not found"),
    }
}

/// One of Guaca's tools in the shape the protocol lists it.
fn mcp_tool(spec: ToolSpec) -> Value {
    json!({ "name": spec.name, "description": spec.description, "inputSchema": spec.parameters })
}

/// A picture a tool answered with, as the protocol carries one. The runtime
/// renders pictures as data URLs; the protocol wants the bytes and the type
/// apart.
fn image_block(data_url: &str) -> Option<Value> {
    let rest = data_url.strip_prefix("data:")?;
    let (mime, data) = rest.split_once(";base64,")?;
    Some(json!({ "type": "image", "data": data, "mimeType": mime }))
}

fn result(id: Value, result: Value) -> Response {
    Json(json!({ "jsonrpc": "2.0", "id": id, "result": result })).into_response()
}

fn error(id: Value, code: i64, message: &str) -> Response {
    Json(json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } }))
        .into_response()
}
