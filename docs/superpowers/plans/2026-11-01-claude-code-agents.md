# Claude Code Agents Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A fourth provider, Claude Code, whose agents are headless `claude` sessions with their own working directory and tools, orchestrated by Guaca's existing rooms, channels and peer messaging.

**Architecture:** One new provider file (`llm/claude_code.rs`) spawns one `claude -p` process per turn and folds its stream-json into the `Completion` the runtime already consumes; the runtime offers it no tools and persists its session id. A one-tool MCP endpoint on the daemon gives the process `send_message`. A command opens Ghostty on the session as the escape hatch.

**Tech Stack:** Rust (tokio, axum 0.7, rusqlite, serde), React 19 + TypeScript (vitest), Claude Code CLI ≥ 2.1.316, Ghostty.

**Spec:** `docs/superpowers/specs/2026-11-01-claude-code-agents-design.md`

## Global Constraints

- Keep it simple; disturb Guaca as little as possible. No terminal emulator, PTY or console in the app.
- Provider wire/SQLite spelling: `claudeCode` (serde `rename_all = "camelCase"` on `Provider`; `as_str` and `parse` must agree with it).
- CLI argv per turn: `claude -p --input-format stream-json --output-format stream-json --verbose --dangerously-skip-permissions --model <m>` plus `--session-id <uuid>` (new) or `--resume <id>` (stored), plus `--append-system-prompt <text>` on the first turn only, plus `--mcp-config <json>` when a bridge is configured. Never `--tools ""`, `--strict-mcp-config`, `--setting-sources ""`, `--disable-slash-commands`, `--no-session-persistence`.
- `cost` is `None` for this provider; tokens are counted.
- The subprocess's tool uses never enter `Completion.tool_calls`.
- A `/clear` batch spawns nothing and clears the stored session; any other `/command` is forwarded unchanged.
- Native daemon: `GUACA_ROOT=~/.local/share/guaca-native`, `GUACA_BIND=127.0.0.1:8788`, `GUACA_WEB=dist`.
- AGENTS.md conventions: failure-path tests first; every error says what happened and what to do; comments explain why; no dead code; American spelling. Commits carry **no** model co-author trailer and no "Generated with" footer. Commit as `git -c user.name=Joshua -c user.email=joshua@nennya.com commit ...`.
- Verify Rust changes with both `cargo test --manifest-path src-tauri/Cargo.toml` and `cargo clippy --manifest-path src-tauri/Cargo.toml --no-default-features --features server --all-targets -- -D warnings`. The server feature build has no `dist/`; run `pnpm build` once first (CI's `ensure_dist`).
- Only touch the three existing providers where the compiler forces a new match arm.

## Review Focus

1. A turn whose process dies mid-stream (no `result` line): the operator sees the program's stderr and exit code, not a decode error. Test in Task 4.
2. A stored session id whose file is gone (operator deleted `~/.claude/projects/...`): `claude --resume` fails; the turn fails with the program's words, and the next `/clear` recovers. Test in Task 4 (stand-in exits 1 on `--resume missing`).
3. Two turns for the same agent can never run concurrently (the runtime already serializes an agent's turns); an `in_terminal` agent must not consume its inbox, and must drain it when the window closes. Test in Task 7.
4. A `tools/call` on the MCP bridge for a run that is not live (finished, stopped, or never existed) is refused with a JSON-RPC error, not a panic or a message sent as nobody. Test in Task 6.
5. A `cwd` that is relative, blank, or not a directory is refused on save with a sentence naming the field. Test in Task 2.

---

### Task 1: The provider exists

**Files:**
- Modify: `src-tauri/src/config.rs:38-84` (enum, `ALL`, `as_str`, `parse`), `:171-215` (`is_ready`, `endpoint`, `active_model`)
- Modify: `src-tauri/src/domain/agent.rs:256-263` (`own_model`), `src/lib/providers.ts:156` (`ownModel`)
- Modify: `src/lib/types.ts:828` (`Provider` union)
- Modify: `src/components/SettingsDialog.tsx:802-845` (a fourth row), `src/components/GroupEditor.tsx:325-336` (labels), `src/components/AgentEditor.tsx:75-82` (provider name)
- Test: `src-tauri/src/config.rs` tests module; `src/lib/providers.test.ts` if present, else `src/components/SettingsDialog.test.tsx`

**Interfaces:**
- Produces: `Provider::ClaudeCode`, `as_str() == "claudeCode"`, `crate::llm::claude_code::{PROGRAM, DEFAULT_MODEL}` constants (`"claude"`, `"sonnet"`). `AgentCard::own_model(Provider::ClaudeCode)` returns `None` in this task, exactly as `Claude` does; Task 2 adds the field and changes it.

- [ ] **Step 1: Write the failing round-trip test** in `config.rs` tests:

```rust
#[test]
fn claude_code_is_spelled_the_same_in_sqlite_and_on_the_wire() {
    assert_eq!(Provider::ClaudeCode.as_str(), "claudeCode");
    assert_eq!(Provider::parse("claudeCode"), Some(Provider::ClaudeCode));
    assert_eq!(serde_json::to_value(Provider::ClaudeCode).unwrap(), serde_json::json!("claudeCode"));
    let cfg = InferenceConfig { provider: Provider::ClaudeCode, ..InferenceConfig::default() };
    assert!(cfg.is_ready());
    assert_eq!(cfg.endpoint(), "claude");
    assert_eq!(cfg.active_model(), "sonnet");
}
```

- [ ] **Step 2: Run it** — `cargo test --manifest-path src-tauri/Cargo.toml claude_code_is_spelled` — expect compile failure on the missing variant.
- [ ] **Step 3: Implement.** Add the variant with a doc comment saying this one is a session, not a model (contrast with `Claude` above it). `ALL: [Provider; 4]`. Create `src-tauri/src/llm/claude_code.rs` with only the two constants and a module doc stating the reversal of `claude.rs`'s isolation flags and why; register it in `llm/mod.rs`. `is_ready => true`, `endpoint => claude_code::PROGRAM`, `active_model => claude_code::DEFAULT_MODEL`. `own_model`: `Provider::ClaudeCode => return None` for now.
- [ ] **Step 4: TS.** `Provider = "compatible" | "chatgpt" | "claude" | "claudeCode"`. `ownModel`: `if (provider === "claudeCode") return "";` for now. Labels: AgentEditor `providerName` "Claude Code"; GroupEditor `inherited` "Claude Code · the agent's own session"; SettingsDialog: duplicate the Claude row as a `preset--plain` titled "Claude Code" with text "Runs full Claude Code sessions with their own tools and working directories. Each agent is one session." and a "Use it" button calling `setProvider("claudeCode")`, gated by the same `claudeInstalled` check. `GroupEditor` offers the same fourth choice wherever it offers the other three (`:96, :160, :328-336, :464`), with the label "Claude Code".
- [ ] **Step 5: Run** `cargo test --manifest-path src-tauri/Cargo.toml` and `pnpm typecheck && pnpm test` — expect PASS. Fix every exhaustive `match` the compiler names.
- [ ] **Step 6: Commit** — `feat(provider): name the Claude Code provider`.

---

### Task 2: An agent has a working directory, a model and a session

**Files:**
- Modify: `src-tauri/src/db/migrations.rs` (append 66), `src-tauri/src/db/store.rs:3859` (`AGENT_COLUMNS`), `row_to_card`, `insert_agent` (`:211`), `update_agent` (`:369`), `new_card`
- Modify: `src-tauri/src/domain/agent.rs:133-238` (`AgentCard`), `:332-427` (`AgentDraft`, `DraftError`, `validate`, `CleanDraft`), `own_model`
- Modify: `src-tauri/src/transfer.rs:23` (agents allowlist gains `cwd,claude_model`)
- Modify: `src/lib/types.ts:719-797`, `src/lib/providers.ts:156`, `src/components/AgentEditor.tsx:30-40, 234-290`
- Test: `src-tauri/src/domain/agent.rs` tests, `src-tauri/src/db/store.rs` tests, `src/components/AgentEditor.test.tsx`

**Interfaces:**
- Produces: `AgentCard { cwd: String, claude_model: String, claude_session: Option<String>, .. }` (all `#[serde(default)]`); `AgentDraft { cwd: Option<String>, claude_model: Option<String>, .. }`; `CleanDraft` same two; `DraftError::BadWorkingDirectory { got: String, why: &'static str }`; `Store::set_claude_session(&self, id: AgentId, session: Option<&str>) -> Result<(), StoreError>`; TS `AgentCard.cwd`, `.claudeModel`, `.claudeSession: string | null`, `AgentDraft.cwd?`, `.claudeModel?`.

- [ ] **Step 1: Failing validation tests** in `agent.rs`:

```rust
#[test]
fn a_relative_working_directory_is_refused() {
    let draft = AgentDraft { cwd: Some("projects/x".into()), ..valid_draft() };
    assert!(matches!(draft.validate(), Err(DraftError::BadWorkingDirectory { .. })));
}
#[test]
fn a_working_directory_that_is_not_a_directory_is_refused() {
    let file = tempfile::NamedTempFile::new().unwrap();
    let draft = AgentDraft { cwd: Some(file.path().display().to_string()), ..valid_draft() };
    assert!(matches!(draft.validate(), Err(DraftError::BadWorkingDirectory { .. })));
}
#[test]
fn an_absent_or_blank_working_directory_is_kept_as_is() {
    assert_eq!(AgentDraft { cwd: None, ..valid_draft() }.validate().unwrap().cwd, None);
    assert_eq!(AgentDraft { cwd: Some("  ".into()), ..valid_draft() }.validate().unwrap().cwd, Some(String::new()));
}
```
`DraftError::BadWorkingDirectory` message: `"working directory {got:?} {why}. Give the agent an absolute path to a folder that exists."` with `why` one of `"is not an absolute path"` / `"is not a directory"`.

- [ ] **Step 2: Run** — expect compile failure.
- [ ] **Step 3: Implement** the fields, the error, `validate` (blank stays blank: an agent on another provider never names one), `own_model(Provider::ClaudeCode) => &self.claude_model`.
- [ ] **Step 4: Failing store test** in `store.rs` tests: create an agent with `cwd: Some("/tmp")`, `claude_model: Some("opus")`; `set_claude_session(id, Some("abc"))`; `get_agent` returns all three; `update_agent` with `cwd: None` keeps `/tmp`; `set_claude_session(id, None)` clears it.
- [ ] **Step 5: Migration 66**:

```sql
-- A Claude Code agent is a session rather than a model: where it runs, which
-- model its program is told to use, and the session the program keeps on disk
-- that every turn resumes. Blank on every other provider, which ignores them.
ALTER TABLE agents ADD COLUMN cwd TEXT NOT NULL DEFAULT '';
ALTER TABLE agents ADD COLUMN claude_model TEXT NOT NULL DEFAULT '';
ALTER TABLE agents ADD COLUMN claude_session TEXT;
```
Append `,cwd,claude_model,claude_session` to `AGENT_COLUMNS` (indexes 25, 26, 27); read them in `row_to_card`; insert them; `update_agent` uses `cwd = coalesce(?12, cwd), claude_model = coalesce(?13, claude_model)`; `new_card` copies them (`unwrap_or_default`). `set_claude_session` is a single `UPDATE agents SET claude_session = ?2 WHERE id = ?1` that does **not** bump `version` (no peer cares).
- [ ] **Step 6: Run** `cargo test --manifest-path src-tauri/Cargo.toml` — PASS.
- [ ] **Step 7: TS + editor.** Types as above. `ownModel`: `claudeCode` reads `agent.claudeModel`. In `AgentEditor`, for `provider === "claudeCode"` render instead of the model branch: a `Working directory` text input (`input--mono`, placeholder `/home/you/projects/thing`, hint "Where this agent's Claude Code session runs. Absolute path to an existing folder."), a `Model` input (`input--mono`, placeholder `sonnet`, hint "A Claude model alias or full name, passed to claude --model."), and when `agent?.claudeSession` a read-only `Session` field showing the id in `input--mono` with a "Copy" `btn btn--small` using `navigator.clipboard.writeText`. Hide `ModelSuggestions` for this provider. Draft state initializes `cwd: agent?.cwd ?? ""`, `claudeModel: agent?.claudeModel ?? ""`.
- [ ] **Step 8: Editor test** `AgentEditor.test.tsx` (follow an existing component test's render setup): with a crew whose `inference.provider === "claudeCode"`, the labels "Working directory" and "Model" render and "Skills" still renders; with `provider === "compatible"` "Working directory" does not.
- [ ] **Step 9: Run** `pnpm check && pnpm typecheck && pnpm test` — PASS.
- [ ] **Step 10: Commit** — `feat(agents): a working directory, a model and a session for Claude Code`.

---

### Task 3: The stream can carry a tool the provider ran itself

**Files:**
- Modify: `src-tauri/src/llm/openrouter.rs:151-156` (`ChatRequest`), `:201-212` (`Token`), `:232-240` (`Completion`)
- Modify: `src-tauri/src/runtime/mod.rs:7906-7923` (`Pen::write`), and every `ChatRequest { .. }` literal (`runtime/mod.rs:1418, 3203`, `runtime/errand.rs:226`, tests in `llm/codex.rs`, `llm/openrouter.rs`, `tests/claude.rs`, `tests/evals.rs`) and every `Completion { .. }` literal (`llm/codex.rs:730`, `llm/openrouter.rs:768`) and every exhaustive `match` on `Token` (`llm/codex.rs:1078`, `llm/claude.rs:864`)
- Test: `src-tauri/src/runtime/mod.rs` tests (Pen)

**Interfaces:**
- Produces:
```rust
pub struct ChatRequest { ..existing, /// Set only by a Claude Code turn. pub claude_code: Option<crate::llm::claude_code::Session> }  // derive Default on ChatRequest
pub enum Token<'a> { Text(&'a str), Reasoning(&'a str),
    /// A tool the provider's own program is running, for the chips. Never a call the runtime executes.
    ToolStarted { call_id: &'a str, name: &'a str, arguments: &'a serde_json::Value },
    ToolFinished { call_id: &'a str, part: &'a crate::domain::envelope::Part } }
pub struct Completion { ..existing, /// Parts for tools the provider ran itself. pub work: Vec<Part>, /// The provider's session to keep. `Some("")` means "forget it". pub session: Option<String> }
```
- `claude_code::Session` is defined in Task 4; in this task define it as the full struct (fields below) so the field compiles:
```rust
#[derive(Debug, Clone, PartialEq)]
pub struct Session { pub agent_id: AgentId, pub cwd: PathBuf, pub resume: Option<String>, /// First turn only. pub system_prompt: Option<String>, pub bridge: Option<Bridge> }
#[derive(Debug, Clone, PartialEq)]
pub struct Bridge { pub url: String, pub token: String }
```

- [ ] **Step 1: Failing Pen test** in `runtime/mod.rs` tests next to the existing Pen tests: a `RecordingSink`; `pen.write(Token::Text("a"))`; `pen.write(Token::ToolStarted { call_id: "1", name: "Bash", arguments: &json!({"command":"ls"}) })`; `pen.write(Token::ToolFinished { call_id: "1", part: &Part::tool_call("Bash", json!({}), ToolOutcome::Ok { summary: "ok".into() }) })`; `pen.flush()`. Assert the sink saw, in order: `StreamDelta{text:"a"}`, `ToolStarted{name:"Bash"}`, `ToolFinished{call_id:"1"}`. (Text held before a tool must be flushed first so the transcript reads in order.)
- [ ] **Step 2: Run** — compile failure.
- [ ] **Step 3: Implement.** `Token` arms in `Pen::write`: flush, then `emit(UiEvent::ToolStarted { message_id, call_id, name, arguments })` / `emit(UiEvent::ToolFinished { message_id, call_id, part: part.clone() })`. Add `claude_code: None`, `work: Vec::new()`, `session: None` where the compiler demands; `_ => {}` in the two test matches.
- [ ] **Step 4: Run** full `cargo test` — PASS, no behavior change elsewhere.
- [ ] **Step 5: Commit** — `refactor(llm): let a provider report work it did itself`.

---

### Task 4: The Claude Code provider

**Files:**
- Modify: `src-tauri/src/llm/claude_code.rs` (the whole provider), `src-tauri/src/llm/openrouter.rs:622-624` (dispatch), `src-tauri/src/llm/claude.rs` (make `conversation`'s block builder `pub(crate)` if reusing it)
- Test: `src-tauri/src/llm/claude_code.rs` unit tests; `src-tauri/tests/claude_code.rs` (stand-in program)

**Interfaces:**
- Consumes: Task 3 types.
- Produces:
```rust
pub const PROGRAM: &str = "claude"; pub const DEFAULT_MODEL: &str = "sonnet";
pub const CLEAR: &str = "/clear"; pub const CLEARED: &str = "Session cleared. The next message starts a fresh Claude Code session.";
pub const MCP_SERVER: &str = "guaca"; pub const MCP_SEND: &str = "mcp__guaca__send_message";
pub fn argv(session: &Session, model: &str) -> Vec<String>;
/// The messages after the last assistant turn, which is what the session has not seen.
pub fn batch(messages: &[ChatMessage]) -> &[ChatMessage];
/// `Some(())` when the batch is exactly one operator line reading `/clear`.
pub fn is_clear(batch: &[ChatMessage]) -> bool;
#[derive(Default)] pub struct Fold { pub content: String, pub work: Vec<Part>, pub usage: Option<Usage>, pub session: Option<String>, pub failed: Option<String>, open: BTreeMap<String, (String, serde_json::Value)>, answered: bool }
/// One stdout line. Returns Ok(true) when it was the result frame.
pub fn fold<F: FnMut(Token<'_>)>(line: &str, state: &mut Fold, on_token: &mut F) -> Result<bool, LlmError>;
pub async fn stream<F: FnMut(Token<'_>)>(cfg: &InferenceConfig, request: &ChatRequest, on_token: &mut F) -> Result<Completion, LlmError>;
```
- Behavior pinned:
  - `argv`: as in Global Constraints. `--resume <id>` when `session.resume` is `Some`, else `--session-id <uuid v4>` (the uuid also becomes `Fold.session` if the result frame lacks one). `--append-system-prompt` only when `session.system_prompt` is `Some`. `--mcp-config` is the JSON string `{"mcpServers":{"guaca":{"type":"http","url":"<bridge.url>","headers":{"Authorization":"Bearer <bridge.token>"}}}}` only when `session.bridge` is `Some`.
  - `stream`: if `request.claude_code` is `None` → `LlmError::ProgramFailed { program: PROGRAM, message: "this turn was not given a Claude Code session; the runtime should have attached one. This is a bug in Guaca." }`. If `is_clear(batch)` → return `Completion { content: CLEARED.into(), session: Some(String::new()), ..Default::default() }` without spawning. If `session.cwd` is not a directory → `ProgramFailed` with `"working directory {cwd} does not exist. Edit the agent and point it at a folder that exists."`. Spawn with `current_dir(cwd)`, `kill_on_drop(true)`, stdin piped from a task, one `{"type":"user","message":{"role":"user","content":<blocks>}}` line per batch message (reuse claude.rs's block conversion). Read lines with an **idle** timeout of `IDLE: Duration = 60 min` between lines (not a whole-turn timeout: a coding turn legitimately runs long); on idle, kill and return `ProgramFailed { message: "claude wrote nothing for 60 minutes and was stopped" }` (not `Timeout`, which would be retried). No result frame → the same stderr/exit-code report `claude.rs:689-717` makes. `Fold.failed` (result `is_error`) → `ProgramFailed { message: <result text> }`.
  - `fold`: `assistant` message content blocks: `text` → `Token::Text` and append to `content` (append `THOUGHT_BREAK`-style `"\n\n"` between separate text blocks); `thinking` → `Token::Reasoning`; `tool_use {id,name,input}` → remember in `open`, map `MCP_SEND` → `send_message` for the chip name, `Token::ToolStarted`. `user` message with `tool_result {tool_use_id, content, is_error}` → take from `open`, build `Part::tool_call(name, input, if is_error { ToolOutcome::Failed { error } } else { ToolOutcome::Ok { summary } })` where summary/error is the result text truncated to 200 chars, push to `work`, `Token::ToolFinished`. `result` → `usage = Some(Usage { prompt_tokens: input_tokens + cache_creation_input_tokens + cache_read_input_tokens, completion_tokens: output_tokens, cost: None })`, `session = Some(session_id)`, `failed = is_error.then(|| result)`, return `Ok(true)`. `system`, `rate_limit_event`, `stream_event` and unknown types → `Ok(false)`. Not JSON → `Ok(false)` with a `tracing::debug!` (the program prints hook noise).
  - Dispatch: `if cfg.provider == Provider::ClaudeCode { return claude_code::stream(cfg, request, &mut on_token).await; }` in `stream_chat`.

- [ ] **Step 1: Failing unit tests** (failure paths first), in `claude_code.rs`:
  - `a_turn_without_a_session_is_a_bug_not_a_spawn` (stream with `claude_code: None` → `ProgramFailed`, message contains "bug").
  - `an_error_result_is_the_programs_own_words` (fold a `{"type":"result","is_error":true,"result":"Not logged in"}` line → `failed == Some("Not logged in")`).
  - `argv_resumes_a_stored_session_and_never_isolates` (contains `--resume abc`, `--dangerously-skip-permissions`, `--model opus`; contains none of `--tools`, `--strict-mcp-config`, `--no-session-persistence`, `--setting-sources`, `--disable-slash-commands`; no `--append-system-prompt`).
  - `argv_starts_a_new_session_with_the_system_prompt_once` (`--session-id` present and parses as a uuid; `--append-system-prompt` present).
  - `argv_carries_the_bridge_when_given_one` (the `--mcp-config` value parses and `["mcpServers"]["guaca"]["url"]` equals the bridge url).
  - `a_clear_batch_is_answered_without_spawning` (`is_clear` true for one user message `"/clear"`, false for `"/compact"`, false for two messages).
  - `a_tool_use_and_its_result_become_one_part` (fold the three lines from the probe in the spec: tool_use `Bash`, tool_result `hi`, result → `work.len() == 1`, name `Bash`, `ToolOutcome::Ok`; the on_token saw `ToolStarted` then `ToolFinished`; usage prompt_tokens sums the three input counts; `cost == None`).
  - `the_bridge_send_draws_as_send_message` (tool_use named `mcp__guaca__send_message` → part name `send_message`).
  - `batch_is_everything_after_the_last_assistant_turn`.
- [ ] **Step 2: Run** `cargo test --manifest-path src-tauri/Cargo.toml claude_code::` — compile failure.
- [ ] **Step 3: Implement** `claude_code.rs` and the dispatch line.
- [ ] **Step 4: Run** the unit tests — PASS.
- [ ] **Step 5: Stand-in integration test** `src-tauri/tests/claude_code.rs`, modeled on `tests/claude.rs:45-90`. The stand-in `claude` script records `"$@"` to `<dir>/argv` and stdin to `<dir>/stdin`, then: if argv contains `--resume missing` → `echo 'No conversation found with session ID: missing' >&2; exit 1`; if stdin contains `die-midway` → print one assistant text line and `exit 0` with no result; otherwise print the tool_use / tool_result / assistant text `"done"` / result lines (session_id `fixed-session`, `is_error:false`, usage `{input_tokens:1,cache_creation_input_tokens:2,cache_read_input_tokens:3,output_tokens:4}`). Tests:
  - `a_turn_set_to_claude_code_is_answered_by_the_program` (through `LlmClient::stream_chat` with `provider: ClaudeCode` and a `claude_code: Some(Session{..cwd: tempdir, resume: None, system_prompt: Some("you are Dana")..})`: `content == "done"`, `work.len()==1`, `session == Some("fixed-session")`, `usage.prompt_tokens == 6`, `cost == None`; argv file contains `--session-id` and `--append-system-prompt`; stdin file contains the user text; the stand-in's recorded `pwd -P` equals the tempdir).
  - `a_second_turn_resumes_and_sends_no_system_prompt` (`resume: Some("fixed-session")`, `system_prompt: None` → argv has `--resume fixed-session`, lacks `--append-system-prompt`).
  - `a_missing_session_is_the_programs_own_refusal` (`resume: Some("missing")` → `Err(ProgramFailed)` whose message contains "No conversation found").
  - `a_program_that_dies_midway_reports_its_exit` (user text `die-midway` → `Err(ProgramFailed)` containing "ended without answering").
  - `#[ignore = "live: spends the operator's own Claude plan"] a_live_turn_runs_a_command` (real `claude`, `--model haiku`, cwd = tempdir, asks it to run `echo hi`; asserts `work` has a `Bash` part and `session.is_some()`).
- [ ] **Step 6: Run** `cargo test --manifest-path src-tauri/Cargo.toml --test claude_code` — PASS (ignored test skipped).
- [ ] **Step 7: Commit** — `feat(llm): the Claude Code provider`.

---

### Task 5: A turn on this provider is one session call

**Files:**
- Modify: `src-tauri/src/runtime/mod.rs:3049-3052` (model), `:3159-3162` (`offered`), `:3203-3208` (request), after `:3239` (`count_tokens`) (session persist + work merge)
- Modify: `src-tauri/src/runtime/mod.rs` `Runtime` inner state: `mcp_bridge: Mutex<Option<claude_code::Bridge>>` with `pub fn set_mcp_bridge(&self, bridge: claude_code::Bridge)`
- Test: `src-tauri/tests/claude_code.rs` (runtime half, using the harness `tests/subscription.rs:511-516` builds a `Runtime` with)

**Interfaces:**
- Consumes: `claude_code::{Session, Bridge, batch, CLEARED}`; `Store::set_claude_session`.
- Produces: `Runtime::set_mcp_bridge(&self, Bridge)`; a turn on `Provider::ClaudeCode` offers `tools: vec![]`, fills `request.claude_code = Some(Session { agent_id, cwd: card.cwd.into(), resume: card.claude_session.clone(), system_prompt: card.claude_session.is_none().then(|| <messages[0] system content> + "\n\nTo reach a crewmate, call the send_message tool your guaca MCP server provides."), bridge: self.mcp_bridge().map(|b| Bridge { url: format!("{}/v1/claude-code/mcp/{agent_id}/{run_id}", b.url), token: b.token }) })`; after a completion: `tool_parts.extend(completion.work)`; `match completion.session { Some(s) if s.is_empty() => store.set_claude_session(id, None), Some(s) => store.set_claude_session(id, Some(&s)), None => {} }` followed by `emit(UiEvent::AgentsChanged)` only when the stored value changed.
- The model: `card.own_model(ClaudeCode)` else the group's/app's `active_model()` (which is `DEFAULT_MODEL`).

- [ ] **Step 1: Failing runtime tests** in `tests/claude_code.rs` (reuse the stand-in; build a `Runtime` the way `tests/subscription.rs` does, with a group whose `InferenceConfig.provider == ClaudeCode` and an agent with `cwd` = tempdir):
  - `a_first_turn_stores_the_session_and_a_second_resumes_it` (send an operator message; wait for the turn; `get_agent().claude_session == Some("fixed-session")`; argv file has `--session-id`; send another; argv file now has `--resume fixed-session`).
  - `a_clear_forgets_the_session_without_running_the_program` (store a session, send `/clear`; `claude_session == None`; the stand-in's argv file was not rewritten; the channel's last message text is `CLEARED`).
  - `a_claude_code_turn_is_offered_no_tools` (the stand-in never sees a `--tools` flag, and the recorded request log — if the harness exposes `tools_by_agent` as `tests/harness/mod.rs:890` does — is empty for this agent; otherwise assert on the `turn tool definitions` tracing line via a `tracing_subscriber` test layer, whichever is smaller).
- [ ] **Step 2: Run** — FAIL (session not stored).
- [ ] **Step 3: Implement** the four edits in `run_turn` and `set_mcp_bridge`. Keep the round loop untouched: the provider returns no `tool_calls`, so the loop ends after one round by its own rule.
- [ ] **Step 4: Run** `cargo test --manifest-path src-tauri/Cargo.toml --test claude_code` — PASS; then full `cargo test` — PASS.
- [ ] **Step 5: Commit** — `feat(runtime): a Claude Code turn resumes its own session`.

---

### Task 6: The bridge: one MCP tool, `send_message`

**Files:**
- Create: `src-tauri/src/server/bridge.rs`
- Modify: `src-tauri/src/server/mod.rs:218-260` (route `.route("/v1/claude-code/mcp/:agent_id/:run_id", post(bridge::call))`), `:184-190` (after boot: `runtime.set_mcp_bridge(Bridge { url: format!("http://{bind}"), token })`)
- Modify: `src-tauri/src/runtime/mod.rs`: `Relay` registry + `pub fn relay_send_message(&self, agent_id: AgentId, run_id: RunId, arguments: serde_json::Value) -> Result<String, RelayError>`
- Test: `src-tauri/tests/server.rs` (server feature) or a new `tests/bridge.rs` under `#[cfg(feature = "server")]`

**Interfaces:**
- Produces, in `runtime/mod.rs`:
```rust
struct Relay { agent_id: AgentId, inbound_hop: u16, cause: Option<MessageId>, settled: bool, addressed: HashSet<AgentId> }
// inner: relays: Mutex<HashMap<RunId, Relay>>
fn open_relay(&self, run_id: RunId, relay: Relay);           // called in run_turn right before stream_with_retries on a ClaudeCode turn
fn close_relay(&self, run_id: RunId) -> Option<HashSet<AgentId>>; // right after; the turn extends its `addressed` with it
#[derive(Debug, thiserror::Error)] pub enum RelayError {
  #[error("no Claude Code turn is running under run {run_id}. The message was not sent; it may have arrived after the turn ended.")] NoSuchRun { run_id: RunId },
  #[error("run {run_id} belongs to another agent")] WrongAgent { run_id: RunId },
  #[error("send_message arguments were not understood: {0}")] Arguments(String),
}
pub fn relay_send_message(&self, agent_id, run_id, arguments) -> Result<String, RelayError>  // returns `tools::render_deliveries(&deliveries)`
```
- `relay_send_message` parses `arguments` with the same parser `dispatch_tool` uses for `tools::SEND_MESSAGE`, loads the card, and calls `send_to_peers(&card, run_id, relay.inbound_hop, relay.cause, relay.settled, &mut relay.addressed, &to, &text, intent, &[])`.
- `server/bridge.rs`: `pub async fn call(State(serving), Path((agent_id, run_id)), headers, body: Bytes) -> Response`. Bearer check via the existing `authorized`. Body is one JSON-RPC 2.0 request (or a notification). Methods:
  - `initialize` → `{"protocolVersion": <the client's, else "2025-03-26">, "capabilities": {"tools": {}}, "serverInfo": {"name": "guaca", "version": env!("CARGO_PKG_VERSION")}}`
  - `notifications/initialized` (no id) → `202 Accepted`, empty body
  - `ping` → `{}`
  - `tools/list` → `{"tools":[{"name":"send_message","description":<the Guaca spec's description>,"inputSchema":<the Guaca spec's parameters>}]}` taken from `tools::all_specs` by name (make the lookup `pub(crate)` if needed)
  - `tools/call` with `name == "send_message"` → `{"content":[{"type":"text","text": <rendered>}], "isError": false}`; a `RelayError` → the same shape with `isError: true` and the error's Display as text (the model should read it, not the transport)
  - any other method → JSON-RPC error `-32601` `"method not found"`; malformed body → `-32700`.
  - Responses are `application/json`; no SSE.

- [ ] **Step 1: Failing bridge tests** (server feature; start the server the way `tests/server.rs` does):
  - `a_call_for_a_run_that_is_not_live_is_refused_in_the_result` (`tools/call` with a random run id → `isError: true`, text contains "no Claude Code turn is running").
  - `a_call_without_the_token_is_401`.
  - `tools_list_offers_exactly_send_message`.
  - `initialize_names_the_server` (`serverInfo.name == "guaca"`).
  - `a_live_relay_queues_a_message_for_the_peer` (open a relay by hand through a `#[doc(hidden)] pub fn open_relay_for_tests` if the runtime cannot be driven to a live ClaudeCode turn in-process; call `tools/call` with `{"to":["Pat"],"text":"hi"}`; assert Pat's inbox has one envelope). If the Task 5 harness can hold a turn open (stand-in sleeps until a file appears), prefer driving the real turn.
- [ ] **Step 2: Run** `cargo test --manifest-path src-tauri/Cargo.toml --no-default-features --features server --test bridge` — compile failure.
- [ ] **Step 3: Implement** relay registry, `relay_send_message`, `bridge.rs`, the route, `set_mcp_bridge` at boot, and `open_relay`/`close_relay` around the ClaudeCode stream call in `run_turn` (Task 5's site).
- [ ] **Step 4: Run** server-feature tests and clippy — PASS.
- [ ] **Step 5: Commit** — `feat(server): a one-tool MCP bridge so a Claude Code agent can message its crew`.

---

### Task 7: Resume in terminal

**Files:**
- Modify: `src-tauri/src/runtime/mod.rs` (`in_terminal: Mutex<HashSet<AgentId>>`; `pub fn enter_terminal(&self, AgentId) -> bool`, `pub fn leave_terminal(&self, AgentId)`, `pub fn in_terminal(&self, AgentId) -> bool`), `src-tauri/src/runtime/drain.rs` (skip an agent that is `in_terminal` when choosing work; `leave_terminal` calls the same wake a finished turn does)
- Modify: `src-tauri/src/runtime/events.rs` / `domain` `Activity` enum: new variant `InTerminal`; `src/lib/types.ts` `Activity` union `{ state: "inTerminal" }`; wherever the UI words an activity (grep `"working"` in `src/lib/doing.ts` / `presence.ts`) → "In your terminal"
- Modify: `src-tauri/src/commands.rs` (`pub async fn resume_in_terminal(state: &AppState, id: AgentId) -> Reply<()>`), `src-tauri/src/ipc.rs:366` (`resume_in_terminal(id: AgentId) -> ()`), `src/lib/ipc.ts` (`resumeInTerminal: (id) => invoke<void>("resume_in_terminal", { id })`)
- Modify: `src/components/AgentMenu.tsx:13-27, 164-178` (prop `onResumeInTerminal`, item "Resume in terminal" shown only when `provider === "claudeCode"`), and the component that renders `AgentMenu` (grep `onClearHistory=`) to pass a handler that calls `api.resumeInTerminal` and surfaces a refusal the way the other menu actions do
- Test: `src-tauri/src/commands.rs` tests or `tests/claude_code.rs`; `src/components/AgentMenu.test.tsx`

**Interfaces:**
- `resume_in_terminal` refusals, in order, each a `Refused`/command error with exactly this text:
  1. no session: `"This agent has no Claude Code session yet. Send it a message first, then resume it."`
  2. `ghostty` not found by `programs.rs`'s lookup: `"Ghostty is not installed, or is not on this app's PATH. Install Ghostty, or run `claude --resume <id>` in any terminal."` (with the id filled in)
  3. neither `DISPLAY` nor `WAYLAND_DISPLAY` set: `"This daemon cannot open a window: it is not running on your desktop. Run it with scripts/native.sh, or run `claude --resume <id>` in any terminal."`
  4. already in terminal: `"This agent is already open in a terminal."`
- Then: `enter_terminal(id)`; `emit(UiEvent::ActivityChanged { agent_id, activity: Activity::InTerminal })` (match the existing event's shape); spawn `ghostty --working-directory=<cwd> -e claude --resume <session>` with `kill_on_drop(false)`; `tokio::spawn` awaits `child.wait()`, then `leave_terminal(id)` and emits idle.

- [ ] **Step 1: Failing tests**: the four refusals (unit tests on a pure `fn refusal(card: &AgentCard, ghostty: Option<PathBuf>, display: bool, open: bool) -> Option<String>` extracted from the command so no window is needed); drain test in `runtime` tests: an agent marked `in_terminal` with a queued envelope is not run; after `leave_terminal` it runs.
- [ ] **Step 2: Run** — FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: UI**: menu item + prop + handler; `AgentMenu.test.tsx`: item present with `provider="claudeCode"`, absent with `"compatible"`. Run `pnpm check && pnpm typecheck && pnpm test` and `ipc.contract.test.ts` (it fails if the command is registered but uncalled, or called but unregistered).
- [ ] **Step 5: Run** all suites — PASS.
- [ ] **Step 6: Commit** — `feat: resume a Claude Code agent in a terminal`.

---

### Task 8: Chips read well, docs say why, the daemon runs here

**Files:**
- Modify: `src/lib/trail.ts:400-412` (`describe` cases for `Bash`, `Edit`, `Write`, `MultiEdit`, `Read`, `Glob`, `Grep`, `WebFetch`, `WebSearch`, `Task`), `src/lib/trail.test.ts`
- Create: `scripts/native.sh`
- Modify: `docs/ARCHITECTURE.md` (new section after "A subscription is a second provider, not a second endpoint": "A Claude Code agent is a session, not a model"), `docs/gotchas/providers.md` (one paragraph), `docs/HOSTING.md` (a "Natively on Linux, for Claude Code agents" subsection), `AGENTS.md` "Where things are" (one line each for `llm/claude_code.rs`, `server/bridge.rs`, `scripts/native.sh`)

- [ ] **Step 1: Failing trail tests**: `describe("Bash", {command:"ls"})` → title "Ran a command", target "ls" (first 60 chars); `Edit`/`Write`/`MultiEdit` with `file_path:"/a/b/c.rs"` → "Edited c.rs"; `Read` → "Read c.rs"; `Glob`/`Grep` → "Searched files"; `WebFetch`/`WebSearch` → "Read the web", target url/query; `Task` → "Sent a subagent", target `description`.
- [ ] **Step 2: Run** `pnpm test trail` — FAIL. **Step 3: Implement.** **Step 4: PASS.**
- [ ] **Step 5: `scripts/native.sh`**: `set -euo pipefail`; cd to repo root; `pnpm build`; `cargo build --manifest-path src-tauri/Cargo.toml --release --no-default-features --features server --bin guacad`; `export GUACA_ROOT="${GUACA_ROOT:-$HOME/.local/share/guaca-native}" GUACA_BIND="${GUACA_BIND:-127.0.0.1:8788}" GUACA_WEB="$PWD/dist" GUAC_LOG="${GUAC_LOG:-guac=info,warn}"`; `mkdir -p "$GUACA_ROOT"`; `exec src-tauri/target/release/guacad`. Comment at top: why native (the agents, `~/.claude`, the project folders and Ghostty are on this machine), and that the token is printed/kept under `$GUACA_ROOT/config/token` and the page is at `http://127.0.0.1:8788`.
- [ ] **Step 6: Docs** as listed; the ARCHITECTURE section states: the isolation flags are reversed on purpose, the subprocess owns the tool loop so Guaca's five limits count one model call per turn, `send_message` reaches it through MCP and nothing else does, and `/clear` is the only command Guaca interprets.
- [ ] **Step 7: Run** `./scripts/ci.sh` — PASS. Run `./scripts/native.sh` in the background, `curl -s http://127.0.0.1:8788/health` answers.
- [ ] **Step 8: Commit** — `docs: Claude Code agents, and a script to run the daemon natively`.

---

### Task 9: The success test

Manual, against the native daemon, and recorded in the plan's commit message.

- [ ] **Step 1:** Open `http://127.0.0.1:8788`, paste the token, set the app provider to Claude Code.
- [ ] **Step 2:** Create three agents in one group with three different scratch directories under `/tmp/guaca-crew/{a,b,c}` and model `haiku`.
- [ ] **Step 3:** Message each: "create a file named hello.txt containing your name, then tell <another agent's name> you did." Expect: three files appear, no approval prompt anywhere, each channel shows "Ran a command"/"Edited hello.txt" chips, and each peer's channel shows the relayed message.
- [ ] **Step 4:** Type `/compact` in one channel: the reply is the program's compaction message, no error. Type `/clear`: reply is `CLEARED`; the editor shows no session.
- [ ] **Step 5:** Right-click an agent → "Resume in terminal": Ghostty opens on the session in its directory; the agent shows "In your terminal"; send it a message, it waits; close Ghostty; the message runs.
- [ ] **Step 6:** Run the live test: `cargo test --manifest-path src-tauri/Cargo.toml --test claude_code -- --ignored`.
- [ ] **Step 7:** Commit any fixes; `git log --oneline main..` is the deliverable.
