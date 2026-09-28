//! OpenAI Codex CLI as a Daycare visitor, on the owner's ChatGPT plan.
//!
//! Ported from the Sep 1 prototype (voices-of-history@daycare-codex-0901)
//! now requiring codex-cli 0.158.0 or newer:
//! - Codex runs in a sealed `CODEX_HOME` and a throwaway `HOME`, both under
//!   the Daycare root, so the owner's global AGENTS.md, config, skills, and
//!   thread history stay out of visits (and visit threads stay out of theirs).
//!   The login is copied in and a refreshed one carried back (`login`).
//! - Before every turn, `codex debug prompt-input` with the turn's own flags
//!   proves the prompt holds only Daycare's persona and the message.
//!   `agents.enabled=false` removes collaboration tools before launch.
//! - After every turn, Codex's own rollout record proves the turn ran
//!   read-only, never asking, in the workspace; and the stream proves nothing
//!   but daycare tools and words happened (`rollout`, `stream`).
//! - No shell, ever: the homecoming transcript is inlined in the prompt.
//! - The weekly meter is the app-server's rate-limit read, no TUI scraping
//!   and no model call (`appserver`).

pub mod appserver;
pub mod catalog;
pub mod launch;
pub mod login;
pub mod preflight;
pub mod rollout;
pub mod stream;

use super::{Agent, AgentKind, TranscriptDelivery, TurnSpec};
use crate::launch::{is_homecoming_tool, LaunchPlan, DEVICE_TOKEN_ENV, STRIPPED_CHILD_ENV};
use crate::meter::WeeklyMeter;
use crate::paths::{create_private_dir, shell_quote, shell_quote_path, Layout};
use crate::stream::{StreamReceipt, TurnEvent};
use crate::turn::TurnPurpose;
use crate::{Error, Result};
use appserver::{AppServerMeter, CodexCommand};
use login::{with_synced_login, CodexLogin, CopiedLogin};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

/// Environment that would move Codex off the owner's ChatGPT plan (API
/// billing, another endpoint) or out of the sealed homes.
pub const STRIPPED_CODEX_ENV: [&str; 5] = [
    "OPENAI_API_KEY",
    "CODEX_API_KEY",
    "OPENAI_BASE_URL",
    "OPENAI_ORGANIZATION",
    "CODEX_HOME",
];

/// The runner-owned homes Codex runs in.
#[derive(Debug, Clone)]
pub struct SealedHomes {
    /// `CODEX_HOME`: the copied login, and the visit threads' rollouts.
    pub codex_home: PathBuf,
    /// `HOME`: empty, so nothing Codex reads from HOME (`~/.agents/skills`)
    /// is the owner's.
    pub user_home: PathBuf,
}

impl SealedHomes {
    pub fn under(layout: &Layout) -> Self {
        let root = layout.root().join("codex");
        SealedHomes {
            codex_home: root.join("home"),
            user_home: root.join("user-home"),
        }
    }

    fn ensure(&self) -> Result<()> {
        create_private_dir(&self.codex_home)?;
        create_private_dir(&self.user_home)?;
        Ok(())
    }

    fn sessions(&self) -> PathBuf {
        self.codex_home.join("sessions")
    }

    fn model_catalog(&self) -> PathBuf {
        self.codex_home.join("daycare-models.json")
    }
}

pub struct CodexAgent {
    bin: String,
    homes: SealedHomes,
    login: Arc<dyn CodexLogin>,
    meter: AppServerMeter,
    /// The plan type, once `account/read` accepted the login.
    account: OnceLock<String>,
    /// The visible catalog, once read.
    models: OnceLock<Vec<String>>,
    version_checked: OnceLock<()>,
}

impl CodexAgent {
    pub fn new(bin: &str, layout: &Layout) -> Result<Self> {
        Ok(Self::with_login(
            bin,
            SealedHomes::under(layout),
            Box::new(CopiedLogin::discover()?),
        ))
    }

    pub fn with_login(bin: &str, homes: SealedHomes, login: Box<dyn CodexLogin>) -> Self {
        let codex = codex_command(bin, &homes, &homes.user_home);
        let login: Arc<dyn CodexLogin> = Arc::from(login);
        let meter = AppServerMeter {
            codex,
            login: Arc::clone(&login),
            sealed_home: homes.codex_home.clone(),
        };
        CodexAgent {
            bin: bin.to_string(),
            homes,
            login,
            meter,
            account: OnceLock::new(),
            models: OnceLock::new(),
            version_checked: OnceLock::new(),
        }
    }

    pub fn homes(&self) -> &SealedHomes {
        &self.homes
    }

    /// Logged in with ChatGPT, in the sealed home. Checked once per process.
    fn ensure_login(&self) -> Result<()> {
        self.homes.ensure()?;
        self.ensure_version()?;
        if self.account.get().is_none() {
            let plan = appserver::check_account(&self.meter.request("account/read", None)?)?;
            let _ = self.account.set(plan);
        }
        Ok(())
    }

    fn catalog(&self) -> Result<&[String]> {
        if let Some(models) = self.models.get() {
            return Ok(models);
        }
        self.ensure_login()?;
        let models = appserver::model_ids(
            &self
                .meter
                .request("model/list", Some(serde_json::json!({})))?,
        );
        Ok(self.models.get_or_init(|| models))
    }

    fn ensure_version(&self) -> Result<()> {
        if self.version_checked.get().is_some() {
            return Ok(());
        }
        let output = self.meter.codex.command().arg("--version").output()?;
        if !output.status.success() {
            return Err(Error::new(
                "could not read Codex CLI version; Daycare requires 0.158.0 or newer",
            ));
        }
        launch::check_version(&String::from_utf8_lossy(&output.stdout))?;
        let _ = self.version_checked.set(());
        Ok(())
    }

    fn verify_rollout(&self, receipt: &StreamReceipt, workspace: &Path) -> Result<()> {
        let Some(path) = rollout::find(&self.homes.sessions(), &receipt.session_id) else {
            // A thread that never got as far as a turn (the required MCP
            // server was down) has nothing to verify and did nothing.
            if receipt.events.is_empty() && receipt.tool_calls.is_empty() {
                return Ok(());
            }
            return Err(Error::new(format!(
                "codex left no record of thread {} under {}; cannot verify the sandbox it ran with",
                receipt.session_id,
                self.homes.sessions().display()
            )));
        };
        match rollout::read(&path)?.turn_context {
            Some(context) => rollout::verify_turn_context(&context, workspace),
            None if receipt.events.is_empty() && receipt.tool_calls.is_empty() => Ok(()),
            None => Err(Error::new(format!(
                "codex's record {} has no turn context; cannot verify the sandbox",
                path.display()
            ))),
        }
    }
}

/// A Codex child in the sealed homes with the owner's API and endpoint
/// variables and the device token removed. `run_turn` adds the token back for
/// turns that reach the daycare server.
fn codex_command(bin: &str, homes: &SealedHomes, cwd: &Path) -> CodexCommand {
    CodexCommand {
        program: bin.to_string(),
        env_remove: env_remove(),
        env: sealed_env(homes),
        cwd: cwd.to_path_buf(),
    }
}

fn env_remove() -> Vec<String> {
    STRIPPED_CHILD_ENV
        .iter()
        .chain(STRIPPED_CODEX_ENV.iter())
        .chain([DEVICE_TOKEN_ENV].iter())
        .map(|name| name.to_string())
        .collect()
}

fn sealed_env(homes: &SealedHomes) -> Vec<(String, String)> {
    vec![
        (
            "CODEX_HOME".into(),
            homes.codex_home.to_string_lossy().into_owned(),
        ),
        (
            "HOME".into(),
            homes.user_home.to_string_lossy().into_owned(),
        ),
    ]
}

/// Managed policy can inject instructions, MCP servers, and hooks that no
/// flag excludes. Refuse it, as Claude's managed settings are refused.
pub fn guard_no_managed_codex(sealed_home: &Path) -> Result<()> {
    let mut sources = vec![
        PathBuf::from("/etc/codex/requirements.toml"),
        PathBuf::from("/etc/codex/managed_config.toml"),
        PathBuf::from("/etc/codex/config.toml"),
        sealed_home.join("requirements.toml"),
        sealed_home.join("managed_config.toml"),
        sealed_home.join("config.toml"),
    ];
    if cfg!(target_os = "macos") {
        // The MDM domain; not verified on a managed Mac.
        sources.push(PathBuf::from(
            "/Library/Managed Preferences/com.openai.codex.plist",
        ));
    }
    for path in sources {
        if std::fs::symlink_metadata(&path).is_ok() {
            return Err(Error::new(format!(
                "Codex on this machine is configured by {}; Daycare cannot seal a managed or \
                 system-configured Codex",
                path.display()
            )));
        }
    }
    Ok(())
}

/// Codex reads AGENTS.md and project `.codex` layers (config, hooks, rules)
/// from the cwd and its ancestors. The workspace carries none, and nothing
/// above it may either.
pub fn guard_codex_ancestors(physical_workspace: &Path) -> Result<()> {
    let mut cursor = Some(physical_workspace);
    while let Some(dir) = cursor {
        for name in ["AGENTS.md", "AGENTS.override.md", ".codex", ".agents"] {
            let candidate = dir.join(name);
            if std::fs::symlink_metadata(&candidate).is_ok() {
                return Err(Error::new(format!(
                    "refusing to run a Codex turn: {} would be loaded into the character's \
                     context from the workspace {}. Move the workspace with \
                     DAYCARE_WORKSPACE_ROOT, or remove it.",
                    candidate.display(),
                    physical_workspace.display()
                )));
            }
        }
        cursor = dir.parent();
    }
    Ok(())
}

impl Agent for CodexAgent {
    fn kind(&self) -> AgentKind {
        AgentKind::Codex
    }

    fn program(&self) -> &str {
        &self.bin
    }

    fn product_name(&self) -> &'static str {
        "Codex"
    }

    fn default_model(&self) -> &'static str {
        launch::DEFAULT_CODEX_MODEL
    }

    fn check_model(&self, model: &str) -> Result<()> {
        launch::check_verified_model(model)?;
        let catalog = self.catalog()?;
        if catalog.iter().any(|known| known == model) {
            return Ok(());
        }
        Err(Error::new(format!(
            "--model must be a Codex model this account can use ({}); got {model:?}",
            catalog.join(", ")
        )))
    }

    fn reserves_session_ids(&self) -> bool {
        false
    }

    fn guard(&self, spec: &TurnSpec<'_>) -> Result<()> {
        self.check_model(spec.model)?;
        guard_no_managed_codex(&self.homes.codex_home)?;
        guard_codex_ancestors(spec.workspace)?;
        self.ensure_login()?;
        let persona = launch::developer_instructions(spec.actor_name);
        let codex = codex_command(&self.bin, &self.homes, spec.workspace);
        with_synced_login(self.login.as_ref(), &self.homes.codex_home, || {
            catalog::prepare(&codex, spec.model, &self.homes.model_catalog())?;
            preflight::prove_sealed_prompt(
                &codex,
                &launch::sealed_model_args(spec.model, &persona, &self.homes.model_catalog()),
                &persona,
            )
        })
    }

    fn launch_plan(&self, spec: &TurnSpec<'_>) -> Result<LaunchPlan> {
        self.ensure_version()?;
        let mut plan = launch::build_exec_plan(&self.bin, spec, &self.homes.model_catalog())?;
        plan.env_remove = env_remove();
        plan.env = sealed_env(&self.homes);
        Ok(plan)
    }

    fn after_turn(&self) -> Result<()> {
        self.login.carry_back(&self.homes.codex_home)
    }

    fn parse_receipt(&self, archive: &str) -> Result<StreamReceipt> {
        let mut receipt = stream::parse(archive)?;
        // The account's weekly window is in Codex's record, not the stream.
        if let Some(path) = rollout::find(&self.homes.sessions(), &receipt.session_id) {
            if let Some(limits) = rollout::read(&path)?.rate_limits {
                rollout::apply_rate_limits(&limits, &mut receipt.usage);
            }
        }
        Ok(receipt)
    }

    fn transcript_events(&self, archive: &str) -> Result<Vec<TurnEvent>> {
        stream::transcript_events(archive)
    }

    fn verify_seal(
        &self,
        receipt: &StreamReceipt,
        purpose: TurnPurpose,
        workspace: &Path,
    ) -> Result<()> {
        // Defense in depth after the launch configuration and preflight:
        // anything but daycare calls and words throws the turn away.
        if !receipt.foreign_reach.is_empty() {
            return Err(Error::new(format!(
                "codex used more than the daycare tools: {}",
                receipt.foreign_reach.join(", ")
            )));
        }
        if purpose == TurnPurpose::DayReport && !receipt.tool_calls.is_empty() {
            return Err(Error::new("day report reached an MCP server"));
        }
        self.verify_rollout(receipt, workspace)
    }

    fn verify_capability(&self, receipt: &StreamReceipt, purpose: TurnPurpose) -> Result<()> {
        match purpose {
            // `required = true` means a turn that ran had the server: Codex
            // exits before the model call when it cannot connect.
            TurnPurpose::World => Ok(()),
            TurnPurpose::PrivateHomecoming => {
                // A declined call reached nothing; failing on it would rerun
                // the homecoming and save every memory twice.
                if let Some(name) = receipt
                    .permitted_tool_calls
                    .iter()
                    .find(|name| !is_homecoming_tool(name))
                {
                    return Err(Error::new(format!(
                        "private homecoming invoked {name}; only memory tools may be called after a visit"
                    )));
                }
                Ok(())
            }
            TurnPurpose::DayReport => {
                if !receipt.tool_calls.is_empty() {
                    return Err(Error::new(
                        "day report invoked a tool instead of remaining local",
                    ));
                }
                Ok(())
            }
        }
    }

    fn verify_archived(
        &self,
        receipt: &StreamReceipt,
        purpose: TurnPurpose,
        workspace: &Path,
    ) -> Result<()> {
        self.verify_seal(receipt, purpose, workspace)?;
        self.verify_capability(receipt, purpose)
            .map_err(|error| Error::new(format!("{error}; the archived receipt cannot be adopted")))
    }

    fn transcript_delivery(&self) -> TranscriptDelivery {
        TranscriptDelivery::Inline
    }

    fn meter(&self) -> Option<&dyn WeeklyMeter> {
        Some(&self.meter)
    }

    fn reopen_command(&self, workspace: &Path, session_id: Option<&str>) -> String {
        let home = format!("CODEX_HOME={}", shell_quote_path(&self.homes.codex_home));
        match session_id {
            Some(id) => format!(
                "cd {} && {home} {} resume {}",
                shell_quote_path(workspace),
                shell_quote(&self.bin),
                shell_quote(id)
            ),
            None => format!(
                "cd {} && {home} {}   # no daycare thread yet; run a turn first",
                shell_quote_path(workspace),
                shell_quote(&self.bin)
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_seal_env_removes_api_routes_and_the_token_and_sets_both_homes() {
        let layout = Layout::at("/tmp/daycare-root");
        let homes = SealedHomes::under(&layout);
        let removed = env_remove();
        for name in [
            "OPENAI_API_KEY",
            "CODEX_API_KEY",
            "OPENAI_BASE_URL",
            "ANTHROPIC_API_KEY",
            DEVICE_TOKEN_ENV,
        ] {
            assert!(removed.contains(&name.to_string()), "{name}");
        }
        let env = sealed_env(&homes);
        assert!(env.contains(&(
            "CODEX_HOME".to_string(),
            "/tmp/daycare-root/codex/home".to_string()
        )));
        assert!(env.contains(&(
            "HOME".to_string(),
            "/tmp/daycare-root/codex/user-home".to_string()
        )));
    }

    #[test]
    fn instruction_files_above_the_workspace_are_refused() {
        let root = crate::testdir::unique_dir("daycare-codex-ancestors");
        let workspace = root.join("ws");
        std::fs::create_dir_all(&workspace).unwrap();
        guard_codex_ancestors(&workspace).unwrap();
        std::fs::write(root.join("AGENTS.md"), "be someone else").unwrap();
        let error = guard_codex_ancestors(&workspace).unwrap_err().to_string();
        assert!(error.contains("AGENTS.md"), "{error}");
        std::fs::remove_file(root.join("AGENTS.md")).unwrap();
        std::fs::create_dir_all(workspace.join(".codex")).unwrap();
        let error = guard_codex_ancestors(&workspace).unwrap_err().to_string();
        assert!(error.contains(".codex"), "{error}");
    }

    #[test]
    fn a_system_config_in_the_sealed_home_is_refused() {
        let home = crate::testdir::unique_dir("daycare-codex-managed");
        std::fs::create_dir_all(&home).unwrap();
        if std::path::Path::new("/etc/codex").exists() {
            return;
        }
        guard_no_managed_codex(&home).unwrap();
        std::fs::write(home.join("requirements.toml"), "").unwrap();
        let error = guard_no_managed_codex(&home).unwrap_err().to_string();
        assert!(error.contains("requirements.toml"), "{error}");
    }
}
