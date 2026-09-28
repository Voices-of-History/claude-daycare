//! Codex's own record of a thread: `$CODEX_HOME/sessions/YYYY/MM/DD/
//! rollout-<timestamp>-<thread_id>.jsonl`.
//!
//! Codex writes it, not the runner, and it is the closest thing Codex has to
//! Claude's `system/init` event: every `turn_context` record carries the
//! sandbox, approval policy, and cwd the turn actually ran with. Its
//! `event_msg/token_count` records carry the account's rate limits, which the
//! `--json` stream does not. Shapes checked against codex-cli 0.154.

use crate::stream::TurnUsage;
use crate::visit::WEEKLY_WINDOW;
use crate::{Error, Result};
use serde_json::Value;
use std::path::{Path, PathBuf};

use super::appserver::WEEKLY_WINDOW_MINUTES;

/// The rollout file for a thread, if Codex wrote one.
pub fn find(sessions: &Path, thread_id: &str) -> Option<PathBuf> {
    find_in(sessions, &format!("-{thread_id}.jsonl"), 0)
}

fn find_in(dir: &Path, suffix: &str, depth: usize) -> Option<PathBuf> {
    if depth > 4 {
        return None;
    }
    let mut entries: Vec<_> = std::fs::read_dir(dir).ok()?.flatten().collect();
    // Newest date folders first: a visit's thread is recent.
    entries.sort_by_key(|entry| std::cmp::Reverse(entry.file_name()));
    for entry in entries {
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if path.is_dir() {
            if let Some(found) = find_in(&path, suffix, depth + 1) {
                return Some(found);
            }
        } else if name.starts_with("rollout-") && name.ends_with(suffix) {
            return Some(path);
        }
    }
    None
}

/// What one rollout says about its last turn.
#[derive(Debug, Default)]
pub struct Rollout {
    /// The last `turn_context` payload.
    pub turn_context: Option<Value>,
    /// The last `token_count` rate limits.
    pub rate_limits: Option<Value>,
}

pub fn read(path: &Path) -> Result<Rollout> {
    let text = std::fs::read_to_string(path)?;
    let mut rollout = Rollout::default();
    for line in text.lines() {
        let Ok(record) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let payload = record.get("payload");
        match record.get("type").and_then(Value::as_str) {
            Some("turn_context") => rollout.turn_context = payload.cloned(),
            Some("event_msg")
                if payload
                    .and_then(|payload| payload.get("type"))
                    .and_then(Value::as_str)
                    == Some("token_count") =>
            {
                if let Some(limits) = payload
                    .and_then(|payload| payload.get("rate_limits"))
                    .filter(|limits| !limits.is_null())
                {
                    rollout.rate_limits = Some(limits.clone());
                }
            }
            _ => {}
        }
    }
    Ok(rollout)
}

/// The turn ran read-only, never asking, in the daycare workspace.
pub fn verify_turn_context(context: &Value, workspace: &Path) -> Result<()> {
    let sandbox = context
        .get("sandbox_policy")
        .and_then(|policy| policy.get("type"))
        .and_then(Value::as_str);
    if sandbox != Some("read-only") {
        return Err(Error::new(format!(
            "codex ran the turn with sandbox {:?}, expected read-only",
            sandbox.unwrap_or("unreported")
        )));
    }
    let approval = context.get("approval_policy").and_then(Value::as_str);
    if approval != Some("never") {
        return Err(Error::new(format!(
            "codex ran the turn with approval policy {:?}, expected never",
            approval.unwrap_or("unreported")
        )));
    }
    let cwd = context
        .get("cwd")
        .and_then(Value::as_str)
        .ok_or_else(|| Error::new("codex did not record the turn's cwd"))?;
    let reported = Path::new(cwd)
        .canonicalize()
        .unwrap_or_else(|_| PathBuf::from(cwd));
    let expected = workspace
        .canonicalize()
        .unwrap_or_else(|_| workspace.to_path_buf());
    if reported != expected {
        return Err(Error::new(format!(
            "codex ran the turn in {} instead of the daycare workspace {}",
            reported.display(),
            expected.display()
        )));
    }
    Ok(())
}

/// Copy the weekly window into the receipt's usage the way Claude's
/// `rate_limit_event` fills it, so the ledger and the rate-limit nap read
/// Codex without knowing it is Codex.
pub fn apply_rate_limits(limits: &Value, usage: &mut TurnUsage) {
    for slot in ["primary", "secondary"] {
        let Some(window) = limits.get(slot).filter(|window| !window.is_null()) else {
            continue;
        };
        if window.get("window_minutes").and_then(Value::as_i64) != Some(WEEKLY_WINDOW_MINUTES) {
            continue;
        }
        usage.rate_limit_type = Some(WEEKLY_WINDOW.into());
        usage.rate_limit_utilization = window
            .get("used_percent")
            .and_then(Value::as_f64)
            .map(|percent| percent / 100.0);
        usage.rate_limit_resets_at = window.get("resets_at").and_then(Value::as_i64);
        let reached = limits
            .get("rate_limit_reached_type")
            .is_some_and(|reached| !reached.is_null());
        // A usage-limit failure in the stream already said "rejected".
        if usage.rate_limit_status.is_none() || reached {
            usage.rate_limit_status = Some(if reached { "rejected" } else { "allowed" }.into());
        }
        return;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_rollout_is_found_by_thread_id_under_date_folders() {
        let root = crate::testdir::unique_dir("daycare-codex-rollout");
        let day = root.join("2026/09/28");
        std::fs::create_dir_all(&day).unwrap();
        let id = "01a0e892-193c-76f1-9f75-3c60bd3c13cc";
        let path = day.join(format!("rollout-2026-09-28T15-11-23-{id}.jsonl"));
        std::fs::write(
            &path,
            [
                json!({"type": "session_meta", "payload": {}}),
                json!({"type": "turn_context", "payload": {"sandbox_policy": {"type": "workspace-write"}}}),
                json!({"type": "turn_context", "payload": {"sandbox_policy": {"type": "read-only"}, "approval_policy": "never", "cwd": root.to_string_lossy()}}),
                json!({"type": "event_msg", "payload": {"type": "token_count", "rate_limits": {"limit_id": "codex", "primary": {"used_percent": 13.0, "window_minutes": 10080, "resets_at": 1791055017}, "secondary": null, "rate_limit_reached_type": null}}}),
            ]
            .iter()
            .map(|record| record.to_string() + "\n")
            .collect::<String>(),
        )
        .unwrap();
        assert_eq!(find(&root, id), Some(path.clone()));
        assert_eq!(find(&root, "01a0e892-0000-76f1-9f75-3c60bd3c13cc"), None);

        let rollout = read(&path).unwrap();
        let context = rollout.turn_context.unwrap();
        verify_turn_context(&context, &root).unwrap();
        let elsewhere = verify_turn_context(&context, &day).unwrap_err().to_string();
        assert!(
            elsewhere.contains("instead of the daycare workspace"),
            "{elsewhere}"
        );

        let mut usage = TurnUsage::default();
        apply_rate_limits(&rollout.rate_limits.unwrap(), &mut usage);
        assert_eq!(usage.rate_limit_type.as_deref(), Some("seven_day"));
        assert_eq!(usage.rate_limit_utilization, Some(0.13));
        assert_eq!(usage.rate_limit_resets_at, Some(1791055017));
        assert_eq!(usage.rate_limit_status.as_deref(), Some("allowed"));
    }

    #[test]
    fn a_writable_or_asking_turn_fails() {
        let ws = std::env::temp_dir();
        let writable = json!({"sandbox_policy": {"type": "workspace-write"}, "approval_policy": "never", "cwd": ws});
        assert!(verify_turn_context(&writable, &ws)
            .unwrap_err()
            .to_string()
            .contains("read-only"));
        let asking = json!({"sandbox_policy": {"type": "read-only"}, "approval_policy": "on-request", "cwd": ws});
        assert!(verify_turn_context(&asking, &ws)
            .unwrap_err()
            .to_string()
            .contains("never"));
    }

    #[test]
    fn a_reached_limit_reads_as_rejected() {
        let mut usage = TurnUsage::default();
        apply_rate_limits(
            &json!({"primary": {"used_percent": 100.0, "window_minutes": 10080, "resets_at": 1791055017}, "rate_limit_reached_type": "primary"}),
            &mut usage,
        );
        assert_eq!(usage.rate_limit_status.as_deref(), Some("rejected"));
    }
}
