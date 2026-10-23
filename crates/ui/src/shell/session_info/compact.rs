//! The session panel's Compaction section: whether the context is near the
//! point Pi compacts it, is being compacted now, or was just compacted from
//! the cached request, and what the section says. Pure, so it is unit-tested
//! without a window; drawn by `Shell::render_compact`.

use chrono::{DateTime, Utc};
use zeron_proto::ContextUsage;
use zeron_proto::extension_ui::{CachedCompactionState, CompactStatus};

/// Near: within this share of the context window below the threshold. Once
/// shown it stays until further than `KEEP_BAND`, so a wobbling reading
/// does not fold the section in and out.
const NEAR_BAND: f64 = 0.10;
const KEEP_BAND: f64 = 0.12;

/// Below this share of the summary call's prompt read from cache, the replay
/// missed the cache: the section claims no saving.
const COLD_SHARE: f64 = 0.02;

#[derive(Debug, Clone, PartialEq)]
pub(super) enum Phase {
    Near,
    Compacting {
        since: Option<DateTime<Utc>>,
        reason: Option<String>,
    },
    /// A cached compaction just finished and the context is below the
    /// threshold: the section shows what it cost until the next reply.
    Compacted,
}

/// What the last cached compaction cost against Pi's own compaction (which is
/// an estimate: Pi's request was never sent).
#[derive(Debug, Clone, PartialEq)]
pub(super) struct CompactionCost {
    /// The measured cost of the cached summary call (`usage.cost.total`).
    pub cached_usd: f64,
    /// Pi's own compaction's estimated cost, from chars/4 and the prices.
    pub native_usd: Option<f64>,
    /// `native_usd - cached_usd`; negative means the cached path cost more.
    pub saved_usd: Option<f64>,
    /// cacheRead / prompt tokens of the summary call.
    pub cache_share: Option<f64>,
}

impl CompactionCost {
    /// The last compaction's cost, for one that actually took the cached path.
    fn of(state: &CachedCompactionState) -> Option<Self> {
        let last = state
            .last
            .as_ref()
            .filter(|last| last.outcome == "compaction")?;
        Some(Self {
            cached_usd: last.cost_usd.unwrap_or(0.0),
            native_usd: last.native_cost_usd,
            saved_usd: last.saved_usd,
            cache_share: last.cache_share,
        })
    }

    /// What the cached path saved against Pi's estimate, when it saved.
    pub fn saved(&self) -> Option<f64> {
        self.saved_usd.filter(|saved| *saved > 0.0)
    }

    /// The share of Pi's estimated cost the cached path saved, 0..1.
    pub fn saved_share(&self) -> Option<f64> {
        let native = self.native_usd.filter(|native| *native > 0.0)?;
        Some((self.saved()? / native).clamp(0.0, 1.0))
    }

    /// The replay missed the cache, so there is nothing to claim.
    pub fn cold(&self) -> bool {
        self.cache_share.unwrap_or(0.0) < COLD_SHARE
    }

    /// The measured line: what the summary call cost, and how much of its
    /// prompt came from the cache.
    pub fn cached_line(&self) -> String {
        match self.cache_share {
            Some(share) => format!(
                "cached {} · {:.0}% from cache",
                money(self.cached_usd),
                (share * 100.0).clamp(0.0, 100.0)
            ),
            None => format!("cached {}", money(self.cached_usd)),
        }
    }

    /// The comparison line: Pi's own compaction, estimated, and the difference.
    pub fn native_line(&self) -> String {
        let Some(native) = self.native_usd else {
            return "native est. unknown".into();
        };
        let tail = match self.saved_usd {
            Some(saved) if saved > 0.0 => format!(" · saved {} (est.)", money(saved)),
            Some(saved) if saved < 0.0 => format!(" · cost {} more", money(-saved)),
            Some(_) => " · broke even".into(),
            None => String::new(),
        };
        format!("native est. {}{tail}", money(native))
    }
}

fn money(value: f64) -> String {
    if value >= 1.0 {
        format!("${value:.2}")
    } else {
        format!("${value:.3}")
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(super) struct CompactMeter {
    pub phase: Phase,
    pub tokens: Option<u64>,
    pub threshold: Option<u64>,
    pub error: Option<String>,
    /// What the last cached compaction cost, when the cached-compaction
    /// extension reports one.
    pub cost: Option<CompactionCost>,
}

impl CompactMeter {
    /// `None` while there is nothing to say: auto-compaction off, or the
    /// context still far from where it compacts. `shown` is whether the
    /// section is up already (the hysteresis latch). `cached` is the
    /// cached-compaction channel, whose last compaction turns the section
    /// into a cost card while its reading catches up.
    pub fn of(
        status: Option<&CompactStatus>,
        usage: Option<ContextUsage>,
        shown: bool,
        cached: Option<&CachedCompactionState>,
    ) -> Option<Self> {
        let status = status?;
        // A compaction just finished: the measured usage has not caught up.
        let tokens = status.tokens_after.or(usage.and_then(|usage| usage.tokens));
        let cost = cached.and_then(CompactionCost::of);
        let base = |phase| Self {
            phase,
            tokens,
            threshold: status.threshold_tokens,
            error: status.error.clone(),
            cost: cost.clone(),
        };
        if status.compacting {
            let since = status
                .started_at_ms
                .and_then(|ms| DateTime::from_timestamp_millis(ms as i64));
            return Some(base(Phase::Compacting {
                since,
                reason: status.reason.clone(),
            }));
        }
        if !status.enabled {
            return None;
        }
        let threshold = status.threshold_tokens.filter(|tokens| *tokens > 0)?;
        let window = usage
            .and_then(|usage| usage.window)
            .filter(|window| *window > 0)?;
        let band = if shown { KEEP_BAND } else { NEAR_BAND };
        let start = threshold.saturating_sub((window as f64 * band) as u64);
        if tokens? >= start {
            return Some(base(Phase::Near));
        }
        // Just compacted from cache: the section shows the cost card until
        // the next reply clears `tokens_after`.
        (status.tokens_after.is_some() && cost.is_some()).then(|| base(Phase::Compacted))
    }

    /// When the running compaction began, for the section's seat.
    pub fn since(&self) -> Option<DateTime<Utc>> {
        match &self.phase {
            Phase::Compacting { since, .. } => *since,
            Phase::Near | Phase::Compacted => None,
        }
    }

    pub fn compacting(&self) -> bool {
        matches!(self.phase, Phase::Compacting { .. })
    }

    /// Share of the way to the threshold, 0..1: the bar's end is where Pi
    /// compacts, not the model's window (the composer's ring shows that).
    pub fn fraction(&self) -> f32 {
        match (self.tokens, self.threshold) {
            (Some(tokens), Some(threshold)) if threshold > 0 => {
                (tokens as f64 / threshold as f64).clamp(0.0, 1.0) as f32
            }
            _ => 0.0,
        }
    }

    pub fn caption(&self) -> String {
        match (&self.phase, &self.error, self.threshold) {
            (Phase::Compacted, ..) => match &self.cost {
                Some(cost) if cost.saved().is_some() => match cost.saved_share() {
                    Some(share) => format!("Compacted from cache · -{:.0}% vs Pi", share * 100.0),
                    None => "Compacted from cache".into(),
                },
                Some(cost) if cost.cold() => "Compacted · cache cold, no saving".into(),
                _ => "Compacted · no saving".into(),
            },
            (Phase::Compacting { reason, .. }, ..) => match reason.as_deref() {
                Some("manual") => "Compacting on request…".into(),
                Some("overflow") => "Context overflowed, compacting…".into(),
                _ => "Compacting context…".into(),
            },
            (Phase::Near, Some(error), _) => format!("Compaction failed: {error}"),
            (Phase::Near, None, Some(threshold)) => format!(
                "Compacts at {} tokens",
                crate::context_usage::with_separators(threshold)
            ),
            (Phase::Near, None, None) => "Near the compaction limit".into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zeron_proto::extension_ui::{CachedCompactionLast, CachedCompactionState};

    const THRESHOLD: u64 = 134_464;

    fn status(enabled: bool) -> CompactStatus {
        CompactStatus {
            enabled,
            threshold_tokens: Some(THRESHOLD),
            ..Default::default()
        }
    }

    fn usage(tokens: u64) -> Option<ContextUsage> {
        Some(ContextUsage {
            tokens: Some(tokens),
            window: Some(200_000),
        })
    }

    /// A cached compaction of `cost` with the given cache share and saving.
    fn cached(share: Option<f64>, saved: Option<f64>) -> CachedCompactionState {
        CachedCompactionState {
            last: Some(CachedCompactionLast {
                outcome: "compaction".into(),
                cost_usd: Some(0.0481),
                native_cost_usd: Some(0.1459),
                saved_usd: saved,
                cache_share: share,
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    #[test]
    fn nothing_shows_far_from_the_limit_or_with_auto_compaction_off() {
        assert_eq!(CompactMeter::of(None, usage(130_000), false, None), None);
        assert_eq!(
            CompactMeter::of(Some(&status(false)), usage(130_000), false, None),
            None
        );
        let unknown = CompactStatus {
            threshold_tokens: None,
            ..status(true)
        };
        assert_eq!(
            CompactMeter::of(Some(&unknown), usage(130_000), false, None),
            None
        );
        assert_eq!(CompactMeter::of(Some(&status(true)), None, false, None), None);
        let no_window = Some(ContextUsage {
            tokens: Some(130_000),
            window: None,
        });
        assert_eq!(
            CompactMeter::of(Some(&status(true)), no_window, false, None),
            None
        );
    }

    #[test]
    fn near_starts_ten_points_of_the_window_below_the_threshold_and_lingers() {
        let on = status(true);
        assert_eq!(
            CompactMeter::of(Some(&on), usage(114_463), false, None),
            None
        );
        let near = CompactMeter::of(Some(&on), usage(114_464), false, None).unwrap();
        assert_eq!(near.phase, Phase::Near);
        assert_eq!(near.caption(), "Compacts at 134,464 tokens");
        assert!(!near.compacting());
        // Once up, it stays a little longer.
        assert_eq!(
            CompactMeter::of(Some(&on), usage(112_000), false, None),
            None
        );
        assert!(CompactMeter::of(Some(&on), usage(112_000), true, None).is_some());
        assert_eq!(CompactMeter::of(Some(&on), usage(110_000), true, None), None);
    }

    #[test]
    fn a_running_compaction_always_shows() {
        let running = CompactStatus {
            compacting: true,
            started_at_ms: Some(1_000),
            reason: Some("manual".into()),
            ..status(false)
        };
        let meter = CompactMeter::of(Some(&running), None, false, None).unwrap();
        assert!(meter.compacting());
        assert_eq!(meter.since(), DateTime::from_timestamp_millis(1_000));
        assert_eq!(meter.caption(), "Compacting on request…");
    }

    #[test]
    fn what_a_compaction_left_wins_over_a_stale_measurement() {
        let after = CompactStatus {
            tokens_after: Some(9),
            ..status(true)
        };
        assert_eq!(
            CompactMeter::of(Some(&after), usage(130_000), false, None),
            None
        );
    }

    #[test]
    fn the_bar_fills_toward_the_threshold_and_a_failure_shows_while_near() {
        let failed = CompactStatus {
            error: Some("too large".into()),
            ..status(true)
        };
        let meter = CompactMeter::of(Some(&failed), usage(200_000), false, None).unwrap();
        assert_eq!(meter.fraction(), 1.0);
        assert_eq!(meter.caption(), "Compaction failed: too large");
        let half = CompactMeter {
            phase: Phase::Near,
            tokens: Some(THRESHOLD / 2),
            threshold: Some(THRESHOLD),
            error: None,
            cost: None,
        };
        assert!((half.fraction() - 0.5).abs() < 0.001);
        let unknown = CompactMeter {
            threshold: None,
            ..half
        };
        assert_eq!(unknown.fraction(), 0.0);
        assert_eq!(unknown.caption(), "Near the compaction limit");
    }

    #[test]
    fn a_just_compacted_cache_shows_its_cost_until_the_next_reply() {
        let after = CompactStatus {
            tokens_after: Some(9_000),
            ..status(true)
        };
        let cached = cached(Some(0.99), Some(0.0978));
        let meter = CompactMeter::of(Some(&after), usage(130_000), false, Some(&cached)).unwrap();
        assert_eq!(meter.phase, Phase::Compacted);
        assert_eq!(meter.since(), None);
        assert_eq!(meter.caption(), "Compacted from cache · -67% vs Pi");
        let cost = meter.cost.as_ref().unwrap();
        assert_eq!(cost.cached_line(), "cached $0.048 · 99% from cache");
        assert_eq!(cost.native_line(), "native est. $0.146 · saved $0.098 (est.)");
        // Once the reply clears `tokens_after`, the phase folds away even
        // though the cost is still reported.
        let replied = CompactStatus {
            tokens_after: None,
            ..status(true)
        };
        assert_eq!(
            CompactMeter::of(Some(&replied), usage(9_000), false, Some(&cached)),
            None
        );
    }

    #[test]
    fn a_cold_replay_claims_no_saving() {
        let after = CompactStatus {
            tokens_after: Some(9_000),
            ..status(true)
        };
        let cold = cached(Some(0.0), None);
        let meter = CompactMeter::of(Some(&after), usage(9_000), false, Some(&cold)).unwrap();
        assert_eq!(meter.phase, Phase::Compacted);
        assert_eq!(meter.caption(), "Compacted · cache cold, no saving");
        let cost = meter.cost.as_ref().unwrap();
        assert!(cost.cold());
        assert_eq!(cost.saved(), None);
        // A path that cost more than Pi's says so.
        let more = cached(Some(0.1), Some(-0.02));
        let meter = CompactMeter::of(Some(&after), usage(9_000), false, Some(&more)).unwrap();
        assert_eq!(
            meter.cost.as_ref().unwrap().native_line(),
            "native est. $0.146 · cost $0.020 more"
        );
        assert_eq!(meter.caption(), "Compacted · no saving");
        // A native fallback reports no cost card at all.
        let native = CachedCompactionState {
            last: Some(CachedCompactionLast {
                outcome: "native".into(),
                cost_usd: Some(0.1),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(
            CompactMeter::of(Some(&after), usage(9_000), false, Some(&native)),
            None
        );
    }
}
