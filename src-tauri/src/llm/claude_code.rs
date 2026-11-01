//! The Claude Code provider, where an agent is a whole `claude` session.
//!
//! [`super::claude`] runs the program as a model: tools off, settings ignored,
//! one call per turn, so that Guaca's own loop stays the agent. This provider
//! reverses every one of those isolation flags. The session keeps its own
//! tools, its own working directory and its own memory, because that is what
//! the operator is asking for by choosing it; Guaca carries messages in and
//! out and no longer decides what the agent may do.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use serde::Deserialize;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use crate::config::InferenceConfig;
use crate::domain::envelope::{Part, ToolOutcome};
use crate::domain::ids::AgentId;
use crate::llm::openrouter::{
    ChatMessage, ChatRequest, Completion, LlmError, Token, Usage, UserContent, THOUGHT_BREAK,
};
use crate::llm::tools::SEND_MESSAGE;

/// What one Claude Code turn needs to know about who it is speaking for.
///
/// Carried on the request rather than held by the client, because the client is
/// shared by every agent and these belong to exactly one.
#[derive(Debug, Clone, PartialEq)]
pub struct Session {
    pub agent_id: AgentId,
    pub cwd: PathBuf,
    pub resume: Option<String>,
    /// First turn only.
    pub system_prompt: Option<String>,
    pub bridge: Option<Bridge>,
}

/// Where the session reaches Guaca's own tools, and the credential for it.
#[derive(Debug, Clone, PartialEq)]
pub struct Bridge {
    pub url: String,
    pub token: String,
}

/// The program that is run, found on `PATH` like [`super::claude::PROGRAM`].
pub const PROGRAM: &str = "claude";

/// What this provider reports as its model.
///
/// A label, not a model id. Which model runs is the session's own choice, as
/// in a terminal, and this app does not pass `--model`, so there is nothing
/// truthful to put here but the name of the thing that decided.
pub const MODEL_LABEL: &str = "Claude Code's own model";

/// What the operator types in the channel to start the agent over.
pub const CLEAR: &str = "/clear";

/// What the channel is told when that happens, in place of a turn.
pub const CLEARED: &str = "Session cleared. The next message starts a fresh Claude Code session.";

/// The name the bridge is registered under in the session's MCP config, which
/// is also what the program prefixes its tools with.
pub const MCP_SERVER: &str = "guaca";

/// The bridge's one tool as the program names it.
pub const MCP_SEND: &str = "mcp__guaca__send_message";

/// How long the program may go without writing a line before it is stopped.
///
/// Between lines rather than for the whole turn, because a coding session can
/// legitimately run for an hour and still be working: it writes a frame per
/// tool it runs. An hour of nothing at all is a process that is stuck, and the
/// operator's plan is still open under it.
const IDLE: Duration = Duration::from_secs(60 * 60);

/// How long the no-answer report waits for stderr to close once the program
/// has exited. `stream` says what can hold it open.
const STDERR_GRACE: Duration = Duration::from_secs(3);

/// How much of a tool's output is kept on its chip. The session holds the
/// whole of it; the transcript only has to say what happened.
const SUMMARY_CHARS: usize = 200;

/// The label `runtime::prompt::render_incoming` puts on the operator's own
/// lines, which is what tells a command the operator typed from one a crewmate
/// quoted.
const OPERATOR: &str = "[OPERATOR]";

/// The argument vector for one turn.
///
/// Every isolation flag [`super::claude::argv`] sets is absent here on purpose,
/// and the tests beside this file hold that line: no `--tools ""`, no
/// `--strict-mcp-config`, no `--setting-sources ""`, no
/// `--disable-slash-commands`, no `--no-session-persistence`. What is set
/// instead:
///
/// - **`--dangerously-skip-permissions`** because nobody is at a terminal to
///   answer a permission prompt. A session that asked would wait for the idle
///   limit and then be killed.
/// - **`--session-id`** on a first turn, so the id is this app's to name
///   rather than something read back out of a stream that may never finish,
///   and **`--resume`** on every turn after, which is where the session's
///   memory lives.
/// - **`--append-system-prompt`** only on the first turn. A resumed session
///   already has it, and appending it again would stack a copy per turn.
/// - **`--mcp-config`** when the bridge is up, so the session can reach its
///   crewmates. Merged with the operator's own servers rather than replacing
///   them, because those are part of what the operator chose this for.
///
/// No `--model`. The session picks its model exactly as it does in a terminal,
/// from the operator's settings or a `/model` sent in the channel, and a flag
/// here would override both on every turn.
pub fn argv(session: &Session) -> Vec<String> {
    let mut args: Vec<String> = [
        "-p",
        "--input-format",
        "stream-json",
        "--output-format",
        "stream-json",
        "--verbose",
        "--dangerously-skip-permissions",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();

    match &session.resume {
        Some(id) => args.extend(["--resume".to_string(), id.clone()]),
        None => args.extend(["--session-id".to_string(), uuid::Uuid::new_v4().to_string()]),
    }

    if let Some(prompt) = &session.system_prompt {
        args.extend(["--append-system-prompt".to_string(), prompt.clone()]);
    }

    if let Some(bridge) = &session.bridge {
        let config = serde_json::json!({
            "mcpServers": {
                MCP_SERVER: {
                    "type": "http",
                    "url": bridge.url,
                    "headers": { "Authorization": format!("Bearer {}", bridge.token) },
                },
            },
        });
        args.extend(["--mcp-config".to_string(), config.to_string()]);
    }

    args
}

/// The messages after the last assistant turn, which is what the session has
/// not seen.
///
/// Everything before it is already in the session's own memory, and sending it
/// again would be the session reading its whole history twice a turn.
pub fn batch(messages: &[ChatMessage]) -> &[ChatMessage] {
    let after = messages
        .iter()
        .rposition(|message| matches!(message, ChatMessage::Assistant { .. }))
        .map_or(0, |at| at + 1);
    &messages[after..]
}

/// The slash command an operator line carries, without the label the runtime
/// wrote on it.
///
/// Only the operator's: a crewmate's message is labeled as theirs, so a
/// `/clear` it quotes is text and not a command.
fn operator_command(message: &ChatMessage) -> Option<&str> {
    let ChatMessage::User { content: UserContent::Text(body) } = message else { return None };
    let said = body.strip_prefix(OPERATOR).unwrap_or(body).trim();
    said.starts_with('/').then_some(said)
}

/// Whether the batch is exactly one operator line reading `/clear`.
///
/// The system prompt is not a line anyone typed, so it does not count against
/// "exactly one".
pub fn is_clear(batch: &[ChatMessage]) -> bool {
    let mut lines = batch.iter().filter(|m| !matches!(m, ChatMessage::System { .. }));
    matches!((lines.next(), lines.next()), (Some(only), None) if operator_command(only) == Some(CLEAR))
}

/// The stdin lines for a batch, one user message per message.
///
/// The system prompt is not among them: it goes on the command line, once.
/// An operator's slash command goes without its label, because the program
/// only recognizes one at the start of the message.
fn payload(batch: &[ChatMessage]) -> Vec<String> {
    batch
        .iter()
        .filter_map(|message| {
            let blocks = match operator_command(message) {
                Some(command) => vec![serde_json::json!({ "type": "text", "text": command })],
                None => super::claude::conversation(std::slice::from_ref(message)).1,
            };
            (!blocks.is_empty()).then(|| {
                serde_json::json!({
                    "type": "user",
                    "message": { "role": "user", "content": blocks },
                })
                .to_string()
            })
        })
        .collect()
}

/// What one turn's stream has said so far.
#[derive(Debug, Default)]
pub struct Fold {
    /// Every text block the session wrote, in order. The reply is the whole
    /// turn's narration and not only the last message, because the channel is
    /// the only place the operator reads it.
    pub content: String,
    pub work: Vec<Part>,
    pub usage: Option<Usage>,
    pub session: Option<String>,
    /// The program's own words, when the result frame says the turn failed.
    pub failed: Option<String>,
    /// Tool uses waiting for their result, by id: the result names the call
    /// and not the tool, so the name and input are carried until it arrives.
    open: BTreeMap<String, (String, serde_json::Value)>,
    answered: bool,
    /// Whether a local command answered in its own words. `/compact` usually
    /// does, and when it does the program's line is the reply and nothing is
    /// added to it.
    said_locally: bool,
    /// How a compaction ended, from its status frame, and the token counts its
    /// boundary frame reported. The reply when the program's own line never
    /// came, so a compaction is never answered with silence.
    compacted: Option<String>,
    compacted_tokens: Option<(u64, u64)>,
}

impl Fold {
    /// Adds a piece of the reply, kept apart from what came before it.
    fn say<F: FnMut(Token<'_>)>(&mut self, text: &str, on_token: &mut F) {
        if !self.content.is_empty() {
            self.content.push_str(THOUGHT_BREAK);
            on_token(Token::Text(THOUGHT_BREAK));
        }
        self.content.push_str(text);
        on_token(Token::Text(text));
    }

    /// The sentence a compaction is answered with when the program wrote none.
    fn compaction(&self) -> Option<String> {
        Some(match (self.compacted.as_deref()?, self.compacted_tokens) {
            ("success", Some((before, after))) => {
                format!("Compacted the session: {before} → {after} tokens.")
            }
            ("success", None) => "Compacted the session.".to_string(),
            (other, _) => format!("Compaction failed: {other}."),
        })
    }
}

/// What the program wraps a local command's output in when it replays it as a
/// user message.
const LOCAL_STDOUT: (&str, &str) = ("<local-command-stdout>", "</local-command-stdout>");

/// The result frame, as far as this app reads it.
#[derive(Debug, Default, Deserialize)]
struct ResultFrame {
    #[serde(default)]
    is_error: bool,
    #[serde(default)]
    result: Option<String>,
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default)]
    usage: Option<ProgramUsage>,
    /// Set when the turn was a slash command such as `/compact`, which writes
    /// no assistant message and answers in `result` alone. On a model turn
    /// `result` is the last text block again and is not read.
    #[serde(default)]
    local_command: Option<serde_json::Value>,
}

/// The program counts cache tokens apart from fresh ones. All three were read.
#[derive(Debug, Default, Deserialize)]
struct ProgramUsage {
    #[serde(default)]
    input_tokens: u32,
    #[serde(default)]
    cache_creation_input_tokens: u32,
    #[serde(default)]
    cache_read_input_tokens: u32,
    #[serde(default)]
    output_tokens: u32,
}

/// The text of a tool result, which the program writes either as a string or
/// as a list of text blocks.
fn result_text(content: Option<&serde_json::Value>) -> String {
    match content {
        Some(serde_json::Value::String(text)) => text.clone(),
        Some(serde_json::Value::Array(blocks)) => blocks
            .iter()
            .filter_map(|block| block.get("text").and_then(serde_json::Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// Folds one stdout line into the turn. Returns `Ok(true)` when it was the
/// result frame.
///
/// The session's tool uses become [`Part`]s in `work` and never tool calls:
/// they have already run, and the runtime dispatching them again would run
/// every command twice.
pub fn fold<F: FnMut(Token<'_>)>(
    line: &str,
    state: &mut Fold,
    on_token: &mut F,
) -> Result<bool, LlmError> {
    let Ok(event) = serde_json::from_str::<serde_json::Value>(line) else {
        // The operator's hooks run in this session and some of them print.
        tracing::debug!(line, "{PROGRAM} wrote a line that is not JSON");
        return Ok(false);
    };
    let blocks = || {
        event
            .pointer("/message/content")
            .and_then(serde_json::Value::as_array)
            .into_iter()
            .flatten()
    };
    let str_at = |block: &serde_json::Value, key: &str| {
        block.get(key).and_then(serde_json::Value::as_str).map(str::to_string)
    };

    match event.get("type").and_then(serde_json::Value::as_str) {
        Some("assistant") => {
            for block in blocks() {
                match block.get("type").and_then(serde_json::Value::as_str) {
                    Some("text") => {
                        let Some(text) = str_at(block, "text").filter(|t| !t.is_empty()) else {
                            continue;
                        };
                        state.say(&text, on_token);
                    }
                    Some("thinking") => {
                        if let Some(text) = str_at(block, "thinking").filter(|t| !t.is_empty()) {
                            on_token(Token::Reasoning(&text));
                        }
                    }
                    Some("tool_use") => {
                        let Some(id) = str_at(block, "id") else { continue };
                        let name = str_at(block, "name").unwrap_or_default();
                        // Drawn as this app's own tool, which is what it is.
                        let name = if name == MCP_SEND { SEND_MESSAGE.to_string() } else { name };
                        let input = block.get("input").cloned().unwrap_or_default();
                        let (name, input) = state.open.entry(id.clone()).or_insert((name, input));
                        on_token(Token::ToolStarted { call_id: &id, name, arguments: input });
                    }
                    _ => {}
                }
            }
            Ok(false)
        }
        Some("user") => {
            // A string rather than blocks is the program replaying something
            // as the user: a local command's output, which is its answer, or a
            // compaction's summary, which is the session's memory and not a
            // reply to anyone.
            if let Some(text) = event.pointer("/message/content").and_then(|c| c.as_str()) {
                let output = text
                    .trim()
                    .strip_prefix(LOCAL_STDOUT.0)
                    .and_then(|rest| rest.strip_suffix(LOCAL_STDOUT.1))
                    .map(str::trim)
                    .filter(|output| !output.is_empty());
                if let Some(output) = output {
                    state.say(output, on_token);
                    state.said_locally = true;
                }
                return Ok(false);
            }
            for block in blocks() {
                if block.get("type").and_then(serde_json::Value::as_str) != Some("tool_result") {
                    continue;
                }
                let Some(id) = str_at(block, "tool_use_id") else { continue };
                let Some((name, input)) = state.open.remove(&id) else { continue };
                let said: String =
                    result_text(block.get("content")).chars().take(SUMMARY_CHARS).collect();
                let outcome =
                    if block.get("is_error").and_then(serde_json::Value::as_bool) == Some(true) {
                        ToolOutcome::Failed { error: said }
                    } else {
                        ToolOutcome::Ok { summary: said }
                    };
                state.work.push(Part::tool_call(name, input, outcome));
                if let Some(part) = state.work.last() {
                    on_token(Token::ToolFinished { call_id: &id, part });
                }
            }
            Ok(false)
        }
        Some("result") => {
            let frame: ResultFrame = serde_json::from_value(event)
                .map_err(|err| LlmError::Decode(format!("{PROGRAM} result frame: {err}")))?;
            let said = frame.result.unwrap_or_default();

            // Cost is left out for the reason `ProgramUsage::tally` in
            // `llm/claude.rs` gives: on a plan it is a price, not a charge.
            state.usage = frame.usage.map(|u| Usage {
                prompt_tokens: u
                    .input_tokens
                    .saturating_add(u.cache_creation_input_tokens)
                    .saturating_add(u.cache_read_input_tokens),
                completion_tokens: u.output_tokens,
                cost: None,
            });
            if frame.session_id.is_some() {
                state.session = frame.session_id;
            }
            if frame.is_error {
                let said = said.trim();
                state.failed =
                    Some(if said.is_empty() { "it reported no reason" } else { said }.to_string());
            } else if !state.said_locally {
                // In order of whose words they are: the program's result, then
                // this app's account of a compaction it saw happen.
                if frame.local_command.is_some() && !said.is_empty() {
                    state.say(&said, on_token);
                } else if let Some(sentence) = state.compaction() {
                    state.say(&sentence, on_token);
                }
            }
            state.answered = true;
            Ok(true)
        }
        Some("system") => {
            if let Some(ended) = event.get("compact_result") {
                state.compacted = Some(ended.as_str().unwrap_or("no reason given").to_string());
            }
            if let Some(meta) = event.get("compact_metadata") {
                let count = |key: &str| meta.get(key).and_then(serde_json::Value::as_u64);
                state.compacted_tokens = count("pre_tokens").zip(count("post_tokens"));
            }
            Ok(false)
        }
        _ => Ok(false),
    }
}

/// Runs one turn of the agent's session.
///
/// Reached from `LlmClient::stream_chat` and nowhere else. Neither the config
/// nor `request.model` is read: the config only said which provider, and the
/// model is the session's own choice, as [`argv`] explains.
pub async fn stream<F: FnMut(Token<'_>)>(
    _cfg: &InferenceConfig,
    request: &ChatRequest,
    on_token: &mut F,
) -> Result<Completion, LlmError> {
    let Some(session) = &request.claude_code else {
        return Err(LlmError::ProgramFailed {
            program: PROGRAM,
            message: "this turn was not given a Claude Code session; the runtime should have \
                      attached one. This is a bug in Guaca."
                .to_string(),
        });
    };

    let batch = batch(&request.messages);
    if is_clear(batch) {
        return Ok(Completion {
            content: CLEARED.into(),
            session: Some(String::new()),
            ..Default::default()
        });
    }

    // Checked here rather than left to the spawn, which reports a missing
    // working directory as a missing program and would tell the operator to
    // install something they have.
    if !session.cwd.is_dir() {
        return Err(LlmError::ProgramFailed {
            program: PROGRAM,
            message: format!(
                "working directory {} does not exist. Edit the agent and point it at a folder \
                 that exists.",
                session.cwd.display()
            ),
        });
    }

    let args = argv(session);
    // The id a first turn named, which is the session to keep if the result
    // frame never says otherwise.
    let named =
        args.iter().position(|arg| arg == "--session-id").and_then(|at| args.get(at + 1)).cloned();

    let mut child = tokio::process::Command::new(PROGRAM)
        .args(&args)
        .current_dir(&session.cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // What a stop does to a turn in flight: the runtime drops this future
        // and the drop kills the session's process. Only that process: a
        // command it started is in the same process group and is not signaled,
        // so a long build the session began can outlive the stop.
        .kill_on_drop(true)
        .spawn()
        .map_err(|err| match err.kind() {
            std::io::ErrorKind::NotFound => LlmError::ProgramMissing { program: PROGRAM },
            _ => LlmError::ProgramFailed { program: PROGRAM, message: err.to_string() },
        })?;

    // From a task of its own, for the reason `claude::stream` gives: a
    // screenshot is bigger than a pipe.
    let lines = payload(batch);
    let mut stdin = child.stdin.take();
    let writer = tokio::spawn(async move {
        if let Some(stdin) = stdin.as_mut() {
            for line in lines {
                let _ = stdin.write_all(line.as_bytes()).await;
                let _ = stdin.write_all(b"\n").await;
            }
            let _ = stdin.flush().await;
        }
        // Dropped here, which is the end of input the program waits for.
        drop(stdin);
    });

    // Drained as it arrives rather than after: an hour-long session that fills
    // the pipe with warnings would otherwise block on its next one and look
    // exactly like a session that is thinking.
    // Kept in a buffer shared with the reader rather than returned by it, so
    // that what arrived is still there when the reader is given up on below.
    let stderr = child.stderr.take();
    let complaints = Arc::new(Mutex::new(String::new()));
    let mut drain = tokio::spawn({
        let complaints = Arc::clone(&complaints);
        async move {
            let Some(stderr) = stderr else { return };
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let mut said = complaints.lock();
                said.push_str(&line);
                said.push('\n');
            }
        }
    });

    let stdout = child.stdout.take().ok_or(LlmError::ProgramFailed {
        program: PROGRAM,
        message: "the program produced no output stream".to_string(),
    })?;

    let mut state = Fold::default();
    let mut reader = BufReader::new(stdout).lines();
    loop {
        let line = match tokio::time::timeout(IDLE, reader.next_line()).await {
            // Not `Timeout`: that is retried, and a retry would resend the
            // batch into a session that may still be holding it.
            Err(_) => {
                let _ = child.start_kill();
                return Err(LlmError::ProgramFailed {
                    program: PROGRAM,
                    message: format!(
                        "{PROGRAM} wrote nothing for {} minutes and was stopped",
                        IDLE.as_secs() / 60
                    ),
                });
            }
            Ok(read) => read.map_err(|err| {
                LlmError::Decode(format!("could not read what {PROGRAM} wrote: {err}"))
            })?,
        };
        let Some(line) = line else { break };
        fold(&line, &mut state, on_token)?;
    }

    writer.abort();
    let status = child.wait().await.ok();

    // No result frame: the exit status and stderr are the only account there
    // is. The commonest cause is a stored session the program no longer has,
    // and its own words say so.
    if !state.answered {
        // Bounded, because the end of the pipe is not the end of the program.
        // The session's MCP servers and the commands it ran inherit its stderr,
        // which this provider allows on purpose, and one left behind holds the
        // pipe open after the program is gone. What was read by then is the
        // program's own account, and it is enough.
        if tokio::time::timeout(STDERR_GRACE, &mut drain).await.is_err() {
            drain.abort();
        }
        let said = complaints.lock().trim().to_string();
        let code = status
            .and_then(|s| s.code())
            .map(|c| format!("exit {c}"))
            .unwrap_or_else(|| "no exit status".to_string());
        let detail = if said.is_empty() { code } else { format!("{code}: {said}") };
        return Err(LlmError::ProgramFailed {
            program: PROGRAM,
            message: format!("{PROGRAM} ended without answering ({detail})"),
        });
    }
    drain.abort();

    if let Some(said) = state.failed {
        return Err(LlmError::ProgramFailed { program: PROGRAM, message: said });
    }

    Ok(Completion {
        content: state.content,
        usage: state.usage,
        work: state.work,
        session: state.session.or(named),
        ..Default::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session() -> Session {
        Session {
            agent_id: AgentId::new(),
            cwd: std::env::temp_dir(),
            resume: None,
            system_prompt: None,
            bridge: None,
        }
    }

    fn value_after<'a>(args: &'a [String], flag: &str) -> Option<&'a str> {
        let at = args.iter().position(|arg| arg == flag)?;
        args.get(at + 1).map(String::as_str)
    }

    /// Folds lines into a fresh state, keeping what the tokens said in order.
    fn fold_all(lines: &[&str]) -> (Fold, Vec<String>) {
        let mut state = Fold::default();
        let mut seen = Vec::new();
        for line in lines {
            fold(line, &mut state, &mut |token| {
                seen.push(match token {
                    Token::Text(text) => format!("text:{text}"),
                    Token::Reasoning(text) => format!("reasoning:{text}"),
                    Token::ToolStarted { call_id, name, .. } => format!("started:{call_id}:{name}"),
                    Token::ToolFinished { call_id, .. } => format!("finished:{call_id}"),
                })
            })
            .unwrap();
        }
        (state, seen)
    }

    const TOOL_USE: &str = r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","id":"toolu_x","name":"Bash","input":{"command":"echo hi"}}]},"session_id":"s-1"}"#;
    const TOOL_RESULT: &str = r#"{"type":"user","message":{"role":"user","content":[{"tool_use_id":"toolu_x","type":"tool_result","content":"hi","is_error":false}]},"session_id":"s-1"}"#;
    const RESULT: &str = r#"{"type":"result","subtype":"success","is_error":false,"session_id":"s-1","result":"hi","total_cost_usd":0.005,"usage":{"input_tokens":4,"cache_creation_input_tokens":23145,"cache_read_input_tokens":42631,"output_tokens":128}}"#;

    #[tokio::test]
    async fn a_turn_without_a_session_is_a_bug_not_a_spawn() {
        let request = ChatRequest {
            messages: vec![ChatMessage::user("hello")],
            claude_code: None,
            ..ChatRequest::default()
        };
        let err = stream(&InferenceConfig::default(), &request, &mut |_| {})
            .await
            .expect_err("a turn with nowhere to run cannot run");
        assert!(matches!(err, LlmError::ProgramFailed { .. }), "{err}");
        assert!(err.to_string().contains("bug"), "{err}");
    }

    #[test]
    fn an_error_result_is_the_programs_own_words() {
        let (state, _) =
            fold_all(&[r#"{"type":"result","is_error":true,"result":"Not logged in"}"#]);
        assert_eq!(state.failed.as_deref(), Some("Not logged in"));
    }

    #[test]
    fn noise_that_is_not_json_is_skipped_rather_than_fatal() {
        let mut state = Fold::default();
        assert!(!fold("hook_started: SessionStart", &mut state, &mut |_| {}).unwrap());
        assert!(!fold(r#"{"type":"system","subtype":"init"}"#, &mut state, &mut |_| {}).unwrap());
        assert!(!fold(r#"{"type":"rate_limit_event"}"#, &mut state, &mut |_| {}).unwrap());
    }

    #[test]
    fn argv_resumes_a_stored_session_and_never_isolates() {
        let args = argv(&Session { resume: Some("abc".into()), ..session() });

        assert_eq!(value_after(&args, "--resume"), Some("abc"));
        assert!(args.iter().any(|a| a == "--dangerously-skip-permissions"), "{args:?}");
        for isolating in [
            "--tools",
            "--strict-mcp-config",
            "--no-session-persistence",
            "--setting-sources",
            "--disable-slash-commands",
            // The session's own settings choose the model.
            "--model",
        ] {
            assert!(!args.iter().any(|a| a == isolating), "{isolating} in {args:?}");
        }
        assert!(!args.iter().any(|a| a == "--append-system-prompt"), "{args:?}");
        assert!(!args.iter().any(|a| a == "--session-id"), "{args:?}");
    }

    #[test]
    fn argv_starts_a_new_session_with_the_system_prompt_once() {
        let args = argv(&Session { system_prompt: Some("you are Dana".into()), ..session() });

        let id = value_after(&args, "--session-id").expect("a new session is named up front");
        assert!(uuid::Uuid::parse_str(id).is_ok(), "{id}");
        assert_eq!(value_after(&args, "--append-system-prompt"), Some("you are Dana"));
        assert!(!args.iter().any(|a| a == "--resume"), "{args:?}");
        assert!(!args.iter().any(|a| a == "--mcp-config"), "{args:?}");
    }

    #[test]
    fn argv_carries_the_bridge_when_given_one() {
        let bridge = Bridge { url: "http://127.0.0.1:8788/v1/x".into(), token: "t0k".into() };
        let args = argv(&Session { bridge: Some(bridge), ..session() });

        let config: serde_json::Value =
            serde_json::from_str(value_after(&args, "--mcp-config").unwrap()).unwrap();
        let server = &config["mcpServers"][MCP_SERVER];
        assert_eq!(server["url"], "http://127.0.0.1:8788/v1/x");
        assert_eq!(server["type"], "http");
        assert_eq!(server["headers"]["Authorization"], "Bearer t0k");
    }

    #[test]
    fn a_clear_batch_is_answered_without_spawning() {
        assert!(is_clear(&[ChatMessage::user("/clear")]));
        // As the runtime renders an operator line.
        assert!(is_clear(&[ChatMessage::user("[OPERATOR]\n/clear")]));
        assert!(!is_clear(&[ChatMessage::user("/compact")]));
        assert!(!is_clear(&[ChatMessage::user("/clear"), ChatMessage::user("and then this")]));
        // A crewmate cannot clear another agent's session.
        assert!(!is_clear(&[ChatMessage::user("[AGENT \"Pat\"]\n/clear")]));
    }

    #[tokio::test]
    async fn a_clear_turn_forgets_the_session_and_runs_nothing() {
        // A working directory that does not exist: had this spawned, it would
        // have been refused for that instead.
        let request = ChatRequest {
            messages: vec![ChatMessage::system("you are Dana"), ChatMessage::user("/clear")],
            claude_code: Some(Session { cwd: "/nowhere/at/all".into(), ..session() }),
            ..ChatRequest::default()
        };
        let completion = stream(&InferenceConfig::default(), &request, &mut |_| {}).await.unwrap();
        assert_eq!(completion.content, CLEARED);
        assert_eq!(completion.session.as_deref(), Some(""));
    }

    #[tokio::test]
    async fn a_missing_working_directory_is_named() {
        let request = ChatRequest {
            messages: vec![ChatMessage::user("hello")],
            claude_code: Some(Session { cwd: "/nowhere/at/all".into(), ..session() }),
            ..ChatRequest::default()
        };
        let err = stream(&InferenceConfig::default(), &request, &mut |_| {}).await.unwrap_err();
        assert!(matches!(err, LlmError::ProgramFailed { .. }), "{err}");
        assert!(err.to_string().contains("/nowhere/at/all"), "{err}");
        assert!(err.to_string().contains("Edit the agent"), "{err}");
    }

    #[test]
    fn a_tool_use_and_its_result_become_one_part() {
        let (state, seen) = fold_all(&[TOOL_USE, TOOL_RESULT, RESULT]);

        assert_eq!(state.work.len(), 1);
        let Part::ToolCall { name, arguments, outcome, .. } = &state.work[0] else {
            panic!("{:?}", state.work[0]);
        };
        assert_eq!(name, "Bash");
        assert_eq!(arguments["command"], "echo hi");
        assert_eq!(outcome, &ToolOutcome::Ok { summary: "hi".into() });
        assert_eq!(seen, ["started:toolu_x:Bash", "finished:toolu_x"]);

        let usage = state.usage.unwrap();
        assert_eq!(usage.prompt_tokens, 4 + 23145 + 42631);
        assert_eq!(usage.completion_tokens, 128);
        assert_eq!(usage.cost, None);
        assert_eq!(state.session.as_deref(), Some("s-1"));
    }

    #[test]
    fn a_failed_tool_reads_as_failed_and_is_cut_short() {
        let long = "x".repeat(500);
        let result = serde_json::json!({
            "type": "user",
            "message": { "role": "user", "content": [{
                "tool_use_id": "toolu_x",
                "type": "tool_result",
                "content": [{ "type": "text", "text": long }],
                "is_error": true,
            }]},
        })
        .to_string();
        let (state, _) = fold_all(&[TOOL_USE, &result]);

        let Part::ToolCall { outcome: ToolOutcome::Failed { error }, .. } = &state.work[0] else {
            panic!("{:?}", state.work[0]);
        };
        assert_eq!(error.chars().count(), SUMMARY_CHARS);
    }

    #[test]
    fn the_bridge_send_draws_as_send_message() {
        let tool_use = TOOL_USE.replace("\"Bash\"", &format!("\"{MCP_SEND}\""));
        let (state, seen) = fold_all(&[&tool_use, TOOL_RESULT]);

        let Part::ToolCall { name, .. } = &state.work[0] else { panic!("{:?}", state.work[0]) };
        assert_eq!(name, "send_message");
        assert_eq!(seen[0], "started:toolu_x:send_message");
    }

    #[test]
    fn separate_text_blocks_do_not_run_together() {
        let text = |body: &str| {
            serde_json::json!({
                "type": "assistant",
                "message": { "role": "assistant", "content": [{ "type": "text", "text": body }] },
            })
            .to_string()
        };
        let thinking = r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"thinking","thinking":"hmm"}]}}"#;
        let (state, seen) = fold_all(&[&text("first"), thinking, &text("second")]);

        assert_eq!(state.content, "first\n\nsecond");
        assert_eq!(seen, ["text:first", "reasoning:hmm", "text:\n\n", "text:second"]);
    }

    #[test]
    fn a_local_command_answers_in_its_result_text() {
        // `/compact` writes no assistant message; the result is all there is.
        let (state, seen) = fold_all(&[
            r#"{"type":"result","subtype":"success","is_error":false,"session_id":"s-1","result":"Compacted.","local_command":"compact"}"#,
        ]);
        assert_eq!(state.content, "Compacted.");
        assert_eq!(seen, ["text:Compacted."]);
    }

    const COMPACTING: &str =
        r#"{"type":"system","subtype":"status","status":"compacting","session_id":"s-1"}"#;
    const COMPACTED: &str = r#"{"type":"system","subtype":"status","status":null,"compact_result":"success","session_id":"s-1"}"#;
    const REINIT: &str = r#"{"type":"system","subtype":"init","session_id":"s-1"}"#;
    const BOUNDARY: &str = r#"{"type":"system","subtype":"compact_boundary","session_id":"s-1","compact_metadata":{"trigger":"manual","pre_tokens":35644,"post_tokens":9819}}"#;
    const SUMMARY: &str = r#"{"type":"user","message":{"role":"user","content":"This session is being continued from a previous conversation. Summary: ..."},"isCompactSummary":true}"#;
    const STDOUT: &str = r#"{"type":"user","message":{"role":"user","content":"<local-command-stdout>Compacted </local-command-stdout>"},"isReplay":true}"#;
    const COMMAND_DONE: &str = r#"{"type":"result","subtype":"success","is_error":false,"session_id":"s-1","result":"","local_command":"compact"}"#;

    #[test]
    fn a_failed_compaction_says_so() {
        let failed = COMPACTED.replace(r#""success""#, r#""failed""#);
        let (state, seen) = fold_all(&[COMPACTING, &failed, COMMAND_DONE]);
        assert_eq!(state.content, "Compaction failed: failed.");
        assert_eq!(seen, ["text:Compaction failed: failed."]);
    }

    #[test]
    fn a_compaction_answers_in_the_programs_own_words() {
        let (state, seen) =
            fold_all(&[COMPACTING, COMPACTED, REINIT, BOUNDARY, SUMMARY, STDOUT, COMMAND_DONE]);
        assert_eq!(state.content, "Compacted");
        assert_eq!(seen, ["text:Compacted"]);
    }

    #[test]
    fn a_compaction_with_no_stdout_line_is_still_answered() {
        let (state, _) = fold_all(&[COMPACTING, COMPACTED, REINIT, BOUNDARY, COMMAND_DONE]);
        assert_eq!(state.content, "Compacted the session: 35644 → 9819 tokens.");

        // Without the boundary there are no counts to quote.
        let (state, _) = fold_all(&[COMPACTING, COMPACTED, COMMAND_DONE]);
        assert_eq!(state.content, "Compacted the session.");
    }

    #[test]
    fn the_replayed_summary_is_not_a_reply() {
        let (state, seen) = fold_all(&[SUMMARY]);
        assert_eq!(state.content, "");
        assert!(seen.is_empty(), "{seen:?}");
    }

    #[test]
    fn batch_is_everything_after_the_last_assistant_turn() {
        let messages = vec![
            ChatMessage::system("you are Dana"),
            ChatMessage::user("one"),
            ChatMessage::assistant("did one"),
            ChatMessage::user("two"),
            ChatMessage::user("three"),
        ];
        assert_eq!(batch(&messages), &messages[3..]);

        let first = vec![ChatMessage::system("you are Dana"), ChatMessage::user("one")];
        assert_eq!(batch(&first), &first[..]);
    }

    #[test]
    fn an_operator_command_reaches_the_program_without_its_label() {
        // The program only recognizes a slash command at the start of the line.
        let lines = payload(&[ChatMessage::user("[OPERATOR]\n/compact")]);
        assert_eq!(lines.len(), 1);
        let line: serde_json::Value = serde_json::from_str(&lines[0]).unwrap();
        assert_eq!(line["message"]["content"][0]["text"], "/compact");
    }

    #[test]
    fn each_message_in_the_batch_is_its_own_line_and_the_system_prompt_is_not_one() {
        let lines = payload(&[
            ChatMessage::system("you are Dana"),
            ChatMessage::user("[OPERATOR]\nhello"),
            ChatMessage::user("[AGENT \"Pat\"]\nhi"),
        ]);
        assert_eq!(lines.len(), 2);
        let first: serde_json::Value = serde_json::from_str(&lines[0]).unwrap();
        assert_eq!(first["type"], "user");
        assert_eq!(first["message"]["role"], "user");
        assert_eq!(first["message"]["content"][0]["text"], "[OPERATOR]\nhello");
    }
}
