//! Tokens and spend for every Pi provider and model on one device, read by
//! that device's engine from Pi's session files and usage ledger
//! (`GetUsageReport`).
//!
//! Providers, models and surfaces are free strings taken from the records:
//! a provider added later through `/login`, or a new extension that calls a
//! model, appears the first time it is used.
use serde::{Deserialize, Serialize};

/// Bucket width of [`UsageReport::series`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum UsageBucket {
    Hour,
    #[default]
    Day,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct UsageQuery {
    /// Start of the window in epoch milliseconds; none for all time.
    pub since_ms: Option<i64>,
    pub bucket: UsageBucket,
    /// The requester's UTC offset in minutes east, so days start at its
    /// midnight.
    pub utc_offset_minutes: i32,
    /// Look for new records now instead of serving the last scan.
    pub refresh: bool,
    /// Only this surface's requests in `models` and `series`; `surfaces`
    /// always lists every one.
    pub surface: Option<String>,
    /// Only the requests Pi sessions with these ids made, in everything the
    /// report lists. None counts every session. A tree's spend is the
    /// sessions of its chats.
    pub sessions: Option<Vec<String>>,
}

/// Token counts. The total is `input + cache_read + cache_write + output`;
/// `reasoning` is part of `output` and never added again.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct UsageTokens {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub reasoning: u64,
}

impl UsageTokens {
    pub fn total(&self) -> u64 {
        self.input + self.cache_read + self.cache_write + self.output
    }

    pub fn add(&mut self, other: &UsageTokens) {
        self.input += other.input;
        self.output += other.output;
        self.cache_read += other.cache_read;
        self.cache_write += other.cache_write;
        self.reasoning += other.reasoning;
    }
}

/// One provider's model over the window. An empty provider holds usage Pi
/// recorded without one: tool results and summaries.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct UsageModelRow {
    pub provider: String,
    pub model: String,
    /// Requests (assistant messages and other usage entries).
    pub requests: u64,
    pub tokens: UsageTokens,
    /// Spend Pi recorded at the time.
    pub recorded_usd: f64,
    /// Spend priced here for requests Pi recorded at $0.
    pub estimated_usd: f64,
    /// Requests on a free model.
    pub free_requests: u64,
    /// Requests with no price known anywhere.
    pub unpriced_requests: u64,
    pub last_used_ms: i64,
}

impl UsageModelRow {
    pub fn cost_usd(&self) -> f64 {
        self.recorded_usd + self.estimated_usd
    }
}

/// One surface over the window: what made the calls. `main` is a Pi
/// session's own agent, `delegate` a delegated session; `subagent`,
/// `workflow`, `advisor` and `agent` are child agents; `compaction`,
/// `branch_summary` and `cache_warm` are Pi's own calls; `ext:<name>` and
/// names like `jev` are extensions; `core` is anything unattributed.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct UsageSurfaceRow {
    pub surface: String,
    pub requests: u64,
    pub tokens: UsageTokens,
    pub recorded_usd: f64,
    pub estimated_usd: f64,
    pub free_requests: u64,
    pub unpriced_requests: u64,
    pub last_used_ms: i64,
}

impl UsageSurfaceRow {
    pub fn cost_usd(&self) -> f64 {
        self.recorded_usd + self.estimated_usd
    }
}

/// Where the numbers came from, so the page can say what it cannot see.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct UsageCoverage {
    /// When Pi's usage ledger started recording every call, epoch ms. Before
    /// it, child agents are known only from what their parents logged.
    pub ledger_from_ms: Option<i64>,
    /// Ledger calls counted in the window.
    pub ledger_requests: u64,
    /// Ledger calls skipped because a session file counts them.
    pub ledger_duplicates: u64,
    /// Requests recovered from subagent, advisor and workflow logs.
    pub backfilled_requests: u64,
    /// Problems the ledger's writers reported, such as a blind stream hook.
    pub warnings: Vec<String>,
}

/// One provider's usage in one bucket.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct UsagePoint {
    /// Bucket start, epoch milliseconds.
    pub start_ms: i64,
    pub provider: String,
    pub tokens: u64,
    pub cost_usd: f64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct UsageReport {
    /// Most spend first.
    pub models: Vec<UsageModelRow>,
    /// Ordered by bucket, then provider.
    pub series: Vec<UsagePoint>,
    pub bucket: UsageBucket,
    /// A scan is running; ask again shortly for fresher numbers.
    pub scanning: bool,
    /// When the last scan finished, epoch milliseconds.
    pub scanned_at_ms: Option<i64>,
    /// Session files read.
    pub files: u64,
    /// Sessions with usage in the window.
    pub sessions: u64,
    /// Records skipped because a forked session copied them.
    pub duplicates: u64,
    /// Earliest record in the window, epoch milliseconds.
    pub first_ms: Option<i64>,
    /// Every surface in the window, most spend first.
    pub surfaces: Vec<UsageSurfaceRow>,
    pub coverage: UsageCoverage,
}
