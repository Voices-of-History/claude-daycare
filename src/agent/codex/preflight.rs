//! Proof, before a turn, that the prompt Codex will build holds nothing but
//! Daycare's own words.
//!
//! `codex debug prompt-input` renders the model-visible input list for a
//! config without calling a model. It is run with the turn's own seal flags,
//! persona, and homes, in the turn's cwd. Every message part carries its
//! kind in `internal_chat_message_metadata_passthrough.content_item_kinds`;
//! the proof is an allow-list over those kinds plus exact text for the parts
//! Daycare wrote.
//!
//! Sealed, codex-cli 0.158 renders two parts
//! (`tests/fixtures/codex-prompt-input-sealed.json`): the persona
//! (`generic.developer_instructions`) and the user message (`user.text`).
//! Multi-agent blocks are forbidden. Unsealed, the owner's skills, AGENTS.md,
//! plugin list, permissions, and environment appear
//! (`tests/fixtures/codex-prompt-input-leaky.json`).
//!
//! From 0.165, a code-mode model's render adds two things
//! (`tests/fixtures/codex-prompt-input-sealed-0166.json`), and each is checked
//! exactly:
//! - an `additional_tools` item: the `functions` namespace with only `exec`
//!   and `wait`, where `exec` nests only the three MCP resource helpers and
//!   Daycare's own tools and still describes its isolate as having no file
//!   system and no network;
//! - Codex's base instructions (`model.base_instructions`), which must be
//!   exactly the text of the runner's private model catalog. That catalog is
//!   Codex's bundled one, read with the sealed homes, so the owner's
//!   configuration cannot change it.

use super::appserver::CodexCommand;
use crate::{Error, Result};
use serde_json::Value;
use std::process::Stdio;

pub const PROBE_MESSAGE: &str = "DAYCARE-SEAL-PROBE";

/// Only Daycare's persona and the user message may reach the model, plus a
/// code-mode model's base instructions when they match the private catalog.
const ALLOWED_KINDS: [&str; 3] = [
    "generic.developer_instructions",
    "user.text",
    "model.base_instructions",
];

/// The only tools a code-mode model may be declared (0.165 on): `exec`,
/// which runs a script in the isolate and nests the turn's tools, and `wait`.
const CODE_MODE_TOOLS: [&str; 2] = ["exec", "wait"];

/// What `exec` may nest besides Daycare's own tools: the MCP resource helpers
/// Codex adds whenever a server is configured. They reach only the Daycare
/// server, and the post-turn checks reject their use.
const RESOURCE_HELPERS: [&str; 3] = [
    "list_mcp_resource_templates",
    "list_mcp_resources",
    "read_mcp_resource",
];

/// Daycare's own MCP tools, as code mode names them.
const DAYCARE_TOOL_PREFIX: &str = "mcp__daycare__";

/// The isolate's own description of what it lacks. If Codex stops saying so,
/// the isolate is unverified and the turn does not start.
const ISOLATE_STATEMENT: &str = "no file system, no network access";

/// Run the proof. `seal_args` are the turn's own features, settings, model,
/// and persona (`launch::seal_args`); `base_instructions` is the private
/// catalog's text for the model (`catalog::base_instructions`).
pub fn prove_sealed_prompt(
    codex: &CodexCommand,
    seal_args: &[String],
    developer_instructions: &str,
    base_instructions: Option<&str>,
) -> Result<()> {
    let output = codex
        .command()
        .args(["debug", "prompt-input"])
        .args(seal_args)
        .arg(PROBE_MESSAGE)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|error| {
            Error::new(format!(
                "could not start {} for the seal check: {error}. Is Codex CLI installed and on PATH?",
                codex.program
            ))
        })?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let last = stderr
            .lines()
            .rev()
            .find(|line| !line.trim().is_empty() && !line.starts_with("WARNING"))
            .unwrap_or("")
            .trim();
        return Err(Error::new(format!(
            "codex debug prompt-input failed ({}){}{last}; cannot prove the seal",
            output.status,
            if last.is_empty() { "" } else { ": " }
        )));
    }
    let rendered = String::from_utf8_lossy(&output.stdout);
    check_prompt_input(&rendered, developer_instructions, base_instructions)
}

/// A code-mode model's `additional_tools` item: `functions` with only `exec`
/// and `wait`, and `exec` nesting only the resource helpers and Daycare's
/// tools, inside the isolate it describes.
fn check_code_mode_tools(item: &Value) -> Result<()> {
    let refuse = |what: String| {
        Err(Error::new(format!(
            "the Codex prompt declares {what}; only code mode's exec and wait may be there"
        )))
    };
    let Some(declared) = item.get("tools").and_then(Value::as_array) else {
        return refuse("tools in a form Daycare cannot read".into());
    };
    for namespace in declared {
        let kind = namespace
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("typeless");
        let name = namespace
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("unnamed");
        if kind != "namespace" || name != "functions" {
            return refuse(format!("{kind} {name}"));
        }
        for tool in namespace
            .get("tools")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or_default()
        {
            let tool_name = tool
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("unnamed");
            if !CODE_MODE_TOOLS.contains(&tool_name) {
                return refuse(format!("the tool {tool_name}"));
            }
            if tool_name != "exec" {
                continue;
            }
            let description = tool
                .get("description")
                .and_then(Value::as_str)
                .unwrap_or("");
            if !description.contains(ISOLATE_STATEMENT) {
                return refuse(
                    "an exec tool that no longer says its isolate has no file system or network"
                        .into(),
                );
            }
            for nested in nested_tools(description) {
                if !RESOURCE_HELPERS.contains(&nested) && !nested.starts_with(DAYCARE_TOOL_PREFIX) {
                    return refuse(format!("{nested} inside exec"));
                }
            }
        }
    }
    Ok(())
}

/// The tools `exec` nests: the `### \`name\`` headings of its description.
fn nested_tools(description: &str) -> Vec<&str> {
    description
        .lines()
        .filter_map(|line| line.strip_prefix("### `"))
        .filter_map(|rest| rest.split_once('`').map(|(name, _)| name))
        .collect()
}

/// The allow-list over one `prompt-input` rendering.
pub fn check_prompt_input(
    rendered: &str,
    developer_instructions: &str,
    base_instructions: Option<&str>,
) -> Result<()> {
    let items: Value = serde_json::from_str(rendered)
        .map_err(|error| Error::new(format!("codex prompt-input is not JSON: {error}")))?;
    let items = items
        .as_array()
        .ok_or_else(|| Error::new("codex prompt-input is not a list"))?;

    // Named first because they are the leaks seen in the wild: skills come
    // through HOME whatever CODEX_HOME says.
    if rendered.contains("<skills_instructions>")
        || rendered.contains("Skill roots")
        || rendered.contains("host_skills.")
    {
        return Err(Error::new(
            "the owner's skills reached the prompt (skill roots are listed); the Codex seal is broken",
        ));
    }
    if rendered.contains("agents_md.") {
        return Err(Error::new(
            "an AGENTS.md reached the prompt; the Codex seal is broken",
        ));
    }

    let mut persona_parts = 0;
    let mut user_parts = 0;
    for item in items {
        let role = item.get("role").and_then(Value::as_str).unwrap_or("");
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
        if item.get("type").and_then(Value::as_str) == Some("additional_tools") {
            check_code_mode_tools(item)?;
            continue;
        }
        if item.get("type").and_then(Value::as_str) != Some("message") {
            return Err(Error::new(format!(
                "the Codex prompt holds a {} item; only messages were expected",
                item.get("type")
                    .and_then(Value::as_str)
                    .unwrap_or("typeless")
            )));
        }
        if kinds.len() != parts.len() {
            return Err(Error::new(format!(
                "a {role} message in the Codex prompt does not say what its parts are; cannot prove the seal"
            )));
        }
        for (kind, part) in kinds.iter().zip(&parts) {
            if !ALLOWED_KINDS.contains(kind) {
                return Err(Error::new(format!(
                    "the Codex prompt holds {kind} content; only Daycare's own instructions may be there"
                )));
            }
            let text = part.get("text").and_then(Value::as_str).unwrap_or("");
            match *kind {
                "generic.developer_instructions" => {
                    persona_parts += 1;
                    if text != developer_instructions {
                        return Err(Error::new(
                            "the Codex prompt's developer instructions are not Daycare's persona",
                        ));
                    }
                }
                "model.base_instructions" => {
                    if base_instructions != Some(text) {
                        return Err(Error::new(
                            "the Codex prompt's base instructions are not the ones in Daycare's \
                             private model catalog",
                        ));
                    }
                }
                "user.text" => {
                    user_parts += 1;
                    if text != PROBE_MESSAGE {
                        return Err(Error::new(
                            "the Codex prompt added text to the user's message",
                        ));
                    }
                }
                _ => {}
            }
        }
    }
    if persona_parts != 1 || user_parts != 1 {
        return Err(Error::new(format!(
            "the Codex prompt held {persona_parts} persona and {user_parts} user parts; expected one of each"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SEALED: &str = include_str!("../../../tests/fixtures/codex-prompt-input-sealed.json");
    const LEAKY: &str = include_str!("../../../tests/fixtures/codex-prompt-input-leaky.json");
    const CODE_MODE: &str =
        include_str!("../../../tests/fixtures/codex-prompt-input-sealed-0166.json");
    const BASE: &str = "You are Codex, an agent based on GPT-6. (The catalog's text.)";

    /// The 0.166 code-mode render with the probe and the catalog's base text.
    fn code_mode() -> Value {
        serde_json::from_str(
            &with_probe(CODE_MODE).replace("{{BASE_INSTRUCTIONS}}", &BASE.replace('"', "\\\"")),
        )
        .unwrap()
    }

    fn exec_description(items: &mut Value) -> &mut Value {
        &mut items[0]["tools"][0]["tools"][0]["description"]
    }

    fn with_probe(rendered: &str) -> String {
        rendered.replace("\"PROBE-USER-MESSAGE\"", &format!("\"{PROBE_MESSAGE}\""))
    }

    #[test]
    fn the_captured_sealed_prompt_passes() {
        check_prompt_input(&with_probe(SEALED), "You are Pip, a visitor.", None).unwrap();
    }

    #[test]
    fn a_codex_0166_code_mode_prompt_passes_with_the_catalogs_base_instructions() {
        let rendered = code_mode().to_string();
        check_prompt_input(&rendered, "You are Pip, a visitor.", Some(BASE)).unwrap();
    }

    #[test]
    fn base_instructions_must_be_the_private_catalogs_text() {
        let rendered = code_mode().to_string();
        for base in [Some("You are Codex, but different."), None] {
            let error = check_prompt_input(&rendered, "You are Pip, a visitor.", base)
                .unwrap_err()
                .to_string();
            assert!(error.contains("base instructions"), "{error}");
        }
    }

    #[test]
    fn code_mode_may_declare_only_exec_and_wait_and_daycare_tools_inside() {
        let check = |items: &Value| {
            check_prompt_input(&items.to_string(), "You are Pip, a visitor.", Some(BASE))
        };

        let mut daycare = code_mode();
        let text = exec_description(&mut daycare).as_str().unwrap().to_string();
        *exec_description(&mut daycare) = Value::String(format!(
            "{text}\n\n### `mcp__daycare__daycare_probe`\nA Daycare tool."
        ));
        check(&daycare).unwrap();

        let mut shell = code_mode();
        *exec_description(&mut shell) =
            Value::String(format!("{text}\n\n### `exec_command`\nRuns a command."));
        let error = check(&shell).unwrap_err().to_string();
        assert!(error.contains("exec_command inside exec"), "{error}");

        let mut no_isolate = code_mode();
        *exec_description(&mut no_isolate) =
            Value::String(text.replace("no file system, no network access", "files and network"));
        let error = check(&no_isolate).unwrap_err().to_string();
        assert!(error.contains("isolate"), "{error}");

        let mut extra_tool = code_mode();
        extra_tool[0]["tools"][0]["tools"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!({"type": "function", "name": "request_user_input_async"}));
        let error = check(&extra_tool).unwrap_err().to_string();
        assert!(error.contains("request_user_input_async"), "{error}");

        let mut extra_namespace = code_mode();
        extra_namespace[0]["tools"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!({"type": "namespace", "name": "collaboration", "tools": []}));
        let error = check(&extra_namespace).unwrap_err().to_string();
        assert!(error.contains("namespace collaboration"), "{error}");
    }

    #[test]
    fn user_skills_are_named_when_they_leak() {
        let error = check_prompt_input(&with_probe(LEAKY), "You are Pip, a visitor.", None)
            .unwrap_err()
            .to_string();
        assert!(error.contains("skills"), "{error}");
    }

    #[test]
    fn a_foreign_part_or_a_changed_persona_fails() {
        let error = check_prompt_input(&with_probe(SEALED), "You are Pip.", None)
            .unwrap_err()
            .to_string();
        assert!(error.contains("persona"), "{error}");

        let mut items: Value = serde_json::from_str(&with_probe(SEALED)).unwrap();
        items[1]["internal_chat_message_metadata_passthrough"]["content_item_kinds"] =
            serde_json::json!(["environments.environment_context"]);
        let error = check_prompt_input(&items.to_string(), "You are Pip, a visitor.", None)
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("environments.environment_context"),
            "{error}"
        );

        let mut items: Value = serde_json::from_str(&with_probe(SEALED)).unwrap();
        let first = items[0].clone();
        items.as_array_mut().unwrap().push(first);
        let error = check_prompt_input(&items.to_string(), "You are Pip, a visitor.", None)
            .unwrap_err()
            .to_string();
        assert!(error.contains("expected one of each"), "{error}");
    }

    #[test]
    fn legacy_multi_agent_prompt_blocks_are_rejected_before_launch() {
        let legacy = include_str!("../../../tests/fixtures/codex-prompt-input-multi-agent.json");
        let error =
            check_prompt_input(&with_probe(legacy), "You are Pip, a visitor.", None).unwrap_err();
        assert!(error.to_string().contains("multi_agent."));
    }
}
