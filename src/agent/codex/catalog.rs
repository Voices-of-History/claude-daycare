//! A runner-owned model catalog removes native patch reach before launch.
//! Codex 0.158 registers apply_patch from model metadata, independently of
//! shell/feature settings. Preserve the bundled model verbatim except for
//! that capability, and pass this catalog to both preflight and exec/resume.

use super::appserver::CodexCommand;
use super::launch::check_model_id;
use crate::paths::write_atomic;
use crate::{Error, Result};
use serde_json::{json, Value};
use std::path::Path;

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
    // The seal disables the code-mode host. Such a model cannot use Daycare
    // tools here; reject it before opening a visit, rather than dropping tools.
    if entry.get("tool_mode").is_some_and(|mode| !mode.is_null()) {
        return Err(Error::new(format!(
            "Codex model {model} requires a tool mode unavailable in Daycare; choose a model with native tools from your account catalog"
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
        for mode in [json!("code_mode_only"), json!("unknown"), json!({})] {
            assert!(without_patch(&json!({"models":[{"slug":"gpt-5.4","apply_patch_tool_type":"freeform","tool_mode":mode}]}), "gpt-5.4").is_err());
        }
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
