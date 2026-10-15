//! Scanned files to a [`UsageReport`]: fork copies, and ledger calls a
//! session file also holds, counted once; each request priced; totals per
//! model, per surface, and per provider over time.
use super::{
    price::{Price, PriceBook},
    scan::{FileKind, FileState, Record, Source},
};
use std::collections::{BTreeMap, HashMap, HashSet};
use zeron_proto::usage::{
    UsageBucket, UsageCoverage, UsageModelRow, UsagePoint, UsageQuery, UsageReport,
    UsageSurfaceRow, UsageTokens,
};

const HOUR_MS: i64 = 3_600_000;
const DAY_MS: i64 = 24 * HOUR_MS;
/// How far a cache warm's ledger call can be from its session entry: Pi
/// stamps the entry when it writes it, not when the call answered.
const WARM_SLACK_MS: i64 = 120_000;

/// The start of the bucket holding `ms`, at the requester's midnight.
fn bucket_start(ms: i64, width: i64, offset_ms: i64) -> i64 {
    (ms + offset_ms).div_euclid(width) * width - offset_ms
}

/// What made the call.
fn surface<'a>(file: &'a FileState, record: &Record) -> &'a str {
    match record.source() {
        Source::Assistant if file.session_id.starts_with("dlg-") => "delegate",
        Source::Assistant => "main",
        Source::Usage => "cache_warm",
        Source::Tool => "tool",
        Source::Compaction => "compaction",
        Source::BranchSummary => "branch_summary",
        Source::Ledger | Source::Backfill => match file.surface(record) {
            "" => "core",
            surface => surface,
        },
    }
}

/// A cache warm as both its session entry and its ledger call know it.
fn warm<'a>(file: &'a FileState, record: &Record) -> (&'a str, &'a str, [u64; 4]) {
    let (provider, model) = file.name(record);
    let [input, output, cache_read, cache_write, _] = record.3;
    (provider, model, [input, output, cache_read, cache_write])
}

/// One row's counters, for a model or a surface.
#[derive(Default)]
struct Tally {
    requests: u64,
    tokens: UsageTokens,
    recorded_usd: f64,
    estimated_usd: f64,
    free_requests: u64,
    unpriced_requests: u64,
    last_used_ms: i64,
}

impl Tally {
    /// Counts one request and returns what it cost.
    fn add(&mut self, tokens: &UsageTokens, price: Price, ms: i64) -> f64 {
        self.requests += 1;
        self.tokens.add(tokens);
        self.last_used_ms = self.last_used_ms.max(ms);
        match price {
            Price::Recorded(cost) => {
                self.recorded_usd += cost;
                cost
            }
            Price::Estimated(cost) => {
                self.estimated_usd += cost;
                cost
            }
            Price::Free => {
                self.free_requests += 1;
                0.0
            }
            Price::Unpriced => {
                self.unpriced_requests += 1;
                0.0
            }
        }
    }

    fn cost(&self) -> f64 {
        self.recorded_usd + self.estimated_usd
    }
}

pub(super) fn build(
    files: &[(&str, &FileState)],
    book: &PriceBook,
    query: &UsageQuery,
) -> UsageReport {
    // The earliest session holds the original of anything a fork copied.
    let mut files = files.to_vec();
    files.sort_by_key(|(path, file)| (file.started_ms, *path));
    let in_window = |ms: i64| query.since_ms.is_none_or(|since| ms >= since);

    // What a ledger call may repeat: every assistant message, and when each
    // cache warm was written.
    let mut answered = HashSet::new();
    let mut warmed: HashMap<_, Vec<i64>> = HashMap::new();
    let mut coverage = UsageCoverage::default();
    for (_, file) in &files {
        if file.kind == FileKind::Ledger {
            let from = file
                .ledger_from_ms
                .or_else(|| file.records.iter().map(Record::ms).min());
            coverage.ledger_from_ms = match (coverage.ledger_from_ms, from) {
                (Some(a), Some(b)) => Some(a.min(b)),
                (a, b) => a.or(b),
            };
            for warning in &file.warnings {
                if !coverage.warnings.contains(warning) {
                    coverage.warnings.push(warning.clone());
                }
            }
        }
        for record in &file.records {
            match record.source() {
                Source::Assistant => {
                    answered.insert(record.match_key());
                }
                Source::Usage => warmed
                    .entry(warm(file, record))
                    .or_default()
                    .push(record.ms()),
                _ => {}
            }
        }
    }
    let ledger_from = coverage.ledger_from_ms;

    let mut seen = HashSet::new();
    let mut counted = vec![];
    let mut duplicates = 0;
    for (ix, (_, file)) in files.iter().enumerate() {
        for record in &file.records {
            if !seen.insert(record.key()) {
                if in_window(record.ms()) {
                    duplicates += 1;
                }
                continue;
            }
            match record.source() {
                Source::Ledger => {
                    let warmed_too = || {
                        surface(file, record) == "core"
                            && warmed.get(&warm(file, record)).is_some_and(|times| {
                                times
                                    .iter()
                                    .any(|at| (at - record.ms()).abs() <= WARM_SLACK_MS)
                            })
                    };
                    if answered.contains(&record.match_key()) || warmed_too() {
                        if in_window(record.ms()) {
                            coverage.ledger_duplicates += 1;
                        }
                        continue;
                    }
                }
                // From its start the ledger has these calls itself.
                Source::Backfill if ledger_from.is_some_and(|from| record.ms() >= from) => {
                    continue;
                }
                _ => {}
            }
            counted.push((ix, record));
        }
    }
    let mut book = book.clone();
    book.learn(counted.iter().map(|(ix, record)| {
        let (provider, model) = files[*ix].1.name(record);
        (provider, model, record.tokens(), record.recorded())
    }));

    let width = match query.bucket {
        UsageBucket::Hour => HOUR_MS,
        UsageBucket::Day => DAY_MS,
    };
    let offset_ms = i64::from(query.utc_offset_minutes) * 60_000;
    let mut models: HashMap<(&str, &str), Tally> = HashMap::new();
    let mut surfaces: HashMap<&str, Tally> = HashMap::new();
    let mut series: BTreeMap<(i64, &str), (u64, f64)> = BTreeMap::new();
    let mut sessions = HashSet::new();
    let mut first_ms: Option<i64> = None;
    for (ix, record) in counted {
        let ms = record.ms();
        if !in_window(ms) {
            continue;
        }
        let file = files[ix].1;
        let (provider, model) = file.name(record);
        let surface = surface(file, record);
        let tokens = record.tokens();
        let price = book.price(provider, model, &tokens, record.recorded());
        surfaces.entry(surface).or_default().add(&tokens, price, ms);
        match record.source() {
            Source::Ledger => coverage.ledger_requests += 1,
            Source::Backfill => coverage.backfilled_requests += 1,
            _ => {}
        }
        if query.surface.as_deref().is_some_and(|only| only != surface) {
            continue;
        }
        let cost = models
            .entry((provider, model))
            .or_default()
            .add(&tokens, price, ms);
        let point = series
            .entry((bucket_start(ms, width, offset_ms), provider))
            .or_default();
        point.0 += tokens.total();
        point.1 += cost;
        if file.kind == FileKind::Session {
            sessions.insert(ix);
        }
        first_ms = Some(first_ms.map_or(ms, |first| first.min(ms)));
    }
    let mut models: Vec<UsageModelRow> = models
        .into_iter()
        .map(|((provider, model), tally)| UsageModelRow {
            provider: provider.to_owned(),
            model: model.to_owned(),
            requests: tally.requests,
            tokens: tally.tokens,
            recorded_usd: tally.recorded_usd,
            estimated_usd: tally.estimated_usd,
            free_requests: tally.free_requests,
            unpriced_requests: tally.unpriced_requests,
            last_used_ms: tally.last_used_ms,
        })
        .collect();
    models.sort_by(|a, b| {
        b.cost_usd()
            .total_cmp(&a.cost_usd())
            .then(b.tokens.total().cmp(&a.tokens.total()))
            .then_with(|| (&a.provider, &a.model).cmp(&(&b.provider, &b.model)))
    });
    let mut surfaces: Vec<(&str, Tally)> = surfaces.into_iter().collect();
    surfaces.sort_by(|(a_name, a), (b_name, b)| {
        b.cost()
            .total_cmp(&a.cost())
            .then(b.tokens.total().cmp(&a.tokens.total()))
            .then_with(|| a_name.cmp(b_name))
    });
    UsageReport {
        models,
        series: series
            .into_iter()
            .map(|((start_ms, provider), (tokens, cost_usd))| UsagePoint {
                start_ms,
                provider: provider.to_owned(),
                tokens,
                cost_usd,
            })
            .collect(),
        bucket: query.bucket,
        files: files
            .iter()
            .filter(|(_, file)| !file.gone && file.kind == FileKind::Session)
            .count() as u64,
        sessions: sessions.len() as u64,
        duplicates,
        first_ms,
        surfaces: surfaces
            .into_iter()
            .map(|(surface, tally)| UsageSurfaceRow {
                surface: surface.to_owned(),
                requests: tally.requests,
                tokens: tally.tokens,
                recorded_usd: tally.recorded_usd,
                estimated_usd: tally.estimated_usd,
                free_requests: tally.free_requests,
                unpriced_requests: tally.unpriced_requests,
                last_used_ms: tally.last_used_ms,
            })
            .collect(),
        coverage,
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::usage::scan::{
        scan_file,
        tests::{assistant, header, ledger_call},
    };
    use serde_json::json;

    fn file(dir: &std::path::Path, name: &str, lines: &[String]) -> FileState {
        file_of(dir, name, lines, FileKind::Session)
    }

    fn file_of(dir: &std::path::Path, name: &str, lines: &[String], kind: FileKind) -> FileState {
        let path = dir.join(name);
        std::fs::write(&path, lines.join("\n") + "\n").unwrap();
        scan_file(&path, None, kind).unwrap().unwrap()
    }

    fn subagent_result(timestamp: &str, input: u64) -> String {
        json!({"type":"custom_message","customType":"subagent-result","id":"r1","timestamp":timestamp,
            "content":format!("- Launch model: openai/gpt-5.6\n- Usage: input {input}, output 5, cache read 0, cache write 0, cost 0.2, turns 1"),
            "details":{"jobId":format!("job-{input}")}})
        .to_string()
    }

    #[test]
    fn every_surface_counts_once_whether_a_session_or_the_ledger_saw_it() {
        let dir = tempfile::tempdir().unwrap();
        // The ledger started at 2026-10-01T09:01:40Z.
        let ledger_from = 1_790_845_300_000;
        let session = file(
            dir.path(),
            "s.jsonl",
            &[
                header("2026-10-01T09:00:00Z"),
                assistant("a1", "anthropic", "claude-opus-5-5", 1_000, 10, 0.5),
                json!({"type":"usage","id":"w1","timestamp":"2026-10-01T10:02:00.000Z",
                    "kind":"cache_warm","provider":"anthropic","model":"claude-opus-5-5",
                    "usage":{"input":0,"output":0,"cacheRead":0,"cacheWrite":900,"cost":{"total":0.01}}})
                .to_string(),
                // Logged by the parent before the ledger, and after it.
                subagent_result("2026-10-01T09:00:30.000Z", 70),
                subagent_result("2026-10-01T10:00:00.000Z", 80),
            ],
        );
        let delegate = file(
            dir.path(),
            "d.jsonl",
            &[
                header("2026-10-01T09:30:00Z").replace("\"id\":\"s1\"", "\"id\":\"dlg-1\""),
                assistant("d1", "anthropic", "claude-opus-5-5", 3_000, 20, 0.25),
            ],
        );
        let ledger = file_of(
            dir.path(),
            "2026-10.jsonl",
            &[
                json!({"v":1,"t":"start","ts":ledger_from,"pid":1,"sid":"s1","depth":0}).to_string(),
                // The session holds this one already.
                ledger_call("1-a-1", "main", "anthropic", "claude-opus-5-5", 1_000, 10),
                ledger_call("2-a-1", "subagent", "openai", "gpt-5.6", 1_790_848_000_000, 30),
                ledger_call("3-a-1", "jev", "openrouter", "typesafe/jev-1", 1_790_848_100_000, 40),
                // The cache warm Pi also wrote into the session, 20 s earlier.
                json!({"v":1,"t":"call","id":"1-a-2","ts":1_790_848_900_000_i64,"provider":"anthropic",
                    "model":"claude-opus-5-5","u":{"i":0,"o":0,"cr":0,"cw":900},"cost":0.01,"surface":"core"})
                .to_string(),
                json!({"v":1,"t":"warn","ts":ledger_from,"what":"stream-hook-blind"}).to_string(),
            ],
            FileKind::Ledger,
        );
        let files = [
            ("s.jsonl", &session),
            ("d.jsonl", &delegate),
            ("2026-10.jsonl", &ledger),
        ];
        let report = build(&files, &PriceBook::default(), &UsageQuery::default());
        let surfaces: Vec<_> = report
            .surfaces
            .iter()
            .map(|row| (row.surface.as_str(), row.requests))
            .collect();
        assert_eq!(
            surfaces,
            [
                // Most spend first; equal spend, more tokens first.
                ("subagent", 2),
                ("jev", 1),
                ("main", 1),
                ("delegate", 1),
                ("cache_warm", 1),
            ]
        );
        assert_eq!(report.coverage.ledger_from_ms, Some(ledger_from));
        assert_eq!(report.coverage.ledger_requests, 2);
        assert_eq!(report.coverage.ledger_duplicates, 2);
        assert_eq!(report.coverage.backfilled_requests, 1);
        assert_eq!(report.coverage.warnings, ["stream-hook-blind"]);
        assert_eq!(report.sessions, 2);
        assert_eq!(report.files, 2);
        let total: u64 = report.models.iter().map(|row| row.requests).sum();
        assert_eq!(total, 6);

        // One surface's models and series; every surface still listed.
        let only = build(
            &files,
            &PriceBook::default(),
            &UsageQuery {
                surface: Some("subagent".into()),
                ..Default::default()
            },
        );
        assert_eq!(only.surfaces.len(), 5);
        assert_eq!(only.models.len(), 1);
        assert_eq!(only.models[0].requests, 2);
        assert_eq!(only.models[0].tokens.input, 100);
        assert!(only.series.iter().all(|point| point.provider == "openai"));

        // Without a ledger, everything the parents logged counts.
        let before = build(&files[..2], &PriceBook::default(), &UsageQuery::default());
        assert_eq!(before.coverage.backfilled_requests, 2);
        assert_eq!(before.coverage.ledger_from_ms, None);
    }

    #[test]
    fn a_fork_counts_its_copied_prefix_once_and_the_parent_keeps_the_credit() {
        let dir = tempfile::tempdir().unwrap();
        let shared = [
            assistant("a1", "anthropic", "claude-opus-5-5", 1_000, 10, 0.5),
            assistant("a2", "anthropic", "claude-opus-5-5", 2_000, 10, 0.5),
        ];
        let parent = file(
            dir.path(),
            "z-parent.jsonl",
            &[
                header("2026-10-01T09:00:00Z"),
                shared[0].clone(),
                shared[1].clone(),
            ],
        );
        let fork = file(
            dir.path(),
            "a-fork.jsonl",
            &[
                header("2026-10-01T09:30:00Z"),
                shared[0].clone(),
                shared[1].clone(),
                assistant("b1", "anthropic", "claude-opus-5-5", 3_000, 10, 0.5),
            ],
        );
        let report = build(
            &[("a-fork.jsonl", &fork), ("z-parent.jsonl", &parent)],
            &PriceBook::default(),
            &UsageQuery::default(),
        );
        assert_eq!(report.models.len(), 1);
        assert_eq!(report.models[0].requests, 3);
        assert!((report.models[0].recorded_usd - 1.5).abs() < 1e-9);
        assert_eq!(report.duplicates, 2);
        assert_eq!(report.sessions, 2);
        assert_eq!(report.files, 2);
    }

    #[test]
    fn a_provider_never_seen_before_shows_up_with_its_models() {
        let dir = tempfile::tempdir().unwrap();
        let state = file(
            dir.path(),
            "s.jsonl",
            &[
                header("2026-10-01T09:00:00Z"),
                assistant("a1", "acme-llm", "x-1", 1_000, 10, 0.0),
                assistant("a2", "anthropic", "claude-opus-5-5", 2_000, 10, 0.25),
            ],
        );
        let report = build(
            &[("s.jsonl", &state)],
            &PriceBook::default(),
            &UsageQuery::default(),
        );
        let acme = report
            .models
            .iter()
            .find(|row| row.provider == "acme-llm")
            .unwrap();
        assert_eq!(acme.model, "x-1");
        assert_eq!(acme.unpriced_requests, 1);
        // Totals add cache reads and writes but never reasoning again.
        assert_eq!(acme.tokens.total(), 10 + 10 + 100 + 5);
        assert_eq!(report.models[0].provider, "anthropic");
    }

    #[test]
    fn days_start_at_the_requesters_midnight_and_the_window_drops_older_requests() {
        let dir = tempfile::tempdir().unwrap();
        // 2026-10-01 20:00 and 2026-10-02 02:00 UTC: one day at UTC+5:30.
        let (evening, night) = (1_790_884_800_000, 1_790_906_400_000);
        let state = file(
            dir.path(),
            "s.jsonl",
            &[
                header("2026-10-01T09:00:00Z"),
                assistant("a0", "p", "m", 1_000, 10, 1.0),
                assistant("a1", "p", "m", evening, 10, 1.0),
                assistant("a2", "p", "m", night, 10, 1.0),
            ],
        );
        let query = UsageQuery {
            since_ms: Some(evening),
            utc_offset_minutes: 330,
            ..Default::default()
        };
        let report = build(&[("s.jsonl", &state)], &PriceBook::default(), &query);
        assert_eq!(report.series.len(), 1, "{:?}", report.series);
        // Local midnight of 2026-10-02 at UTC+5:30 is 2026-10-01 18:30 UTC.
        assert_eq!(report.series[0].start_ms, 1_790_879_400_000);
        assert!((report.series[0].cost_usd - 2.0).abs() < 1e-9);
        assert_eq!(report.models[0].requests, 2);
        assert_eq!(report.first_ms, Some(evening));

        let hourly = build(
            &[("s.jsonl", &state)],
            &PriceBook::default(),
            &UsageQuery {
                bucket: UsageBucket::Hour,
                ..query
            },
        );
        assert_eq!(hourly.series.len(), 2);
    }
}
