//! The seam between the shared visit machinery and the coding agent that
//! actually lives the visit.
//!
//! Everything agent-specific — the binary, its argv and environment, how its
//! output stream is parsed, how the runner proves the seal held, which meter
//! reads its weekly allowance — sits behind `Agent`. The visit loop, ledger,
//! homecoming renderer, platform client, and identity/token handling are
//! shared and only ever see the normalized `StreamReceipt`.
//!
//! `AgentKind` is the data half: it is stored in visit records and keys
//! `sessions.json`, because a Claude session id cannot be resumed by Codex.

pub mod claude;

use crate::launch::{LaunchPlan, SessionMode};
use crate::meter::WeeklyMeter;
use crate::paths::Layout;
use crate::stream::{StreamReceipt, TurnEvent};
use crate::turn::TurnPurpose;
use crate::{Error, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::time::Duration;

/// Which agent runs a visit. Serialized lowercase into visit records and
/// session keys; a record without the field is a Claude visit.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentKind {
    #[default]
    Claude,
}

impl AgentKind {
    pub fn as_str(self) -> &'static str {
        match self {
            AgentKind::Claude => "claude",
        }
    }

    pub fn parse(value: &str) -> Result<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "claude" => Ok(AgentKind::Claude),
            other => Err(Error::new(format!("--agent must be claude; got {other:?}"))),
        }
    }

    pub fn is_claude(self) -> bool {
        self == AgentKind::Claude
    }

    /// For `skip_serializing_if`, which passes a reference.
    pub fn is_claude_ref(kind: &AgentKind) -> bool {
        kind.is_claude()
    }
}

impl std::fmt::Display for AgentKind {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// One turn as the shared runner asks for it. The agent turns this into a
/// concrete command line; nothing in here is agent-specific.
pub struct TurnSpec<'a> {
    pub mode: &'a SessionMode,
    pub message: &'a str,
    /// The character's name, for agents whose persona travels with the turn
    /// instead of living in a workspace file.
    pub actor_name: &'a str,
    /// The physical (symlink-resolved) workspace, already guarded.
    pub workspace: &'a Path,
    pub purpose: TurnPurpose,
    pub model: &'a str,
}

/// How the homecoming reader gets the rendered visit transcript.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TranscriptDelivery {
    /// Written into the workspace and read with a file tool scoped to it
    /// (Claude's `Read(./homecoming/**)`).
    ReadFile,
    /// Put in the reader's prompt. For agents with no file tool narrow enough
    /// to grant: Codex's only reader is a shell, and a read-only sandbox still
    /// reads the whole disk.
    Inline,
}

pub trait Agent {
    fn kind(&self) -> AgentKind;

    /// The binary this adapter launches.
    fn program(&self) -> &str;

    /// How the agent is named in failure text (`claude reported …`).
    fn label(&self) -> &'static str {
        self.kind().as_str()
    }

    /// The product name for people (`Claude Code`, `Codex`).
    fn product_name(&self) -> &'static str;

    /// The model a visit runs on when the person names none.
    fn default_model(&self) -> &'static str;

    /// Refuse a model this agent will not run a visit on.
    fn check_model(&self, model: &str) -> Result<()>;

    /// True when the runner picks the session id before launch (Claude's
    /// `--session-id`); false when the agent mints it and reports it in the
    /// stream (Codex's `thread.started`).
    fn reserves_session_ids(&self) -> bool;

    /// How long the MCP connection gets before the first input. Only Claude
    /// freezes its tool list on first input; everyone else returns zero.
    fn mcp_settle(&self) -> Duration {
        Duration::ZERO
    }

    /// Everything that must hold before any child of this agent is started
    /// with a prompt or the device token: managed policy, login, and — for
    /// agents whose seal can be inspected without a model call — proof that
    /// the seal holds for exactly this turn.
    fn guard(&self, spec: &TurnSpec<'_>) -> Result<()>;

    /// The command line, stdin, and environment changes for one turn. The
    /// device token is not in here: `run_turn` adds it for turns that reach
    /// the daycare server.
    fn launch_plan(&self, spec: &TurnSpec<'_>) -> Result<LaunchPlan>;

    /// Runs after every child exits, whatever happened to the turn.
    fn after_turn(&self) -> Result<()> {
        Ok(())
    }

    /// The receipt for one archived turn stream.
    fn parse_receipt(&self, archive: &str) -> Result<StreamReceipt>;

    /// The transcript events of one archived turn, leniently: a partial
    /// archive still renders what it holds. Fails only on a line that is not
    /// JSON, with `line N is not stream JSON: …`.
    fn transcript_events(&self, archive: &str) -> Result<Vec<TurnEvent>>;

    /// Proof the turn ran with no more reach than Daycare allows. Checked on
    /// every turn; a failure outranks any other outcome.
    fn verify_seal(
        &self,
        receipt: &StreamReceipt,
        purpose: TurnPurpose,
        workspace: &Path,
    ) -> Result<()>;

    /// Proof the turn had the reach its purpose needs and used no more: a
    /// world turn could reach the world, a homecoming called only memory
    /// tools, a day report called nothing. Checked on turns that claim
    /// success. Whether a world turn held or invented a world is shared
    /// (`stream::verify_reached_the_world`).
    fn verify_capability(&self, receipt: &StreamReceipt, purpose: TurnPurpose) -> Result<()>;

    /// The same proof for a homecoming or day-report archive found on disk
    /// after a crash, before its text is adopted.
    fn verify_archived(
        &self,
        receipt: &StreamReceipt,
        purpose: TurnPurpose,
        workspace: &Path,
    ) -> Result<()>;

    fn transcript_delivery(&self) -> TranscriptDelivery;

    /// The meter for "percent of the weekly allowance", or `None` when the
    /// agent has no such meter and a visit is bounded by a token cap instead.
    fn meter(&self) -> Option<&dyn WeeklyMeter>;

    /// The shell command that opens the same session interactively.
    fn reopen_command(&self, workspace: &Path, session_id: Option<&str>) -> String;
}

/// The binaries a command line named, so an adapter for any kind can be built
/// later (a visit resumed from its record uses the record's agent, not the
/// flag).
#[derive(Debug, Clone)]
pub struct AgentBins {
    pub claude: String,
}

impl Default for AgentBins {
    fn default() -> Self {
        AgentBins {
            claude: "claude".into(),
        }
    }
}

/// Build the adapter for `kind`. `layout` locates the runner-owned state an
/// adapter needs (Codex's sealed home).
pub fn agent(kind: AgentKind, bins: &AgentBins, _layout: &Layout) -> Box<dyn Agent> {
    match kind {
        AgentKind::Claude => Box::new(claude::ClaudeAgent::new(&bins.claude)),
    }
}
