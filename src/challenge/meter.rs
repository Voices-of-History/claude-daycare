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
/// What the platform keeps of the readings (`zMeterReadings` in the
/// platform's `entries.ts`): for each reading, the share of the weekly
/// allowance used and when, or only that it failed. Nothing else goes: not
/// the error text, which can name paths on this machine, and not the ids the
/// platform already has. A reading in any other shape, such as an edited or
/// unreadable file, goes as failed.
pub fn for_platform(readings: &Value) -> Option<Value> {
    let mut sent = serde_json::Map::new();
    for name in ["start", "end"] {
        let Some(reading) = readings.get(name).filter(|reading| reading.is_object()) else {
            continue;
        };
        let taken_at = reading
            .get("taken_at")
            .and_then(Value::as_str)
            .filter(|text| is_utc_timestamp(text));
        let used = reading
            .get("used_percentage")
            .and_then(Value::as_f64)
            .filter(|used| used.is_finite() && (0.0..=100.0).contains(used));
        let ok = reading.get("ok") == Some(&Value::Bool(true));
        let value = match (ok, used, taken_at) {
            (true, Some(used), Some(taken_at)) => {
                json!({"ok": true, "used_percentage": used, "taken_at": taken_at})
            }
            (_, _, Some(taken_at)) => json!({"ok": false, "taken_at": taken_at}),
            _ => json!({"ok": false}),
        };
        sent.insert(name.to_string(), value);
    }
    (!sent.is_empty()).then_some(Value::Object(sent))
}

/// The readings in words, for the owner before their typed yes.
pub fn describe(sent: &Value) -> String {
    [("start", "before"), ("end", "after")]
        .into_iter()
        .filter_map(|(name, label)| {
            let reading = sent.get(name)?;
            Some(
                match reading.get("used_percentage").and_then(Value::as_f64) {
                    Some(used) => format!("{label} the session {used}%"),
                    None => format!("{label} the session: failed"),
                },
            )
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// `2026-11-09T17:36:27Z`, the form `take` writes.
fn is_utc_timestamp(text: &str) -> bool {
    let bytes = text.as_bytes();
    bytes.len() == 20
        && bytes.iter().enumerate().all(|(at, byte)| match at {
            4 | 7 => *byte == b'-',
            10 => *byte == b'T',
            13 | 16 => *byte == b':',
            19 => *byte == b'Z',
            _ => byte.is_ascii_digit(),
        })
}

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
    fn the_platform_gets_the_share_used_and_when_and_nothing_else() {
        let readings = json!({
            "start": {"version": 1, "reading": "start", "client": "codex", "challenge_id": "c1",
                "entry_id": "e1", "taken_at": "2026-11-09T17:36:27Z", "ok": true,
                "used_percentage": 44.0, "resets_at": "live:11:11", "meter_key": "codex_weekly:codex"},
            "end": {"taken_at": "2026-11-09T17:44:02Z", "ok": false,
                "error": "could not read /Users/someone/.codex/auth.json"},
        });
        let sent = for_platform(&readings).unwrap();
        assert_eq!(
            sent,
            json!({
                "start": {"ok": true, "used_percentage": 44.0, "taken_at": "2026-11-09T17:36:27Z"},
                "end": {"ok": false, "taken_at": "2026-11-09T17:44:02Z"},
            })
        );
        assert_eq!(
            describe(&sent),
            "before the session 44%, after the session: failed"
        );

        // An edited or unreadable reading goes as failed, never as a number.
        for odd in [
            json!({"ok": true, "used_percentage": 140, "taken_at": "2026-11-09T17:36:27Z"}),
            json!({"ok": true, "used_percentage": 12.5}),
            json!({"ok": true, "used_percentage": 12.5, "taken_at": "yesterday"}),
            json!({"ok": "yes", "used_percentage": 12.5, "taken_at": "2026-11-09T17:36:27Z"}),
            json!({"unreadable": "not valid JSON"}),
        ] {
            let sent = for_platform(&json!({ "start": odd })).unwrap();
            assert_eq!(sent["start"]["ok"], false, "{odd}");
            assert!(sent["start"].get("used_percentage").is_none(), "{odd}");
        }
        assert_eq!(for_platform(&json!({"start": "44", "during": {}})), None);
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
