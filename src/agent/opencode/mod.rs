//! OpenCode (`opencode run --format json`), behind the `Agent` trait.
//!
//! A turn runs as `opencode run --pure --format json --agent daycare -m
//! <provider/model> --title "Daycare visit" [--session ID [--fork]]` with the
//! message on stdin, inside a seal the runner owns:
//!
//! - **Config.** `<seal>/daycare.json`, passed by `OPENCODE_CONFIG`: every
//!   permission denied except this turn's daycare tools, one primary agent
//!   `daycare` whose prompt is the persona plus the controller prompt (it
//!   replaces OpenCode's own system prompt), and the daycare server as a
//!   remote MCP server whose header reads `Bearer {env:DAYCARE_DEVICE_TOKEN}`,
//!   so the token never touches disk.
//! - **Environment.** Emptied, then rebuilt: `HOME` and `XDG_CONFIG_HOME` point
//!   into the seal (OpenCode always loads `~/.opencode/opencode.json` and the
//!   global `AGENTS.md`), `XDG_DATA_HOME` stays real so the person's provider
//!   logins in `auth.json` still work, and the `OPENCODE_DISABLE_*` switches
//!   turn off project config, Claude Code compatibility (`~/.claude`),
//!   `~/.agents/skills`, auto-update, sharing, and LSP downloads. Only the
//!   chosen provider's API-key variable is passed through.
//! - **Sessions.** `OPENCODE_DB=<root>/opencode/<identity>.db`, so visit
//!   sessions never mix with the person's own OpenCode history.
//! - **Never** `--auto` or `--dangerously-skip-permissions`: without them
//!   `run` refuses anything that would ask.
//!
//! Proof, before the child gets the prompt or the token: `opencode debug
//! config` resolves exactly the daycare MCP server and no plugins or extra
//! instructions; `opencode debug agent daycare` is our agent, with our prompt
//! and every built-in tool off; `opencode mcp list` shows the daycare server
//! connected (OpenCode drops an unreachable MCP server silently, and a turn
//! with no tools would otherwise pass as a held turn). After the turn: every
//! `tool_use` named a daycare tool, and `opencode export` shows every message
//! of the session was answered by the `daycare` agent — `run` warns and falls
//! back to the built-in `build` agent when `--agent` is not found.
//!
//! OpenCode has no subscription meter. A visit is bounded by a token cap
//! instead, counted from `step_finish.tokens` while the turn runs, and the
//! turn is killed when it passes what the visit has left.
//!
//! Verified against opencode 1.18.33 with a mock LLM and the mock daycare
//! server; see `tests/fixtures/opencode-1.18.33/`.

pub mod stream;

use super::{Agent, AgentKind, TranscriptDelivery, TurnSpec};
use crate::launch::{is_homecoming_tool, LaunchPlan, SessionMode, DEVICE_TOKEN_ENV, MCP_SERVER};
use crate::meter::WeeklyMeter;
use crate::paths::{
    create_private_dir, sanitize_segment, shell_quote, shell_quote_path, write_atomic, Layout,
};
use crate::stream::{StreamReceipt, TurnEvent};
use crate::turn::TurnPurpose;
use crate::workspace::{Workspace, CLAUDE_MD, CONTROLLER_PROMPT, MCP_CONFIG};
use crate::{Error, Result};
use serde_json::{json, Map, Value};
use std::ffi::OsString;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// The primary agent every turn runs as. `run` falls back to `build` — every
/// built-in tool allowed — when this name is missing, which is why the config
/// also denies everything at the top level and the runner checks both before
/// and after the turn.
pub const AGENT_NAME: &str = "daycare";

/// Without a title, the first `run` of a session spends an extra model call
/// generating one.
pub const SESSION_TITLE: &str = "Daycare visit";

/// Model steps per turn. After this many, OpenCode turns tools off and asks
/// for a final answer. The token cap is the real bound; this keeps one
/// runaway turn from looping on tool calls until the cap.
pub const STEPS_PER_TURN: u32 = 40;

/// How long a preflight or post-turn `opencode` inspection may take.
const INSPECT_TIMEOUT: Duration = Duration::from_secs(60);

/// MCP server connect timeout in the config, in milliseconds.
const MCP_TIMEOUT_MS: u64 = 30_000;

/// The daycare tool OpenCode names `daycare_daycare_memory_save`. Denied on
/// visit turns; the homecoming is where memory is written.
const MEMORY_SAVE_TOOL: &str = "daycare_daycare_memory_save";
const MEMORY_TOOLS: &str = "daycare_daycare_memory_*";

/// Switches that close OpenCode's other sources of instructions, tools, and
/// network side effects. `OPENCODE_DISABLE_DEFAULT_PLUGINS` is deliberately
/// absent: it removes the Copilot and ChatGPT login plugins.
pub const SEAL_SWITCHES: [&str; 6] = [
    "OPENCODE_DISABLE_PROJECT_CONFIG",
    "OPENCODE_DISABLE_CLAUDE_CODE",
    "OPENCODE_DISABLE_EXTERNAL_SKILLS",
    "OPENCODE_DISABLE_AUTOUPDATE",
    "OPENCODE_DISABLE_SHARE",
    "OPENCODE_DISABLE_LSP_DOWNLOAD",
];

/// Parent variables a turn keeps. Everything else, credentials included, is
/// removed before launch.
const KEPT_ENV: [&str; 8] = [
    "PATH", "USER", "LOGNAME", "LANG", "LC_ALL", "LC_CTYPE", "TMPDIR", "TZ",
];

/// The API-key variables OpenCode reads for the providers people commonly
/// use (models.dev `env`). The chosen provider's are passed through; a
/// provider not listed here authenticates from `auth.json` only.
const PROVIDER_KEYS: [(&str, &[&str]); 10] = [
    ("anthropic", &["ANTHROPIC_API_KEY"]),
    ("openai", &["OPENAI_API_KEY"]),
    (
        "google",
        &["GOOGLE_GENERATIVE_AI_API_KEY", "GEMINI_API_KEY"],
    ),
    ("openrouter", &["OPENROUTER_API_KEY"]),
    ("opencode", &["OPENCODE_API_KEY"]),
    ("groq", &["GROQ_API_KEY"]),
    ("xai", &["XAI_API_KEY"]),
    ("deepseek", &["DEEPSEEK_API_KEY"]),
    ("mistral", &["MISTRAL_API_KEY"]),
    ("zai", &["ZHIPU_API_KEY"]),
];

/// Providers whose credentials live under `~` (`~/.aws`, Google ADC). The
/// seal moves `HOME`, so they cannot authenticate; refuse them up front.
const UNSUPPORTED_PROVIDERS: [&str; 3] =
    ["amazon-bedrock", "google-vertex", "google-vertex-anthropic"];

/// A debug build may add one provider to the sealed config, so
/// `dev/opencode-check.sh` can drive a real `opencode` against a mock LLM. A
/// release build ignores it: the seal's config is the runner's alone.
pub const DEV_PROVIDER_ENV: &str = "DAYCARE_OPENCODE_DEV_PROVIDER";

pub struct OpencodeAgent {
    bin: String,
    /// `<daycare root>/opencode`: seals and session databases.
    root: PathBuf,
}

/// The files and directories one identity's turns run inside.
struct Seal {
    dir: PathBuf,
    config: PathBuf,
    home: PathBuf,
    config_home: PathBuf,
    state_home: PathBuf,
    db: PathBuf,
}

impl OpencodeAgent {
    pub fn new(bin: &str, layout: &Layout) -> Self {
        OpencodeAgent {
            bin: bin.to_string(),
            root: layout.root().join("opencode"),
        }
    }

    /// One seal per identity, keyed like its workspace directory.
    fn seal(&self, workspace: &Path) -> Seal {
        let key = sanitize_segment(
            &workspace
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default(),
        );
        let dir = self.root.join("seal").join(&key);
        Seal {
            config: dir.join("daycare.json"),
            home: dir.join("home"),
            config_home: dir.join("config"),
            state_home: dir.join("state"),
            db: self.root.join(format!("{key}.db")),
            dir,
        }
    }

    /// Write this turn's config into the seal and return the environment the
    /// child runs with.
    fn prepare(&self, spec: &TurnSpec<'_>) -> Result<(Seal, String, ChildEnv)> {
        self.check_model(spec.model)?;
        let workspace = Workspace::new(spec.workspace);
        workspace.guard_scaffold_files()?;
        let persona = std::fs::read_to_string(spec.workspace.join(CLAUDE_MD))?;
        let controller = std::fs::read_to_string(spec.workspace.join(CONTROLLER_PROMPT))?;
        let prompt = agent_prompt(&persona, &controller);
        let mcp_url = match spec.purpose {
            TurnPurpose::DayReport => None,
            _ => Some(read_mcp_url(&spec.workspace.join(MCP_CONFIG))?),
        };
        let seal = self.seal(spec.workspace);
        for dir in [
            &self.root,
            &seal.dir,
            &seal.home,
            &seal.config_home,
            &seal.state_home,
        ] {
            create_private_dir(dir)?;
        }
        let config = sealed_config(spec.purpose, &prompt, mcp_url.as_deref(), dev_provider()?);
        write_atomic(
            &seal.config,
            format!("{}\n", serde_json::to_string_pretty(&config)?).as_bytes(),
            0o600,
        )?;
        let env = child_env(&seal, spec.model, &std::env::vars_os().collect::<Vec<_>>())?;
        Ok((seal, prompt, env))
    }

    /// Run an inspection subcommand in the seal, with no device token unless
    /// one is given. Output is bounded by `INSPECT_TIMEOUT`.
    fn inspect(
        &self,
        workspace: &Path,
        env: &ChildEnv,
        args: &[&str],
        device_token: Option<&str>,
    ) -> Result<String> {
        let mut command = Command::new(&self.bin);
        command
            .args(args)
            .current_dir(workspace)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        env.apply(&mut command);
        match device_token {
            Some(token) => command.env(DEVICE_TOKEN_ENV, token),
            None => command.env_remove(DEVICE_TOKEN_ENV),
        };
        let output = run_bounded(command, INSPECT_TIMEOUT).map_err(|error| {
            Error::new(format!(
                "could not run opencode {}: {error}",
                args.join(" ")
            ))
        })?;
        if !output.success {
            return Err(Error::new(format!(
                "opencode {} failed: {}",
                args.join(" "),
                excerpt(&output.stderr)
            )));
        }
        Ok(output.stdout)
    }
}

impl Agent for OpencodeAgent {
    fn kind(&self) -> AgentKind {
        AgentKind::Opencode
    }

    fn program(&self) -> &str {
        &self.bin
    }

    fn product_name(&self) -> &'static str {
        "OpenCode"
    }

    /// None: OpenCode runs any provider's models and there is no safe one to
    /// pick for the person. `check_model` asks for `--model`.
    fn default_model(&self) -> &'static str {
        ""
    }

    fn check_model(&self, model: &str) -> Result<()> {
        check_model(model)
    }

    fn reserves_session_ids(&self) -> bool {
        false
    }

    fn guard(&self, spec: &TurnSpec<'_>) -> Result<()> {
        refuse_managed_config()?;
        let (_seal, prompt, env) = self.prepare(spec)?;
        let pure = "--pure";
        let config = self.inspect(spec.workspace, &env, &["debug", "config", pure], None)?;
        verify_resolved_config(&config, spec.purpose)?;
        let agent = self.inspect(
            spec.workspace,
            &env,
            &["debug", "agent", AGENT_NAME, pure],
            None,
        )?;
        verify_agent_report(&agent, &prompt)?;
        if spec.purpose != TurnPurpose::DayReport {
            let token = spec.device_token.ok_or_else(|| {
                Error::new("no device token to check the daycare MCP server with")
            })?;
            let servers =
                self.inspect(spec.workspace, &env, &["mcp", "list", pure], Some(token))?;
            verify_mcp_connected(&servers)?;
        }
        Ok(())
    }

    fn launch_plan(&self, spec: &TurnSpec<'_>) -> Result<LaunchPlan> {
        if spec.message.trim().is_empty() {
            return Err(Error::new("turn message must not be empty"));
        }
        let (_seal, _prompt, env) = self.prepare(spec)?;
        let mut args: Vec<String> = vec![
            "run".into(),
            "--pure".into(),
            "--format".into(),
            "json".into(),
            "--agent".into(),
            AGENT_NAME.into(),
            "-m".into(),
            spec.model.into(),
            "--title".into(),
            SESSION_TITLE.into(),
        ];
        match spec.mode {
            SessionMode::New { .. } => {}
            SessionMode::Resume { session_id } => {
                args.extend(["--session".into(), session_id.clone()]);
            }
            SessionMode::Fork { parent_session_id } => {
                args.extend([
                    "--session".into(),
                    parent_session_id.clone(),
                    "--fork".into(),
                ]);
            }
        }
        Ok(LaunchPlan {
            program: self.bin.clone(),
            cwd: spec.workspace.to_path_buf(),
            args,
            // `run` appends piped stdin to the message; with no positional
            // message, stdin is the whole message and never shows in `ps`.
            stdin: spec.message.to_string(),
            env_remove: env.remove,
            env: env.set,
        })
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
        workspace: &Path,
    ) -> Result<()> {
        if let Some(name) = receipt.foreign_reach.first() {
            return Err(Error::new(format!(
                "opencode called {name}, which is not a daycare tool"
            )));
        }
        // The only proof of which agent answered: `run` names no agent in its
        // stream, and falls back to `build` when `--agent` is not found.
        let seal = self.seal(workspace);
        let env = ChildEnv::for_inspection(&seal, &std::env::vars_os().collect::<Vec<_>>())?;
        let export = self.inspect(
            workspace,
            &env,
            &["export", &receipt.session_id, "--pure"],
            None,
        )?;
        verify_export_agents(&export, &receipt.session_id)
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
        // The reader has no file tool at all; the transcript rides in the
        // prompt.
        TranscriptDelivery::Inline
    }

    fn meter(&self) -> Option<&dyn WeeklyMeter> {
        None
    }

    fn reopen_command(&self, workspace: &Path, session_id: Option<&str>) -> String {
        let seal = self.seal(workspace);
        let real_home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_default();
        let data = real_xdg("XDG_DATA_HOME", &real_home, ".local/share");
        let cache = real_xdg("XDG_CACHE_HOME", &real_home, ".cache");
        let mut command = format!(
            "cd {} && env -i PATH=\"$PATH\" HOME={} XDG_CONFIG_HOME={} XDG_STATE_HOME={} XDG_DATA_HOME={} XDG_CACHE_HOME={} OPENCODE_CONFIG={} OPENCODE_DB={}",
            shell_quote_path(workspace),
            shell_quote_path(&seal.home),
            shell_quote_path(&seal.config_home),
            shell_quote_path(&seal.state_home),
            shell_quote_path(&data),
            shell_quote_path(&cache),
            shell_quote_path(&seal.config),
            shell_quote_path(&seal.db),
        );
        for switch in SEAL_SWITCHES {
            command.push_str(&format!(" {switch}=1"));
        }
        command.push_str(&format!(" {} --pure", shell_quote(&self.bin)));
        match session_id {
            Some(id) => command.push_str(&format!(" --session {}", shell_quote(id))),
            None => command.push_str("   # no daycare session yet; run a turn first"),
        }
        command
    }
}

/// `provider/model`, from a provider the seal can authenticate.
pub fn check_model(model: &str) -> Result<()> {
    let Some((provider, name)) = model.split_once('/') else {
        return Err(Error::new(format!(
            "OpenCode visits need --model provider/model (for example openai/gpt-5.5 or anthropic/claude-sonnet-5); got {model:?}"
        )));
    };
    let provider_ok = !provider.is_empty()
        && provider
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
    let name_ok = !name.is_empty()
        && model.len() <= 200
        && !name.starts_with('-')
        && name.chars().all(|c| !c.is_whitespace() && !c.is_control());
    if !provider_ok || !name_ok {
        return Err(Error::new(format!(
            "--model must be provider/model with no spaces; got {model:?}"
        )));
    }
    if UNSUPPORTED_PROVIDERS.contains(&provider) {
        return Err(Error::new(format!(
            "OpenCode provider {provider} keeps its credentials under your home directory, \
             which a Daycare visit cannot see; choose a provider that uses an API key or an \
             OpenCode login"
        )));
    }
    Ok(())
}

/// The persona first, then the standing rules. The agent's prompt replaces
/// OpenCode's own system prompt, so this is everything the model is told
/// before the turn message.
fn agent_prompt(persona: &str, controller: &str) -> String {
    format!("{}\n\n{}", persona.trim_end(), controller.trim_end())
}

fn read_mcp_url(path: &Path) -> Result<String> {
    let text = std::fs::read_to_string(path).map_err(|error| {
        Error::new(format!(
            "MCP config is missing: {}: {error}",
            path.display()
        ))
    })?;
    let parsed: Value = serde_json::from_str(&text)
        .map_err(|error| Error::new(format!("MCP config is not valid JSON: {error}")))?;
    parsed["mcpServers"][MCP_SERVER]["url"]
        .as_str()
        .filter(|url| url.starts_with("https://") || url.starts_with("http://"))
        .map(str::to_string)
        .ok_or_else(|| Error::new("MCP config has no daycare server URL"))
}

/// What a turn may call, as OpenCode permission rules. The last matching rule
/// wins, and a tool whose every rule denies it is removed from the model's
/// list (captured: the visit turn was offered four daycare tools, without
/// memory save; the homecoming two; the day report none).
fn permission(purpose: TurnPurpose) -> Value {
    let mut rules = Map::new();
    rules.insert("*".into(), json!("deny"));
    match purpose {
        TurnPurpose::World => {
            rules.insert("daycare_*".into(), json!("allow"));
            rules.insert(MEMORY_SAVE_TOOL.into(), json!("deny"));
        }
        TurnPurpose::PrivateHomecoming => {
            rules.insert(MEMORY_TOOLS.into(), json!("allow"));
        }
        TurnPurpose::DayReport => {}
    }
    Value::Object(rules)
}

/// The whole of OpenCode's configuration for one turn. The device token is
/// referenced, never written.
pub fn sealed_config(
    purpose: TurnPurpose,
    prompt: &str,
    mcp_url: Option<&str>,
    dev_provider: Option<Value>,
) -> Value {
    let permission = permission(purpose);
    let mut config = json!({
        "$schema": "https://opencode.ai/config.json",
        "autoupdate": false,
        "share": "disabled",
        "snapshot": false,
        "permission": permission.clone(),
        "agent": {
            AGENT_NAME: {
                "mode": "primary",
                "prompt": prompt,
                "steps": STEPS_PER_TURN,
                "permission": permission,
            }
        },
    });
    if let Some(url) = mcp_url {
        config["mcp"] = json!({
            MCP_SERVER: {
                "type": "remote",
                "url": url,
                "headers": { "Authorization": format!("Bearer {{env:{DEVICE_TOKEN_ENV}}}") },
                "oauth": false,
                "enabled": true,
                "timeout": MCP_TIMEOUT_MS,
            }
        });
    }
    if let Some(provider) = dev_provider {
        config["provider"] = provider;
    }
    config
}

fn dev_provider() -> Result<Option<Value>> {
    if !cfg!(debug_assertions) {
        return Ok(None);
    }
    match std::env::var(DEV_PROVIDER_ENV) {
        Ok(text) if !text.trim().is_empty() => serde_json::from_str(&text)
            .map(Some)
            .map_err(|error| Error::new(format!("{DEV_PROVIDER_ENV} is not JSON: {error}"))),
        _ => Ok(None),
    }
}

/// The child's environment as removals from the parent's plus settings, so it
/// fits `LaunchPlan` and means the same as `env -i` with a few variables
/// carried over.
struct ChildEnv {
    remove: Vec<String>,
    set: Vec<(String, String)>,
}

impl ChildEnv {
    fn apply(&self, command: &mut Command) {
        for name in &self.remove {
            command.env_remove(name);
        }
        for (name, value) in &self.set {
            command.env(name, value);
        }
    }

    /// The seal with no provider key: for `export`, which reads only the
    /// session database.
    fn for_inspection(seal: &Seal, parent: &[(OsString, OsString)]) -> Result<Self> {
        child_env(seal, "", parent)
    }
}

fn child_env(seal: &Seal, model: &str, parent: &[(OsString, OsString)]) -> Result<ChildEnv> {
    let lookup = |name: &str| {
        parent
            .iter()
            .find(|(key, _)| key == name)
            .and_then(|(_, value)| value.to_str())
            .map(str::to_string)
    };
    let real_home = lookup("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| Error::new("HOME is not set; cannot locate OpenCode's logins"))?;
    let provider = model
        .split_once('/')
        .map(|(provider, _)| provider)
        .unwrap_or("");
    let provider_keys: &[&str] = PROVIDER_KEYS
        .iter()
        .find(|(name, _)| *name == provider)
        .map(|(_, keys)| *keys)
        .unwrap_or(&[]);

    let mut set: Vec<(String, String)> = Vec::new();
    for name in KEPT_ENV.iter().chain(provider_keys) {
        if let Some(value) = lookup(name) {
            set.push((name.to_string(), value));
        }
    }
    let path = |path: &Path| path.to_string_lossy().into_owned();
    set.extend([
        ("HOME".into(), path(&seal.home)),
        ("XDG_CONFIG_HOME".into(), path(&seal.config_home)),
        ("XDG_STATE_HOME".into(), path(&seal.state_home)),
        (
            "XDG_DATA_HOME".into(),
            path(&real_xdg_from(
                lookup("XDG_DATA_HOME"),
                &real_home,
                ".local/share",
            )),
        ),
        (
            "XDG_CACHE_HOME".into(),
            path(&real_xdg_from(
                lookup("XDG_CACHE_HOME"),
                &real_home,
                ".cache",
            )),
        ),
        ("OPENCODE_CONFIG".into(), path(&seal.config)),
        ("OPENCODE_DB".into(), path(&seal.db)),
    ]);
    for switch in SEAL_SWITCHES {
        set.push((switch.into(), "1".into()));
    }
    let remove = parent
        .iter()
        .filter_map(|(key, _)| key.to_str())
        .filter(|key| !set.iter().any(|(name, _)| name == key))
        .map(str::to_string)
        .collect();
    Ok(ChildEnv { remove, set })
}

/// Where OpenCode keeps logins (`auth.json`) and its model cache when `HOME`
/// is the person's own: the explicit variable if absolute, else the XDG
/// default under the real home.
fn real_xdg(name: &str, real_home: &Path, default: &str) -> PathBuf {
    real_xdg_from(
        std::env::var_os(name).and_then(|value| value.into_string().ok()),
        real_home,
        default,
    )
}

fn real_xdg_from(value: Option<String>, real_home: &Path, default: &str) -> PathBuf {
    value
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .unwrap_or_else(|| real_home.join(default))
}

/// OpenCode's system-wide managed config overrides every other source and
/// cannot be excluded. Refuse to run while one exists.
fn refuse_managed_config() -> Result<()> {
    let dir = if cfg!(target_os = "macos") {
        PathBuf::from("/Library/Application Support/opencode")
    } else {
        PathBuf::from("/etc/opencode")
    };
    for name in ["opencode.json", "opencode.jsonc", "config.json"] {
        let path = dir.join(name);
        if std::fs::symlink_metadata(&path).is_ok() {
            return Err(Error::new(format!(
                "OpenCode managed configuration is present at {}; Daycare cannot keep it out of a visit",
                path.display()
            )));
        }
    }
    Ok(())
}

/// `opencode debug config`: the config OpenCode resolved from every source.
/// Only the daycare server may be configured (none on a day report), and
/// nothing may add plugins or instruction files.
pub fn verify_resolved_config(text: &str, purpose: TurnPurpose) -> Result<()> {
    let config: Value = serde_json::from_str(text)
        .map_err(|_| Error::new("opencode debug config was not JSON; refusing the Daycare turn"))?;
    let servers: Vec<&str> = config
        .get("mcp")
        .and_then(Value::as_object)
        .map(|servers| servers.keys().map(String::as_str).collect())
        .unwrap_or_default();
    let expected: &[&str] = match purpose {
        TurnPurpose::DayReport => &[],
        _ => &[MCP_SERVER],
    };
    if servers != expected {
        return Err(Error::new(format!(
            "OpenCode resolved MCP servers {servers:?}; a Daycare turn allows only {expected:?}"
        )));
    }
    let non_empty = |key: &str| match config.get(key) {
        None | Some(Value::Null) => false,
        Some(Value::Array(items)) => !items.is_empty(),
        Some(Value::Object(items)) => !items.is_empty(),
        Some(_) => true,
    };
    for key in ["plugin", "instructions"] {
        if non_empty(key) {
            return Err(Error::new(format!(
                "OpenCode resolved {key} entries from another config source; refusing the Daycare turn"
            )));
        }
    }
    let agents: Vec<&str> = config
        .get("agent")
        .and_then(Value::as_object)
        .map(|agents| agents.keys().map(String::as_str).collect())
        .unwrap_or_default();
    if agents != [AGENT_NAME] {
        return Err(Error::new(format!(
            "OpenCode resolved agents {agents:?}; only the daycare agent may be configured"
        )));
    }
    Ok(())
}

/// `opencode debug agent daycare`: the agent this turn will run as. It must
/// be ours (our prompt, not a same-named agent from elsewhere) with every
/// tool OpenCode lists turned off. MCP tools are not in this list; which of
/// them the model gets is decided by the same permission rules and checked
/// after the turn.
pub fn verify_agent_report(text: &str, prompt: &str) -> Result<()> {
    let agent: Value = serde_json::from_str(text)
        .map_err(|_| Error::new("opencode debug agent was not JSON; refusing the Daycare turn"))?;
    if agent.get("name").and_then(Value::as_str) != Some(AGENT_NAME) {
        return Err(Error::new("OpenCode did not resolve the daycare agent"));
    }
    if agent.get("prompt").and_then(Value::as_str) != Some(prompt) {
        return Err(Error::new(
            "OpenCode's daycare agent does not carry the runner's prompt; another config source overrides it",
        ));
    }
    let tools = agent
        .get("tools")
        .and_then(Value::as_object)
        .ok_or_else(|| {
            Error::new("OpenCode's agent report lists no tools; refusing the Daycare turn")
        })?;
    let enabled: Vec<&str> = tools
        .iter()
        .filter(|(name, on)| {
            on.as_bool() != Some(false) && !name.starts_with(stream::OPENCODE_TOOL_PREFIX)
        })
        .map(|(name, _)| name.as_str())
        .collect();
    if !enabled.is_empty() {
        return Err(Error::new(format!(
            "OpenCode's daycare agent has built-in tools enabled: {}",
            enabled.join(", ")
        )));
    }
    Ok(())
}

/// `opencode mcp list`, a screen meant for people: after the colors are
/// stripped, the daycare server's line must say `connected`.
pub fn verify_mcp_connected(text: &str) -> Result<()> {
    let plain = strip_ansi(text);
    let line = plain
        .lines()
        .find(|line| line.split_whitespace().any(|word| word == MCP_SERVER))
        .ok_or_else(|| Error::new("OpenCode did not list the daycare MCP server"))?;
    if line.split_whitespace().any(|word| word == "connected") {
        return Ok(());
    }
    let status = line
        .split_whitespace()
        .last()
        .unwrap_or("in an unreported state");
    Err(Error::new(format!(
        "daycare MCP server was {status} before the turn started, not connected; the character could not reach the world"
    )))
}

/// `opencode export <session>`: every message in the session belongs to the
/// daycare agent.
pub fn verify_export_agents(text: &str, session_id: &str) -> Result<()> {
    // `export` prints a status line on stderr; stdout is the JSON document,
    // but tolerate anything printed before its first brace.
    let start = text
        .find('{')
        .ok_or_else(|| Error::new("opencode export printed no session"))?;
    let export: Value = serde_json::from_str(&text[start..])
        .map_err(|_| Error::new("opencode export was not JSON"))?;
    if export.pointer("/info/id").and_then(Value::as_str) != Some(session_id) {
        return Err(Error::new("opencode export returned a different session"));
    }
    let messages = export
        .get("messages")
        .and_then(Value::as_array)
        .ok_or_else(|| Error::new("opencode export listed no messages"))?;
    for message in messages {
        let agent = message.pointer("/info/agent").and_then(Value::as_str);
        if agent != Some(AGENT_NAME) {
            return Err(Error::new(format!(
                "the turn ran as OpenCode agent {:?}, not {AGENT_NAME}",
                agent.unwrap_or("unknown")
            )));
        }
    }
    Ok(())
}

/// What each purpose may have called. A turn's seal is checked separately.
fn verify_calls(receipt: &StreamReceipt, purpose: TurnPurpose, noun: &str) -> Result<()> {
    match purpose {
        TurnPurpose::World => {
            if let Some(name) = receipt
                .tool_calls
                .iter()
                .find(|name| name.ends_with("daycare_memory_save"))
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

fn strip_ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            if chars.peek() == Some(&'[') {
                chars.next();
                for next in chars.by_ref() {
                    if next.is_ascii_alphabetic() {
                        break;
                    }
                }
            }
            continue;
        }
        out.push(c);
    }
    out
}

fn excerpt(text: &str) -> String {
    let plain = strip_ansi(text);
    let trimmed = plain.trim();
    let tail: String = trimmed
        .chars()
        .rev()
        .take(300)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    if tail.is_empty() {
        "no output".into()
    } else {
        tail
    }
}

struct BoundedOutput {
    success: bool,
    stdout: String,
    stderr: String,
}

/// Run to completion or kill at the deadline. Both pipes are drained on
/// threads so a chatty child cannot block on a full pipe.
fn run_bounded(mut command: Command, timeout: Duration) -> std::io::Result<BoundedOutput> {
    let mut child = command.spawn()?;
    let drain = |pipe: Option<Box<dyn Read + Send>>| {
        std::thread::spawn(move || {
            let mut text = String::new();
            if let Some(mut pipe) = pipe {
                let _ = pipe.read_to_string(&mut text);
            }
            text
        })
    };
    let stdout = drain(
        child
            .stdout
            .take()
            .map(|pipe| Box::new(pipe) as Box<dyn Read + Send>),
    );
    let stderr = drain(
        child
            .stderr
            .take()
            .map(|pipe| Box::new(pipe) as Box<dyn Read + Send>),
    );
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
        stdout: stdout.join().unwrap_or_default(),
        stderr: stderr.join().unwrap_or_default(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> String {
        std::fs::read_to_string(
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures/opencode-1.18.33")
                .join(name),
        )
        .unwrap()
    }

    fn seal() -> Seal {
        let dir = PathBuf::from("/seal/ws-1");
        Seal {
            config: dir.join("daycare.json"),
            home: dir.join("home"),
            config_home: dir.join("config"),
            state_home: dir.join("state"),
            db: PathBuf::from("/seal/ws-1.db"),
            dir,
        }
    }

    fn vars(pairs: &[(&str, &str)]) -> Vec<(OsString, OsString)> {
        pairs
            .iter()
            .map(|(key, value)| (OsString::from(key), OsString::from(value)))
            .collect()
    }

    #[test]
    fn models_must_name_a_provider_the_seal_can_authenticate() {
        assert!(check_model("openai/gpt-5.5").is_ok());
        assert!(check_model("opencode/big-pickle").is_ok());
        assert!(check_model("openrouter/anthropic/claude-sonnet-5").is_ok());
        assert!(check_model("")
            .unwrap_err()
            .to_string()
            .contains("--model provider/model"));
        assert!(check_model("sonnet").is_err());
        assert!(check_model("openai/").is_err());
        assert!(check_model("openai/gpt 5").is_err());
        assert!(check_model("openai/--auto").is_err());
        assert!(check_model("amazon-bedrock/claude")
            .unwrap_err()
            .to_string()
            .contains("home directory"));
    }

    #[test]
    fn the_visit_config_allows_daycare_tools_except_memory_save() {
        let config = sealed_config(
            TurnPurpose::World,
            "You are Pip.",
            Some("https://example.test/mcp"),
            None,
        );
        let expected =
            json!({"*": "deny", "daycare_*": "allow", "daycare_daycare_memory_save": "deny"});
        assert_eq!(config["permission"], expected);
        assert_eq!(config["agent"]["daycare"]["permission"], expected);
        assert_eq!(config["agent"]["daycare"]["mode"], "primary");
        assert_eq!(config["agent"]["daycare"]["prompt"], "You are Pip.");
        assert_eq!(config["mcp"]["daycare"]["type"], "remote");
        assert_eq!(
            config["mcp"]["daycare"]["headers"]["Authorization"],
            "Bearer {env:DAYCARE_DEVICE_TOKEN}"
        );
        assert_eq!(config["autoupdate"], false);
        assert_eq!(config["share"], "disabled");
        // Permission order is the rule order OpenCode evaluates (last match
        // wins), so the deny-all must come first in the serialized file.
        let text = serde_json::to_string(&config["permission"]).unwrap();
        assert!(text.find("\"*\"").unwrap() < text.find("daycare_*").unwrap());
        assert!(text.find("daycare_*").unwrap() < text.find("memory_save").unwrap());
    }

    #[test]
    fn the_homecoming_config_allows_only_memory_tools() {
        let config = sealed_config(
            TurnPurpose::PrivateHomecoming,
            "p",
            Some("https://x.test/mcp"),
            None,
        );
        assert_eq!(
            config["permission"],
            json!({"*": "deny", "daycare_daycare_memory_*": "allow"})
        );
    }

    #[test]
    fn the_day_report_config_has_no_server_and_no_tools() {
        let config = sealed_config(TurnPurpose::DayReport, "p", None, None);
        assert_eq!(config["permission"], json!({"*": "deny"}));
        assert!(config.get("mcp").is_none());
    }

    #[test]
    fn the_child_env_is_the_seal_plus_a_few_parent_variables() {
        let parent = vars(&[
            ("HOME", "/Users/pip"),
            ("PATH", "/usr/bin"),
            ("OPENAI_API_KEY", "sk-openai"),
            ("ANTHROPIC_API_KEY", "sk-ant"),
            ("CLAUDECODE", "1"),
            ("OPENCODE_CONFIG_CONTENT", "{}"),
            ("XDG_CONFIG_HOME", "/Users/pip/.config"),
        ]);
        let env = child_env(&seal(), "openai/gpt-5.5", &parent).unwrap();
        let get = |name: &str| {
            env.set
                .iter()
                .find(|(key, _)| key == name)
                .map(|(_, v)| v.as_str())
        };
        assert_eq!(get("HOME"), Some("/seal/ws-1/home"));
        assert_eq!(get("XDG_CONFIG_HOME"), Some("/seal/ws-1/config"));
        assert_eq!(get("XDG_DATA_HOME"), Some("/Users/pip/.local/share"));
        assert_eq!(get("XDG_CACHE_HOME"), Some("/Users/pip/.cache"));
        assert_eq!(get("OPENCODE_CONFIG"), Some("/seal/ws-1/daycare.json"));
        assert_eq!(get("OPENCODE_DB"), Some("/seal/ws-1.db"));
        assert_eq!(get("PATH"), Some("/usr/bin"));
        assert_eq!(get("OPENAI_API_KEY"), Some("sk-openai"));
        for switch in SEAL_SWITCHES {
            assert_eq!(get(switch), Some("1"));
        }
        // The other provider's key and everything else is removed.
        assert_eq!(get("ANTHROPIC_API_KEY"), None);
        for name in ["ANTHROPIC_API_KEY", "CLAUDECODE", "OPENCODE_CONFIG_CONTENT"] {
            assert!(env.remove.iter().any(|key| key == name), "{name} kept");
        }
        assert!(!env.remove.iter().any(|key| key == "OPENAI_API_KEY"));
        assert!(!env.remove.iter().any(|key| key == "PATH"));
    }

    #[test]
    fn an_explicit_data_home_is_kept_for_logins() {
        let parent = vars(&[("HOME", "/Users/pip"), ("XDG_DATA_HOME", "/data")]);
        let env = child_env(&seal(), "opencode/big-pickle", &parent).unwrap();
        assert!(env.set.contains(&("XDG_DATA_HOME".into(), "/data".into())));
    }

    #[test]
    fn the_captured_config_and_agent_reports_pass_the_preflight() {
        verify_resolved_config(&fixture("debug-config.json"), TurnPurpose::World).unwrap();
        verify_agent_report(&fixture("debug-agent.json"), "You are Pip.").unwrap();
    }

    #[test]
    fn a_config_with_a_foreign_server_or_the_wrong_purpose_fails_the_preflight() {
        let config = fixture("debug-config.json");
        assert!(verify_resolved_config(&config, TurnPurpose::DayReport).is_err());
        let mut parsed: Value = serde_json::from_str(&config).unwrap();
        parsed["mcp"]["github"] = json!({"type": "local"});
        assert!(verify_resolved_config(&parsed.to_string(), TurnPurpose::World).is_err());
        let mut parsed: Value = serde_json::from_str(&config).unwrap();
        parsed["plugin"] = json!(["evil"]);
        let error = verify_resolved_config(&parsed.to_string(), TurnPurpose::World).unwrap_err();
        assert!(error.to_string().contains("plugin"));
        let mut parsed: Value = serde_json::from_str(&config).unwrap();
        parsed["instructions"] = json!(["~/notes.md"]);
        assert!(verify_resolved_config(&parsed.to_string(), TurnPurpose::World).is_err());
    }

    #[test]
    fn another_prompt_or_an_enabled_built_in_fails_the_preflight() {
        let report = fixture("debug-agent.json");
        let error = verify_agent_report(&report, "You are someone else.").unwrap_err();
        assert!(error.to_string().contains("prompt"));
        let mut parsed: Value = serde_json::from_str(&report).unwrap();
        parsed["tools"]["bash"] = json!(true);
        let error = verify_agent_report(&parsed.to_string(), "You are Pip.").unwrap_err();
        assert!(error.to_string().contains("bash"));
        let mut parsed: Value = serde_json::from_str(&report).unwrap();
        parsed["name"] = json!("build");
        assert!(verify_agent_report(&parsed.to_string(), "You are Pip.").is_err());
    }

    #[test]
    fn the_mcp_screen_must_show_daycare_connected() {
        verify_mcp_connected(&fixture("mcp-list-connected.txt")).unwrap();
        let error = verify_mcp_connected(&fixture("mcp-list-failed.txt")).unwrap_err();
        assert!(error.to_string().contains("failed"), "{error}");
        assert!(verify_mcp_connected("MCP Servers\n0 server(s)\n").is_err());
    }

    #[test]
    fn an_export_answered_by_the_build_agent_fails_the_seal() {
        let daycare = fixture("export-daycare.json");
        verify_export_agents(&daycare, "ses_f173d91f7ffebhfg3cOJwHnscV").unwrap();
        assert!(verify_export_agents(&daycare, "ses_other").is_err());
        let build = fixture("export-build.json");
        let id = serde_json::from_str::<Value>(&build).unwrap()["info"]["id"]
            .as_str()
            .unwrap()
            .to_string();
        let error = verify_export_agents(&build, &id).unwrap_err();
        assert!(error.to_string().contains("\"build\""), "{error}");
    }

    #[test]
    fn each_purpose_calls_only_what_it_may() {
        let receipt = stream::parse_stream(&fixture("world.jsonl")).unwrap();
        verify_calls(&receipt, TurnPurpose::World, "").unwrap();
        assert!(verify_calls(&receipt, TurnPurpose::PrivateHomecoming, "").is_err());
        assert!(verify_calls(&receipt, TurnPurpose::DayReport, "").is_err());
        let homecoming = stream::parse_stream(&fixture("homecoming.jsonl")).unwrap();
        verify_calls(&homecoming, TurnPurpose::PrivateHomecoming, "").unwrap();
        let report = stream::parse_stream(&fixture("day-report.jsonl")).unwrap();
        verify_calls(&report, TurnPurpose::DayReport, "").unwrap();
    }

    #[test]
    fn ansi_colors_are_stripped() {
        assert_eq!(strip_ansi("\u{1b}[90mconnected\u{1b}[0m"), "connected");
    }
}
