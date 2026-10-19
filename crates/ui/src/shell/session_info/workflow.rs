//! The session panel's Workflow section: the Pi workflow run under this
//! chat, from the workflows extension's `workflow:state` channel. Pure, so
//! it is unit-tested without a window; drawn by `Shell::render_workflow`.
//!
//! The section is up while there is something to act on or watch: a run in
//! flight, a paused or interrupted run that can be resumed, or a run that
//! just ended. A run that finished (done, stopped, or interrupted for good)
//! clears within five seconds; a failed one stays so its reason is read. Tasks group the way the
//! Sub-agents section does: running, retrying and failed work leads, and
//! queued and finished work folds.
//!
//! A v2 payload (Pi's workflows extension from `WORKFLOW_STATE_VERSION` 2)
//! groups its tasks by phase instead, the way Claude Code's workflow card
//! does: any number of phases in the order the script announced them (a
//! repeated title such as a second Verify is its own phase), each with its
//! progress dots and a table of its agents: model, tokens and time. A
//! failed grouped run stays up longer, so its table can be read.

use chrono::{DateTime, Utc};
use zeron_proto::extension_ui::{
    ExtensionUiState, WorkflowAction, WorkflowState, WorkflowTask, WorkflowTaskState,
};

/// How long a finished run stays up: long enough to see it end, and short
/// enough that it has folded away (`motion::MENU_OUT`) five seconds after.
const FINISHED_LINGER_MS: i64 = 4_800;
/// A failed run grouped by phase stays up long enough to read its table.
const GROUPED_LINGER_SECS: i64 = 600;
/// Dots a phase draws before they stand for more than one agent each.
pub(super) const PHASE_DOTS: usize = 24;
/// A run that failed and cannot be resumed stays longer, so its reason is
/// read.
const FAILED_LINGER_SECS: i64 = 120;

/// How long a run that ended in `phase` stays up after it ended.
fn linger(phase: RunPhase, grouped: bool) -> chrono::TimeDelta {
    match phase {
        RunPhase::Failed if grouped => chrono::TimeDelta::seconds(GROUPED_LINGER_SECS),
        RunPhase::Failed => chrono::TimeDelta::seconds(FAILED_LINGER_SECS),
        _ => chrono::TimeDelta::milliseconds(FINISHED_LINGER_MS),
    }
}
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
    /// The model as the payload names it (`provider/id`).
    pub model: Option<String>,
    /// Over every attempt.
    pub tokens: Option<u64>,
    /// How long a finished agent took; reused results have none.
    pub elapsed: Option<String>,
    /// Position in the payload's `phases`.
    pub phase_index: Option<usize>,
    /// A place for an agent the run has not named yet.
    pub placeholder: bool,
}

impl TaskRow {
    fn of(task: &WorkflowTask, now: DateTime<Utc>) -> Self {
        let at = |ms: Option<u64>| ms.and_then(|ms| DateTime::from_timestamp_millis(ms as i64));
        let started = at(task.started_at).map(|t| t.min(now));
        let duration = match (at(task.started_at), at(task.ended_at)) {
            (Some(start), Some(end)) if end >= start => Some(short_duration(end - start)),
            _ => None,
        };
        let elapsed = matches!(
            task.state,
            WorkflowTaskState::Done | WorkflowTaskState::Failed | WorkflowTaskState::Aborted
        )
        .then(|| duration.clone())
        .flatten();
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
            model: task.model.as_deref().map(clean).filter(|m| !m.is_empty()),
            tokens: task.tokens.filter(|&t| t > 0),
            elapsed,
            phase_index: task.phase_index.map(|i| i as usize),
            placeholder: false,
        }
    }

    /// A dim row for an agent the phase expects but cannot name yet.
    fn placeholder(phase: usize, n: u32) -> Self {
        Self {
            id: format!("pending-{phase}-{n}"),
            label: "agent".into(),
            state: WorkflowTaskState::Queued,
            started: None,
            trailing: None,
            detail: None,
            model: None,
            tokens: None,
            elapsed: None,
            phase_index: Some(phase),
            placeholder: true,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PhaseState {
    Pending,
    Active,
    Done,
    /// The phase a run ended in without completing.
    Stopped,
}

impl PhaseState {
    fn of(state: &str) -> Self {
        match state {
            "active" => Self::Active,
            "done" => Self::Done,
            "stopped" => Self::Stopped,
            _ => Self::Pending,
        }
    }
}

/// One dot of a phase's progress strip.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Dot {
    Done,
    Running,
    Retrying,
    Failed,
    Pending,
}

/// One phase of a grouped run: its header and its agents.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct PhaseRow {
    /// Position in the run, which also keys its fold.
    pub index: usize,
    pub title: String,
    pub state: PhaseState,
    pub done: u32,
    pub total: u32,
    pub dots: Vec<Dot>,
    pub tokens: u64,
    /// Running agents first, then in the order they started; placeholders
    /// last.
    pub rows: Vec<TaskRow>,
    /// Agents the payload counted but left out of its rows.
    pub hidden: u32,
}

impl PhaseRow {
    /// Open unless the user folded it: the phase in progress.
    pub fn opens_by_default(&self) -> bool {
        self.state == PhaseState::Active
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
    /// "3 results reused from the last run".
    pub reuse: Option<String>,
    pub pause: Option<WorkflowAction>,
    pub stop: Option<WorkflowAction>,
    pub recovery: Option<Recovery>,
    /// The payload groups by phase: draw `phases` instead of `lead`,
    /// `queued` and `finished`.
    pub grouped: bool,
    pub phases: Vec<PhaseRow>,
    /// Agents admitted, and tokens over all of them.
    pub agents: u32,
    pub tokens: u64,
    pub ended: Option<DateTime<Utc>>,
    /// When a run that ended for good stops being shown; the panel arms a
    /// redraw for it, since an idle chat repaints only once a minute.
    pub expires: Option<DateTime<Utc>>,
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
                reuse: None,
                pause: None,
                stop: None,
                recovery: Some(Recovery {
                    reason: reason_text("interrupted", latest.reason.as_deref()),
                    resume: Some(resume),
                    discard: action("discard").filter(|a| a.run_id == latest.run_id),
                }),
                grouped: false,
                phases: Vec::new(),
                agents: 0,
                tokens: 0,
                ended: None,
                expires: None,
            });
        };

        let phase = RunPhase::of(&run.status, state.progress.retrying > 0);
        let ended = run.ended_at.and_then(at);
        let expires = ended
            .filter(|_| !state.recovery.resumable)
            .map(|ended| ended + linger(phase, state.grouped()));
        if expires.is_some_and(|expires| now > expires) {
            return None;
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
            // A repeated title names more than one phase: the one in
            // progress is the last that started.
            let index = if state.grouped() {
                state
                    .phases
                    .iter()
                    .rposition(|p| p.title == title && p.state != "pending")
            } else {
                None
            }
            .or_else(|| state.phases.iter().position(|p| p.title == title))
            .map_or(0, |i| i + 1);
            (clean(title), index, state.phases.len())
        });
        let phases = if state.grouped() {
            phase_rows(state, now)
        } else {
            Vec::new()
        };
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
            reuse,
            pause: if live { action("pause") } else { None },
            stop: if live { action("stop") } else { None },
            recovery,
            grouped: state.grouped(),
            phases,
            agents: if state.grouped() {
                run.agents
            } else {
                state.tasks.len() as u32
            },
            // v2 counts every agent, rows the payload dropped included.
            tokens: if state.grouped() {
                run.tokens
            } else {
                state.tasks.iter().filter_map(|t| t.tokens).sum()
            },
            ended,
            expires,
        })
    }

    /// How long the run took once it ended, frozen: `33s`, `2m 5s`.
    pub fn took(&self) -> Option<String> {
        Some(short_duration(self.ended? - self.started?))
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
        if self.grouped {
            // Title, totals and "Phases", then each header (two lines) and
            // the table of each open phase.
            return 4
                + self
                    .phases
                    .iter()
                    .map(|p| {
                        2 + if p.opens_by_default() {
                            1 + p.rows.len().min(GROUP_ROWS)
                        } else {
                            0
                        }
                    })
                    .sum::<usize>()
                + usize::from(self.recovery.is_some());
        }
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

/// The phases of a v2 payload with their agents. Headers and dots come from
/// the phase counts, which cover every agent, so rows the payload left out
/// never change them.
fn phase_rows(state: &WorkflowState, now: DateTime<Utc>) -> Vec<PhaseRow> {
    let last = state.phases.len().saturating_sub(1);
    let mut rows: Vec<Vec<TaskRow>> = vec![Vec::new(); state.phases.len()];
    for task in &state.tasks {
        let row = TaskRow::of(task, now);
        let index = row
            .phase_index
            .filter(|&i| i < state.phases.len())
            .or_else(|| {
                let title = task.phase.as_deref()?;
                state.phases.iter().rposition(|p| p.title == title)
            })
            .unwrap_or(last);
        if let Some(list) = rows.get_mut(index) {
            list.push(row);
        }
    }
    let ended = state.run.as_ref().is_some_and(|r| r.ended_at.is_some());
    state
        .phases
        .iter()
        .zip(rows)
        .enumerate()
        .map(|(index, (phase, mut rows))| {
            let state_ = PhaseState::of(&phase.state);
            // Running work first; otherwise the order agents started in.
            rows.sort_by_key(|row| {
                !matches!(
                    row.state,
                    WorkflowTaskState::Running | WorkflowTaskState::Retrying
                )
            });
            let total = phase.total.max(phase.done).max(rows.len() as u32);
            let queued = if state_ == PhaseState::Active && !ended {
                phase.queued
            } else {
                0
            };
            let hidden = phase.agents.saturating_sub(rows.len() as u32);
            for n in 0..queued.min(GROUP_ROWS as u32) {
                rows.push(TaskRow::placeholder(index, n));
            }
            let total = total.max(phase.agents + queued);
            PhaseRow {
                index,
                title: clean_or(&phase.title, "Phase"),
                state: state_,
                done: phase.done,
                total,
                dots: dots(phase, total),
                tokens: phase.tokens,
                rows,
                hidden,
            }
        })
        .collect()
}

/// The strip under a phase's title: done, then running, retrying, failed,
/// then what is still to come. Past [`PHASE_DOTS`] agents a dot stands for
/// several, rounded so every kind present keeps at least one.
fn dots(phase: &zeron_proto::extension_ui::WorkflowPhase, total: u32) -> Vec<Dot> {
    let counted = [
        (Dot::Done, phase.done),
        (Dot::Running, phase.running),
        (Dot::Retrying, phase.retrying),
        (Dot::Failed, phase.failed + phase.skipped),
    ];
    let known: u32 = counted.iter().map(|(_, n)| n).sum();
    let pending = total.saturating_sub(known);
    let all = counted
        .into_iter()
        .chain(std::iter::once((Dot::Pending, pending)));
    let total = (known + pending) as usize;
    if total <= PHASE_DOTS {
        return all
            .flat_map(|(dot, n)| std::iter::repeat_n(dot, n as usize))
            .collect();
    }
    let mut out = Vec::with_capacity(PHASE_DOTS);
    for (dot, n) in all {
        if n == 0 {
            continue;
        }
        let share = ((n as usize * PHASE_DOTS) / total).max(1);
        out.extend(std::iter::repeat_n(dot, share));
    }
    // Rounding may overshoot: trim from the kind with the most dots.
    while out.len() > PHASE_DOTS {
        let most = [
            Dot::Done,
            Dot::Pending,
            Dot::Running,
            Dot::Failed,
            Dot::Retrying,
        ]
        .into_iter()
        .max_by_key(|d| out.iter().filter(|x| *x == d).count())
        .unwrap_or(Dot::Pending);
        if let Some(i) = out.iter().rposition(|d| *d == most) {
            out.remove(i);
        }
    }
    out
}

/// `941`, `86.2k`, `170.8k`, `1.5M`: Pi's `formatTokens`.
pub(super) fn compact_tokens(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}k", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}

/// A model id short enough for the table: `anthropic/claude-sonnet-5-5` →
/// `Sonnet 5.5`; anything else keeps the part after its last `/`.
pub(super) fn short_model(spec: &str) -> String {
    let id = spec.rsplit('/').next().unwrap_or(spec);
    let id = id.split(['@', ':']).next().unwrap_or(id);
    if let Some(rest) = id.strip_prefix("claude-") {
        let mut words = rest.split('-');
        if let Some(family) = words.next() {
            let version: Vec<&str> = words
                .take_while(|w| w.chars().all(|c| c.is_ascii_digit()) && w.len() <= 2)
                .collect();
            let mut name = family[..1].to_uppercase() + &family[1..];
            if !version.is_empty() {
                name.push(' ');
                name.push_str(&version.join("."));
            }
            return name;
        }
    }
    id.to_owned()
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
        assert!(view.pause.is_some() && view.stop.is_some());
        assert!(view.recovery.is_none());
        assert!((view.fraction() - 2.0 / 6.0).abs() < 1e-6);
    }

    /// Every frame a real run published, then its resume: Pi on a Linux box,
    /// the workflows extension, DeepSeek subagents, and one agent on a model
    /// that does not exist so it retries, fails, and its dependent skips.
    #[test]
    fn a_real_run_and_its_resume_render_frame_by_frame() {
        replay_a_real_run(include_str!(
            "../../../tests/fixtures/workflow-live-run.json"
        ));
    }

    /// The same run captured again on Pi 1.2.0.
    #[test]
    fn a_real_run_on_pi_1_2_renders_frame_by_frame() {
        replay_a_real_run(include_str!(
            "../../../tests/fixtures/workflow-live-run-pi-1.2.json"
        ));
    }

    fn replay_a_real_run(fixture: &str) {
        let fixture: serde_json::Value = serde_json::from_str(fixture).unwrap();
        let view_at = |frame: &serde_json::Value| {
            let at = DateTime::from_timestamp_millis(frame["updatedAt"].as_i64().unwrap()).unwrap();
            WorkflowView::of(&ui(frame.clone()), at).expect("every frame of a live run shows")
        };
        let views: Vec<WorkflowView> = fixture["run"]
            .as_array()
            .unwrap()
            .iter()
            .map(view_at)
            .collect();
        assert!(
            views.iter().any(|v| v.phase == RunPhase::Retrying
                && v.lead
                    .iter()
                    .any(|t| t.label == "pick a fruit" && t.state == WorkflowTaskState::Retrying)),
            "the failing agent was seen waiting to retry"
        );
        assert!(
            views.iter().any(|v| v
                .lead
                .iter()
                .filter(|t| t.state == WorkflowTaskState::Running)
                .count()
                >= 2),
            "the parallel agents were seen running together"
        );
        let last = views.last().unwrap();
        assert_eq!(last.phase, RunPhase::Completed);
        assert_eq!((last.done, last.total), (4, 6));
        assert_eq!(last.lead.len(), 1);
        assert_eq!(last.lead[0].label, "pick a fruit");
        assert_eq!(last.lead[0].state, WorkflowTaskState::Failed);
        assert!(
            last.finished
                .iter()
                .any(|t| t.label == "salad" && t.state == WorkflowTaskState::Skipped)
        );
        assert!(
            last.recovery
                .as_ref()
                .and_then(|r| r.resume.as_ref())
                .is_some(),
            "a run with a failure offers resume"
        );

        let resumed: Vec<WorkflowView> = fixture["resume"]
            .as_array()
            .unwrap()
            .iter()
            .map(view_at)
            .collect();
        let last = resumed.last().unwrap();
        assert_eq!(
            last.finished
                .iter()
                .filter(|t| t.state == WorkflowTaskState::Cached)
                .count(),
            4,
            "finished work was reused, not run again"
        );
        assert_eq!(last.lead.len(), 1, "only the failed agent ran again");
    }

    #[test]
    fn a_finished_run_clears_within_five_seconds() {
        let mut payload = running();
        payload["run"]["status"] = json!("completed");
        payload["run"]["endedAt"] = json!(T0 + 50_000);
        payload["recovery"]["actions"] = json!([]);
        let ended = DateTime::from_timestamp_millis(T0 + 50_000).unwrap();
        let at = |ms: i64| ended + chrono::TimeDelta::milliseconds(ms);
        let view = WorkflowView::of(&ui(payload.clone()), at(1_000)).expect("lingers");
        assert_eq!(view.phase, RunPhase::Completed);
        assert!(view.stop.is_none() && view.pause.is_none());
        assert_eq!(view.expires, Some(at(FINISHED_LINGER_MS)));
        assert!(WorkflowView::of(&ui(payload.clone()), at(FINISHED_LINGER_MS)).is_some());
        assert_eq!(WorkflowView::of(&ui(payload.clone()), at(5_000)), None);

        // Stopped by hand is finished too.
        payload["run"]["status"] = json!("aborted");
        assert_eq!(WorkflowView::of(&ui(payload), at(5_000)), None);
    }

    #[test]
    fn a_failed_run_stays_so_its_reason_is_read() {
        let mut payload = running();
        payload["run"]["status"] = json!("error");
        payload["run"]["endedAt"] = json!(T0 + 50_000);
        payload["recovery"]["actions"] = json!([]);
        let ended = DateTime::from_timestamp_millis(T0 + 50_000).unwrap();
        let at = |secs: i64| ended + chrono::TimeDelta::seconds(secs);
        let view = WorkflowView::of(&ui(payload.clone()), at(60)).expect("still shown");
        assert_eq!(view.phase, RunPhase::Failed);
        assert_eq!(view.expires, Some(at(FAILED_LINGER_SECS)));
        assert_eq!(WorkflowView::of(&ui(payload), at(FAILED_LINGER_SECS + 1)), None);
    }

    #[test]
    fn a_live_run_never_expires() {
        let view = WorkflowView::of(&ui(running()), now()).expect("shown");
        assert_eq!(view.expires, None);
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

    /// A v2 run as Pi's workflows extension publishes it: Trace, Implement,
    /// then Verify twice, phase counts that cover rows the payload dropped.
    fn grouped() -> serde_json::Value {
        json!({
            "v": 2,
            "updatedAt": T0 + 59_000,
            "run": {"id": "wf-2", "name": "capability-map-trace", "goal": "Trace prompts", "status": "running",
                    "startedAt": T0, "phase": "Verify", "agents": 9, "tokens": 170_800},
            "progress": {"total": 10, "done": 6, "running": 2, "queued": 1},
            "phases": [
                {"title": "Trace", "state": "done", "done": 4, "total": 4, "agents": 4, "tokens": 120_000,
                 "startedAt": T0, "endedAt": T0 + 20_000},
                {"title": "Implement", "state": "done", "done": 2, "total": 2, "agents": 2, "tokens": 40_000},
                {"title": "Verify", "state": "done", "done": 0, "total": 0},
                {"title": "Verify", "state": "active", "done": 0, "total": 4, "agents": 2, "running": 2,
                 "queued": 2, "tokens": 10_800},
                {"title": "Report", "state": "pending"}
            ],
            "tasks": [
                {"id": "0", "label": "zeron-launch", "state": "done", "phaseIndex": 0, "model": "anthropic/claude-sonnet-5-5",
                 "tokens": 86_200, "startedAt": T0, "endedAt": T0 + 33_000},
                {"id": "1", "label": "pi-prompt", "state": "done", "phaseIndex": 0, "model": "anthropic/claude-sonnet-5-5",
                 "tokens": 75_600, "startedAt": T0, "endedAt": T0 + 33_000},
                {"id": "6", "label": "check a", "state": "running", "phaseIndex": 3, "model": "openrouter/deepseek-v4.1-flash",
                 "tokens": 4_000, "startedAt": T0 + 50_000},
                {"id": "7", "label": "check b", "state": "running", "phase": "Verify", "tokens": 6_800, "startedAt": T0 + 51_000},
                {"id": "c", "label": "reused one", "state": "cached", "phaseIndex": 1, "reusedFrom": 3,
                 "startedAt": T0 + 22_000, "endedAt": T0 + 22_000}
            ],
            "tasksTruncated": 4,
            "recovery": {"actions": [{"id": "stop", "label": "Stop", "runId": "wf-2", "confirm": true}]}
        })
    }

    #[test]
    fn a_grouped_run_lists_every_phase_with_its_own_agents() {
        let view = WorkflowView::of(&ui(grouped()), now()).expect("shown");
        assert!(view.grouped);
        assert_eq!((view.agents, view.tokens), (9, 170_800));
        assert_eq!(
            view.phases
                .iter()
                .map(|p| (p.title.as_str(), p.state, p.done, p.total))
                .collect::<Vec<_>>(),
            [
                ("Trace", PhaseState::Done, 4, 4),
                ("Implement", PhaseState::Done, 2, 2),
                ("Verify", PhaseState::Done, 0, 0),
                ("Verify", PhaseState::Active, 0, 4),
                ("Report", PhaseState::Pending, 0, 0),
            ],
            "a repeated title is its own phase, in announce order"
        );
        assert_eq!(
            view.step,
            Some(("Verify".into(), 4, 5)),
            "the step is the Verify in progress, not the first one"
        );
        let trace = &view.phases[0];
        assert_eq!(trace.rows.len(), 2);
        assert_eq!(trace.hidden, 2, "the counts cover rows the payload dropped");
        assert_eq!(
            trace.rows[0].model.as_deref().map(short_model).as_deref(),
            Some("Sonnet 5.5")
        );
        assert_eq!(trace.rows[0].tokens, Some(86_200));
        assert_eq!(trace.rows[0].elapsed.as_deref(), Some("33s"));
        assert_eq!(
            view.phases[1].rows[0].elapsed, None,
            "a reused result took no time"
        );
        let verify = &view.phases[3];
        assert!(verify.opens_by_default());
        assert!(!view.phases[0].opens_by_default());
        assert_eq!(
            verify
                .rows
                .iter()
                .map(|r| (r.label.as_str(), r.placeholder))
                .collect::<Vec<_>>(),
            [
                ("check a", false),
                ("check b", false),
                ("agent", true),
                ("agent", true)
            ],
            "a task with only a title joins the last phase of that name; queued agents are placeholders"
        );
        assert!(
            verify.rows[0].started.is_some(),
            "a running agent's time ticks"
        );
        assert_eq!(
            verify.dots,
            [Dot::Running, Dot::Running, Dot::Pending, Dot::Pending]
        );
        assert_eq!(trace.dots, [Dot::Done; 4]);
        assert!(view.stop.is_some());
        assert_eq!(view.took(), None, "a live run has no frozen time");
    }

    #[test]
    fn a_finished_grouped_run_freezes_its_time_then_clears() {
        let mut payload = grouped();
        payload["run"]["status"] = json!("completed");
        payload["run"]["endedAt"] = json!(T0 + 33_000);
        payload["phases"][3]["state"] = json!("done");
        let at = |secs: i64| DateTime::from_timestamp_millis(T0 + 33_000 + secs * 1_000).unwrap();
        let view = WorkflowView::of(&ui(payload.clone()), at(4)).expect("shown");
        assert_eq!(view.took().as_deref(), Some("33s"));
        assert!(view.stop.is_none());
        assert!(
            view.phases.iter().all(|p| !p.opens_by_default()),
            "an ended run opens no phase by itself"
        );
        assert!(
            view.phases[3].rows.iter().all(|r| !r.placeholder),
            "an ended run expects no more agents"
        );
        assert!(WorkflowView::of(&ui(payload.clone()), at(5)).is_none());

        // A failed grouped run keeps its table up to be read.
        payload["run"]["status"] = json!("error");
        assert!(WorkflowView::of(&ui(payload.clone()), at(300)).is_some());
        assert!(WorkflowView::of(&ui(payload), at(601)).is_none());
    }

    #[test]
    fn a_v1_payload_keeps_the_lead_queued_finished_layout() {
        let view = WorkflowView::of(&ui(running()), now()).expect("shown");
        assert!(!view.grouped);
        assert!(view.phases.is_empty());
        assert!(!view.lead.is_empty());
    }

    #[test]
    fn many_agents_share_dots_and_every_kind_keeps_one() {
        let phase = zeron_proto::extension_ui::WorkflowPhase {
            done: 90,
            running: 3,
            failed: 1,
            total: 120,
            ..Default::default()
        };
        let dots = dots(&phase, 120);
        assert!(dots.len() <= PHASE_DOTS);
        for kind in [Dot::Done, Dot::Running, Dot::Failed, Dot::Pending] {
            assert!(dots.contains(&kind), "{kind:?} keeps a dot");
        }
        assert!(dots.iter().filter(|d| **d == Dot::Done).count() > 10);
    }

    #[test]
    fn tokens_and_models_read_like_claude_codes_table() {
        assert_eq!(compact_tokens(941), "941");
        assert_eq!(compact_tokens(1_000), "1.0k");
        assert_eq!(compact_tokens(86_200), "86.2k");
        assert_eq!(compact_tokens(170_800), "170.8k");
        assert_eq!(compact_tokens(1_500_000), "1.5M");
        assert_eq!(short_model("anthropic/claude-sonnet-5-5"), "Sonnet 5.5");
        assert_eq!(short_model("claude-opus-5-5"), "Opus 5.5");
        assert_eq!(short_model("claude-haiku-5-5-20260101"), "Haiku 5.5");
        assert_eq!(
            short_model("clinepass/cline-pass/deepseek-v4.1-flash"),
            "deepseek-v4.1-flash"
        );
        assert_eq!(short_model("gpt-5.5"), "gpt-5.5");
    }

    /// Every frame Pi's workflows extension published for a run of Trace (4
    /// agents at once), Implement (one agent retried after a 429), Verify,
    /// Verify again and Report, driven through its real state code.
    #[test]
    fn a_dynamic_run_renders_phase_by_phase_frame_by_frame() {
        let views = replay_grouped(include_str!(
            "../../../tests/fixtures/workflow-dynamic-run.json"
        ));
        let last = views.last().unwrap();
        assert_eq!(last.phase, RunPhase::Completed);
        assert_eq!((last.agents, last.tokens), (12, 445_085));
        assert_eq!(
            last.phases
                .iter()
                .map(|p| (p.title.as_str(), p.done, p.total))
                .collect::<Vec<_>>(),
            [
                ("Trace", 4, 4),
                ("Implement", 2, 2),
                ("Verify", 3, 3),
                ("Verify", 2, 2),
                ("Report", 1, 1)
            ]
        );
        assert!(last.took().is_some());
        // Mid-run the retried agent's tokens only grow, and a phase waiting
        // on a slot shows a placeholder for the agent to come.
        let retried: Vec<u64> = views
            .iter()
            .filter_map(|v| {
                v.phases
                    .iter()
                    .flat_map(|p| &p.rows)
                    .find(|r| r.label == "implement:phase-view")?
                    .tokens
            })
            .collect();
        assert!(retried.windows(2).all(|w| w[0] <= w[1]), "{retried:?}");
        assert!(
            views.iter().any(|v| v
                .phases
                .iter()
                .any(|p| p.rows.iter().any(|r| r.placeholder))),
            "a queued agent shows as a placeholder"
        );
    }

    /// The same script stopped mid-Implement: that phase reads stopped, its
    /// agents failed, and nothing past it opens or waits.
    #[test]
    fn a_stopped_dynamic_run_ends_where_it_stopped() {
        let views = replay_grouped(include_str!(
            "../../../tests/fixtures/workflow-dynamic-stopped.json"
        ));
        let last = views.last().unwrap();
        assert_eq!(last.phase, RunPhase::Stopped);
        assert_eq!(last.phases[1].state, PhaseState::Stopped);
        assert!(
            last.phases[2..]
                .iter()
                .all(|p| p.state == PhaseState::Pending)
        );
        assert!(last.phases.iter().all(|p| !p.opens_by_default()));
        assert!(last.stop.is_none());
    }

    /// Checks every frame: it shows, each phase's rows are its own, and no
    /// phase ever claims more done than total.
    #[test]
    fn a_real_pi_run_replays_round_by_round() {
        // Every payload a real Pi published for a three-round script.
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../../tests/fixtures/workflow-live-pi-rounds.json"
        ))
        .unwrap();
        let views: Vec<WorkflowView> = fixture["run"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|frame| !frame["run"].is_null())
            .map(|frame| {
                let at =
                    DateTime::from_timestamp_millis(frame["updatedAt"].as_i64().unwrap()).unwrap();
                WorkflowView::of(&ui(frame.clone()), at).expect("a run shows")
            })
            .collect();
        assert!(views.iter().all(|v| v.grouped));
        // The rounds open one at a time, in order.
        let mut active: Vec<&str> = views
            .iter()
            .filter_map(|v| v.phases.iter().find(|p| p.state == PhaseState::Active))
            .map(|p| p.title.as_str())
            .collect();
        active.dedup();
        assert_eq!(active, ["Trace", "Implement", "Verify"]);
        // Trace's three agents ran at once.
        assert!(views.iter().any(|v| {
            v.phases[0]
                .rows
                .iter()
                .filter(|r| r.state == WorkflowTaskState::Running)
                .count()
                == 3
        }));
        let last = views.last().unwrap();
        assert_eq!(last.phase, RunPhase::Completed);
        assert_eq!((last.agents, last.tokens), (6, 54));
        assert_eq!(
            last.phases
                .iter()
                .map(|p| (p.title.as_str(), p.state, p.done, p.total, p.tokens))
                .collect::<Vec<_>>(),
            [
                ("Trace", PhaseState::Done, 3, 3, 27),
                ("Implement", PhaseState::Done, 1, 1, 9),
                ("Verify", PhaseState::Done, 2, 2, 18)
            ]
        );
        // Every agent row has its model, tokens and time.
        let rows: Vec<&TaskRow> = last.phases.iter().flat_map(|p| &p.rows).collect();
        assert_eq!(rows.len(), 6);
        for row in rows {
            assert_eq!(
                row.model.as_deref().map(short_model).as_deref(),
                Some("Haiku 5.5"),
                "{row:?}"
            );
            assert_eq!(row.tokens, Some(9));
            assert!(row.elapsed.is_some(), "{row:?}");
        }
        assert!(last.took().is_some());
    }

    fn replay_grouped(fixture: &str) -> Vec<WorkflowView> {
        let fixture: serde_json::Value = serde_json::from_str(fixture).unwrap();
        fixture["run"]
            .as_array()
            .unwrap()
            .iter()
            .map(|frame| {
                let at =
                    DateTime::from_timestamp_millis(frame["updatedAt"].as_i64().unwrap()).unwrap();
                let view = WorkflowView::of(&ui(frame.clone()), at).expect("every frame shows");
                assert!(view.grouped);
                assert_eq!(view.phases.len(), frame["phases"].as_array().unwrap().len());
                for phase in &view.phases {
                    assert!(phase.done <= phase.total, "{phase:?}");
                    assert!(phase.dots.len() <= PHASE_DOTS);
                    for row in &phase.rows {
                        assert_eq!(row.phase_index.unwrap_or(phase.index), phase.index);
                    }
                }
                let active = view
                    .phases
                    .iter()
                    .filter(|p| p.state == PhaseState::Active)
                    .count();
                assert!(active <= 1, "one phase at a time");
                view
            })
            .collect()
    }
}
