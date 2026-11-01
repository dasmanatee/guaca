//! "Test connection" on the Claude Code provider reports the program, not a
//! model call.
//!
//! Its own binary because it empties `PATH`, which would take the stand-in
//! away from every test in `tests/claude_code.rs`.

use guac_lib::config::{AppConfig, InferenceConfig, Provider};
use guac_lib::db::Store;
use guac_lib::llm::openrouter::{LlmClient, LlmError};
use guac_lib::runtime::events::RecordingSink;
use guac_lib::runtime::{OnDisk, Runtime};

#[tokio::test]
async fn testing_the_connection_without_the_program_says_it_is_missing() {
    let dir = tempfile::tempdir().unwrap();
    let empty = tempfile::tempdir().unwrap();
    std::env::set_var("PATH", empty.path());

    let config = AppConfig {
        version: guac_lib::config::CURRENT_VERSION,
        inference: InferenceConfig { provider: Provider::ClaudeCode, ..InferenceConfig::default() },
        ..Default::default()
    };
    let runtime = Runtime::new(
        Store::open(&dir.path().join("guac.db")).unwrap(),
        LlmClient::new().unwrap(),
        config.clone(),
        OnDisk::under(dir.path()),
        RecordingSink::new(),
    );

    let err = runtime.probe(&config).await.expect_err("no claude on PATH");
    assert!(matches!(err, LlmError::ProgramMissing { program: "claude" }), "{err:?}");
}
