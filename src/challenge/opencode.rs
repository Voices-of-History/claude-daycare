//! OpenCode as a challenge client (plan task N18).
//!
//! A challenge session is the owner's own `opencode`, in their terminal, in
//! the challenge folder, on their own provider API key. Nothing is sealed the
//! way a visit turn is: HOME and the XDG folders are never redirected (that
//! would break git, ssh, gh and the owner's OpenCode logins). Instead the
//! session runs with:
//!
//! - the `OPENCODE_DISABLE_*` switches in `CHALLENGE_SWITCHES`: no project
//!   config or project AGENTS.md from the folder's ancestors, no Claude Code
//!   compatibility (`~/.claude/CLAUDE.md`, `~/.claude/skills`), no
//!   `~/.agents/skills`, no self-update, no sharing;
//! - `OPENCODE_CONFIG` pointing at a config Daycare generates outside the
//!   folder (`challenge_config`): the folder's own AGENTS.md as the one
//!   instruction file, every unapproved MCP server `enabled: false`, every
//!   unapproved agent `disable: true`, a skill rule that denies every skill
//!   but the approved ones, `build` as the agent it starts as, and the one
//!   provider the owner chose as the only provider. It holds no secret: an
//!   approved server keeps its own settings where they are;
//! - `--pure` when no plugin is approved, so no plugin from the owner's
//!   configuration loads. OpenCode has no setting that turns off one plugin,
//!   so with a plugin approved, any unapproved one refuses the start;
//! - `OPENCODE_CONFIG_DIR`, `OPENCODE_CONFIG_CONTENT` and
//!   `OPENCODE_PERMISSION` removed, since each adds a source of config.
//!
//! What a challenge cannot leave out, and so refuses unless it is approved:
//! the global `AGENTS.md` (OpenCode reads it whatever its switches say), a
//! custom tool (code OpenCode imports in every session), a change to
//! OpenCode's own `build` agent, instruction files and skill folders listed
//! in the owner's config (`instructions`, `skills`), and a managed config.
//!
//! Before launch, `verify` proves what OpenCode resolved, with the session's
//! own folder, environment and flags: `opencode debug config` (every source
//! merged, including `~/.opencode/opencode.json`, which OpenCode reads after
//! `OPENCODE_CONFIG` and so could undo it) and `opencode debug agent build`
//! (the agent the session starts as, and its permission rules, against which
//! every unapproved skill must evaluate to `deny`).
//!
//! Provider keys: a challenge runs on the owner's own API key only. The
//! runner reads the login type for the chosen provider from OpenCode's
//! `auth.json` (never the key) and refuses a subscription login (OAuth) or an
//! organisation login. It never stores, prints or forwards a key: OpenCode
//! reads its own logins, and the owner's environment is inherited as it is.
//! The chosen provider's API-key variable is kept even where other challenge
//! sessions drop it (`ANTHROPIC_API_KEY` for `anthropic/*`).
//!
//! Checked against the OpenCode 1.18.33 source and the captures in
//! `tests/fixtures/opencode-1.18.33/`; the runner refuses an older OpenCode.

use super::exclusion::LaunchSpec;
use super::launch::base_command;
use crate::approval::scan::{Client, Item, Kind};
use crate::approval::Reconciled;
use crate::{Error, Result};
use serde_json::{json, Map, Value};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Stdio;

/// The switches a challenge session sets: no Claude Code compatibility
/// (`~/.claude`), no `~/.agents/skills`, no project config or project
/// AGENTS.md from the folder's ancestors, no self-update and no sharing.
pub const CHALLENGE_SWITCHES: [&str; 5] = [
    "OPENCODE_DISABLE_CLAUDE_CODE",
    "OPENCODE_DISABLE_EXTERNAL_SKILLS",
    "OPENCODE_DISABLE_PROJECT_CONFIG",
    "OPENCODE_DISABLE_AUTOUPDATE",
    "OPENCODE_DISABLE_SHARE",
];

/// Variables a challenge session never inherits: each adds a source of
/// configuration or permission rules that the scan does not cover.
pub const STRIPPED_OPENCODE_CHALLENGE_ENV: [&str; 3] = [
    "OPENCODE_CONFIG_DIR",
    "OPENCODE_CONFIG_CONTENT",
    "OPENCODE_PERMISSION",
];

/// OpenCode's own default primary agent.
pub const DEFAULT_AGENT: &str = "build";

/// The generated config's file name, in the challenge's private folder.
pub const CONFIG_FILE: &str = "opencode-config.json";

/// The flag that keeps every plugin from the owner's configuration out.
pub const PURE_FLAG: &str = "--pure";

/// The oldest OpenCode these checks were written against.
pub const MIN_VERSION: (u64, u64, u64) = (1, 18, 33);

/// A skill name nobody has. It must evaluate to `deny` too, so a skill the
/// scan did not see is hidden as well.
pub const PROBE_SKILL: &str = "daycare-unlisted-skill-probe";

fn refuse(what: impl std::fmt::Display) -> Error {
    Error::new(format!("Daycare did not start OpenCode: {what}"))
}

fn refuse_after_setup(what: impl std::fmt::Display) -> Error {
    Error::new(format!(
        "Daycare did not start OpenCode: {what}. Nothing was launched; the challenge folder is \
         set up, so run `challenge start` again once this is fixed"
    ))
}

fn non_empty(config: &Value, key: &str) -> bool {
    match config.get(key) {
        None | Some(Value::Null) => false,
        Some(Value::Array(items)) => !items.is_empty(),
        Some(Value::Object(items)) => !items.is_empty(),
        Some(Value::String(text)) => !text.is_empty(),
        Some(_) => true,
    }
}

/// `provider/model`, the way OpenCode names a model. Returns the provider.
/// Unlike a visit, any provider may be used: nothing about the owner's home
/// is moved.
pub fn check_model(model: &str) -> Result<String> {
    let Some((provider, name)) = model.split_once('/') else {
        return Err(Error::new(format!(
            "OpenCode challenges need --model provider/model (for example \
             openai/gpt-5.5 or anthropic/claude-sonnet-5); got {model:?}"
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
    Ok(provider.to_string())
}

/// `$XDG_DATA_HOME/opencode/auth.json`, or `~/.local/share/opencode/auth.json`.
pub fn auth_file(home: &Path) -> PathBuf {
    std::env::var_os("XDG_DATA_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .unwrap_or_else(|| home.join(".local/share"))
        .join("opencode/auth.json")
}

/// Challenges run on the owner's own API key. Reads only the login type
/// OpenCode stored for `provider`: an API key, or none (a key from the
/// environment or the owner's provider config), passes; a subscription
/// (OAuth) or organisation login refuses. The key is never read out.
pub fn check_api_key_login(auth_file: &Path, provider: &str) -> Result<()> {
    let Ok(bytes) = std::fs::read(auth_file) else {
        return Ok(());
    };
    let Ok(logins) = serde_json::from_slice::<Value>(&bytes) else {
        return Ok(());
    };
    let kind = logins
        .get(provider)
        .and_then(|login| login.get("type"))
        .and_then(Value::as_str)
        .map(str::to_string);
    drop(logins);
    match kind.as_deref() {
        None | Some("api") => Ok(()),
        Some(other) => {
            let what = match other {
                "oauth" => "a subscription login (OAuth)",
                "wellknown" => "an organisation login",
                _ => "a login that is not an API key",
            };
            Err(refuse(format!(
                "OpenCode is signed in to {provider} with {what}, and challenges run on your own \
                 provider API key only. Run `opencode auth login`, choose {provider} and enter an \
                 API key, or choose another provider with --model"
            )))
        }
    }
}

/// OpenCode's system-wide managed configuration overrides every other
/// source and cannot be left out.
pub fn guard_no_managed_config() -> Result<()> {
    let mut paths = Vec::new();
    let dir = if cfg!(target_os = "macos") {
        PathBuf::from("/Library/Application Support/opencode")
    } else {
        PathBuf::from("/etc/opencode")
    };
    for name in ["opencode.json", "opencode.jsonc", "config.json"] {
        paths.push(dir.join(name));
    }
    if cfg!(target_os = "macos") {
        let managed = PathBuf::from("/Library/Managed Preferences");
        paths.push(managed.join("ai.opencode.managed.plist"));
        if let Some(user) = std::env::var_os("USER").filter(|user| !user.is_empty()) {
            paths.push(managed.join(user).join("ai.opencode.managed.plist"));
        }
    }
    for path in paths {
        if std::fs::symlink_metadata(&path).is_ok() {
            return Err(refuse(format!(
                "OpenCode on this machine is configured by {}, and Daycare cannot leave managed \
                 configuration out of a challenge",
                path.display()
            )));
        }
    }
    Ok(())
}

/// The owner's OpenCode memory, sorted into what the approval allows and
/// what it does not.
#[derive(Debug, Clone, Default)]
pub struct Selection {
    pub approved_mcp: BTreeSet<String>,
    pub unapproved_mcp: BTreeSet<String>,
    pub approved_agents: BTreeSet<String>,
    pub unapproved_agents: BTreeSet<String>,
    pub approved_skills: BTreeSet<String>,
    pub unapproved_skills: BTreeSet<String>,
    pub approved_plugins: Vec<ApprovedPlugin>,
    /// Ids and descriptions of unapproved plugins.
    pub unapproved_plugins: Vec<(String, String)>,
    pub unapproved_tools: Vec<PathBuf>,
    pub unapproved_instructions: Vec<PathBuf>,
    /// Item ids of what the generated config switches off, and how.
    pub excluded: Vec<(String, &'static str)>,
}

impl Selection {
    /// No plugin approved: `--pure` keeps every one out.
    pub fn pure(&self) -> bool {
        self.approved_plugins.is_empty()
    }
}

/// An approved plugin, as the resolved config can name it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApprovedPlugin {
    /// A file in a `plugin/` folder; OpenCode names it by `file://` URL.
    File(PathBuf),
    /// A `plugin` entry in a config file, and that file's folder (a
    /// path-like entry is resolved against it).
    Spec(String, PathBuf),
}

fn plugin_label(item: &Item) -> String {
    match &item.name {
        Some(spec) => format!("{spec} (in {})", item.path.display()),
        None => item.path.display().to_string(),
    }
}

/// Sort the owner's OpenCode items by the approval.
pub fn select(items: &[Item], reconciled: &Reconciled) -> Selection {
    let mut selection = Selection::default();
    for item in items.iter().filter(|item| item.client == Client::Opencode) {
        let approved = reconciled.scope_of(&item.id).is_some();
        let name = item.name.clone().unwrap_or_default();
        match (item.kind, approved) {
            (Kind::McpServer, true) => {
                selection.approved_mcp.insert(name);
            }
            (Kind::McpServer, false) => {
                selection.unapproved_mcp.insert(name);
                selection.excluded.push((
                    item.id.clone(),
                    "enabled: false in Daycare's OpenCode config",
                ));
            }
            (Kind::Agent, true) => {
                selection.approved_agents.insert(name);
            }
            (Kind::Agent, false) => {
                selection.unapproved_agents.insert(name);
                selection.excluded.push((
                    item.id.clone(),
                    "disable: true in Daycare's OpenCode config",
                ));
            }
            (Kind::Skill, true) => {
                selection.approved_skills.insert(name);
            }
            (Kind::Skill, false) => {
                selection.unapproved_skills.insert(name);
                selection.excluded.push((
                    item.id.clone(),
                    "a skill deny rule in Daycare's OpenCode config",
                ));
            }
            (Kind::Plugin, true) => selection.approved_plugins.push(match &item.name {
                None => ApprovedPlugin::File(item.path.clone()),
                Some(spec) => ApprovedPlugin::Spec(
                    spec.clone(),
                    item.path.parent().unwrap_or(Path::new("/")).to_path_buf(),
                ),
            }),
            (Kind::Plugin, false) => selection
                .unapproved_plugins
                .push((item.id.clone(), plugin_label(item))),
            (Kind::Tool, false) => selection.unapproved_tools.push(item.path.clone()),
            (Kind::AgentsMd | Kind::ProjectAgentsMd, false) => {
                selection.unapproved_instructions.push(item.path.clone())
            }
            _ => {}
        }
    }
    // An approved name is never switched off, even if an unapproved item
    // shares it (the same agent in two files).
    let approved_agents = selection.approved_agents.clone();
    selection
        .unapproved_agents
        .retain(|name| !approved_agents.contains(name));
    let approved_skills = selection.approved_skills.clone();
    selection
        .unapproved_skills
        .retain(|name| !approved_skills.contains(name));
    let approved_mcp = selection.approved_mcp.clone();
    selection
        .unapproved_mcp
        .retain(|name| !approved_mcp.contains(name));
    if selection.pure() {
        for (id, _) in &selection.unapproved_plugins {
            selection.excluded.push((
                id.clone(),
                "--pure: OpenCode loads no plugin from your configuration",
            ));
        }
    }
    selection
}

/// What stops an OpenCode start before anything is set up, one line each.
pub fn refusals(selection: &Selection) -> Vec<String> {
    let mut lines = Vec::new();
    for path in &selection.unapproved_instructions {
        lines.push(format!(
            "  {} (not approved; OpenCode reads it in every session and has no setting to leave \
             it out)",
            path.display()
        ));
    }
    for path in &selection.unapproved_tools {
        lines.push(format!(
            "  the custom tool {} (not approved; OpenCode imports and runs it in every session)",
            path.display()
        ));
    }
    if !selection.pure() {
        for (_, label) in &selection.unapproved_plugins {
            lines.push(format!(
                "  the plugin {label} (not approved; OpenCode can only leave plugins out all \
                 together, and you approved another one)"
            ));
        }
    }
    if selection.unapproved_agents.contains(DEFAULT_AGENT) {
        lines.push(format!(
            "  your change to OpenCode's own {DEFAULT_AGENT:?} agent (not approved; a challenge \
             starts as that agent, so it cannot be switched off)"
        ));
    }
    for name in &selection.approved_skills {
        if name.contains(['*', '?']) {
            lines.push(format!(
                "  the skill {name:?} (approved, but its name is a pattern, so it cannot be \
                 allowed by name alone)"
            ));
        }
    }
    lines
}

/// The config Daycare writes for one OpenCode challenge session. It holds
/// no secret and no setting of the owner's: only switches.
pub fn challenge_config(pointer: &Path, provider: &str, selection: &Selection) -> Value {
    let mut config = json!({
        "$schema": "https://opencode.ai/config.json",
        "instructions": [pointer.to_string_lossy()],
        "default_agent": DEFAULT_AGENT,
        "enabled_providers": [provider],
    });
    if !selection.unapproved_mcp.is_empty() {
        let servers: Map<String, Value> = selection
            .unapproved_mcp
            .iter()
            .map(|name| (name.clone(), json!({ "enabled": false })))
            .collect();
        config["mcp"] = Value::Object(servers);
    }
    let agents: Map<String, Value> = selection
        .unapproved_agents
        .iter()
        .filter(|name| name.as_str() != DEFAULT_AGENT)
        .map(|name| (name.clone(), json!({ "disable": true })))
        .collect();
    if !agents.is_empty() {
        config["agent"] = Value::Object(agents);
    }
    // `*` sorts before every ordinary name, so it comes first and each name
    // after it has the last word. Unapproved names are denied by name too,
    // in case the owner's own rules allow them after the wildcard.
    let mut skill = Map::new();
    skill.insert("*".into(), json!("deny"));
    for name in &selection.unapproved_skills {
        skill.insert(name.clone(), json!("deny"));
    }
    for name in &selection.approved_skills {
        skill.insert(name.clone(), json!("allow"));
    }
    config["permission"] = json!({ "skill": skill });
    config
}

/// What the resolved configuration must show.
#[derive(Debug, Clone)]
pub struct Expected<'a> {
    pub pointer: String,
    pub provider: &'a str,
    pub selection: &'a Selection,
}

fn names(list: impl IntoIterator<Item = impl std::fmt::Display>) -> String {
    list.into_iter()
        .map(|name| name.to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

fn percent_decode(text: &str) -> String {
    let hex = |byte: u8| (byte as char).to_digit(16).map(|digit| digit as u8);
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            if let (Some(high), Some(low)) = (hex(bytes[index + 1]), hex(bytes[index + 2])) {
                out.push(high * 16 + low);
                index += 3;
                continue;
            }
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn same_file(a: &Path, b: &Path) -> bool {
    a == b
        || matches!(
            (std::fs::canonicalize(a), std::fs::canonicalize(b)),
            (Ok(a), Ok(b)) if a == b
        )
}

/// A path-like plugin spec, resolved as OpenCode resolves it.
fn spec_path(spec: &str, base: &Path) -> Option<PathBuf> {
    if let Some(rest) = spec.strip_prefix("file://") {
        return Some(PathBuf::from(percent_decode(rest)));
    }
    if spec.starts_with('/') {
        return Some(PathBuf::from(spec));
    }
    if spec.starts_with("./") || spec.starts_with("../") {
        return Some(base.join(spec));
    }
    None
}

fn plugin_is_approved(spec: &str, approved: &[ApprovedPlugin]) -> bool {
    let resolved = spec_path(spec, Path::new("/"));
    approved.iter().any(|plugin| match (plugin, &resolved) {
        (ApprovedPlugin::File(path), Some(resolved)) => same_file(path, resolved),
        (ApprovedPlugin::File(_), None) => false,
        (ApprovedPlugin::Spec(approved, _), None) => approved == spec,
        (ApprovedPlugin::Spec(approved, base), Some(resolved)) => {
            approved == spec
                || spec_path(approved, base).is_some_and(|path| same_file(&path, resolved))
        }
    })
}

/// A list of strings, or of `[string, options]` pairs, as strings.
fn strings(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .map(|item| match item {
                    Value::String(text) => text.clone(),
                    Value::Array(pair) => pair
                        .first()
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string(),
                    other => other.to_string(),
                })
                .collect()
        })
        .unwrap_or_default()
}

/// `opencode debug config`: every source merged, as the session will run.
/// Returns what was proven, one line each.
pub fn verify_challenge_config(text: &str, expected: &Expected<'_>) -> Result<Vec<String>> {
    let config: Value = serde_json::from_str(text)
        .map_err(|_| refuse_after_setup("`opencode debug config` did not print JSON"))?;
    let selection = expected.selection;

    let instructions = strings(config.get("instructions"));
    if !instructions.contains(&expected.pointer) {
        return Err(refuse_after_setup(
            "OpenCode did not pick up this folder's AGENTS.md from Daycare's config, so it would \
             start without the challenge's instructions",
        ));
    }
    let extra: Vec<&String> = instructions
        .iter()
        .filter(|entry| **entry != expected.pointer)
        .collect();
    if !extra.is_empty() {
        return Err(refuse_after_setup(format!(
            "your OpenCode configuration loads instruction files Daycare cannot leave out \
             (`instructions`: {}). Remove them from your OpenCode config for the challenge",
            names(extra)
        )));
    }

    if !selection.pure() {
        let unapproved: Vec<String> = strings(config.get("plugin"))
            .into_iter()
            .filter(|spec| !plugin_is_approved(spec, &selection.approved_plugins))
            .collect();
        if !unapproved.is_empty() {
            return Err(refuse_after_setup(format!(
                "OpenCode resolved plugins you did not approve ({}), and Daycare cannot keep \
                 plugin code out while another plugin is approved",
                names(unapproved)
            )));
        }
    }

    let agents = config
        .get("agent")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let disabled = |name: &str| {
        agents
            .get(name)
            .and_then(|agent| agent.get("disable"))
            .and_then(Value::as_bool)
            == Some(true)
    };
    if disabled(DEFAULT_AGENT) {
        return Err(refuse_after_setup(format!(
            "your OpenCode configuration switches off its own {DEFAULT_AGENT:?} agent, which a \
             challenge starts as"
        )));
    }
    let mut on: Vec<&str> = agents
        .keys()
        .map(String::as_str)
        .filter(|name| !selection.approved_agents.contains(*name) && !disabled(name))
        .collect();
    if let Some(modes) = config.get("mode").and_then(Value::as_object) {
        on.extend(
            modes
                .keys()
                .map(String::as_str)
                .filter(|name| !selection.approved_agents.contains(*name) && !disabled(name)),
        );
    }
    on.sort();
    on.dedup();
    if !on.is_empty() {
        return Err(refuse_after_setup(format!(
            "OpenCode resolved agents from your configuration that you did not approve ({}), \
             and they are not switched off",
            names(on)
        )));
    }
    match config.get("default_agent").and_then(Value::as_str) {
        None | Some(DEFAULT_AGENT) => {}
        Some(other) => {
            return Err(refuse_after_setup(format!(
                "OpenCode would start as the agent {other:?}, not its own {DEFAULT_AGENT:?}"
            )))
        }
    }

    let servers = config
        .get("mcp")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let unapproved: Vec<&str> = servers
        .iter()
        .filter(|(name, server)| {
            !selection.approved_mcp.contains(*name)
                && server.get("enabled") != Some(&Value::Bool(false))
        })
        .map(|(name, _)| name.as_str())
        .collect();
    if !unapproved.is_empty() {
        return Err(refuse_after_setup(format!(
            "OpenCode would start MCP servers you did not approve: {}",
            names(unapproved)
        )));
    }

    let providers = strings(config.get("enabled_providers"));
    if providers != [expected.provider] {
        return Err(refuse_after_setup(format!(
            "OpenCode would offer providers other than {} ({}), and a challenge uses only the one \
             you chose",
            expected.provider,
            names(&providers)
        )));
    }

    let skills = config.get("skills");
    let skill_sources: Vec<String> = ["paths", "urls"]
        .iter()
        .flat_map(|key| strings(skills.and_then(|skills| skills.get(*key))))
        .collect();
    if !skill_sources.is_empty() {
        return Err(refuse_after_setup(format!(
            "your OpenCode configuration adds skill folders Daycare did not scan (`skills`: {}). \
             Remove them from your OpenCode config for the challenge",
            names(skill_sources)
        )));
    }

    let mut proven = vec![
        "OpenCode's resolved configuration holds nothing you did not approve (opencode debug \
         config)"
            .to_string(),
        "the only instruction file it adds is this folder's AGENTS.md, written by Daycare"
            .to_string(),
    ];
    let running: Vec<&str> = servers
        .iter()
        .filter(|(_, server)| server.get("enabled") != Some(&Value::Bool(false)))
        .map(|(name, _)| name.as_str())
        .collect();
    proven.push(if running.is_empty() {
        "no MCP server of yours starts".to_string()
    } else {
        format!(
            "the only MCP servers that start are ones you approved ({})",
            names(running)
        )
    });
    if !selection.unapproved_agents.is_empty() {
        proven.push(format!(
            "the agents you did not approve are switched off ({})",
            names(&selection.unapproved_agents)
        ));
    }
    proven.push(format!(
        "the only provider it can use is {}, on your own API key",
        expected.provider
    ));
    if selection.pure() {
        proven.push("it starts with --pure, so no plugin from your configuration loads".into());
    }
    Ok(proven)
}

/// OpenCode's wildcard match: `*` is any run of characters, `?` one, a
/// trailing ` *` also matches nothing, and the whole string must match.
pub fn wildcard(text: &str, pattern: &str) -> bool {
    fn core(text: &[char], pattern: &[char]) -> bool {
        match pattern.split_first() {
            None => text.is_empty(),
            Some(('*', rest)) => (0..=text.len()).any(|at| core(&text[at..], rest)),
            Some(('?', rest)) => !text.is_empty() && core(&text[1..], rest),
            Some((c, rest)) => text.first() == Some(c) && core(&text[1..], rest),
        }
    }
    let text: Vec<char> = text.replace('\\', "/").chars().collect();
    let pattern = pattern.replace('\\', "/");
    if let Some(prefix) = pattern.strip_suffix(" *") {
        let prefix: Vec<char> = prefix.chars().collect();
        if core(&text, &prefix) {
            return true;
        }
    }
    let pattern: Vec<char> = pattern.chars().collect();
    core(&text, &pattern)
}

/// The action OpenCode takes for `permission` on `pattern` under `rules`
/// (the agent's resolved list): the last matching rule wins, and with none
/// it asks.
pub fn evaluate(rules: &[Value], permission: &str, pattern: &str) -> String {
    rules
        .iter()
        .rev()
        .find(|rule| {
            let field = |key: &str| rule.get(key).and_then(Value::as_str).unwrap_or("");
            wildcard(permission, field("permission")) && wildcard(pattern, field("pattern"))
        })
        .and_then(|rule| rule.get("action").and_then(Value::as_str))
        .unwrap_or("ask")
        .to_string()
}

/// `opencode debug agent build`: the agent the session starts as. It must be
/// OpenCode's own (or a change to it the owner approved), and every skill
/// the owner did not approve must be denied to it.
pub fn verify_default_agent(text: &str, expected: &Expected<'_>) -> Result<Vec<String>> {
    let agent: Value = serde_json::from_str(text)
        .map_err(|_| refuse_after_setup("`opencode debug agent` did not print JSON"))?;
    let selection = expected.selection;
    if agent.get("name").and_then(Value::as_str) != Some(DEFAULT_AGENT) {
        return Err(refuse_after_setup(format!(
            "OpenCode did not resolve its own {DEFAULT_AGENT:?} agent"
        )));
    }
    if agent.get("native").and_then(Value::as_bool) != Some(true) {
        return Err(refuse_after_setup(format!(
            "the {DEFAULT_AGENT:?} agent comes from your configuration, not OpenCode"
        )));
    }
    let changed_build = selection.approved_agents.contains(DEFAULT_AGENT);
    if non_empty(&agent, "prompt") && !changed_build {
        return Err(refuse_after_setup(format!(
            "the {DEFAULT_AGENT:?} agent carries a prompt from your configuration, which you \
             did not approve"
        )));
    }
    let rules = agent
        .get("permission")
        .and_then(Value::as_array)
        .cloned()
        .ok_or_else(|| {
            refuse_after_setup("OpenCode's agent report lists no permission rules to check")
        })?;
    let offered: Vec<String> = selection
        .unapproved_skills
        .iter()
        .map(String::as_str)
        .chain([PROBE_SKILL])
        .filter(|name| evaluate(&rules, "skill", name) != "deny")
        .map(|name| {
            if name == PROBE_SKILL {
                "any skill Daycare did not find".to_string()
            } else {
                name.to_string()
            }
        })
        .collect();
    if !offered.is_empty() {
        return Err(refuse_after_setup(format!(
            "OpenCode's permission rules would still offer skills you did not approve ({}); \
             your own OpenCode permissions allow them after Daycare's deny rule",
            names(offered)
        )));
    }
    let mut proven = vec![format!(
        "the session starts as OpenCode's own {DEFAULT_AGENT:?} agent (opencode debug agent \
         {DEFAULT_AGENT})"
    )];
    proven.push(if selection.approved_skills.is_empty() {
        "every skill is hidden from it: you approved none".to_string()
    } else {
        format!(
            "the only skills it is offered are ones you approved ({})",
            names(&selection.approved_skills)
        )
    });
    Ok(proven)
}

/// The checks the owner makes once OpenCode starts.
pub fn manual_checks() -> Vec<String> {
    vec![
        "If OpenCode shows an agent, MCP server, plugin or skill you did not approve, quit \
         OpenCode before you start the work."
            .into(),
    ]
}

fn parse_version(text: &str) -> Option<(u64, u64, u64)> {
    let token = text
        .split_whitespace()
        .find(|word| word.starts_with(|c: char| c.is_ascii_digit()))?;
    let core = token.split(['-', '+']).next()?;
    let mut parts = core.split('.').map(|part| part.parse::<u64>().ok());
    Some((
        parts.next()??,
        parts.next()??,
        parts.next().flatten().unwrap_or(0),
    ))
}

/// OpenCode 1.18.33 or newer: the version these checks were written against.
pub fn check_version(launch: &LaunchSpec) -> Result<()> {
    let output = base_command(launch)
        .arg("--version")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .map_err(|error| {
            Error::new(format!(
                "could not start {} to check its version: {error}. Is OpenCode installed and on \
                 PATH?",
                launch.program
            ))
        })?;
    let text = String::from_utf8_lossy(&output.stdout);
    let (major, minor, patch) = MIN_VERSION;
    match parse_version(&text) {
        Some(version) if output.status.success() && version >= MIN_VERSION => Ok(()),
        Some((a, b, c)) if output.status.success() => Err(refuse(format!(
            "challenges need OpenCode {major}.{minor}.{patch} or newer, and this is {a}.{b}.{c}. \
             Run `opencode upgrade`"
        ))),
        _ => Err(refuse(format!(
            "could not read OpenCode's version; challenges need {major}.{minor}.{patch} or newer"
        ))),
    }
}

/// Run one `opencode debug` command with the session's own folder,
/// environment and flags.
pub fn debug_output(launch: &LaunchSpec, args: &[&str], flags: &[String]) -> Result<String> {
    let output = base_command(launch)
        .args(args)
        .args(flags)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|error| {
            Error::new(format!(
                "could not start {} to check what OpenCode will load: {error}",
                launch.program
            ))
        })?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let last: String = stderr
            .lines()
            .rev()
            .find(|line| !line.trim().is_empty())
            .unwrap_or("")
            .trim()
            .chars()
            .take(300)
            .collect();
        return Err(refuse_after_setup(format!(
            "`opencode {}` failed ({}){}{last}, so Daycare cannot check what OpenCode would load",
            args.join(" "),
            output.status,
            if last.is_empty() { "" } else { ": " }
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
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

    fn selection() -> Selection {
        Selection {
            approved_mcp: BTreeSet::from(["docs".to_string()]),
            unapproved_mcp: BTreeSet::from(["daycare".to_string()]),
            unapproved_agents: BTreeSet::from(["reviewer".to_string()]),
            approved_skills: BTreeSet::from(["keep".to_string()]),
            unapproved_skills: BTreeSet::from(["secret-notes".to_string()]),
            ..Selection::default()
        }
    }

    fn expected(selection: &Selection) -> Expected<'_> {
        Expected {
            pointer: "/h/daycare/challenges/c1/AGENTS.md".into(),
            provider: "openai",
            selection,
        }
    }

    /// The visit capture's config, with Daycare's challenge config merged
    /// over it the way OpenCode merges objects.
    fn resolved(selection: &Selection) -> Value {
        let mut config: Value = serde_json::from_str(&fixture("debug-config.json")).unwrap();
        config.as_object_mut().unwrap().remove("agent");
        let generated = challenge_config(
            Path::new("/h/daycare/challenges/c1/AGENTS.md"),
            "openai",
            selection,
        );
        fn merge(target: &mut Value, source: &Value) {
            match (target, source) {
                (Value::Object(target), Value::Object(source)) => {
                    for (key, value) in source {
                        merge(target.entry(key.clone()).or_insert(Value::Null), value);
                    }
                }
                (target, source) => *target = source.clone(),
            }
        }
        merge(&mut config, &generated);
        config
    }

    #[test]
    fn the_visit_config_is_not_a_clean_challenge_config() {
        let selection = Selection::default();
        let visit = fixture("debug-config.json");
        let error = verify_challenge_config(&visit, &expected(&selection))
            .unwrap_err()
            .to_string();
        assert!(error.contains("AGENTS.md"), "{error}");
    }

    #[test]
    fn a_merged_challenge_config_passes_and_each_leak_refuses() {
        let selection = selection();
        let config = resolved(&selection);
        let proven = verify_challenge_config(&config.to_string(), &expected(&selection)).unwrap();
        assert!(proven[0].contains("opencode debug config"), "{proven:?}");
        assert!(
            proven.iter().any(|line| line.contains("--pure")),
            "{proven:?}"
        );
        assert_eq!(config["mcp"]["daycare"]["enabled"], false);
        assert_eq!(config["agent"]["reviewer"]["disable"], true);

        let cases: Vec<(&str, Value, &str)> = vec![
            (
                "instructions",
                json!(["/h/daycare/challenges/c1/AGENTS.md", "~/notes.md"]),
                "~/notes.md",
            ),
            ("instructions", json!([]), "AGENTS.md"),
            ("default_agent", json!("plan-mine"), "plan-mine"),
            (
                "enabled_providers",
                json!(["openai", "anthropic"]),
                "anthropic",
            ),
            (
                "skills",
                json!({"paths": ["~/more-skills"]}),
                "~/more-skills",
            ),
            ("agent", json!({"reviewer": {"prompt": "x"}}), "reviewer"),
            ("mode", json!({"focus": {"prompt": "x"}}), "focus"),
        ];
        for (key, value, named) in cases {
            let mut bad = config.clone();
            bad[key] = value;
            let error = verify_challenge_config(&bad.to_string(), &expected(&selection))
                .unwrap_err()
                .to_string();
            assert!(error.contains(named), "{key}: {error}");
        }
        // A server re-enabled after Daycare's config refuses and is named.
        let mut bad = config.clone();
        bad["mcp"]["daycare"]["enabled"] = json!(true);
        let error = verify_challenge_config(&bad.to_string(), &expected(&selection))
            .unwrap_err()
            .to_string();
        assert!(error.contains("did not approve: daycare"), "{error}");
        // A switched-off build agent cannot be started as.
        let mut bad = config.clone();
        bad["agent"]["build"] = json!({"disable": true});
        assert!(verify_challenge_config(&bad.to_string(), &expected(&selection)).is_err());
    }

    #[test]
    fn plugins_pass_only_when_pure_or_approved() {
        let mut selection = selection();
        let mut config = resolved(&selection);
        config["plugin"] = json!([
            "opencode-wakatime",
            "file:///h/.config/opencode/plugin/x.ts"
        ]);
        // No plugin approved: --pure, so the list does not load.
        verify_challenge_config(&config.to_string(), &expected(&selection)).unwrap();
        selection.approved_plugins = vec![ApprovedPlugin::Spec(
            "opencode-wakatime".into(),
            PathBuf::from("/h/.config/opencode"),
        )];
        let error = verify_challenge_config(&config.to_string(), &expected(&selection))
            .unwrap_err()
            .to_string();
        assert!(error.contains("plugin/x.ts"), "{error}");
        selection.approved_plugins.push(ApprovedPlugin::File(
            "/h/.config/opencode/plugin/x.ts".into(),
        ));
        verify_challenge_config(&config.to_string(), &expected(&selection)).unwrap();
        assert!(plugin_is_approved(
            "file:///h/a%20b/x.ts",
            &[ApprovedPlugin::File("/h/a b/x.ts".into())]
        ));
        assert!(plugin_is_approved(
            "file:///h/.config/opencode/local.ts",
            &[ApprovedPlugin::Spec(
                "./local.ts".into(),
                "/h/.config/opencode".into()
            )]
        ));
        assert!(!plugin_is_approved(
            "opencode-other",
            &[ApprovedPlugin::File("/h/x.ts".into())]
        ));
    }

    #[test]
    fn only_opencodes_own_build_agent_with_skills_denied_passes() {
        let selection = selection();
        let mut agent: Value = serde_json::from_str(&fixture("debug-agent-build.json")).unwrap();
        // The capture's rules end with the visit's "*": deny, which denies
        // every skill already.
        verify_default_agent(&agent.to_string(), &expected(&selection)).unwrap();
        // The owner's own "*": allow after Daycare's rule would offer them.
        agent["permission"]
            .as_array_mut()
            .unwrap()
            .push(json!({"permission": "*", "action": "allow", "pattern": "*"}));
        let error = verify_default_agent(&agent.to_string(), &expected(&selection))
            .unwrap_err()
            .to_string();
        assert!(error.contains("secret-notes"), "{error}");
        assert!(error.contains("did not find"), "{error}");
        // Daycare's rules after it, as the generated skill map flattens.
        for (pattern, action) in [("*", "deny"), ("secret-notes", "deny"), ("keep", "allow")] {
            agent["permission"]
                .as_array_mut()
                .unwrap()
                .push(json!({"permission": "skill", "action": action, "pattern": pattern}));
        }
        verify_default_agent(&agent.to_string(), &expected(&selection)).unwrap();

        let build = fixture("debug-agent-build.json");
        let mut agent: Value = serde_json::from_str(&build).unwrap();
        agent["prompt"] = Value::String("Always use my house style.".into());
        assert!(verify_default_agent(&agent.to_string(), &expected(&selection)).is_err());
        let mut changed = selection.clone();
        changed.approved_agents.insert("build".into());
        verify_default_agent(&agent.to_string(), &expected(&changed)).unwrap();
        let mut agent: Value = serde_json::from_str(&build).unwrap();
        agent["native"] = Value::Bool(false);
        assert!(verify_default_agent(&agent.to_string(), &expected(&selection)).is_err());
        // The visit's own agent is not OpenCode's default.
        assert!(verify_default_agent(&fixture("debug-agent.json"), &expected(&selection)).is_err());
    }

    #[test]
    fn wildcards_match_as_opencode_matches_them() {
        assert!(wildcard("skill", "*"));
        assert!(wildcard("internal-notes", "internal-*"));
        assert!(!wildcard("notes", "internal-*"));
        assert!(wildcard("ab", "a?"));
        assert!(!wildcard("abc", "a?"));
        assert!(wildcard("ls", "ls *"));
        assert!(wildcard("ls -la", "ls *"));
        assert!(!wildcard("lsof", "ls *"));
        assert!(wildcard("a.b", "a.b"));
        assert!(!wildcard("axb", "a.b"));
        let rules = vec![
            json!({"permission": "*", "action": "allow", "pattern": "*"}),
            json!({"permission": "skill", "action": "deny", "pattern": "*"}),
            json!({"permission": "skill", "action": "allow", "pattern": "keep"}),
        ];
        assert_eq!(evaluate(&rules, "skill", "keep"), "allow");
        assert_eq!(evaluate(&rules, "skill", "other"), "deny");
        assert_eq!(evaluate(&rules, "bash", "ls"), "allow");
        assert_eq!(evaluate(&[], "skill", "x"), "ask");
    }

    #[test]
    fn the_generated_config_holds_switches_only() {
        let selection = selection();
        let config = challenge_config(Path::new("/f/AGENTS.md"), "openai", &selection);
        assert_eq!(config["instructions"], json!(["/f/AGENTS.md"]));
        assert_eq!(config["default_agent"], "build");
        assert_eq!(config["enabled_providers"], json!(["openai"]));
        assert_eq!(config["mcp"], json!({"daycare": {"enabled": false}}));
        assert_eq!(config["agent"], json!({"reviewer": {"disable": true}}));
        let skill = config["permission"]["skill"].as_object().unwrap();
        assert_eq!(skill.keys().next().unwrap(), "*");
        assert_eq!(skill["*"], "deny");
        assert_eq!(skill["secret-notes"], "deny");
        assert_eq!(skill["keep"], "allow");
        // The approved server is not in it: its settings stay where they are.
        assert!(config["mcp"].get("docs").is_none());
    }

    #[test]
    fn an_api_key_login_passes_and_a_subscription_login_refuses() {
        let base = crate::testdir::unique_dir("daycare-opencode-auth");
        std::fs::create_dir_all(&base).unwrap();
        let auth = base.join("auth.json");
        check_api_key_login(&auth, "openai").unwrap();
        std::fs::write(
            &auth,
            r#"{"openai":{"type":"api","key":"sk-planted-1"},"anthropic":{"type":"oauth","access":"a","refresh":"r","expires":1}}"#,
        )
        .unwrap();
        check_api_key_login(&auth, "openai").unwrap();
        check_api_key_login(&auth, "groq").unwrap();
        let error = check_api_key_login(&auth, "anthropic")
            .unwrap_err()
            .to_string();
        assert!(error.contains("subscription login"), "{error}");
        assert!(!error.contains("sk-planted-1"), "{error}");
        std::fs::remove_dir_all(base).ok();
    }

    #[test]
    fn models_and_versions_parse() {
        assert_eq!(check_model("openai/gpt-5.5").unwrap(), "openai");
        assert_eq!(check_model("amazon-bedrock/x.y").unwrap(), "amazon-bedrock");
        assert!(check_model("gpt").is_err());
        assert!(check_model("openai/-x").is_err());
        assert_eq!(parse_version("1.18.33\n"), Some((1, 18, 33)));
        assert_eq!(parse_version("opencode 1.19.0-beta.2"), Some((1, 19, 0)));
        assert_eq!(parse_version("nothing"), None);
        assert!(parse_version("1.18.32").unwrap() < MIN_VERSION);
        assert_eq!(percent_decode("a%20b%zz%2"), "a b%zz%2");
    }

    #[test]
    fn refusals_name_what_cannot_be_left_out() {
        let mut selection = selection();
        assert!(refusals(&selection).is_empty());
        selection.unapproved_instructions = vec!["/h/.config/opencode/AGENTS.md".into()];
        selection.unapproved_tools = vec!["/h/.config/opencode/tools/x.ts".into()];
        selection.unapproved_agents.insert("build".into());
        selection.unapproved_plugins = vec![("id".into(), "opencode-wakatime".into())];
        // Pure: an unapproved plugin is left out, not refused.
        let lines = refusals(&selection).join("\n");
        assert!(
            lines.contains("AGENTS.md") && lines.contains("tools/x.ts"),
            "{lines}"
        );
        assert!(lines.contains("\"build\""), "{lines}");
        assert!(!lines.contains("wakatime"), "{lines}");
        selection.approved_plugins = vec![ApprovedPlugin::File("/p.ts".into())];
        assert!(refusals(&selection).join("\n").contains("wakatime"));
        // Build is never switched off by Daycare's config; it refuses instead.
        let config = challenge_config(Path::new("/f/AGENTS.md"), "openai", &selection);
        assert!(config["agent"].get("build").is_none());
    }
}
