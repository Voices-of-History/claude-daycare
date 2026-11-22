//! Running exactly one agent turn and turning it into a receipt.
//!
//! The raw stream is written to `turns/<command_id>.jsonl` as it arrives, so a
//! turn that times out or crashes still leaves the evidence of what happened.

use crate::agent::{Agent, TurnSpec};
use crate::launch::{validate_session_id, SessionMode, DEVICE_TOKEN_ENV};
use crate::paths::create_private_dir;
use crate::stream::{verify_reached_the_world, StreamReceipt, WorldReach};
use crate::workspace::Workspace;
use crate::{Error, Result};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

pub const DEFAULT_TIMEOUT_SECS: u64 = 300;

/// The child exited before it accepted the turn message. This is the one
/// launch failure a resumed turn may recover from with a fresh session: no
/// model input reached the agent, so no tool call or other side effect can have
/// happened.
pub const PRE_INPUT_BROKEN_PIPE_ERROR: &str =
    "claude closed stdin before accepting the turn (broken pipe)";

pub struct TurnRequest<'a> {
    pub agent: &'a dyn Agent,
    pub workspace: &'a Workspace,
    pub mode: SessionMode,
    pub message: &'a str,
    /// The character this turn is for; agents whose persona travels with the
    /// turn need it.
    pub actor_name: &'a str,
    pub device_token: &'a str,
    pub archive_path: &'a Path,
    pub timeout: Duration,
    pub purpose: TurnPurpose,
    /// The visit's stored model choice, already checked by the agent; not
    /// the machine's own default.
    pub model: &'a str,
    /// How long the MCP connection gets before the first input freezes the
    /// child's tool list. `agent.mcp_settle()` in production; zero in tests,
    /// which never launch a real child.
    pub mcp_settle: Duration,
    /// The tokens the visit has left. For an agent that reports usage while
    /// the turn runs (`Agent::live_token_counter`), the turn is killed once it
    /// spends more than this. Usage arrives after a model step, and shutdown
    /// happens on the next process poll, so this is not a provider-side cap. `None`
    /// outside a token-capped visit, and ignored by agents that report usage
    /// only at the end.
    pub token_ceiling: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnPurpose {
    World,
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
    /// The turn succeeded without calling any daycare tool: the agent watched,
    /// waited, or declined, and said so. A held turn is a turn, not a failure.
    pub held: bool,
    /// The turn passed `token_ceiling` (killed if it was still running).
    pub over_token_ceiling: bool,
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

    let agent = request.agent;
    let label = agent.label();

    // Activation checks this too. Recheck immediately before every child
    // launch because a long visit can outlive a changed workspace-root symlink.
    let physical_workspace = request.workspace.guard_ancestors()?;

    let expected_session_id = match &request.mode {
        SessionMode::New {
            reserved_session_id,
        } if agent.reserves_session_ids() => Some(reserved_session_id.clone()),
        SessionMode::New { .. } => None,
        SessionMode::Resume { session_id } => Some(session_id.clone()),
        SessionMode::Fork { .. } => None,
    };
    let spec = TurnSpec {
        mode: &request.mode,
        message: request.message,
        actor_name: request.actor_name,
        workspace: &physical_workspace,
        purpose: request.purpose,
        model: request.model,
        device_token: (request.purpose != TurnPurpose::DayReport).then_some(request.device_token),
    };
    // Managed policy, login, and whatever proof of the seal the agent can
    // give without a model call, before the child receives either the turn
    // prompt or the device token.
    agent.guard(&spec)?;
    // From here an agent may have put something on disk for this turn (the
    // token-bearing MCP file), so every way out, early returns included, ends
    // with `after_turn`.
    let after_turn = AfterTurn::new(agent);
    let plan = agent.launch_plan(&spec)?;
    // The plan preserves the established missing-file errors. This second check
    // adds the no-symlink property before the files reach the agent.
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
    for name in &plan.env_remove {
        command.env_remove(name);
    }
    command.env_remove(DEVICE_TOKEN_ENV);
    for (name, value) in &plan.env {
        command.env(name, value);
    }
    // The only place the device token exists outside the keychain, for agents
    // that read it from here. The homecoming turn needs it too: it saves the
    // visit's memories through the same MCP server the visit used. The day
    // report has no server to reach. An agent that carries the token in its
    // own config file (Antigravity) gets no copy in its environment.
    if request.purpose != TurnPurpose::DayReport && agent.device_token_in_env() {
        command.env(DEVICE_TOKEN_ENV, request.device_token);
    }

    let started = Instant::now();
    let mut child = command.spawn().map_err(|error| {
        Error::new(format!(
            "could not start {}: {error}. Is {} installed and on PATH?",
            plan.program,
            agent.product_name()
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
    // the events it produced on disk. Agents that report usage as they go have
    // it counted here, where the lines are.
    let token_counter = request
        .token_ceiling
        .and_then(|_| agent.live_token_counter());
    let tokens_spent = Arc::new(AtomicU64::new(0));
    let (archive_done, archive_result) = std::sync::mpsc::channel();
    let counted = Arc::clone(&tokens_spent);
    std::thread::spawn(move || {
        let mut archive = archive;
        let mut outcome = Ok(());
        for line in BufReader::new(stdout).split(b'\n') {
            match line {
                Ok(mut line) => {
                    if let Some(count) = token_counter {
                        let spent = count(&String::from_utf8_lossy(&line));
                        counted.fetch_add(spent, Ordering::SeqCst);
                    }
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
    let mut over_token_ceiling = false;
    let status = loop {
        match child.try_wait()? {
            Some(status) => break Some(status),
            None => {
                if started.elapsed() >= request.timeout {
                    timed_out = true;
                    let _ = child.kill();
                    break child.wait().ok();
                }
                if token_counter.is_some()
                    && request
                        .token_ceiling
                        .is_some_and(|ceiling| tokens_spent.load(Ordering::SeqCst) > ceiling)
                {
                    over_token_ceiling = true;
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
    if let Ok(Err(error)) = archive_result.recv_timeout(drain) {
        return Err(Error::new(format!(
            "could not archive turn stream: {error}"
        )));
    }
    let stderr_text = stderr_result.recv_timeout(drain).unwrap_or_default();
    // A short final step can finish and exit between polls. Recheck after
    // draining stdout so an over-budget exit cannot be reported as success.
    over_token_ceiling |= token_counter.is_some()
        && request
            .token_ceiling
            .is_some_and(|ceiling| tokens_spent.load(Ordering::SeqCst) > ceiling);
    // Whatever the turn did, the agent may have state to settle (Codex hands a
    // refreshed login back to the owner's own home).
    let after_turn = after_turn.run();

    let mut failure = None;
    let mut held = false;
    if timed_out {
        failure = Some(format!(
            "turn exceeded {}s and was killed",
            request.timeout.as_secs()
        ));
    }
    if over_token_ceiling {
        failure = Some(format!(
            "turn spent {} tokens, more than the {} left in the visit's token cap",
            tokens_spent.load(Ordering::SeqCst),
            request.token_ceiling.unwrap_or(0)
        ));
    }

    let receipt = match std::fs::read_to_string(request.archive_path)
        .map_err(Error::from)
        .and_then(|text| agent.parse_receipt(&text))
    {
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
            failure = Some(format!(
                "{label} reported an invalid session id; it will not be persisted"
            ));
        } else if expected_session_id
            .as_deref()
            .is_some_and(|expected| receipt.session_id != expected)
        {
            failure = Some(format!(
                "{label} reported a different session id than the runner assigned; it will not be persisted"
            ));
        } else {
            trusted_session_id = Some(receipt.session_id.clone());
        }
        if let Err(error) = agent.verify_seal(receipt, request.purpose, &request.workspace.dir) {
            // A sandbox violation outranks any other outcome: the turn may
            // have had more reach than Daycare allows.
            failure = Some(format!("sandbox check failed: {error}"));
        }
        if failure.is_none() && !receipt.success {
            failure = Some(format!(
                "{label} reported {}{}",
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
            if let Err(error) = agent.verify_capability(receipt, request.purpose) {
                failure = Some(error.to_string());
            }
        }
        if failure.is_none() && request.purpose == TurnPurpose::World {
            match verify_reached_the_world(receipt) {
                Ok(WorldReach::Reached) => {}
                Ok(WorldReach::Held) => held = true,
                Err(error) => failure = Some(error.to_string()),
            }
        }
    }

    if failure.is_none() && exit_code.unwrap_or(0) != 0 {
        failure = Some(format!(
            "{label} exited {}{}",
            exit_code.unwrap_or(-1),
            exit_note(None, &stderr_text)
        ));
    }

    if let Err(error) = after_turn {
        if failure.is_none() {
            failure = Some(error.to_string());
        }
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
        over_token_ceiling,
    })
}

/// Runs `Agent::after_turn` once for the turn, on the normal path through
/// `run` (which hands back its result) or, on any other exit (an early `?`
/// return, a panic), when dropped.
struct AfterTurn<'a> {
    agent: &'a dyn Agent,
    done: bool,
}

impl<'a> AfterTurn<'a> {
    fn new(agent: &'a dyn Agent) -> Self {
        AfterTurn { agent, done: false }
    }

    fn run(mut self) -> Result<()> {
        self.done = true;
        self.agent.after_turn()
    }
}

impl Drop for AfterTurn<'_> {
    fn drop(&mut self) {
        if !self.done {
            // The turn is already failing for another reason; that is the
            // error the caller needs.
            let _ = self.agent.after_turn();
        }
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::{AgentKind, TranscriptDelivery};
    use crate::launch::LaunchPlan;
    use crate::meter::WeeklyMeter;
    use crate::stream::TurnEvent;
    use std::cell::Cell;

    /// Counts `after_turn`; nothing else of it is ever called.
    struct Counting {
        after_turn_calls: Cell<u32>,
        fail: bool,
    }

    impl Agent for Counting {
        fn kind(&self) -> AgentKind {
            AgentKind::Claude
        }
        fn program(&self) -> &str {
            unimplemented!()
        }
        fn product_name(&self) -> &'static str {
            unimplemented!()
        }
        fn default_model(&self) -> Result<String> {
            unimplemented!()
        }
        fn check_model(&self, _model: &str) -> Result<()> {
            unimplemented!()
        }
        fn reserves_session_ids(&self) -> bool {
            unimplemented!()
        }
        fn guard(&self, _spec: &crate::agent::TurnSpec<'_>) -> Result<()> {
            unimplemented!()
        }
        fn launch_plan(&self, _spec: &crate::agent::TurnSpec<'_>) -> Result<LaunchPlan> {
            unimplemented!()
        }
        fn after_turn(&self) -> Result<()> {
            self.after_turn_calls.set(self.after_turn_calls.get() + 1);
            if self.fail {
                Err(Error::new("cleanup failed"))
            } else {
                Ok(())
            }
        }
        fn parse_receipt(&self, _archive: &str) -> Result<StreamReceipt> {
            unimplemented!()
        }
        fn transcript_events(&self, _archive: &str) -> Result<Vec<TurnEvent>> {
            unimplemented!()
        }
        fn verify_seal(&self, _: &StreamReceipt, _: TurnPurpose, _: &Path) -> Result<()> {
            unimplemented!()
        }
        fn verify_capability(&self, _: &StreamReceipt, _: TurnPurpose) -> Result<()> {
            unimplemented!()
        }
        fn verify_archived(&self, _: &StreamReceipt, _: TurnPurpose, _: &Path) -> Result<()> {
            unimplemented!()
        }
        fn transcript_delivery(&self) -> TranscriptDelivery {
            unimplemented!()
        }
        fn meter(&self) -> Option<&dyn WeeklyMeter> {
            unimplemented!()
        }
        fn reopen_command(&self, _: &Path, _: Option<&str>) -> String {
            unimplemented!()
        }
    }

    #[test]
    fn after_turn_runs_once_whether_the_turn_finishes_or_leaves_early() {
        let agent = Counting {
            after_turn_calls: Cell::new(0),
            fail: true,
        };
        // The normal path runs it once and hands back its result.
        let guard = AfterTurn::new(&agent);
        assert!(guard.run().is_err());
        assert_eq!(agent.after_turn_calls.get(), 1);

        // An early return (here, just a drop) runs it once, and says nothing.
        agent.after_turn_calls.set(0);
        drop(AfterTurn::new(&agent));
        assert_eq!(agent.after_turn_calls.get(), 1);

        // A panic unwinds through the guard.
        agent.after_turn_calls.set(0);
        let unwound = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = AfterTurn::new(&agent);
            panic!("mid-turn");
        }));
        assert!(unwound.is_err());
        assert_eq!(agent.after_turn_calls.get(), 1);
    }
}
