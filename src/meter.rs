//! The weekly-allowance meter, as the visit loop sees it.
//!
//! `--weekly-percent` is enforced by reading the agent's own account meter
//! before the visit, after each turn, and after homecoming. Each agent reads
//! its meter differently (Claude: `/usage` in a PTY; Codex: the app-server's
//! `account/rateLimits/read`), and some agents have none. An agent without a
//! meter bounds its visits by tokens instead (`visit::DEFAULT_TOKEN_CAP`).

use crate::paths::Layout;
use crate::usage_meter::WeeklyUsageSnapshot;
use crate::Result;

pub trait WeeklyMeter {
    /// One reading of the weekly window that `model` draws from. Spends no
    /// model turn. `meter_key` names the window, so a reading from a
    /// different window is never compared against this one.
    fn sample(&self, model: &str, layout: &Layout) -> Result<WeeklyUsageSnapshot>;
}
