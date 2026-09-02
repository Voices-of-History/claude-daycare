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
/// A slow `/usage` is a flake, not a verdict. Each attempt is a fresh Claude
/// process; the pause between attempts is jittered so two runners on one box
/// do not retry in lock-step. Worst case per call: 3 × (2 s settle + 15 s wait)
/// plus at most 2 × 1.5 s of pauses, about 54 s.
pub const SAMPLE_ATTEMPTS: u32 = 3;
const RETRY_PAUSE_MIN: Duration = Duration::from_millis(500);
const RETRY_PAUSE_SPAN_MS: u64 = 1_000;
/// How many consecutive mid-visit sampling rounds may go unanswered before the
/// visit ends. One round is a full `SAMPLE_ATTEMPTS` sequence.
pub const MAX_CONSECUTIVE_METER_MISSES: u32 = 3;
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
    #[serde(default)]
    percent: Option<f64>,
    #[serde(default)]
    resets_at: Option<String>,
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

/// Anthropic's per-model usage endpoint is itself rate limited now and then.
/// Claude's `/usage` then renders "Per-model breakdown unavailable (rate
/// limited — try again in a moment)" after `Refreshing…` and never draws a
/// `Current week` row, so the live parser cannot answer no matter how long
/// the sampler waits. This is that screen, not a slow one.
fn usage_screen_is_rate_limited(input: &[u8]) -> bool {
    let screen = compact_terminal_text(input);
    screen
        .rsplit_once("refreshing")
        .is_some_and(|(_, refreshed)| refreshed.contains("ratelimited"))
}

/// A weekly reading taken from Claude's own structured cache, used only when
/// the live `/usage` refresh is rate limited. The selection rule is the live
/// one: the `weekly_scoped` limit whose model matches the visit's model, else
/// `weekly_all`. The 330 s freshness cutoff does not apply here — the cache
/// is the best reading available — but a cache whose reset instant has
/// already passed describes a window that no longer exists and is refused.
struct CachedWeeklyReading {
    snapshot: WeeklyUsageSnapshot,
    fetched_at_ms: u64,
}

fn parse_cached_weekly_usage(
    input: &[u8],
    requested_model: &str,
    now_ms: u64,
) -> Result<CachedWeeklyReading> {
    let cache: UsageCache = serde_json::from_slice(input)
        .map_err(|error| Error::new(format!("Claude usage cache was not JSON: {error}")))?;
    let fetched_at_ms = cache.cached_usage_utilization.fetched_at_ms;
    let limits: Vec<CachedLimit> = cache
        .cached_usage_utilization
        .utilization
        .limits
        .into_iter()
        .filter(|limit| limit.group == "weekly")
        .collect();
    let scoped: Vec<&CachedLimit> = limits
        .iter()
        .filter(|limit| limit.kind == "weekly_scoped")
        .filter(|limit| {
            limit
                .scope
                .as_ref()
                .and_then(|scope| scope.model.as_ref())
                .is_some_and(|model| {
                    scope_matches_model(&compact_label(&model.display_name), requested_model)
                })
        })
        .collect();
    if scoped.len() > 1 {
        return Err(Error::new(
            "Claude's usage cache holds more than one applicable model-scoped weekly meter",
        ));
    }
    let selected = match scoped.first() {
        Some(limit) => *limit,
        None => limits
            .iter()
            .find(|limit| limit.kind == "weekly_all")
            .ok_or_else(|| Error::new("Claude's usage cache holds no weekly meter"))?,
    };
    let used_percentage = selected
        .percent
        .filter(|percent| percent.is_finite() && (0.0..=100.0).contains(percent))
        .ok_or_else(|| Error::new("Claude's usage cache holds no valid weekly percentage"))?;
    let resets_at_iso = selected
        .resets_at
        .as_deref()
        .ok_or_else(|| Error::new("Claude's usage cache holds no weekly reset date"))?;
    let (year, month, day) = parse_iso_date(resets_at_iso)
        .ok_or_else(|| Error::new("Claude's usage cache holds an unreadable weekly reset date"))?;
    let resets_at_secs = parse_iso_unix_secs(resets_at_iso)
        .ok_or_else(|| Error::new("Claude's usage cache holds an unreadable weekly reset date"))?;
    if resets_at_secs <= now_ms / 1000 {
        return Err(Error::new(format!(
            "Claude's cached weekly meter reset on {year}-{month:02}-{day:02}; refusing to guess at the new window"
        )));
    }
    let scope = selected
        .scope
        .as_ref()
        .and_then(|scope| scope.model.as_ref())
        .map(|model| model.display_name.as_str())
        .unwrap_or("account");
    Ok(CachedWeeklyReading {
        snapshot: WeeklyUsageSnapshot {
            used_percentage,
            resets_at: format!("live:{month}:{day}"),
            meter_key: format!("{}:{scope}", selected.kind),
        },
        fetched_at_ms,
    })
}

/// `YYYY-MM-DD` from the front of an ISO 8601 timestamp.
fn parse_iso_date(value: &str) -> Option<(i64, u32, u32)> {
    let mut parts = value.get(..10)?.split('-');
    let year: i64 = parts.next()?.parse().ok()?;
    let month: u32 = parts.next()?.parse().ok()?;
    let day: u32 = parts.next()?.parse().ok()?;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    Some((year, month, day))
}

/// Unix seconds for an ISO 8601 timestamp such as
/// `2026-09-02T06:59:59.742500+00:00` or `2026-09-02T06:59:59Z`. Handles the
/// numeric offset forms Claude writes; fractional seconds are dropped.
fn parse_iso_unix_secs(value: &str) -> Option<u64> {
    let (year, month, day) = parse_iso_date(value)?;
    let rest = value.get(10..)?;
    let rest = rest.strip_prefix('T').or_else(|| rest.strip_prefix(' '))?;
    let hour: i64 = rest.get(0..2)?.parse().ok()?;
    let minute: i64 = rest.get(3..5)?.parse().ok()?;
    let second: i64 = rest.get(6..8)?.parse().ok()?;
    let tail = rest.get(8..)?;
    let tail = match tail.find(|ch| ch == 'Z' || ch == '+' || ch == '-') {
        Some(at) => &tail[at..],
        None => "",
    };
    let offset_secs: i64 = match tail.chars().next() {
        None | Some('Z') => 0,
        Some(sign) => {
            let sign = if sign == '-' { -1 } else { 1 };
            let digits: String = tail[1..].chars().filter(char::is_ascii_digit).collect();
            let hours: i64 = digits.get(0..2)?.parse().ok()?;
            let minutes: i64 = digits.get(2..4).map_or(Some(0), |m| m.parse().ok())?;
            sign * (hours * 3600 + minutes * 60)
        }
    };
    // Howard Hinnant's days-from-civil.
    let y = if month <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let m = month as i64;
    let d = day as i64;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    let secs = days * 86_400 + hour * 3600 + minute * 60 + second - offset_secs;
    u64::try_from(secs).ok()
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
///
/// Retries a slow or empty answer up to [`SAMPLE_ATTEMPTS`] times, each in a
/// fresh Claude process. The final error says what the owner can do.
pub fn sample_weekly_usage(
    claude_bin: &str,
    model: &str,
    cwd: &Path,
    private_dir: &Path,
) -> Result<WeeklyUsageSnapshot> {
    guard_no_managed_claude(claude_bin)?;
    let home = std::env::var_os("HOME")
        .ok_or_else(|| Error::new("HOME is not set, so Claude's usage cache is unavailable"))?;
    let cache_path = PathBuf::from(home).join(".claude.json");
    retry_sample(
        SAMPLE_ATTEMPTS,
        |attempt| {
            let outcome =
                sample_weekly_usage_once(claude_bin, model, cwd, private_dir, &cache_path);
            if let Err(error) = &outcome {
                eprintln!(
                    "usage meter attempt {attempt} of {SAMPLE_ATTEMPTS} did not answer: {error}"
                );
            }
            outcome
        },
        |pause| thread::sleep(pause),
    )
    .map_err(|last| usage_unavailable_error(&meter_gave_up_message(&last), &cache_path))
}

/// Run `attempt` up to `attempts` times, pausing a jittered moment between
/// tries. Returns the last attempt's error when every try fails.
fn retry_sample<T>(
    attempts: u32,
    mut attempt: impl FnMut(u32) -> Result<T>,
    mut pause: impl FnMut(Duration),
) -> std::result::Result<T, Error> {
    let attempts = attempts.max(1);
    let mut last = None;
    for number in 1..=attempts {
        match attempt(number) {
            Ok(value) => return Ok(value),
            Err(error) => last = Some(error),
        }
        if number < attempts {
            pause(retry_pause());
        }
    }
    Err(last.unwrap_or_else(|| Error::new("usage meter was never attempted")))
}

fn retry_pause() -> Duration {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|value| value.subsec_nanos() as u64)
        .unwrap_or(0);
    let salt = nanos ^ (std::process::id() as u64).wrapping_mul(0x9E37_79B9);
    RETRY_PAUSE_MIN + Duration::from_millis(salt % RETRY_PAUSE_SPAN_MS)
}

fn meter_gave_up_message(last: &Error) -> String {
    format!("Claude's /usage meter did not answer in {SAMPLE_ATTEMPTS} tries (last try: {last})")
}

/// One mid-visit outage counter: how many sampling rounds in a row have gone
/// unanswered. A miss with an earlier reading keeps the visit going on that
/// reading; the visit ends only once the meter has stayed silent for
/// [`MAX_CONSECUTIVE_METER_MISSES`] rounds.
#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub struct MeterOutage {
    consecutive_misses: u32,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum MeterMiss {
    /// Keep playing on the last good reading.
    KeepLastReading { consecutive_misses: u32 },
    /// The meter has been silent too long; end the visit.
    EndVisit,
}

impl MeterOutage {
    pub fn answered(&mut self) {
        self.consecutive_misses = 0;
    }

    pub fn missed(&mut self) -> MeterMiss {
        self.consecutive_misses = self.consecutive_misses.saturating_add(1);
        if self.consecutive_misses >= MAX_CONSECUTIVE_METER_MISSES {
            MeterMiss::EndVisit
        } else {
            MeterMiss::KeepLastReading {
                consecutive_misses: self.consecutive_misses,
            }
        }
    }
}

fn sample_weekly_usage_once(
    claude_bin: &str,
    model: &str,
    cwd: &Path,
    private_dir: &Path,
    cache_path: &Path,
) -> Result<WeeklyUsageSnapshot> {
    create_private_dir(private_dir)?;
    let capture = capture_path(private_dir);
    let _capture_guard = CaptureGuard(capture.clone());

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
        if let (Ok(cache), Ok(screen)) = (fs::read(cache_path), fs::read(&capture)) {
            if let Ok(fresh) = parse_fresh_usage_screen(&screen, model) {
                if let Ok(snapshot) =
                    parse_refreshed_usage_cache(&cache, refresh_started_ms, &fresh)
                {
                    break Ok(snapshot);
                }
            } else if usage_screen_is_rate_limited(&screen) {
                // Waiting cannot help: the live refresh will not draw a weekly
                // row until Anthropic's limit lifts. Fall back to Claude's own
                // cached reading at once instead of spending the timeout and
                // two more attempts on the same screen.
                break rate_limited_fallback(&cache, model, unix_millis());
            }
        }
        if child.0.try_wait()?.is_some() {
            break Err(Error::new(
                "Claude exited before /usage refreshed its weekly cache",
            ));
        }
        if started.elapsed() >= SAMPLE_TIMEOUT {
            break Err(Error::new(format!(
                "Claude /usage did not refresh its weekly cache within {} seconds",
                SAMPLE_TIMEOUT.as_secs()
            )));
        }
        thread::sleep(Duration::from_millis(100));
    };

    // A failed attempt has nothing to hand back: kill the child at once so the
    // retry starts fresh instead of spending the exit grace on a stuck UI.
    if sample.is_err() {
        drop(stdin);
        let _ = child.0.kill();
        let _ = child.0.wait();
        return sample;
    }

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

fn rate_limited_fallback(cache: &[u8], model: &str, now_ms: u64) -> Result<WeeklyUsageSnapshot> {
    let reading = parse_cached_weekly_usage(cache, model, now_ms).map_err(|error| {
        Error::new(format!(
            "Claude /usage is rate limited and its cached weekly reading is unusable: {error}"
        ))
    })?;
    let age_minutes = now_ms.saturating_sub(reading.fetched_at_ms) / 60_000;
    eprintln!(
        "usage meter is rate limited; using Claude's cached reading from {age_minutes} minutes ago"
    );
    Ok(reading.snapshot)
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
    Error::new(format!(
        "{message}. Run `claude`, type /usage once by hand, exit, then retry"
    ))
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
    use std::cell::Cell;

    #[test]
    fn retries_a_slow_sample_up_to_three_times_then_reports_the_last_error() {
        let calls = Cell::new(0u32);
        let pauses = Cell::new(0u32);
        let result: std::result::Result<(), Error> = retry_sample(
            SAMPLE_ATTEMPTS,
            |attempt| {
                calls.set(calls.get() + 1);
                assert_eq!(attempt, calls.get());
                Err(Error::new(format!("try {attempt} timed out")))
            },
            |pause| {
                pauses.set(pauses.get() + 1);
                assert!(pause >= RETRY_PAUSE_MIN);
                assert!(pause < RETRY_PAUSE_MIN + Duration::from_millis(RETRY_PAUSE_SPAN_MS));
            },
        );
        assert_eq!(calls.get(), 3);
        assert_eq!(pauses.get(), 2, "no pause after the final attempt");
        assert_eq!(result.unwrap_err().to_string(), "try 3 timed out");
    }

    #[test]
    fn a_single_slow_sample_never_fails_the_call() {
        let calls = Cell::new(0u32);
        let result = retry_sample(
            SAMPLE_ATTEMPTS,
            |attempt| {
                calls.set(calls.get() + 1);
                if attempt == 1 {
                    Err(Error::new("first try timed out"))
                } else {
                    Ok(attempt)
                }
            },
            |_| {},
        );
        assert_eq!(result.unwrap(), 2);
        assert_eq!(calls.get(), 2);
    }

    #[test]
    fn the_gave_up_sentence_tells_the_owner_what_to_do() {
        let dir = crate::testdir::unique_dir("daycare-usage-meter-error");
        let cache = dir.join(".claude.json");
        fs::write(&cache, br#"{"hasCompletedOnboarding":true}"#).unwrap();
        let message = usage_unavailable_error(
            &meter_gave_up_message(&Error::new(
                "Claude /usage did not refresh within 15 seconds",
            )),
            &cache,
        )
        .to_string();
        assert_eq!(
            message,
            "Claude's /usage meter did not answer in 3 tries (last try: Claude /usage did not refresh within 15 seconds). Run `claude`, type /usage once by hand, exit, then retry"
        );
        fs::write(&cache, br#"{}"#).unwrap();
        let onboarding = usage_unavailable_error("meter silent", &cache).to_string();
        assert!(
            onboarding.contains("one-time setup is incomplete"),
            "{onboarding}"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_mid_visit_miss_keeps_the_last_reading_until_three_rounds_in_a_row() {
        let mut outage = MeterOutage::default();
        assert_eq!(
            outage.missed(),
            MeterMiss::KeepLastReading {
                consecutive_misses: 1
            }
        );
        assert_eq!(
            outage.missed(),
            MeterMiss::KeepLastReading {
                consecutive_misses: 2
            }
        );
        // One good answer clears the streak.
        outage.answered();
        assert_eq!(
            outage.missed(),
            MeterMiss::KeepLastReading {
                consecutive_misses: 1
            }
        );
        assert_eq!(
            outage.missed(),
            MeterMiss::KeepLastReading {
                consecutive_misses: 2
            }
        );
        assert_eq!(outage.missed(), MeterMiss::EndVisit);
    }

    /// Stripped from the Otto capture on cloud-agents
    /// (usage-1058982-1788311972567916842.log, 2026-09-02T01:19:32Z): the
    /// pre-refresh screen carries a weekly row, the refresh draws only the
    /// rate-limited notice.
    const RATE_LIMITED_SCREEN: &[u8] = b"Current week (all models)\r\n\xe2\x96\x88\xe2\x96\x88\xe2\x96\x88 39% used\r\nResets Sep 2 at 3am\r\nCurrent week (Fable)\r\n\xe2\x96\x88\xe2\x96\x88 73% used\r\nResets Sep 2 at 3am\r\n\x1b[2JRefreshing\xe2\x80\xa6\r\n\x1b[2JPer-model breakdown unavailable (rate limited \xe2\x80\x94 try again in a moment) \xc2\xb7 r to retry \xc2\xb7 Esc to cancel\r\n";

    const OTTO_CACHE: &[u8] = br#"{"cachedUsageUtilization":{"fetchedAtMs":1788330000000,"utilization":{"limits":[{"kind":"session","group":"session","percent":4,"resets_at":"2026-09-02T11:09:59.742472+00:00","scope":null,"is_active":false},{"kind":"weekly_all","group":"weekly","percent":39,"resets_at":"2026-09-02T06:59:59.742500+00:00","scope":null,"is_active":false},{"kind":"weekly_scoped","group":"weekly","percent":73,"resets_at":"2026-09-02T06:59:59.742772+00:00","scope":{"model":{"id":null,"display_name":"Fable"}},"is_active":true}]}}}"#;

    #[test]
    fn a_rate_limited_refresh_falls_back_to_claudes_cached_weekly_reading() {
        // The live parser cannot answer: no weekly row after Refreshing.
        assert!(parse_fresh_usage_screen(RATE_LIMITED_SCREEN, "sonnet").is_err());
        assert!(usage_screen_is_rate_limited(RATE_LIMITED_SCREEN));
        // A slow screen is not a rate-limited one.
        assert!(!usage_screen_is_rate_limited(
            b"Current week (all models) 39% used\r\nRefreshing..."
        ));

        // 495 s after the cache was written, before the window resets.
        let now_ms = 1788330000000 + 495_000;
        let snapshot = rate_limited_fallback(OTTO_CACHE, "sonnet", now_ms).unwrap();
        assert_eq!(
            snapshot,
            WeeklyUsageSnapshot {
                used_percentage: 73.0,
                resets_at: "live:9:2".to_string(),
                meter_key: "weekly_scoped:Fable".to_string(),
            }
        );
        // Opus has no scoped meter in this cache: the account meter answers.
        let account = rate_limited_fallback(OTTO_CACHE, "opus", now_ms).unwrap();
        assert_eq!(account.used_percentage, 39.0);
        assert_eq!(account.meter_key, "weekly_all:account");
        assert_eq!(account.resets_at, "live:9:2");
        // Far older than the live path's 330 s cutoff is still accepted.
        assert!(rate_limited_fallback(OTTO_CACHE, "sonnet", now_ms + 30 * 60 * 1000).is_ok());
    }

    #[test]
    fn a_cache_whose_window_already_reset_is_refused_even_when_rate_limited() {
        // 2026-09-02T07:00:00Z is one tick past the cached resets_at.
        let after_reset_ms = parse_iso_unix_secs("2026-09-02T07:00:00Z").unwrap() * 1000;
        let error = rate_limited_fallback(OTTO_CACHE, "sonnet", after_reset_ms).unwrap_err();
        assert!(error.to_string().contains("rate limited"), "{error}");
        assert!(error.to_string().contains("reset on 2026-09-02"), "{error}");
        // The same cache two seconds earlier still answers.
        assert!(rate_limited_fallback(OTTO_CACHE, "sonnet", after_reset_ms - 2000).is_ok());
    }

    #[test]
    fn reads_claudes_iso_reset_instants() {
        assert_eq!(
            parse_iso_unix_secs("2026-09-02T06:59:59.742500+00:00"),
            Some(1_788_332_399)
        );
        assert_eq!(
            parse_iso_unix_secs("2026-09-02T06:59:59Z"),
            Some(1_788_332_399)
        );
        assert_eq!(
            parse_iso_unix_secs("2026-09-02T01:59:59-05:00"),
            Some(1_788_332_399)
        );
        assert_eq!(parse_iso_unix_secs("later"), None);
    }

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
