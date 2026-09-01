//! Running exactly one Claude turn and turning it into a receipt.
//!
//! The raw stream is written to `turns/<command_id>.jsonl` as it arrives, so a
//! turn that times out or crashes still leaves the evidence of what happened.

use crate::launch::{
    build_launch_plan, is_homecoming_tool, validate_session_id, LaunchOptions, LaunchTools,
    SessionMode, DEVICE_TOKEN_ENV, STRIPPED_CHILD_ENV,
};
use crate::paths::create_private_dir;
use crate::stream::{
    parse_stream_file, verify_reached_the_world, verify_sandbox, verify_world_was_reachable,
    SandboxAllowance, StreamReceipt, WorldReach,
};
use crate::workspace::{guard_no_managed_claude, Workspace, CONTROLLER_PROMPT, MCP_CONFIG};
use crate::{Error, Result};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

pub const DEFAULT_TIMEOUT_SECS: u64 = 300;

/// The child exited before it accepted the turn message. This is the one
/// launch failure a resumed turn may recover from with a fresh session: no
/// model input reached Claude, so no tool call or other side effect can have
/// happened.
pub const PRE_INPUT_BROKEN_PIPE_ERROR: &str =
    "claude closed stdin before accepting the turn (broken pipe)";

pub struct TurnRequest<'a> {
    pub claude_bin: &'a str,
    pub workspace: &'a Workspace,
    pub mode: SessionMode,
    pub message: &'a str,
    pub device_token: &'a str,
    pub archive_path: &'a Path,
    pub timeout: Duration,
    pub purpose: TurnPurpose,
    /// One of `ALLOWED_TURN_MODELS`; the visit's stored choice, not the
    /// machine's `claude` default.
    pub model: &'a str,
    /// How long the MCP connection gets before the first input freezes the
    /// child's tool list. `MCP_SETTLE` in production; zero in tests, which
    /// never launch a real child.
    pub mcp_settle: Duration,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnPurpose {
    World,
    MatchPrep,
    AmbientPulse,
    /// After the visit, same session: memory tools only.
    PrivateHomecoming,
    /// After the private account, same session: no tools at all. The owner's
    /// story must never depend on the daycare server.
    DayReport,
}

#[derive(Debug, Clone)]
pub struct TurnOutcome {
    pub receipt: Option<StreamReceipt>,
    trusted_session_id: Option<String>,
    pub archive_path: PathBuf,
    pub elapsed_ms: u64,
    pub timed_out: bool,
    pub exit_code: Option<i32>,
    /// `None` means the turn ran clean, produced a receipt, and the child
    /// reported the sandbox we asked for.
    pub failure: Option<String>,
    /// The turn succeeded without calling any daycare tool: Claude watched,
    /// waited, or declined, and said so. A held turn is a turn, not a failure.
    pub held: bool,
}

impl TurnOutcome {
    pub fn succeeded(&self) -> bool {
        self.failure.is_none()
    }

    pub fn session_id(&self) -> Option<&str> {
        self.trusted_session_id.as_deref()
    }
}

pub fn run_turn(request: TurnRequest<'_>) -> Result<TurnOutcome> {
    if request.purpose != TurnPurpose::DayReport && request.device_token.trim().is_empty() {
        return Err(Error::new(
            "device token is empty; the MCP server would reject every tool call",
        ));
    }

    // Enterprise policy can inject managed CLAUDE.md into every ordinary
    // authenticated session and cannot be excluded by project settings. Refuse
    // it before the child receives either the turn prompt or the device token.
    guard_no_managed_claude(request.claude_bin)?;

    // Activation checks this too. Recheck immediately before every child
    // launch because a long visit can outlive a changed workspace-root symlink.
    let physical_workspace = request.workspace.guard_ancestors()?;

    let expected_session_id = match &request.mode {
        SessionMode::New {
            reserved_session_id,
        } => Some(reserved_session_id.clone()),
        SessionMode::Resume { session_id } => Some(session_id.clone()),
        SessionMode::Fork { .. } => None,
    };
    let plan = build_launch_plan(LaunchOptions {
        claude_bin: request.claude_bin,
        mode: request.mode,
        message: request.message,
        workspace: &physical_workspace,
        mcp_config: &physical_workspace.join(MCP_CONFIG),
        system_prompt_file: &physical_workspace.join(CONTROLLER_PROMPT),
        tools: match request.purpose {
            TurnPurpose::World => LaunchTools::DaycareWorld,
            TurnPurpose::MatchPrep => LaunchTools::DaycarePrep,
            TurnPurpose::AmbientPulse => LaunchTools::DaycareAmbientPulse,
            TurnPurpose::PrivateHomecoming => LaunchTools::DaycareHomecoming,
            TurnPurpose::DayReport => LaunchTools::None,
        },
        model: request.model,
    })?;
    // The plan preserves the established missing-file errors. This second check
    // adds the no-symlink property before the files reach Claude.
    Workspace::new(&physical_workspace).guard_scaffold_files()?;

    if let Some(parent) = request.archive_path.parent() {
        create_private_dir(parent)?;
    }
    let archive = std::fs::File::create(request.archive_path)?;
    set_owner_only(request.archive_path)?;

    let mut command = Command::new(&plan.program);
    command
        .args(&plan.args)
        .current_dir(&plan.cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for name in STRIPPED_CHILD_ENV {
        command.env_remove(name);
    }
    command.env_remove(DEVICE_TOKEN_ENV);
    // The only place the device token exists outside the keychain. The
    // homecoming turn needs it too: it saves the visit's memories through the
    // same MCP server the visit used. The day report has no server to reach.
    if request.purpose != TurnPurpose::DayReport {
        command.env(DEVICE_TOKEN_ENV, request.device_token);
        // Image generation waits on a remote model; Claude Code's default MCP
        // tool timeout gave up before Replicate finished, so the Claude never
        // saw the URL while the file still landed in the bucket (live,
        // 2026-08-26). Three minutes covers a cold model; the turn timeout
        // still bounds the whole visit turn above this.
        command.env("MCP_TOOL_TIMEOUT", "180000");
    }

    let started = Instant::now();
    let mut child = command.spawn().map_err(|error| {
        Error::new(format!(
            "could not start {}: {error}. Is Claude Code installed and on PATH?",
            plan.program
        ))
    })?;

    {
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| Error::new("child stdin unavailable"))?;
        // The child's tool list is frozen when this write lands, so the MCP
        // connection has to win the race against it.
        // `verify_world_was_reachable` fails the turn if it loses.
        std::thread::sleep(request.mcp_settle);
        if let Err(error) = stdin.write_all(plan.stdin.as_bytes()) {
            // A stale cwd-scoped Claude session can make `--resume` exit before
            // reading stdin. Reap it before returning so a caller can safely
            // start a replacement without leaving a concurrent child behind.
            drop(stdin);
            let _ = child.kill();
            let _ = child.wait();
            if error.kind() == std::io::ErrorKind::BrokenPipe {
                return Err(Error::new(PRE_INPUT_BROKEN_PIPE_ERROR));
            }
            return Err(error.into());
        }
        // Closing stdin ends the turn's input; the child answers and exits.
    }

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| Error::new("child stdout unavailable"))?;
    // Each line is flushed as it arrives, so a turn that is killed still leaves
    // the events it produced on disk.
    let (archive_done, archive_result) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut archive = archive;
        let mut outcome = Ok(());
        for line in BufReader::new(stdout).split(b'\n') {
            match line {
                Ok(mut line) => {
                    line.push(b'\n');
                    if let Err(error) = archive.write_all(&line).and_then(|_| archive.flush()) {
                        outcome = Err(error);
                        break;
                    }
                }
                Err(error) => {
                    outcome = Err(error);
                    break;
                }
            }
        }
        let _ = archive_done.send(outcome);
    });

    let stderr = child.stderr.take();
    let (stderr_done, stderr_result) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut collected = String::new();
        if let Some(stderr) = stderr {
            for line in BufReader::new(stderr)
                .lines()
                .map_while(std::result::Result::ok)
            {
                if collected.len() < 4000 {
                    collected.push_str(&line);
                    collected.push('\n');
                }
            }
        }
        let _ = stderr_done.send(collected);
    });

    let mut timed_out = false;
    let status = loop {
        match child.try_wait()? {
            Some(status) => break Some(status),
            None => {
                if started.elapsed() >= request.timeout {
                    timed_out = true;
                    let _ = child.kill();
                    break child.wait().ok();
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        }
    };

    let elapsed_ms = started.elapsed().as_millis() as u64;
    let exit_code = status.as_ref().and_then(|status| status.code());

    // A killed `claude` can leave a grandchild holding the pipe open, so the
    // reader threads get a deadline rather than an unbounded join. Whatever was
    // flushed before the deadline is the archive.
    let drain = Duration::from_secs(2);
    match archive_result.recv_timeout(drain) {
        Ok(Err(error)) => {
            return Err(Error::new(format!(
                "could not archive turn stream: {error}"
            )))
        }
        Ok(Ok(())) | Err(_) => {}
    }
    let stderr_text = stderr_result.recv_timeout(drain).unwrap_or_default();

    let mut failure = None;
    let mut held = false;
    if timed_out {
        failure = Some(format!(
            "turn exceeded {}s and was killed",
            request.timeout.as_secs()
        ));
    }

    let receipt = match parse_stream_file(request.archive_path) {
        Ok(receipt) => Some(receipt),
        Err(error) => {
            if failure.is_none() {
                failure = Some(format!("{error}{}", exit_note(exit_code, &stderr_text)));
            }
            None
        }
    };

    let mut trusted_session_id = None;
    if let Some(receipt) = &receipt {
        if validate_session_id(&receipt.session_id).is_err() {
            failure =
                Some("claude reported an invalid session id; it will not be persisted".into());
        } else if expected_session_id
            .as_deref()
            .is_some_and(|expected| receipt.session_id != expected)
        {
            failure = Some(
                "claude reported a different session id than the runner assigned; it will not be persisted"
                    .into(),
            );
        } else {
            trusted_session_id = Some(receipt.session_id.clone());
        }
        if let Some(init) = &receipt.init {
            let allowance = match request.purpose {
                TurnPurpose::MatchPrep => SandboxAllowance::WebSearch,
                TurnPurpose::PrivateHomecoming => SandboxAllowance::Read,
                TurnPurpose::World | TurnPurpose::AmbientPulse | TurnPurpose::DayReport => {
                    SandboxAllowance::None
                }
            };
            if let Err(error) = verify_sandbox(init, &request.workspace.dir, allowance) {
                // A sandbox violation outranks any other outcome: the turn may
                // have had more reach than Daycare allows.
                failure = Some(format!("sandbox check failed: {error}"));
            }
        }
        if failure.is_none() && !receipt.success {
            failure = Some(format!(
                "claude reported {}{}",
                receipt
                    .error_subtype
                    .clone()
                    .unwrap_or_else(|| "a failed turn".to_string()),
                exit_note(exit_code, &stderr_text)
            ));
        }
        // Only a turn that claims success can smuggle fiction into a receipt;
        // a turn that already failed has a more specific cause to report.
        if failure.is_none() {
            match request.purpose {
                TurnPurpose::World | TurnPurpose::MatchPrep | TurnPurpose::AmbientPulse => {
                    if let Some(init) = &receipt.init {
                        if let Err(error) = verify_world_was_reachable(init) {
                            failure = Some(error.to_string());
                        }
                    }
                    if failure.is_none() {
                        match verify_reached_the_world(receipt) {
                            Ok(WorldReach::Reached) => {}
                            Ok(WorldReach::Held) => held = true,
                            Err(error) => failure = Some(error.to_string()),
                        }
                    }
                }
                TurnPurpose::PrivateHomecoming => {
                    // The homecoming's one job beyond reflection is saving
                    // memories, so the memory tools must have been reachable;
                    // a homecoming that silently could not save is the failure
                    // this feature exists to prevent.
                    match &receipt.init {
                        Some(init) => {
                            if let Err(error) = verify_world_was_reachable(init) {
                                failure = Some(error.to_string());
                            }
                        }
                        None => {
                            failure = Some("private homecoming omitted its sandbox report".into());
                        }
                    }
                    // A call the permission layer refused reached nothing;
                    // failing on it would rerun the homecoming and re-save
                    // every memory a second time.
                    if failure.is_none() {
                        if let Some(name) = receipt
                            .permitted_tool_calls
                            .iter()
                            .find(|name| !is_homecoming_tool(name))
                        {
                            failure = Some(format!(
                                "private homecoming invoked {name}; only memory tools may be called after a visit"
                            ));
                        }
                    }
                    // Zero memory calls and an empty reply are a fine
                    // homecoming: both are offered, never owed.
                }
                TurnPurpose::DayReport => {
                    let exposed_capability = receipt
                        .init
                        .as_ref()
                        .is_none_or(|init| !init.tools.is_empty() || !init.mcp_servers.is_empty());
                    if exposed_capability {
                        failure =
                            Some("day report started with tools or MCP servers enabled".into());
                    } else if !receipt.tool_calls.is_empty() {
                        failure =
                            Some("day report invoked a tool instead of remaining local".into());
                    }
                    // An empty reply is a fine report: offered, never owed.
                }
            }
        }
    }

    if failure.is_none() && exit_code.unwrap_or(0) != 0 {
        failure = Some(format!(
            "claude exited {}{}",
            exit_code.unwrap_or(-1),
            exit_note(None, &stderr_text)
        ));
    }

    Ok(TurnOutcome {
        receipt,
        trusted_session_id,
        archive_path: request.archive_path.to_path_buf(),
        elapsed_ms,
        timed_out,
        exit_code,
        failure,
        held,
    })
}

fn exit_note(exit_code: Option<i32>, stderr: &str) -> String {
    let mut note = String::new();
    if let Some(code) = exit_code {
        note.push_str(&format!(" (exit {code})"));
    }
    let stderr = stderr.trim();
    if !stderr.is_empty() {
        let excerpt: String = stderr
            .chars()
            .rev()
            .take(300)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        note.push_str(&format!(": {excerpt}"));
    }
    note
}

#[cfg(unix)]
fn set_owner_only(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(())
}

#[cfg(not(unix))]
fn set_owner_only(_path: &Path) -> Result<()> {
    Ok(())
}
