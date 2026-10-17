//! Tokio scheduling-lag monitor.
//!
//! A 50 ms interval task measures how late each tick actually runs. A tick
//! that is >= 100 ms late means the runtime could not schedule a ready task
//! (a blocking call on a worker, or starved workers), which is exactly what
//! makes a turn feel stuck between the harness and the UI. It logs a
//! `zeron::lag` warning, at most one per 10 s; late ticks inside that window
//! are folded into the next line as `suppressed` and `max_late_ms`.
//!
//! Always on and cheap (about 20 wakeups a second, no allocation). Tune with
//! `ZERON_LAG_WARN_MS` (milliseconds; `0` turns the monitor off).

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tokio::time::{Instant, MissedTickBehavior};

const TICK: Duration = Duration::from_millis(50);
const DEFAULT_THRESHOLD: Duration = Duration::from_millis(100);
const WARN_WINDOW: Duration = Duration::from_secs(10);

/// What one emitted warning says.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LagReport {
    /// How late the tick that triggered this report was.
    pub late: Duration,
    /// The worst lag since the previous report (>= `late`).
    pub max_late: Duration,
    /// Late ticks swallowed by the rate limit since the previous report.
    pub suppressed: u32,
}

/// Rate-limit state carried between ticks.
#[derive(Default)]
struct Limiter {
    last_warn: Option<Instant>,
    suppressed: u32,
    max_late: Duration,
}

/// Decide whether a tick that ran `late` after its deadline is worth a line.
fn judge(
    threshold: Duration,
    late: Duration,
    now: Instant,
    state: &mut Limiter,
) -> Option<LagReport> {
    if threshold.is_zero() || late < threshold {
        return None;
    }
    state.max_late = state.max_late.max(late);
    if state
        .last_warn
        .is_some_and(|at| now.saturating_duration_since(at) < WARN_WINDOW)
    {
        state.suppressed = state.suppressed.saturating_add(1);
        return None;
    }
    let report = LagReport {
        late,
        max_late: state.max_late,
        suppressed: state.suppressed,
    };
    *state = Limiter {
        last_warn: Some(now),
        ..Limiter::default()
    };
    Some(report)
}

/// Tick forever, handing each rate-limited report to `sink`.
async fn run(threshold: Duration, mut sink: impl FnMut(LagReport)) {
    let mut tick = tokio::time::interval(TICK);
    // Delay (not Burst): after a stall we want one late tick, not a catch-up storm.
    tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
    tick.tick().await;
    let mut expected = Instant::now() + TICK;
    let mut state = Limiter::default();
    loop {
        tick.tick().await;
        let now = Instant::now();
        let late = now.saturating_duration_since(expected);
        if let Some(report) = judge(threshold, late, now, &mut state) {
            sink(report);
        }
        expected = now + TICK;
    }
}

fn threshold() -> Duration {
    std::env::var("ZERON_LAG_WARN_MS")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .map_or(DEFAULT_THRESHOLD, Duration::from_millis)
}

/// Start the monitor on the current runtime, once per process. A no-op
/// outside a runtime (so the synchronous engine assembly may call it) and
/// when `ZERON_LAG_WARN_MS=0`. The task holds no engine state and ends with
/// its runtime.
pub fn ensure_started() {
    static STARTED: AtomicBool = AtomicBool::new(false);
    let threshold = threshold();
    if threshold.is_zero() {
        return;
    }
    let Ok(runtime) = tokio::runtime::Handle::try_current() else {
        return;
    };
    if STARTED.swap(true, Ordering::AcqRel) {
        return;
    }
    runtime.spawn(run(threshold, |report| {
        tracing::warn!(
            target: "zeron::lag",
            late_ms = report.late.as_millis() as u64,
            max_late_ms = report.max_late.as_millis() as u64,
            suppressed = report.suppressed,
            "tokio scheduling lag"
        );
    }));
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    const T: Duration = Duration::from_millis(100);

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    #[test]
    fn below_threshold_is_quiet() {
        let mut state = Limiter::default();
        assert_eq!(judge(T, ms(99), Instant::now(), &mut state), None);
        assert_eq!(state.suppressed, 0);
    }

    #[test]
    fn threshold_zero_disables() {
        let mut state = Limiter::default();
        assert_eq!(
            judge(Duration::ZERO, ms(5000), Instant::now(), &mut state),
            None
        );
    }

    #[test]
    fn at_threshold_reports_then_rate_limits() {
        let mut state = Limiter::default();
        let t0 = Instant::now();
        let first = judge(T, ms(100), t0, &mut state).unwrap();
        assert_eq!(first.late, ms(100));
        assert_eq!(first.suppressed, 0);
        assert_eq!(judge(T, ms(150), t0 + ms(1000), &mut state), None);
        assert_eq!(judge(T, ms(400), t0 + ms(2000), &mut state), None);
        assert_eq!(state.suppressed, 2);
        let next = judge(T, ms(120), t0 + WARN_WINDOW, &mut state).unwrap();
        assert_eq!(next.late, ms(120));
        assert_eq!(next.max_late, ms(400));
        assert_eq!(next.suppressed, 2);
        // The counters reset after a report.
        assert_eq!(state.suppressed, 0);
        assert_eq!(state.max_late, Duration::ZERO);
    }

    #[tokio::test(start_paused = true)]
    async fn late_tick_reaches_the_sink() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let sink = seen.clone();
        let task = tokio::spawn(run(T, move |r| sink.lock().unwrap().push(r)));
        tokio::task::yield_now().await;
        assert!(seen.lock().unwrap().is_empty());
        // One jump of the paused clock: the tick runs 250 ms after its deadline.
        tokio::time::advance(ms(300)).await;
        tokio::task::yield_now().await;
        let got = seen.lock().unwrap().clone();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].late, ms(250));
        task.abort();
    }
}
