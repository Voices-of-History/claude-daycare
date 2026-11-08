//! OpenCode as a challenge client (plan task N18): started, not wired.
//!
//! Built so far: the checks an OpenCode launch would make before it starts,
//! over what OpenCode itself resolved, tested against the 1.18.33 captures in
//! `tests/fixtures/opencode-1.18.33/` only.
//!
//! - `opencode debug config`, the config merged from every source: no
//!   plugins, no instruction files, no custom agents, the built-in default
//!   agent, and every MCP server either approved or switched off.
//! - `opencode debug agent build`, the agent the session runs as: OpenCode's
//!   own, with no prompt from anywhere else.
//! - Plugin folders on disk: OpenCode runs plugins as code, and a folder
//!   plugin is not something the config check can be trusted to see. Any file
//!   in one refuses, since nothing here can prove it is off.
//!
//! Not built, and why `challenge start --agent opencode` still refuses:
//!
//! - The memory scan (`approval::scan`) has no OpenCode inventory, and its
//!   `Client` has no OpenCode variant, so no OpenCode memory can be approved
//!   or recorded in ENTRY.json yet. That module is outside this stream.
//! - The generated `OPENCODE_CONFIG` (approved MCP only, the others switched
//!   off with `enabled: false`) needs a check on a real OpenCode first: that a
//!   partial server entry validates and merges over the global one, and that
//!   `OPENCODE_DISABLE_PROJECT_CONFIG` leaves the folder's own AGENTS.md in.
//! - HOME is not redirected (it would break git, ssh and logins), so the
//!   global `~/.config/opencode/AGENTS.md` loads; until it can be approved,
//!   its presence would be a refusal.

use crate::{Error, Result};
use serde_json::Value;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// The switches a challenge session would set: no Claude Code compatibility
/// (`~/.claude`), no `~/.agents/skills`, no project config from the folder's
/// ancestors, no self-update and no sharing.
pub const CHALLENGE_SWITCHES: [&str; 5] = [
    "OPENCODE_DISABLE_CLAUDE_CODE",
    "OPENCODE_DISABLE_EXTERNAL_SKILLS",
    "OPENCODE_DISABLE_PROJECT_CONFIG",
    "OPENCODE_DISABLE_AUTOUPDATE",
    "OPENCODE_DISABLE_SHARE",
];

/// OpenCode's own default primary agent.
pub const DEFAULT_AGENT: &str = "build";

fn refuse(what: impl std::fmt::Display) -> Error {
    Error::new(format!("Daycare did not start OpenCode: {what}"))
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

/// `opencode debug config`, for a challenge whose approved MCP servers are
/// `approved_mcp`.
pub fn verify_challenge_config(text: &str, approved_mcp: &BTreeSet<String>) -> Result<()> {
    let config: Value = serde_json::from_str(text)
        .map_err(|_| refuse("`opencode debug config` did not print JSON"))?;
    if non_empty(&config, "plugin") {
        return Err(refuse(
            "OpenCode resolved plugins, and Daycare cannot keep plugin code out of a challenge",
        ));
    }
    if non_empty(&config, "instructions") {
        return Err(refuse(
            "OpenCode resolved instruction files you did not approve",
        ));
    }
    if non_empty(&config, "agent") {
        let names: Vec<&str> = config["agent"]
            .as_object()
            .map(|agents| agents.keys().map(String::as_str).collect())
            .unwrap_or_default();
        return Err(refuse(format!(
            "OpenCode resolved agents from your configuration ({}), whose prompts you did not \
             approve",
            names.join(", ")
        )));
    }
    if let Some(default) = config.get("default_agent").and_then(Value::as_str) {
        if default != DEFAULT_AGENT {
            return Err(refuse(format!(
                "OpenCode would start as the agent {default:?}, not its own {DEFAULT_AGENT:?}"
            )));
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
            !approved_mcp.contains(*name) && server.get("enabled") != Some(&Value::Bool(false))
        })
        .map(|(name, _)| name.as_str())
        .collect();
    if !unapproved.is_empty() {
        return Err(refuse(format!(
            "OpenCode would start MCP servers you did not approve: {}",
            unapproved.join(", ")
        )));
    }
    Ok(())
}

/// `opencode debug agent build`: OpenCode's own agent, with no prompt from
/// any config.
pub fn verify_default_agent(text: &str) -> Result<()> {
    let agent: Value = serde_json::from_str(text)
        .map_err(|_| refuse("`opencode debug agent` did not print JSON"))?;
    if agent.get("name").and_then(Value::as_str) != Some(DEFAULT_AGENT) {
        return Err(refuse(format!(
            "OpenCode did not resolve its own {DEFAULT_AGENT:?} agent"
        )));
    }
    if agent.get("native").and_then(Value::as_bool) != Some(true) {
        return Err(refuse(format!(
            "the {DEFAULT_AGENT:?} agent comes from your configuration, not OpenCode"
        )));
    }
    if non_empty(&agent, "prompt") {
        return Err(refuse(format!(
            "the {DEFAULT_AGENT:?} agent carries a prompt from your configuration, which you \
             did not approve"
        )));
    }
    Ok(())
}

/// Files in OpenCode's plugin folders under `config_dir`
/// (`~/.config/opencode`). Any one refuses a challenge.
pub fn plugin_files(config_dir: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    for name in ["plugin", "plugins"] {
        let Ok(entries) = std::fs::read_dir(config_dir.join(name)) else {
            continue;
        };
        found.extend(entries.flatten().map(|entry| entry.path()));
    }
    found.sort();
    found
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

    #[test]
    fn the_visit_config_is_not_a_clean_challenge_config() {
        // The visit seal's config carries the daycare server and agent.
        let visit = fixture("debug-config.json");
        let error = verify_challenge_config(&visit, &BTreeSet::new())
            .unwrap_err()
            .to_string();
        assert!(error.contains("agents"), "{error}");

        let mut config: Value = serde_json::from_str(&visit).unwrap();
        config.as_object_mut().unwrap().remove("agent");
        let error = verify_challenge_config(&config.to_string(), &BTreeSet::new())
            .unwrap_err()
            .to_string();
        assert!(error.contains("did not approve: daycare"), "{error}");
        // Approved, or switched off, the same server passes.
        let approved = BTreeSet::from(["daycare".to_string()]);
        verify_challenge_config(&config.to_string(), &approved).unwrap();
        config["mcp"]["daycare"]["enabled"] = Value::Bool(false);
        verify_challenge_config(&config.to_string(), &BTreeSet::new()).unwrap();

        for (key, value) in [
            ("plugin", serde_json::json!(["opencode-wakatime"])),
            ("instructions", serde_json::json!(["~/notes.md"])),
            ("default_agent", serde_json::json!("plan-mine")),
        ] {
            let mut bad = config.clone();
            bad[key] = value;
            assert!(
                verify_challenge_config(&bad.to_string(), &BTreeSet::new()).is_err(),
                "{key}"
            );
        }
    }

    #[test]
    fn only_opencodes_own_build_agent_passes() {
        let build = fixture("debug-agent-build.json");
        verify_default_agent(&build).unwrap();
        let mut agent: Value = serde_json::from_str(&build).unwrap();
        agent["prompt"] = Value::String("Always use my house style.".into());
        assert!(verify_default_agent(&agent.to_string()).is_err());
        let mut agent: Value = serde_json::from_str(&build).unwrap();
        agent["native"] = Value::Bool(false);
        assert!(verify_default_agent(&agent.to_string()).is_err());
        // The visit's own agent is not OpenCode's default.
        assert!(verify_default_agent(&fixture("debug-agent.json")).is_err());
    }

    #[test]
    fn any_file_in_a_plugin_folder_is_found() {
        let base = crate::testdir::unique_dir("daycare-opencode-plugins");
        std::fs::create_dir_all(base.join("plugin")).unwrap();
        assert!(plugin_files(&base).is_empty());
        std::fs::write(base.join("plugin/notify.ts"), "export default {}").unwrap();
        assert_eq!(plugin_files(&base), [base.join("plugin/notify.ts")]);
        std::fs::remove_dir_all(base).ok();
    }
}
