//! `codex app-server`: Codex's JSON-RPC interface over stdio, used for the
//! things a turn must not spend a model call on — who is logged in, the
//! weekly allowance, and the model catalog.
//!
//! Shapes checked against codex-cli 0.154 (`tests/fixtures/codex-app-server.json`
//! is a redacted capture): `initialize` → `initialized` → requests.
//! `account/read` → `{account: {type: "chatgpt", planType}}`;
//! `account/rateLimits/read` → `{rateLimits: {limitId, primary, secondary,
//! planType}, rateLimitsByLimitId}` with each window as `{usedPercent,
//! windowDurationMins, resetsAt}`; `model/list` → `{data: [{id, hidden,
//! isDefault}]}`.

use crate::meter::WeeklyMeter;
use crate::paths::Layout;
use crate::usage_meter::{local_month_day, WeeklyUsageSnapshot};
use crate::{Error, Result};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

/// The weekly window, in minutes. On a Pro plan it is `primary`; on other
/// plans it may be `secondary` beside a five-hour `primary`. Select by length,
/// never by position.
pub const WEEKLY_WINDOW_MINUTES: i64 = 10_080;

const ANSWER_TIMEOUT: Duration = Duration::from_secs(20);

/// How to start a Codex child in the sealed homes: binary, environment, cwd.
#[derive(Debug, Clone)]
pub struct CodexCommand {
    pub program: String,
    pub env_remove: Vec<String>,
    pub env: Vec<(String, String)>,
    pub cwd: PathBuf,
}

impl CodexCommand {
    pub fn command(&self) -> Command {
        let mut command = Command::new(&self.program);
        command.current_dir(&self.cwd);
        for name in &self.env_remove {
            command.env_remove(name);
        }
        for (name, value) in &self.env {
            command.env(name, value);
        }
        command
    }
}

/// One running `codex app-server`, killed on drop.
pub struct AppServer {
    child: Child,
    stdin: ChildStdin,
    lines: Receiver<String>,
    next_id: u64,
}

impl AppServer {
    pub fn start(codex: &CodexCommand) -> Result<Self> {
        let mut child = codex
            .command()
            .arg("app-server")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|error| {
                Error::new(format!(
                    "could not start {} app-server: {error}. Is Codex CLI installed and on PATH?",
                    codex.program
                ))
            })?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| Error::new("codex app-server stdin unavailable"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| Error::new("codex app-server stdout unavailable"))?;
        let (send, lines) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout)
                .lines()
                .map_while(std::result::Result::ok)
            {
                if send.send(line).is_err() {
                    break;
                }
            }
        });
        let mut server = AppServer {
            child,
            stdin,
            lines,
            next_id: 0,
        };
        server.request(
            "initialize",
            Some(json!({"clientInfo": {"name": "daycare-runner", "version": env!("CARGO_PKG_VERSION")}})),
        )?;
        server.send(&json!({"method": "initialized"}))?;
        Ok(server)
    }

    fn send(&mut self, message: &Value) -> Result<()> {
        let mut line = message.to_string();
        line.push('\n');
        self.stdin
            .write_all(line.as_bytes())
            .and_then(|_| self.stdin.flush())
            .map_err(|error| Error::new(format!("codex app-server stopped listening: {error}")))
    }

    /// One request and its answer. Notifications in between are skipped.
    pub fn request(&mut self, method: &str, params: Option<Value>) -> Result<Value> {
        let id = self.next_id;
        self.next_id += 1;
        // Some methods (account/read) refuse a request without `params`;
        // every method accepts an empty object.
        let message =
            json!({"id": id, "method": method, "params": params.unwrap_or_else(|| json!({}))});
        self.send(&message)?;
        let deadline = Instant::now() + ANSWER_TIMEOUT;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let line = match self.lines.recv_timeout(left) {
                Ok(line) => line,
                Err(RecvTimeoutError::Timeout) => {
                    return Err(Error::new(format!(
                        "codex app-server did not answer {method} within {}s",
                        ANSWER_TIMEOUT.as_secs()
                    )))
                }
                Err(RecvTimeoutError::Disconnected) => {
                    return Err(Error::new(format!(
                        "codex app-server exited before answering {method}"
                    )))
                }
            };
            let Ok(answer) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            if answer.get("id").and_then(Value::as_u64) != Some(id) {
                continue;
            }
            if let Some(error) = answer.get("error") {
                let text = error
                    .get("message")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .unwrap_or_else(|| error.to_string());
                return Err(Error::new(format!("codex app-server {method}: {text}")));
            }
            return Ok(answer.get("result").cloned().unwrap_or(Value::Null));
        }
    }
}

impl Drop for AppServer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Refuse anything but a ChatGPT login: an API key would move every visit
/// onto the owner's API bill. `account/read` answers without a model call.
pub fn check_account(result: &Value) -> Result<String> {
    let account = result.get("account").filter(|account| !account.is_null());
    let Some(account) = account else {
        return Err(Error::new(
            "Codex is not logged in. Run `codex login` and sign in with ChatGPT",
        ));
    };
    match account.get("type").and_then(Value::as_str) {
        Some("chatgpt") => Ok(account
            .get("planType")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_string()),
        Some(other) => Err(Error::new(format!(
            "Codex is logged in with {other}; Daycare runs on a ChatGPT plan only. \
             Run `codex logout` and `codex login` with ChatGPT"
        ))),
        None => Err(Error::new(
            "codex app-server reported an account with no type",
        )),
    }
}

/// The visible model ids in a `model/list` answer.
pub fn model_ids(result: &Value) -> Vec<String> {
    result
        .get("data")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|model| model.get("hidden").and_then(Value::as_bool) != Some(true))
        .filter_map(|model| model.get("id").and_then(Value::as_str))
        .map(str::to_string)
        .collect()
}

/// The weekly window out of an `account/rateLimits/read` answer.
pub fn weekly_snapshot(result: &Value) -> Result<WeeklyUsageSnapshot> {
    let mut limits: Vec<&Value> = Vec::new();
    if let Some(main) = result.get("rateLimits") {
        limits.push(main);
    }
    if let Some(by_id) = result.get("rateLimitsByLimitId").and_then(Value::as_object) {
        limits.extend(by_id.values());
    }
    for limit in limits {
        let limit_id = limit
            .get("limitId")
            .and_then(Value::as_str)
            .unwrap_or("codex");
        for slot in ["primary", "secondary"] {
            let Some(window) = limit.get(slot).filter(|window| !window.is_null()) else {
                continue;
            };
            if window.get("windowDurationMins").and_then(Value::as_i64)
                != Some(WEEKLY_WINDOW_MINUTES)
            {
                continue;
            }
            let used = window
                .get("usedPercent")
                .and_then(Value::as_f64)
                .ok_or_else(|| Error::new("Codex's weekly window has no usedPercent"))?;
            let resets_at = window
                .get("resetsAt")
                .and_then(Value::as_u64)
                .ok_or_else(|| Error::new("Codex's weekly window has no resetsAt"))?;
            let (month, day) = local_month_day(resets_at);
            return Ok(WeeklyUsageSnapshot {
                used_percentage: used,
                resets_at: format!("live:{month}:{day}"),
                meter_key: format!("codex_weekly:{limit_id}"),
            });
        }
    }
    Err(Error::new(
        "Codex reported no weekly (10080-minute) usage window for this account",
    ))
}

/// The weekly meter: one `account/rateLimits/read`, no model call.
pub struct AppServerMeter {
    pub codex: CodexCommand,
}

impl WeeklyMeter for AppServerMeter {
    fn sample(&self, _model: &str, _layout: &Layout) -> Result<WeeklyUsageSnapshot> {
        let mut server = AppServer::start(&self.codex)?;
        let result = server.request("account/rateLimits/read", None)?;
        weekly_snapshot(&result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> Value {
        serde_json::from_str(include_str!(
            "../../../tests/fixtures/codex-app-server.json"
        ))
        .unwrap()
    }

    #[test]
    fn the_weekly_window_is_chosen_by_length() {
        let snapshot = weekly_snapshot(&fixture()["account/rateLimits/read"]["result"]).unwrap();
        assert_eq!(snapshot.used_percentage, 14.0);
        assert_eq!(snapshot.meter_key, "codex_weekly:codex");
        assert!(snapshot.resets_at.starts_with("live:"), "{snapshot:?}");

        // A plan with a five-hour primary and the week in secondary.
        let split = json!({"rateLimits": {
            "limitId": "codex",
            "primary": {"usedPercent": 90, "windowDurationMins": 300, "resetsAt": 1791055017},
            "secondary": {"usedPercent": 31.5, "windowDurationMins": 10080, "resetsAt": 1791055017},
        }});
        assert_eq!(weekly_snapshot(&split).unwrap().used_percentage, 31.5);

        let five_hour_only = json!({"rateLimits": {
            "primary": {"usedPercent": 90, "windowDurationMins": 300, "resetsAt": 1791055017},
            "secondary": null,
        }});
        let error = weekly_snapshot(&five_hour_only).unwrap_err().to_string();
        assert!(error.contains("10080"), "{error}");
    }

    #[test]
    fn only_a_chatgpt_login_is_accepted() {
        assert_eq!(
            check_account(&fixture()["account/read"]["result"]).unwrap(),
            "pro"
        );
        let api = json!({"account": {"type": "apiKey"}});
        assert!(check_account(&api)
            .unwrap_err()
            .to_string()
            .contains("ChatGPT plan only"));
        let none = json!({"account": null, "requiresOpenaiAuth": true});
        assert!(check_account(&none)
            .unwrap_err()
            .to_string()
            .contains("not logged in"));
    }

    #[test]
    fn the_catalog_lists_visible_models() {
        let ids = model_ids(&fixture()["model/list"]["result"]);
        assert!(ids.contains(&"gpt-5.6-sol".to_string()), "{ids:?}");
        assert!(ids.contains(&"gpt-6-astra".to_string()), "{ids:?}");
        let hidden = json!({"data": [{"id": "secret", "hidden": true}, {"id": "shown"}]});
        assert_eq!(model_ids(&hidden), vec!["shown".to_string()]);
    }
}
