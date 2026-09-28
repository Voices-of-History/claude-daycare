//! Claude Code, the agent Daycare was built on.
//!
//! This adapter only routes: the argv lives in `launch.rs`, the stream parser
//! and sandbox checks in `stream.rs`, the managed-policy and subscription
//! guard in `workspace.rs`, and the `/usage` meter in `usage_meter.rs`, all
//! unchanged from before agents were pluggable.

use super::{Agent, AgentKind, TranscriptDelivery, TurnSpec};
use crate::launch::{
    build_launch_plan, is_homecoming_tool, LaunchOptions, LaunchPlan, LaunchTools,
    ALLOWED_TURN_MODELS, DEFAULT_TURN_MODEL, MCP_SETTLE,
};
use crate::meter::WeeklyMeter;
use crate::paths::{shell_quote, shell_quote_path, Layout};
use crate::stream::{
    parse_stream, transcript_events, verify_sandbox, verify_world_was_reachable, SandboxAllowance,
    StreamReceipt, TurnEvent,
};
use crate::turn::TurnPurpose;
use crate::usage_meter::{sample_weekly_usage, WeeklyUsageSnapshot};
use crate::workspace::{guard_no_managed_claude, CONTROLLER_PROMPT, MCP_CONFIG};
use crate::{Error, Result};
use std::path::Path;
use std::time::Duration;

pub struct ClaudeAgent {
    bin: String,
    meter: ClaudeUsageMeter,
}

impl ClaudeAgent {
    pub fn new(bin: &str) -> Self {
        ClaudeAgent {
            bin: bin.to_string(),
            meter: ClaudeUsageMeter {
                bin: bin.to_string(),
            },
        }
    }
}

/// `/usage` typed into an interactive Claude Code under a PTY.
pub struct ClaudeUsageMeter {
    bin: String,
}

impl WeeklyMeter for ClaudeUsageMeter {
    fn sample(&self, model: &str, layout: &Layout) -> Result<WeeklyUsageSnapshot> {
        sample_weekly_usage(&self.bin, model, layout)
    }
}

impl Agent for ClaudeAgent {
    fn kind(&self) -> AgentKind {
        AgentKind::Claude
    }

    fn program(&self) -> &str {
        &self.bin
    }

    fn product_name(&self) -> &'static str {
        "Claude Code"
    }

    fn default_model(&self) -> &'static str {
        DEFAULT_TURN_MODEL
    }

    fn check_model(&self, model: &str) -> Result<()> {
        if ALLOWED_TURN_MODELS.contains(&model) {
            return Ok(());
        }
        Err(Error::new(format!(
            "--model must be one of {}; got {model:?}",
            ALLOWED_TURN_MODELS.join(", ")
        )))
    }

    fn reserves_session_ids(&self) -> bool {
        true
    }

    fn mcp_settle(&self) -> Duration {
        MCP_SETTLE
    }

    fn guard(&self, _spec: &TurnSpec<'_>) -> Result<()> {
        // Enterprise policy can inject managed CLAUDE.md into every ordinary
        // authenticated session and cannot be excluded by project settings.
        // Refuse it before the child receives either the turn prompt or the
        // device token.
        guard_no_managed_claude(&self.bin)
    }

    fn launch_plan(&self, spec: &TurnSpec<'_>) -> Result<LaunchPlan> {
        let mut plan = build_launch_plan(LaunchOptions {
            claude_bin: &self.bin,
            mode: spec.mode.clone(),
            message: spec.message,
            workspace: spec.workspace,
            mcp_config: &spec.workspace.join(MCP_CONFIG),
            system_prompt_file: &spec.workspace.join(CONTROLLER_PROMPT),
            tools: match spec.purpose {
                TurnPurpose::World => LaunchTools::DaycareWorld,
                TurnPurpose::PrivateHomecoming => LaunchTools::DaycareHomecoming,
                TurnPurpose::DayReport => LaunchTools::None,
            },
            model: spec.model,
        })?;
        // A visit is one Claude Code session resumed turn after turn, so its
        // context only grows. Left to defaults it ran to ~583k at 100 turns
        // and only compacted near the 1M ceiling at 200 (2026-09-01
        // checkpoint test). Josh: "it should compact at 250k context just like
        // my default setting" — 25% of a 1M window, regardless of what the
        // owner's own settings say.
        plan.env.extend([
            ("CLAUDE_CODE_AUTO_COMPACT_WINDOW".into(), "1000000".into()),
            ("CLAUDE_AUTOCOMPACT_PCT_OVERRIDE".into(), "25".into()),
        ]);
        if spec.purpose != TurnPurpose::DayReport {
            // Image generation waits on a remote model; Claude Code's default
            // MCP tool timeout gave up before Replicate finished, so the Claude
            // never saw the URL while the file still landed in the bucket
            // (live, 2026-08-26). Three minutes covers a cold model; the turn
            // timeout still bounds the whole visit turn above this.
            plan.env.push(("MCP_TOOL_TIMEOUT".into(), "180000".into()));
        }
        Ok(plan)
    }

    fn parse_receipt(&self, archive: &str) -> Result<StreamReceipt> {
        parse_stream(archive)
    }

    fn transcript_events(&self, archive: &str) -> Result<Vec<TurnEvent>> {
        transcript_events(archive)
    }

    fn verify_seal(
        &self,
        receipt: &StreamReceipt,
        purpose: TurnPurpose,
        workspace: &Path,
    ) -> Result<()> {
        let Some(init) = &receipt.init else {
            return Ok(());
        };
        let allowance = match purpose {
            TurnPurpose::PrivateHomecoming => SandboxAllowance::Read,
            TurnPurpose::World | TurnPurpose::DayReport => SandboxAllowance::None,
        };
        verify_sandbox(init, workspace, allowance)
    }

    fn verify_capability(&self, receipt: &StreamReceipt, purpose: TurnPurpose) -> Result<()> {
        match purpose {
            TurnPurpose::World => {
                if let Some(init) = &receipt.init {
                    verify_world_was_reachable(init)?;
                }
            }
            TurnPurpose::PrivateHomecoming => {
                // The homecoming's one job beyond reflection is saving
                // memories, so the memory tools must have been reachable; a
                // homecoming that silently could not save is the failure this
                // feature exists to prevent.
                match &receipt.init {
                    Some(init) => verify_world_was_reachable(init)?,
                    None => {
                        return Err(Error::new("private homecoming omitted its sandbox report"))
                    }
                }
                // A call the permission layer refused reached nothing; failing
                // on it would rerun the homecoming and re-save every memory a
                // second time.
                if let Some(name) = receipt
                    .permitted_tool_calls
                    .iter()
                    .find(|name| !is_homecoming_tool(name))
                {
                    return Err(Error::new(format!(
                        "private homecoming invoked {name}; only memory tools may be called after a visit"
                    )));
                }
                // Zero memory calls and an empty reply are a fine homecoming:
                // both are offered, never owed.
            }
            TurnPurpose::DayReport => {
                let exposed_capability = receipt
                    .init
                    .as_ref()
                    .is_none_or(|init| !init.tools.is_empty() || !init.mcp_servers.is_empty());
                if exposed_capability {
                    return Err(Error::new(
                        "day report started with tools or MCP servers enabled",
                    ));
                }
                if !receipt.tool_calls.is_empty() {
                    return Err(Error::new(
                        "day report invoked a tool instead of remaining local",
                    ));
                }
                // An empty reply is a fine report: offered, never owed.
            }
        }
        Ok(())
    }

    fn verify_archived(
        &self,
        receipt: &StreamReceipt,
        purpose: TurnPurpose,
        workspace: &Path,
    ) -> Result<()> {
        match purpose {
            TurnPurpose::PrivateHomecoming => {
                let init = receipt.init.as_ref().ok_or_else(|| {
                    Error::new("private homecoming receipt omitted its sandbox report")
                })?;
                verify_sandbox(init, workspace, SandboxAllowance::Read)?;
                // The memory tools had to be reachable, and nothing else may
                // have been called: a homecoming looks back and remembers; it
                // does not play on. A call the permission layer refused
                // reached nothing and does not count — failing on it would
                // rerun the turn and save every memory twice.
                verify_world_was_reachable(init)?;
                if let Some(name) = receipt
                    .permitted_tool_calls
                    .iter()
                    .find(|name| !is_homecoming_tool(name))
                {
                    return Err(Error::new(format!(
                        "private homecoming receipt invoked {name} and cannot be adopted; only memory tools may be called after a visit"
                    )));
                }
                Ok(())
            }
            TurnPurpose::DayReport => {
                let init = receipt
                    .init
                    .as_ref()
                    .ok_or_else(|| Error::new("day report receipt omitted its sandbox report"))?;
                verify_sandbox(init, workspace, SandboxAllowance::None)?;
                if !init.tools.is_empty() || !init.mcp_servers.is_empty() {
                    return Err(Error::new(
                        "day report receipt exposed tools or MCP servers",
                    ));
                }
                if !receipt.tool_calls.is_empty() {
                    return Err(Error::new(
                        "day report receipt invoked a tool and cannot be adopted",
                    ));
                }
                Ok(())
            }
            TurnPurpose::World => self.verify_seal(receipt, purpose, workspace),
        }
    }

    fn transcript_delivery(&self) -> TranscriptDelivery {
        TranscriptDelivery::ReadFile
    }

    fn meter(&self) -> Option<&dyn WeeklyMeter> {
        Some(&self.meter)
    }

    fn reopen_command(&self, workspace: &Path, session_id: Option<&str>) -> String {
        match session_id {
            Some(id) => format!(
                "cd {} && claude --resume {}",
                shell_quote_path(workspace),
                shell_quote(id)
            ),
            None => format!(
                "cd {} && claude   # no daycare session yet; run a turn first",
                shell_quote_path(workspace)
            ),
        }
    }
}
