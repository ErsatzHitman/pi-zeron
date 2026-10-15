//! Tokens and spend for every Pi provider and model on one device, read by
//! that device's engine from Pi's session files (`GetUsageReport`).
//!
//! Providers and models are free strings taken from the records: one added
//! later through `/login` appears the first time it is used.
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
}
