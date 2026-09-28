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

use super::appserver::CodexCommand;
use crate::{Error, Result};
use serde_json::Value;
use std::process::Stdio;

pub const PROBE_MESSAGE: &str = "DAYCARE-SEAL-PROBE";

/// Only Daycare's persona and the user message may reach the model.
const ALLOWED_KINDS: [&str; 2] = ["generic.developer_instructions", "user.text"];

/// Run the proof. `seal_args` are the turn's own features, settings, model,
/// and persona (`launch::seal_args`).
pub fn prove_sealed_prompt(
    codex: &CodexCommand,
    seal_args: &[String],
    developer_instructions: &str,
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
    check_prompt_input(&rendered, developer_instructions)
}

/// The allow-list over one `prompt-input` rendering.
pub fn check_prompt_input(rendered: &str, developer_instructions: &str) -> Result<()> {
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

    fn with_probe(rendered: &str) -> String {
        rendered.replace("\"PROBE-USER-MESSAGE\"", &format!("\"{PROBE_MESSAGE}\""))
    }

    #[test]
    fn the_captured_sealed_prompt_passes() {
        check_prompt_input(&with_probe(SEALED), "You are Pip, a visitor.").unwrap();
    }

    #[test]
    fn user_skills_are_named_when_they_leak() {
        let error = check_prompt_input(&with_probe(LEAKY), "You are Pip, a visitor.")
            .unwrap_err()
            .to_string();
        assert!(error.contains("skills"), "{error}");
    }

    #[test]
    fn a_foreign_part_or_a_changed_persona_fails() {
        let error = check_prompt_input(&with_probe(SEALED), "You are Pip.")
            .unwrap_err()
            .to_string();
        assert!(error.contains("persona"), "{error}");

        let mut items: Value = serde_json::from_str(&with_probe(SEALED)).unwrap();
        items[1]["internal_chat_message_metadata_passthrough"]["content_item_kinds"] =
            serde_json::json!(["environments.environment_context"]);
        let error = check_prompt_input(&items.to_string(), "You are Pip, a visitor.")
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("environments.environment_context"),
            "{error}"
        );

        let mut items: Value = serde_json::from_str(&with_probe(SEALED)).unwrap();
        let first = items[0].clone();
        items.as_array_mut().unwrap().push(first);
        let error = check_prompt_input(&items.to_string(), "You are Pip, a visitor.")
            .unwrap_err()
            .to_string();
        assert!(error.contains("expected one of each"), "{error}");
    }

    #[test]
    fn legacy_multi_agent_prompt_blocks_are_rejected_before_launch() {
        let legacy = include_str!("../../../tests/fixtures/codex-prompt-input-multi-agent.json");
        let error = check_prompt_input(&with_probe(legacy), "You are Pip, a visitor.").unwrap_err();
        assert!(error.to_string().contains("multi_agent."));
    }
}
