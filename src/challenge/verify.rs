//! Verification before an interactive challenge launch (plan task N10).
//!
//! **Codex.** `codex debug prompt-input` renders the model-visible input for a
//! configuration without calling a model, as the visit preflight uses it
//! (`agent::codex::preflight`). It runs here with the session's own switches,
//! environment and folder, so what is checked is what would launch. Every
//! message part carries its kind; the visit seal allows only Daycare's persona
//! and the user message, and a challenge widens that to:
//!
//! - Codex's own words about its sandbox, environment and collaboration tools
//!   (`CODEX_OWN_KINDS`), which are not the owner's memory;
//! - AGENTS.md text, but only text that matches, by hash, the folder's own
//!   AGENTS.md (which Daycare wrote) or an AGENTS.md the owner approved;
//! - a skill list, but only skills the owner approved;
//! - an empty `developer_instructions` (the launch sets it to "");
//! - from Codex 0.165, an `additional_tools` item declaring Codex's own tool
//!   namespaces (`CODEX_OWN_TOOL_NAMESPACES`), and Codex's base instructions,
//!   but only when the owner's configuration has no key that replaces them
//!   (`BASE_INSTRUCTION_KEYS`). Those keys put the owner's text in that part
//!   under the same kind and provenance as Codex's own, so the configuration
//!   is what is checked, and a key anywhere in it refuses the launch.
//!
//! Anything else refuses the launch and says what it was: an unknown kind, an
//! unapproved skill, AGENTS.md text that matches nothing approved (the likely
//! files are named, never quoted), apps or plugins that should be off.
//!
//! What this does not cover: MCP servers are tools, not prompt text, so an
//! unapproved server is left out by configuration and shows only in Codex's
//! `/mcp` once it starts.
//!
//! **Claude Code.** There is no command that shows what a session will load
//! before it starts. The owner checks `/status` and `/memory` (and `/mcp`)
//! themselves; the runner prints exactly what to look for.
//!
//! The challenge fixtures, `tests/fixtures/codex-prompt-input-challenge*.json`,
//! are synthetic: the part kinds and texts of the 0.158 visit captures beside
//! them, with `{{FOLDER}}` and `{{AGENTS_MD}}` filled in at test time.
//! `codex-prompt-input-challenge-0165.json` is the shape of a real challenge
//! render from the dry run (N23), on Codex 0.165.0-alpha.1.2 as the ChatGPT
//! desktop app ships it: every item, kind, provenance and tool name as Codex
//! rendered them, with long texts shortened and paths replaced. An unknown
//! kind or tool namespace is still a refusal, not a pass.

use super::exclusion::LaunchSpec;
use super::launch::base_command;
use crate::agent::codex::launch::{check_version, toml_string};
use crate::self_update::sha256_hex;
use crate::{Error, Result};
use serde_json::Value;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Stdio;

/// The user message the prompt is rendered with.
pub const PROBE_MESSAGE: &str = "DAYCARE-CHALLENGE-PROBE";

/// Codex's own words about its sandbox, environment and collaboration tools.
/// They describe Codex, not the owner.
pub const CODEX_OWN_KINDS: [&str; 6] = [
    "permissions.instructions",
    "environments.environment_context",
    "multi_agent.usage_hint",
    "multi_agent.mode_instructions",
    "multi_agent.role_instructions",
    "collaboration_mode.instructions",
];

/// The tool namespaces Codex declares for itself in an `additional_tools`
/// item (0.165): `functions` (its code-mode `exec`, which wraps the shell and
/// patch tools, plus waiting and asking the user), a clock, and its sub-agent
/// tools. `debug prompt-input` starts no MCP server, so MCP tools never show
/// here; apps and plugins are switched off by the launch.
pub const CODEX_OWN_TOOL_NAMESPACES: [&str; 3] = ["functions", "clock", "collaboration"];

/// Codex configuration keys that replace Codex's own base instructions: a
/// file of the owner's text, its older name, and a model catalog, which
/// carries base instructions per model.
pub const BASE_INSTRUCTION_KEYS: [&str; 3] = [
    "model_instructions_file",
    "experimental_instructions_file",
    "model_catalog_json",
];

/// Codex joins the global AGENTS.md and the project ones with this line.
const PROJECT_DOC_SEPARATOR: &str = "--- project-doc ---";

/// Instruction text the prompt may hold, known by the hash of its trimmed
/// bytes (Codex trims what it reads).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovedText {
    /// Where it came from, for the proof and for messages.
    pub source: String,
    pub sha256: String,
    pub len: usize,
}

impl ApprovedText {
    pub fn new(source: impl Into<String>, text: &str) -> Self {
        let trimmed = text.trim();
        ApprovedText {
            source: source.into(),
            sha256: sha256_hex(trimmed.as_bytes()),
            len: trimmed.len(),
        }
    }
}

/// What a challenge's Codex prompt may hold.
#[derive(Debug, Clone, Default)]
pub struct CodexAllowed {
    pub instructions: Vec<ApprovedText>,
    /// Names of the Codex skills the owner approved.
    pub skills: BTreeSet<String>,
    /// The challenge folder, which must be Codex's working folder.
    pub folder: PathBuf,
    /// The owner's AGENTS.md files that were not approved: named as the
    /// likely source when unmatched text appears, never quoted.
    pub unapproved_instructions: Vec<PathBuf>,
    /// Keys in the Codex configuration this session reads that would replace
    /// Codex's base instructions, as `key in path`. Empty for a separate
    /// Codex home, whose configuration Daycare wrote.
    pub base_instruction_overrides: Vec<String>,
}

/// The checks the owner makes once Claude Code starts.
pub fn claude_manual_checks() -> Vec<String> {
    vec![
        "Run /status and /memory. The only CLAUDE.md listed should be this folder's, written \
         by Daycare, and no memory file you did not approve should be listed."
            .into(),
        "Run /mcp. Only the MCP servers you approved should be there.".into(),
        "If anything else is listed, quit (Ctrl-D) before you start the work.".into(),
    ]
}

/// The one check the prompt cannot cover for Codex.
pub fn codex_manual_checks() -> Vec<String> {
    vec![
        "Run /mcp once Codex starts. Only the MCP servers you approved should be there; quit \
         before you start the work if anything else is."
            .into(),
    ]
}

/// Codex CLI 0.158.0 or newer: `--disable` must know every feature name.
pub fn check_codex_version(launch: &LaunchSpec) -> Result<()> {
    let output = base_command(launch)
        .arg("--version")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .map_err(|error| {
            Error::new(format!(
                "could not start {} to check its version: {error}. Is Codex CLI installed and on PATH?",
                launch.program
            ))
        })?;
    if !output.status.success() {
        return Err(Error::new(
            "could not read Codex CLI's version; challenges need 0.158.0 or newer",
        ));
    }
    check_version(&String::from_utf8_lossy(&output.stdout))
}

/// Render the session's prompt and check it. Returns what was proven.
pub fn prove_codex_prompt(
    launch: &LaunchSpec,
    config_args: &[String],
    allowed: &CodexAllowed,
) -> Result<Vec<String>> {
    let output = base_command(launch)
        .args(["debug", "prompt-input"])
        .args(config_args)
        .args([
            "-c".to_string(),
            format!("sandbox_mode={}", toml_string("workspace-write")),
        ])
        .arg(PROBE_MESSAGE)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|error| {
            Error::new(format!(
                "could not start {} to check what Codex will load: {error}",
                launch.program
            ))
        })?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let last = stderr
            .lines()
            .rev()
            .find(|line| !line.trim().is_empty() && !line.starts_with("WARNING"))
            .unwrap_or("")
            .trim()
            .to_string();
        return Err(Error::new(format!(
            "codex debug prompt-input failed ({}){}{last}; Daycare cannot check what Codex \
             would load, so it did not start it",
            output.status,
            if last.is_empty() { "" } else { ": " }
        )));
    }
    check_codex_prompt(&String::from_utf8_lossy(&output.stdout), allowed)
}

fn refuse(what: impl std::fmt::Display) -> Error {
    Error::new(format!(
        "Daycare did not start Codex: {what}. Nothing was launched; the challenge folder is \
         set up, so run `challenge start` again once this is fixed"
    ))
}

/// The allow-list over one `prompt-input` rendering. Returns one line per
/// thing proven.
pub fn check_codex_prompt(rendered: &str, allowed: &CodexAllowed) -> Result<Vec<String>> {
    let items: Value = serde_json::from_str(rendered)
        .map_err(|error| refuse(format!("its prompt-input is not JSON ({error})")))?;
    let items = items
        .as_array()
        .ok_or_else(|| refuse("its prompt-input is not a list"))?;
    if !allowed.base_instruction_overrides.is_empty() {
        return Err(refuse(format!(
            "your Codex configuration replaces Codex's own base instructions ({}), and Daycare \
             cannot tell that text from your own memory. Remove it for the challenge, or start \
             with --separate-codex-home, whose configuration holds only what you approved",
            allowed.base_instruction_overrides.join("; ")
        )));
    }
    let mut proven: Vec<String> = Vec::new();
    let mut user_parts = 0;
    for item in items {
        let role = item.get("role").and_then(Value::as_str).unwrap_or("");
        if item.get("type").and_then(Value::as_str) == Some("additional_tools") {
            let namespaces = check_codex_tools(item)?;
            let line = format!(
                "every tool the prompt declares is Codex's own ({})",
                namespaces.join(", ")
            );
            if !proven.contains(&line) {
                proven.push(line);
            }
            continue;
        }
        if item.get("type").and_then(Value::as_str) != Some("message") {
            return Err(refuse(format!(
                "its prompt holds a {} item, and only messages were expected",
                item.get("type")
                    .and_then(Value::as_str)
                    .unwrap_or("typeless")
            )));
        }
        let parts = item
            .get("content")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let kinds: Vec<&str> = item
            .get("internal_chat_message_metadata_passthrough")
            .and_then(|meta| meta.get("content_item_kinds"))
            .and_then(Value::as_array)
            .map(|kinds| kinds.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        if kinds.len() != parts.len() {
            return Err(refuse(format!(
                "a {role} message in its prompt does not say what its parts are"
            )));
        }
        for (kind, part) in kinds.iter().zip(&parts) {
            let text = part.get("text").and_then(Value::as_str).unwrap_or("");
            match *kind {
                "user.text" => {
                    user_parts += 1;
                    if text != PROBE_MESSAGE {
                        return Err(refuse("Codex added text to the user's message"));
                    }
                }
                "generic.developer_instructions" => {
                    if !text.trim().is_empty() {
                        return Err(refuse(
                            "developer instructions from your Codex configuration reached the \
                             prompt, and you did not approve them",
                        ));
                    }
                }
                "agents_md.instructions" => {
                    for source in match_agents_md(text, allowed)? {
                        let line = format!("the AGENTS.md text in the prompt is {source}");
                        if !proven.contains(&line) {
                            proven.push(line);
                        }
                    }
                }
                "host_skills.instructions" => {
                    let listed = listed_skills(text);
                    if listed.is_empty() {
                        return Err(refuse(
                            "its prompt lists skills in a form Daycare cannot read, so it cannot \
                             tell whether you approved them",
                        ));
                    }
                    let unapproved: Vec<&str> = listed
                        .iter()
                        .map(String::as_str)
                        .filter(|name| !allowed.skills.contains(*name))
                        .collect();
                    if !unapproved.is_empty() {
                        return Err(refuse(format!(
                            "its prompt lists skills you did not approve: {}. Approve them with \
                             the daycare-memory-review skill or move them aside for the \
                             challenge",
                            unapproved.join(", ")
                        )));
                    }
                    proven.push(format!(
                        "every skill in the prompt is one you approved ({})",
                        listed.join(", ")
                    ));
                }
                "environments.environment_context" => check_cwd(text, &allowed.folder)?,
                // Safe only because no key that replaces them is set (checked above).
                "model.base_instructions" => {
                    let line = "Codex's base instructions are its own: your Codex configuration \
                                does not replace them"
                        .to_string();
                    if !proven.contains(&line) {
                        proven.push(line);
                    }
                }
                kind if CODEX_OWN_KINDS.contains(&kind) => {}
                "apps.instructions" => {
                    return Err(refuse(
                        "Codex's apps (connectors) reached the prompt, and they should be off \
                         in a challenge",
                    ))
                }
                "plugins.recommendations" => {
                    return Err(refuse(
                        "Codex's plugin list reached the prompt, and plugins should be off in a \
                         challenge",
                    ))
                }
                other => {
                    return Err(refuse(format!(
                        "its prompt holds {other} content, which Daycare cannot match to \
                         anything you approved"
                    )))
                }
            }
        }
    }
    if user_parts != 1 {
        return Err(refuse(format!(
            "its prompt held {user_parts} user messages; expected exactly one"
        )));
    }
    proven.insert(
        0,
        "Codex's rendered prompt holds nothing you did not approve (codex debug prompt-input)"
            .into(),
    );
    Ok(proven)
}

/// An `additional_tools` item: every entry must be one of Codex's own tool
/// namespaces. Returns the namespaces it declared.
fn check_codex_tools(item: &Value) -> Result<Vec<String>> {
    let tools = item
        .get("tools")
        .and_then(Value::as_array)
        .ok_or_else(|| refuse("its prompt declares tools in a form Daycare cannot read"))?;
    let mut namespaces = Vec::new();
    for tool in tools {
        let kind = tool
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("typeless");
        let name = tool
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("unnamed");
        if kind != "namespace" || !CODEX_OWN_TOOL_NAMESPACES.contains(&name) {
            return Err(refuse(format!(
                "its prompt declares tools Daycare does not know as Codex's own ({kind} {name})"
            )));
        }
        if !namespaces.iter().any(|known| known == name) {
            namespaces.push(name.to_string());
        }
    }
    Ok(namespaces)
}

/// The keys in `config.toml` text that would replace Codex's base
/// instructions (`BASE_INSTRUCTION_KEYS`). Deliberately coarse: an assignment
/// to one of those names anywhere outside a comment counts, at the root, in a
/// profile, as a dotted key or inside an inline table, so no layout hides one.
/// Text that merely looks like one inside a multi-line string also counts,
/// which refuses rather than lets a real one through.
pub fn base_instruction_overrides(config: &str) -> Vec<&'static str> {
    let mut found = Vec::new();
    for line in config.lines() {
        let line = line.trim_start();
        if line.starts_with('#') {
            continue;
        }
        for key in BASE_INSTRUCTION_KEYS {
            let assigned = line.match_indices(key).any(|(at, _)| {
                let before = line[..at].chars().next_back();
                let after = line[at + key.len()..].trim_start_matches(['"', '\'']);
                before.is_none_or(|c| !(c.is_ascii_alphanumeric() || c == '_' || c == '-'))
                    && after.trim_start().starts_with('=')
            });
            if assigned && !found.contains(&key) {
                found.push(key);
            }
        }
    }
    found
}

/// Match the text inside one `agents_md.instructions` part, piece by piece,
/// against the allowed texts by hash. Returns the sources it matched.
fn match_agents_md(text: &str, allowed: &CodexAllowed) -> Result<Vec<String>> {
    let unreadable = || refuse("its prompt holds AGENTS.md text in a form Daycare cannot read");
    let start = text.find("<INSTRUCTIONS>").ok_or_else(unreadable)? + "<INSTRUCTIONS>".len();
    let end = text
        .rfind("</INSTRUCTIONS>")
        .filter(|end| *end >= start)
        .ok_or_else(unreadable)?;
    let mut rest = text[start..end].trim();
    let mut matched = Vec::new();
    loop {
        rest = rest.trim_start();
        if let Some(after) = rest.strip_prefix(PROJECT_DOC_SEPARATOR) {
            rest = after;
            continue;
        }
        if rest.is_empty() {
            break;
        }
        let found = allowed
            .instructions
            .iter()
            .filter(|candidate| {
                candidate.len > 0
                    && rest.len() >= candidate.len
                    && rest.is_char_boundary(candidate.len)
                    && rest[candidate.len..]
                        .chars()
                        .next()
                        .is_none_or(char::is_whitespace)
                    && sha256_hex(rest[..candidate.len].as_bytes()) == candidate.sha256
            })
            .max_by_key(|candidate| candidate.len);
        let Some(found) = found else {
            let likely = if allowed.unapproved_instructions.is_empty() {
                String::new()
            } else {
                format!(
                    " (likely from {}, which you did not approve)",
                    allowed
                        .unapproved_instructions
                        .iter()
                        .map(|path| path.display().to_string())
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            };
            return Err(refuse(format!(
                "its prompt holds AGENTS.md text that matches nothing you approved{likely}"
            )));
        };
        if !matched.contains(&found.source) {
            matched.push(found.source.clone());
        }
        rest = &rest[found.len..];
    }
    Ok(matched)
}

/// Skill names in a rendered skill list: the `- name: …` lines under
/// `### Available skills`.
fn listed_skills(text: &str) -> Vec<String> {
    let mut names = Vec::new();
    let mut in_list = false;
    for line in text.lines() {
        let trimmed = line.trim();
        if let Some(heading) = trimmed.strip_prefix("### ") {
            in_list = heading.trim().eq_ignore_ascii_case("available skills");
            continue;
        }
        if !in_list {
            continue;
        }
        if let Some(entry) = trimmed.strip_prefix("- ") {
            let name = entry
                .split(':')
                .next()
                .unwrap_or("")
                .trim()
                .trim_matches('`');
            if !name.is_empty() && !names.iter().any(|known| known == name) {
                names.push(name.to_string());
            }
        }
    }
    names
}

/// The environment part names the working folder; it must be this one.
fn check_cwd(text: &str, folder: &Path) -> Result<()> {
    let Some(start) = text.find("<cwd>") else {
        return Ok(());
    };
    let start = start + "<cwd>".len();
    let Some(length) = text[start..].find("</cwd>") else {
        return Ok(());
    };
    let shown = PathBuf::from(text[start..start + length].trim());
    let physical = |path: &Path| std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    if shown == folder || physical(&shown) == physical(folder) {
        return Ok(());
    }
    Err(refuse(format!(
        "Codex would start in {}, not the challenge folder {}",
        shown.display(),
        folder.display()
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    const CLEAN: &str = include_str!("../../tests/fixtures/codex-prompt-input-challenge.json");
    const WITH_SKILL: &str =
        include_str!("../../tests/fixtures/codex-prompt-input-challenge-skill.json");
    const CODEX_0165: &str =
        include_str!("../../tests/fixtures/codex-prompt-input-challenge-0165.json");

    const POINTER: &str = "# Daycare challenge: Signup Rescue\n\nRead `BRIEF.md` first.\n";

    /// Fill a fixture's placeholders the way the fake codex does.
    fn render(fixture: &str, folder: &str, agents_md: &str) -> String {
        let mut value: Value = serde_json::from_str(fixture).unwrap();
        fn walk(value: &mut Value, folder: &str, agents_md: &str) {
            match value {
                Value::String(text) => {
                    *text = text
                        .replace("{{FOLDER}}", folder)
                        .replace("{{AGENTS_MD}}", agents_md.trim());
                }
                Value::Array(items) => items.iter_mut().for_each(|v| walk(v, folder, agents_md)),
                Value::Object(map) => map.values_mut().for_each(|v| walk(v, folder, agents_md)),
                _ => {}
            }
        }
        walk(&mut value, folder, agents_md);
        value.to_string()
    }

    fn allowed() -> CodexAllowed {
        CodexAllowed {
            instructions: vec![ApprovedText::new("the folder's AGENTS.md", POINTER)],
            skills: BTreeSet::new(),
            folder: PathBuf::from("/h/daycare/challenges/c1"),
            unapproved_instructions: vec![PathBuf::from("/h/.codex/AGENTS.md")],
            base_instruction_overrides: Vec::new(),
        }
    }

    #[test]
    fn a_codex_0165_prompt_passes_with_its_own_tools_and_base_instructions() {
        let rendered = render(CODEX_0165, "/h/daycare/challenges/c1", POINTER);
        let proven = check_codex_prompt(&rendered, &allowed()).unwrap();
        for wanted in [
            "the folder's AGENTS.md",
            "every tool the prompt declares is Codex's own (functions, clock, collaboration)",
            "Codex's base instructions are its own",
        ] {
            assert!(
                proven.iter().any(|line| line.contains(wanted)),
                "{wanted}: {proven:?}"
            );
        }
    }

    #[test]
    fn a_key_that_replaces_base_instructions_refuses_and_names_it() {
        let rendered = render(CODEX_0165, "/h/daycare/challenges/c1", POINTER);
        let mut allowed = allowed();
        allowed.base_instruction_overrides =
            vec!["model_instructions_file in /h/.codex/config.toml".into()];
        let error = check_codex_prompt(&rendered, &allowed)
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("model_instructions_file in /h/.codex/config.toml")
                && error.contains("--separate-codex-home"),
            "{error}"
        );
        // The older shape shows no base instructions at all; the key still refuses.
        let older = render(CLEAN, "/h/daycare/challenges/c1", POINTER);
        assert!(check_codex_prompt(&older, &allowed).is_err());
    }

    #[test]
    fn a_tool_namespace_that_is_not_codexs_own_refuses_by_name() {
        let mut value: Value =
            serde_json::from_str(&render(CODEX_0165, "/h/daycare/challenges/c1", POINTER)).unwrap();
        let tools = value[0]["tools"].as_array_mut().unwrap();
        tools.push(serde_json::json!({"type": "namespace", "name": "node_repl", "tools": []}));
        let error = check_codex_prompt(&value.to_string(), &allowed())
            .unwrap_err()
            .to_string();
        assert!(error.contains("namespace node_repl"), "{error}");

        value[0]["tools"] = serde_json::json!([{"type": "web_search"}]);
        let error = check_codex_prompt(&value.to_string(), &allowed())
            .unwrap_err()
            .to_string();
        assert!(error.contains("web_search unnamed"), "{error}");
    }

    #[test]
    fn base_instruction_keys_are_found_in_every_layout() {
        for (config, wanted) in [
            (
                "model_instructions_file = \"/x.md\"\n",
                vec!["model_instructions_file"],
            ),
            (
                "[profiles.work]\nexperimental_instructions_file=\"/x.md\"\n",
                vec!["experimental_instructions_file"],
            ),
            (
                "profiles.work.model_catalog_json = \"/c.json\"\n",
                vec!["model_catalog_json"],
            ),
            (
                "[profiles]\nwork = { model = \"m\", model_instructions_file = \"/x.md\" }\n",
                vec!["model_instructions_file"],
            ),
            (
                "\"model_instructions_file\" = \"/x.md\"\n",
                vec!["model_instructions_file"],
            ),
            (
                "  model_instructions_file = 'a'\nmodel_catalog_json='b'\n",
                vec!["model_instructions_file", "model_catalog_json"],
            ),
        ] {
            assert_eq!(base_instruction_overrides(config), wanted, "{config}");
        }
        for config in [
            "# model_instructions_file = \"/x.md\"\n",
            "model = \"gpt\"\nnotes = \"model_instructions_file is unset\"\n",
            "my_model_instructions_file = \"/x.md\"\n",
            "model_instructions_files = 1\n",
            "",
        ] {
            assert!(base_instruction_overrides(config).is_empty(), "{config}");
        }
    }

    #[test]
    fn a_clean_challenge_prompt_passes_and_says_what_matched() {
        let rendered = render(CLEAN, "/h/daycare/challenges/c1", POINTER);
        let proven = check_codex_prompt(&rendered, &allowed()).unwrap();
        assert!(
            proven
                .iter()
                .any(|line| line.contains("the folder's AGENTS.md")),
            "{proven:?}"
        );
    }

    #[test]
    fn an_unapproved_skill_fails_verify_by_name() {
        let rendered = render(WITH_SKILL, "/h/daycare/challenges/c1", POINTER);
        let error = check_codex_prompt(&rendered, &allowed())
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("skills you did not approve: notes"),
            "{error}"
        );
        // Approved, the same prompt passes.
        let mut approved = allowed();
        approved.skills.insert("notes".into());
        let proven = check_codex_prompt(&rendered, &approved).unwrap();
        assert!(
            proven.iter().any(|line| line.contains("(notes)")),
            "{proven:?}"
        );
    }

    #[test]
    fn agents_md_text_must_match_an_approved_hash() {
        // The global AGENTS.md joined to the folder's, as Codex joins them.
        let global = "GLOBAL-AGENTS-PROBE";
        let joined = format!("{global}\n\n--- project-doc ---\n\n{POINTER}");
        let rendered = render(CLEAN, "/h/daycare/challenges/c1", &joined);
        let error = check_codex_prompt(&rendered, &allowed())
            .unwrap_err()
            .to_string();
        assert!(error.contains("matches nothing you approved"), "{error}");
        assert!(error.contains("/h/.codex/AGENTS.md"), "{error}");
        assert!(
            !error.contains(global),
            "the refusal quoted memory: {error}"
        );

        let mut approved = allowed();
        approved.instructions.push(ApprovedText::new(
            "/h/.codex/AGENTS.md, which you approved",
            "GLOBAL-AGENTS-PROBE\n",
        ));
        let proven = check_codex_prompt(&rendered, &approved).unwrap();
        assert!(
            proven
                .iter()
                .any(|line| line.contains("which you approved")),
            "{proven:?}"
        );

        // A prefix of an approved text is not a match for a longer one.
        let longer = format!("{POINTER}\nand one more line nobody approved");
        let rendered = render(CLEAN, "/h/daycare/challenges/c1", &longer);
        assert!(check_codex_prompt(&rendered, &allowed()).is_err());
    }

    #[test]
    fn foreign_kinds_developer_text_and_the_wrong_folder_fail() {
        let rendered = render(CLEAN, "/h/daycare/challenges/c1", POINTER);
        let mut items: Value = serde_json::from_str(&rendered).unwrap();
        items[0]["internal_chat_message_metadata_passthrough"]["content_item_kinds"] =
            serde_json::json!(["apps.instructions"]);
        let error = check_codex_prompt(&items.to_string(), &allowed())
            .unwrap_err()
            .to_string();
        assert!(error.contains("apps"), "{error}");

        let mut items: Value = serde_json::from_str(&rendered).unwrap();
        items[0]["internal_chat_message_metadata_passthrough"]["content_item_kinds"] =
            serde_json::json!(["memories.instructions"]);
        let error = check_codex_prompt(&items.to_string(), &allowed())
            .unwrap_err()
            .to_string();
        assert!(error.contains("memories.instructions"), "{error}");

        let mut items: Value = serde_json::from_str(&rendered).unwrap();
        items[0]["internal_chat_message_metadata_passthrough"]["content_item_kinds"] =
            serde_json::json!(["generic.developer_instructions"]);
        let error = check_codex_prompt(&items.to_string(), &allowed())
            .unwrap_err()
            .to_string();
        assert!(error.contains("developer instructions"), "{error}");

        let elsewhere = render(CLEAN, "/somewhere/else", POINTER);
        let error = check_codex_prompt(&elsewhere, &allowed())
            .unwrap_err()
            .to_string();
        assert!(error.contains("/somewhere/else"), "{error}");
    }

    #[test]
    fn the_visit_captures_are_judged_too() {
        // The leaky visit capture holds skills, AGENTS.md, apps and plugins.
        let leaky = include_str!("../../tests/fixtures/codex-prompt-input-leaky.json")
            .replace("PROBE-USER-MESSAGE", PROBE_MESSAGE);
        assert!(check_codex_prompt(&leaky, &allowed()).is_err());
        // The sealed one holds a non-empty persona, which a challenge never sets.
        let sealed = include_str!("../../tests/fixtures/codex-prompt-input-sealed.json")
            .replace("PROBE-USER-MESSAGE", PROBE_MESSAGE);
        let error = check_codex_prompt(&sealed, &allowed())
            .unwrap_err()
            .to_string();
        assert!(error.contains("developer instructions"), "{error}");
    }

    #[test]
    fn skill_lists_are_read_from_either_rendering() {
        let text = "<skills_instructions>\n## Skills\n### Skill roots\n- `r0` = `/h/.agents/skills`\n### Available skills\n- notes: Take notes (file: r0/notes/SKILL.md)\n- `review`: Review it\n### How to use skills\n- Use them.\n</skills_instructions>";
        assert_eq!(listed_skills(text), ["notes", "review"]);
    }
}
