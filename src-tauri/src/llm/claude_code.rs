//! The Claude Code provider, where an agent is a whole `claude` session.
//!
//! [`super::claude`] runs the program as a model: tools off, settings ignored,
//! one call per turn, so that Guaca's own loop stays the agent. This provider
//! reverses every one of those isolation flags. The session keeps its own
//! tools, its own working directory and its own memory, because that is what
//! the operator is asking for by choosing it; Guaca carries messages in and
//! out and no longer decides what the agent may do.

use std::path::PathBuf;

use crate::domain::ids::AgentId;

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

/// The model an agent runs when neither it nor its group names one.
pub const DEFAULT_MODEL: &str = "sonnet";
