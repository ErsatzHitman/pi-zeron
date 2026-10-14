//! The session panel's Compaction section: whether the context is near the
//! point Pi compacts it, or is being compacted now, and what the section
//! says. Pure, so it is unit-tested without a window; drawn by
//! `Shell::render_compact`.

use chrono::{DateTime, Utc};
use zeron_proto::ContextUsage;
use zeron_proto::extension_ui::CompactStatus;

/// Near: within this share of the context window below the threshold. Once
/// shown it stays until further than `KEEP_BAND`, so a wobbling reading
/// does not fold the section in and out.
const NEAR_BAND: f64 = 0.10;
const KEEP_BAND: f64 = 0.12;

#[derive(Debug, Clone, PartialEq)]
pub(super) enum Phase {
    Near,
    Compacting {
        since: Option<DateTime<Utc>>,
        reason: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub(super) struct CompactMeter {
    pub phase: Phase,
    pub tokens: Option<u64>,
    pub threshold: Option<u64>,
    pub error: Option<String>,
}

impl CompactMeter {
    /// `None` while there is nothing to say: auto-compaction off, or the
    /// context still far from where it compacts. `shown` is whether the
    /// section is up already (the hysteresis latch).
    pub fn of(
        status: Option<&CompactStatus>,
        usage: Option<ContextUsage>,
        shown: bool,
    ) -> Option<Self> {
        let status = status?;
        // A compaction just finished: the measured usage has not caught up.
        let tokens = status.tokens_after.or(usage.and_then(|usage| usage.tokens));
        let base = |phase| Self {
            phase,
            tokens,
            threshold: status.threshold_tokens,
            error: status.error.clone(),
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
        (tokens? >= start).then(|| base(Phase::Near))
    }

    /// When the running compaction began, for the section's seat.
    pub fn since(&self) -> Option<DateTime<Utc>> {
        match &self.phase {
            Phase::Compacting { since, .. } => *since,
            Phase::Near => None,
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

    #[test]
    fn nothing_shows_far_from_the_limit_or_with_auto_compaction_off() {
        assert_eq!(CompactMeter::of(None, usage(130_000), false), None);
        assert_eq!(
            CompactMeter::of(Some(&status(false)), usage(130_000), false),
            None
        );
        let unknown = CompactStatus {
            threshold_tokens: None,
            ..status(true)
        };
        assert_eq!(
            CompactMeter::of(Some(&unknown), usage(130_000), false),
            None
        );
        assert_eq!(CompactMeter::of(Some(&status(true)), None, false), None);
        let no_window = Some(ContextUsage {
            tokens: Some(130_000),
            window: None,
        });
        assert_eq!(
            CompactMeter::of(Some(&status(true)), no_window, false),
            None
        );
    }

    #[test]
    fn near_starts_ten_points_of_the_window_below_the_threshold_and_lingers() {
        let on = status(true);
        assert_eq!(CompactMeter::of(Some(&on), usage(114_463), false), None);
        let near = CompactMeter::of(Some(&on), usage(114_464), false).unwrap();
        assert_eq!(near.phase, Phase::Near);
        assert_eq!(near.caption(), "Compacts at 134,464 tokens");
        assert!(!near.compacting());
        // Once up, it stays a little longer.
        assert_eq!(CompactMeter::of(Some(&on), usage(112_000), false), None);
        assert!(CompactMeter::of(Some(&on), usage(112_000), true).is_some());
        assert_eq!(CompactMeter::of(Some(&on), usage(110_000), true), None);
    }

    #[test]
    fn a_running_compaction_always_shows() {
        let running = CompactStatus {
            compacting: true,
            started_at_ms: Some(1_000),
            reason: Some("manual".into()),
            ..status(false)
        };
        let meter = CompactMeter::of(Some(&running), None, false).unwrap();
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
        assert_eq!(CompactMeter::of(Some(&after), usage(130_000), false), None);
    }

    #[test]
    fn the_bar_fills_toward_the_threshold_and_a_failure_shows_while_near() {
        let failed = CompactStatus {
            error: Some("too large".into()),
            ..status(true)
        };
        let meter = CompactMeter::of(Some(&failed), usage(200_000), false).unwrap();
        assert_eq!(meter.fraction(), 1.0);
        assert_eq!(meter.caption(), "Compaction failed: too large");
        let half = CompactMeter {
            phase: Phase::Near,
            tokens: Some(THRESHOLD / 2),
            threshold: Some(THRESHOLD),
            error: None,
        };
        assert!((half.fraction() - 0.5).abs() < 0.001);
        let unknown = CompactMeter {
            threshold: None,
            ..half
        };
        assert_eq!(unknown.fraction(), 0.0);
        assert_eq!(unknown.caption(), "Near the compaction limit");
    }
}
