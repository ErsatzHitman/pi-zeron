//! Scanned files to a [`UsageReport`]: fork copies counted once, each
//! request priced, totals per model and per provider over time.
use super::{
    price::{Price, PriceBook},
    scan::FileState,
};
use std::collections::{BTreeMap, HashMap, HashSet};
use zeron_proto::usage::{UsageBucket, UsageModelRow, UsagePoint, UsageQuery, UsageReport};

const HOUR_MS: i64 = 3_600_000;
const DAY_MS: i64 = 24 * HOUR_MS;

/// The start of the bucket holding `ms`, at the requester's midnight.
fn bucket_start(ms: i64, width: i64, offset_ms: i64) -> i64 {
    (ms + offset_ms).div_euclid(width) * width - offset_ms
}

pub(super) fn build(
    files: &[(&str, &FileState)],
    book: &PriceBook,
    query: &UsageQuery,
) -> UsageReport {
    // The earliest session holds the original of anything a fork copied.
    let mut files = files.to_vec();
    files.sort_by_key(|(path, file)| (file.started_ms, *path));
    let mut seen = HashSet::new();
    let mut counted = vec![];
    let mut duplicates = 0;
    for (ix, (_, file)) in files.iter().enumerate() {
        for record in &file.records {
            if seen.insert(record.key()) {
                counted.push((ix, record));
            } else if query.since_ms.is_none_or(|since| record.ms() >= since) {
                duplicates += 1;
            }
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
    let mut models: HashMap<(&str, &str), UsageModelRow> = HashMap::new();
    let mut series: BTreeMap<(i64, &str), (u64, f64)> = BTreeMap::new();
    let mut sessions = HashSet::new();
    let mut first_ms: Option<i64> = None;
    for (ix, record) in counted {
        let ms = record.ms();
        if query.since_ms.is_some_and(|since| ms < since) {
            continue;
        }
        let file = files[ix].1;
        let (provider, model) = file.name(record);
        let tokens = record.tokens();
        let row = models
            .entry((provider, model))
            .or_insert_with(|| UsageModelRow {
                provider: provider.to_owned(),
                model: model.to_owned(),
                ..Default::default()
            });
        row.requests += 1;
        row.tokens.add(&tokens);
        row.last_used_ms = row.last_used_ms.max(ms);
        let cost = match book.price(provider, model, &tokens, record.recorded()) {
            Price::Recorded(cost) => {
                row.recorded_usd += cost;
                cost
            }
            Price::Estimated(cost) => {
                row.estimated_usd += cost;
                cost
            }
            Price::Free => {
                row.free_requests += 1;
                0.0
            }
            Price::Unpriced => {
                row.unpriced_requests += 1;
                0.0
            }
        };
        let point = series
            .entry((bucket_start(ms, width, offset_ms), provider))
            .or_default();
        point.0 += tokens.total();
        point.1 += cost;
        sessions.insert(ix);
        first_ms = Some(first_ms.map_or(ms, |first| first.min(ms)));
    }
    let mut models: Vec<UsageModelRow> = models.into_values().collect();
    models.sort_by(|a, b| {
        b.cost_usd()
            .total_cmp(&a.cost_usd())
            .then(b.tokens.total().cmp(&a.tokens.total()))
            .then_with(|| (&a.provider, &a.model).cmp(&(&b.provider, &b.model)))
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
        files: files.iter().filter(|(_, file)| !file.gone).count() as u64,
        sessions: sessions.len() as u64,
        duplicates,
        first_ms,
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::usage::scan::{
        scan_file,
        tests::{assistant, header},
    };

    fn file(dir: &std::path::Path, name: &str, lines: &[String]) -> FileState {
        let path = dir.join(name);
        std::fs::write(&path, lines.join("\n") + "\n").unwrap();
        scan_file(&path, None).unwrap().unwrap()
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
