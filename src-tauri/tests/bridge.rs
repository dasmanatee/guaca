//! The bridge a Claude Code session reaches its crew through.
//!
//! A `claude` session keeps its own tools and is handed exactly one of
//! Guaca's: `send_message`, over the MCP server this route speaks. What is
//! tested here is that a client speaking MCP gets the answers it expects, that
//! the route is behind the workspace's token like everything else, and that a
//! call names the turn it belongs to so a message lands in that turn's run.
//!
//! Entirely offline. Nothing spawns `claude` and no model is called: the
//! relay a running turn would open is opened by hand.

#![cfg(feature = "server")]

use std::net::SocketAddr;

use guac_lib::domain::envelope::Participant;
use guac_lib::domain::ids::{AgentId, RunId};
use serde_json::{json, Value};

const TOKEN: &str = "a-token-nobody-guessed";

/// A daemon on a free port, with a workspace of its own and a handle on its
/// runtime for opening a relay the way a running turn would.
async fn workspace() -> (SocketAddr, guac_lib::runtime::Runtime, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("a temporary workspace");
    let bound = guac_lib::server::bind(guac_lib::server::Settings {
        root: dir.path().to_path_buf(),
        bind: "127.0.0.1:0".parse().expect("a loopback address"),
        token: TOKEN.to_string(),
        web: None,
        origin: None,
        updater: None,
    })
    .await
    .expect("the workspace opens");

    let addr = bound.addr;
    let runtime = bound.runtime().clone();
    tokio::spawn(bound.serve());
    (addr, runtime, dir)
}

fn route(addr: SocketAddr, agent: AgentId, run: RunId) -> String {
    format!("http://{addr}/v1/claude-code/mcp/{agent}/{run}")
}

/// One JSON-RPC request, as Claude Code sends it.
async fn rpc(url: &str, method: &str, params: Value) -> Value {
    let response = reqwest::Client::new()
        .post(url)
        .bearer_auth(TOKEN)
        .json(&json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params }))
        .send()
        .await
        .expect("the daemon answers");
    assert_eq!(response.status(), 200);
    let kind = response.headers()[reqwest::header::CONTENT_TYPE].to_str().unwrap().to_string();
    assert!(kind.starts_with("application/json"), "answered as {kind}");
    let body: Value = response.json().await.expect("the answer is JSON");
    assert_eq!(body["jsonrpc"], "2.0");
    assert_eq!(body["id"], 1);
    body
}

/// An agent made the way the operator makes one, and its id.
async fn agent(addr: SocketAddr, name: &str) -> AgentId {
    let response = reqwest::Client::new()
        .post(format!("http://{addr}/v1/call"))
        .bearer_auth(TOKEN)
        .json(&json!({ "name": "create_agent", "args": { "draft": {
            "name": name,
            "avatar": "avocado",
            "color": "#7ab55c",
            "model": "",
            "systemPrompt": "A test agent.",
        }}}))
        .send()
        .await
        .expect("the daemon answers");
    let body: Value = response.json().await.unwrap();
    serde_json::from_value(body["ok"]["id"].clone()).expect("the agent came back with an id")
}

/// Messages from one agent to another in a run, whatever the recipient has
/// done since.
fn sent_by(runtime: &guac_lib::runtime::Runtime, from: AgentId, to: AgentId, run: RunId) -> usize {
    let between = runtime.store().pair_messages(from, to, 50).unwrap();
    between.iter().filter(|e| e.from == Participant::Agent { id: from } && e.run_id == run).count()
}

#[tokio::test]
async fn a_call_without_the_token_is_401() {
    let (addr, _runtime, _dir) = workspace().await;
    let url = route(addr, AgentId::new(), RunId::new());
    let client = reqwest::Client::new();

    for token in [None, Some("not-the-token")] {
        let mut request =
            client.post(&url).json(&json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" }));
        if let Some(token) = token {
            request = request.bearer_auth(token);
        }
        let response = request.send().await.expect("the daemon answers");
        assert_eq!(response.status(), 401, "the bridge was reachable with {token:?}");
    }
}

#[tokio::test]
async fn a_call_for_a_run_that_is_not_live_is_refused_in_the_result() {
    let (addr, _runtime, _dir) = workspace().await;
    let url = route(addr, AgentId::new(), RunId::new());

    let body = rpc(
        &url,
        "tools/call",
        json!({ "name": "send_message", "arguments": { "to": ["Pat"], "text": "hi", "intent": "work" } }),
    )
    .await;

    // The model reads the refusal; the transport did nothing wrong.
    assert!(body.get("error").is_none(), "{body}");
    assert_eq!(body["result"]["isError"], true, "{body}");
    let said = body["result"]["content"][0]["text"].as_str().unwrap_or_default();
    assert!(said.contains("no Claude Code turn is running"), "{said}");
}

#[tokio::test]
async fn tools_list_offers_exactly_send_message() {
    let (addr, _runtime, _dir) = workspace().await;
    let body = rpc(&route(addr, AgentId::new(), RunId::new()), "tools/list", json!({})).await;

    let tools = body["result"]["tools"].as_array().expect("a list of tools");
    assert_eq!(tools.len(), 1, "{body}");
    assert_eq!(tools[0]["name"], "send_message");
    assert!(tools[0]["description"].as_str().unwrap().contains("Send a message"));
    let schema = &tools[0]["inputSchema"];
    assert_eq!(schema["type"], "object");
    assert_eq!(schema["required"], json!(["to", "text", "intent"]));
}

#[tokio::test]
async fn initialize_names_the_server() {
    let (addr, _runtime, _dir) = workspace().await;
    let url = route(addr, AgentId::new(), RunId::new());

    let body = rpc(&url, "initialize", json!({ "protocolVersion": "2025-06-18" })).await;
    assert_eq!(body["result"]["serverInfo"]["name"], "guaca");
    assert_eq!(body["result"]["serverInfo"]["version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(body["result"]["protocolVersion"], "2025-06-18", "the client's version is echoed");
    assert!(body["result"]["capabilities"]["tools"].is_object());

    let body = rpc(&url, "initialize", json!({})).await;
    assert_eq!(body["result"]["protocolVersion"], "2025-03-26");
}

#[tokio::test]
async fn the_rest_of_the_protocol_is_answered_as_a_client_expects() {
    let (addr, _runtime, _dir) = workspace().await;
    let url = route(addr, AgentId::new(), RunId::new());
    let client = reqwest::Client::new();

    let body = rpc(&url, "ping", json!({})).await;
    assert_eq!(body["result"], json!({}));

    let body = rpc(&url, "resources/list", json!({})).await;
    assert_eq!(body["error"]["code"], -32601, "{body}");

    let body = rpc(&url, "tools/call", json!({ "name": "Bash", "arguments": {} })).await;
    assert_eq!(body["error"]["code"], -32602, "{body}");
    assert!(body["error"]["message"].as_str().unwrap().contains("send_message"), "{body}");

    let notified = client
        .post(&url)
        .bearer_auth(TOKEN)
        .json(&json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }))
        .send()
        .await
        .unwrap();
    assert_eq!(notified.status(), 202);
    assert!(notified.bytes().await.unwrap().is_empty());

    let garbled = client.post(&url).bearer_auth(TOKEN).body("{not json").send().await.unwrap();
    assert_eq!(garbled.status(), 200);
    let body: Value = garbled.json().await.unwrap();
    assert_eq!(body["error"]["code"], -32700, "{body}");

    // A client that wants a stream of server messages is told there is none.
    let listened = client.get(&url).bearer_auth(TOKEN).send().await.unwrap();
    assert_eq!(listened.status(), 405);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_live_relay_queues_a_message_for_the_peer() {
    let (addr, runtime, _dir) = workspace().await;
    let dana = agent(addr, "Dana").await;
    let pat = agent(addr, "Pat").await;
    let run = RunId::new();
    runtime.open_relay_for_tests(dana, run);

    let body = rpc(
        &route(addr, dana, run),
        "tools/call",
        json!({ "name": "send_message", "arguments": { "to": ["Pat"], "text": "hi", "intent": "work" } }),
    )
    .await;

    assert_eq!(body["result"]["isError"], false, "{body}");
    let said = body["result"]["content"][0]["text"].as_str().unwrap_or_default();
    assert!(said.contains("Pat"), "{said}");
    let sent = sent_by(&runtime, dana, pat, run);
    assert_eq!(sent, 1, "one envelope went from Dana to Pat, in the turn's run");

    // An agent with no turn of its own in the run cannot speak through Dana's.
    let lee = agent(addr, "Lee").await;
    let body = rpc(
        &route(addr, lee, run),
        "tools/call",
        json!({ "name": "send_message", "arguments": { "to": ["Pat"], "text": "hi", "intent": "work" } }),
    )
    .await;
    assert_eq!(body["result"]["isError"], true, "{body}");
    let said = body["result"]["content"][0]["text"].as_str().unwrap_or_default();
    assert!(said.contains("belongs to another agent"), "{said}");

    // Arguments the parser refuses come back as a refusal the model can read.
    let body = rpc(
        &route(addr, dana, run),
        "tools/call",
        json!({ "name": "send_message", "arguments": { "to": ["Pat"] } }),
    )
    .await;
    assert_eq!(body["result"]["isError"], true, "{body}");
    let said = body["result"]["content"][0]["text"].as_str().unwrap_or_default();
    assert!(said.contains("send_message arguments were not understood"), "{said}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_turns_in_one_run_each_send_as_themselves() {
    // A run is the whole cascade, so Dana and Pat answering each other are
    // two live turns under one run id, and neither may displace the other.
    let (addr, runtime, _dir) = workspace().await;
    let dana = agent(addr, "Dana").await;
    let pat = agent(addr, "Pat").await;
    let run = RunId::new();
    runtime.open_relay_for_tests(dana, run);
    runtime.open_relay_for_tests(pat, run);

    for (from, to) in [(dana, "Pat"), (pat, "Dana")] {
        let body = rpc(
            &route(addr, from, run),
            "tools/call",
            json!({ "name": "send_message", "arguments": { "to": [to], "text": "hi", "intent": "work" } }),
        )
        .await;
        assert_eq!(body["result"]["isError"], false, "{body}");
    }
    assert_eq!(sent_by(&runtime, dana, pat, run), 1);
    assert_eq!(sent_by(&runtime, pat, dana, run), 1);
}
