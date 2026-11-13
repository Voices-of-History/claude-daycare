//! The challenge clients (plan tasks N9 and N18): Claude Code, Codex CLI and
//! OpenCode behind `ChallengeClient`, the one seam `challenge start` uses for
//! both `--print` and the interactive launch.
//!
//! This is deliberately not the visit `Agent` trait (`crate::agent`). A visit
//! turn is headless, sealed, and reaches only the daycare server. A challenge
//! session is the owner's own client, in their own terminal, in the challenge
//! folder, with their own tools and only the memory they approved. What the
//! two share is the Codex login copy (`agent::codex::login`) and the weekly
//! meters.
//!
//! - `exclusion_plan`: the launch (built by `exclusion`), the files it needs,
//!   all written outside the challenge folder, and what was left out.
//! - `verify`: before launch. Codex renders its prompt with `codex debug
//!   prompt-input`, and every part must be Codex's own or match something the
//!   owner approved, by hash (`verify`). OpenCode shows its resolved config
//!   and its starting agent (`opencode debug config`, `debug agent build`),
//!   checked against the approval (`opencode`). Claude Code has no such
//!   command, so its verification is the checks the owner runs once it starts.
//! - `interactive`: the launch itself, inheriting this terminal (`launch`).
//! - `meter`: one weekly-allowance reading for `meter/start.json` and
//!   `meter/end.json`. A reading, never a cap.
//!
//! A separate Codex home (`--separate-codex-home`, an opt-in) is a folder
//! under Daycare's own state, `<daycare>/challenges/<id>/codex-home/`, never
//! inside the challenge folder. It holds only approved parts: the approved
//! global AGENTS.md, a `config.toml` with the approved MCP servers and nothing
//! else, and a copy of the owner's ChatGPT login (`auth.json`, 0600, in a 0700
//! folder). The login is copied in before each Codex child and a refreshed one
//! is carried back to the owner's own `auth.json` when it exits, the same
//! reconcile visits use, so the owner's own `codex` stays logged in.

use super::exclusion::{self, Excluded, LaunchSpec, PlanInput};
use super::launch::{self, SessionExit};
use super::verify::{self, ApprovedText, CodexAllowed};
use crate::agent::codex::login::{with_synced_login, CodexLogin, CopiedLogin};
use crate::agent::Agent;
use crate::approval::scan::{codex_mcp_stanzas, Client, Kind};
use crate::paths::{create_private_dir, write_atomic, Layout};
use crate::self_update::sha256_hex;
use crate::usage_meter::WeeklyUsageSnapshot;
use crate::{Error, Result};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

/// The folder name of a separate Codex home, under a challenge's private
/// folder in Daycare's state.
pub const SEPARATE_CODEX_HOME: &str = "codex-home";

/// Files in a separate Codex home that Daycare writes from approved parts,
/// and so removes before every start: an item approved last time and not now
/// must not linger.
const GENERATED_CODEX_HOME_FILES: [&str; 3] = ["AGENTS.md", "AGENTS.override.md", "config.toml"];

/// Where the separate Codex home for one challenge lives.
pub fn separate_codex_home(private: &Path) -> PathBuf {
    private.join(SEPARATE_CODEX_HOME)
}

/// Everything one session needs, decided before anything is written.
#[derive(Debug, Clone)]
pub struct ExclusionPlan {
    pub client: Client,
    pub launch: LaunchSpec,
    pub excluded: Vec<Excluded>,
    /// Claude Code's generated settings, or Daycare's OpenCode config, shown
    /// by `--print`. Neither holds a secret.
    pub settings: Option<Value>,
    pub settings_file: Option<PathBuf>,
    pub mcp_config_file: Option<PathBuf>,
    /// Folders to create owner-only before the files are written.
    pub dirs: Vec<PathBuf>,
    /// Generated files Daycare replaces at every start.
    pub remove: Vec<PathBuf>,
    /// Files written 0600 before launch. Never inside the challenge folder.
    pub files: Vec<(PathBuf, Vec<u8>)>,
    /// Codex: the switches `codex debug prompt-input` runs with, the same
    /// ones the launch carries. OpenCode: the flags its `debug` commands run
    /// with (`--pure`, or none).
    pub config_args: Vec<String>,
    /// Codex with the opt-in: the separate `CODEX_HOME`.
    pub codex_home: Option<PathBuf>,
}

/// What `verify` established before launch.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Verification {
    /// Proved by the runner, one line each.
    pub proven: Vec<String>,
    /// For the owner to check once the client has started.
    pub manual: Vec<String>,
}

pub trait ChallengeClient {
    fn client(&self) -> Client;

    /// The product name for people (`Claude Code`, `Codex`, `OpenCode`).
    fn product_name(&self) -> &'static str;

    /// The binary the launch names.
    fn program(&self) -> &str;

    /// The launch and its files for this folder and entry. Reads the
    /// approved bytes it copies and refuses any that changed; writes nothing.
    fn exclusion_plan(&self, input: &PlanInput<'_>, private: &Path) -> Result<ExclusionPlan>;

    /// Before launch: proof where the client can give one, and the checks
    /// the owner makes by hand where it cannot. An error refuses the launch
    /// and says what was found.
    fn verify(&self, plan: &ExclusionPlan, input: &PlanInput<'_>) -> Result<Verification>;

    /// Run the session in this terminal and wait for it to end. With
    /// `stdout_to_stderr`, the client draws on stderr, so `--json` stdout
    /// stays one result.
    fn interactive(&self, plan: &ExclusionPlan, stdout_to_stderr: bool) -> Result<SessionExit>;

    /// One weekly-allowance reading. Spends no model turn.
    fn meter(&self, layout: &Layout) -> Result<WeeklyUsageSnapshot>;
}

/// The binaries `challenge start` was told to run, and OpenCode's model.
#[derive(Debug, Clone, Default)]
pub struct Programs {
    pub claude: String,
    pub codex: String,
    pub opencode: String,
    /// OpenCode only: `provider/model`.
    pub opencode_model: Option<String>,
}

/// The client for `client`, reading the owner's MCP settings from the same
/// places the memory scan did.
pub fn for_client(
    client: Client,
    programs: &Programs,
    roots: &crate::approval::scan::Roots,
) -> Box<dyn ChallengeClient> {
    match client {
        Client::Claude => Box::new(ClaudeClient {
            bin: programs.claude.clone(),
            claude_json: roots.claude_json.clone(),
        }),
        Client::Codex => Box::new(CodexClient {
            bin: programs.codex.clone(),
            owner_codex_home: roots.codex_home.clone(),
        }),
        Client::Opencode => Box::new(OpencodeClient {
            bin: programs.opencode.clone(),
            model: programs.opencode_model.clone().unwrap_or_default(),
            auth_file: super::opencode::auth_file(&roots.home),
        }),
    }
}

/// Write a plan's folders and files. Every path must be outside the
/// challenge folder: generated settings and the Codex login are never
/// something the session's own work could upload.
pub fn write_plan_files(plan: &ExclusionPlan, folder: &Path) -> Result<()> {
    let inside = |path: &Path| path.starts_with(folder) || folder.starts_with(path);
    for path in plan
        .dirs
        .iter()
        .chain(plan.remove.iter())
        .chain(plan.files.iter().map(|(path, _)| path))
    {
        if inside(path) {
            return Err(Error::new(format!(
                "refusing to write {} for the session: it is inside the challenge folder {}",
                path.display(),
                folder.display()
            )));
        }
    }
    for dir in &plan.dirs {
        create_private_dir(dir)?;
    }
    for path in &plan.remove {
        match std::fs::remove_file(path) {
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => return Err(error.into()),
            _ => {}
        }
    }
    for (path, bytes) in &plan.files {
        write_atomic(path, bytes, 0o600)?;
    }
    Ok(())
}

fn approved_of<'a>(
    input: &'a PlanInput<'_>,
    client: Client,
    kind: Kind,
) -> impl Iterator<Item = &'a crate::approval::scan::Item> {
    input
        .reconciled
        .approved
        .iter()
        .map(|(item, _)| item)
        .filter(move |item| item.client == client && item.kind == kind)
}

fn changed_after_approval(what: &str) -> Error {
    Error::new(format!(
        "{what} changed after you approved it; review it again before starting"
    ))
}

/// Read an approved file and check it against the approved hash.
fn read_approved(item: &crate::approval::scan::Item) -> Result<Vec<u8>> {
    let bytes = std::fs::read(&item.path).map_err(|error| {
        Error::new(format!(
            "could not read {}, which you approved: {error}",
            item.path.display()
        ))
    })?;
    if sha256_hex(&bytes) != item.sha256 {
        return Err(changed_after_approval(&item.path.display().to_string()));
    }
    Ok(bytes)
}

// --- Claude Code -----------------------------------------------------------

pub struct ClaudeClient {
    bin: String,
    claude_json: PathBuf,
}

impl ClaudeClient {
    /// Approved MCP stanzas, read from `~/.claude.json` now and checked
    /// against the approved hash. For the generated MCP config; never printed.
    fn approved_mcp(&self, input: &PlanInput<'_>) -> Result<BTreeMap<String, Value>> {
        let wanted: Vec<_> = approved_of(input, Client::Claude, Kind::McpServer).collect();
        let mut stanzas = BTreeMap::new();
        if wanted.is_empty() {
            return Ok(stanzas);
        }
        let bytes = std::fs::read(&self.claude_json)?;
        let value: Value = serde_json::from_slice(&bytes).map_err(|_| {
            Error::new(format!(
                "{} is not valid JSON; cannot bring its MCP servers",
                self.claude_json.display()
            ))
        })?;
        for item in wanted {
            let name = item.name.clone().unwrap_or_default();
            let stanza = value["mcpServers"].get(&name).cloned().ok_or_else(|| {
                Error::new(format!(
                    "the MCP server {name} is gone from {}",
                    self.claude_json.display()
                ))
            })?;
            if sha256_hex(stanza.to_string().as_bytes()) != item.sha256 {
                return Err(changed_after_approval(&format!("the MCP server {name}")));
            }
            stanzas.insert(name, stanza);
        }
        Ok(stanzas)
    }
}

impl ChallengeClient for ClaudeClient {
    fn client(&self) -> Client {
        Client::Claude
    }

    fn product_name(&self) -> &'static str {
        "Claude Code"
    }

    fn program(&self) -> &str {
        &self.bin
    }

    fn exclusion_plan(&self, input: &PlanInput<'_>, private: &Path) -> Result<ExclusionPlan> {
        let approved_mcp = self.approved_mcp(input)?;
        let settings_file = private.join("claude-settings.json");
        let mcp_file = private.join("claude-mcp.json");
        let plan = exclusion::claude_plan(input, &settings_file, &mcp_file, &approved_mcp);
        let settings_text = format!("{}\n", serde_json::to_string_pretty(&plan.settings)?);
        Ok(ExclusionPlan {
            client: Client::Claude,
            launch: plan.launch,
            excluded: plan.excluded,
            files: vec![
                (settings_file.clone(), settings_text.into_bytes()),
                (mcp_file.clone(), plan.mcp_config.to_string().into_bytes()),
            ],
            settings: Some(plan.settings),
            settings_file: Some(settings_file),
            mcp_config_file: Some(mcp_file),
            dirs: vec![],
            remove: vec![],
            config_args: vec![],
            codex_home: None,
        })
    }

    fn verify(&self, _plan: &ExclusionPlan, _input: &PlanInput<'_>) -> Result<Verification> {
        // Claude Code has no command that shows what a session will load
        // before it starts, so nothing here is proven by the runner.
        Ok(Verification {
            proven: vec![],
            manual: verify::claude_manual_checks(),
        })
    }

    fn interactive(&self, plan: &ExclusionPlan, stdout_to_stderr: bool) -> Result<SessionExit> {
        launch::run_interactive(&plan.launch, stdout_to_stderr)
    }

    fn meter(&self, layout: &Layout) -> Result<WeeklyUsageSnapshot> {
        // The visit meter's own reader: `/usage` typed into Claude Code under
        // a PTY in an empty folder, with no prompt and no model turn.
        crate::usage_meter::sample_weekly_usage(
            &self.bin,
            crate::launch::DEFAULT_TURN_MODEL,
            layout,
        )
    }
}

// --- Codex CLI -------------------------------------------------------------

pub struct CodexClient {
    bin: String,
    /// The owner's own Codex home (`CODEX_HOME` or `~/.codex`): where the
    /// approved MCP stanzas and the login are read from.
    owner_codex_home: PathBuf,
}

impl CodexClient {
    fn login(&self) -> CopiedLogin {
        CopiedLogin::new(self.owner_codex_home.clone())
    }

    /// Approved MCP stanzas from the owner's `config.toml`, as raw TOML,
    /// checked against the approved hash.
    fn approved_mcp(&self, input: &PlanInput<'_>) -> Result<BTreeMap<String, String>> {
        let wanted: Vec<_> = approved_of(input, Client::Codex, Kind::McpServer).collect();
        let mut stanzas = BTreeMap::new();
        if wanted.is_empty() {
            return Ok(stanzas);
        }
        let config = self.owner_codex_home.join("config.toml");
        let text = std::fs::read_to_string(&config)
            .map_err(|error| Error::new(format!("could not read {}: {error}", config.display())))?;
        let found: BTreeMap<String, String> = codex_mcp_stanzas(&text).into_iter().collect();
        for item in wanted {
            let name = item.name.clone().unwrap_or_default();
            let stanza = found.get(&name).ok_or_else(|| {
                Error::new(format!(
                    "the MCP server {name} is gone from {}",
                    config.display()
                ))
            })?;
            if sha256_hex(stanza.as_bytes()) != item.sha256 {
                return Err(changed_after_approval(&format!("the MCP server {name}")));
            }
            stanzas.insert(name, stanza.clone());
        }
        Ok(stanzas)
    }

    /// The separate home's contents: approved global AGENTS.md files and a
    /// config.toml holding the approved MCP servers only.
    fn home_files(&self, input: &PlanInput<'_>, home: &Path) -> Result<Vec<(PathBuf, Vec<u8>)>> {
        let mut files = Vec::new();
        for item in approved_of(input, Client::Codex, Kind::AgentsMd) {
            let name = item
                .path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default();
            if !GENERATED_CODEX_HOME_FILES[..2].contains(&name.as_str()) {
                return Err(Error::new(format!(
                    "{} is not an AGENTS.md Daycare can copy into a separate Codex home",
                    item.path.display()
                )));
            }
            files.push((home.join(name), read_approved(item)?));
        }
        let config = codex_home_config(&self.approved_mcp(input)?)?;
        files.push((home.join("config.toml"), config.into_bytes()));
        Ok(files)
    }

    /// What may appear in the rendered prompt: the folder's own AGENTS.md,
    /// approved AGENTS.md files (by hash), and approved skills (by name).
    fn allowed(&self, input: &PlanInput<'_>) -> Result<CodexAllowed> {
        let pointer = input.folder.join("AGENTS.md");
        let pointer_text = std::fs::read_to_string(&pointer).map_err(|error| {
            Error::new(format!(
                "could not read {} to verify the Codex prompt: {error}",
                pointer.display()
            ))
        })?;
        let mut instructions = vec![ApprovedText::new(
            "this folder's AGENTS.md, written by Daycare",
            &pointer_text,
        )];
        for kind in [Kind::AgentsMd, Kind::ProjectAgentsMd] {
            for item in approved_of(input, Client::Codex, kind) {
                let bytes = read_approved(item)?;
                instructions.push(ApprovedText::new(
                    format!("{}, which you approved", item.path.display()),
                    &String::from_utf8_lossy(&bytes),
                ));
            }
        }
        let skills: BTreeSet<String> = approved_of(input, Client::Codex, Kind::Skill)
            .filter_map(|item| item.name.clone())
            .collect();
        let unapproved_instructions = input
            .inventory
            .for_client(Client::Codex)
            .filter(|item| matches!(item.kind, Kind::AgentsMd | Kind::ProjectAgentsMd))
            .filter(|item| input.reconciled.scope_of(&item.id).is_none())
            .map(|item| item.path.clone())
            .collect();
        // A separate home's configuration is Daycare's own and sets none of
        // these; the owner's own one is read as it stands.
        let config = self.owner_codex_home.join("config.toml");
        let base_instruction_overrides = match (input.codex_home, std::fs::read_to_string(&config))
        {
            (None, Ok(text)) => verify::base_instruction_overrides(&text)
                .into_iter()
                .map(|key| format!("{key} in {}", config.display()))
                .collect(),
            _ => Vec::new(),
        };
        Ok(CodexAllowed {
            instructions,
            skills,
            folder: input.folder.to_path_buf(),
            unapproved_instructions,
            base_instruction_overrides,
        })
    }

    /// Run `operation` with the owner's login in the separate home, and the
    /// refreshed login handed back afterwards, errors included.
    fn with_login<T>(
        &self,
        plan: &ExclusionPlan,
        operation: impl FnOnce() -> Result<T>,
    ) -> Result<T> {
        match &plan.codex_home {
            Some(home) => {
                let login = self.login();
                with_synced_login(&login as &dyn CodexLogin, home, operation)
            }
            None => operation(),
        }
    }
}

/// A `config.toml` holding the approved MCP stanzas and nothing else. Read
/// back with the scanner's own parser before it is used, so every server is
/// there with exactly the bytes that were approved.
pub fn codex_home_config(stanzas: &BTreeMap<String, String>) -> Result<String> {
    let mut root = Vec::new();
    let mut tables = Vec::new();
    let mut inline = Vec::new();
    for text in stanzas.values() {
        let first = text.lines().next().unwrap_or("").trim_start();
        if first.starts_with('[') {
            tables.push(text.as_str());
        } else if first.starts_with("mcp_servers.") {
            root.push(text.as_str());
        } else {
            inline.push(text.as_str());
        }
    }
    let mut config = String::from(
        "# Written by Daycare for one challenge session: the MCP servers you approved, and\n\
         # nothing else. Daycare rewrites this file at every `challenge start`.\n",
    );
    for text in root {
        config.push_str(text);
        config.push('\n');
    }
    for text in tables {
        config.push('\n');
        config.push_str(text);
        config.push('\n');
    }
    if !inline.is_empty() {
        config.push_str("\n[mcp_servers]\n");
        for text in inline {
            config.push_str(text);
            config.push('\n');
        }
    }
    let read_back: BTreeMap<String, String> = codex_mcp_stanzas(&config).into_iter().collect();
    if &read_back != stanzas {
        let names: Vec<&str> = stanzas.keys().map(String::as_str).collect();
        return Err(Error::new(format!(
            "could not copy the approved MCP servers ({}) into a separate Codex home exactly; \
             start without --separate-codex-home",
            names.join(", ")
        )));
    }
    Ok(config)
}

impl ChallengeClient for CodexClient {
    fn client(&self) -> Client {
        Client::Codex
    }

    fn product_name(&self) -> &'static str {
        "Codex"
    }

    fn program(&self) -> &str {
        &self.bin
    }

    fn exclusion_plan(&self, input: &PlanInput<'_>, _private: &Path) -> Result<ExclusionPlan> {
        let plan = exclusion::codex_plan(input)?;
        let (dirs, remove, files) = match input.codex_home {
            Some(home) => (
                vec![home.to_path_buf()],
                GENERATED_CODEX_HOME_FILES
                    .iter()
                    .map(|name| home.join(name))
                    .collect(),
                self.home_files(input, home)?,
            ),
            None => (vec![], vec![], vec![]),
        };
        Ok(ExclusionPlan {
            client: Client::Codex,
            launch: plan.launch,
            excluded: plan.excluded,
            settings: None,
            settings_file: None,
            mcp_config_file: None,
            dirs,
            remove,
            files,
            config_args: plan.config_args,
            codex_home: input.codex_home.map(Path::to_path_buf),
        })
    }

    fn verify(&self, plan: &ExclusionPlan, input: &PlanInput<'_>) -> Result<Verification> {
        let allowed = self.allowed(input)?;
        verify::check_codex_version(&plan.launch)?;
        let mut proven = self.with_login(plan, || {
            verify::prove_codex_prompt(&plan.launch, &plan.config_args, &allowed)
        })?;
        if let Some(home) = &plan.codex_home {
            proven.push(format!(
                "Codex runs with its own Codex home, {}, which holds only what you approved",
                home.display()
            ));
        }
        Ok(Verification {
            proven,
            manual: verify::codex_manual_checks(),
        })
    }

    fn interactive(&self, plan: &ExclusionPlan, stdout_to_stderr: bool) -> Result<SessionExit> {
        self.with_login(plan, || {
            launch::run_interactive(&plan.launch, stdout_to_stderr)
        })
    }

    fn meter(&self, layout: &Layout) -> Result<WeeklyUsageSnapshot> {
        // The app-server read the visit meter uses, in Daycare's own sealed
        // Codex home with the owner's login copied in and carried back.
        let agent = crate::agent::codex::CodexAgent::with_login(
            &self.bin,
            crate::agent::codex::SealedHomes::under(layout),
            Box::new(self.login()),
        );
        agent
            .meter()
            .ok_or_else(|| Error::new("Codex has no usage meter"))?
            .sample("", layout)
    }
}

// --- OpenCode --------------------------------------------------------------

pub struct OpencodeClient {
    bin: String,
    /// `provider/model`, from `--model`.
    model: String,
    /// OpenCode's logins, read for the chosen provider's login type only.
    auth_file: PathBuf,
}

impl OpencodeClient {
    fn expected<'a>(
        &'a self,
        input: &PlanInput<'_>,
        provider: &'a str,
        selection: &'a super::opencode::Selection,
    ) -> super::opencode::Expected<'a> {
        super::opencode::Expected {
            pointer: input
                .folder
                .join("AGENTS.md")
                .to_string_lossy()
                .into_owned(),
            provider,
            selection,
        }
    }
}

impl ChallengeClient for OpencodeClient {
    fn client(&self) -> Client {
        Client::Opencode
    }

    fn product_name(&self) -> &'static str {
        "OpenCode"
    }

    fn program(&self) -> &str {
        &self.bin
    }

    fn exclusion_plan(&self, input: &PlanInput<'_>, private: &Path) -> Result<ExclusionPlan> {
        let provider = super::opencode::check_model(&self.model)?;
        super::opencode::check_api_key_login(&self.auth_file, &provider)?;
        let config_file = private.join(super::opencode::CONFIG_FILE);
        let plan = exclusion::opencode_plan(input, &self.model, &config_file)?;
        let text = format!("{}\n", serde_json::to_string_pretty(&plan.config)?);
        Ok(ExclusionPlan {
            client: Client::Opencode,
            launch: plan.launch,
            excluded: plan.excluded,
            settings: Some(plan.config),
            settings_file: Some(config_file.clone()),
            mcp_config_file: None,
            dirs: vec![],
            remove: vec![],
            files: vec![(config_file, text.into_bytes())],
            config_args: plan.config_args,
            codex_home: None,
        })
    }

    fn verify(&self, plan: &ExclusionPlan, input: &PlanInput<'_>) -> Result<Verification> {
        use super::opencode;
        let provider = opencode::check_model(&self.model)?;
        opencode::check_version(&plan.launch)?;
        let selection = opencode::select(&input.inventory.items, input.reconciled);
        if selection.pure()
            != plan
                .config_args
                .iter()
                .any(|arg| arg == opencode::PURE_FLAG)
        {
            return Err(Error::new(
                "the plugin approval changed while the session was being set up; run \
                 `challenge start` again",
            ));
        }
        let expected = self.expected(input, &provider, &selection);
        let config = opencode::debug_output(&plan.launch, &["debug", "config"], &plan.config_args)?;
        let mut proven = opencode::verify_challenge_config(&config, &expected)?;
        let agent = opencode::debug_output(
            &plan.launch,
            &["debug", "agent", opencode::DEFAULT_AGENT],
            &plan.config_args,
        )?;
        proven.extend(opencode::verify_default_agent(&agent, &expected)?);
        Ok(Verification {
            proven,
            manual: opencode::manual_checks(),
        })
    }

    fn interactive(&self, plan: &ExclusionPlan, stdout_to_stderr: bool) -> Result<SessionExit> {
        launch::run_interactive(&plan.launch, stdout_to_stderr)
    }

    fn meter(&self, _layout: &Layout) -> Result<WeeklyUsageSnapshot> {
        Err(Error::new(
            "OpenCode has no weekly allowance to read: it runs on your own provider API key",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_separate_home_config_holds_exactly_the_approved_stanzas() {
        let text = "model = \"gpt\"\nmcp_servers.dotted.command = \"a\"\n\n[mcp_servers]\ninline = { command = \"b\" }\n\n[mcp_servers.plain]\ncommand = \"d\"\n\n[mcp_servers.plain.env]\nKEY = \"v\"\n\n[mcp_servers.secret]\ncommand = \"e\"\n";
        let mut approved: BTreeMap<String, String> = codex_mcp_stanzas(text).into_iter().collect();
        approved.remove("secret");
        let config = codex_home_config(&approved).unwrap();
        assert!(!config.contains("secret"), "{config}");
        assert!(!config.contains("model"), "{config}");
        let read_back: BTreeMap<String, String> = codex_mcp_stanzas(&config).into_iter().collect();
        assert_eq!(read_back, approved);
        assert!(codex_home_config(&BTreeMap::new())
            .unwrap()
            .starts_with("# Written by Daycare"));
    }

    #[test]
    fn plan_files_inside_the_challenge_folder_are_refused() {
        let base = crate::testdir::unique_dir("daycare-plan-files");
        let folder = base.join("folder");
        let plan = ExclusionPlan {
            client: Client::Codex,
            launch: LaunchSpec {
                program: "codex".into(),
                args: vec![],
                cwd: folder.clone(),
                env_set: BTreeMap::new(),
                env_unset: vec![],
            },
            excluded: vec![],
            settings: None,
            settings_file: None,
            mcp_config_file: None,
            dirs: vec![folder.join("codex-home")],
            remove: vec![],
            files: vec![(folder.join("codex-home/auth.json"), b"{}".to_vec())],
            config_args: vec![],
            codex_home: Some(folder.join("codex-home")),
        };
        let error = write_plan_files(&plan, &folder).unwrap_err();
        assert!(
            error.message().contains("inside the challenge folder"),
            "{error}"
        );
        assert!(!folder.join("codex-home").exists());
        let outside = ExclusionPlan {
            dirs: vec![base.join("private/codex-home")],
            files: vec![(
                base.join("private/codex-home/config.toml"),
                b"# x\n".to_vec(),
            )],
            ..plan
        };
        write_plan_files(&outside, &folder).unwrap();
        assert!(base.join("private/codex-home/config.toml").is_file());
        std::fs::remove_dir_all(base).ok();
    }
}
