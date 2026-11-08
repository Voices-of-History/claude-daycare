//! Weekly-allowance readings around an interactive challenge session (plan
//! task N11): `meter/start.json` before the client starts, `meter/end.json`
//! after it exits, in the challenge folder.
//!
//! These are readings, not a cap. Nothing here ends a session: the visit
//! loop's between-turn decision (`Ledger::should_end`) has no place in a
//! session the owner drives. Claude Code is read with the visit meter's own
//! `/usage` reader (`usage_meter::sample_weekly_usage`) and Codex with the
//! app-server's `account/rateLimits/read`; neither spends a model turn.
//!
//! A reading that fails is written as failed (`"ok": false` and the error),
//! never left out, so a missing number is visible as missing. Each start
//! removes the previous session's `end.json`, so the two files always belong
//! to the same session. The files sit in the folder the agent works in, so a
//! session could edit them; like the declaration, they are self-reported.
//!
//! The schema, version 1:
//!
//! ```json
//! {"version": 1, "reading": "start" | "end", "client": "claude" | "codex",
//!  "challenge_id": "...", "entry_id": "...", "taken_at": "<RFC 3339 UTC>",
//!  "ok": true, "used_percentage": 64.0, "resets_at": "...", "meter_key": "...",
//!  "session": {"started": true, "exit_code": 0, "signal": null}}
//! ```
//!
//! A failed reading has `"ok": false, "error": "..."` instead of the three
//! meter fields. `session` is on `end.json` only; `{"started": false,
//! "error": "..."}` when the client could not be started.

use super::client::ChallengeClient;
use super::folder::{remove_file_in, write_file_in};
use crate::approval::scan::Client;
use crate::paths::Layout;
use crate::usage_meter::WeeklyUsageSnapshot;
use crate::Result;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

pub const METER_DIR: &str = "meter";
pub const START_FILE: &str = "meter/start.json";
pub const END_FILE: &str = "meter/end.json";
pub const READING_VERSION: u32 = 1;

/// One reading, or why there is none.
#[derive(Debug, Clone, PartialEq)]
pub struct Reading {
    pub taken_at: String,
    pub outcome: std::result::Result<WeeklyUsageSnapshot, String>,
}

/// Which session a reading belongs to.
pub struct MeterContext<'a> {
    pub client: Client,
    pub challenge_id: &'a str,
    pub entry_id: &'a str,
}

/// Take one reading. A failure is part of the reading, not an error.
pub fn take(client: &dyn ChallengeClient, layout: &Layout, now: fn() -> String) -> Reading {
    let taken_at = now();
    let outcome = client
        .meter(layout)
        .map_err(|error| error.message().to_string());
    Reading { taken_at, outcome }
}

pub fn reading_json(
    which: &str,
    reading: &Reading,
    context: &MeterContext<'_>,
    session: Option<Value>,
) -> Value {
    let mut value = json!({
        "version": READING_VERSION,
        "reading": which,
        "client": context.client,
        "challenge_id": context.challenge_id,
        "entry_id": context.entry_id,
        "taken_at": reading.taken_at,
    });
    match &reading.outcome {
        Ok(snapshot) => {
            value["ok"] = json!(true);
            value["used_percentage"] = json!(snapshot.used_percentage);
            value["resets_at"] = json!(snapshot.resets_at);
            value["meter_key"] = json!(snapshot.meter_key);
        }
        Err(error) => {
            value["ok"] = json!(false);
            value["error"] = json!(error);
        }
    }
    if let Some(session) = session {
        value["session"] = session;
    }
    value
}

fn write(folder: &Path, relative: &str, value: &Value) -> Result<PathBuf> {
    let mut text = serde_json::to_string_pretty(value)?;
    text.push('\n');
    write_file_in(folder, relative, text.as_bytes())
}

/// Write `meter/start.json`, first removing the last session's `end.json`.
pub fn write_start(folder: &Path, value: &Value) -> Result<PathBuf> {
    remove_file_in(folder, END_FILE)?;
    write(folder, START_FILE, value)
}

pub fn write_end(folder: &Path, value: &Value) -> Result<PathBuf> {
    write(folder, END_FILE, value)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn context() -> MeterContext<'static> {
        MeterContext {
            client: Client::Codex,
            challenge_id: "c1",
            entry_id: "e1",
        }
    }

    #[test]
    fn a_failed_reading_is_recorded_as_failed_and_a_new_start_clears_the_old_end() {
        let base = crate::testdir::unique_dir("daycare-meter");
        let folder = base.join("c1");
        std::fs::create_dir_all(&folder).unwrap();
        crate::paths::create_private_dir(&folder).unwrap();
        let good = Reading {
            taken_at: "2026-11-07T12:00:00Z".into(),
            outcome: Ok(WeeklyUsageSnapshot {
                used_percentage: 14.0,
                resets_at: "live:11:9".into(),
                meter_key: "codex_weekly:codex".into(),
            }),
        };
        let failed = Reading {
            taken_at: "2026-11-07T13:00:00Z".into(),
            outcome: Err("codex app-server exited before answering".into()),
        };
        write_start(&folder, &reading_json("start", &good, &context(), None)).unwrap();
        write_end(
            &folder,
            &reading_json(
                "end",
                &failed,
                &context(),
                Some(json!({"started": true, "exit_code": 0, "signal": null})),
            ),
        )
        .unwrap();
        let read = |name: &str| -> Value {
            serde_json::from_slice(&std::fs::read(folder.join(name)).unwrap()).unwrap()
        };
        let start = read(START_FILE);
        assert_eq!(start["ok"], true);
        assert_eq!(start["used_percentage"], 14.0);
        assert_eq!(start["entry_id"], "e1");
        assert!(start.get("session").is_none());
        let end = read(END_FILE);
        assert_eq!(end["ok"], false);
        assert_eq!(end["error"], "codex app-server exited before answering");
        assert!(end.get("used_percentage").is_none());
        assert_eq!(end["session"]["exit_code"], 0);

        write_start(&folder, &reading_json("start", &failed, &context(), None)).unwrap();
        assert!(!folder.join(END_FILE).exists());
        assert_eq!(read(START_FILE)["ok"], false);
        std::fs::remove_dir_all(base).ok();
    }
}
