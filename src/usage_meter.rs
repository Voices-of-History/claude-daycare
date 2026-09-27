//! Subscription usage metering through Claude Code's own `/usage` surface.
//!
//! Headless `claude -p` does not report the plan's weekly meter. An
//! interactive Claude in a pseudo-terminal does, through `/usage`. This
//! sampler opens one with no tools, no MCP servers, no customizations, and
//! every API credential stripped, types `/usage`, and never sends a model
//! prompt.
//!
//! Two independent readings can answer, so a small change to either one does
//! not blind the meter:
//!
//! 1. Claude's structured cache (`cachedUsageUtilization` in `.claude.json`),
//!    accepted only when it was fetched after `/usage` was typed, or when the
//!    screen shows that refresh finished and the cache is minutes old at most.
//! 2. The `/usage` screen itself, replayed through [`crate::terminal`] into
//!    what a person would see, accepted only after it was seen refreshing and
//!    the refresh has finished.
//!
//! Claude asks whether to trust a folder the first time it opens there, with
//! "No, exit" selected. The sampler therefore runs in its own empty folder
//! (`Layout::usage_meter_dir`), and answers "Yes, I trust this folder" only
//! when the question names that folder. Claude records the answer itself, so
//! it is asked once per machine.

use crate::launch::{DEVICE_TOKEN_ENV, STRIPPED_CHILD_ENV};
use crate::paths::{create_private_dir, write_atomic, Layout};
use crate::terminal::Screen;
use crate::workspace::guard_no_managed_claude;
use crate::{Error, Result};
use serde::Deserialize;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Claude is given at least this long to draw its prompt before `/usage` is
/// typed, and its screen must then have been still for `STARTUP_QUIET`.
const STARTUP_SETTLE: Duration = Duration::from_secs(2);
const STARTUP_QUIET: Duration = Duration::from_millis(500);
/// A child that never draws anything is typed at after this long anyway.
const STARTUP_BLANK_LIMIT: Duration = Duration::from_secs(6);
/// Startup, the one-time trust question included, must finish within this.
const STARTUP_TIMEOUT: Duration = Duration::from_secs(20);
/// After a key answers the trust question, the screen gets this long to move.
const KEY_PACE: Duration = Duration::from_millis(400);
/// More presses than this on the trust question means it is not the one the
/// sampler knows how to answer.
const MAX_TRUST_KEYS: u32 = 4;
const SAMPLE_TIMEOUT: Duration = Duration::from_secs(15);
const EXIT_GRACE: Duration = Duration::from_secs(2);
/// A slow `/usage` is a flake, not a verdict. Each attempt is a fresh Claude
/// process; the pause between attempts is jittered so two runners on one box
/// do not retry in lock-step. Worst case per call: 3 × (20 s startup + 15 s
/// wait) plus at most 2 × 1.5 s of pauses, under two minutes.
pub const SAMPLE_ATTEMPTS: u32 = 3;
const RETRY_PAUSE_MIN: Duration = Duration::from_millis(500);
const RETRY_PAUSE_SPAN_MS: u64 = 1_000;
/// How many consecutive mid-visit sampling rounds may go unanswered before the
/// visit ends. One round is a full `SAMPLE_ATTEMPTS` sequence.
pub const MAX_CONSECUTIVE_METER_MISSES: u32 = 3;
const CACHE_METADATA_MAX_AGE: Duration = Duration::from_secs(5 * 60 + 30);

/// Down arrow and Enter, as a terminal sends them.
const KEY_DOWN: &[u8] = b"\x1b[B";
const KEY_ENTER: &[u8] = b"\r";

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

/// Does a weekly meter's model label cover the visit's model? Only the
/// model's own family counts. Anything else leaves the visit on the
/// all-models meter, which every model's use moves, so a visit is never
/// measured against an allowance its turns do not spend.
fn scope_matches_model(label: &str, requested_model: &str) -> bool {
    let label = compact_label(label);
    match requested_model {
        "opus" => label.contains("opus"),
        "sonnet" => label.contains("sonnet"),
        _ => false,
    }
}

/// A weekly reading taken from Claude's own structured cache.
struct CachedWeeklyReading {
    snapshot: WeeklyUsageSnapshot,
    fetched_at_ms: u64,
}

/// Select the visit's weekly meter from Claude's cache: the `weekly_scoped`
/// limit for the visit's model family, else `weekly_all`. A cache whose reset
/// instant has already passed describes a window that no longer exists and is
/// refused. `utc_offset_secs` turns the reset instant into the local date the
/// `/usage` screen shows, so both readings name a window the same way.
fn parse_cached_weekly_usage(
    input: &[u8],
    requested_model: &str,
    now_ms: u64,
    utc_offset_secs: impl Fn(u64) -> i64,
) -> Result<CachedWeeklyReading> {
    let cache: UsageCache = serde_json::from_slice(input)
        .map_err(|error| Error::new(format!("Claude usage cache was not readable: {error}")))?;
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
                .is_some_and(|model| scope_matches_model(&model.display_name, requested_model))
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
    let resets_at_secs = selected
        .resets_at
        .as_deref()
        .and_then(parse_iso_unix_secs)
        .ok_or_else(|| Error::new("Claude's usage cache holds no readable weekly reset date"))?;
    // Claude's reset instants wobble around the hour (06:59:59.6, then
    // 07:00:00.04). Name the window by the nearest minute so that wobble can
    // never move it to another local date.
    let window_secs = (resets_at_secs + 30) / 60 * 60;
    let (month, day) = month_day(window_secs, utc_offset_secs(window_secs));
    if resets_at_secs <= now_ms / 1000 {
        return Err(Error::new(format!(
            "Claude's cached weekly meter reset on {month}/{day}; refusing to guess at the new window"
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

/// The local calendar month and day of a Unix instant.
fn month_day(unix_secs: u64, utc_offset_secs: i64) -> (u32, u32) {
    let days = (unix_secs as i64 + utc_offset_secs).div_euclid(86_400);
    // Howard Hinnant's civil-from-days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
    (month, day)
}

/// This machine's UTC offset at a given instant, as Claude's screen uses it.
fn local_utc_offset_secs(unix_secs: u64) -> i64 {
    let time = unix_secs as libc::time_t;
    // SAFETY: localtime_r only writes the caller-owned `tm`.
    unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        if libc::localtime_r(&time, &mut tm).is_null() {
            return 0;
        }
        // `c_long` is narrower than i64 on 32-bit targets.
        #[allow(clippy::unnecessary_cast)]
        let offset = tm.tm_gmtoff as i64;
        offset
    }
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
    let tail = match tail.find(['Z', '+', '-']) {
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

/// One "Current week (…)" block as the `/usage` screen shows it.
#[derive(Debug, Clone, PartialEq)]
struct ScreenMeter {
    label: String,
    used_percentage: Option<f64>,
    reset: Option<(u32, u32)>,
}

/// Every weekly block on the rendered screen. A block is its heading and the
/// lines under it up to the next blank line or heading.
fn screen_weekly_meters(screen: &Screen) -> Vec<ScreenMeter> {
    let lines = screen.lines();
    let mut meters = Vec::new();
    for (index, line) in lines.iter().enumerate() {
        let lower = line.to_lowercase();
        let Some(at) = lower.find("current week (") else {
            continue;
        };
        let after = &line[at + "current week (".len()..];
        let Some(close) = after.find(')') else {
            continue;
        };
        let mut meter = ScreenMeter {
            label: after[..close].trim().to_string(),
            used_percentage: None,
            reset: None,
        };
        for body in lines.iter().skip(index + 1).take(4) {
            let body = body.trim();
            if body.is_empty() || body.to_lowercase().starts_with("current ") {
                break;
            }
            meter.used_percentage = meter
                .used_percentage
                .or_else(|| parse_used_percentage(body));
            meter.reset = meter.reset.or_else(|| parse_reset_date(body));
        }
        meters.push(meter);
    }
    meters
}

/// "███ 74% used" → 74.
fn parse_used_percentage(line: &str) -> Option<f64> {
    let lower = line.to_lowercase();
    for (at, _) in lower.match_indices('%') {
        if !lower[at + 1..].trim_start().starts_with("used") {
            continue;
        }
        let digits: String = lower[..at]
            .chars()
            .rev()
            .take_while(|ch| ch.is_ascii_digit() || *ch == '.')
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        if let Ok(percent) = digits.parse::<f64>() {
            if percent.is_finite() && (0.0..=100.0).contains(&percent) {
                return Some(percent);
            }
        }
    }
    None
}

/// "Resets Sep 30 at 2:59am (America/New_York)" → (9, 30). A reset shown as
/// a time alone has no date to read.
fn parse_reset_date(line: &str) -> Option<(u32, u32)> {
    const MONTHS: [&str; 12] = [
        "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
    ];
    let lower = line.to_lowercase();
    let rest = lower.strip_prefix("resets")?;
    let mut words = rest.split_whitespace();
    let month_word = words.next()?;
    let month = MONTHS
        .iter()
        .position(|month| month_word.starts_with(month))? as u32
        + 1;
    let day: u32 = words
        .next()?
        .trim_end_matches(|ch: char| !ch.is_ascii_digit())
        .parse()
        .ok()?;
    (1..=31).contains(&day).then_some((month, day))
}

/// The visit's weekly meter from the rendered screen: the block for the
/// visit's model family, else the all-models block.
fn screen_weekly_usage(screen: &Screen, requested_model: &str) -> Result<WeeklyUsageSnapshot> {
    let meters = screen_weekly_meters(screen);
    let scoped: Vec<&ScreenMeter> = meters
        .iter()
        .filter(|meter| scope_matches_model(&meter.label, requested_model))
        .collect();
    if scoped.len() > 1 {
        return Err(Error::new(
            "Claude /usage showed more than one applicable model-scoped weekly meter",
        ));
    }
    let (meter, meter_key) = match scoped.first() {
        Some(meter) => (*meter, format!("weekly_scoped:{}", meter.label)),
        None => (
            meters
                .iter()
                .find(|meter| compact_label(&meter.label) == "allmodels")
                .ok_or_else(|| Error::new("Claude /usage showed no weekly meter"))?,
            "weekly_all:account".to_string(),
        ),
    };
    let used_percentage = meter
        .used_percentage
        .ok_or_else(|| Error::new("Claude /usage showed no weekly percentage"))?;
    let (month, day) = meter
        .reset
        .ok_or_else(|| Error::new("Claude /usage showed no weekly reset date"))?;
    Ok(WeeklyUsageSnapshot {
        used_percentage,
        resets_at: format!("live:{month}:{day}"),
        meter_key,
    })
}

/// Anthropic's per-model usage endpoint is itself rate limited now and then.
/// Claude's `/usage` then says "rate limited — try again in a moment" and
/// never draws a fresh weekly row, however long the sampler waits.
fn usage_screen_is_rate_limited(screen: &Screen) -> bool {
    screen.contains("rate limited")
}

/// What one look at the child's screen and Claude's cache says.
#[derive(Debug)]
enum Reading {
    Answered(WeeklyUsageSnapshot),
    RateLimited,
    Waiting,
}

/// Everything the sampler knows about one attempt after `/usage` was typed.
struct Observation<'a> {
    /// The whole capture, replayed.
    screen: &'a Screen,
    /// The capture bytes written after `/usage` was typed.
    since_typed: &'a [u8],
    /// `.claude.json`, when it could be read.
    cache: Option<&'a [u8]>,
    typed_at_ms: u64,
    now_ms: u64,
}

fn read_usage(
    observation: &Observation,
    requested_model: &str,
    utc_offset_secs: impl Fn(u64) -> i64 + Copy,
) -> Reading {
    let cached = observation.cache.and_then(|cache| {
        parse_cached_weekly_usage(cache, requested_model, observation.now_ms, utc_offset_secs).ok()
    });
    // Fetched after `/usage` was typed: this attempt's own refresh wrote it.
    if let Some(reading) = &cached {
        if reading.fetched_at_ms >= observation.typed_at_ms {
            return Reading::Answered(reading.snapshot.clone());
        }
    }
    let screen = observation.screen;
    if usage_screen_is_rate_limited(screen) {
        return Reading::RateLimited;
    }
    // The screen said it was refreshing after `/usage` was typed, and no
    // longer does: the refresh finished. A cache written within the last few
    // minutes then stands (Claude may not refetch a reading that fresh), and
    // otherwise the refreshed screen answers for itself.
    let refreshed = compact_terminal_text(observation.since_typed).contains("refreshing")
        && !screen.contains("refreshing")
        && screen.contains("current week");
    if !refreshed {
        return Reading::Waiting;
    }
    if let Some(reading) = cached {
        if reading.fetched_at_ms + CACHE_METADATA_MAX_AGE.as_millis() as u64
            >= observation.typed_at_ms
        {
            return Reading::Answered(reading.snapshot);
        }
    }
    match screen_weekly_usage(screen, requested_model) {
        Ok(snapshot) => Reading::Answered(snapshot),
        Err(_) => Reading::Waiting,
    }
}

fn compact_label(value: &str) -> String {
    value
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

/// Lowercase printable ASCII with escapes and whitespace removed. Good enough
/// to ask whether a word was ever drawn, whatever was drawn over it later.
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

/// What to press, if anything, while Claude asks whether to trust a folder.
#[derive(Debug, PartialEq)]
enum TrustAnswer {
    /// No trust question on screen.
    NotAsked,
    /// The question is on screen mid-redraw, with no answer highlighted yet.
    Drawing,
    /// Move the selection toward "Yes, I trust this folder".
    Down,
    /// "Yes, I trust this folder" is selected.
    Enter,
}

/// Answer Claude's trust question for the meter folder, and only for it.
/// Enter is pressed only once the highlighted choice is the yes; the default
/// choice, "No, exit", is what an unanswered question would pick.
fn trust_answer(screen: &Screen, meter_dir: &Path) -> Result<TrustAnswer> {
    if !(screen.contains("trust this folder") || screen.contains("trust the files in this folder"))
    {
        return Ok(TrustAnswer::NotAsked);
    }
    if !screen_names_folder(screen, meter_dir) {
        return Err(Error::new(format!(
            "Claude asked to trust a folder other than the usage meter's own ({})",
            meter_dir.display()
        )));
    }
    let Some(selected) = screen
        .lines()
        .into_iter()
        .rev()
        .find(|line| line.trim_start().starts_with('\u{276f}'))
    else {
        return Ok(TrustAnswer::Drawing);
    };
    let selected = selected.to_lowercase();
    if selected.contains("yes") && selected.contains("trust") {
        Ok(TrustAnswer::Enter)
    } else {
        Ok(TrustAnswer::Down)
    }
}

/// Claude wraps a long path across lines; compare with whitespace removed.
/// It may show the real path or a `~/` one.
fn screen_names_folder(screen: &Screen, folder: &Path) -> bool {
    let squeeze = |text: &str| -> String { text.split_whitespace().collect() };
    let shown = squeeze(&screen.text());
    let full = folder.to_string_lossy().to_string();
    if shown.contains(&squeeze(&full)) {
        return true;
    }
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .and_then(|home| folder.strip_prefix(home).ok().map(Path::to_path_buf))
        .is_some_and(|relative| {
            shown.contains(&squeeze(&format!("~/{}", relative.to_string_lossy())))
        })
}

/// Where Claude keeps `.claude.json`: `$CLAUDE_CONFIG_DIR` when set, else `$HOME`.
fn claude_global_config() -> Result<PathBuf> {
    if let Some(dir) = std::env::var_os("CLAUDE_CONFIG_DIR") {
        let path = PathBuf::from(dir).join(".claude.json");
        if path.exists() {
            return Ok(path);
        }
    }
    let home = std::env::var_os("HOME")
        .ok_or_else(|| Error::new("HOME is not set, so Claude's usage cache is unavailable"))?;
    Ok(PathBuf::from(home).join(".claude.json"))
}

/// Refresh and read the subscription meter without sending a model prompt.
///
/// Retries a slow or empty answer up to [`SAMPLE_ATTEMPTS`] times, each in a
/// fresh Claude process. The final error says what the owner can do.
pub fn sample_weekly_usage(
    claude_bin: &str,
    model: &str,
    layout: &Layout,
) -> Result<WeeklyUsageSnapshot> {
    guard_no_managed_claude(claude_bin)?;
    let cache_path = claude_global_config()?;
    let screen_file = layout.usage_meter_screen_file();
    retry_sample(
        SAMPLE_ATTEMPTS,
        |attempt| {
            let outcome = sample_weekly_usage_once(claude_bin, model, layout, &cache_path);
            if let Err(error) = &outcome {
                eprintln!(
                    "usage meter attempt {attempt} of {SAMPLE_ATTEMPTS} did not answer: {error}"
                );
            }
            outcome
        },
        thread::sleep,
    )
    .inspect(|_| {
        let _ = fs::remove_file(&screen_file);
    })
    .map_err(|last| {
        usage_unavailable_error(&meter_gave_up_message(&last), &cache_path, &screen_file)
    })
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

/// The meter folder, created empty and owner-only, as Claude will name it.
fn prepare_meter_dir(layout: &Layout) -> Result<PathBuf> {
    create_private_dir(layout.workspace_root())?;
    let dir = layout.usage_meter_dir();
    create_private_dir(&dir)?;
    if fs::read_dir(&dir)?.next().is_some() {
        return Err(Error::new(format!(
            "the usage meter's folder {} should be empty; remove what is in it and retry",
            dir.display()
        )));
    }
    Ok(fs::canonicalize(&dir)?)
}

fn send_keys(stdin: &mut ChildStdin, keys: &[u8]) -> Result<()> {
    stdin.write_all(keys)?;
    stdin.flush()?;
    Ok(())
}

/// Wait for Claude's prompt, answering the one-time trust question for the
/// meter folder on the way.
fn wait_for_prompt(
    child: &mut ChildGuard,
    stdin: &mut ChildStdin,
    capture: &Path,
    meter_dir: &Path,
) -> Result<()> {
    let started = Instant::now();
    let mut last_len = 0;
    let mut last_change = started;
    let mut last_key: Option<Instant> = None;
    let mut keys_sent = 0;
    loop {
        let bytes = fs::read(capture).unwrap_or_default();
        if bytes.len() != last_len {
            last_len = bytes.len();
            last_change = Instant::now();
        }
        let screen = Screen::render(&bytes);
        let key_settled = last_key.is_none_or(|at| at.elapsed() >= KEY_PACE);
        match trust_answer(&screen, meter_dir)? {
            TrustAnswer::NotAsked => {
                let drawn = !screen.text().trim().is_empty();
                if key_settled
                    && started.elapsed() >= STARTUP_SETTLE
                    && last_change.elapsed() >= STARTUP_QUIET
                    && (drawn || started.elapsed() >= STARTUP_BLANK_LIMIT)
                {
                    return Ok(());
                }
            }
            TrustAnswer::Drawing => {}
            answer if key_settled => {
                if keys_sent >= MAX_TRUST_KEYS {
                    return Err(Error::new(
                        "Claude's trust question for the usage meter folder did not accept its answer",
                    ));
                }
                let keys = if answer == TrustAnswer::Enter {
                    KEY_ENTER
                } else {
                    KEY_DOWN
                };
                send_keys(stdin, keys)?;
                keys_sent += 1;
                last_key = Some(Instant::now());
            }
            _ => {}
        }
        if child.0.try_wait()?.is_some() {
            return Err(Error::new("Claude exited before its prompt appeared"));
        }
        if started.elapsed() >= STARTUP_TIMEOUT {
            return Err(Error::new(format!(
                "Claude's prompt did not appear within {} seconds",
                STARTUP_TIMEOUT.as_secs()
            )));
        }
        thread::sleep(Duration::from_millis(100));
    }
}

fn sample_weekly_usage_once(
    claude_bin: &str,
    model: &str,
    layout: &Layout,
    cache_path: &Path,
) -> Result<WeeklyUsageSnapshot> {
    create_private_dir(layout.root())?;
    let meter_dir = prepare_meter_dir(layout)?;
    let capture = capture_path(layout.root());
    let _capture_guard = CaptureGuard(capture.clone());

    let capture_arg = capture
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
    configure_script_command(&mut process, capture_arg, claude_bin, &claude_args)?;
    process
        .current_dir(&meter_dir)
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
    let mut stdin = child
        .0
        .stdin
        .take()
        .ok_or_else(|| Error::new("Claude usage meter stdin was unavailable"))?;

    let sample = wait_for_prompt(&mut child, &mut stdin, &capture, &meter_dir).and_then(|()| {
        let typed_at_ms = unix_millis();
        let typed_at_len = fs::read(&capture).map(|bytes| bytes.len()).unwrap_or(0);
        send_keys(&mut stdin, b"/usage\r")?;
        await_reading(
            &mut child,
            &capture,
            cache_path,
            model,
            typed_at_ms,
            typed_at_len,
        )
    });

    // A failed attempt has nothing to hand back: keep what Claude last showed
    // for the owner, and kill the child at once so the retry starts fresh.
    if sample.is_err() {
        let screen = Screen::render(&fs::read(&capture).unwrap_or_default());
        let _ = write_atomic(
            &layout.usage_meter_screen_file(),
            screen.text().as_bytes(),
            0o600,
        );
        drop(stdin);
        let _ = child.0.kill();
        let _ = child.0.wait();
        return sample;
    }

    // Leave the usage overlay, then ask the interactive shell to exit. A stuck
    // UI is killed after a short grace; it has received no model prompt.
    let _ = send_keys(&mut stdin, b"\x1b");
    thread::sleep(Duration::from_millis(100));
    let _ = send_keys(&mut stdin, b"/exit\r");
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

fn await_reading(
    child: &mut ChildGuard,
    capture: &Path,
    cache_path: &Path,
    model: &str,
    typed_at_ms: u64,
    typed_at_len: usize,
) -> Result<WeeklyUsageSnapshot> {
    let started = Instant::now();
    loop {
        let bytes = fs::read(capture).unwrap_or_default();
        let screen = Screen::render(&bytes);
        let cache = fs::read(cache_path).ok();
        let now_ms = unix_millis();
        let observation = Observation {
            screen: &screen,
            since_typed: bytes.get(typed_at_len..).unwrap_or_default(),
            cache: cache.as_deref(),
            typed_at_ms,
            now_ms,
        };
        match read_usage(&observation, model, local_utc_offset_secs) {
            Reading::Answered(snapshot) => return Ok(snapshot),
            // Waiting cannot help: the live refresh will not draw a weekly
            // row until Anthropic's limit lifts. Fall back to Claude's own
            // cached reading at once instead of spending the timeout and two
            // more attempts on the same screen.
            Reading::RateLimited => {
                return rate_limited_fallback(
                    cache.as_deref().unwrap_or_default(),
                    model,
                    now_ms,
                    local_utc_offset_secs,
                )
            }
            Reading::Waiting => {}
        }
        if child.0.try_wait()?.is_some() {
            return Err(Error::new("Claude exited before /usage answered"));
        }
        if started.elapsed() >= SAMPLE_TIMEOUT {
            return Err(Error::new(format!(
                "Claude /usage did not answer within {} seconds",
                SAMPLE_TIMEOUT.as_secs()
            )));
        }
        thread::sleep(Duration::from_millis(100));
    }
}

/// The rate-limited reading: Claude's cache however old, as long as its
/// window has not reset.
fn rate_limited_fallback(
    cache: &[u8],
    model: &str,
    now_ms: u64,
    utc_offset_secs: impl Fn(u64) -> i64,
) -> Result<WeeklyUsageSnapshot> {
    let reading =
        parse_cached_weekly_usage(cache, model, now_ms, utc_offset_secs).map_err(|error| {
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

fn usage_unavailable_error(message: &str, cache_path: &Path, screen_file: &Path) -> Error {
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
        "{message}. Claude's last screen is saved in {}; check that `claude` starts and is signed in, then retry",
        screen_file.display()
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

    /// Eastern daylight time, where the captures below were taken.
    fn edt(_: u64) -> i64 {
        -4 * 3600
    }

    /// Claude Code 2.1.280 opened in a fresh meter folder: the trust question
    /// ("No, exit" selected), Down, Enter, the prompt, `/usage` refreshing from
    /// 75% to 76%, and `/exit`. Recorded with `script -q -F` on 2026-09-27;
    /// the folder path and the per-session usage breakdown are anonymized
    /// byte for byte, so every cursor move still lands where it did.
    const CAPTURE: &[u8] = include_bytes!("../tests/fixtures/usage-2.1.280.log");
    const CAPTURE_DIR: &str =
        "/private/var/folders/xx/xxxxxxxxxxxxxxxxxxxxxxxxxxxxxx/T/claude-daycare-fixture/usage-meter";

    /// Every prefix of the capture that ends just before an escape sequence:
    /// the screens a poll could have seen.
    fn capture_prefixes(from: usize) -> impl Iterator<Item = &'static [u8]> {
        (from + 1..=CAPTURE.len())
            .filter(|&end| end == CAPTURE.len() || CAPTURE[end] == 0x1b)
            .map(|end| &CAPTURE[..end])
    }

    /// Where `/usage` was typed at the prompt.
    fn typed_at_len() -> usize {
        let echo = "\u{276f} /usage".as_bytes();
        CAPTURE
            .windows(echo.len())
            .position(|window| window == echo)
            .expect("the capture shows /usage being typed")
    }

    #[test]
    fn answers_the_trust_question_yes_for_the_meter_folder_only() {
        let dir = Path::new(CAPTURE_DIR);
        let mut answers: Vec<TrustAnswer> = Vec::new();
        for prefix in capture_prefixes(0) {
            let answer = trust_answer(&Screen::render(prefix), dir).unwrap();
            if answers.last() != Some(&answer) {
                answers.push(answer);
            }
        }
        // "No, exit" starts selected, so the first press is Down; Enter comes
        // only once "Yes, I trust this folder" is highlighted. A poll landing
        // mid-redraw, with no answer highlighted, presses nothing.
        assert_eq!(
            answers,
            vec![
                TrustAnswer::NotAsked,
                TrustAnswer::Down,
                TrustAnswer::Drawing,
                TrustAnswer::Enter,
                TrustAnswer::NotAsked,
            ]
        );

        let elsewhere = Path::new("/Users/someone/projects/private-repo");
        let refusal = capture_prefixes(0)
            .find_map(|prefix| trust_answer(&Screen::render(prefix), elsewhere).err())
            .expect("a trust question for another folder is refused");
        assert!(
            refusal
                .to_string()
                .contains("a folder other than the usage meter's own"),
            "{refusal}"
        );
    }

    #[test]
    fn reads_the_refreshed_screen_and_never_the_one_before_it() {
        let typed = typed_at_len();
        let mut first_answer = None;
        for prefix in capture_prefixes(typed) {
            let screen = Screen::render(prefix);
            let observation = Observation {
                screen: &screen,
                since_typed: &prefix[typed..],
                cache: None,
                typed_at_ms: 1_000,
                now_ms: 2_000,
            };
            match read_usage(&observation, "sonnet", edt) {
                Reading::Answered(snapshot) => {
                    first_answer = Some(snapshot);
                    break;
                }
                Reading::RateLimited => panic!("the capture was never rate limited"),
                Reading::Waiting => {}
            }
        }
        // The screen showed last week's 75% while it refreshed; only the
        // refreshed 76% counts. The Fable meter is not a Sonnet visit's.
        assert_eq!(
            first_answer,
            Some(WeeklyUsageSnapshot {
                used_percentage: 76.0,
                resets_at: "live:9:30".to_string(),
                meter_key: "weekly_all:account".to_string(),
            })
        );
        let finished = Screen::render(&CAPTURE[..CAPTURE.len()]);
        assert_eq!(
            screen_weekly_usage(&finished, "opus").unwrap().meter_key,
            "weekly_all:account"
        );
    }

    /// A cache shaped like the one Claude 2.1.280 writes, extra fields included.
    fn cache(fetched_at_ms: u64, limits: &str) -> Vec<u8> {
        format!(
            r#"{{"numStartups":12,"cachedUsageUtilization":{{"fetchedAtMs":{fetched_at_ms},"utilization":{{"limits":[{{"kind":"session","group":"session","percent":4,"resets_at":"2099-09-30T11:09:59.742472+00:00","scope":null,"is_active":false}},{limits}],"extra_usage":{{"is_enabled":false}}}}}}}}"#
        )
        .into_bytes()
    }

    const ALL_60: &str = r#"{"kind":"weekly_all","group":"weekly","percent":60,"resets_at":"2099-09-30T06:59:59.742500+00:00","scope":null,"is_active":false}"#;
    const SONNET_30: &str = r#"{"kind":"weekly_scoped","group":"weekly","percent":30,"resets_at":"2099-09-30T06:59:59.742772+00:00","scope":{"model":{"id":null,"display_name":"Sonnet"}},"is_active":true}"#;
    const FABLE_90: &str = r#"{"kind":"weekly_scoped","group":"weekly","percent":90,"resets_at":"2099-09-30T06:59:59.742772+00:00","scope":{"model":{"id":null,"display_name":"Fable"}},"is_active":true}"#;

    #[test]
    fn selects_the_visit_models_own_weekly_meter_or_the_all_models_one() {
        let limits = format!("{ALL_60},{SONNET_30},{FABLE_90}");
        let sonnet = parse_cached_weekly_usage(&cache(5, &limits), "sonnet", 0, edt).unwrap();
        assert_eq!(sonnet.fetched_at_ms, 5);
        assert_eq!(
            sonnet.snapshot,
            WeeklyUsageSnapshot {
                used_percentage: 30.0,
                resets_at: "live:9:30".to_string(),
                meter_key: "weekly_scoped:Sonnet".to_string(),
            }
        );
        // No Opus meter: the all-models one, never Fable's.
        let opus = parse_cached_weekly_usage(&cache(5, &limits), "opus", 0, edt).unwrap();
        assert_eq!(opus.snapshot.used_percentage, 60.0);
        assert_eq!(opus.snapshot.meter_key, "weekly_all:account");

        let two_sonnets = format!(
            "{ALL_60},{SONNET_30},{}",
            SONNET_30.replace("Sonnet", "Sonnet 5")
        );
        let error = parse_cached_weekly_usage(&cache(5, &two_sonnets), "sonnet", 0, edt)
            .err()
            .expect("two Sonnet meters are ambiguous");
        assert!(error.to_string().contains("more than one"), "{error}");
        assert!(parse_cached_weekly_usage(&cache(5, &two_sonnets), "opus", 0, edt).is_ok());

        let no_weekly = cache(5, r#"{"kind":"session","group":"session","percent":4}"#);
        assert!(parse_cached_weekly_usage(&no_weekly, "sonnet", 0, edt).is_err());
        let past_full = cache(5, &ALL_60.replace("\"percent\":60", "\"percent\":160"));
        assert!(parse_cached_weekly_usage(&past_full, "sonnet", 0, edt).is_err());
    }

    #[test]
    fn names_the_window_by_the_local_reset_date_the_screen_shows() {
        let at = |resets_at: &str, offset: i64| {
            let limit = ALL_60.replace("2099-09-30T06:59:59.742500+00:00", resets_at);
            parse_cached_weekly_usage(&cache(5, &limit), "opus", 0, move |_| offset)
                .unwrap()
                .snapshot
                .resets_at
        };
        assert_eq!(
            at("2099-09-30T06:59:59.742500+00:00", -4 * 3600),
            "live:9:30"
        );
        assert_eq!(
            at("2099-09-30T06:59:59.742500+00:00", 9 * 3600),
            "live:9:30"
        );
        assert_eq!(
            at("2099-09-30T06:59:59.742500+00:00", -8 * 3600),
            "live:9:29"
        );
        // In Mountain time 07:00 UTC is midnight: the instant's wobble either
        // side of the hour must not move the window a day.
        assert_eq!(
            at("2099-09-30T06:59:59.742500+00:00", -7 * 3600),
            "live:9:30"
        );
        assert_eq!(
            at("2099-09-30T07:00:00.040000+00:00", -7 * 3600),
            "live:9:30"
        );
        assert_eq!(at("2099-12-31T23:30:00Z", 3600), "live:1:1");
    }

    #[test]
    fn a_fresh_cache_answers_and_a_stale_one_waits_for_the_refresh() {
        let limits = format!("{ALL_60},{SONNET_30}");
        let blank = Screen::render(b"");
        let typed_at_ms = 10_000_000;
        let look = |screen: &Screen, since_typed: &[u8], fetched_at_ms: u64| {
            let cache = cache(fetched_at_ms, &limits);
            let observation = Observation {
                screen,
                since_typed,
                cache: Some(&cache),
                typed_at_ms,
                now_ms: typed_at_ms + 3_000,
            };
            match read_usage(&observation, "sonnet", edt) {
                Reading::Answered(snapshot) => Some(snapshot.used_percentage),
                Reading::RateLimited => panic!("not rate limited"),
                Reading::Waiting => None,
            }
        };
        // Written by this attempt's refresh: answers before the screen does.
        assert_eq!(look(&blank, b"", typed_at_ms + 1), Some(30.0));
        // Written a minute earlier: waits until the screen finishes refreshing.
        let refreshing: &[u8] =
            b"Current week (Sonnet)\r\n  \xe2\x96\x88 29% used\r\n  Resets Sep 30\r\n\r\n  Refreshing\xe2\x80\xa6\r\n";
        let recent = typed_at_ms - 60_000;
        assert_eq!(look(&Screen::render(refreshing), refreshing, recent), None);
        let mut done = refreshing.to_vec();
        done.extend_from_slice(b"\x1b[1A\x1b[2K");
        let done_screen = Screen::render(&done);
        assert_eq!(look(&done_screen, &done, recent), Some(30.0));
        // Refreshed, but the cache is older than Claude's own refetch window:
        // the refreshed screen answers for itself.
        let old = typed_at_ms - 20 * 60_000;
        let mut redrawn = refreshing.to_vec();
        redrawn.extend_from_slice(b"\x1b[4A\x1b[5G31\x1b[3B\r\x1b[2K");
        let redrawn_screen = Screen::render(&redrawn);
        assert_eq!(look(&redrawn_screen, &redrawn, old), Some(31.0));
        // A screen never seen refreshing after `/usage` is last week's.
        assert_eq!(look(&redrawn_screen, b"", old), None);
    }

    #[test]
    fn a_meter_block_never_borrows_the_next_blocks_numbers() {
        let screen = Screen::render(
            b"  Current week (Sonnet)\r\n  Resets Oct 1 at 3am\r\n\r\n  Current week (all models)\r\n  \xe2\x96\x88 65% used\r\n  Resets Oct 1 at 3am\r\n",
        );
        let error = screen_weekly_usage(&screen, "sonnet").unwrap_err();
        assert!(
            error.to_string().contains("no weekly percentage"),
            "{error}"
        );
        assert_eq!(
            screen_weekly_usage(&screen, "opus").unwrap(),
            WeeklyUsageSnapshot {
                used_percentage: 65.0,
                resets_at: "live:10:1".to_string(),
                meter_key: "weekly_all:account".to_string(),
            }
        );
    }

    /// From an Otto capture on cloud-agents (2026-09-02T01:19:32Z): the
    /// screen before the refresh carries weekly rows, the refresh draws only
    /// the rate-limited notice.
    const RATE_LIMITED_SCREEN: &[u8] = b"Current week (all models)\r\n\xe2\x96\x88\xe2\x96\x88\xe2\x96\x88 39% used\r\nResets Sep 2 at 3am\r\nCurrent week (Fable)\r\n\xe2\x96\x88\xe2\x96\x88 73% used\r\nResets Sep 2 at 3am\r\n\x1b[2JRefreshing\xe2\x80\xa6\r\n\x1b[2JPer-model breakdown unavailable (rate limited \xe2\x80\x94 try again in a moment) \xc2\xb7 r to retry \xc2\xb7 Esc to cancel\r\n";

    const OTTO_CACHE: &[u8] = br#"{"cachedUsageUtilization":{"fetchedAtMs":1788330000000,"utilization":{"limits":[{"kind":"session","group":"session","percent":4,"resets_at":"2026-09-02T11:09:59.742472+00:00","scope":null,"is_active":false},{"kind":"weekly_all","group":"weekly","percent":39,"resets_at":"2026-09-02T06:59:59.742500+00:00","scope":null,"is_active":false},{"kind":"weekly_scoped","group":"weekly","percent":73,"resets_at":"2026-09-02T06:59:59.742772+00:00","scope":{"model":{"id":null,"display_name":"Fable"}},"is_active":true}]}}}"#;

    #[test]
    fn a_rate_limited_refresh_falls_back_to_claudes_cached_weekly_reading() {
        let screen = Screen::render(RATE_LIMITED_SCREEN);
        assert!(usage_screen_is_rate_limited(&screen));
        assert!(!usage_screen_is_rate_limited(&Screen::render(
            b"Current week (all models) 39% used\r\nRefreshing..."
        )));
        // 495 s after the cache was written, before the window resets.
        let now_ms = 1_788_330_000_000 + 495_000;
        let observation = Observation {
            screen: &screen,
            since_typed: RATE_LIMITED_SCREEN,
            cache: Some(OTTO_CACHE),
            typed_at_ms: now_ms - 5_000,
            now_ms,
        };
        assert!(matches!(
            read_usage(&observation, "sonnet", edt),
            Reading::RateLimited
        ));
        // The Fable meter is not a Sonnet visit's: the account meter answers.
        assert_eq!(
            rate_limited_fallback(OTTO_CACHE, "sonnet", now_ms, edt).unwrap(),
            WeeklyUsageSnapshot {
                used_percentage: 39.0,
                resets_at: "live:9:2".to_string(),
                meter_key: "weekly_all:account".to_string(),
            }
        );
        // Far older than the refreshed path's cutoff is still accepted.
        assert!(rate_limited_fallback(OTTO_CACHE, "opus", now_ms + 30 * 60_000, edt).is_ok());
    }

    #[test]
    fn a_cache_whose_window_already_reset_is_refused_even_when_rate_limited() {
        let after_reset_ms = parse_iso_unix_secs("2026-09-02T07:00:00Z").unwrap() * 1000;
        let error = rate_limited_fallback(OTTO_CACHE, "sonnet", after_reset_ms, edt).unwrap_err();
        assert!(error.to_string().contains("rate limited"), "{error}");
        assert!(error.to_string().contains("reset on 9/2"), "{error}");
        assert!(rate_limited_fallback(OTTO_CACHE, "sonnet", after_reset_ms - 2000, edt).is_ok());
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
        assert_eq!(month_day(1_788_332_399, 0), (9, 2));
    }

    #[test]
    fn the_meter_folder_is_private_and_must_stay_empty() {
        let root = crate::testdir::unique_dir("daycare-usage-meter-dir");
        let layout = Layout::at(&root);
        let dir = prepare_meter_dir(&layout).unwrap();
        assert_eq!(dir, fs::canonicalize(layout.usage_meter_dir()).unwrap());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o700);
        }
        fs::write(dir.join("CLAUDE.md"), "planted").unwrap();
        let error = prepare_meter_dir(&layout).unwrap_err();
        assert!(error.to_string().contains("should be empty"), "{error}");
        let _ = fs::remove_dir_all(&root);
    }

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
        let screen_file = dir.join("usage-meter-last-screen.txt");
        fs::write(&cache, br#"{"hasCompletedOnboarding":true}"#).unwrap();
        let message = usage_unavailable_error(
            &meter_gave_up_message(&Error::new(
                "Claude /usage did not answer within 15 seconds",
            )),
            &cache,
            &screen_file,
        )
        .to_string();
        assert_eq!(
            message,
            format!(
                "Claude's /usage meter did not answer in 3 tries (last try: Claude /usage did not answer within 15 seconds). Claude's last screen is saved in {}; check that `claude` starts and is signed in, then retry",
                screen_file.display()
            )
        );
        fs::write(&cache, br#"{}"#).unwrap();
        let onboarding = usage_unavailable_error("meter silent", &cache, &screen_file).to_string();
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
}
