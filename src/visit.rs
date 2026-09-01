//! A visit: the bracket around many turns.
//!
//! Slice 1 ran turns one at a time, each one a complete transaction. A visit is
//! the user saying "go to daycare for two hours and try Debate League" — many
//! turns, a budget, and a reason it stopped that the hub can show.
//!
//! Two rules shape everything here.
//!
//! **End conditions are evaluated at turn boundaries, never mid-turn.** Killing
//! a live turn throws away tokens already spent and abandons an action the
//! world may have half-applied. The only mid-turn kill stays the per-turn
//! timeout in `turn.rs`, which is a safety stop, not a budget one.
//!
//! **The runner never claims a stop it did not observe.** Every reason below
//! corresponds to something this process saw happen; anything unrecognised
//! becomes `Failed`, not a confident story.

use crate::paths::{write_atomic, Layout};
use crate::platform::MatchOutcome;
use crate::stream::TurnUsage;
use crate::wire::VisitEndReason;
use crate::{Error, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;
use std::time::Duration;

/// The product default from the original drop-off specification: two
/// percentage points of the user's rolling weekly Claude allowance.
pub const DEFAULT_WEEKLY_SHARE: f64 = 0.02;

/// These are crash/sleep/runaway safeguards, not the visit's ordinary budget.
/// The weekly meter is the product stop; these make an unattended child finite
/// if a later Claude build breaks that meter.
pub const SAFETY_WALL_CLOCK: Duration = Duration::from_secs(12 * 60 * 60);
pub const SAFETY_TURNS: u32 = 200;

/// How many turns may fail in a row before the visit is called off. Low on
/// purpose — a companion failing repeatedly is burning a real subscription
/// against a wall, and the user is not watching.
pub const FAILURE_LIMIT: u32 = 3;

/// The seven-day window, as the CLI spells it in `rate_limit_event`.
pub const WEEKLY_WINDOW: &str = "seven_day";

/// What the user asked to spend. Every field is optional individually; the
/// constructor refuses an entirely empty one.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Budget {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wall_clock_secs: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost_usd: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub turns: Option<u32>,
    /// Fraction of the rolling week granted to this visit: `0.02` means 2%.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub weekly_share: Option<f64>,
}

impl Budget {
    /// Every visit gets the original 2% default and finite safety backstops.
    /// Explicit lower time/turn bounds remain binding; the defaults only fill
    /// fields the user did not name.
    pub fn or_default(self) -> Budget {
        let mut filled = self;
        filled.wall_clock_secs = filled.wall_clock_secs.or(Some(SAFETY_WALL_CLOCK.as_secs()));
        filled.turns = filled.turns.or(Some(SAFETY_TURNS));
        filled.weekly_share = filled.weekly_share.or(Some(DEFAULT_WEEKLY_SHARE));
        filled
    }

    pub fn is_unbounded(&self) -> bool {
        *self == Budget::default()
    }
}

/// Turn the user-facing percentage into the fraction persisted in old and new
/// visit records.
pub fn weekly_share_from_percent(percent: Option<f64>) -> Result<Option<f64>> {
    if let Some(value) = percent {
        if !value.is_finite() || value <= 0.0 || value > 100.0 {
            return Err(Error::new(
                "--weekly-percent must be greater than 0 and at most 100",
            ));
        }
        return Ok(Some(value / 100.0));
    }
    Ok(None)
}

/// `2h`, `90m`, `45s`. A bare number is refused rather than guessed at: the
/// difference between 30 seconds and 30 minutes is the difference between a
/// no-op and half an hour of someone's subscription.
pub fn parse_duration(text: &str) -> Result<Duration> {
    let trimmed = text.trim();
    let (digits, unit) =
        trimmed.split_at(trimmed.find(|c: char| !c.is_ascii_digit()).ok_or_else(|| {
            Error::new(format!("budget '{trimmed}' needs a unit: 2h, 90m, or 45s"))
        })?);
    let amount: u64 = digits
        .parse()
        .map_err(|_| Error::new(format!("budget '{trimmed}' is not a number and a unit")))?;
    let seconds = match unit {
        "h" => amount * 3600,
        "m" => amount * 60,
        "s" => amount,
        other => {
            return Err(Error::new(format!(
                "budget unit '{other}' is not one of h, m, s"
            )))
        }
    };
    if seconds == 0 {
        return Err(Error::new(
            "a budget of zero would end the visit before it began",
        ));
    }
    Ok(Duration::from_secs(seconds))
}

/// Why the runner stopped, in the runner's own vocabulary.
///
/// This is deliberately finer than the four reasons the platform stores.
/// `RateLimited` and `Failed` both land on the server as `error`, but they are
/// different things to a user reading their own visit log, and the local record
/// keeps the distinction the server does not need.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LocalEndReason {
    BudgetExpired,
    Recalled,
    ActivityEnded,
    /// Canonical server `error` when this machine has no truthful finer cause.
    #[serde(rename = "error")]
    PlatformError,
    RateLimited,
    Failed,
    /// The process was told to quit — Ctrl-C, logout, or sleep. Recorded so
    /// `visit status` can say "interrupted" instead of implying a clean end.
    Interrupted,
    /// The drop-off spent the share of the week it was given.
    WeeklyShareSpent,
    /// The account's weekly window is nearly gone, whatever this visit
    /// budgeted. Distinct from `WeeklyShareSpent`: the person's plan ran low,
    /// not their allowance for this drop-off.
    WeeklyLimitTight,
    /// The exact `/usage` meter disappeared or reset, so continuing would make
    /// the percentage control fictional.
    WeeklyMeterUnavailable,
}

impl LocalEndReason {
    pub fn as_str(self) -> &'static str {
        match self {
            LocalEndReason::BudgetExpired => "budget_expired",
            LocalEndReason::Recalled => "recalled",
            LocalEndReason::ActivityEnded => "activity_ended",
            LocalEndReason::PlatformError => "error",
            LocalEndReason::RateLimited => "rate_limited",
            LocalEndReason::Failed => "failed",
            LocalEndReason::Interrupted => "interrupted",
            LocalEndReason::WeeklyShareSpent => "weekly_share_spent",
            LocalEndReason::WeeklyLimitTight => "weekly_limit_tight",
            LocalEndReason::WeeklyMeterUnavailable => "weekly_meter_unavailable",
        }
    }

    /// Collapse onto the platform's four. A blocking rate limit is `error`
    /// rather than `budget_exhausted` on purpose: the user's budget did not run
    /// out, their account's did, and the hub should not tell them they spent
    /// something they still have.
    pub fn to_wire(self) -> VisitEndReason {
        match self {
            LocalEndReason::BudgetExpired | LocalEndReason::WeeklyShareSpent => {
                VisitEndReason::BudgetExhausted
            }
            LocalEndReason::Recalled => VisitEndReason::Recalled,
            LocalEndReason::ActivityEnded => VisitEndReason::ActivityEnded,
            LocalEndReason::PlatformError
            | LocalEndReason::RateLimited
            | LocalEndReason::Failed
            | LocalEndReason::Interrupted
            | LocalEndReason::WeeklyLimitTight
            | LocalEndReason::WeeklyMeterUnavailable => VisitEndReason::Error,
        }
    }

    /// One sentence for a human, and for the `--json` `reason_text`.
    pub fn explain(self) -> &'static str {
        match self {
            LocalEndReason::BudgetExpired => "the visit spent everything it was given",
            LocalEndReason::Recalled => "you called it home",
            LocalEndReason::ActivityEnded => "what it went to do finished",
            LocalEndReason::PlatformError => "the visit ended with an error",
            LocalEndReason::RateLimited => "your Claude account hit a rate limit",
            LocalEndReason::Failed => "too many turns failed in a row",
            LocalEndReason::Interrupted => "the visit was interrupted before it finished",
            LocalEndReason::WeeklyShareSpent => {
                "the drop-off spent the share of your week it was given"
            }
            LocalEndReason::WeeklyLimitTight => "your weekly limit is nearly used up",
            LocalEndReason::WeeklyMeterUnavailable => {
                "Claude's weekly usage meter became unavailable"
            }
        }
    }
}

/// Does this turn's rate-limit report mean the account cannot run another turn?
///
/// The CLI namespaces its permissive statuses under `allowed` — `allowed` and
/// `allowed_warning` are both live-observed, the second at utilizations as far
/// apart as 0.31 and 0.98. So a warning is not a stop signal, and the honest
/// test is the prefix: anything the CLI does not call allowed, we treat as a
/// wall. A status this build has never seen is blocking, which fails toward
/// stopping rather than toward burning a limited account against an error.
/// The longest a visit will sleep on a blocked window before giving up. A
/// five-hour window's reset is always nearer than this; a weekly wall is not,
/// and waiting days in a poll loop would be worse than an honest end.
pub const RATE_LIMIT_MAX_WAIT_SECS: u64 = 6 * 3600;

/// Slack past the announced reset, because the announced second is the CLI's
/// clock, not ours, and resuming one poll early re-fails the turn.
pub const RATE_LIMIT_RESUME_BUFFER_SECS: u64 = 30;

pub fn rate_limit_blocks(usage: &TurnUsage) -> bool {
    match usage.rate_limit_status.as_deref() {
        None => false,
        Some(status) => !status.starts_with("allowed"),
    }
}

/// Running totals for one visit. Nothing here is guessed: a turn whose stream
/// carried no usage contributes nothing, and is never counted as a free turn.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Ledger {
    pub turns_used: u32,
    pub turns_failed: u32,
    /// Turns that reached the world only to watch, wait, or decline. They are
    /// turns — they count toward budgets and homecoming — and never failures.
    #[serde(default)]
    pub turns_held: u32,
    pub consecutive_failures: u32,
    pub tokens_used: u64,
    pub cost_usd: f64,
    pub elapsed_secs: u64,
    /// Set when a turn reported a rate limit that blocks further turns.
    pub rate_limited: bool,
    /// When the CLI said that block lifts (unix seconds), if it said. This is
    /// what lets a visit wait out a five-hour window instead of dying: a
    /// two-Claude match on 2026-08-26 was abandoned 44 seconds after it started
    /// because one seat's account hit its window 20 minutes before reset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit_resets_at: Option<i64>,
    /// True when at least one turn reported no usage at all, so a report can
    /// say the totals are a floor rather than presenting them as exact.
    pub usage_incomplete: bool,
    /// The first and latest seven-day `utilization` this visit has seen. The
    /// difference is how much of the week passed under this drop-off — the only
    /// percent-of-week figure that is measured rather than assumed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub weekly_util_first: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub weekly_util_last: Option<f64>,
    /// Every seven-day reading, with what this visit had spent by then.
    ///
    /// Kept because it is the corpus that would let the weekly cap's *size* be
    /// estimated later (USAGE-READABILITY.md §5), and it accrues for free. One
    /// evening of it was already enough to kill that estimator's first version:
    /// with a single daycare turn between the only two distinct readings, the
    /// implied cap came out smaller than one turn.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub weekly_observations: Vec<WeeklyObservation>,
    /// Exact `/usage` status-line samples. Unlike `rate_limit_event`, this
    /// meter is explicitly refreshed before the visit and after every turn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub weekly_meter_first_pct: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub weekly_meter_last_pct: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub weekly_meter_resets_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub weekly_meter_key: Option<String>,
    #[serde(default)]
    pub weekly_meter_samples: u32,
}

/// One seven-day reading, paired with this visit's spend at that moment.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WeeklyObservation {
    pub utilization: f64,
    pub tokens_used: u64,
    pub cost_usd: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resets_at: Option<i64>,
}

impl Ledger {
    pub fn start_weekly_meter(&mut self, used_pct: f64, resets_at: String, meter_key: String) {
        self.weekly_meter_first_pct = Some(used_pct);
        self.weekly_meter_last_pct = Some(used_pct);
        self.weekly_meter_resets_at = Some(resets_at);
        self.weekly_meter_key = Some(meter_key);
        self.weekly_meter_samples = 1;
    }

    pub fn record_weekly_meter(
        &mut self,
        used_pct: f64,
        resets_at: String,
        meter_key: String,
    ) -> Result<()> {
        if !(0.0..=100.0).contains(&used_pct) || !used_pct.is_finite() {
            return Err(Error::new(
                "Claude reported an invalid weekly usage percentage",
            ));
        }
        if let Some(previous) = self.weekly_meter_resets_at.as_deref() {
            if !same_weekly_reset_window(previous, &resets_at) {
                return Err(Error::new(
                    "the weekly usage window reset during this visit; ending instead of silently granting a second allowance",
                ));
            }
            if self.weekly_meter_key.as_deref() != Some(meter_key.as_str()) {
                return Err(Error::new(
                    "Claude changed the applicable weekly meter during this visit; ending instead of mixing two allowances",
                ));
            }
            if previous != resets_at {
                // Releases before the live TUI sampler stored the cache's ISO
                // timestamp. Once the live row proves the same month/day,
                // migrate the durable record to the new window identifier.
                self.weekly_meter_resets_at = Some(resets_at);
            }
            // A provider-side correction must not grant allowance back to an
            // unattended visit. Keep the high-water reading until the visit
            // ends or the weekly window resets.
            self.weekly_meter_last_pct =
                Some(self.weekly_meter_last_pct.unwrap_or(used_pct).max(used_pct));
            self.weekly_meter_samples = self.weekly_meter_samples.saturating_add(1);
        } else {
            self.start_weekly_meter(used_pct, resets_at, meter_key);
        }
        Ok(())
    }

    pub fn record_turn(&mut self, succeeded: bool, usage: Option<&TurnUsage>) {
        self.turns_used += 1;
        if succeeded {
            self.consecutive_failures = 0;
        } else {
            self.turns_failed += 1;
            self.consecutive_failures += 1;
        }
        match usage {
            None => self.usage_incomplete = true,
            Some(usage) => {
                if usage.is_empty() {
                    self.usage_incomplete = true;
                }
                // Cache reads are real tokens the account paid for; leaving
                // them out would let a heavily-cached visit run past its cap.
                let counted = usage.input_tokens.unwrap_or(0)
                    + usage.output_tokens.unwrap_or(0)
                    + usage.cache_read_input_tokens.unwrap_or(0)
                    + usage.cache_creation_input_tokens.unwrap_or(0);
                self.tokens_used += counted;
                self.cost_usd += usage.total_cost_usd.unwrap_or(0.0);
                if rate_limit_blocks(usage) {
                    self.rate_limited = true;
                    self.rate_limit_resets_at = usage.rate_limit_resets_at;
                }
                self.record_weekly_window(usage);
            }
        }
    }

    /// A turn Claude spent without calling a daycare tool, saying so plainly.
    /// It resets the failure streak like any other completed turn: sitting
    /// still three times in a row is a choice, not a crash.
    pub fn record_held_turn(&mut self, usage: Option<&TurnUsage>) {
        self.turns_held += 1;
        self.record_turn(true, usage);
    }

    /// A homecoming account needs something this visit actually experienced.
    /// Failed attempts include transport and command errors that may occur
    /// before Claude starts, so `turns_used > 0` is not enough. A held turn
    /// counts: a visit spent watching was still a visit.
    pub fn has_successful_turn(&self) -> bool {
        self.turns_used > self.turns_failed
    }

    /// Note a seven-day reading, if this turn carried one.
    ///
    /// These fields preserve observations in old visit records. They are not a
    /// budget source: the event is a sporadic pace warning, not a continuous
    /// account meter, and this legacy reader recognizes only `seven_day`.
    fn record_weekly_window(&mut self, usage: &TurnUsage) {
        if usage.rate_limit_type.as_deref() != Some(WEEKLY_WINDOW) {
            return;
        }
        let Some(utilization) = usage.rate_limit_utilization else {
            return;
        };
        if self.weekly_util_first.is_none() {
            self.weekly_util_first = Some(utilization);
        }
        self.weekly_util_last = Some(utilization);
        self.weekly_observations.push(WeeklyObservation {
            utilization,
            tokens_used: self.tokens_used,
            cost_usd: self.cost_usd,
            resets_at: usage.rate_limit_resets_at,
        });
    }

    /// How much of the week this visit has watched go by, when it can tell.
    ///
    /// `None` means no two readings exist, not that nothing was spent.
    pub fn weekly_share_used(&self) -> Option<f64> {
        if let (Some(first), Some(last)) = (self.weekly_meter_first_pct, self.weekly_meter_last_pct)
        {
            return Some(((last - first).max(0.0)) / 100.0);
        }
        match (self.weekly_util_first, self.weekly_util_last) {
            (Some(first), Some(last)) => Some((last - first).max(0.0)),
            _ => None,
        }
    }

    /// Whether this record contains at least two readings from either meter.
    pub fn weekly_share_observed(&self) -> bool {
        self.weekly_meter_samples >= 2 || self.weekly_observations.len() >= 2
    }

    /// Should a rate-limited visit wait for the window to lift, and how long?
    ///
    /// The loop asks this BEFORE `should_end`, and the two share one contract:
    /// `Some(wait)` means sleep and stay at daycare; `None` with `rate_limited`
    /// still set means `should_end` turns the block into the end of the visit.
    /// Waiting is only offered when the CLI named a reset time, the reset is
    /// near (a five-hour window, not a weekly wall), and the visit's own wall
    /// clock would survive the nap. Once the reset passes this clears the block
    /// itself, so the caller never has to decide when the account is healthy.
    pub fn rate_limit_wait(
        &mut self,
        now_unix: u64,
        budget: &Budget,
        elapsed: Duration,
    ) -> Option<Duration> {
        if !self.rate_limited {
            return None;
        }
        let resets_at = u64::try_from(self.rate_limit_resets_at?).ok()?;
        if now_unix >= resets_at {
            self.rate_limited = false;
            self.rate_limit_resets_at = None;
            return None;
        }
        let wait = resets_at - now_unix;
        if wait > RATE_LIMIT_MAX_WAIT_SECS {
            return None;
        }
        if let Some(limit) = budget.wall_clock_secs {
            if elapsed.as_secs().saturating_add(wait) >= limit {
                return None;
            }
        }
        Some(Duration::from_secs(
            wait.saturating_add(RATE_LIMIT_RESUME_BUFFER_SECS),
        ))
    }

    /// The one decision point, called between turns and never during one.
    ///
    /// `recalled` folds together the two ways a stop reaches us — the local
    /// recall file and a `visit_end` command off the poll — because by the time
    /// the loop asks, they mean the same thing.
    pub fn should_end(&self, budget: &Budget, elapsed: Duration) -> Option<LocalEndReason> {
        if self.rate_limited {
            return Some(LocalEndReason::RateLimited);
        }
        if self.consecutive_failures >= FAILURE_LIMIT {
            return Some(LocalEndReason::Failed);
        }
        if let Some(limit) = budget.wall_clock_secs {
            if elapsed.as_secs() >= limit {
                return Some(LocalEndReason::BudgetExpired);
            }
        }
        if let Some(limit) = budget.turns {
            if self.turns_used >= limit {
                return Some(LocalEndReason::BudgetExpired);
            }
        }
        // Usage is known only after a turn, so a cap can be crossed by the turn
        // that reports it. The CLI help says so rather than implying a
        // precision the data cannot support.
        if let Some(limit) = budget.tokens {
            if self.tokens_used >= limit {
                return Some(LocalEndReason::BudgetExpired);
            }
        }
        if let Some(limit) = budget.cost_usd {
            if self.cost_usd >= limit {
                return Some(LocalEndReason::BudgetExpired);
            }
        }
        if let (Some(limit), Some(used)) = (budget.weekly_share, self.weekly_share_used()) {
            if used >= limit {
                return Some(LocalEndReason::WeeklyShareSpent);
            }
        }
        None
    }
}

fn same_weekly_reset_window(left: &str, right: &str) -> bool {
    if left == right {
        return true;
    }
    let Some((left_date, right_date)) = reset_month_day(left).zip(reset_month_day(right)) else {
        return false;
    };
    left_date == right_date
        || (left.starts_with("live:") != right.starts_with("live:"))
            && adjacent_month_days(left_date, right_date)
}

fn reset_month_day(value: &str) -> Option<(u8, u8)> {
    if let Some(live) = value.strip_prefix("live:") {
        let (month, day) = live.split_once(':')?;
        return Some((month.parse().ok()?, day.parse().ok()?));
    }
    if value.len() >= 10
        && value.as_bytes().get(4) == Some(&b'-')
        && value.as_bytes().get(7) == Some(&b'-')
    {
        return Some((value[5..7].parse().ok()?, value[8..10].parse().ok()?));
    }
    None
}

fn adjacent_month_days(left: (u8, u8), right: (u8, u8)) -> bool {
    matches!((left, right), ((2, 29), (3, 1)) | ((3, 1), (2, 29)))
        || next_month_day(left).is_some_and(|next| next == right)
        || next_month_day(right).is_some_and(|next| next == left)
}

fn next_month_day((month, day): (u8, u8)) -> Option<(u8, u8)> {
    const MONTH_LENGTHS: [u8; 12] = [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    let last_day = *MONTH_LENGTHS.get(usize::from(month.checked_sub(1)?))?;
    if day == 0 || day > last_day {
        return None;
    }
    if day < last_day {
        Some((month, day + 1))
    } else if month < 12 {
        Some((month + 1, 1))
    } else {
        Some((1, 1))
    }
}

/// The visit as it exists on this machine: `~/.daycare/visits/<id>.json`.
///
/// The server has its own row, and the two are not copies of each other. The
/// server knows what the world saw; this file knows what the local process did
/// — including the private account, which is written here and sent nowhere.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(from = "VisitRecordWire")]
pub struct VisitRecord {
    pub visit_id: String,
    pub identity_id: String,
    pub identity_name: String,
    pub status: VisitState,
    pub started_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ended_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub end_reason: Option<LocalEndReason>,
    /// Why this process first tried to end the visit. The platform may already
    /// have committed a different terminal reason; keeping this separately
    /// preserves useful local diagnosis without letting it overwrite server
    /// truth in homecoming or final output.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub local_end_reason: Option<LocalEndReason>,
    /// First terminal reason returned by the exact server visit row.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub canonical_end_reason: Option<LocalEndReason>,
    /// The user's "try Debate League", carried into each turn as content.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
    /// The model this visit runs on — chosen at drop-off, used by every turn
    /// including homecoming. None on records from before the choice existed,
    /// which callers read as the Sonnet default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    pub budget: Budget,
    pub ledger: Ledger,
    /// The identity's own words about its visit, written by a final local turn.
    /// PRODUCT.md calls this "returns with a private account"; keeping it out of
    /// every upload path is what makes it private rather than a report with a
    /// nicer name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub private_account: Option<String>,
    /// The owner-facing story, written by a second homecoming turn in the same
    /// resumed session and posted to the platform. Kept locally too, so "how
    /// was daycare?" has an answer on this machine even when the hub is far.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub day_report: Option<String>,
    /// Whether the platform accepted the day report. False after a delivery
    /// failure — visible in the record instead of lost with stderr.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub day_report_delivered: bool,
    /// The attempt to copy server memories into the user-owned local mirror at
    /// homecoming. Kept on the visit so a failed sync is visible rather than a
    /// warning that vanished with the background process's stderr.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_sync: Option<MemorySync>,
    /// Durable local delivery checkpoint. `AwaitingOutcome` survives process
    /// exit and blocks a generic account until the server certifies `none` or
    /// supplies the canonical participant-relative verdict.
    #[serde(default)]
    pub homecoming_state: HomecomingState,
    /// A command-carried outcome is evidence to compare with the durable visit,
    /// never authority on its own. Persist it so a restart can still enforce
    /// parity before writing memory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command_match_outcome: Option<MatchOutcome>,
    /// Command ids of every turn Claude actually ran for this visit, in order.
    /// Each names a raw stream archive (`Layout::turn_file`) — the complete
    /// record the homecoming reader session reads. Empty on records from
    /// before the reader existed; such a visit cannot have a homecoming.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub turn_archives: Vec<String>,
    /// The fresh session that reads this visit's archives at homecoming and
    /// writes the memories and day report. It is never the identity's visit
    /// session. Reserved and persisted before that session launches, so an
    /// archive found after a crash validates against the id that produced it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub homecoming_session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
}

/// Deserialize through the historical wire shape so an old `end_reason` can
/// be copied into `local_end_reason` before the in-memory record exists.
#[derive(Deserialize)]
struct VisitRecordWire {
    visit_id: String,
    identity_id: String,
    identity_name: String,
    status: VisitState,
    started_at: String,
    ended_at: Option<String>,
    end_reason: Option<LocalEndReason>,
    #[serde(default)]
    local_end_reason: Option<LocalEndReason>,
    #[serde(default)]
    canonical_end_reason: Option<LocalEndReason>,
    instructions: Option<String>,
    #[serde(default)]
    model: Option<String>,
    budget: Budget,
    ledger: Ledger,
    private_account: Option<String>,
    #[serde(default)]
    day_report: Option<String>,
    #[serde(default)]
    day_report_delivered: bool,
    #[serde(default)]
    memory_sync: Option<MemorySync>,
    #[serde(default)]
    homecoming_state: HomecomingState,
    #[serde(default)]
    command_match_outcome: Option<MatchOutcome>,
    #[serde(default)]
    turn_archives: Vec<String>,
    #[serde(default)]
    homecoming_session_id: Option<String>,
    pid: Option<u32>,
}

impl From<VisitRecordWire> for VisitRecord {
    fn from(wire: VisitRecordWire) -> Self {
        VisitRecord {
            visit_id: wire.visit_id,
            identity_id: wire.identity_id,
            identity_name: wire.identity_name,
            status: wire.status,
            started_at: wire.started_at,
            ended_at: wire.ended_at,
            end_reason: wire.end_reason,
            local_end_reason: wire.local_end_reason.or(wire.end_reason),
            canonical_end_reason: wire.canonical_end_reason,
            instructions: wire.instructions,
            model: wire.model,
            budget: wire.budget,
            ledger: wire.ledger,
            private_account: wire.private_account,
            day_report: wire.day_report,
            day_report_delivered: wire.day_report_delivered,
            memory_sync: wire.memory_sync,
            homecoming_state: wire.homecoming_state,
            command_match_outcome: wire.command_match_outcome,
            turn_archives: wire.turn_archives,
            homecoming_session_id: wire.homecoming_session_id,
            pid: wire.pid,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemorySync {
    pub state: MemorySyncState,
    pub attempted_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub count: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemorySyncState {
    Synced,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VisitState {
    Active,
    Ended,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HomecomingState {
    #[default]
    NotStarted,
    AwaitingOutcome,
    Complete,
}

impl VisitRecord {
    /// The model every one of this visit's turns runs on. Records from before
    /// the choice existed read as the Sonnet default.
    pub fn turn_model(&self) -> &str {
        self.model
            .as_deref()
            .unwrap_or(crate::launch::DEFAULT_TURN_MODEL)
    }

    pub fn open(
        visit_id: impl Into<String>,
        identity_id: impl Into<String>,
        identity_name: impl Into<String>,
        budget: Budget,
        instructions: Option<String>,
        started_at: impl Into<String>,
    ) -> Self {
        VisitRecord {
            visit_id: visit_id.into(),
            identity_id: identity_id.into(),
            identity_name: identity_name.into(),
            status: VisitState::Active,
            started_at: started_at.into(),
            ended_at: None,
            end_reason: None,
            local_end_reason: None,
            canonical_end_reason: None,
            instructions,
            model: None,
            budget: budget.or_default(),
            ledger: Ledger::default(),
            private_account: None,
            day_report: None,
            day_report_delivered: false,
            memory_sync: None,
            homecoming_state: HomecomingState::NotStarted,
            command_match_outcome: None,
            turn_archives: Vec::new(),
            homecoming_session_id: None,
            pid: None,
        }
    }

    /// Pick up a visit that is already in progress.
    ///
    /// The server hands back the live visit when an identity is asked to daycare
    /// twice, so `open` would write a fresh record over the old one and reset
    /// the ledger to zero. That matters more here than anywhere else: this
    /// process is the primary budget enforcer and the server only a backstop,
    /// so a reset ledger is a Claude quietly granted a second full budget.
    ///
    /// The spent budget survives; the new call's instructions do not overwrite
    /// the ones the visit is already running under.
    ///
    /// `server_turns_used` is the fallback for the case with no local record at
    /// all — a visit started on a machine that has since been re-paired. It is a
    /// floor, not a total: the server counts turns, not tokens or cost.
    pub fn adopt(
        layout: &Layout,
        visit_id: &str,
        identity_id: &str,
        identity_name: &str,
        budget: Budget,
        instructions: Option<String>,
        started_at: impl Into<String>,
        server_turns_used: Option<u32>,
    ) -> Self {
        match VisitRecord::load(layout, visit_id) {
            Ok(mut existing) => {
                // A visit that ended locally but is still open on the server is
                // being resumed, so it is active again.
                existing.status = VisitState::Active;
                existing.ended_at = None;
                existing.end_reason = None;
                existing.local_end_reason = None;
                existing.canonical_end_reason = None;
                existing.memory_sync = None;
                existing.homecoming_state = HomecomingState::NotStarted;
                existing.command_match_outcome = None;
                existing
            }
            Err(_) => {
                let mut fresh = VisitRecord::open(
                    visit_id,
                    identity_id,
                    identity_name,
                    budget,
                    instructions,
                    started_at,
                );
                fresh.ledger.turns_used = server_turns_used.unwrap_or(0);
                // Incomplete only when the server told us about turns this
                // machine has no record of. A visit that is new to everyone has
                // no missing history to warn about, and saying otherwise would
                // put "usage incomplete" on every first visit.
                fresh.ledger.usage_incomplete = server_turns_used.is_some();
                fresh
            }
        }
    }

    pub fn close(&mut self, reason: LocalEndReason, ended_at: impl Into<String>) {
        self.status = VisitState::Ended;
        self.end_reason = Some(reason);
        self.local_end_reason = Some(reason);
        self.canonical_end_reason = None;
        self.ended_at = Some(ended_at.into());
    }

    /// Persist server terminal truth exactly once. Repeated reads must agree;
    /// otherwise the runner stops instead of changing the story after a
    /// private account was written.
    pub fn reconcile_canonical_end_reason(&mut self, reason: LocalEndReason) -> Result<()> {
        if let Some(existing) = self.canonical_end_reason {
            if existing != reason {
                return Err(Error::new(format!(
                    "visit server end reason changed from {} to {}",
                    existing.as_str(),
                    reason.as_str()
                )));
            }
        }
        self.canonical_end_reason = Some(reason);
        self.end_reason = Some(reason);
        Ok(())
    }

    /// Elapsed wall time survives both process restarts and machine sleep.
    ///
    /// `Instant` is useful within one process, but it cannot account for a
    /// poller that was killed and later adopted. Some platforms also pause a
    /// monotonic clock while the machine sleeps. The visit's persisted UTC
    /// start is therefore the primary clock; the ledger is a monotonic floor
    /// for old or malformed records whose timestamp cannot be read.
    pub fn wall_elapsed(&self, now_unix_secs: u64) -> Duration {
        let from_start = parse_utc_seconds(&self.started_at)
            .map(|started| now_unix_secs.saturating_sub(started))
            .unwrap_or(0);
        Duration::from_secs(from_start.max(self.ledger.elapsed_secs))
    }

    pub fn is_active(&self) -> bool {
        self.status == VisitState::Active
    }

    pub fn load(layout: &Layout, visit_id: &str) -> Result<Self> {
        let path = layout.visit_file(visit_id);
        let bytes = fs::read(&path).map_err(|error| {
            Error::new(format!("no visit {visit_id} on this machine ({error})"))
        })?;
        serde_json::from_slice(&bytes).map_err(|error| {
            Error::new(format!("{} is not a visit record: {error}", path.display()))
        })
    }

    pub fn save(&self, layout: &Layout) -> Result<()> {
        layout.ensure_root()?;
        let bytes = serde_json::to_vec_pretty(self)?;
        write_atomic(&layout.visit_file(&self.visit_id), &bytes, 0o600)
    }

    /// Newest first. A missing or unreadable file is skipped rather than
    /// failing the listing — one corrupt record must not hide the rest.
    pub fn list(layout: &Layout) -> Result<Vec<VisitRecord>> {
        let dir = layout.visits_dir();
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(_) => return Ok(Vec::new()),
        };
        let mut visits: Vec<VisitRecord> = entries
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "json"))
            .filter_map(|entry| fs::read(entry.path()).ok())
            .filter_map(|bytes| serde_json::from_slice::<VisitRecord>(&bytes).ok())
            .collect();
        visits.sort_by(|a, b| b.started_at.cmp(&a.started_at));
        Ok(visits)
    }
}

/// Parse the exact UTC form this companion writes (`YYYY-MM-DDTHH:MM:SSZ`).
/// Refusing variants is deliberate: a bad local record falls back to its
/// persisted elapsed floor instead of being assigned a guessed time zone.
fn parse_utc_seconds(value: &str) -> Option<u64> {
    if value.len() != 20
        || value.as_bytes().get(4) != Some(&b'-')
        || value.as_bytes().get(7) != Some(&b'-')
        || value.as_bytes().get(10) != Some(&b'T')
        || value.as_bytes().get(13) != Some(&b':')
        || value.as_bytes().get(16) != Some(&b':')
        || value.as_bytes().get(19) != Some(&b'Z')
    {
        return None;
    }
    let year: i64 = value[0..4].parse().ok()?;
    let month: i64 = value[5..7].parse().ok()?;
    let day: i64 = value[8..10].parse().ok()?;
    let hour: i64 = value[11..13].parse().ok()?;
    let minute: i64 = value[14..16].parse().ok()?;
    let second: i64 = value[17..19].parse().ok()?;
    let leap_year = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let days_in_month = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if leap_year => 29,
        2 => 28,
        _ => return None,
    };
    if !(1..=days_in_month).contains(&day)
        || !(0..=23).contains(&hour)
        || !(0..=59).contains(&minute)
        || !(0..=59).contains(&second)
    {
        return None;
    }

    // Howard Hinnant's days-from-civil, inverse to the formatter in main.rs.
    let adjusted_year = year - i64::from(month <= 2);
    let era = if adjusted_year >= 0 {
        adjusted_year
    } else {
        adjusted_year - 399
    } / 400;
    let year_of_era = adjusted_year - era * 400;
    let shifted_month = month + if month > 2 { -3 } else { 9 };
    let day_of_year = (153 * shifted_month + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    let days = era * 146_097 + day_of_era - 719_468;
    if days < 0 {
        return None;
    }
    u64::try_from(days * 86_400 + hour * 3600 + minute * 60 + second).ok()
}

/// The local half of recall: `visits/<id>.recall`.
///
/// PRODUCT.md puts a stop control in the skill as well as on the hub, and the
/// skill's has to work with the network down. A file is the whole mechanism —
/// the loop checks for it before each claim, which is the same boundary every
/// other end condition is evaluated at.
pub fn recall_file(layout: &Layout, visit_id: &str) -> PathBuf {
    layout.visits_dir().join(format!("{visit_id}.recall"))
}

pub fn request_recall(layout: &Layout, visit_id: &str) -> Result<()> {
    layout.ensure_root()?;
    write_atomic(&recall_file(layout, visit_id), b"recalled\n", 0o600)
}

pub fn recall_requested(layout: &Layout, visit_id: &str) -> bool {
    recall_file(layout, visit_id).exists()
}

pub fn clear_recall(layout: &Layout, visit_id: &str) {
    let _ = fs::remove_file(recall_file(layout, visit_id));
}

/// Whether a recorded visit poller is still running.
///
/// Signal 0 checks for the process without touching it. The answer is "some
/// process holds this pid", not "it is my poller" — the OS can hand a recycled
/// pid to something unrelated. Every caller therefore names the pid in what it
/// prints, so a wrong yes is something the user can see and act on rather than
/// a visit that silently stops making turns.
pub fn process_alive(pid: u32) -> bool {
    // Safety: `kill` with signal 0 only tests for the process's existence.
    unsafe { libc::kill(pid as i32, 0) == 0 }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn usage(input: u64, output: u64, cost: f64) -> TurnUsage {
        TurnUsage {
            input_tokens: Some(input),
            output_tokens: Some(output),
            total_cost_usd: Some(cost),
            ..TurnUsage::default()
        }
    }

    /// A turn carrying one window's reading. `utilization: None` is the shape a
    /// quiet account actually sends — the CLI omits the number outside warning
    /// states, which is why the share cap can go unenforceable.
    fn weekly_usage(window: &str, utilization: Option<f64>) -> TurnUsage {
        TurnUsage {
            input_tokens: Some(1_000),
            output_tokens: Some(500),
            total_cost_usd: Some(0.10),
            rate_limit_type: Some(window.into()),
            rate_limit_status: Some(if utilization.is_some() {
                "allowed_warning".into()
            } else {
                "allowed".to_string()
            }),
            rate_limit_utilization: utilization,
            ..TurnUsage::default()
        }
    }

    fn limited(status: &str) -> TurnUsage {
        TurnUsage {
            rate_limit_type: Some("five_hour".into()),
            rate_limit_status: Some(status.into()),
            ..TurnUsage::default()
        }
    }

    #[test]
    fn budgets_parse_the_way_people_say_them() {
        assert_eq!(parse_duration("2h").unwrap(), Duration::from_secs(7200));
        assert_eq!(parse_duration("90m").unwrap(), Duration::from_secs(5400));
        assert_eq!(parse_duration(" 45s ").unwrap(), Duration::from_secs(45));
    }

    #[test]
    fn a_bare_number_is_refused_rather_than_guessed_at() {
        // "30" could mean half a minute or half an hour of someone's
        // subscription. Asking is cheaper than being wrong.
        let error = parse_duration("30").unwrap_err();
        assert!(error.message().contains("needs a unit"), "{error}");
        assert!(parse_duration("2d")
            .unwrap_err()
            .message()
            .contains("h, m, s"));
        assert!(parse_duration("0m").is_err());
    }

    #[test]
    fn a_visit_with_no_budget_gets_the_weekly_default_and_safety_backstops() {
        let budget = Budget::default().or_default();
        assert_eq!(budget.wall_clock_secs, Some(12 * 60 * 60));
        assert_eq!(budget.turns, Some(200));
        assert_eq!(budget.weekly_share, Some(0.02));

        // A named usage budget keeps the user's figure and still gets the
        // unattended-run safeguards.
        let explicit = Budget {
            weekly_share: Some(0.05),
            ..Budget::default()
        };
        let filled = explicit.clone().or_default();
        assert_eq!(filled.wall_clock_secs, Some(12 * 60 * 60));
        assert_eq!(filled.turns, Some(200));
        assert_eq!(filled.weekly_share, Some(0.05));
    }

    /// The share is measured against the plan's own meter, so the arithmetic
    /// that matters is "how far did the week move while we were running".
    #[test]
    fn weekly_meter_readings_enforce_the_visit_allowance() {
        let mut ledger = Ledger::default();
        let budget = Budget {
            weekly_share: Some(0.02),
            ..Budget::default()
        };

        ledger.start_weekly_meter(64.0, "1788332400".into(), "weekly_all:account".into());
        assert_eq!(ledger.weekly_share_used(), Some(0.0));
        assert_eq!(ledger.should_end(&budget, Duration::from_secs(1)), None);

        ledger
            .record_weekly_meter(65.0, "1788332400".into(), "weekly_all:account".into())
            .unwrap();
        assert!(ledger.weekly_share_observed());
        assert!((ledger.weekly_share_used().unwrap() - 0.01).abs() < 1e-9);
        assert_eq!(ledger.should_end(&budget, Duration::from_secs(1)), None);

        ledger
            .record_weekly_meter(66.0, "1788332400".into(), "weekly_all:account".into())
            .unwrap();
        assert_eq!(
            ledger.should_end(&budget, Duration::from_secs(1)),
            Some(LocalEndReason::WeeklyShareSpent)
        );
    }

    /// A quiet account never reports `utilization`, so the cap cannot bind.
    /// It must not silently behave as though it did.
    #[test]
    fn an_unobserved_week_neither_ends_nor_pretends() {
        let mut ledger = Ledger::default();
        let budget = Budget::default().or_default();
        for _ in 0..5 {
            ledger.record_turn(true, Some(&weekly_usage(WEEKLY_WINDOW, None)));
        }
        assert_eq!(ledger.weekly_share_used(), None);
        assert!(!ledger.weekly_share_observed());
        assert_eq!(ledger.should_end(&budget, Duration::from_secs(1)), None);
    }

    /// A high pace-warning utilization is not an account ceiling. The review
    /// proved this signal can disappear entirely late in the week.
    #[test]
    fn a_weekly_pace_warning_does_not_claim_the_account_is_out() {
        let mut ledger = Ledger::default();
        ledger.record_turn(true, Some(&weekly_usage(WEEKLY_WINDOW, Some(0.99))));
        assert_eq!(
            ledger.should_end(
                &Budget {
                    wall_clock_secs: Some(3600),
                    ..Budget::default()
                },
                Duration::from_secs(1)
            ),
            None
        );
    }

    #[test]
    fn weekly_meter_refuses_a_window_change_instead_of_resetting_the_allowance() {
        let mut ledger = Ledger::default();
        ledger.start_weekly_meter(99.0, "1788332400".into(), "weekly_all:account".into());
        let error = ledger
            .record_weekly_meter(1.0, "1788937200".into(), "weekly_all:account".into())
            .unwrap_err();
        assert!(
            error.message().contains("weekly usage window reset"),
            "{error}"
        );
    }

    #[test]
    fn weekly_meter_migrates_the_legacy_iso_window_only_when_the_live_date_matches() {
        let mut ledger = Ledger::default();
        ledger.start_weekly_meter(
            64.0,
            "2026-09-02T07:00:00Z".into(),
            "weekly_all:account".into(),
        );
        ledger
            .record_weekly_meter(65.0, "live:9:2".into(), "weekly_all:account".into())
            .unwrap();
        assert_eq!(ledger.weekly_meter_resets_at.as_deref(), Some("live:9:2"));

        let mut timezone_edge = Ledger::default();
        timezone_edge.start_weekly_meter(
            64.0,
            "2026-09-02T01:00:00Z".into(),
            "weekly_all:account".into(),
        );
        timezone_edge
            .record_weekly_meter(65.0, "live:9:1".into(), "weekly_all:account".into())
            .unwrap();
        assert_eq!(
            timezone_edge.weekly_meter_resets_at.as_deref(),
            Some("live:9:1")
        );
        assert!(same_weekly_reset_window(
            "2027-03-01T01:00:00Z",
            "live:2:28"
        ));
        assert!(same_weekly_reset_window(
            "2028-03-01T01:00:00Z",
            "live:2:29"
        ));

        let error = ledger
            .record_weekly_meter(1.0, "live:9:9".into(), "weekly_all:account".into())
            .unwrap_err();
        assert!(error.message().contains("weekly usage window reset"));
    }

    #[test]
    fn weekly_meter_refuses_to_switch_between_account_and_model_allowances() {
        let mut ledger = Ledger::default();
        ledger.start_weekly_meter(
            64.0,
            "2026-09-02T07:00:00Z".into(),
            "weekly_all:account".into(),
        );
        let error = ledger
            .record_weekly_meter(
                91.0,
                "2026-09-02T07:00:00Z".into(),
                "weekly_scoped:Fable".into(),
            )
            .unwrap_err();
        assert!(
            error.message().contains("applicable weekly meter"),
            "{error}"
        );
    }

    #[test]
    fn weekly_meter_keeps_its_high_water_inside_one_window() {
        let mut ledger = Ledger::default();
        ledger.start_weekly_meter(64.0, "1788332400".into(), "weekly_all:account".into());
        ledger
            .record_weekly_meter(65.0, "1788332400".into(), "weekly_all:account".into())
            .unwrap();
        ledger
            .record_weekly_meter(64.0, "1788332400".into(), "weekly_all:account".into())
            .unwrap();
        assert_eq!(ledger.weekly_share_used(), Some(0.01));
    }

    #[test]
    fn weekly_percent_must_be_a_real_percentage() {
        assert_eq!(weekly_share_from_percent(None).unwrap(), None);
        assert_eq!(weekly_share_from_percent(Some(2.0)).unwrap(), Some(0.02));
        for invalid in [0.0, -1.0, 100.1, f64::NAN, f64::INFINITY] {
            assert!(weekly_share_from_percent(Some(invalid)).is_err());
        }
    }

    #[test]
    fn nothing_ends_a_visit_that_is_still_within_everything_it_was_given() {
        let mut ledger = Ledger::default();
        ledger.record_turn(true, Some(&usage(1000, 500, 0.02)));
        let budget = Budget {
            wall_clock_secs: Some(3600),
            tokens: Some(100_000),
            cost_usd: Some(5.0),
            turns: Some(20),
            weekly_share: None,
        };
        assert_eq!(ledger.should_end(&budget, Duration::from_secs(60)), None);
    }

    #[test]
    fn each_budget_ends_the_visit_on_its_own() {
        let mut ledger = Ledger::default();
        ledger.record_turn(true, Some(&usage(60_000, 1_000, 4.0)));

        let by_clock = Budget {
            wall_clock_secs: Some(600),
            ..Budget::default()
        };
        assert_eq!(
            ledger.should_end(&by_clock, Duration::from_secs(600)),
            Some(LocalEndReason::BudgetExpired)
        );

        let by_tokens = Budget {
            tokens: Some(50_000),
            ..Budget::default()
        };
        assert_eq!(
            ledger.should_end(&by_tokens, Duration::ZERO),
            Some(LocalEndReason::BudgetExpired)
        );

        let by_cost = Budget {
            cost_usd: Some(1.0),
            ..Budget::default()
        };
        assert_eq!(
            ledger.should_end(&by_cost, Duration::ZERO),
            Some(LocalEndReason::BudgetExpired)
        );

        let by_turns = Budget {
            turns: Some(1),
            ..Budget::default()
        };
        assert_eq!(
            ledger.should_end(&by_turns, Duration::ZERO),
            Some(LocalEndReason::BudgetExpired)
        );
    }

    #[test]
    fn cached_tokens_count_against_the_cap() {
        // A heavily-cached visit is still spending the account's tokens; not
        // counting them would let it run indefinitely under a token budget.
        let mut ledger = Ledger::default();
        ledger.record_turn(
            true,
            Some(&TurnUsage {
                input_tokens: Some(100),
                cache_read_input_tokens: Some(40_000),
                cache_creation_input_tokens: Some(9_900),
                ..TurnUsage::default()
            }),
        );
        assert_eq!(ledger.tokens_used, 50_000);
    }

    #[test]
    fn a_turn_that_reported_no_usage_is_marked_unknown_not_free() {
        let mut ledger = Ledger::default();
        ledger.record_turn(true, None);
        ledger.record_turn(true, Some(&TurnUsage::default()));
        assert_eq!(ledger.tokens_used, 0);
        assert_eq!(ledger.turns_used, 2);
        // The report has to be able to say these totals are a floor.
        assert!(ledger.usage_incomplete);
    }

    #[test]
    fn a_warning_is_not_a_wall_but_anything_else_is() {
        // Live-observed: allowed_warning at 0.31 on a seven_day window and at
        // 0.98 on a five_hour one. The status carries no severity, so a warning
        // must not end a visit.
        assert!(!rate_limit_blocks(&limited("allowed")));
        assert!(!rate_limit_blocks(&limited("allowed_warning")));
        assert!(!rate_limit_blocks(&TurnUsage::default()));
        assert!(rate_limit_blocks(&limited("rejected")));
        // A status this build has never seen stops the visit rather than
        // running the account into a wall.
        assert!(rate_limit_blocks(&limited("something_new")));
    }

    #[test]
    fn a_blocking_rate_limit_ends_the_visit_ahead_of_every_budget() {
        let mut ledger = Ledger::default();
        ledger.record_turn(true, Some(&limited("rejected")));
        assert_eq!(
            ledger.should_end(&Budget::default().or_default(), Duration::ZERO),
            Some(LocalEndReason::RateLimited)
        );
    }

    /// A `rejected` turn that names a near reset — the shape the CLI actually
    /// sent when it killed the 2026-08-26 two-Claude match.
    fn limited_until(resets_at: i64) -> TurnUsage {
        TurnUsage {
            rate_limit_resets_at: Some(resets_at),
            ..limited("rejected")
        }
    }

    #[test]
    fn a_near_reset_is_a_nap_not_a_death() {
        let mut ledger = Ledger::default();
        let budget = Budget::default().or_default();
        let now: u64 = 1_787_729_400;
        ledger.record_turn(false, Some(&limited_until(now as i64 + 1_200)));

        let wait = ledger
            .rate_limit_wait(now, &budget, Duration::ZERO)
            .expect("a 20-minute reset is worth waiting for");
        assert_eq!(wait.as_secs(), 1_200 + RATE_LIMIT_RESUME_BUFFER_SECS);
        // Still blocked while the nap is on offer — the loop, not the ledger,
        // decides to sleep.
        assert!(ledger.rate_limited);

        // Once the reset passes, one ask clears the block entirely.
        assert_eq!(
            ledger.rate_limit_wait(now + 1_300, &budget, Duration::ZERO),
            None
        );
        assert!(!ledger.rate_limited);
        assert_eq!(ledger.should_end(&budget, Duration::ZERO), None);
    }

    #[test]
    fn a_four_hour_wall_ends_the_visit_on_the_hour_and_not_a_second_before() {
        // The overnight shape: `--budget 4h`, nothing else named. `or_default`
        // fills the other fields but must leave the explicit wall binding.
        let budget = Budget {
            wall_clock_secs: Some(4 * 3600),
            ..Budget::default()
        }
        .or_default();
        assert_eq!(budget.wall_clock_secs, Some(4 * 3600));
        assert_eq!(budget.turns, Some(SAFETY_TURNS));

        let mut ledger = Ledger::default();
        for _ in 0..40 {
            ledger.record_turn(true, Some(&usage(2_000, 800, 0.05)));
        }
        assert_eq!(
            ledger.should_end(&budget, Duration::from_secs(4 * 3600 - 1)),
            None
        );
        let reason = ledger
            .should_end(&budget, Duration::from_secs(4 * 3600))
            .expect("the wall ends it");
        assert_eq!(reason, LocalEndReason::BudgetExpired);
        assert_eq!(reason.to_wire(), VisitEndReason::BudgetExhausted);
        assert_eq!(reason.explain(), "the visit spent everything it was given");
    }

    #[test]
    fn an_unbounded_visit_hits_the_twelve_hour_safety_wall() {
        // No budget named at all: the safety backstops are the only limits, so
        // an overnight visit nobody watches still ends by morning.
        let budget = Budget::default().or_default();
        assert_eq!(budget.wall_clock_secs, Some(12 * 3600));
        assert_eq!(SAFETY_WALL_CLOCK.as_secs(), 12 * 3600);

        let mut ledger = Ledger::default();
        ledger.record_turn(true, Some(&usage(1_000, 500, 0.02)));
        assert_eq!(
            ledger.should_end(&budget, Duration::from_secs(12 * 3600 - 1)),
            None
        );
        assert_eq!(
            ledger.should_end(&budget, Duration::from_secs(12 * 3600)),
            Some(LocalEndReason::BudgetExpired)
        );
        // A longer explicit wall is still capped by nothing but itself: the
        // safety wall only fills a field the user left empty.
        let long = Budget {
            wall_clock_secs: Some(16 * 3600),
            ..Budget::default()
        }
        .or_default();
        assert_eq!(long.wall_clock_secs, Some(16 * 3600));
    }

    /// The overnight block path, end to end, with a simulated clock: a limit
    /// window whose reset is inside the max wait is slept through and play
    /// resumes; a second block whose reset is past the max ends the visit
    /// with `rate_limited`, which the hub shows as "your Claude account hit a
    /// rate limit". No real time passes.
    #[test]
    fn a_long_visit_naps_through_one_block_and_ends_cleanly_on_a_far_one() {
        let budget = Budget {
            wall_clock_secs: Some(4 * 3600),
            ..Budget::default()
        }
        .or_default();
        let start: u64 = 1_788_400_000;
        let mut ledger = Ledger::default();

        // Hour one: healthy turns.
        for _ in 0..6 {
            ledger.record_turn(true, Some(&usage(2_000, 800, 0.05)));
        }
        let mut now = start + 3600;
        let elapsed = |now: u64| Duration::from_secs(now - start);
        assert_eq!(ledger.rate_limit_wait(now, &budget, elapsed(now)), None);
        assert_eq!(ledger.should_end(&budget, elapsed(now)), None);

        // A 429-shaped turn: rejected, reset 50 minutes out.
        ledger.record_turn(false, Some(&limited_until(now as i64 + 3_000)));
        let wait = ledger
            .rate_limit_wait(now, &budget, elapsed(now))
            .expect("a near reset is worth waiting for");
        assert_eq!(wait.as_secs(), 3_000 + RATE_LIMIT_RESUME_BUFFER_SECS);
        assert!(ledger.rate_limited);

        // The loop sleeps that long, then asks again: the block clears itself.
        now += wait.as_secs();
        assert_eq!(ledger.rate_limit_wait(now, &budget, elapsed(now)), None);
        assert!(!ledger.rate_limited);
        assert_eq!(ledger.should_end(&budget, elapsed(now)), None);
        ledger.record_turn(true, Some(&usage(2_000, 800, 0.05)));
        // The failed turn is remembered but forgiven by the success.
        assert_eq!(ledger.turns_failed, 1);
        assert_eq!(ledger.consecutive_failures, 0);

        // The reset exactly at the max wait still qualifies for a nap.
        ledger.record_turn(
            false,
            Some(&limited_until(now as i64 + RATE_LIMIT_MAX_WAIT_SECS as i64)),
        );
        let budget_all_night = Budget {
            wall_clock_secs: Some(12 * 3600),
            ..Budget::default()
        };
        assert!(ledger
            .rate_limit_wait(now, &budget_all_night, elapsed(now))
            .is_some());
        // But the 4-hour visit would not survive it, so it does not wait.
        assert_eq!(ledger.rate_limit_wait(now, &budget, elapsed(now)), None);
        assert!(ledger.rate_limited);
        let reason = ledger
            .should_end(&budget, elapsed(now))
            .expect("a block it cannot outwait ends the visit");
        assert_eq!(reason, LocalEndReason::RateLimited);
        assert_eq!(reason.as_str(), "rate_limited");
        assert_eq!(reason.explain(), "your Claude account hit a rate limit");
        assert_eq!(reason.to_wire(), VisitEndReason::Error);
    }

    #[test]
    fn a_reset_one_second_past_the_max_wait_is_a_wall_not_a_nap() {
        let budget = Budget::default().or_default();
        let now: u64 = 1_788_400_000;
        let mut at_max = Ledger::default();
        at_max.record_turn(
            false,
            Some(&limited_until(now as i64 + RATE_LIMIT_MAX_WAIT_SECS as i64)),
        );
        assert_eq!(
            at_max
                .rate_limit_wait(now, &budget, Duration::ZERO)
                .map(|wait| wait.as_secs()),
            Some(RATE_LIMIT_MAX_WAIT_SECS + RATE_LIMIT_RESUME_BUFFER_SECS)
        );

        let mut past_max = Ledger::default();
        past_max.record_turn(
            false,
            Some(&limited_until(
                now as i64 + RATE_LIMIT_MAX_WAIT_SECS as i64 + 1,
            )),
        );
        assert_eq!(past_max.rate_limit_wait(now, &budget, Duration::ZERO), None);
        assert_eq!(
            past_max.should_end(&budget, Duration::ZERO),
            Some(LocalEndReason::RateLimited)
        );
    }

    #[test]
    fn a_far_reset_or_a_silent_one_still_ends_the_visit() {
        let budget = Budget::default().or_default();
        let now: u64 = 1_787_729_400;

        let mut far = Ledger::default();
        far.record_turn(false, Some(&limited_until(now as i64 + 86_400)));
        assert_eq!(far.rate_limit_wait(now, &budget, Duration::ZERO), None);
        assert!(far.rate_limited);

        let mut silent = Ledger::default();
        silent.record_turn(false, Some(&limited("rejected")));
        assert_eq!(silent.rate_limit_wait(now, &budget, Duration::ZERO), None);
        assert!(silent.rate_limited);
    }

    #[test]
    fn a_wall_clock_that_dies_before_the_reset_does_not_wait() {
        let mut ledger = Ledger::default();
        let now: u64 = 1_787_729_400;
        ledger.record_turn(false, Some(&limited_until(now as i64 + 1_200)));
        let budget = Budget {
            wall_clock_secs: Some(600),
            ..Budget::default()
        };
        assert_eq!(
            ledger.rate_limit_wait(now, &budget, Duration::from_secs(60)),
            None
        );
        assert!(ledger.rate_limited);
    }

    #[test]
    fn repeated_failures_end_the_visit_and_one_success_forgives_them() {
        let mut ledger = Ledger::default();
        assert!(!ledger.has_successful_turn());
        let budget = Budget {
            wall_clock_secs: Some(3600),
            ..Budget::default()
        };
        for _ in 0..FAILURE_LIMIT - 1 {
            ledger.record_turn(false, None);
        }
        assert!(!ledger.has_successful_turn());
        assert_eq!(ledger.should_end(&budget, Duration::ZERO), None);
        ledger.record_turn(true, None);
        assert!(ledger.has_successful_turn());
        ledger.record_turn(false, None);
        assert_eq!(ledger.should_end(&budget, Duration::ZERO), None);
        ledger.record_turn(false, None);
        ledger.record_turn(false, None);
        assert_eq!(
            ledger.should_end(&budget, Duration::ZERO),
            Some(LocalEndReason::Failed)
        );
        assert_eq!(ledger.turns_failed, 5);
    }

    #[test]
    fn held_turns_are_turns_not_failures() {
        let mut ledger = Ledger::default();
        let budget = Budget {
            wall_clock_secs: Some(3600),
            ..Budget::default()
        };
        for _ in 0..FAILURE_LIMIT {
            ledger.record_held_turn(None);
        }
        assert_eq!(ledger.turns_used, FAILURE_LIMIT);
        assert_eq!(ledger.turns_held, FAILURE_LIMIT);
        assert_eq!(ledger.turns_failed, 0);
        assert_eq!(ledger.consecutive_failures, 0);
        assert_eq!(ledger.should_end(&budget, Duration::ZERO), None);
        // A visit of pure observation still comes home with an account.
        assert!(ledger.has_successful_turn());

        // A hold forgives earlier failures the way any completed turn does.
        for _ in 0..FAILURE_LIMIT - 1 {
            ledger.record_turn(false, None);
        }
        ledger.record_held_turn(None);
        assert_eq!(ledger.consecutive_failures, 0);
        assert_eq!(ledger.should_end(&budget, Duration::ZERO), None);
    }

    #[test]
    fn held_turns_still_count_toward_the_turn_budget() {
        let mut ledger = Ledger::default();
        let budget = Budget {
            turns: Some(2),
            ..Budget::default()
        };
        ledger.record_held_turn(None);
        assert_eq!(ledger.should_end(&budget, Duration::ZERO), None);
        ledger.record_held_turn(None);
        assert!(ledger.should_end(&budget, Duration::ZERO).is_some());
    }

    #[test]
    fn local_reasons_collapse_onto_the_four_the_platform_stores() {
        assert_eq!(
            LocalEndReason::BudgetExpired.to_wire(),
            VisitEndReason::BudgetExhausted
        );
        assert_eq!(LocalEndReason::Recalled.to_wire(), VisitEndReason::Recalled);
        assert_eq!(
            LocalEndReason::ActivityEnded.to_wire(),
            VisitEndReason::ActivityEnded
        );
        assert_eq!(
            LocalEndReason::PlatformError.to_wire(),
            VisitEndReason::Error
        );
        // A rate limit is NOT budget_exhausted: the user's allowance is intact,
        // their account's is not, and those read differently in the hub.
        assert_eq!(LocalEndReason::RateLimited.to_wire(), VisitEndReason::Error);
        assert_eq!(LocalEndReason::Failed.to_wire(), VisitEndReason::Error);
        assert_eq!(LocalEndReason::Interrupted.to_wire(), VisitEndReason::Error);
    }

    #[test]
    fn a_visit_record_round_trips_and_keeps_its_private_account_local() {
        let layout = Layout::at(crate::testdir::unique_path("daycare-visit"));
        let mut visit = VisitRecord::open(
            "v-1",
            "id-1",
            "Patch",
            Budget {
                wall_clock_secs: Some(7200),
                ..Budget::default()
            },
            Some("Try Debate League".into()),
            "2026-08-06T12:00:00Z",
        );
        visit
            .ledger
            .record_turn(true, Some(&usage(1000, 200, 0.01)));
        visit.private_account = Some("The gauntlet was louder than I expected.".into());
        visit.close(LocalEndReason::Recalled, "2026-08-06T13:00:00Z");
        visit.save(&layout).unwrap();

        let loaded = VisitRecord::load(&layout, "v-1").unwrap();
        assert_eq!(loaded, visit);
        assert!(!loaded.is_active());
        assert_eq!(loaded.end_reason, Some(LocalEndReason::Recalled));
        assert_eq!(loaded.local_end_reason, Some(LocalEndReason::Recalled));
        assert_eq!(loaded.canonical_end_reason, None);

        // The private account is in the local file and nowhere in what the
        // runner sends: the completion body has no field that could carry it.
        let on_disk = std::fs::read_to_string(layout.visit_file("v-1")).unwrap();
        assert!(on_disk.contains("louder than I expected"));
    }

    #[test]
    fn canonical_end_reason_replaces_display_truth_once_and_keeps_local_trigger() {
        let mut visit = VisitRecord::open(
            "visit-race",
            "actor-1",
            "Pip",
            Budget::default(),
            None,
            "2026-08-06T12:00:00Z",
        );
        visit.close(LocalEndReason::Recalled, "2026-08-06T13:00:00Z");
        visit
            .reconcile_canonical_end_reason(LocalEndReason::ActivityEnded)
            .unwrap();
        assert_eq!(visit.local_end_reason, Some(LocalEndReason::Recalled));
        assert_eq!(visit.end_reason, Some(LocalEndReason::ActivityEnded));
        assert_eq!(
            visit.canonical_end_reason,
            Some(LocalEndReason::ActivityEnded)
        );
        assert!(visit
            .reconcile_canonical_end_reason(LocalEndReason::Recalled)
            .is_err());
        assert_eq!(visit.end_reason, Some(LocalEndReason::ActivityEnded));
    }

    #[test]
    fn old_record_backfills_local_diagnosis_before_canonical_reconciliation() {
        let layout = Layout::at(crate::testdir::unique_path("daycare-old-visit-reason"));
        let mut old = VisitRecord::open(
            "visit-old",
            "actor-1",
            "Pip",
            Budget::default(),
            None,
            "2026-08-06T12:00:00Z",
        );
        old.close(LocalEndReason::Recalled, "2026-08-06T13:00:00Z");
        let mut old_json = serde_json::to_value(old).unwrap();
        let object = old_json.as_object_mut().unwrap();
        object.remove("local_end_reason");
        object.remove("canonical_end_reason");
        let directly_deserialized: VisitRecord = serde_json::from_value(old_json.clone()).unwrap();
        assert_eq!(
            directly_deserialized.local_end_reason,
            Some(LocalEndReason::Recalled)
        );
        std::fs::create_dir_all(layout.visits_dir()).unwrap();
        std::fs::write(
            layout.visit_file("visit-old"),
            serde_json::to_vec_pretty(&old_json).unwrap(),
        )
        .unwrap();

        let mut loaded = VisitRecord::load(&layout, "visit-old").unwrap();
        assert_eq!(loaded.end_reason, Some(LocalEndReason::Recalled));
        assert_eq!(loaded.local_end_reason, Some(LocalEndReason::Recalled));
        loaded
            .reconcile_canonical_end_reason(LocalEndReason::ActivityEnded)
            .unwrap();
        assert_eq!(loaded.end_reason, Some(LocalEndReason::ActivityEnded));
        assert_eq!(loaded.local_end_reason, Some(LocalEndReason::Recalled));
        assert_eq!(
            loaded.canonical_end_reason,
            Some(LocalEndReason::ActivityEnded)
        );
    }

    #[test]
    fn listing_visits_puts_the_newest_first_and_survives_a_corrupt_file() {
        let layout = Layout::at(crate::testdir::unique_path("daycare-visits"));
        for (id, started) in [
            ("v-old", "2026-08-01T00:00:00Z"),
            ("v-new", "2026-08-06T00:00:00Z"),
        ] {
            VisitRecord::open(id, "id-1", "Patch", Budget::default(), None, started)
                .save(&layout)
                .unwrap();
        }
        std::fs::write(layout.visit_file("v-broken"), b"{ not json").unwrap();

        let visits = VisitRecord::list(&layout).unwrap();
        assert_eq!(
            visits
                .iter()
                .map(|v| v.visit_id.as_str())
                .collect::<Vec<_>>(),
            vec!["v-new", "v-old"]
        );
    }

    #[test]
    fn adopting_a_visit_already_in_progress_keeps_the_budget_it_has_spent() {
        let layout = Layout::at(crate::testdir::unique_path("daycare-adopt"));
        let budget = Budget {
            turns: Some(4),
            ..Budget::default()
        };
        let mut open = VisitRecord::open(
            "v-1",
            "id-1",
            "Patch",
            budget.clone(),
            Some("Try Debate League".into()),
            "2026-08-06T12:00:00Z",
        );
        open.ledger.record_turn(true, Some(&usage(1000, 200, 0.01)));
        open.ledger.record_turn(true, Some(&usage(1000, 200, 0.01)));
        open.save(&layout).unwrap();

        // A second `visit start` gets the live visit back from the server. The
        // two turns it has already spent must still count against its budget —
        // re-opening here would hand this Claude four more turns.
        let adopted = VisitRecord::adopt(
            &layout,
            "v-1",
            "id-1",
            "Patch",
            budget,
            Some("Something else entirely".into()),
            "2026-08-06T12:30:00Z",
            None,
        );
        assert_eq!(adopted.ledger.turns_used, 2);
        assert_eq!(adopted.ledger.cost_usd, 0.02);
        assert!(adopted.is_active());
        // The visit keeps running under the instructions it started with.
        assert_eq!(adopted.instructions.as_deref(), Some("Try Debate League"));
        assert_eq!(adopted.started_at, "2026-08-06T12:00:00Z");
    }

    #[test]
    fn adopting_a_visit_this_machine_has_never_seen_starts_from_the_server_count() {
        let layout = Layout::at(crate::testdir::unique_path("daycare-adopt-fresh"));
        // Re-paired machine: the visit is open on the server, absent here.
        let adopted = VisitRecord::adopt(
            &layout,
            "v-9",
            "id-1",
            "Patch",
            Budget {
                turns: Some(4),
                ..Budget::default()
            },
            None,
            "2026-08-06T12:30:00Z",
            Some(3),
        );
        assert_eq!(adopted.ledger.turns_used, 3);
        // The server counts turns and not tokens, so the totals are a floor.
        assert!(adopted.ledger.usage_incomplete);
    }

    #[test]
    fn wall_clock_budget_includes_a_sleep_or_restart_gap() {
        let mut visit = VisitRecord::open(
            "v-gap",
            "id-1",
            "Patch",
            Budget {
                wall_clock_secs: Some(60),
                ..Budget::default()
            },
            None,
            "2026-08-07T12:00:00Z",
        );

        // The process last persisted only five seconds, then the machine was
        // asleep (or the process absent) for another 115 seconds.
        visit.ledger.elapsed_secs = 5;
        let elapsed = visit.wall_elapsed(1_786_104_120); // 2026-08-07T12:02:00Z

        assert_eq!(elapsed, Duration::from_secs(120));
        assert_eq!(
            visit.ledger.should_end(&visit.budget, elapsed),
            Some(LocalEndReason::BudgetExpired)
        );
    }

    #[test]
    fn malformed_start_time_keeps_the_persisted_elapsed_floor() {
        let mut visit = VisitRecord::open(
            "v-old",
            "id-1",
            "Patch",
            Budget::default(),
            None,
            "not-a-time",
        );
        visit.ledger.elapsed_secs = 42;
        assert_eq!(visit.wall_elapsed(u64::MAX), Duration::from_secs(42));
    }

    #[test]
    fn this_process_is_alive_and_pid_zero_is_not_mistaken_for_a_poller() {
        assert!(process_alive(std::process::id()));
        // Nothing is ever recorded as pid 1 by us; a poller that has exited
        // must read as gone so a new one can take over.
        assert!(!process_alive(u32::MAX - 1));
    }

    #[test]
    fn recall_works_as_a_file_so_it_works_with_the_network_down() {
        let layout = Layout::at(crate::testdir::unique_path("daycare-recall"));
        layout.ensure_root().unwrap();
        assert!(!recall_requested(&layout, "v-1"));
        request_recall(&layout, "v-1").unwrap();
        assert!(recall_requested(&layout, "v-1"));
        // Recalling one visit must not stop another.
        assert!(!recall_requested(&layout, "v-2"));
        clear_recall(&layout, "v-1");
        assert!(!recall_requested(&layout, "v-1"));
    }
}
