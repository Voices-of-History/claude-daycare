//! Subscription usage metering through Claude Code's own `/usage` surface.
//!
//! Headless `claude -p` does not run the status line. An interactive Claude in
//! a pseudo-terminal does. This sampler sends no model prompt and strips every
//! API credential before launching the child. It reads the fresh percentage
//! rendered after `/usage` refreshes, plus Claude's recent structured cache for
//! the matching model scope and reset window.

use crate::launch::{DEVICE_TOKEN_ENV, STRIPPED_CHILD_ENV};
use crate::paths::create_private_dir;
use crate::workspace::guard_no_managed_claude;
use crate::{Error, Result};
use serde::Deserialize;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const STARTUP_SETTLE: Duration = Duration::from_secs(2);
const SAMPLE_TIMEOUT: Duration = Duration::from_secs(15);
const EXIT_GRACE: Duration = Duration::from_secs(2);
const CACHE_METADATA_MAX_AGE: Duration = Duration::from_secs(5 * 60 + 30);

#[derive(Debug, Clone, PartialEq)]
pub struct WeeklyUsageSnapshot {
    pub used_percentage: f64,
    pub resets_at: String,
    pub meter_key: String,
}

struct ChildGuard(Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if self.0.try_wait().ok().flatten().is_none() {
            let _ = self.0.kill();
        }
        let _ = self.0.wait();
    }
}

struct CaptureGuard(PathBuf);

impl Drop for CaptureGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct UsageCache {
    cached_usage_utilization: CachedUsageUtilization,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CachedUsageUtilization {
    fetched_at_ms: u64,
    utilization: CachedUtilization,
}

#[derive(Deserialize)]
struct CachedUtilization {
    limits: Vec<CachedLimit>,
}

#[derive(Deserialize)]
struct CachedLimit {
    kind: String,
    group: String,
    scope: Option<CachedScope>,
}

#[derive(Deserialize)]
struct CachedScope {
    model: Option<CachedModel>,
}

#[derive(Deserialize)]
struct CachedModel {
    display_name: String,
}

#[derive(Debug, Clone, PartialEq)]
struct FreshUsageRow {
    scope: String,
    used_percentage: Option<f64>,
    reset_window: Option<String>,
}

fn parse_refreshed_usage_cache(
    input: &[u8],
    refresh_started_ms: u64,
    fresh: &FreshUsageRow,
) -> Result<WeeklyUsageSnapshot> {
    let cache: UsageCache = serde_json::from_slice(input)
        .map_err(|error| Error::new(format!("Claude usage cache was not JSON: {error}")))?;
    if cache.cached_usage_utilization.fetched_at_ms + (CACHE_METADATA_MAX_AGE.as_millis() as u64)
        < refresh_started_ms
    {
        return Err(Error::new(
            "Claude /usage left its weekly cache stale; refusing to guess",
        ));
    }
    let matching: Vec<_> = cache
        .cached_usage_utilization
        .utilization
        .limits
        .into_iter()
        .filter(|limit| limit.group == "weekly")
        .filter(|limit| {
            if fresh.scope == "allmodels" {
                return limit.kind == "weekly_all";
            }
            limit.kind == "weekly_scoped"
                && limit
                    .scope
                    .as_ref()
                    .and_then(|scope| scope.model.as_ref())
                    .is_some_and(|model| compact_label(&model.display_name) == fresh.scope)
        })
        .collect();
    if matching.len() != 1 {
        return Err(Error::new(
            "Claude /usage cache did not contain exactly one copy of the weekly meter rendered by the live refresh",
        ));
    }
    let selected = &matching[0];
    let scope = selected
        .scope
        .as_ref()
        .and_then(|scope| scope.model.as_ref())
        .map(|model| model.display_name.as_str())
        .unwrap_or("account");
    let used_percentage = fresh.used_percentage.ok_or_else(|| {
        Error::new("Claude /usage did not render a valid refreshed weekly percentage")
    })?;
    let resets_at = fresh.reset_window.clone().ok_or_else(|| {
        Error::new("Claude /usage did not render the refreshed weekly meter's reset date")
    })?;
    Ok(WeeklyUsageSnapshot {
        used_percentage,
        resets_at,
        meter_key: format!("{}:{scope}", selected.kind),
    })
}

fn parse_fresh_usage_screen(input: &[u8], requested_model: &str) -> Result<FreshUsageRow> {
    let screen = compact_terminal_text(input);
    let refreshed = screen
        .rsplit_once("refreshing")
        .map(|(_, refreshed)| refreshed)
        .ok_or_else(|| Error::new("Claude /usage has not started its live refresh yet"))?;
    let mut rows = Vec::new();
    let mut remaining = refreshed;
    while let Some((_, after_heading)) = remaining.split_once("currentweek(") {
        let Some((scope, after_scope)) = after_heading.split_once(')') else {
            break;
        };
        let block_end = after_scope
            .find("currentweek(")
            .unwrap_or(after_scope.len());
        rows.push(FreshUsageRow {
            scope: scope.to_string(),
            used_percentage: parse_usage_percentage(&after_scope[..block_end]),
            reset_window: parse_reset_window(&after_scope[..block_end]),
        });
        remaining = after_scope;
    }

    let mut matching_scopes: Vec<_> = rows
        .iter()
        .filter(|row| row.scope != "allmodels" && scope_matches_model(&row.scope, requested_model))
        .map(|row| row.scope.as_str())
        .collect();
    matching_scopes.sort_unstable();
    matching_scopes.dedup();
    if matching_scopes.len() > 1 {
        return Err(Error::new(
            "Claude /usage rendered more than one applicable model-scoped weekly meter",
        ));
    }
    let selected = if let Some(scope) = matching_scopes.first() {
        rows.iter().rev().find(|row| row.scope == **scope)
    } else {
        rows.iter().rev().find(|row| row.scope == "allmodels")
    };
    selected.cloned().ok_or_else(|| {
        Error::new("Claude /usage did not render an applicable weekly meter after refreshing")
    })
}

fn parse_usage_percentage(reading: &str) -> Option<f64> {
    for (percent_at, _) in reading.match_indices('%') {
        let suffix = &reading[percent_at + 1..];
        if !suffix.starts_with('u') {
            continue;
        }
        let prefix = &reading[..percent_at];
        let digits_reversed: String = prefix
            .chars()
            .rev()
            .take_while(|ch| ch.is_ascii_digit() || *ch == '.')
            .collect();
        let digits: String = digits_reversed.chars().rev().collect();
        if let Ok(percent) = digits.parse::<f64>() {
            if percent.is_finite() && (0.0..=100.0).contains(&percent) {
                return Some(percent);
            }
        }
    }
    None
}

fn parse_reset_window(reading: &str) -> Option<String> {
    const MONTHS: [&str; 12] = [
        "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
    ];
    let mut earliest: Option<(usize, usize, u8)> = None;
    for (month_index, month) in MONTHS.iter().enumerate() {
        for (position, _) in reading.match_indices(month) {
            let day_text: String = reading[position + month.len()..]
                .chars()
                .take_while(char::is_ascii_digit)
                .collect();
            let Ok(day) = day_text.parse::<u8>() else {
                continue;
            };
            if !(1..=31).contains(&day) {
                continue;
            }
            if earliest.is_none_or(|(best, _, _)| position < best) {
                earliest = Some((position, month_index + 1, day));
            }
        }
    }
    earliest.map(|(_, month, day)| format!("live:{month}:{day}"))
}

fn compact_label(value: &str) -> String {
    value
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

fn compact_terminal_text(input: &[u8]) -> String {
    let mut plain = Vec::with_capacity(input.len());
    let mut index = 0;
    while index < input.len() {
        if input[index] == 0x1b {
            index += 1;
            if index >= input.len() {
                break;
            }
            match input[index] {
                b'[' => {
                    index += 1;
                    while index < input.len() {
                        let byte = input[index];
                        index += 1;
                        if (0x40..=0x7e).contains(&byte) {
                            break;
                        }
                    }
                }
                b']' => {
                    index += 1;
                    while index < input.len() {
                        if input[index] == 0x07 {
                            index += 1;
                            break;
                        }
                        if input[index] == 0x1b && input.get(index + 1).copied() == Some(b'\\') {
                            index += 2;
                            break;
                        }
                        index += 1;
                    }
                }
                _ => index += 1,
            }
            continue;
        }
        let byte = input[index];
        index += 1;
        if byte.is_ascii() && !byte.is_ascii_whitespace() && !byte.is_ascii_control() {
            plain.push(byte.to_ascii_lowercase());
        }
    }
    String::from_utf8(plain).unwrap_or_default()
}

fn scope_matches_model(display_name: &str, requested_model: &str) -> bool {
    match requested_model {
        "opus" => display_name.contains("opus"),
        // Claude's rolling Sonnet alias currently resolves to Fable on Max
        // accounts, and the usage cache labels that scoped allowance Fable.
        "sonnet" => display_name.contains("sonnet") || display_name.contains("fable"),
        _ => false,
    }
}

/// Refresh and read the subscription meter without sending a model prompt.
pub fn sample_weekly_usage(
    claude_bin: &str,
    model: &str,
    cwd: &Path,
    private_dir: &Path,
) -> Result<WeeklyUsageSnapshot> {
    guard_no_managed_claude(claude_bin)?;
    create_private_dir(private_dir)?;
    let capture = capture_path(private_dir);
    let _capture_guard = CaptureGuard(capture.clone());
    let home = std::env::var_os("HOME")
        .ok_or_else(|| Error::new("HOME is not set, so Claude's usage cache is unavailable"))?;
    let cache_path = PathBuf::from(home).join(".claude.json");

    let capture = capture
        .to_str()
        .ok_or_else(|| Error::new("Claude usage transcript path is not valid UTF-8"))?;
    let claude_args = [
        "--safe-mode",
        "--model",
        model,
        "--setting-sources",
        "",
        "--tools",
        "",
        "--strict-mcp-config",
        "--mcp-config",
        r#"{"mcpServers":{}}"#,
        "--permission-mode",
        "dontAsk",
        "--no-chrome",
    ];
    let mut process = Command::new("/usr/bin/script");
    configure_script_command(&mut process, capture, claude_bin, &claude_args)?;
    process
        .current_dir(cwd)
        .env("DAYCARE_USAGE_SAMPLER", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    for name in STRIPPED_CHILD_ENV {
        process.env_remove(name);
    }
    process.env_remove(DEVICE_TOKEN_ENV);

    let mut child = ChildGuard(process.spawn().map_err(|error| {
        Error::new(format!(
            "could not open Claude's subscription usage meter: {error}"
        ))
    })?);
    thread::sleep(STARTUP_SETTLE);
    let mut stdin = child
        .0
        .stdin
        .take()
        .ok_or_else(|| Error::new("Claude usage meter stdin was unavailable"))?;
    let refresh_started_ms = unix_millis();
    stdin.write_all(b"/usage\r")?;
    stdin.flush()?;

    let started = Instant::now();
    let sample = loop {
        if let (Ok(cache), Ok(screen)) = (fs::read(&cache_path), fs::read(&capture)) {
            if let Ok(fresh) = parse_fresh_usage_screen(&screen, model) {
                if let Ok(snapshot) =
                    parse_refreshed_usage_cache(&cache, refresh_started_ms, &fresh)
                {
                    break Ok(snapshot);
                }
            }
        }
        if child.0.try_wait()?.is_some() {
            break Err(usage_unavailable_error(
                "Claude exited before /usage refreshed its weekly cache",
                &cache_path,
            ));
        }
        if started.elapsed() >= SAMPLE_TIMEOUT {
            break Err(usage_unavailable_error(
                "Claude /usage did not refresh its weekly cache within 15 seconds",
                &cache_path,
            ));
        }
        thread::sleep(Duration::from_millis(100));
    };

    // Leave the usage overlay, then ask the interactive shell to exit. A stuck
    // UI is killed after a short grace; it has received no model prompt.
    let _ = stdin.write_all(b"\x1b");
    let _ = stdin.flush();
    thread::sleep(Duration::from_millis(100));
    let _ = stdin.write_all(b"/exit\r");
    let _ = stdin.flush();
    drop(stdin);
    let exit_started = Instant::now();
    while child.0.try_wait()?.is_none() && exit_started.elapsed() < EXIT_GRACE {
        thread::sleep(Duration::from_millis(50));
    }
    if child.0.try_wait()?.is_none() {
        let _ = child.0.kill();
        let _ = child.0.wait();
    }
    sample
}

fn configure_script_command(
    process: &mut Command,
    capture: &str,
    claude_bin: &str,
    claude_args: &[&str],
) -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        // BSD script accepts the output file followed by an argv vector. -F
        // flushes each write so the sampler can inspect the live TUI.
        process
            .args(["-q", "-F", capture, claude_bin])
            .args(claude_args);
        return Ok(());
    }

    #[cfg(target_os = "linux")]
    {
        // util-linux script takes the child through --command instead of an
        // argv tail. Quote every word independently: claude_bin is supplied by
        // the caller and may contain spaces or shell metacharacters.
        let command = std::iter::once(claude_bin)
            .chain(claude_args.iter().copied())
            .map(crate::paths::shell_quote)
            .collect::<Vec<_>>()
            .join(" ");
        process
            .args(["-q", "-f", "-e", "-c", &command, capture])
            .env("SHELL", "/bin/sh");
        return Ok(());
    }

    #[allow(unreachable_code)]
    Err(Error::new(
        "weekly usage metering requires a runner with a supported script(1) implementation",
    ))
}

fn usage_unavailable_error(message: &str, cache_path: &Path) -> Error {
    let onboarding_incomplete = fs::read(cache_path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
        .and_then(|value| {
            value
                .get("hasCompletedOnboarding")
                .and_then(|flag| flag.as_bool())
        })
        != Some(true);
    if onboarding_incomplete {
        return Error::new(format!(
            "{message}. Claude Code's one-time setup is incomplete; run `claude`, finish setup, exit, then retry"
        ));
    }
    Error::new(message)
}

fn unix_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

fn capture_path(private_dir: &Path) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    private_dir.join(format!("usage-{}-{nanos}.log", std::process::id()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selects_the_active_model_scoped_weekly_meter() {
        let fresh = parse_fresh_usage_screen(
            b"Refreshing...Current week (Fable) 92% used Resets Sep 2 at 3am",
            "sonnet",
        )
        .unwrap();
        let sample = parse_refreshed_usage_cache(
            br#"{"cachedUsageUtilization":{"fetchedAtMs":2000,"utilization":{"limits":[{"kind":"weekly_all","group":"weekly","percent":65,"resets_at":"2026-09-02T07:00:00Z","is_active":false,"scope":null},{"kind":"weekly_scoped","group":"weekly","percent":91,"resets_at":"2026-09-02T07:00:00Z","is_active":true,"scope":{"model":{"display_name":"Fable"}}}]}}}"#,
            1000,
            &fresh,
        )
        .unwrap();
        assert_eq!(sample.used_percentage, 92.0);
        assert_eq!(sample.meter_key, "weekly_scoped:Fable");
    }

    #[test]
    fn rejects_stale_or_ambiguous_scoped_caches() {
        let account = FreshUsageRow {
            scope: "allmodels".to_string(),
            used_percentage: Some(65.0),
            reset_window: Some("live:9:2".to_string()),
        };
        let stale = br#"{"cachedUsageUtilization":{"fetchedAtMs":1,"utilization":{"limits":[{"kind":"weekly_all","group":"weekly","percent":65,"resets_at":"later","is_active":false,"scope":null}]}}}"#;
        assert!(parse_refreshed_usage_cache(stale, 400_000, &account).is_err());

        let ambiguous = br#"{"cachedUsageUtilization":{"fetchedAtMs":1000,"utilization":{"limits":[{"kind":"weekly_scoped","group":"weekly","percent":65,"resets_at":"later","is_active":true,"scope":{"model":{"display_name":"Fable"}}},{"kind":"weekly_scoped","group":"weekly","percent":25,"resets_at":"later","is_active":true,"scope":{"model":{"display_name":"Sonnet"}}}]}}}"#;
        let fresh = parse_fresh_usage_screen(
            b"Refreshing...Current week (Fable) 65% used Resets Sep 2 Current week (Sonnet) 25% used Resets Sep 2",
            "sonnet",
        );
        assert!(fresh.is_err());
        assert!(parse_refreshed_usage_cache(ambiguous, 1000, &account).is_err());
    }

    #[test]
    fn ignores_an_active_scope_for_a_different_visit_model() {
        let fresh = parse_fresh_usage_screen(
            b"Refreshing...Current week (Fable) 91% used Resets Sep 2 Current week (all models) 65% used Resets Sep 2",
            "opus",
        )
        .unwrap();
        let sample = parse_refreshed_usage_cache(
            br#"{"cachedUsageUtilization":{"fetchedAtMs":2000,"utilization":{"limits":[{"kind":"weekly_all","group":"weekly","percent":65,"resets_at":"account-reset","is_active":false,"scope":null},{"kind":"weekly_scoped","group":"weekly","percent":91,"resets_at":"fable-reset","is_active":true,"scope":{"model":{"display_name":"Fable"}}}]}}}"#,
            1000,
            &fresh,
        )
        .unwrap();
        assert_eq!(sample.used_percentage, 65.0);
        assert_eq!(sample.meter_key, "weekly_all:account");
    }

    #[test]
    fn accepts_only_the_live_refresh_after_the_requested_meter_label() {
        let screen = b"Current week (all models) 64% used Resets Aug 26\r\nRefreshing...\r\nCurrent week (all models) 65% used Resets Sep 2";
        assert_eq!(
            parse_fresh_usage_screen(screen, "opus").unwrap(),
            FreshUsageRow {
                scope: "allmodels".to_string(),
                used_percentage: Some(65.0),
                reset_window: Some("live:9:2".to_string()),
            }
        );
        assert!(parse_fresh_usage_screen(
            b"Current week (all models) 64% used\r\nRefreshing...",
            "opus"
        )
        .is_err());
    }

    #[test]
    fn reads_the_model_scoped_tui_even_when_cursor_output_drops_a_letter() {
        let screen =
            b"Refreshing...\x1b[2JCurrent week (Fable)\r\n\xe2\x96\x88\xe2\x96\x88 92% usd\r\nRests Sep 2 at 2:59am";
        assert_eq!(
            parse_fresh_usage_screen(screen, "sonnet").unwrap(),
            FreshUsageRow {
                scope: "fable".to_string(),
                used_percentage: Some(92.0),
                reset_window: Some("live:9:2".to_string()),
            }
        );
    }

    #[test]
    fn never_borrows_a_later_rows_percentage() {
        let fresh = parse_fresh_usage_screen(
            b"Refreshing...Current week (Fable) Resets Sep 2 Current week (all models) 65% used Resets Sep 2",
            "sonnet",
        )
        .unwrap();
        assert_eq!(fresh.scope, "fable");
        assert_eq!(fresh.used_percentage, None);
        assert_eq!(fresh.reset_window.as_deref(), Some("live:9:2"));
    }

    #[test]
    fn a_live_model_scope_must_exist_in_the_recent_structured_cache() {
        let fresh = parse_fresh_usage_screen(
            b"Refreshing...Current week (Opus) 11% used Resets Sep 2",
            "opus",
        )
        .unwrap();
        let stale_scope = br#"{"cachedUsageUtilization":{"fetchedAtMs":2000,"utilization":{"limits":[{"kind":"weekly_all","group":"weekly","percent":65,"resets_at":"account-reset","scope":null},{"kind":"weekly_scoped","group":"weekly","percent":91,"resets_at":"fable-reset","scope":{"model":{"display_name":"Fable"}}}]}}}"#;
        assert!(parse_refreshed_usage_cache(stale_scope, 1000, &fresh).is_err());
    }
}
