//! Per-client exclusion for a challenge session (plan tasks N8 and N12).
//!
//! A challenge runs in the owner's own client with the owner's own tools, so
//! nothing here seals the client the way a visit turn is sealed. Instead it
//! leaves out exactly the memory the owner did not approve, using each
//! client's own switches, and never redirects HOME (which would break git,
//! ssh, gh and browser profiles).
//!
//! Claude Code: a generated `--settings` file, written outside the challenge
//! folder, carrying `autoMemoryEnabled: false`, `claudeMdExcludes` with full
//! paths, `skillOverrides` and `enabledPlugins` for unapproved skills and
//! plugins, hooks off, and `permissions.deny` read rules for unapproved items,
//! `~/.ssh`, `~/.aws` and Daycare's own folder. Approved MCP servers reach the
//! session through `--strict-mcp-config --mcp-config`, so an unapproved server
//! never starts. `CLAUDE_CODE_DISABLE_AUTO_MEMORY=1` backs the setting up.
//!
//! Codex: config switches only. `--disable memories` is the same form the
//! visit seal uses (`agent::codex::launch`): `--disable` rejects a name Codex
//! does not know, so a renamed feature fails loudly instead of being ignored.
//! Skill instructions are off (approved skills are listed in the folder's
//! AGENTS.md instead), and each unapproved MCP server is switched off by name.
//! Codex has no switch that leaves out a global or parent AGENTS.md without a
//! different CODEX_HOME, so an unapproved one is a refusal, not a silent load.
//!
//! Codex, with `--separate-codex-home` (an opt-in): the same switches, and
//! `CODEX_HOME` points at a folder under Daycare's own state that holds only
//! approved parts (`client::CodexClient` fills it). The owner's global
//! AGENTS.md and unapproved MCP servers are then simply not there, so they
//! are left out instead of refused. A parent folder's AGENTS.md is still read
//! from the folder's ancestors whatever `CODEX_HOME` says, so an unapproved
//! one still refuses. HOME is never redirected.
//!
//! OpenCode: the `OPENCODE_DISABLE_*` switches, a generated `OPENCODE_CONFIG`
//! outside the folder that switches off unapproved MCP servers and agents and
//! denies unapproved skills, and `--pure` when no plugin is approved
//! (`opencode_plan`; the details and the refusals are in `challenge::opencode`).
//! HOME is never redirected, so its global AGENTS.md and custom tools are
//! refusals unless approved.
//!
//! In every client the challenge environment never carries a Daycare token,
//! and it carries the session marker that stops `daycare-runner` from
//! starting visits, challenges or memory approvals from inside the session.
//!
//! What this cannot do: a deny rule binds the client's file tools, not a shell
//! command, and Codex's `workspace-write` limits writes, not reads. A process
//! running as the owner can read what the owner can. The owner's declaration
//! of how the work was done is self-reported.

use crate::agent::codex::launch::toml_string;
use crate::approval::scan::{Client, Inventory, Item, Kind};
use crate::approval::{Reconciled, Scope};
use crate::launch::{DEVICE_TOKEN_ENV, STRIPPED_CHILD_ENV};
use crate::paths::shell_quote;
use crate::{Error, Result};
use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

/// Set in every challenge session's environment to the entry id.
pub const SESSION_MARKER_ENV: &str = "DAYCARE_CHALLENGE_SESSION";

/// Variables a challenge session never inherits, beyond `STRIPPED_CHILD_ENV`:
/// Daycare's credentials and pointers to them.
pub const DAYCARE_SECRET_ENV: [&str; 2] = [DEVICE_TOKEN_ENV, "DAYCARE_TOKEN_FILE"];

/// API keys that would move Codex off the owner's ChatGPT plan, as the visit
/// adapter strips them. `CODEX_HOME` is kept: it is where the owner's login
/// and the memory that was reviewed live.
pub const STRIPPED_CODEX_CHALLENGE_ENV: [&str; 4] = [
    "OPENAI_API_KEY",
    "CODEX_API_KEY",
    "OPENAI_BASE_URL",
    "OPENAI_ORGANIZATION",
];

/// Codex features a challenge session turns off. All are names the visit
/// seal already passes to `--disable`, so a supported Codex knows each one.
pub const CODEX_DISABLED_FEATURES: [&str; 4] = ["memories", "hooks", "plugins", "apps"];

/// The exact command for the owner to run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchSpec {
    pub program: String,
    pub args: Vec<String>,
    pub cwd: PathBuf,
    pub env_set: BTreeMap<String, String>,
    pub env_unset: Vec<String>,
}

impl LaunchSpec {
    /// One line for a POSIX shell: `cd <folder> && env -u ... K=V <client> ...`.
    pub fn shell_command(&self) -> String {
        let mut words = vec!["env".to_string()];
        for name in &self.env_unset {
            words.push("-u".into());
            words.push(name.clone());
        }
        for (name, value) in &self.env_set {
            words.push(format!("{name}={}", shell_quote(value)));
        }
        words.push(shell_quote(&self.program));
        words.extend(self.args.iter().map(|arg| shell_quote(arg)));
        format!(
            "cd {} && {}",
            shell_quote(&self.cwd.to_string_lossy()),
            words.join(" ")
        )
    }

    pub fn to_json(&self) -> Value {
        json!({
            "program": self.program,
            "args": self.args,
            "cwd": self.cwd,
            "env_set": self.env_set,
            "env_unset": self.env_unset,
            "command": self.shell_command(),
        })
    }
}

/// How one unapproved item is kept out of the session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Excluded {
    pub id: String,
    pub how: &'static str,
}

/// Everything the planners need, gathered by `challenge start`.
pub struct PlanInput<'a> {
    pub inventory: &'a Inventory,
    pub reconciled: &'a Reconciled,
    pub folder: &'a Path,
    pub home: &'a Path,
    pub claude_config_dir: &'a Path,
    pub daycare_root: &'a Path,
    pub entry_id: &'a str,
    pub program: &'a str,
    /// Names of `DAYCARE_*` variables in the runner's own environment, so the
    /// session drops any that are set (a test override, a token file path).
    pub inherited_daycare_env: &'a [String],
    /// Codex only: the separate `CODEX_HOME` the owner opted into, or `None`
    /// for the owner's own Codex home.
    pub codex_home: Option<&'a Path>,
}

impl PlanInput<'_> {
    fn approved(&self, item: &Item) -> Option<Scope> {
        self.reconciled.scope_of(&item.id)
    }

    fn env_unset(&self, extra: &[&str]) -> Vec<String> {
        let mut names: BTreeSet<String> = STRIPPED_CHILD_ENV
            .iter()
            .chain(DAYCARE_SECRET_ENV.iter())
            .chain(extra.iter())
            .map(|name| name.to_string())
            .collect();
        names.extend(
            self.inherited_daycare_env
                .iter()
                .filter(|name| name.as_str() != SESSION_MARKER_ENV)
                .cloned(),
        );
        names.into_iter().collect()
    }

    fn marker(&self) -> (String, String) {
        (SESSION_MARKER_ENV.to_string(), self.entry_id.to_string())
    }
}

/// The generated Claude Code launch.
#[derive(Debug, Clone)]
pub struct ClaudePlan {
    pub settings: Value,
    /// Approved MCP servers only. Written beside the settings file, never into
    /// the challenge folder, and never printed: stanzas can hold secrets.
    pub mcp_config: Value,
    pub launch: LaunchSpec,
    pub excluded: Vec<Excluded>,
}

/// A Claude Code permission rule path: `//` marks an absolute path (a single
/// leading `/` would be read relative to the settings file).
fn rule_path(path: &Path) -> String {
    format!("/{}", path.display())
}

/// Claude Code's spelling of a server name inside tool names.
pub fn mcp_tool_prefix(server: &str) -> String {
    let normalized: String = server
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    format!("mcp__{normalized}")
}

pub fn claude_plan(
    input: &PlanInput<'_>,
    settings_file: &Path,
    mcp_config_file: &Path,
    approved_mcp: &BTreeMap<String, Value>,
) -> ClaudePlan {
    let mut excludes = BTreeSet::new();
    let mut deny = BTreeSet::new();
    let mut skill_overrides = Map::new();
    let mut plugins = Map::new();
    let mut excluded = Vec::new();
    let mut unapproved_mcp = false;

    // Daycare's state (tokens, identities, turn archives, this approval) and
    // the usual credential folders are never readable through Claude's tools.
    for (root, glob) in [
        (input.daycare_root.to_path_buf(), true),
        (input.home.join(".ssh"), true),
        (input.home.join(".aws"), true),
        (input.claude_config_dir.join(".credentials.json"), false),
    ] {
        let rule = if glob {
            format!("{}/**", rule_path(&root))
        } else {
            rule_path(&root)
        };
        deny.insert(format!("Read({rule})"));
        if root == input.daycare_root {
            deny.insert(format!("Edit({rule})"));
        }
    }

    // Approved skill names are never switched off, even if an unapproved
    // command shares the name.
    let approved_names: BTreeSet<&str> = input
        .inventory
        .for_client(Client::Claude)
        .filter(|item| input.approved(item).is_some())
        .filter_map(|item| item.name.as_deref())
        .collect();

    for item in input.inventory.for_client(Client::Claude) {
        let instruction = matches!(
            item.kind,
            Kind::ClaudeMd | Kind::Rules | Kind::ProjectClaudeMd | Kind::ProjectRules
        );
        // Every owner CLAUDE.md layer is excluded at its source, approved or
        // not: an approved one reaches the session as the copy in the folder,
        // whose bytes were checked against the approval.
        if instruction {
            excludes.insert(item.path.to_string_lossy().into_owned());
            if let Ok(physical) = std::fs::canonicalize(&item.path) {
                excludes.insert(physical.to_string_lossy().into_owned());
            }
        }
        if input.approved(item).is_some() {
            continue;
        }
        match item.kind {
            Kind::ClaudeMd | Kind::Rules | Kind::ProjectClaudeMd | Kind::ProjectRules => {
                deny.insert(format!("Read({})", rule_path(&item.path)));
                excluded.push(Excluded {
                    id: item.id.clone(),
                    how: "claudeMdExcludes and a read deny rule",
                });
            }
            Kind::AutoMemory => {
                deny.insert(format!("Read({})", rule_path(&item.path)));
                excluded.push(Excluded {
                    id: item.id.clone(),
                    how: "auto memory off and a read deny rule",
                });
            }
            Kind::Skill | Kind::Command => {
                let name = item.name.clone().unwrap_or_default();
                if !approved_names.contains(name.as_str()) {
                    skill_overrides.insert(name.clone(), json!("off"));
                    deny.insert(format!("Skill({name})"));
                }
                let rule = if item.kind == Kind::Skill {
                    format!("{}/**", rule_path(&item.path))
                } else {
                    rule_path(&item.path)
                };
                deny.insert(format!("Read({rule})"));
                excluded.push(Excluded {
                    id: item.id.clone(),
                    how: "skillOverrides off and deny rules",
                });
            }
            Kind::Plugin => {
                if let Some(name) = &item.name {
                    plugins.insert(name.clone(), json!(false));
                }
                excluded.push(Excluded {
                    id: item.id.clone(),
                    how: "enabledPlugins false",
                });
            }
            Kind::McpServer => {
                unapproved_mcp = true;
                if let Some(name) = &item.name {
                    deny.insert(mcp_tool_prefix(name));
                }
                excluded.push(Excluded {
                    id: item.id.clone(),
                    how: "left out of --mcp-config, and a deny rule",
                });
            }
            Kind::AgentsMd | Kind::ProjectAgentsMd | Kind::Agent | Kind::Tool => {}
        }
    }
    // Unapproved servers' settings (and their secrets) live in this file.
    if unapproved_mcp {
        if let Some(item) = input
            .inventory
            .for_client(Client::Claude)
            .find(|item| item.kind == Kind::McpServer)
        {
            deny.insert(format!("Read({})", rule_path(&item.path)));
        }
    }

    let settings = json!({
        "autoMemoryEnabled": false,
        "claudeMdExcludes": excludes.into_iter().collect::<Vec<_>>(),
        "disableAllHooks": true,
        "disableSkillShellExecution": true,
        "disableClaudeAiConnectors": true,
        "enabledPlugins": plugins,
        "skillOverrides": skill_overrides,
        "permissions": { "deny": deny.into_iter().collect::<Vec<_>>() },
    });
    let mcp_config = json!({ "mcpServers": approved_mcp });

    let mut env_set = BTreeMap::new();
    env_set.insert(
        "CLAUDE_CODE_DISABLE_AUTO_MEMORY".to_string(),
        "1".to_string(),
    );
    let (marker, entry) = input.marker();
    env_set.insert(marker, entry);
    let launch = LaunchSpec {
        program: input.program.to_string(),
        args: vec![
            "--settings".into(),
            settings_file.to_string_lossy().into_owned(),
            "--strict-mcp-config".into(),
            "--mcp-config".into(),
            mcp_config_file.to_string_lossy().into_owned(),
        ],
        cwd: input.folder.to_path_buf(),
        env_set,
        env_unset: input.env_unset(&[]),
    };
    ClaudePlan {
        settings,
        mcp_config,
        launch,
        excluded,
    }
}

/// The generated Codex launch.
#[derive(Debug, Clone)]
pub struct CodexPlan {
    pub launch: LaunchSpec,
    pub excluded: Vec<Excluded>,
    /// The switches that shape what Codex loads: features, settings and the
    /// MCP switches. The launch is `--cd <folder> --sandbox workspace-write`
    /// plus these, and `codex debug prompt-input` is run with the same ones,
    /// so the verified prompt is the launched prompt.
    pub config_args: Vec<String>,
}

/// A server name `-c mcp_servers.<name>.enabled=false` can address.
fn codex_addressable(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

pub fn codex_plan(input: &PlanInput<'_>) -> Result<CodexPlan> {
    let separate = input.codex_home.is_some();
    let mut config_args = Vec::new();
    for feature in CODEX_DISABLED_FEATURES {
        config_args.extend(["--disable".to_string(), feature.to_string()]);
    }
    // Approved skills are listed in the folder's AGENTS.md, from their checked
    // copies; this keeps every user skill's instructions out of the prompt.
    config_args.extend([
        "-c".to_string(),
        "skills.include_instructions=false".to_string(),
        "-c".to_string(),
        format!("developer_instructions={}", toml_string("")),
    ]);
    let mut excluded = Vec::new();
    let mut refusals = Vec::new();
    for item in input.inventory.for_client(Client::Codex) {
        if input.approved(item).is_some() {
            continue;
        }
        match item.kind {
            // The global AGENTS.md lives in the owner's Codex home, which a
            // separate home does not read.
            Kind::AgentsMd if separate => excluded.push(Excluded {
                id: item.id.clone(),
                how: "not copied into the separate Codex home",
            }),
            Kind::AgentsMd | Kind::ProjectAgentsMd => refusals.push(format!(
                "  {} (not approved; Codex would load it and has no setting to leave it out)",
                item.path.display()
            )),
            Kind::McpServer if separate => excluded.push(Excluded {
                id: item.id.clone(),
                how: "not copied into the separate Codex home",
            }),
            Kind::McpServer => {
                let name = item.name.clone().unwrap_or_default();
                if !codex_addressable(&name) {
                    refusals.push(format!(
                        "  MCP server {name:?} in {} (not approved; its name cannot be switched \
                         off from the command line)",
                        item.path.display()
                    ));
                    continue;
                }
                config_args.extend([
                    "-c".to_string(),
                    format!("mcp_servers.{name}.enabled=false"),
                ]);
                excluded.push(Excluded {
                    id: item.id.clone(),
                    how: "mcp_servers.<name>.enabled=false",
                });
            }
            Kind::Skill => excluded.push(Excluded {
                id: item.id.clone(),
                how: "skills.include_instructions=false",
            }),
            _ => {}
        }
    }
    if !refusals.is_empty() {
        let way_out = if separate {
            "Approve it with the daycare-memory-review skill, move it aside for the challenge, \
             or start with --agent claude. A parent folder's AGENTS.md is read whatever \
             CODEX_HOME says, so a separate Codex home cannot leave it out"
        } else {
            "Approve it with the daycare-memory-review skill, move it aside for the challenge, \
             start with --agent claude, or start with --separate-codex-home so Codex runs with \
             a Codex home that holds only what you approved. Daycare does not redirect your \
             own Codex home, so it cannot leave these out for you"
        };
        return Err(Error::new(format!(
            "this challenge cannot start in Codex yet, because Codex would load memory you did \
             not approve:\n{}\n{way_out}",
            refusals.join("\n")
        )));
    }
    let mut env_set = BTreeMap::new();
    let (marker, entry) = input.marker();
    env_set.insert(marker, entry);
    if let Some(home) = input.codex_home {
        env_set.insert(
            "CODEX_HOME".to_string(),
            home.to_string_lossy().into_owned(),
        );
    }
    let mut args = vec![
        "--cd".to_string(),
        input.folder.to_string_lossy().into_owned(),
        "--sandbox".into(),
        "workspace-write".into(),
    ];
    args.extend(config_args.iter().cloned());
    Ok(CodexPlan {
        launch: LaunchSpec {
            program: input.program.to_string(),
            args,
            cwd: input.folder.to_path_buf(),
            env_set,
            env_unset: input.env_unset(&STRIPPED_CODEX_CHALLENGE_ENV),
        },
        excluded,
        config_args,
    })
}

/// The generated OpenCode launch.
#[derive(Debug, Clone)]
pub struct OpencodePlan {
    pub launch: LaunchSpec,
    pub excluded: Vec<Excluded>,
    /// Daycare's `OPENCODE_CONFIG`: switches only, no secret.
    pub config: Value,
    /// The flags `opencode debug config` and `debug agent` run with, the
    /// same ones the launch carries (`--pure`, or none).
    pub config_args: Vec<String>,
}

/// The OpenCode launch for `model` (`provider/model`), with Daycare's config
/// at `config_file`. Refuses what OpenCode would load and no switch leaves
/// out. Writes nothing.
pub fn opencode_plan(
    input: &PlanInput<'_>,
    model: &str,
    config_file: &Path,
) -> Result<OpencodePlan> {
    use super::opencode;
    let provider = opencode::check_model(model)?;
    let selection = opencode::select(&input.inventory.items, input.reconciled);
    let refusals = opencode::refusals(&selection);
    if !refusals.is_empty() {
        return Err(Error::new(format!(
            "this challenge cannot start in OpenCode yet, because OpenCode would load memory you              did not approve:\n{}\nApprove it with the daycare-memory-review skill, move it aside              for the challenge, or start with --agent claude or --agent codex. Daycare does not              redirect your home folder, so it cannot leave these out for you",
            refusals.join("\n")
        )));
    }
    let pointer = input.folder.join("AGENTS.md");
    let config = opencode::challenge_config(&pointer, &provider, &selection);
    let config_args: Vec<String> = if selection.pure() {
        vec![opencode::PURE_FLAG.to_string()]
    } else {
        Vec::new()
    };
    let mut env_set = BTreeMap::new();
    let (marker, entry) = input.marker();
    env_set.insert(marker, entry);
    env_set.insert(
        "OPENCODE_CONFIG".to_string(),
        config_file.to_string_lossy().into_owned(),
    );
    for switch in opencode::CHALLENGE_SWITCHES {
        env_set.insert(switch.to_string(), "1".to_string());
    }
    // The owner's own key for the chosen provider stays, even where other
    // challenge sessions drop it (`ANTHROPIC_API_KEY`).
    let keep = crate::agent::opencode::provider_key_names(&provider);
    let env_unset: Vec<String> = input
        .env_unset(&opencode::STRIPPED_OPENCODE_CHALLENGE_ENV)
        .into_iter()
        .filter(|name| !keep.contains(&name.as_str()))
        .collect();
    let mut args = config_args.clone();
    args.extend(["--model".to_string(), model.to_string()]);
    Ok(OpencodePlan {
        launch: LaunchSpec {
            program: input.program.to_string(),
            args,
            cwd: input.folder.to_path_buf(),
            env_set,
            env_unset,
        },
        excluded: selection
            .excluded
            .iter()
            .map(|(id, how)| Excluded {
                id: id.clone(),
                how,
            })
            .collect(),
        config,
        config_args,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::approval::scan::item_id;
    use crate::approval::{reconcile, Approval, ApprovedItem, APPROVAL_VERSION};

    fn item(client: Client, kind: Kind, path: &str, name: Option<&str>) -> Item {
        Item {
            id: item_id(client, kind, Path::new(path), name),
            client,
            kind,
            path: PathBuf::from(path),
            name: name.map(str::to_string),
            bytes: 1,
            sha256: "a".repeat(64),
            origin: None,
        }
    }

    fn inventory() -> Inventory {
        let mut items = vec![
            item(Client::Claude, Kind::ClaudeMd, "/h/.claude/CLAUDE.md", None),
            item(Client::Claude, Kind::ProjectClaudeMd, "/h/CLAUDE.md", None),
            item(Client::Claude, Kind::Rules, "/h/.claude/rules/a.md", None),
            item(
                Client::Claude,
                Kind::AutoMemory,
                "/h/.claude/projects/p/memory/MEMORY.md",
                None,
            ),
            item(
                Client::Claude,
                Kind::Skill,
                "/h/.claude/skills/review",
                Some("review"),
            ),
            item(
                Client::Claude,
                Kind::Skill,
                "/h/.claude/skills/keep",
                Some("keep"),
            ),
            item(Client::Claude, Kind::Plugin, "/p/fmt", Some("fmt@tools")),
            item(
                Client::Claude,
                Kind::McpServer,
                "/h/.claude.json",
                Some("github"),
            ),
            item(
                Client::Claude,
                Kind::McpServer,
                "/h/.claude.json",
                Some("docs"),
            ),
            item(
                Client::Codex,
                Kind::McpServer,
                "/h/.codex/config.toml",
                Some("search"),
            ),
            item(
                Client::Codex,
                Kind::Skill,
                "/h/.agents/skills/notes",
                Some("notes"),
            ),
        ];
        items.sort_by(|a, b| a.id.cmp(&b.id));
        Inventory {
            items,
            warnings: vec![],
        }
    }

    fn approve(inventory: &Inventory, names: &[&str]) -> Reconciled {
        let items = inventory
            .items
            .iter()
            .filter(|item| {
                names.contains(&item.name.as_deref().unwrap_or(""))
                    || names.contains(&item.kind.as_str())
            })
            .map(|item| ApprovedItem {
                id: item.id.clone(),
                client: item.client,
                kind: item.kind,
                path: item.path.clone(),
                name: item.name.clone(),
                sha256: item.sha256.clone(),
                scope: Scope::Private,
            })
            .collect();
        let approval = Approval {
            version: APPROVAL_VERSION,
            written_at: "t".into(),
            items,
        };
        reconcile(Some(&approval), inventory)
    }

    fn input<'a>(
        inventory: &'a Inventory,
        reconciled: &'a Reconciled,
        env: &'a [String],
    ) -> PlanInput<'a> {
        PlanInput {
            inventory,
            reconciled,
            folder: Path::new("/h/daycare/challenges/c1"),
            home: Path::new("/h"),
            claude_config_dir: Path::new("/h/.claude"),
            daycare_root: Path::new("/h/.claude-daycare"),
            entry_id: "entry-1",
            program: "claude",
            inherited_daycare_env: env,
            codex_home: None,
        }
    }

    #[test]
    fn claude_settings_exclude_unapproved_memory_and_keep_approved() {
        let inventory = inventory();
        let reconciled = approve(&inventory, &["claude_md", "keep", "docs"]);
        let env = vec!["DAYCARE_HOME".to_string(), SESSION_MARKER_ENV.to_string()];
        let mut docs = BTreeMap::new();
        docs.insert("docs".to_string(), json!({"command": "docs-server"}));
        let plan = claude_plan(
            &input(&inventory, &reconciled, &env),
            Path::new("/h/.claude-daycare/challenges/c1/claude-settings.json"),
            Path::new("/h/.claude-daycare/challenges/c1/claude-mcp.json"),
            &docs,
        );
        let s = &plan.settings;
        assert_eq!(s["autoMemoryEnabled"], false);
        assert_eq!(s["disableAllHooks"], true);
        // Every CLAUDE.md layer is excluded at the source, approved or not.
        let excludes: Vec<&str> = s["claudeMdExcludes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        for path in [
            "/h/.claude/CLAUDE.md",
            "/h/CLAUDE.md",
            "/h/.claude/rules/a.md",
        ] {
            assert!(excludes.contains(&path), "{excludes:?}");
        }
        let deny: Vec<&str> = s["permissions"]["deny"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        for rule in [
            "Read(//h/.claude-daycare/**)",
            "Edit(//h/.claude-daycare/**)",
            "Read(//h/.ssh/**)",
            "Read(//h/.aws/**)",
            "Read(//h/CLAUDE.md)",
            "Read(//h/.claude/projects/p/memory/MEMORY.md)",
            "Read(//h/.claude/skills/review/**)",
            "Skill(review)",
            "mcp__github",
            "Read(//h/.claude.json)",
        ] {
            assert!(deny.contains(&rule), "missing {rule} in {deny:?}");
        }
        // Approved items are not denied.
        assert!(!deny.contains(&"Read(//h/.claude/CLAUDE.md)"));
        assert!(!deny.contains(&"Skill(keep)"));
        assert!(!deny.contains(&"mcp__docs"));
        assert_eq!(s["skillOverrides"]["review"], "off");
        assert!(s["skillOverrides"].get("keep").is_none());
        assert_eq!(s["enabledPlugins"]["fmt@tools"], false);
        // Only approved servers are in the MCP config.
        assert_eq!(plan.mcp_config["mcpServers"].as_object().unwrap().len(), 1);
        let launch = &plan.launch;
        assert_eq!(launch.env_set["CLAUDE_CODE_DISABLE_AUTO_MEMORY"], "1");
        assert_eq!(launch.env_set[SESSION_MARKER_ENV], "entry-1");
        for name in [
            "DAYCARE_DEVICE_TOKEN",
            "DAYCARE_TOKEN_FILE",
            "DAYCARE_HOME",
            "ANTHROPIC_API_KEY",
        ] {
            assert!(launch.env_unset.iter().any(|n| n == name), "{name}");
        }
        assert!(!launch.env_unset.iter().any(|n| n == SESSION_MARKER_ENV));
        assert!(launch.args.contains(&"--strict-mcp-config".to_string()));
        assert!(!launch
            .args
            .iter()
            .any(|a| a == "--bare" || a == "--safe-mode"));
        assert!(!launch.args.iter().any(|a| a == "--setting-sources"));
        let command = launch.shell_command();
        assert!(
            command.starts_with("cd '/h/daycare/challenges/c1' && env -u "),
            "{command}"
        );
        assert!(
            command.contains("CLAUDE_CODE_DISABLE_AUTO_MEMORY='1'"),
            "{command}"
        );
        assert!(command
            .contains("'--settings' '/h/.claude-daycare/challenges/c1/claude-settings.json'"));
    }

    #[test]
    fn codex_switches_off_unapproved_servers_and_never_redirects_home() {
        let inventory = inventory();
        let reconciled = approve(&inventory, &[]);
        let plan = codex_plan(&PlanInput {
            program: "codex",
            ..input(&inventory, &reconciled, &[])
        })
        .unwrap();
        let args = &plan.launch.args;
        let pairs: Vec<(String, String)> = args
            .windows(2)
            .map(|pair| (pair[0].clone(), pair[1].clone()))
            .collect();
        for (flag, value) in [
            ("--disable", "memories"),
            ("--sandbox", "workspace-write"),
            ("-c", "skills.include_instructions=false"),
            ("-c", "mcp_servers.search.enabled=false"),
        ] {
            assert!(
                pairs.contains(&(flag.to_string(), value.to_string())),
                "missing {flag} {value} in {args:?}"
            );
        }
        // Config switches only: no home redirection, no ignored user config.
        assert!(!plan.launch.env_set.contains_key("HOME"));
        assert!(!plan.launch.env_set.contains_key("CODEX_HOME"));
        assert!(!plan.launch.env_unset.contains(&"CODEX_HOME".to_string()));
        assert!(!args.iter().any(|a| a == "--ignore-user-config"));
        assert!(plan
            .launch
            .env_unset
            .contains(&"OPENAI_API_KEY".to_string()));
        assert!(plan
            .launch
            .env_unset
            .contains(&DEVICE_TOKEN_ENV.to_string()));
        assert_eq!(plan.launch.env_set[SESSION_MARKER_ENV], "entry-1");
    }

    #[test]
    fn codex_refuses_an_unapproved_agents_md_it_cannot_leave_out() {
        let mut inventory = inventory();
        inventory.items.push(item(
            Client::Codex,
            Kind::AgentsMd,
            "/h/.codex/AGENTS.md",
            None,
        ));
        let reconciled = approve(&inventory, &[]);
        let error = codex_plan(&input(&inventory, &reconciled, &[])).unwrap_err();
        assert!(error.message().contains("/h/.codex/AGENTS.md"), "{error}");
        // Approved, it is allowed: Codex loads it natively.
        let reconciled = approve(&inventory, &["agents_md"]);
        assert!(codex_plan(&input(&inventory, &reconciled, &[])).is_ok());
    }

    #[test]
    fn codex_refuses_a_server_name_it_cannot_address() {
        let mut inventory = inventory();
        inventory.items.push(item(
            Client::Codex,
            Kind::McpServer,
            "/h/.codex/config.toml",
            Some("has space"),
        ));
        let reconciled = approve(&inventory, &[]);
        assert!(codex_plan(&input(&inventory, &reconciled, &[])).is_err());
    }

    #[test]
    fn a_separate_codex_home_leaves_the_global_agents_md_out_instead_of_refusing() {
        let mut inventory = inventory();
        inventory.items.push(item(
            Client::Codex,
            Kind::AgentsMd,
            "/h/.codex/AGENTS.md",
            None,
        ));
        inventory.items.push(item(
            Client::Codex,
            Kind::McpServer,
            "/h/.codex/config.toml",
            Some("has space"),
        ));
        let reconciled = approve(&inventory, &[]);
        let home = Path::new("/h/.claude-daycare/challenges/c1/codex-home");
        let plan = codex_plan(&PlanInput {
            program: "codex",
            codex_home: Some(home),
            ..input(&inventory, &reconciled, &[])
        })
        .unwrap();
        assert_eq!(
            plan.launch.env_set["CODEX_HOME"],
            "/h/.claude-daycare/challenges/c1/codex-home"
        );
        // HOME is never redirected, and the owner's CODEX_HOME is not kept.
        assert!(!plan.launch.env_set.contains_key("HOME"));
        assert!(plan
            .excluded
            .iter()
            .any(|e| e.id.contains("/h/.codex/AGENTS.md")
                && e.how == "not copied into the separate Codex home"));
        // Unapproved servers are simply absent; no switch is needed, and a
        // name the command line cannot address is no longer a refusal.
        assert!(!plan
            .config_args
            .iter()
            .any(|a| a.starts_with("mcp_servers.")));
        assert!(plan
            .config_args
            .contains(&"skills.include_instructions=false".to_string()));
        // The launch is the folder and sandbox plus exactly the probe switches.
        assert_eq!(
            plan.launch.args[..4],
            [
                "--cd",
                "/h/daycare/challenges/c1",
                "--sandbox",
                "workspace-write"
            ]
        );
        assert_eq!(plan.launch.args[4..], plan.config_args[..]);

        // A parent folder's AGENTS.md is read whatever CODEX_HOME says.
        inventory.items.push(item(
            Client::Codex,
            Kind::ProjectAgentsMd,
            "/h/daycare/AGENTS.md",
            None,
        ));
        let reconciled = approve(&inventory, &[]);
        let error = codex_plan(&PlanInput {
            codex_home: Some(home),
            ..input(&inventory, &reconciled, &[])
        })
        .unwrap_err();
        assert!(error.message().contains("/h/daycare/AGENTS.md"), "{error}");
        assert!(
            !error.message().contains("--separate-codex-home so"),
            "{error}"
        );
        // Without the opt-in, the refusal names the opt-in.
        let error = codex_plan(&input(&inventory, &reconciled, &[])).unwrap_err();
        assert!(error.message().contains("--separate-codex-home"), "{error}");
    }

    #[test]
    fn mcp_tool_prefixes_match_claude_spelling() {
        assert_eq!(mcp_tool_prefix("github"), "mcp__github");
        assert_eq!(mcp_tool_prefix("my.server"), "mcp__my_server");
    }

    #[test]
    fn the_shell_command_quotes_every_word() {
        let spec = LaunchSpec {
            program: "claude".into(),
            args: vec!["it's".into()],
            cwd: PathBuf::from("/a b"),
            env_set: BTreeMap::from([("K".to_string(), "v w".to_string())]),
            env_unset: vec!["X".into()],
        };
        assert_eq!(
            spec.shell_command(),
            r#"cd '/a b' && env -u X K='v w' 'claude' 'it'"'"'s'"#
        );
    }
}
