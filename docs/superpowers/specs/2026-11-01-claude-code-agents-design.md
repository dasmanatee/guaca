# Claude Code agents

Guaca becomes the orchestrator for a crew of Claude Code sessions. Each agent of
the new kind *is* a Claude Code process with its own working directory and its
own tools; Guaca carries messages between them and the operator. Nothing else
about Guaca changes.

## Why

The operator today runs several Claude Code terminals and hand-carries context
between them. Guaca already has the room, the channels, agent-to-agent
messaging, memory and the watching UI. The missing piece is an agent whose turn
is "hand this to a Claude Code session and relay what it says". Claude Code
keeps doing what it is good at (editing, running commands, its own tool loop);
Guaca does only the orchestration.

Guiding rule from the operator: **keep it simple**, and **disturb Guaca as
little as possible**.

## Goals

- A fourth `Provider` variant, `ClaudeCode`, selectable where the other three
  are (app, group).
- Each such agent has a working directory. Which model runs is Claude Code's own setting (its config or `/model` in the terminal), never Guaca's.
- A turn runs one headless `claude` process with permissions skipped; its text
  streams into the channel; its tool calls draw as the existing work chips and
  are kept on the saved message.
- The session persists in the operator's real `~/.claude` and is resumed on
  every turn. `/clear` typed in the channel starts a fresh session; any other
  `/command` is forwarded to the process.
- A **Resume in terminal** action opens Ghostty on `claude --resume <id>` in the
  agent's directory. While the window is open the agent's inbox waits; when it
  closes, the queued turn runs.
- The success test: three such agents in one group, each editing files and
  running commands in different directories with no permission prompt, while
  the operator messages them and they message each other.

## Non-goals

- No terminal emulator, PTY, xterm.js or console panel in the app.
- No change to the three existing providers or to the runtime's tool loop,
  limits, approvals or prompt assembly beyond the dispatch point.
- No sandbox for these agents. They run as the operator, on the operator's
  machine, with permissions skipped. That is the point.
- No Docker image change. The Docker path keeps working as it does; this kind
  simply needs the daemon to run where `claude`, `~/.claude`, the project
  folders and Ghostty are.
- No support for E2B computer / Kernel browser on this kind. Claude Code has
  its own.

## Anti-goals

- Rebuilding the removed coding subsystem (#242): bridge, relay, console. If
  this grows a second message protocol, it has gone wrong. MCP is not a second
  protocol: Guaca already speaks it to plugins, and Claude Code speaks it
  natively.
- Teaching the runtime a second tool loop. The subprocess's tool uses are
  reported for display only and never enter `Completion.tool_calls`.

## Constraints

- The existing `Provider::Claude` strips the CLI to a bare model (no tools, no
  sessions, no settings) and `docs/gotchas/providers.md` calls every one of
  those flags load-bearing. This kind reverses all of them on purpose: it wants
  the operator's full Claude Code, settings, skills and MCP servers, because the
  agent *is* a Claude Code session. The gotcha file gets a paragraph saying so.
- Upstream removed the previous Claude Code integration as "a second product
  inside Guaca". This one is one provider file, a few agent fields, a menu item
  and a command. Hold that line.
- AGENTS.md conventions apply: failure-path tests first, every error says what
  to do, comments say why, no dead code, American spelling. Commits in this repo
  carry no model co-author trailer (AGENTS.md Ownership).
- The daemon runs natively on the operator's Linux machine for this kind:
  `cargo build --no-default-features --features server --bin guacad`, with
  `GUACA_ROOT=~/.local/share/guaca-native` and `GUACA_BIND=127.0.0.1:8788`
  (designer's call: a fresh store on its own port, so the Docker daemon on 8787
  keeps running untouched; a crew moves over with the existing group export and
  import). A `scripts/native.sh` builds and runs it, serving the built `dist/`.

## Decisions the operator made

- Guaca chat is the way to talk to the agents; no terminal in the app.
- Model selection stays inside Claude Code (its settings, or `/model` via the
  terminal escape hatch). Guaca never passes `--model`. (Revised 2026-11-01:
  the wrapper is around Claude Code, not around a model.)
- `/compact` and `/clear` are frequent and must work from the channel.
- The escape hatch is a button that opens Ghostty on the session.
- Native daemon on this machine.

## Shape (decided by the designer, not the operator)

### Provider and config

`Provider::ClaudeCode` (`config.rs`), wire spelling `claude_code`. `is_ready`
needs only the `claude` binary on PATH (reuse `programs.rs` lookup).
`endpoint` is the program name; `active_model` is a label, as for `Claude`.

### Agent fields

Three new nullable columns on `agents`, migration 66:

- `cwd TEXT` — working directory. Required for this kind (validated on save:
  exists, is a directory, is absolute). Other kinds ignore it.
- `claude_session TEXT` — the current session id. Set by the runtime, cleared
  by `/clear`, shown on the agent in the UI. Never edited by the operator.

Both drafts (`AgentDraft` Rust and TS) gain `cwd` as `Option` +
`#[serde(default)]`. `claude_session` is card-only. `own_model` returns `None`
for this provider, as it does for `Claude`: no model field exists for it.

### The turn

In `run_turn`, immediately after `inference_for` resolves to `ClaudeCode`, the
turn skips the round loop and calls `claude_code::run(...)`. It receives the
same `ChatRequest` message list `prompt.rs` already builds (so history, peer
messages and operator messages arrive in the shape every other provider gets).
Only the trailing user turns of this batch are sent to the process, because the
session already holds the earlier history. First turn with no session: send the
system prompt via `--append-system-prompt` once, together with the batch.

One process per turn:

```
claude -p --input-format stream-json --output-format stream-json --verbose
       --dangerously-skip-permissions
       (--session-id <new uuid> | --resume <stored>)
       [--append-system-prompt <card.system_prompt>]   # first turn only
```

cwd = the agent's `cwd`. stdin gets one `{"type":"user","message":{...}}`
line per inbound message, then closes. Per output line:

- `assistant` with `text` → `Token::Text` stream deltas (the reply).
- `assistant` with `thinking` → `Token::Reasoning` (shown, never kept).
- `assistant` with `tool_use` → `ToolStarted` event + a `Part::tool_call`.
- `user` with `tool_result` → `ToolFinished` with the matching Part.
- `result` → `Usage` (tokens from `usage`, `cost: Some(total_cost_usd)`),
  the `session_id` to store, and the turn ends. `is_error` becomes an
  `LlmError::ProgramFailed` carrying `result`.
- `system` and `rate_limit_event` lines are ignored.

Stop: the child is `kill_on_drop(true)`; the runtime's `until_stopped` drops
it. Same as the existing Claude provider. No retries: a failed turn is reported
as every other failed model call is; retrying would resend the batch into a
session that may have half-processed it.

The reply is `collected_text`, filed by the existing `ReplyMode` logic, which
answers the peer or operator who asked.

**Guaca's tools over the bridge (operator's ruling, 2026-11-01, superseding the
one-tool design).** The first live crew could message but could not see the
directory or make an agent, and the operator's terms are "no lost
functionality". So the daemon's MCP server lists exactly the tools the turn
would have offered a model: everything in `tools::specs` for the agent's grants
plus its crew's plugins, less `errand` (a session has subagents of its own). Each
turn's process gets `--mcp-config` pointing at it with the agent id and the
turn's run id in the URL; `--strict-mcp-config` is **not** passed, so the
operator's own MCP servers still load. `tools/call` goes through the same
`dispatch_tool` every other agent's calls take, with the turn's state held in a
relay for as long as the session runs, so limits, grants, parked permission
requests, trust labels and the words the model reads are unchanged. A tool use
of `mcp__guaca__<name>` in the stream draws as the existing chip for `<name>`.
This is the one place Guaca's tools reach the subprocess, and the only one
allowed.

**System prompt (revised with the above).** The full assembled prompt, tools
described, plus one section telling the session which tools are its own and how
the program names Guaca's. Sent on every turn by `--append-system-prompt-file`
(a private file, like the MCP config): the program builds its prompt per process
and keeps none of it with the session, and a live probe confirmed an appended
prompt does not stack across resumes.

**Cost (designer's call).** Same as the existing Claude provider: tokens are
counted and `cost` is `None`, because a plan is unpriced and the usage table's
meaning must not change. The `result` frame's `total_cost_usd` is dropped.

### Slash commands

The Composer does not intercept `/`. The runtime does, for this kind only: a
batch whose sole operator message is `/clear` clears `claude_session` and
replies "Session cleared." without spawning. Anything else starting with `/` is
sent to the process unchanged; `/compact` was verified to work in print mode.

### Resume in terminal

Command `resume_in_terminal(agent_id)`, one line in `surface!`. It:

1. Refuses with a clear message if the agent has no session yet, if `ghostty`
   is not on PATH, or if neither `DISPLAY` nor `WAYLAND_DISPLAY` is set in the
   daemon's environment (the Docker daemon). No new `Capabilities` flag
   (designer's call): the check lives in the command and the refusal names what
   to do ("run the daemon on your desktop", "install Ghostty"). The UI shows the
   item always and surfaces the refusal as it does other refusals.
2. Marks the agent `in_terminal` (an in-memory runtime flag, emitted as an
   `ActivityChanged`-style event so the UI can show "In your terminal").
3. Spawns `ghostty --working-directory=<cwd> -e claude --resume <session>` and
   awaits exit in a task; on exit clears the flag and nudges the agent so a
   waiting inbox drains.

While `in_terminal` is set, `run_turn` for that agent returns early without
consuming its inbox. The menu item lives in `AgentMenu.tsx` beside the other
actions.

### UI

- `SettingsDialog` and `GroupEditor` provider choice gains "Claude Code".
- `AgentEditor` shows, for this provider: Working directory (text, required)
  and the read-only session id with a copy affordance. No model field: the
  operator changes the model inside Claude Code (Resume in terminal, `/model`). Hides E2B/Kernel toggles for this kind.
- `trail.ts` already describes unknown tools as "Used Bash"; add friendly
  names for Claude Code's built-ins (Bash → "Ran a command", Edit/Write →
  "Edited <basename>", Read → "Read <basename>") in the same table the other
  tools use.

### Tests

`tests/claude_code.rs` modeled on `tests/claude.rs`: a fake `claude` script on
PATH that records argv and stdin and replays scripted stream-json. Cases, failure
paths first: binary missing; `is_error` result; process killed on Stop; cwd
missing on save is refused; `/clear` clears the session without spawning; tool
use lines become `ToolStarted`/`ToolFinished` and land in `tool_parts`;
`--resume` is passed on the second turn; `--append-system-prompt` only on the
first; cost is `Some`. One `#[ignore]`d live test spends the operator's plan.
UI: `AgentEditor` renders the two fields for this provider; the menu item is
present; the command's three refusals each have a test.

### Documentation

A section in `docs/ARCHITECTURE.md` after the subscription one: "A Claude Code
agent is a session, not a model", stating the reversal of the isolation flags
and why. A paragraph in `docs/gotchas/providers.md`. A short `docs/HOSTING.md`
note on running the daemon natively on Linux for this kind.

## Left to the builder

- Exact backfill and `coalesce` wiring in `store.rs`, `transfer.rs` allowlist.
- Whether the first-turn system prompt should be `--append-system-prompt` or
  the first user message; prefer the flag.
- The in-memory shape of `in_terminal` and the event name.
- Ordering of the native-daemon setup (a `scripts/native.sh` or just HOSTING
  instructions); keep whichever is smaller.
