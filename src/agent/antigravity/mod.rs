//! Antigravity CLI (`agy`), Google's agent CLI, behind the `Agent` trait.
//!
//! A turn runs as `agy --input-format stream-json --output-format stream-json
//! --agent daycare --disable-slash-commands --model <model> [--conversation
//! ID]`, with the message as one stream-json line on stdin, inside a seal the
//! runner owns:
//!
//! - **Home.** `HOME` is `<seal>/home`, so none of the person's own `~/.gemini`
//!   (their `GEMINI.md`, rules, skills, plugins, MCP servers, hooks, settings)
//!   is read. The only thing linked in is `~/Library/Keychains`, where `agy`
//!   keeps its Google sign-in; without it the sealed home is signed out.
//! - **Settings.** `<home>/.gemini/antigravity-cli/settings.json`: permission
//!   mode `strict`, shell, file and URL actions denied, MCP allowed only on
//!   turns that reach the server (`mcp(*)`: narrower `mcp(server/tool)` rules
//!   did not match in headless mode on 1.5.13), no access outside the
//!   workspace, and `useG1Credits: false` so a visit never spends paid AI
//!   credits after the plan's quota runs out.
//! - **Agent.** `<home>/.gemini/config/agents/daycare.md`: a custom agent whose
//!   `tools` list is only `finish`. On 1.5.13 that leaves `agy` with `finish`,
//!   `manage_task`, `list_resources`, `read_resource` and `call_mcp_tool` —
//!   no shell, files, web, browser, images, scheduling or messaging. Its body
//!   is the persona plus the controller prompt.
//! - **MCP.** `<home>/.gemini/config/mcp_config.json` names one server,
//!   `daycare`, with this turn's tools (`enabledTools`/`disabledTools`).
//!   `agy` does not expand `${VAR}` in MCP headers (checked on 1.5.13: the
//!   server silently failed to load), so unlike the other adapters **the
//!   device token is written into this file** (0600, in a 0700 seal) just
//!   before the child starts, and the file is removed in `after_turn`. A day
//!   report gets no file at all.
//!
//! Proof: the stream's `init` names the agent and permission mode that ran,
//! and every tool step names its tool; anything but a `daycare` MCP call or
//! `finish` is foreign reach (`stream`). `init.tools` is the global tool list,
//! not the agent's, so it proves nothing.
//!
//! Antigravity has no weekly meter a program can read (its quota panel is
//! interactive only), so visits are bounded by a token cap, counted from each
//! finished response step while the turn runs.
//!
//! Google's Antigravity FAQ says wrapping `agy` inside third-party
//! applications "is not supported under consumer plans"; the owner's own
//! Google account carries that risk. See the README.

pub mod stream;

use super::{Agent, AgentKind, TranscriptDelivery, TurnSpec};
use crate::launch::{
    is_homecoming_tool, LaunchPlan, SessionMode, DEVICE_TOKEN_ENV, MCP_SERVER, MCP_TOOL_PREFIX,
};
use crate::meter::WeeklyMeter;
use crate::paths::{
    create_private_dir, sanitize_segment, shell_quote, shell_quote_path, write_atomic, Layout,
};
use crate::stream::{StreamReceipt, TurnEvent};
use crate::turn::TurnPurpose;
use crate::workspace::{Workspace, CLAUDE_MD, CONTROLLER_PROMPT, MCP_CONFIG};
use crate::{Error, Result};
use serde_json::{json, Value};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

pub use stream::AGENT_NAME;

/// The oldest `agy` the seal was checked against.
pub const MIN_VERSION: (u64, u64, u64) = (1, 5, 13);

/// The only operating system Antigravity visits run on: the seal links the
/// macOS login keychain, where `agy` keeps its Google sign-in.
pub const SUPPORTED_OS: &str = "macos";

/// The first major `agy` release the seal was not checked against.
pub const UNSUPPORTED_MAJOR: u64 = 2;

/// The memory tools a homecoming may see.
const HOMECOMING_TOOLS: [&str; 2] = ["daycare_memory_list", "daycare_memory_save"];

/// Denied on visit turns; the homecoming is where memory is written.
const MEMORY_SAVE_TOOL: &str = "daycare_memory_save";

/// Every action the permission engine knows besides MCP.
const DENIED_ACTIONS: [&str; 6] = [
    "command(*)",
    "read_file(*)",
    "write_file(*)",
    "read_url(*)",
    "execute_url(*)",
    "download_url(*)",
];

/// Parent variables a turn keeps. Everything else is removed before launch.
const KEPT_ENV: [&str; 8] = [
    "PATH", "USER", "LOGNAME", "LANG", "LC_ALL", "LC_CTYPE", "TMPDIR", "TZ",
];

/// How long `agy --version` or `agy models` may take.
const INSPECT_TIMEOUT: Duration = Duration::from_secs(60);

/// How long `agy` may take to connect to the daycare server and list its
/// tools, and to answer one tool call.
const MCP_TIMEOUT_SECONDS: u64 = 120;

pub struct AntigravityAgent {
    bin: String,
    /// `std::env::consts::OS`, unless a test says otherwise.
    os: String,
    /// `<daycare root>/antigravity`: one sealed home per identity.
    root: PathBuf,
    version_checked: OnceLock<()>,
    login_checked: OnceLock<()>,
    /// The MCP config holding this turn's token, removed in `after_turn`.
    token_file: Mutex<Option<PathBuf>>,
}

impl AntigravityAgent {
    pub fn new(bin: &str, layout: &Layout) -> Self {
        AntigravityAgent {
            bin: bin.to_string(),
            os: std::env::consts::OS.to_string(),
            root: layout.root().join("antigravity"),
            version_checked: OnceLock::new(),
            login_checked: OnceLock::new(),
            token_file: Mutex::new(None),
        }
    }

    /// Run as if on operating system `os` (`std::env::consts::OS` names), so a
    /// test can see the refusal on a machine where Antigravity does run.
    pub fn on_platform(mut self, os: &str) -> Self {
        self.os = os.to_string();
        self
    }

    /// One sealed home per identity, keyed like its workspace directory.
    fn seal(&self, workspace: &Path) -> Seal {
        let key = sanitize_segment(
            &workspace
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default(),
        );
        Seal::at(self.root.join("seal").join(key))
    }

    /// The release on PATH (or `--antigravity-bin`) is one the seal was
    /// checked against. Asked once per process.
    fn ensure_version(&self) -> Result<()> {
        check_platform(&self.os)?;
        if self.version_checked.get().is_some() {
            return Ok(());
        }
        let mut command = Command::new(&self.bin);
        command.arg("--version");
        let output = run_bounded(command, &bare_env(None)?, INSPECT_TIMEOUT).map_err(|error| {
            Error::new(format!(
                "could not run {} --version: {error}. Is Antigravity CLI installed and on PATH?",
                self.bin
            ))
        })?;
        check_version_output(if output.success {
            output.stdout.as_str()
        } else {
            ""
        })?;
        let _ = self.version_checked.set(());
        Ok(())
    }

    /// The sealed home can reach the person's Google sign-in: `agy models`
    /// lists models without a model call. Asked once per process.
    fn ensure_login(&self, seal: &Seal) -> Result<()> {
        if self.login_checked.get().is_some() {
            return Ok(());
        }
        let mut command = Command::new(&self.bin);
        command.arg("models").current_dir(&seal.dir);
        let output = run_bounded(command, &bare_env(Some(&seal.home))?, INSPECT_TIMEOUT)
            .map_err(|error| Error::new(format!("could not run {} models: {error}", self.bin)))?;
        if !output.success || model_ids(&output.stdout).is_empty() {
            return Err(Error::new(
                "Antigravity CLI is not signed in. Run `agy` once and sign in with the Google \
                 account whose plan Daycare should use, then retry",
            ));
        }
        let _ = self.login_checked.set(());
        Ok(())
    }

    /// Write the seal for this turn: home, keychain link, settings and the
    /// custom agent. The MCP config, which holds the token, is written by
    /// `launch_plan`.
    fn prepare(&self, spec: &TurnSpec<'_>) -> Result<Seal> {
        self.check_model(spec.model)?;
        let workspace = Workspace::new(spec.workspace);
        workspace.guard_scaffold_files()?;
        refuse_workspace_customizations(spec.workspace)?;
        let persona = std::fs::read_to_string(spec.workspace.join(CLAUDE_MD))?;
        let controller = std::fs::read_to_string(spec.workspace.join(CONTROLLER_PROMPT))?;
        let seal = self.seal(spec.workspace);
        seal.create()?;
        write_atomic(
            &seal.settings(),
            format!(
                "{}\n",
                serde_json::to_string_pretty(&sealed_settings(spec.purpose, spec.workspace))?
            )
            .as_bytes(),
            0o600,
        )?;
        write_atomic(
            &seal.agent_file(),
            agent_file(&persona, &turn_controller(spec.purpose, &controller)).as_bytes(),
            0o600,
        )?;
        // No MCP server until `launch_plan` writes this turn's.
        remove_if_present(&seal.mcp_config())?;
        Ok(seal)
    }

    fn forget_token_file(&self) -> Result<()> {
        let path = self
            .token_file
            .lock()
            .map_err(|_| Error::new("antigravity token-file lock poisoned"))?
            .take();
        if let Some(path) = path {
            remove_if_present(&path)?;
        }
        Ok(())
    }
}

impl Agent for AntigravityAgent {
    fn kind(&self) -> AgentKind {
        AgentKind::Antigravity
    }

    fn program(&self) -> &str {
        &self.bin
    }

    fn product_name(&self) -> &'static str {
        "Antigravity CLI"
    }

    /// The first Gemini Flash model the account offers, else its first model.
    fn default_model(&self) -> Result<String> {
        self.ensure_version()?;
        let probe = Seal::at(self.root.join("probe"));
        probe.create()?;
        let mut command = Command::new(&self.bin);
        command.arg("models").current_dir(&probe.dir);
        let output = run_bounded(command, &bare_env(Some(&probe.home))?, INSPECT_TIMEOUT)
            .map_err(|error| Error::new(format!("could not run {} models: {error}", self.bin)))?;
        let ids = model_ids(&output.stdout);
        ids.iter()
            .find(|id| id.starts_with("gemini-") && id.contains("flash"))
            .or_else(|| ids.first())
            .cloned()
            .ok_or_else(|| {
                Error::new(
                    "Antigravity CLI listed no models; run `agy` once and sign in, then retry",
                )
            })
    }

    /// Also refuses an `agy` release the seal was not checked against, so
    /// `visit start` stops before a visit is opened on the platform.
    fn check_model(&self, model: &str) -> Result<()> {
        check_model(model)?;
        self.ensure_version()
    }

    fn reserves_session_ids(&self) -> bool {
        false
    }

    fn guard(&self, spec: &TurnSpec<'_>) -> Result<()> {
        self.ensure_version()?;
        if matches!(spec.mode, SessionMode::Fork { .. }) {
            return Err(Error::new(
                "Antigravity CLI cannot fork a conversation; Daycare runs it only as new or resumed",
            ));
        }
        let seal = self.prepare(spec)?;
        self.ensure_login(&seal)
    }

    fn launch_plan(&self, spec: &TurnSpec<'_>) -> Result<LaunchPlan> {
        if spec.message.trim().is_empty() {
            return Err(Error::new("turn message must not be empty"));
        }
        let seal = self.prepare(spec)?;
        if spec.purpose != TurnPurpose::DayReport {
            let token = spec
                .device_token
                .filter(|token| !token.trim().is_empty())
                .ok_or_else(|| Error::new("no device token for the daycare MCP server"))?;
            let url = read_mcp_url(&spec.workspace.join(MCP_CONFIG))?;
            write_atomic(
                &seal.mcp_config(),
                format!(
                    "{}\n",
                    serde_json::to_string_pretty(&mcp_config(spec.purpose, &url, token))?
                )
                .as_bytes(),
                0o600,
            )?;
            *self
                .token_file
                .lock()
                .map_err(|_| Error::new("antigravity token-file lock poisoned"))? =
                Some(seal.mcp_config());
        }
        let mut args: Vec<String> = vec![
            "--input-format".into(),
            "stream-json".into(),
            "--output-format".into(),
            "stream-json".into(),
            "--agent".into(),
            AGENT_NAME.into(),
            "--disable-slash-commands".into(),
            "--model".into(),
            spec.model.into(),
        ];
        match spec.mode {
            SessionMode::New { .. } => {}
            SessionMode::Resume { session_id } => {
                args.extend(["--conversation".into(), session_id.clone()]);
            }
            SessionMode::Fork { .. } => {
                return Err(Error::new("Antigravity CLI cannot fork a conversation"));
            }
        }
        let env = child_env(&seal.home)?;
        Ok(LaunchPlan {
            program: self.bin.clone(),
            cwd: spec.workspace.to_path_buf(),
            args,
            // One user message; stdin then closes, so `agy` runs one turn and
            // exits. The message never shows in `ps`.
            stdin: format!(
                "{}\n",
                json!({"event": "user", "message": {"content": spec.message}})
            ),
            env_remove: env.remove,
            env: env.set,
        })
    }

    /// The token is in the 0600 MCP config, not in the child's environment.
    fn device_token_in_env(&self) -> bool {
        false
    }

    /// The token-bearing MCP config never outlives the child.
    fn after_turn(&self) -> Result<()> {
        self.forget_token_file()
    }

    fn live_token_counter(&self) -> Option<fn(&str) -> u64> {
        Some(stream::tokens_in_line)
    }

    fn parse_receipt(&self, archive: &str) -> Result<StreamReceipt> {
        stream::parse_stream(archive)
    }

    fn transcript_events(&self, archive: &str) -> Result<Vec<TurnEvent>> {
        stream::transcript_events(archive)
    }

    fn verify_seal(
        &self,
        receipt: &StreamReceipt,
        _purpose: TurnPurpose,
        _workspace: &Path,
    ) -> Result<()> {
        if let Some(reach) = receipt.foreign_reach.first() {
            return Err(Error::new(format!("agy reached outside Daycare: {reach}")));
        }
        Ok(())
    }

    fn verify_capability(&self, receipt: &StreamReceipt, purpose: TurnPurpose) -> Result<()> {
        verify_calls(receipt, purpose, "")
    }

    fn verify_archived(
        &self,
        receipt: &StreamReceipt,
        purpose: TurnPurpose,
        workspace: &Path,
    ) -> Result<()> {
        self.verify_seal(receipt, purpose, workspace)?;
        verify_calls(receipt, purpose, " receipt")
    }

    fn transcript_delivery(&self) -> TranscriptDelivery {
        // The agent has no file tool at all; the transcript rides in the
        // prompt.
        TranscriptDelivery::Inline
    }

    fn meter(&self) -> Option<&dyn WeeklyMeter> {
        None
    }

    fn reopen_command(&self, workspace: &Path, session_id: Option<&str>) -> String {
        let seal = self.seal(workspace);
        let mut command = format!(
            "cd {} && env -i PATH=\"$PATH\" HOME={} AGY_CLI_DISABLE_AUTO_UPDATE=true {} --agent {AGENT_NAME}",
            shell_quote_path(workspace),
            shell_quote_path(&seal.home),
            shell_quote(&self.bin),
        );
        match session_id {
            Some(id) => command.push_str(&format!(
                " --conversation {}   # without the daycare server: its token is not kept on disk",
                shell_quote(id)
            )),
            None => command.push_str("   # no daycare conversation yet; run a turn first"),
        }
        command
    }
}

/// The folders one identity's turns run inside.
struct Seal {
    dir: PathBuf,
    home: PathBuf,
}

impl Seal {
    fn at(dir: PathBuf) -> Self {
        Seal {
            home: dir.join("home"),
            dir,
        }
    }

    fn gemini(&self) -> PathBuf {
        self.home.join(".gemini")
    }

    fn settings(&self) -> PathBuf {
        self.gemini().join("antigravity-cli").join("settings.json")
    }

    fn agent_file(&self) -> PathBuf {
        self.gemini()
            .join("config")
            .join("agents")
            .join(format!("{AGENT_NAME}.md"))
    }

    fn mcp_config(&self) -> PathBuf {
        self.gemini().join("config").join("mcp_config.json")
    }

    /// Create the sealed home and link in only the login keychain.
    fn create(&self) -> Result<()> {
        for dir in [
            self.dir.parent().unwrap_or(&self.dir).to_path_buf(),
            self.dir.clone(),
            self.home.clone(),
            self.home.join("Library"),
            self.gemini(),
            self.gemini().join("antigravity-cli"),
            self.gemini().join("config"),
            self.gemini().join("config").join("agents"),
        ] {
            create_private_dir(&dir)?;
        }
        let link = self.home.join("Library").join("Keychains");
        let real_home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .ok_or_else(|| Error::new("HOME is not set; cannot find the login keychain"))?;
        let target = real_home.join("Library").join("Keychains");
        match std::fs::read_link(&link) {
            Ok(existing) if existing == target => {}
            Ok(_) => {
                std::fs::remove_file(&link)?;
                link_keychains(&target, &link)?;
            }
            Err(_) if link.symlink_metadata().is_ok() => {
                return Err(Error::new(format!(
                    "{} is not the keychain link Daycare made; refusing to replace it",
                    link.display()
                )))
            }
            Err(_) => link_keychains(&target, &link)?,
        }
        Ok(())
    }
}

#[cfg(unix)]
fn link_keychains(target: &Path, link: &Path) -> Result<()> {
    std::os::unix::fs::symlink(target, link)?;
    Ok(())
}

#[cfg(not(unix))]
fn link_keychains(_target: &Path, _link: &Path) -> Result<()> {
    Err(Error::new("Antigravity CLI visits run on macOS"))
}

/// `agy`'s settings for one turn.
pub fn sealed_settings(purpose: TurnPurpose, workspace: &Path) -> Value {
    let mut deny: Vec<&str> = DENIED_ACTIONS.to_vec();
    let allow: Vec<&str> = match purpose {
        TurnPurpose::DayReport => {
            deny.push("mcp(*)");
            Vec::new()
        }
        _ => vec!["mcp(*)"],
    };
    json!({
        "toolPermission": "strict",
        "allowNonWorkspaceAccess": false,
        "enableTerminalSandbox": true,
        "useG1Credits": false,
        "disableSlashCommands": true,
        "trustedWorkspaces": [workspace.to_string_lossy()],
        "permissions": {"allow": allow, "ask": [], "deny": deny},
    })
}

/// Added to the controller prompt on visit turns only. `agy` can drop the
/// daycare server without failing the turn, and its stream doesn't list the
/// agent's tools, so a daycare call is the only proof that a visit turn could
/// reach the daycare (`verify_calls` keeps requiring one). On 2026-11-22 a
/// production turn chose to rest, called nothing, and failed that check; this
/// asks a resting turn to look first instead.
pub const VISIT_TURN_NOTE: &str = "On Antigravity CLI, every visit turn makes at least one daycare tool call, even a turn where you choose to wait, watch or rest: if nothing else, call daycare_world_snapshot first. A turn with no daycare call can't show the daycare that you were there, so it counts as a failed turn, not a rest.";

/// The controller prompt for this turn's purpose. The homecoming and the day
/// report keep theirs as written: they may call only memory tools, or nothing.
pub fn turn_controller(purpose: TurnPurpose, controller: &str) -> String {
    match purpose {
        TurnPurpose::World => format!("{}\n\n{VISIT_TURN_NOTE}", controller.trim()),
        TurnPurpose::PrivateHomecoming | TurnPurpose::DayReport => controller.to_string(),
    }
}

/// The custom agent: only `finish` among the built-in tools, and the persona
/// and controller prompt as its system prompt.
pub fn agent_file(persona: &str, controller: &str) -> String {
    format!(
        "---\nname: {AGENT_NAME}\ndescription: One Daycare turn.\ntools:\n  - finish\nmainAgent: true\nsubagent: false\n---\n\n{}\n\n{}\n",
        persona.trim(),
        controller.trim()
    )
}

/// The one MCP server a turn may reach, with this turn's tools and token.
pub fn mcp_config(purpose: TurnPurpose, url: &str, token: &str) -> Value {
    let mut server = json!({
        "serverUrl": url,
        "headers": {"Authorization": format!("Bearer {token}")},
        "timeoutSeconds": MCP_TIMEOUT_SECONDS,
    });
    match purpose {
        TurnPurpose::World => server["disabledTools"] = json!([MEMORY_SAVE_TOOL]),
        TurnPurpose::PrivateHomecoming => server["enabledTools"] = json!(HOMECOMING_TOOLS),
        TurnPurpose::DayReport => {}
    }
    json!({"mcpServers": {MCP_SERVER: server}})
}

/// `agy` reads `AGENTS.md`, `GEMINI.md` and `.agents/` from the workspace and
/// its parents. The runner's workspace has none; refuse one that does.
fn refuse_workspace_customizations(workspace: &Path) -> Result<()> {
    for name in ["AGENTS.md", "GEMINI.md", ".agents", ".gemini"] {
        let path = workspace.join(name);
        if path.symlink_metadata().is_ok() {
            return Err(Error::new(format!(
                "{} would add instructions or tools to the Antigravity turn; Daycare's workspace \
                 must not contain it",
                path.display()
            )));
        }
    }
    Ok(())
}

/// The daycare server URL from the workspace's MCP config.
fn read_mcp_url(path: &Path) -> Result<String> {
    let parsed: Value = serde_json::from_slice(&std::fs::read(path)?)
        .map_err(|_| Error::new("the workspace MCP config is not JSON"))?;
    let url = parsed["mcpServers"][MCP_SERVER]["url"]
        .as_str()
        .filter(|url| url.starts_with("https://") || url.starts_with("http://"))
        .ok_or_else(|| Error::new("MCP config has no daycare server URL"))?;
    // The turn sends the identity's bearer token to this URL.
    crate::self_update::require_https_or_loopback(url, "the daycare server URL in the MCP config")?;
    Ok(url.to_string())
}

/// Refuse an operating system the seal does not work on, before `agy` is run
/// at all. `os` is a `std::env::consts::OS` name. Without it a Linux or WSL
/// owner would be told they are not signed in: the sealed home links only
/// `~/Library/Keychains` and drops the DBus and XDG variables a Linux `agy`
/// needs for its sign-in, so the login check could never pass.
pub fn check_platform(os: &str) -> Result<()> {
    if os == SUPPORTED_OS {
        return Ok(());
    }
    Err(Error::new(format!(
        "Antigravity visits are macOS-only for now (this machine is {os}): the sealed home reaches \
         the Google sign-in through the macOS login keychain. Use Claude Code, Codex or OpenCode here"
    )))
}

/// A model id `agy --model` accepts: letters, digits, `-`, `_` and `.`.
pub fn check_model(model: &str) -> Result<()> {
    let ok = !model.is_empty()
        && model.len() <= 100
        && model
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"-_.".contains(&c))
        && !model.starts_with('-');
    if ok {
        Ok(())
    } else {
        Err(Error::new(format!(
            "Antigravity CLI visits need --model with a model id from `agy models` (for example gemini-3.8-flash-medium); got {model:?}"
        )))
    }
}

/// The ids in `agy models` output: the first word of each line that names one.
pub fn model_ids(text: &str) -> Vec<String> {
    text.lines()
        .filter_map(|line| line.split_whitespace().next())
        .filter(|word| check_model(word).is_ok() && word.contains('-'))
        .map(str::to_string)
        .collect()
}

/// `1.5.13`, `agy 1.5.13` or `v1.5.13` as a version; anything else is `None`.
fn parse_version(text: &str) -> Option<(u64, u64, u64)> {
    let token = text.split_whitespace().find_map(|word| {
        let word = word.strip_prefix(['v', 'V']).unwrap_or(word);
        word.starts_with(|c: char| c.is_ascii_digit())
            .then_some(word)
    })?;
    let core = token.split(['-', '+', ',']).next()?;
    let mut parts = core.split('.').map(|part| part.parse::<u64>().ok());
    Some((
        parts.next()??,
        parts.next()??,
        parts.next().flatten().unwrap_or(0),
    ))
}

/// Refuse anything but the release the seal was checked against or a later
/// 1.x one. The seal rests on `agy`'s settings, agent and MCP file formats and
/// its stream shape, none of which is documented as stable, so a new major
/// release is refused until it has been checked.
pub fn check_version_output(text: &str) -> Result<()> {
    let (major, minor, patch) = MIN_VERSION;
    match parse_version(text) {
        Some((a, b, c)) if a >= UNSUPPORTED_MAJOR => Err(Error::new(format!(
            "Daycare runs Antigravity CLI {major}.{minor}.{patch} or a later {major}.x release, and this is {a}.{b}.{c}. \
             Antigravity CLI {UNSUPPORTED_MAJOR} may have changed the settings Daycare uses to keep a visit sealed, so it \
             is not supported yet. Update the runner (`daycare-runner update`) in case a newer one supports it, or wait \
             for a release that does; until then use Claude Code, Codex or OpenCode"
        ))),
        Some(version) if version >= MIN_VERSION => Ok(()),
        Some((a, b, c)) => Err(Error::new(format!(
            "Daycare needs Antigravity CLI {major}.{minor}.{patch} or newer, and this is {a}.{b}.{c}. Update it with `brew upgrade --cask antigravity-cli` or `agy update`"
        ))),
        None => Err(Error::new(format!(
            "could not read Antigravity CLI's version; Daycare needs {major}.{minor}.{patch} or newer"
        ))),
    }
}

fn verify_calls(receipt: &StreamReceipt, purpose: TurnPurpose, noun: &str) -> Result<()> {
    match purpose {
        TurnPurpose::World => {
            // `agy` drops an MCP server it cannot reach without failing the
            // turn, and its init event does not list the agent's tools.
            // Require an actual daycare call.
            if !receipt
                .permitted_tool_calls
                .iter()
                .any(|name| name.starts_with(MCP_TOOL_PREFIX))
            {
                return Err(Error::new(format!(
                    "Antigravity visit turn{noun} called no daycare tool"
                )));
            }
            if let Some(name) = receipt
                .tool_calls
                .iter()
                .find(|name| name.ends_with(MEMORY_SAVE_TOOL))
            {
                return Err(Error::new(format!(
                    "visit turn{noun} invoked {name}; memory is written at homecoming"
                )));
            }
        }
        TurnPurpose::PrivateHomecoming => {
            if let Some(name) = receipt
                .permitted_tool_calls
                .iter()
                .find(|name| !is_homecoming_tool(name))
            {
                return Err(Error::new(format!(
                    "private homecoming{noun} invoked {name}; only memory tools may be called after a visit"
                )));
            }
        }
        TurnPurpose::DayReport => {
            if !receipt.tool_calls.is_empty() {
                return Err(Error::new(format!(
                    "day report{noun} invoked a tool instead of remaining local"
                )));
            }
        }
    }
    Ok(())
}

/// The child's environment as removals from the parent's plus settings.
struct ChildEnv {
    remove: Vec<String>,
    set: Vec<(String, String)>,
}

fn child_env(home: &Path) -> Result<ChildEnv> {
    let parent: Vec<(String, String)> = std::env::vars().collect();
    let mut set: Vec<(String, String)> = parent
        .iter()
        .filter(|(name, _)| KEPT_ENV.contains(&name.as_str()))
        .cloned()
        .collect();
    set.push(("HOME".into(), home.to_string_lossy().into_owned()));
    set.push(("AGY_CLI_DISABLE_AUTO_UPDATE".into(), "true".into()));
    let remove = parent
        .into_iter()
        .map(|(name, _)| name)
        .filter(|name| !set.iter().any(|(kept, _)| kept == name))
        .collect();
    Ok(ChildEnv { remove, set })
}

/// For `--version` and `models`: the kept variables, an optional sealed home,
/// and never the device token.
fn bare_env(home: Option<&Path>) -> Result<ChildEnv> {
    let mut env = match home {
        Some(home) => child_env(home)?,
        None => {
            let parent: Vec<(String, String)> = std::env::vars().collect();
            let set: Vec<(String, String)> = parent
                .iter()
                .filter(|(name, _)| KEPT_ENV.contains(&name.as_str()) || name == "HOME")
                .cloned()
                .collect();
            let remove = parent
                .into_iter()
                .map(|(name, _)| name)
                .filter(|name| !set.iter().any(|(kept, _)| kept == name))
                .collect();
            ChildEnv { remove, set }
        }
    };
    env.remove.push(DEVICE_TOKEN_ENV.into());
    Ok(env)
}

fn remove_if_present(path: &Path) -> Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

struct BoundedOutput {
    success: bool,
    stdout: String,
}

/// Run to completion or kill at the deadline, draining stdout on a thread.
fn run_bounded(
    mut command: Command,
    env: &ChildEnv,
    timeout: Duration,
) -> std::io::Result<BoundedOutput> {
    for name in &env.remove {
        command.env_remove(name);
    }
    for (name, value) in &env.set {
        command.env(name, value);
    }
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = command.spawn()?;
    let pipe = child.stdout.take();
    let reader = std::thread::spawn(move || {
        let mut text = String::new();
        if let Some(mut pipe) = pipe {
            let _ = (&mut pipe).take(1024 * 1024).read_to_string(&mut text);
        }
        text
    });
    let started = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if started.elapsed() >= timeout {
            let _ = child.kill();
            let _ = child.wait();
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                format!("no answer in {}s", timeout.as_secs()),
            ));
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    Ok(BoundedOutput {
        success: status.success(),
        stdout: reader.join().unwrap_or_default(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The bearer token goes to this URL, so it must be HTTPS (or loopback).
    #[test]
    fn the_mcp_url_must_be_https_or_loopback() {
        let dir = crate::testdir::unique_dir("daycare-agy-mcp-url");
        let path = dir.join("mcp.json");
        let read = |url: &str| {
            std::fs::write(
                &path,
                json!({"mcpServers": {MCP_SERVER: {"url": url}}}).to_string(),
            )
            .unwrap();
            read_mcp_url(&path)
        };
        for ok in [
            "https://daycare.test/api/daycare/mcp",
            "http://127.0.0.1:3000/mcp",
            "http://localhost:3000/mcp",
            "http://[::1]:3000/mcp",
        ] {
            assert_eq!(read(ok).unwrap(), ok);
        }
        for bad in [
            "http://daycare.test/api/daycare/mcp",
            "http://localhost.evil.test/mcp",
            "http://user@localhost/mcp",
        ] {
            let error = read(bad).unwrap_err().to_string();
            assert!(error.contains("must start with https://"), "{bad}: {error}");
        }
        assert!(read("ftp://daycare.test/mcp").is_err());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn versions_and_models_are_checked() {
        check_version_output("1.5.13\n").unwrap();
        check_version_output("1.6.0").unwrap();
        assert!(check_version_output("1.5.12")
            .unwrap_err()
            .to_string()
            .contains("1.5.13 or newer"));
        assert!(check_version_output("nothing").is_err());
        check_version_output("agy 1.99.0").unwrap();
        check_model("gemini-3.8-flash-medium").unwrap();
        for bad in ["", "-x", "gemini 3", "a/b", "x;y"] {
            assert!(check_model(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn only_macos_runs_antigravity() {
        check_platform("macos").unwrap();
        assert_eq!(
            check_platform(std::env::consts::OS).is_ok(),
            cfg!(target_os = "macos")
        );
        for os in ["linux", "windows", "freebsd", ""] {
            let error = check_platform(os).unwrap_err().to_string();
            assert!(
                error.contains("Antigravity visits are macOS-only for now"),
                "{os}: {error}"
            );
            assert!(!error.contains("not signed in"), "{os}: {error}");
        }
    }

    #[test]
    fn a_new_major_release_is_refused_until_it_is_checked() {
        for two in ["2.0.0", "agy v2.0.1\n", "3.1.0", "2.0.0-beta.1", "10.0"] {
            let error = check_version_output(two).unwrap_err().to_string();
            assert!(error.contains("is not supported yet"), "{two}: {error}");
            assert!(
                error.contains("1.5.13 or a later 1.x release"),
                "{two}: {error}"
            );
            assert!(error.contains("Update the runner"), "{two}: {error}");
            assert!(error.contains("or wait"), "{two}: {error}");
        }
        assert!(check_version_output("2.0.0")
            .unwrap_err()
            .to_string()
            .contains("this is 2.0.0"));
        // The floor still reads as a floor.
        assert!(check_version_output("1.5.12")
            .unwrap_err()
            .to_string()
            .contains("1.5.13 or newer"));
    }

    #[test]
    fn model_ids_are_the_first_word_of_each_model_line() {
        let listing = "Fetching available models...\ngemini-3.8-flash-high\tGemini 3.8 Flash (High)\n\
                       gemini-3.8-flash-medium   Gemini 3.8 Flash (Medium)\nclaude-sonnet-4-6  Claude\n";
        assert_eq!(
            model_ids(listing),
            [
                "gemini-3.8-flash-high",
                "gemini-3.8-flash-medium",
                "claude-sonnet-4-6"
            ]
        );
        assert!(model_ids("Error: Please sign in to view available models.").is_empty());
    }

    #[test]
    fn settings_deny_everything_but_mcp_and_pin_credits_off() {
        let world = sealed_settings(TurnPurpose::World, Path::new("/ws"));
        assert_eq!(world["toolPermission"], "strict");
        assert_eq!(world["useG1Credits"], false);
        assert_eq!(world["allowNonWorkspaceAccess"], false);
        assert_eq!(world["permissions"]["allow"], json!(["mcp(*)"]));
        let deny = world["permissions"]["deny"].as_array().unwrap();
        for action in DENIED_ACTIONS {
            assert!(deny.contains(&json!(action)), "{action}");
        }
        assert_eq!(world["trustedWorkspaces"], json!(["/ws"]));
        let report = sealed_settings(TurnPurpose::DayReport, Path::new("/ws"));
        assert_eq!(report["permissions"]["allow"], json!([]));
        assert!(report["permissions"]["deny"]
            .as_array()
            .unwrap()
            .contains(&json!("mcp(*)")));
    }

    #[test]
    fn each_purpose_sees_only_its_daycare_tools() {
        let world = mcp_config(TurnPurpose::World, "https://x.test/mcp", "dck_t");
        let server = &world["mcpServers"]["daycare"];
        assert_eq!(server["serverUrl"], "https://x.test/mcp");
        assert_eq!(server["headers"]["Authorization"], "Bearer dck_t");
        assert_eq!(server["disabledTools"], json!(["daycare_memory_save"]));
        assert!(server.get("enabledTools").is_none());
        let home = mcp_config(
            TurnPurpose::PrivateHomecoming,
            "https://x.test/mcp",
            "dck_t",
        );
        assert_eq!(
            home["mcpServers"]["daycare"]["enabledTools"],
            json!(HOMECOMING_TOOLS)
        );
    }

    #[test]
    fn the_agent_allows_only_finish_and_carries_the_prompt() {
        let text = agent_file("You are Pip.\n", "Play nicely.");
        assert!(text.starts_with("---\nname: daycare\n"));
        assert!(text.contains("tools:\n  - finish\nmainAgent: true\n"));
        assert!(text.contains("\n---\n\nYou are Pip.\n\nPlay nicely.\n"));
    }

    #[test]
    fn only_a_visit_turn_is_asked_to_make_a_daycare_call_even_when_it_rests() {
        let controller = crate::workspace::controller_prompt("Pip");
        let visit = turn_controller(TurnPurpose::World, &controller);
        assert!(visit.starts_with(controller.trim()));
        assert!(visit.ends_with(VISIT_TURN_NOTE));
        assert!(visit.contains("call daycare_world_snapshot first"));
        // The homecoming may call only memory tools and the day report
        // nothing, so neither is told to call a daycare tool.
        for purpose in [TurnPurpose::PrivateHomecoming, TurnPurpose::DayReport] {
            let text = turn_controller(purpose, &controller);
            assert_eq!(text, controller);
            assert!(!text.contains("daycare_world_snapshot"));
        }
    }

    #[test]
    fn a_workspace_with_its_own_instructions_is_refused() {
        let dir = std::env::temp_dir().join(format!("daycare-agy-ws-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        refuse_workspace_customizations(&dir).unwrap();
        std::fs::write(dir.join("AGENTS.md"), "be someone else").unwrap();
        assert!(refuse_workspace_customizations(&dir)
            .unwrap_err()
            .to_string()
            .contains("AGENTS.md"));
        std::fs::remove_dir_all(&dir).ok();
    }

    fn receipt(calls: &[&str], permitted: &[&str]) -> StreamReceipt {
        let mut receipt = stream::parse_stream(
            &std::fs::read_to_string(
                PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                    .join("tests/fixtures/antigravity-1.5.13/world.jsonl"),
            )
            .unwrap(),
        )
        .unwrap();
        receipt.tool_calls = calls.iter().map(|name| name.to_string()).collect();
        receipt.permitted_tool_calls = permitted.iter().map(|name| name.to_string()).collect();
        receipt
    }

    #[test]
    fn each_purpose_calls_only_what_it_may() {
        let look = "mcp__daycare__daycare_world_snapshot";
        let save = "mcp__daycare__daycare_memory_save";
        verify_calls(&receipt(&[look], &[look]), TurnPurpose::World, "").unwrap();
        assert!(verify_calls(&receipt(&[], &[]), TurnPurpose::World, "")
            .unwrap_err()
            .to_string()
            .contains("called no daycare tool"));
        assert!(verify_calls(&receipt(&[look], &[]), TurnPurpose::World, "").is_err());
        assert!(verify_calls(
            &receipt(&[look, save], &[look, save]),
            TurnPurpose::World,
            ""
        )
        .is_err());
        verify_calls(
            &receipt(&[save], &[save]),
            TurnPurpose::PrivateHomecoming,
            "",
        )
        .unwrap();
        assert!(verify_calls(
            &receipt(&[look], &[look]),
            TurnPurpose::PrivateHomecoming,
            ""
        )
        .is_err());
        verify_calls(&receipt(&[], &[]), TurnPurpose::DayReport, "").unwrap();
        assert!(verify_calls(&receipt(&[save], &[]), TurnPurpose::DayReport, "").is_err());
    }
}
