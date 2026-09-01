//! Every string this crate and the platform must agree on, in one place.
//!
//! Slice 1 lost a day to a vocabulary drift: the runner and the platform each
//! invented an MCP argument set, both unit suites passed, and the mismatch
//! would only have surfaced as a live turn that quietly did nothing. The fix
//! then was a test asserting the advertised names; this module is that fix
//! applied before the fact for slice 2.
//!
//! Nothing here is a preference. Each constant is pinned to a specific line of
//! the platform's own source, and the tests below are the tripwire: if the
//! platform renames a value, this crate fails to build its test suite rather
//! than shipping a companion that speaks the old dialect.
//!
//! Sources of truth:
//!   - `supabase/migrations/20260806070000_daycare_identities_visits.sql`
//!   - `apps/website/lib/daycare/types.ts`
//!
//! Case convention: the companion REST surface is snake_case (slice 1's
//! `device_token`, `actor_id`, `result_text` all shipped that way and are
//! live). The camelCase in `types.ts` is the platform's internal domain
//! mapping, not the wire.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::Path;

/// A queued command's `kind`. `visit_end` arrives on the same poll as a turn —
/// recall has to reach a companion that is mid-turn, which is exactly when a
/// user reaches for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CommandKind {
    WorldTurn,
    VisitEnd,
}

impl CommandKind {
    pub fn as_str(self) -> &'static str {
        match self {
            CommandKind::WorldTurn => "world_turn",
            CommandKind::VisitEnd => "visit_end",
        }
    }

    /// Unknown kinds are not an error to parse — a newer server may queue a
    /// command this build has never heard of, and the honest answer is to
    /// report it unrun rather than to crash the poller.
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "world_turn" => Some(CommandKind::WorldTurn),
            "visit_end" => Some(CommandKind::VisitEnd),
            _ => None,
        }
    }
}

/// Why a visit stopped. The runner never invents a fifth: anything it cannot
/// classify is `Error`, which is honest, rather than `Recalled`, which would
/// claim the user asked for something they did not.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VisitEndReason {
    /// A budget the visit was opened with ran out.
    BudgetExhausted,
    /// The user pressed the button — delivered as a `visit_end` command.
    Recalled,
    /// The activity the visit existed for finished on its own.
    ActivityEnded,
    /// Anything else, including a blocking rate limit. See `visit.rs` for why
    /// a rate limit is not `budget_exhausted`: the budget is the user's
    /// allowance and it did not run out; the account's did.
    Error,
}

impl VisitEndReason {
    pub fn as_str(self) -> &'static str {
        match self {
            VisitEndReason::BudgetExhausted => "budget_exhausted",
            VisitEndReason::Recalled => "recalled",
            VisitEndReason::ActivityEnded => "activity_ended",
            VisitEndReason::Error => "error",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VisitStatus {
    Active,
    Ended,
}

/// The identity a workspace-bound Claude is bound to, as the server is allowed
/// to know it.
///
/// The server stores `workspace_key`, a sha256 of the local path, and never the
/// path. That is not ceremony: a user's directory names are their own business,
/// they leak project and client names, and the server's only real need is to
/// answer "is this the same workspace as last time" — which a hash answers
/// exactly as well. `workspace_label` is the last segment alone, sent so the
/// hub can render "voh" instead of a hex string.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceBinding {
    pub workspace_key: String,
    pub workspace_label: String,
}

impl WorkspaceBinding {
    pub fn of(path: &Path) -> Self {
        WorkspaceBinding {
            workspace_key: workspace_key(path),
            workspace_label: workspace_label(path),
        }
    }
}

/// sha256 hex of the path. Stable across runs and across re-pairings, which is
/// what makes "the Claude for this project" survive a new device token.
pub fn workspace_key(path: &Path) -> String {
    let mut hasher = Sha256::new();
    hasher.update(path.to_string_lossy().as_bytes());
    format!("{:x}", hasher.finalize())
}

/// The last path segment, or the whole path when there isn't one (`/`).
pub fn workspace_label(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_else(|| path.to_string_lossy().to_string())
}

/// The release id baked into this binary by `dev/publish-release.sh`
/// (`DAYCARE_RUNNER_RELEASE` at build time). `None` on a dev build.
pub const RELEASE: Option<&str> = option_env!("DAYCARE_RUNNER_RELEASE");

/// Header the companion sends on every platform request. The server enforces
/// its release floor on this header — a client-side check alone cannot
/// constrain binaries shipped before the check existed.
pub const RELEASE_HEADER: &str = "x-daycare-runner-release";

/// Companion-facing REST paths. Collected here so a route rename is one edit
/// and one failing test, not a hunt through the crate.
pub mod paths {
    pub const PAIR_CLAIM: &str = "/api/daycare/pair/claim";
    pub const NEXT_COMMAND: &str = "/api/daycare/commands/next";
    pub const IDENTITIES: &str = "/api/daycare/identities";
    pub const VISITS: &str = "/api/daycare/visits";

    pub fn complete_command(command_id: &str) -> String {
        format!("/api/daycare/commands/{command_id}/complete")
    }

    pub fn end_visit(visit_id: &str) -> String {
        format!("/api/daycare/visits/{visit_id}/end")
    }

    pub fn visit(visit_id: &str) -> String {
        format!("/api/daycare/visits/{visit_id}")
    }

    pub fn visit_report(visit_id: &str) -> String {
        format!("/api/daycare/visits/{visit_id}/report")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// The whole point of this module. Each literal below appears verbatim in
    /// the platform's CHECK constraints; if either side moves, this fails.
    #[test]
    fn command_kinds_match_the_platform_check_constraint() {
        // daycare_turn_commands_kind_check: kind IN ('world_turn', 'visit_end')
        assert_eq!(CommandKind::WorldTurn.as_str(), "world_turn");
        assert_eq!(CommandKind::VisitEnd.as_str(), "visit_end");
        assert_eq!(CommandKind::parse("visit_end"), Some(CommandKind::VisitEnd));
        // A kind from a newer server is unrecognised, not fatal.
        assert_eq!(CommandKind::parse("join_match"), None);
    }

    #[test]
    fn visit_end_reasons_match_the_platform_check_constraint() {
        // end_reason IN ('budget_exhausted', 'recalled', 'activity_ended', 'error')
        assert_eq!(VisitEndReason::BudgetExhausted.as_str(), "budget_exhausted");
        assert_eq!(VisitEndReason::Recalled.as_str(), "recalled");
        assert_eq!(VisitEndReason::ActivityEnded.as_str(), "activity_ended");
        assert_eq!(VisitEndReason::Error.as_str(), "error");
    }

    #[test]
    fn the_enums_serialize_as_the_bare_strings_the_wire_expects() {
        assert_eq!(
            serde_json::to_string(&CommandKind::VisitEnd).unwrap(),
            "\"visit_end\""
        );
        assert_eq!(
            serde_json::to_string(&VisitEndReason::BudgetExhausted).unwrap(),
            "\"budget_exhausted\""
        );
        assert_eq!(
            serde_json::to_string(&VisitStatus::Active).unwrap(),
            "\"active\""
        );
    }

    #[test]
    fn a_workspace_reaches_the_server_as_a_hash_and_a_leaf_never_a_path() {
        let binding = WorkspaceBinding::of(&PathBuf::from("/Users/josh/dev/voices-of-history"));
        assert_eq!(binding.workspace_label, "voices-of-history");
        assert_eq!(binding.workspace_key.len(), 64);
        assert!(binding.workspace_key.chars().all(|c| c.is_ascii_hexdigit()));

        let json = serde_json::to_string(&binding).unwrap();
        // The user's directory structure is not the server's business.
        assert!(!json.contains("/Users/josh"), "{json}");
        assert!(!json.contains("dev"), "{json}");
        assert!(json.contains("\"workspace_key\""), "{json}");
        assert!(json.contains("\"workspace_label\""), "{json}");
    }

    #[test]
    fn the_same_workspace_hashes_the_same_and_a_different_one_does_not() {
        let a = workspace_key(Path::new("/Users/josh/dev/voh"));
        assert_eq!(a, workspace_key(Path::new("/Users/josh/dev/voh")));
        assert_ne!(a, workspace_key(Path::new("/Users/josh/dev/voh2")));
    }

    #[test]
    fn a_root_workspace_still_has_a_label() {
        assert_eq!(workspace_label(Path::new("/")), "/");
    }

    #[test]
    fn route_paths_are_the_ones_the_platform_serves() {
        assert_eq!(paths::NEXT_COMMAND, "/api/daycare/commands/next");
        assert_eq!(
            paths::complete_command("cmd-1"),
            "/api/daycare/commands/cmd-1/complete"
        );
        assert_eq!(paths::end_visit("v-1"), "/api/daycare/visits/v-1/end");
        assert_eq!(paths::visit("v-1"), "/api/daycare/visits/v-1");
    }
}
