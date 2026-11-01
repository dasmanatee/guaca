//! Whether a turn set to Claude Code is answered by a `claude` session.
//!
//! The fold, the argument vector and the batch are covered beside them in
//! `llm/claude_code.rs` without spawning anything. What this suite is for is
//! the seam those tests cannot see: that a config naming this provider reaches
//! that transport, that the process is started where the agent works and with
//! the session it should resume, that the batch arrives on its stdin, and that
//! a process which refuses or dies says so in its own words.
//!
//! The stand-in is a real executable on `PATH`, as in `tests/claude.rs`, and
//! it branches on what it was handed. Each binary in `tests/` is its own
//! process, so the two suites' stand-ins never meet on one `PATH`.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use guac_lib::config::{AppConfig, InferenceConfig, Provider};
use guac_lib::db::Store;
use guac_lib::domain::agent::CleanDraft;
use guac_lib::domain::envelope::{Envelope, Part};
use guac_lib::domain::group::{CleanGroup, InferenceOverrides};
use guac_lib::domain::ids::{AgentId, RunId};
use guac_lib::llm::claude_code::{Session, CLEARED};
use guac_lib::llm::openrouter::{ChatMessage, ChatRequest, LlmClient, LlmError};
use guac_lib::runtime::events::{RecordingSink, UiEvent};
use guac_lib::runtime::{OnDisk, Runtime};

/// What the test says, so the stdin the stand-in recorded can be checked for
/// it.
const MARKER: &str = "guaca-batch-arrived";

/// Held by every test that reads what the stand-in recorded.
///
/// The records are written beside the stand-in, one set for the whole binary,
/// and these tests run concurrently: without this one test reads the argv
/// another test's turn wrote.
static RECORDS: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Puts a stand-in `claude` on `PATH`, exactly once, and says where it is.
fn stand_in() -> PathBuf {
    static ONCE: std::sync::OnceLock<tempfile::TempDir> = std::sync::OnceLock::new();
    ONCE.get_or_init(|| {
        let dir = tempfile::tempdir().unwrap();
        write_stand_in(dir.path());
        let path = std::env::var("PATH").unwrap_or_default();
        std::env::set_var("PATH", format!("{}:{path}", dir.path().display()));
        dir
    })
    .path()
    .to_path_buf()
}

/// The stand-in: records where it was started, what it was told and what it
/// was handed, then answers on the basis of those.
fn write_stand_in(dir: &Path) {
    let script = format!(
        "#!/bin/sh\n\
         here='{here}'\n\
         pwd -P > \"$here/cwd\"\n\
         printf '%s\\n' \"$@\" > \"$here/argv\"\n\
         input=$(cat)\n\
         printf '%s\\n' \"$input\" > \"$here/stdin\"\n\
         case \" $* \" in\n\
           *' --resume missing '*) echo 'No conversation found with session ID: missing' >&2; exit 1 ;;\n\
         esac\n\
         case \"$input\" in\n\
           *'leave-behind'*)\n\
             echo 'lost its MCP server' >&2\n\
             sleep 30 </dev/null >/dev/null &\n\
             exit 2 ;;\n\
           *'die-midway'*)\n\
             printf '%s\\n' '{{\"type\":\"assistant\",\"message\":{{\"role\":\"assistant\",\"content\":[{{\"type\":\"text\",\"text\":\"starting\"}}]}}}}'\n\
             exit 0 ;;\n\
         esac\n\
         echo 'hook_started: SessionStart'\n\
         printf '%s\\n' '{{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"fixed-session\"}}'\n\
         printf '%s\\n' '{{\"type\":\"assistant\",\"message\":{{\"role\":\"assistant\",\"content\":[{{\"type\":\"tool_use\",\"id\":\"toolu_1\",\"name\":\"Bash\",\"input\":{{\"command\":\"echo hi\"}}}}]}},\"session_id\":\"fixed-session\"}}'\n\
         printf '%s\\n' '{{\"type\":\"user\",\"message\":{{\"role\":\"user\",\"content\":[{{\"tool_use_id\":\"toolu_1\",\"type\":\"tool_result\",\"content\":\"hi\",\"is_error\":false}}]}},\"session_id\":\"fixed-session\"}}'\n\
         printf '%s\\n' '{{\"type\":\"assistant\",\"message\":{{\"role\":\"assistant\",\"content\":[{{\"type\":\"text\",\"text\":\"done\"}}]}},\"session_id\":\"fixed-session\"}}'\n\
         printf '%s\\n' '{{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"session_id\":\"fixed-session\",\"result\":\"done\",\"total_cost_usd\":0.42,\"usage\":{{\"input_tokens\":1,\"cache_creation_input_tokens\":2,\"cache_read_input_tokens\":3,\"output_tokens\":4}}}}'\n",
        here = dir.display()
    );
    let at = dir.join("claude");
    let mut file = std::fs::File::create(&at).unwrap();
    file.write_all(script.as_bytes()).unwrap();
    drop(file);
    std::fs::set_permissions(&at, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
}

fn on_claude_code() -> InferenceConfig {
    InferenceConfig {
        provider: Provider::ClaudeCode,
        // Filled in and wrong, so a turn that reached an endpoint would fail
        // loudly rather than quietly.
        base_url: "http://127.0.0.1:1/not-this".into(),
        api_key: "not-this-either".into(),
        ..InferenceConfig::default()
    }
}

fn asking(
    cwd: &Path,
    resume: Option<&str>,
    system_prompt: Option<&str>,
    said: &str,
) -> ChatRequest {
    ChatRequest {
        model: "sonnet".into(),
        messages: vec![
            ChatMessage::system("you are Dana"),
            ChatMessage::user(format!("[OPERATOR]\n{said}")),
        ],
        claude_code: Some(Session {
            agent_id: AgentId::new(),
            cwd: cwd.to_path_buf(),
            resume: resume.map(str::to_string),
            system_prompt: system_prompt.map(str::to_string),
            bridge: None,
        }),
        ..ChatRequest::default()
    }
}

fn recorded(beside: &Path, what: &str) -> String {
    std::fs::read_to_string(beside.join(what)).expect("the stand-in records what it was given")
}

#[tokio::test]
async fn a_missing_session_is_the_programs_own_refusal() {
    // The operator deleted the session's file. The program's words say so, and
    // they are what the channel shows; `/clear` is the way out.
    let _records = RECORDS.lock().await;
    stand_in();
    let cwd = tempfile::tempdir().unwrap();

    let err = LlmClient::new()
        .unwrap()
        .stream_chat(&on_claude_code(), &asking(cwd.path(), Some("missing"), None, MARKER), |_| {})
        .await
        .expect_err("a session the program does not have cannot be resumed");

    assert!(matches!(err, LlmError::ProgramFailed { .. }), "{err}");
    assert!(err.to_string().contains("No conversation found"), "{err}");
    assert!(!err.is_transient(), "resending the batch cannot bring the session back");
}

#[tokio::test]
async fn a_program_that_dies_midway_reports_its_exit() {
    let _records = RECORDS.lock().await;
    stand_in();
    let cwd = tempfile::tempdir().unwrap();

    let err = LlmClient::new()
        .unwrap()
        .stream_chat(&on_claude_code(), &asking(cwd.path(), None, None, "die-midway"), |_| {})
        .await
        .expect_err("a stream with no result frame is not an answer");

    assert!(matches!(err, LlmError::ProgramFailed { .. }), "{err}");
    let said = err.to_string();
    assert!(said.contains("ended without answering"), "{said}");
    assert!(said.contains("exit 0"), "{said}");
}

#[tokio::test]
async fn a_program_that_leaves_a_process_holding_stderr_still_reports() {
    // The session may start the operator's MCP servers and run commands, and
    // those inherit its stderr. One left behind keeps the pipe open after the
    // program itself is gone, and the turn must not wait for it.
    let _records = RECORDS.lock().await;
    stand_in();
    let cwd = tempfile::tempdir().unwrap();

    let started = std::time::Instant::now();
    let err = tokio::time::timeout(
        std::time::Duration::from_secs(20),
        LlmClient::new().unwrap().stream_chat(
            &on_claude_code(),
            &asking(cwd.path(), None, None, "leave-behind"),
            |_| {},
        ),
    )
    .await
    .expect("the report must not wait on a process the program left behind")
    .expect_err("a stream with no result frame is not an answer");

    assert!(started.elapsed() < std::time::Duration::from_secs(20));
    let said = err.to_string();
    assert!(said.contains("exit 2"), "{said}");
    // What it wrote before it went is still the account of it.
    assert!(said.contains("lost its MCP server"), "{said}");
}

#[tokio::test]
async fn a_turn_set_to_claude_code_is_answered_by_the_program() {
    let _records = RECORDS.lock().await;
    let beside = stand_in();
    let cwd = tempfile::tempdir().unwrap();

    let completion = LlmClient::new()
        .unwrap()
        .stream_chat(
            &on_claude_code(),
            &asking(cwd.path(), None, Some("you are Dana"), MARKER),
            |_| {},
        )
        .await
        .expect("the program should have answered");

    assert_eq!(completion.content, "done");
    assert!(completion.tool_calls.is_empty(), "the session's tools already ran");
    assert_eq!(completion.work.len(), 1);
    let Part::ToolCall { name, .. } = &completion.work[0] else { panic!("{:?}", completion.work) };
    assert_eq!(name, "Bash");
    assert_eq!(completion.session.as_deref(), Some("fixed-session"));
    let usage = completion.usage.unwrap();
    assert_eq!(usage.prompt_tokens, 6);
    assert_eq!(usage.completion_tokens, 4);
    assert_eq!(usage.cost, None);

    let argv = recorded(&beside, "argv");
    assert!(argv.lines().any(|a| a == "--session-id"), "{argv}");
    assert!(argv.lines().any(|a| a == "--append-system-prompt"), "{argv}");
    assert!(recorded(&beside, "stdin").contains(MARKER));

    let started_in = recorded(&beside, "cwd");
    assert_eq!(Path::new(started_in.trim()), std::fs::canonicalize(cwd.path()).unwrap());
}

#[tokio::test]
async fn a_second_turn_resumes_and_sends_no_system_prompt() {
    let _records = RECORDS.lock().await;
    let beside = stand_in();
    let cwd = tempfile::tempdir().unwrap();

    LlmClient::new()
        .unwrap()
        .stream_chat(
            &on_claude_code(),
            &asking(cwd.path(), Some("fixed-session"), None, MARKER),
            |_| {},
        )
        .await
        .expect("the program should have answered");

    let argv = recorded(&beside, "argv");
    let args: Vec<&str> = argv.lines().collect();
    assert!(args.windows(2).any(|w| w == ["--resume", "fixed-session"]), "{argv}");
    assert!(!args.contains(&"--append-system-prompt"), "{argv}");
    assert!(!args.contains(&"--session-id"), "{argv}");
    // Asked for on the request and still not passed: the session chooses.
    assert!(!args.contains(&"--model"), "{argv}");
}

/// One agent in a group set to Claude Code, working in a folder of its own,
/// with the whole runtime around it.
///
/// The app is left on an endpoint that is not listening, so a turn that
/// resolved the app's provider instead of the group's fails rather than
/// passing by accident.
struct Crew {
    runtime: Runtime,
    sink: Arc<RecordingSink>,
    agent: AgentId,
    _cwd: tempfile::TempDir,
    _dir: tempfile::TempDir,
}

fn crew() -> Crew {
    let dir = tempfile::tempdir().unwrap();
    let cwd = tempfile::tempdir().unwrap();
    let store = Store::open(&dir.path().join("guac.db")).unwrap();
    let group = store
        .create_group(&CleanGroup {
            name: "Coders".into(),
            inference: Some(InferenceOverrides {
                provider: Some(Provider::ClaudeCode),
                ..Default::default()
            }),
            ..Default::default()
        })
        .unwrap()
        .id;
    let agent = store
        .create_agent(&CleanDraft {
            group_id: Some(group),
            name: "Dana".into(),
            avatar: "avocado".into(),
            color: "#7fb069".into(),
            model: String::new(),
            subscription_model: None,
            cwd: Some(cwd.path().display().to_string()),
            reasoning_effort: None,
            system_prompt: "You are Dana.".into(),
            skills: Vec::new(),
        })
        .unwrap()
        .id;

    let config = AppConfig {
        version: guac_lib::config::CURRENT_VERSION,
        inference: InferenceConfig {
            base_url: "http://127.0.0.1:1/v1".into(),
            default_model: "endpoint/model-that-would-fail".into(),
            request_timeout_secs: 10,
            ..Default::default()
        },
        ..Default::default()
    };
    let sink = RecordingSink::new();
    let runtime = Runtime::new(
        store,
        LlmClient::new().unwrap(),
        config,
        OnDisk::under(dir.path()),
        sink.clone(),
    );
    runtime.start_all().unwrap();
    Crew { runtime, sink, agent, _cwd: cwd, _dir: dir }
}

impl Crew {
    fn ask(&self, text: &str) -> RunId {
        self.runtime.send_from_human(self.agent, text).unwrap()
    }

    fn session(&self) -> Option<String> {
        self.runtime.store().get_agent(self.agent).unwrap().unwrap().claude_session
    }

    fn channel(&self) -> Vec<Envelope> {
        self.runtime.store().channel_messages(self.agent, 50).unwrap()
    }

    /// Waits for a run to go quiet.
    async fn settle(&self, run: RunId) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        loop {
            let settled = self
                .sink
                .count_of(|e| matches!(e, UiEvent::RunSettled { run_id, .. } if *run_id == run));
            if settled > 0 {
                // Let the last write land before anything reads the store.
                tokio::time::sleep(std::time::Duration::from_millis(60)).await;
                return;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "run never settled: {:?}",
                self.channel().iter().map(|m| &m.parts).collect::<Vec<_>>()
            );
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_first_turn_stores_the_session_and_a_second_resumes_it() {
    let _records = RECORDS.lock().await;
    let beside = stand_in();
    let crew = crew();

    let run = crew.ask(MARKER);
    crew.settle(run).await;

    assert_eq!(crew.session().as_deref(), Some("fixed-session"), "{:?}", crew.channel());
    let first = recorded(&beside, "argv");
    assert!(first.lines().any(|a| a == "--session-id"), "{first}");
    assert!(first.lines().any(|a| a == "--append-system-prompt"), "{first}");
    // The prompt the runtime built for this agent, and how to reach its crew.
    assert!(first.contains("You are Dana."), "{first}");
    assert!(first.contains("call the send_message tool your guaca MCP server provides"), "{first}");
    assert!(recorded(&beside, "stdin").contains(MARKER));
    // The session's own work is on the turn's record, where the operator
    // reads what an agent did.
    let channel = crew.channel();
    assert!(
        channel
            .iter()
            .flat_map(|m| &m.parts)
            .any(|p| matches!(p, Part::ToolCall { name, .. } if name == "Bash")),
        "{channel:?}"
    );

    let run = crew.ask("and again");
    crew.settle(run).await;

    let second = recorded(&beside, "argv");
    let args: Vec<&str> = second.lines().collect();
    assert!(args.windows(2).any(|w| w == ["--resume", "fixed-session"]), "{second}");
    assert!(!args.contains(&"--append-system-prompt"), "the session already has it: {second}");
    assert!(recorded(&beside, "stdin").contains("and again"));
    assert!(!recorded(&beside, "stdin").contains(MARKER), "the session already read that turn");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_clear_forgets_the_session_without_running_the_program() {
    let _records = RECORDS.lock().await;
    let beside = stand_in();
    let crew = crew();
    crew.runtime.store().set_claude_session(crew.agent, Some("fixed-session")).unwrap();
    let _ = std::fs::remove_file(beside.join("argv"));

    let run = crew.ask("/clear");
    crew.settle(run).await;

    assert_eq!(crew.session(), None);
    assert!(!beside.join("argv").exists(), "the program was started for a /clear");
    let last = crew.channel().into_iter().last().unwrap();
    assert_eq!(last.plain_text(), CLEARED);
    assert!(crew.sink.count_of(|e| matches!(e, UiEvent::AgentsChanged)) > 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_claude_code_turn_is_offered_no_tools() {
    // Guaca's tools are the session's to reach through the bridge, not a list
    // the runtime hands it: offering them would also restrict the session's
    // own.
    let _records = RECORDS.lock().await;
    let beside = stand_in();
    let crew = crew();

    let run = crew.ask(MARKER);
    crew.settle(run).await;

    let argv = recorded(&beside, "argv");
    for restriction in ["--tools", "--allowedTools", "--disallowedTools"] {
        assert!(!argv.lines().any(|a| a == restriction), "{restriction} in {argv}");
    }
    assert_eq!(crew.session().as_deref(), Some("fixed-session"), "{:?}", crew.channel());
}

/// The live half: whether the real program still accepts this command line,
/// runs a tool in the directory it was started in, and reports it in the shape
/// the fold reads.
///
/// Run on its own (`--ignored a_live_turn_runs_a_command`): alongside the rest
/// of this suite the stand-in would be first on `PATH`.
#[tokio::test]
#[ignore = "live: spends the operator's own Claude plan"]
async fn a_live_turn_runs_a_command() {
    let cwd = tempfile::tempdir().unwrap();
    let request = asking(
        cwd.path(),
        None,
        Some("You are a test agent. Do exactly what you are asked and nothing more."),
        "Run the shell command `echo hi` with your Bash tool, then say what it printed.",
    );

    let completion = LlmClient::new()
        .unwrap()
        .stream_chat(&on_claude_code(), &request, |_| {})
        .await
        .expect("the real program should answer");

    assert!(
        completion
            .work
            .iter()
            .any(|part| matches!(part, Part::ToolCall { name, .. } if name == "Bash")),
        "{completion:?}"
    );
    assert!(completion.session.is_some(), "{completion:?}");
}
