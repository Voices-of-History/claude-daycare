//! A runner-owned model catalog removes native reach that model metadata
//! grants before launch. Codex 0.158 registers apply_patch from
//! `apply_patch_tool_type`, and 0.162 registers `request_user_input_async`,
//! `send_user_message_async` and the clock tools from
//! `experimental_supported_tools`, independently of shell/feature settings.
//! Preserve the bundled model verbatim except for those capabilities, and
//! pass this catalog to both preflight and exec/resume.

use super::appserver::CodexCommand;
use super::launch::check_model_id;
use crate::paths::write_atomic;
use crate::{Error, Result};
use serde_json::{json, Value};
use std::path::Path;

/// `ToolMode` values in Codex 0.162 (`openai_models.rs`), besides null.
const TOOL_MODES: [&str; 3] = ["direct", "code_mode", "code_mode_only"];

pub fn without_patch(catalog: &Value, model: &str) -> Result<Value> {
    check_model_id(model)?;
    let mut entry = catalog
        .get("models")
        .and_then(Value::as_array)
        .and_then(|models| {
            models
                .iter()
                .find(|entry| entry.get("slug").and_then(Value::as_str) == Some(model))
        })
        .cloned()
        .ok_or_else(|| {
            Error::new(
                "Codex's bundled catalog does not contain the selected model; cannot seal native tools",
            )
        })?;
    // Code mode calls the same registered tools from a V8 isolate with no
    // file, network, or process API (tests/fixtures/codex-exec.md). An
    // unknown mode fails closed rather than assuming it is the same.
    if !entry
        .get("tool_mode")
        .is_none_or(|mode| mode.is_null() || TOOL_MODES.contains(&mode.as_str().unwrap_or("")))
    {
        return Err(Error::new(format!(
            "Codex model {model} uses a tool mode Daycare has not verified; choose another model from your account catalog"
        )));
    }
    // Null disables registration in Codex's tool planner. An unknown schema
    // fails closed rather than assuming the absence of this field is safe.
    if !entry
        .get("apply_patch_tool_type")
        .is_some_and(|kind| kind.is_null() || kind.is_string())
    {
        return Err(Error::new("Codex's model catalog has no recognized patch capability field; cannot seal native tools"));
    }
    entry["apply_patch_tool_type"] = Value::Null;
    match entry.get("experimental_supported_tools") {
        None | Some(Value::Array(_)) => {
            entry["experimental_supported_tools"] = json!([]);
        }
        Some(_) => {
            return Err(Error::new("Codex's model catalog lists experimental tools in an unrecognized shape; cannot seal native tools"));
        }
    }
    Ok(json!({"models": [entry]}))
}

pub fn inspect(codex: &CodexCommand, model: &str) -> Result<Value> {
    let output = codex
        .command()
        .args(["debug", "models", "--bundled"])
        .output()?;
    if !output.status.success() {
        return Err(Error::new(
            "could not read Codex's bundled model catalog; cannot remove apply_patch",
        ));
    }
    let catalog: Value = serde_json::from_slice(&output.stdout)
        .map_err(|_| Error::new("Codex's bundled model catalog is not JSON"))?;
    without_patch(&catalog, model)
}

pub fn prepare(codex: &CodexCommand, model: &str, path: &Path) -> Result<()> {
    let sealed = inspect(codex, model)?;
    write_atomic(path, &serde_json::to_vec(&sealed)?, 0o600)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_patch_capability_changes_and_only_the_selected_model_survives() {
        let model = json!({"slug":"gpt-5.5", "apply_patch_tool_type":"freeform", "tool_mode":null,
            "base_instructions":"original", "context_window":12345, "extra_metadata":{"preserve":true}});
        let mut expected = model.clone();
        expected["apply_patch_tool_type"] = Value::Null;
        expected["experimental_supported_tools"] = json!([]);
        let sealed =
            without_patch(&json!({"models":[model, {"slug":"unverified"}]}), "gpt-5.5").unwrap();
        assert_eq!(sealed, json!({"models":[expected]}));
        assert_eq!(without_patch(&sealed, "gpt-5.5").unwrap(), sealed);
    }

    #[test]
    fn unknown_catalog_shapes_and_absent_models_fail_closed() {
        for catalog in [
            json!({}),
            json!({"models":[]}),
            json!({"models":[{"slug":"gpt-5.5"}]}),
            json!({"models":[{"slug":"gpt-5.5","apply_patch_tool_type":{}}]}),
        ] {
            assert!(without_patch(&catalog, "gpt-5.5").is_err());
        }
        assert!(without_patch(&json!({"models":[]}), "gpt-6-astra").is_err());
    }

    #[test]
    fn selected_nondefault_models_keep_the_same_patch_seal() {
        let selected = json!({"slug":"gpt-5.4","apply_patch_tool_type":"freeform","tool_mode":null,"base_instructions":"preserved"});
        let sealed =
            without_patch(&json!({"models":[{"slug":"gpt-5.5"},selected]}), "gpt-5.4").unwrap();
        assert_eq!(sealed["models"].as_array().unwrap().len(), 1);
        assert_eq!(sealed["models"][0]["slug"], "gpt-5.4");
        assert_eq!(sealed["models"][0]["apply_patch_tool_type"], Value::Null);
        assert_eq!(sealed["models"][0]["base_instructions"], "preserved");
        for mode in [json!("unknown"), json!({}), json!(["code_mode_only"])] {
            assert!(without_patch(&json!({"models":[{"slug":"gpt-5.4","apply_patch_tool_type":"freeform","tool_mode":mode}]}), "gpt-5.4").is_err());
        }
    }

    #[test]
    fn code_mode_models_are_sealed_without_metadata_tools() {
        // The 0.162.1 bundled gpt-6.1-sol entry, trimmed to the fields that
        // decide its tools.
        let model = json!({"slug":"gpt-6.1-sol","tool_mode":"code_mode_only",
            "apply_patch_tool_type":"freeform","shell_type":"unified_exec",
            "experimental_supported_tools":["send_user_message_async","clock"],
            "multi_agent_version":"v2","use_responses_lite":true});
        let sealed = without_patch(&json!({"models":[model]}), "gpt-6.1-sol").unwrap();
        let entry = &sealed["models"][0];
        assert_eq!(entry["tool_mode"], "code_mode_only");
        assert_eq!(entry["apply_patch_tool_type"], Value::Null);
        assert_eq!(entry["experimental_supported_tools"], json!([]));
        assert_eq!(entry["use_responses_lite"], true);
        for mode in ["direct", "code_mode", "code_mode_only"] {
            let model = json!({"slug":"m","tool_mode":mode,"apply_patch_tool_type":null});
            without_patch(&json!({"models":[model]}), "m").unwrap();
        }
        let odd =
            json!({"slug":"m","apply_patch_tool_type":null,"experimental_supported_tools":"clock"});
        assert!(without_patch(&json!({"models":[odd]}), "m").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn catalog_export_writes_private_sealed_metadata_or_fails_without_a_file() {
        use std::os::unix::fs::PermissionsExt;
        for (output, exit_code, succeeds) in [
            (
                r#"{"models":[{"slug":"gpt-5.5","apply_patch_tool_type":"freeform"}]}"#,
                0,
                true,
            ),
            ("not JSON", 0, false),
            (r#"{"models":[]}"#, 0, false),
            (r#"{"models":[]}"#, 1, false),
        ] {
            let root = crate::testdir::unique_dir("daycare-catalog-export");
            // Interpret the fixture instead of executing a freshly written
            // file, avoiding Linux ETXTBSY races in parallel test processes.
            // `prepare` passes `debug models --bundled`: sh reads ./debug.
            let program = root.join("debug");
            std::fs::write(&program, format!(
                "[ \"$*\" = \"models --bundled\" ] || exit 2\ncat <<'CATALOG'\n{output}\nCATALOG\nexit {exit_code}\n"
            )).unwrap();
            let codex = CodexCommand {
                program: "/bin/sh".into(),
                cwd: root.clone(),
                env: Vec::new(),
                env_remove: Vec::new(),
            };
            let path = root.join("models.json");
            let result = prepare(&codex, "gpt-5.5", &path);
            assert_eq!(result.is_ok(), succeeds, "{result:?}");
            assert_eq!(path.exists(), succeeds);
            if succeeds {
                let saved: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
                assert_eq!(saved["models"][0]["apply_patch_tool_type"], Value::Null);
                assert_eq!(
                    std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
                    0o600
                );
            }
            std::fs::remove_dir_all(root).unwrap();
        }
    }
}
