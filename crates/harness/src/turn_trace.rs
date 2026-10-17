//! Opt-in per-turn timing marks (`ZERON_TURN_TRACE=1`).
//!
//! The Pi driver marks `prompt_sent`, `ack` and `barrier`; the engine's run
//! driver marks the first agent event, the first doc commit and `done`, then
//! logs one line per turn. The marks travel on the shared
//! [`crate::TurnControl`], so no event type carries timings.
//!
//! Off by default: every entry point returns after one cached bool load.

use std::sync::{Mutex, OnceLock, PoisonError};
use std::time::Instant;

/// Whether `ZERON_TURN_TRACE` asks for turn traces (read once).
pub fn enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| {
        std::env::var("ZERON_TURN_TRACE").is_ok_and(|v| {
            matches!(
                v.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "on" | "yes"
            )
        })
    })
}

/// A moment in a turn after `prompt_sent` (which is the turn's t0).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    /// Pi accepted the prompt (its `prompt` response).
    Ack,
    /// Pi's `get_state` response after the ack.
    Barrier,
    /// First agent output of the turn.
    FirstEvent,
    /// First successful doc commit of the turn.
    FirstCommit,
}

#[derive(Default)]
struct Marks {
    t0: Option<Instant>,
    ack: Option<Instant>,
    barrier: Option<Instant>,
    first_event: Option<Instant>,
    first_commit: Option<Instant>,
}

/// Millisecond offsets from `prompt_sent`; a phase the turn never reached is `None`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TurnTimings {
    pub ack_ms: Option<u64>,
    pub barrier_ms: Option<u64>,
    pub first_event_ms: Option<u64>,
    pub first_commit_ms: Option<u64>,
    pub done_ms: u64,
}

/// One turn's marks. `Default` follows the environment flag.
pub struct TurnTrace {
    enabled: bool,
    marks: Mutex<Marks>,
}

impl Default for TurnTrace {
    fn default() -> Self {
        Self::with_enabled(enabled())
    }
}

impl TurnTrace {
    /// A trace that records regardless of the environment (tests).
    pub fn with_enabled(enabled: bool) -> Self {
        Self {
            enabled,
            marks: Mutex::default(),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Marks> {
        self.marks.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The prompt is on the wire: start a fresh turn, dropping stale marks.
    pub fn begin(&self) {
        self.begin_at(Instant::now());
    }

    pub fn mark(&self, phase: Phase) {
        if self.enabled {
            self.mark_at(phase, Instant::now());
        }
    }

    /// Take the turn's timings; `None` when it never began (or already finished).
    pub fn finish(&self) -> Option<TurnTimings> {
        self.finish_at(Instant::now())
    }

    fn begin_at(&self, now: Instant) {
        if !self.enabled {
            return;
        }
        *self.lock() = Marks {
            t0: Some(now),
            ..Marks::default()
        };
    }

    fn mark_at(&self, phase: Phase, now: Instant) {
        if !self.enabled {
            return;
        }
        let mut marks = self.lock();
        if marks.t0.is_none() {
            return;
        }
        let slot = match phase {
            Phase::Ack => &mut marks.ack,
            Phase::Barrier => &mut marks.barrier,
            Phase::FirstEvent => &mut marks.first_event,
            Phase::FirstCommit => &mut marks.first_commit,
        };
        slot.get_or_insert(now);
    }

    fn finish_at(&self, now: Instant) -> Option<TurnTimings> {
        if !self.enabled {
            return None;
        }
        let marks = std::mem::take(&mut *self.lock());
        let t0 = marks.t0?;
        let ms = |at: Instant| at.saturating_duration_since(t0).as_millis() as u64;
        Some(TurnTimings {
            ack_ms: marks.ack.map(ms),
            barrier_ms: marks.barrier.map(ms),
            first_event_ms: marks.first_event.map(ms),
            first_commit_ms: marks.first_commit.map(ms),
            done_ms: ms(now),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn disabled_trace_is_inert() {
        let t = TurnTrace::with_enabled(false);
        t.begin();
        t.mark(Phase::Ack);
        assert_eq!(t.finish(), None);
    }

    #[test]
    fn records_offsets_and_finishes_once() {
        let t = TurnTrace::with_enabled(true);
        let t0 = Instant::now();
        t.begin_at(t0);
        t.mark_at(Phase::Ack, t0 + Duration::from_millis(5));
        t.mark_at(Phase::Barrier, t0 + Duration::from_millis(9));
        t.mark_at(Phase::FirstEvent, t0 + Duration::from_millis(40));
        t.mark_at(Phase::FirstCommit, t0 + Duration::from_millis(70));
        let got = t.finish_at(t0 + Duration::from_millis(100)).unwrap();
        assert_eq!(
            got,
            TurnTimings {
                ack_ms: Some(5),
                barrier_ms: Some(9),
                first_event_ms: Some(40),
                first_commit_ms: Some(70),
                done_ms: 100,
            }
        );
        assert_eq!(t.finish(), None);
    }

    #[test]
    fn second_mark_is_ignored() {
        let t = TurnTrace::with_enabled(true);
        let t0 = Instant::now();
        t.begin_at(t0);
        t.mark_at(Phase::Ack, t0 + Duration::from_millis(3));
        t.mark_at(Phase::Ack, t0 + Duration::from_millis(30));
        assert_eq!(
            t.finish_at(t0 + Duration::from_millis(50)).unwrap().ack_ms,
            Some(3)
        );
    }

    #[test]
    fn marks_before_begin_are_dropped_and_begin_resets() {
        let t = TurnTrace::with_enabled(true);
        let t0 = Instant::now();
        t.mark_at(Phase::FirstEvent, t0);
        assert_eq!(t.finish_at(t0), None);
        t.begin_at(t0);
        t.mark_at(Phase::Ack, t0 + Duration::from_millis(2));
        t.begin_at(t0 + Duration::from_millis(10));
        let got = t.finish_at(t0 + Duration::from_millis(20)).unwrap();
        assert_eq!(got.ack_ms, None);
        assert_eq!(got.done_ms, 10);
    }
}
