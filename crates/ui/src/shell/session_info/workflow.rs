//! The session panel's Workflow section: the Pi workflow run under this
//! chat, from the workflows extension's `workflow:state` channel. Pure, so
//! it is unit-tested without a window; drawn by `Shell::render_workflow`.
//!
//! The section is up while there is something to act on or watch: a run in
//! flight, a paused or interrupted run that can be resumed, or a run that
//! just ended (briefly, so its last tick is seen). Tasks group the way the
//! Sub-agents section does: running, retrying and failed work leads, and
//! queued and finished work folds.

use chrono::{DateTime, Utc};
use zeron_proto::extension_ui::{
    ExtensionUiState, WorkflowAction, WorkflowState, WorkflowTask, WorkflowTaskState,
};

/// How long a finished run stays up: long enough to see it end.
const LINGER_SECS: i64 = 20;
/// A run that failed and cannot be resumed stays longer, so its reason is
/// read.
const FAILED_LINGER_SECS: i64 = 120;
/// Rows shown per folded group before a "+N more" line.
pub(super) const GROUP_ROWS: usize = 12;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RunPhase {
    Running,
    /// Running, with at least one task waiting to retry.
    Retrying,
    Paused,
    Completed,
    Failed,
    Interrupted,
    Stopped,
}

impl RunPhase {
    fn of(status: &str, retrying: bool) -> Self {
        match status {
            "running" if retrying => Self::Retrying,
            "running" => Self::Running,
            "paused" => Self::Paused,
            "completed" => Self::Completed,
            "error" => Self::Failed,
            "interrupted" => Self::Interrupted,
            "aborted" => Self::Stopped,
            // A newer extension's status: treat it as still going.
            _ => Self::Running,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Running => "Running",
            Self::Retrying => "Retrying",
            Self::Paused => "Paused",
            Self::Completed => "Done",
            Self::Failed => "Failed",
            Self::Interrupted => "Interrupted",
            Self::Stopped => "Stopped",
        }
    }

    pub fn live(self) -> bool {
        matches!(self, Self::Running | Self::Retrying)
    }
}

/// One task row.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct TaskRow {
    pub id: String,
    pub label: String,
    pub state: WorkflowTaskState,
    /// Running since, for a live timer.
    pub started: Option<DateTime<Utc>>,
    /// What the row says at its right edge when it has no timer.
    pub trailing: Option<String>,
    /// Tooltip: the error, the dependencies, the model.
    pub detail: Option<String>,
}

impl TaskRow {
    fn of(task: &WorkflowTask, now: DateTime<Utc>) -> Self {
        let at = |ms: Option<u64>| ms.and_then(|ms| DateTime::from_timestamp_millis(ms as i64));
        let started = at(task.started_at).map(|t| t.min(now));
        let duration = match (at(task.started_at), at(task.ended_at)) {
            (Some(start), Some(end)) if end >= start => Some(short_duration(end - start)),
            _ => None,
        };
        let attempts =
            (task.max_attempts > 1).then(|| format!("{}/{}", task.attempt, task.max_attempts));
        let trailing = match task.state {
            WorkflowTaskState::Running => attempts
                .filter(|_| task.attempt > 1)
                .map(|a| format!("try {a}")),
            WorkflowTaskState::Retrying => Some(match at(task.next_attempt_at) {
                Some(next) if next > now => {
                    format!("retry in {}", short_duration(next - now))
                }
                _ => attempts.map_or_else(|| "retrying".into(), |a| format!("retry {a}")),
            }),
            WorkflowTaskState::Cached => Some(if task.reused_from.is_some() {
                "reused".into()
            } else {
                "cached".into()
            }),
            WorkflowTaskState::Failed => Some(
                task.error
                    .as_ref()
                    .map(|e| short_code(&e.code))
                    .unwrap_or_else(|| "failed".into()),
            ),
            WorkflowTaskState::Skipped => Some("skipped".into()),
            WorkflowTaskState::Aborted => Some("stopped".into()),
            WorkflowTaskState::Done => duration,
            WorkflowTaskState::Queued => (!task.depends_on.is_empty()).then(|| "waiting".into()),
            WorkflowTaskState::Other => Some("waiting".into()),
        };
        let mut detail = Vec::new();
        if let Some(error) = &task.error {
            let message = clean(&error.message);
            detail.push(
                if error.retriable && task.state == WorkflowTaskState::Retrying {
                    format!("Retrying after: {message}")
                } else {
                    message
                },
            );
        }
        if !task.depends_on.is_empty() {
            let deps: Vec<String> = task.depends_on.iter().map(|d| clean(d)).collect();
            detail.push(format!("After {}", deps.join(", ")));
        }
        if let Some(from) = task.reused_from {
            detail.push(format!("Reused from the earlier run (#{from})"));
        }
        if let Some(model) = task.model.as_deref().filter(|m| !m.is_empty()) {
            detail.push(clean(model));
        }
        Self {
            id: task.id.clone(),
            label: {
                let label = clean(&task.label);
                if label.is_empty() {
                    "agent".into()
                } else {
                    label
                }
            },
            state: task.state,
            started: if task.state == WorkflowTaskState::Running {
                started
            } else {
                None
            },
            trailing,
            detail: (!detail.is_empty()).then(|| detail.join("\n")),
        }
    }

    pub fn leads(&self) -> bool {
        matches!(
            self.state,
            WorkflowTaskState::Running
                | WorkflowTaskState::Retrying
                | WorkflowTaskState::Failed
                | WorkflowTaskState::Aborted
                | WorkflowTaskState::Other
        )
    }
}

/// Resume or Discard for a run that stopped early.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Recovery {
    /// What stopped it, in plain words.
    pub reason: String,
    pub resume: Option<WorkflowAction>,
    pub discard: Option<WorkflowAction>,
}

#[derive(Debug, Clone, PartialEq)]
pub(super) struct WorkflowView {
    pub run_id: String,
    pub name: String,
    pub goal: String,
    pub phase: RunPhase,
    /// The phase in progress, and its place: `("Verify", 2, 3)`.
    pub step: Option<(String, usize, usize)>,
    pub done: u32,
    pub total: u32,
    pub started: Option<DateTime<Utc>>,
    /// Running, retrying and failed tasks.
    pub lead: Vec<TaskRow>,
    pub queued: Vec<TaskRow>,
    /// Queued work the payload only counts.
    pub queued_more: u32,
    pub finished: Vec<TaskRow>,
    /// Rows the extension left out of the payload.
    pub truncated: u32,
    /// "Checkpoint 2 minutes ago · 5 done".
    pub checkpoint: Option<String>,
    /// "3 results reused from the last run".
    pub reuse: Option<String>,
    pub pause: Option<WorkflowAction>,
    pub stop: Option<WorkflowAction>,
    pub recovery: Option<Recovery>,
}

impl WorkflowView {
    pub fn of(ui: &ExtensionUiState, now: DateTime<Utc>) -> Option<Self> {
        Self::from_state(&WorkflowState::of(ui)?, now)
    }

    pub fn from_state(state: &WorkflowState, now: DateTime<Utc>) -> Option<Self> {
        let at = |ms: u64| DateTime::from_timestamp_millis(ms as i64).map(|t| t.min(now));
        let action = |id: &str| state.action(id).cloned();
        let Some(run) = &state.run else {
            // No run tracked: only a run that stopped and can be resumed.
            let latest = state.recovery.interrupted.first()?;
            let resume = action("resume").filter(|a| a.run_id == latest.run_id)?;
            return Some(Self {
                run_id: latest.run_id.clone(),
                name: clean_or(&latest.name, "Workflow"),
                goal: String::new(),
                phase: RunPhase::Interrupted,
                step: None,
                done: latest.done,
                total: latest.total,
                started: None,
                lead: Vec::new(),
                queued: Vec::new(),
                queued_more: 0,
                finished: Vec::new(),
                truncated: 0,
                checkpoint: at(latest.ended_at)
                    .map(|t| format!("Stopped {}", crate::pull_requests::relative_time(t, now))),
                reuse: None,
                pause: None,
                stop: None,
                recovery: Some(Recovery {
                    reason: reason_text("interrupted", latest.reason.as_deref()),
                    resume: Some(resume),
                    discard: action("discard").filter(|a| a.run_id == latest.run_id),
                }),
            });
        };

        let phase = RunPhase::of(&run.status, state.progress.retrying > 0);
        let ended = run.ended_at.and_then(at);
        if let Some(ended) = ended
            && !state.recovery.resumable
        {
            let linger = if phase == RunPhase::Failed {
                FAILED_LINGER_SECS
            } else {
                LINGER_SECS
            };
            if (now - ended).num_seconds() > linger {
                return None;
            }
        }

        let mut lead = Vec::new();
        let mut queued = Vec::new();
        let mut finished = Vec::new();
        for task in &state.tasks {
            let row = TaskRow::of(task, now);
            if row.leads() {
                lead.push(row);
            } else if row.state == WorkflowTaskState::Queued {
                queued.push(row);
            } else {
                finished.push(row);
            }
        }
        // Live work first, then failures; most recently finished first.
        lead.sort_by_key(|row| {
            !matches!(
                row.state,
                WorkflowTaskState::Running | WorkflowTaskState::Retrying
            )
        });
        finished.reverse();

        let step = run.phase.as_deref().map(|title| {
            let index = state
                .phases
                .iter()
                .position(|p| p.title == title)
                .map_or(0, |i| i + 1);
            (clean(title), index, state.phases.len())
        });
        let checkpoint = state.checkpoints.last().and_then(|c| {
            let when = crate::pull_requests::relative_time(at(c.at)?, now);
            Some(match c.kind.as_str() {
                "resume" => format!("Resumed {when}"),
                _ => format!("Checkpoint {when} · {} done", c.done_count),
            })
        });
        let reuse = match (state.recovery.reused, state.recovery.invalidated) {
            (0, 0) => None,
            (n, 0) => Some(format!(
                "{n} result{} reused from the earlier run",
                plural(n)
            )),
            (0, m) => Some(format!("{m} cached result{} no longer matched", plural(m))),
            (n, m) => Some(format!("{n} reused, {m} no longer matched")),
        };
        let recovery = match action("resume") {
            Some(resume) if ended.is_some() && state.recovery.resumable => Some(Recovery {
                reason: reason_text(&run.status, run.status_reason.as_deref()),
                resume: Some(resume),
                discard: action("discard"),
            }),
            _ => None,
        };
        let live = ended.is_none();
        let queued_more = state.progress.queued.saturating_sub(queued.len() as u32);
        Some(Self {
            run_id: run.id.clone(),
            name: clean_or(&run.name, "Workflow"),
            goal: clean(&run.goal),
            phase,
            step,
            done: state.progress.done,
            total: state.progress.total.max(state.progress.done),
            started: at(run.started_at),
            lead,
            queued,
            queued_more,
            finished,
            truncated: state.tasks_truncated,
            checkpoint,
            reuse,
            pause: if live { action("pause") } else { None },
            stop: if live { action("stop") } else { None },
            recovery,
        })
    }

    /// The section's seat: when the run started.
    pub fn since(&self) -> Option<DateTime<Utc>> {
        self.started
    }

    pub fn fraction(&self) -> f32 {
        if self.total == 0 {
            return 0.0;
        }
        (self.done as f32 / self.total as f32).clamp(0.0, 1.0)
    }

    /// Rows the section draws before its first measurement.
    pub fn estimated_rows(&self, queued_open: bool, finished_open: bool) -> usize {
        let groups = usize::from(!self.queued.is_empty() || self.queued_more > 0)
            + usize::from(!self.finished.is_empty());
        let open = |open: bool, rows: usize| if open { rows.min(GROUP_ROWS) } else { 0 };
        2 + self.lead.len()
            + groups
            + open(queued_open, self.queued.len())
            + open(finished_open, self.finished.len())
            + usize::from(self.recovery.is_some())
    }
}

fn reason_text(status: &str, reason: Option<&str>) -> String {
    let reason = reason.map(clean).filter(|r| !r.is_empty());
    match (status, reason.as_deref()) {
        ("interrupted", Some("shutdown")) => "Interrupted when Pi shut down".into(),
        ("interrupted", Some("crashed")) => "Interrupted: Pi stopped mid-run".into(),
        ("interrupted", _) => "Interrupted".into(),
        ("paused", Some(r)) => format!("Paused: {r}"),
        ("paused", None) => "Paused".into(),
        (_, Some(r)) => r.to_owned(),
        _ => "Stopped before it finished".into(),
    }
}

fn plural(n: u32) -> &'static str {
    if n == 1 { "" } else { "s" }
}

/// `AGENT_RATE_LIMIT` → `rate limit`.
fn short_code(code: &str) -> String {
    let code = code.strip_prefix("AGENT_").unwrap_or(code);
    clean(&code.replace('_', " ").to_lowercase())
}

fn short_duration(delta: chrono::TimeDelta) -> String {
    let secs = delta.num_seconds().max(0);
    match secs {
        0..60 => format!("{secs}s"),
        60..3_600 => format!("{}m {}s", secs / 60, secs % 60),
        _ => format!("{}h {}m", secs / 3_600, (secs % 3_600) / 60),
    }
}

/// Text from a script or a model: no control characters (ANSI included),
/// one line, bounded.
pub(super) fn clean(text: &str) -> String {
    let mut out = String::with_capacity(text.len().min(256));
    let mut chars = text.chars().peekable();
    let mut space = false;
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            // Skip an escape sequence: ESC [ params final.
            if chars.peek() == Some(&'[') {
                chars.next();
                for c in chars.by_ref() {
                    if ('@'..='~').contains(&c) {
                        break;
                    }
                }
            }
            continue;
        }
        if c.is_control() || c.is_whitespace() {
            space = !out.is_empty();
            continue;
        }
        if space {
            out.push(' ');
            space = false;
        }
        out.push(c);
        if out.chars().count() >= 300 {
            out.push('…');
            break;
        }
    }
    out
}

fn clean_or(text: &str, fallback: &str) -> String {
    let text = clean(text);
    if text.is_empty() {
        fallback.into()
    } else {
        text
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use zeron_proto::extension_ui::WORKFLOW_STATE_CHANNEL;

    const T0: i64 = 1_760_000_000_000;

    fn now() -> DateTime<Utc> {
        DateTime::from_timestamp_millis(T0 + 60_000).unwrap()
    }

    fn ui(payload: serde_json::Value) -> ExtensionUiState {
        let mut ui = ExtensionUiState::default();
        ui.channels.insert(WORKFLOW_STATE_CHANNEL.into(), payload);
        ui
    }

    fn running() -> serde_json::Value {
        json!({
            "v": 1,
            "updatedAt": T0 + 59_000,
            "run": {"id": "wf-1", "name": "review", "goal": "Review the \u{1b}[1mdiff\u{1b}[0m", "status": "running",
                    "startedAt": T0, "phase": "Verify"},
            "progress": {"total": 6, "done": 2, "running": 1, "retrying": 1, "failed": 1, "queued": 3, "cached": 1},
            "phases": [{"title": "Find", "state": "done"}, {"title": "Verify", "state": "active"}],
            "tasks": [
                {"id": "0", "label": "find", "state": "done", "attempt": 1, "maxAttempts": 2, "startedAt": T0, "endedAt": T0 + 12_000},
                {"id": "1", "label": "cached one", "state": "cached", "cached": true, "reusedFrom": 4},
                {"id": "2", "label": "verify a", "state": "failed", "error": {"code": "AGENT_SCHEMA_INVALID", "message": "bad json", "retriable": true}},
                {"id": "3", "label": "verify b", "state": "retrying", "attempt": 2, "maxAttempts": 3, "nextAttemptAt": T0 + 65_000,
                 "error": {"code": "AGENT_RATE_LIMIT", "message": "429", "retriable": true}},
                {"id": "4", "label": "verify c", "state": "running", "startedAt": T0 + 30_000},
                {"id": "node:join", "label": "join", "state": "queued", "dependsOn": ["verify b", "verify c"]}
            ],
            "checkpoints": [{"at": T0 + 12_000, "kind": "agent", "doneCount": 2}],
            "recovery": {"reused": 1, "actions": [
                {"id": "pause", "label": "Pause", "runId": "wf-1"},
                {"id": "stop", "label": "Stop", "runId": "wf-1", "confirm": true}]}
        })
    }

    #[test]
    fn a_running_run_leads_with_live_work_and_folds_the_rest() {
        let view = WorkflowView::of(&ui(running()), now()).expect("shown");
        assert_eq!(view.phase, RunPhase::Retrying);
        assert_eq!(
            view.goal, "Review the diff",
            "control sequences must not reach the panel"
        );
        assert_eq!(view.step, Some(("Verify".into(), 2, 2)));
        assert_eq!(
            view.lead
                .iter()
                .map(|r| r.label.as_str())
                .collect::<Vec<_>>(),
            ["verify b", "verify c", "verify a"],
            "running and retrying lead, then failures"
        );
        assert_eq!(view.lead[0].trailing.as_deref(), Some("retry in 5s"));
        assert_eq!(view.lead[2].trailing.as_deref(), Some("schema invalid"));
        assert_eq!(view.lead[2].detail.as_deref(), Some("bad json"));
        assert!(
            view.lead[1].started.is_some(),
            "a running task gets a timer"
        );
        assert_eq!(view.queued.len(), 1);
        assert_eq!(view.queued_more, 2);
        assert_eq!(
            view.queued[0].detail.as_deref(),
            Some("After verify b, verify c")
        );
        assert_eq!(
            view.finished
                .iter()
                .map(|r| r.trailing.clone())
                .collect::<Vec<_>>(),
            [Some("reused".to_string()), Some("12s".to_string())]
        );
        assert_eq!(
            view.reuse.as_deref(),
            Some("1 result reused from the earlier run")
        );
        assert_eq!(
            view.checkpoint.as_deref(),
            Some("Checkpoint just now · 2 done")
        );
        assert!(view.pause.is_some() && view.stop.is_some());
        assert!(view.recovery.is_none());
        assert!((view.fraction() - 2.0 / 6.0).abs() < 1e-6);
    }

    #[test]
    fn a_finished_run_lingers_then_goes() {
        let mut payload = running();
        payload["run"]["status"] = json!("completed");
        payload["run"]["endedAt"] = json!(T0 + 50_000);
        payload["recovery"]["actions"] = json!([]);
        let view = WorkflowView::of(&ui(payload.clone()), now()).expect("lingers");
        assert_eq!(view.phase, RunPhase::Completed);
        assert!(view.stop.is_none() && view.pause.is_none());
        let later = now() + chrono::TimeDelta::seconds(30);
        assert_eq!(WorkflowView::of(&ui(payload), later), None);
    }

    #[test]
    fn an_interrupted_run_offers_resume_until_it_is_dealt_with() {
        let mut payload = running();
        payload["run"]["status"] = json!("interrupted");
        payload["run"]["statusReason"] = json!("shutdown");
        payload["run"]["endedAt"] = json!(T0);
        payload["recovery"] = json!({"resumable": true, "actions": [
            {"id": "resume", "label": "Resume", "runId": "wf-1"},
            {"id": "discard", "label": "Discard", "runId": "wf-1", "confirm": true}]});
        let much_later = now() + chrono::TimeDelta::hours(3);
        let view = WorkflowView::of(&ui(payload), much_later).expect("stays while resumable");
        let recovery = view.recovery.expect("offers recovery");
        assert_eq!(recovery.reason, "Interrupted when Pi shut down");
        assert_eq!(recovery.resume.unwrap().run_id, "wf-1");
        assert!(recovery.discard.unwrap().confirm);
    }

    #[test]
    fn with_no_run_only_a_resumable_interrupted_run_shows() {
        let idle = json!({"v": 1, "run": null, "recovery": {"resumable": true,
            "interrupted": [{"runId": "wf-0", "name": "migrate", "status": "interrupted", "endedAt": T0, "done": 4, "total": 9, "reason": "crashed"}],
            "actions": [{"id": "resume", "label": "Resume", "runId": "wf-0"}, {"id": "discard", "label": "Discard", "runId": "wf-0"}]}});
        let view = WorkflowView::of(&ui(idle), now()).expect("offered");
        assert_eq!(view.phase, RunPhase::Interrupted);
        assert_eq!((view.done, view.total), (4, 9));
        assert_eq!(
            view.recovery.unwrap().reason,
            "Interrupted: Pi stopped mid-run"
        );
        assert_eq!(
            WorkflowView::of(&ui(json!({"v": 1, "run": null})), now()),
            None
        );
        assert_eq!(WorkflowView::of(&ExtensionUiState::default(), now()), None);
    }

    #[test]
    fn a_remote_clock_ahead_of_ours_never_starts_in_the_future() {
        let mut payload = running();
        payload["run"]["startedAt"] = json!(T0 + 3_600_000);
        let view = WorkflowView::of(&ui(payload), now()).unwrap();
        assert_eq!(view.since(), Some(now()));
    }

    #[test]
    fn unknown_states_and_missing_fields_still_render() {
        let payload = json!({"run": {"id": "x", "status": "someday"}, "tasks": [{"id": "1", "state": "teleporting"}]});
        let view = WorkflowView::of(&ui(payload), now()).unwrap();
        assert_eq!(view.phase, RunPhase::Running);
        assert_eq!(view.name, "Workflow");
        assert_eq!(view.lead[0].label, "agent");
        assert_eq!(view.lead[0].trailing.as_deref(), Some("waiting"));
    }

    #[test]
    fn clean_strips_controls_and_bounds_length() {
        assert_eq!(clean("a\nb\t\u{7}c"), "a b c");
        assert_eq!(clean("\u{1b}[31mred\u{1b}[0m"), "red");
        assert!(clean(&"x".repeat(1000)).chars().count() <= 301);
    }
}
