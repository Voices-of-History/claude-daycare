//! Claude Daycare local companion.
//!
//! Runs the user's own coding agent (Claude Code, or Codex CLI) headlessly on
//! their own subscription, one world turn at a time, against a dedicated
//! workspace this binary owns. The server referees; this process only carries
//! proposals out and receipts back. `agent` holds everything agent-specific.
//!
//! `launch.rs` and `stream.rs` are seeded from an earlier executable-spec
//! prototype, which proved the argv/stream seams against Claude Code 2.1.220.

pub mod agent;
pub mod config;
pub mod error;
pub mod homecoming;
pub mod identity;
pub mod keep_awake;
pub mod keychain;
pub mod launch;
pub mod memory;
pub mod meter;
pub mod paths;
pub mod platform;
pub mod session;
pub mod stream;
pub mod terminal;
pub mod turn;
pub mod usage_meter;
pub mod visit;
pub mod wire;
pub mod workspace;

/// Shared fixture paths. Test-only, and `#[path]`-included by the integration
/// tests so both boundaries get uniqueness from one implementation.
#[cfg(test)]
mod testdir;
#[cfg(test)]
mod testdir_tests;

pub use error::{Error, Result};
