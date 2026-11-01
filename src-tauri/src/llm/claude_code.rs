//! The Claude Code provider, where an agent is a whole `claude` session.
//!
//! [`super::claude`] runs the program as a model: tools off, settings ignored,
//! one call per turn, so that Guaca's own loop stays the agent. This provider
//! reverses every one of those isolation flags. The session keeps its own
//! tools, its own working directory and its own memory, because that is what
//! the operator is asking for by choosing it; Guaca carries messages in and
//! out and no longer decides what the agent may do.

/// The program that is run, found on `PATH` like [`super::claude::PROGRAM`].
pub const PROGRAM: &str = "claude";

/// The model an agent runs when neither it nor its group names one.
pub const DEFAULT_MODEL: &str = "sonnet";
