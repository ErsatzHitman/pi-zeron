//! Live extension UI: what a harness's extensions draw outside the transcript,
//! held by the host engine per chat and streamed to UIs.
//!
//! For Pi this carries PIUI (the user's extensions publishing their UI as data
//! over `pi --mode rpc`, see `crates/harness/src/pi/PIUI.md`) plus Pi's plain
//! extension UI requests (`notify`, `setStatus`, `setWidget`,
//! `set_editor_text`). It is live state, not history: it never enters the chat
//! doc, and it is empty whenever the harness process is not running.
//!
//! Elements travel raw ([`serde_json::Value`]) so an engine never drops fields
//! a newer UI understands; [`PiUiElement`] is the typed view renderers parse.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// pi-goal's state channel: `{active, status, text, detail, startedAt}`;
/// `status` is `active`, `waiting`, `paused`, `blocked` or `budget_limited`.
///
/// `waiting` means the goal loop is parked on an external event (`goal_wait`,
/// a backoff, an idle check-in) and `detail` says what it waits on. It stays
/// `waiting` while the session answers a user message typed during that wait:
/// that turn is an ordinary manual run that does not resume the goal, which
/// goes on when the wait's deadline elapses, a check-in fires or `/goal resume`
/// is asked for. So `waiting` is neither idle nor awaiting the user: a reader
/// counting live work keeps counting it, while one showing state keeps it at
/// `Waiting` with `detail` verbatim rather than calling it running or asking
/// the user anything.
pub const GOAL_CHANNEL: &str = "pi-goal:status";

/// Zeron's own compaction status for a Pi chat ([`CompactStatus`]).
/// Published by the harness, not an extension, and held beside the PIUI
/// channels; absent when the chat's harness has no such notion or its
/// process is not running.
pub const COMPACT_CHANNEL: &str = "zeron:compact";

/// The [`COMPACT_CHANNEL`] payload.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct CompactStatus {
    /// Pi's auto-compaction flag (`get_state.autoCompactionEnabled`).
    pub enabled: bool,
    /// Context size above which Pi starts compacting; `None` when unknown.
    pub threshold_tokens: Option<u64>,
    pub compacting: bool,
    /// `manual`, `threshold` or `overflow`.
    pub reason: Option<String>,
    /// Host clock, unix ms, when the running compaction began.
    pub started_at_ms: Option<u64>,
    /// Context size the last successful compaction left; cleared by the
    /// next reply.
    pub tokens_after: Option<u64>,
    /// The last compaction's error; cleared by the next compaction.
    pub error: Option<String>,
}

impl CompactStatus {
    pub fn of(ui: &ExtensionUiState) -> Option<Self> {
        Self::deserialize(ui.channels.get(COMPACT_CHANNEL)?).ok()
    }
}

/// The Pi context-window extension's breakdown of the context window: what
/// the provider measured, and how the model's view splits into tools, the
/// system prompt, skills, MCP server instructions and messages (estimated,
/// chars/4), with deferred tools beside it and where Pi auto-compacts.
/// Whole-state per payload; republished after every Pi session reset.
pub const CONTEXT_WINDOW_CHANNEL: &str = "context:window";

/// The [`CONTEXT_WINDOW_CHANNEL`] payload. Every field defaults.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ContextBreakdown {
    pub v: u32,
    /// Increments on each publish (the extension dedupes unchanged ones).
    pub seq: u64,
    pub model: ContextModel,
    pub window: u64,
    pub measured: ContextMeasured,
    /// Estimated tokens added since the provider last measured.
    pub trailing_tokens: u64,
    pub categories: ContextCategories,
    pub deferred: ContextDeferred,
    pub autocompact: Option<ContextAutocompact>,
    pub mcp: ContextMcp,
    pub estimate: ContextEstimate,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ContextModel {
    pub provider: String,
    pub id: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ContextMeasured {
    /// `None` right after a compaction, until the next reply.
    pub tokens: Option<u64>,
    /// `provider`, `compaction` or `estimate`.
    pub source: String,
}

/// One row of the breakdown: tokens, and how many items when they count.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ContextCategory {
    pub tokens: u64,
    pub count: Option<u32>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ContextCategories {
    pub system_tools: ContextCategory,
    pub mcp_tools: ContextCategory,
    pub system_prompt: ContextCategory,
    pub skills: ContextCategory,
    pub mcp_instructions: ContextCategory,
    pub messages: ContextCategory,
    /// Prompt text no other row claims (a prompt an extension forced).
    pub other: ContextCategory,
}

/// Tools the model is not sent until it asks for them.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ContextDeferred {
    pub system_tools: ContextCategory,
    pub mcp_tools: ContextCategory,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ContextAutocompact {
    pub enabled: bool,
    /// Context size at which Pi compacts.
    pub at_tokens: u64,
    /// The window above that point.
    pub buffer_tokens: u64,
    /// `native` or `pi-compact-pct`.
    pub source: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ContextMcp {
    /// MCP tools registered, deferred ones included.
    pub total: u32,
    pub servers: Vec<ContextMcpServer>,
    /// The largest tools only.
    pub top: Vec<ContextMcpTool>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ContextMcpServer {
    pub name: String,
    pub count: u32,
    pub tokens: u64,
    /// Of `count`, how many the model is sent.
    pub declared: u32,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ContextMcpTool {
    pub name: String,
    pub server: String,
    pub tokens: u64,
    pub declared: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ContextEstimate {
    pub method: String,
    pub scale: f64,
    pub calibrated: bool,
}

impl ContextBreakdown {
    /// The selected chat's breakdown; `None` without one or with no window.
    pub fn of(ui: &ExtensionUiState) -> Option<Self> {
        Self::deserialize(ui.channels.get(CONTEXT_WINDOW_CHANNEL)?)
            .ok()
            .filter(|b| b.window > 0)
    }

    /// The context in use as Pi counts it: measured, plus what was added
    /// since.
    pub fn used(&self) -> Option<u64> {
        Some(self.measured.tokens? + self.trailing_tokens)
    }
}

/// The Pi workflows extension's live run state (its `state.ts`): one run's
/// tasks, attempts, errors, cache hits, checkpoints and recovery actions,
/// plus runs that stopped and can be resumed. Whole-state per payload.
pub const WORKFLOW_STATE_CHANNEL: &str = "workflow:state";

/// The element id workflow actions are sent under; the extension routes any
/// `workflows:*` action to itself, so no element needs to exist.
pub const WORKFLOW_STATE_ELEMENT: &str = "workflows:state";

/// The [`WORKFLOW_STATE_CHANNEL`] payload. Every field defaults, so an older
/// or newer extension parses: unknown fields are dropped and unknown task
/// states read as [`WorkflowTaskState::Other`].
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct WorkflowState {
    /// Payload version: v2 groups by phase, v3 adds [`WorkflowRun::gate`].
    /// Additive, so a reader that knows only v2 parses a v3 payload.
    pub v: u32,
    /// Extension clock, unix ms; strictly increasing per Pi process.
    pub updated_at: u64,
    /// The tracked run; `None` when only interrupted runs are listed.
    pub run: Option<WorkflowRun>,
    pub progress: WorkflowProgress,
    pub phases: Vec<WorkflowPhase>,
    pub tasks: Vec<WorkflowTask>,
    /// Tasks the payload left out (its counts still cover them).
    pub tasks_truncated: u32,
    pub checkpoints: Vec<WorkflowCheckpoint>,
    pub recovery: WorkflowRecovery,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct WorkflowRun {
    pub id: String,
    pub name: String,
    pub goal: String,
    /// `running`, `paused`, `completed`, `error`, `aborted` or `interrupted`.
    pub status: String,
    pub status_reason: Option<String>,
    pub started_at: u64,
    pub ended_at: Option<u64>,
    pub dry_run: bool,
    pub resumed: bool,
    pub phase: Option<String>,
    /// Agent calls admitted so far, cached ones included (v2).
    pub agents: u32,
    /// Tokens over every agent, retries included (v2).
    pub tokens: u64,
    /// The host resource gate holding admission, only while it waits (v3).
    pub gate: Option<WorkflowGate>,
}

/// Why the run's resource gate is holding admission (v3).
///
/// `reason` is the gate's own verdict, so it names the metric and both the
/// measured and the threshold value; the panel prints it as it is rather than
/// composing a second explanation that could disagree with the runtime.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct WorkflowGate {
    /// `waiting` or `cleared`; an unknown value passes through.
    pub state: String,
    /// The gate's verdict: which reading is short, and by how much.
    pub reason: String,
    /// Admissions blocked on the gate right now, this one included.
    pub waiting: u32,
    /// Extension clock, unix ms, the wait began (the first poll, across
    /// repeated ones).
    pub since: Option<u64>,
    pub metrics: Option<WorkflowGateMetrics>,
}

/// The gate's readings in the units a panel prints: GB and percent, already
/// rounded by the extension.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct WorkflowGateMetrics {
    pub mem_available_gb: f64,
    pub swap_used_pct: f64,
    pub load1: f64,
    pub disk_free_gb: f64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct WorkflowProgress {
    pub total: u32,
    pub done: u32,
    pub running: u32,
    pub queued: u32,
    pub failed: u32,
    pub cached: u32,
    pub retrying: u32,
    pub skipped: u32,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct WorkflowPhase {
    pub title: String,
    /// `pending`, `active`, `done` or `stopped` (the phase a run ended in
    /// without completing).
    pub state: String,
    pub done: u32,
    pub total: u32,
    /// From here on v2: one entry per announced phase, a repeated title
    /// included, so the array position is the phase's index.
    pub started_at: Option<u64>,
    pub ended_at: Option<u64>,
    pub tokens: u64,
    pub agents: u32,
    pub running: u32,
    pub retrying: u32,
    pub failed: u32,
    pub skipped: u32,
    /// Agents known to be coming that have no name yet.
    pub queued: u32,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WorkflowTaskState {
    #[default]
    Queued,
    Running,
    Retrying,
    Done,
    Cached,
    Failed,
    Skipped,
    Aborted,
    #[serde(other)]
    Other,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct WorkflowTask {
    pub id: String,
    pub label: String,
    pub phase: Option<String>,
    pub state: WorkflowTaskState,
    pub attempt: u32,
    pub max_attempts: u32,
    pub depends_on: Vec<String>,
    /// The dag node the task ran in.
    pub node: Option<String>,
    pub cached: bool,
    /// Original position of a result reused by content on resume.
    pub reused_from: Option<u32>,
    pub started_at: Option<u64>,
    pub ended_at: Option<u64>,
    /// When a retrying task's next attempt starts.
    pub next_attempt_at: Option<u64>,
    pub error: Option<WorkflowTaskError>,
    pub model: Option<String>,
    /// Over every attempt (v2).
    pub tokens: Option<u64>,
    /// Position in `phases` (v2).
    pub phase_index: Option<u32>,
    pub tools: Option<u32>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct WorkflowTaskError {
    pub code: String,
    pub message: String,
    pub retriable: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct WorkflowCheckpoint {
    pub at: u64,
    /// `phase`, `agent` or `resume`.
    pub kind: String,
    pub done_count: u32,
    pub label: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct WorkflowRecovery {
    pub resumable: bool,
    pub reason: Option<String>,
    /// Results served by content on resume, and positions invalidated.
    pub reused: u32,
    pub invalidated: u32,
    /// Other runs that stopped and can be resumed, newest first.
    pub interrupted: Vec<WorkflowInterruptedRun>,
    pub actions: Vec<WorkflowAction>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct WorkflowInterruptedRun {
    pub run_id: String,
    pub name: String,
    pub status: String,
    pub ended_at: u64,
    pub done: u32,
    pub total: u32,
    pub reason: Option<String>,
}

/// An action the extension offers; sent back as an [`ExtensionUiAction`] on
/// [`WORKFLOW_STATE_ELEMENT`] with `row_id` = `run_id`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct WorkflowAction {
    /// `stop`, `pause`, `resume` or `discard`.
    pub id: String,
    pub label: String,
    pub run_id: String,
    pub confirm: bool,
}

impl WorkflowState {
    pub fn of(ui: &ExtensionUiState) -> Option<Self> {
        Self::deserialize(ui.channels.get(WORKFLOW_STATE_CHANNEL)?).ok()
    }

    /// The payload groups its tasks by phase (v2 and later).
    pub fn grouped(&self) -> bool {
        self.v >= 2
    }

    /// The run is executing (not paused, not finished).
    pub fn is_live(&self) -> bool {
        self.run
            .as_ref()
            .is_some_and(|run| run.ended_at.is_none() && run.status != "paused")
    }

    /// The action with this id, when the extension offers it.
    pub fn action(&self, id: &str) -> Option<&WorkflowAction> {
        self.recovery.actions.iter().find(|a| a.id == id)
    }

    /// The [`ExtensionUiAction`] that performs `action`.
    pub fn send(action: &WorkflowAction) -> ExtensionUiAction {
        ExtensionUiAction {
            element_id: WORKFLOW_STATE_ELEMENT.into(),
            action_id: action.id.clone(),
            value: None,
            row_id: Some(action.run_id.clone()),
        }
    }
}

/// One chat's live extension UI, as of `revision`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ExtensionUiState {
    /// Bumped on every change; a UI can skip identical frames.
    pub revision: u64,
    /// The extensions answered the PIUI handshake: elements are authoritative
    /// and blocking questions arrive as sheets rather than dialogs.
    pub connected: bool,
    /// The harness process instance; a change means it restarted.
    pub instance: Option<String>,
    /// PIUI elements in creation order (`{id, ns, kind, placement, ...}`).
    pub elements: Vec<Value>,
    /// Latest payload per PIUI data channel (e.g. `minimal-status:footer`).
    pub channels: BTreeMap<String, Value>,
    /// Plain `setStatus` entries, in first-set order.
    pub statuses: Vec<ExtensionStatus>,
    /// Plain `setWidget` blocks, in first-set order.
    pub widgets: Vec<ExtensionWidget>,
    /// Recent `notify` toasts (newest last, bounded). A UI shows each `seq`
    /// it has not seen yet, once.
    pub notices: Vec<ExtensionNotice>,
    /// The latest `set_editor_text` request; a UI applies a `seq` it has not
    /// seen yet to the chat's composer.
    pub editor_text: Option<SequencedText>,
    /// The latest `setTitle` request; the host renames the chat once per `seq`.
    pub title: Option<SequencedText>,
    /// Elements removed with `durable: true` (newest last, bounded). The host
    /// keeps each `seq` in the transcript once.
    pub durable: Vec<DurableElement>,
}

impl ExtensionUiState {
    /// A sheet (the agent's question, a Pi dialog) waits on the user.
    pub fn has_blocking_sheet(&self) -> bool {
        self.elements
            .iter()
            .any(|element| element.get("placement").and_then(Value::as_str) == Some("sheet"))
    }

    /// Nothing to draw.
    pub fn is_empty(&self) -> bool {
        self.elements.is_empty()
            && self.channels.is_empty()
            && self.statuses.is_empty()
            && self.widgets.is_empty()
    }

    /// Typed views of the elements; ones that fail to parse are skipped.
    ///
    /// Read straight from the raw element: a render path and the fleet
    /// follower both call this on every frame, and copying each element only
    /// to parse the copy was a deep clone per element per frame.
    pub fn typed_elements(&self) -> Vec<PiUiElement> {
        self.elements
            .iter()
            .filter_map(|value| PiUiElement::deserialize(value).ok())
            .collect()
    }

    /// The Pi sub-agents at work (running or waiting), from the fleet
    /// roster: its typed rows while a UI watches the chat, timed from the
    /// [`FLEET_CHANNEL`]; otherwise the plain-text lines Pi mirrors the
    /// roster to, which carry no start. Only the roster is parsed: the
    /// engine follows this on every frame.
    pub fn running_fleet(&self) -> Vec<FleetJob> {
        let live = |state: Option<PiUiRowState>| {
            matches!(state, Some(PiUiRowState::Running | PiUiRowState::Waiting))
        };
        if let Some(roster) = self
            .elements
            .iter()
            .filter(|element| {
                element.get("ns").and_then(Value::as_str) == Some(FLEET_NS)
                    && element.get("kind").and_then(Value::as_str) == Some("roster")
            })
            .find_map(|element| PiUiElement::deserialize(element).ok())
        {
            let entries = self
                .channels
                .get(FLEET_CHANNEL)
                .and_then(|fleet| fleet["entries"].as_array());
            let started = |key: &str| {
                entries?
                    .iter()
                    .find(|entry| entry["key"] == key)
                    .and_then(|entry| entry["startedAt"].as_i64())
            };
            return roster
                .rows
                .into_iter()
                .filter(|row| live(row.state))
                .map(|row| FleetJob {
                    started_at_ms: started(&row.id),
                    row: row.id,
                    label: row.label,
                })
                .collect();
        }
        if self.connected {
            return Vec::new();
        }
        let Some(widget) = self.widgets.iter().find(|w| w.key == FLEET_TEXT_WIDGET) else {
            return Vec::new();
        };
        let mut jobs: Vec<FleetJob> = Vec::new();
        for line in &widget.lines {
            let Some((state, label)) = fleet_text_row(line) else {
                continue;
            };
            if !live(Some(state)) {
                continue;
            }
            // The text has no row ids: the label is one, numbered on repeats.
            let repeats = jobs.iter().filter(|job| job.label == label).count();
            let row = match repeats {
                0 => label.to_owned(),
                n => format!("{label} #{}", n + 1),
            };
            jobs.push(FleetJob {
                row,
                label: label.to_owned(),
                started_at_ms: None,
            });
        }
        jobs
    }

    /// Background bash jobs (tests, builds, CI watches) running now, from
    /// [`BASH_BG_CHANNEL`]. Their results wake the chat by themselves.
    pub fn running_background_jobs(&self) -> u32 {
        let running = self
            .channels
            .get(BASH_BG_CHANNEL)
            .and_then(|fleet| fleet["entries"].as_array())
            .map_or(0, |entries| {
                entries
                    .iter()
                    .filter(|entry| entry["state"] == "running")
                    .count()
            });
        u32::try_from(running).unwrap_or(u32::MAX)
    }
}

/// Pi's background-bash roster channel: each job's `key`, `label` and `state`.
pub const BASH_BG_CHANNEL: &str = "bash-bg:fleet";

/// The Pi extension that publishes the sub-agent fleet roster.
pub const FLEET_NS: &str = "subagents";
/// The fleet's live channel: each job's `key`, `state` and `startedAt`.
pub const FLEET_CHANNEL: &str = "subagents:fleet";
/// The widget Pi mirrors the fleet roster to while no UI watches the chat.
pub const FLEET_TEXT_WIDGET: &str = "piui:subagents:fleet";

/// A Pi sub-agent at work, as [`ExtensionUiState::running_fleet`] reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FleetJob {
    /// Its roster row id (in the text mirror, its label).
    pub row: String,
    pub label: String,
    /// Unix milliseconds, when the fleet channel carries it.
    pub started_at_ms: Option<i64>,
}

/// One mirrored roster line, `{indent}[{state}] {label} — {detail}`: its
/// state and label. Other lines (the title) are not rows.
fn fleet_text_row(line: &str) -> Option<(PiUiRowState, &str)> {
    let (state, rest) = line.trim_start().strip_prefix('[')?.split_once("] ")?;
    let state = serde_json::from_value(Value::String(state.to_owned())).ok()?;
    let label = rest
        .split_once(" \u{2014} ")
        .map_or(rest, |(label, _)| label)
        .trim();
    (!label.is_empty()).then_some((state, label))
}

/// How many recent notices a state keeps for late or reconnecting UIs.
pub const NOTICE_HISTORY: usize = 16;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ExtensionStatus {
    pub key: String,
    pub text: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ExtensionWidget {
    pub key: String,
    pub lines: Vec<String>,
    /// `aboveEditor` (default) or `belowEditor`.
    pub placement: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum NoticeLevel {
    #[default]
    Info,
    Warning,
    Error,
    #[serde(other)]
    Other,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ExtensionNotice {
    pub seq: u64,
    pub message: String,
    pub level: NoticeLevel,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct SequencedText {
    pub seq: u64,
    pub text: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct DurableElement {
    pub seq: u64,
    /// The element's last content (raw, like [`ExtensionUiState::elements`]).
    pub element: Value,
}

/// A user action on an element, sent back to the extensions.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ExtensionUiAction {
    /// `ns:id` of the element.
    pub element_id: String,
    /// One of the element's (or row's) `actions[].id`, or `submit`/`cancel`/`close`.
    pub action_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<Value>,
    /// For roster row actions.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub row_id: Option<String>,
}

// ---------------------------------------------------------------------------
// Typed PIUI views (wire contract: the user's `lib/piui-protocol.ts`)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PiUiPlacement {
    Sheet,
    Screen,
    #[default]
    Pinned,
    Inline,
    Status,
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PiUiKind {
    #[default]
    Panel,
    Roster,
    Progress,
    Log,
    Markdown,
    Diff,
    Form,
    Composer,
    Status,
    Widget,
    #[serde(other)]
    Unknown,
}

/// `busy` shows a spinner; unknown tones render as `info`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PiUiTone {
    #[default]
    Info,
    Success,
    Warning,
    Error,
    Muted,
    Busy,
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PiUiVariant {
    Primary,
    #[default]
    Secondary,
    Danger,
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct PiUiAction {
    pub id: String,
    pub label: String,
    pub variant: Option<PiUiVariant>,
    pub icon: Option<String>,
    /// Ask the user to confirm with this text before sending.
    pub confirm: Option<String>,
    pub disabled: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct PiUiOption {
    pub value: String,
    pub label: String,
    pub description: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PiUiFieldKind {
    Select,
    Multiselect,
    #[default]
    Text,
    Textarea,
    Checkbox,
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct PiUiField {
    pub id: String,
    pub kind: PiUiFieldKind,
    pub label: Option<String>,
    pub placeholder: Option<String>,
    pub options: Vec<PiUiOption>,
    /// String for text kinds, string[] for multiselect, bool for checkbox.
    pub value: Option<Value>,
    pub searchable: bool,
    pub required: bool,
    /// A one-line text field whose value is a secret (an API key, a
    /// password): shown as dots and never copied.
    pub secret: bool,
}

impl PiUiField {
    pub fn initial_text(&self) -> String {
        self.value
            .as_ref()
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned()
    }
    pub fn initial_selection(&self) -> Vec<String> {
        match &self.value {
            Some(Value::String(one)) => vec![one.clone()],
            Some(Value::Array(many)) => many
                .iter()
                .filter_map(|v| v.as_str().map(str::to_owned))
                .collect(),
            _ => vec![],
        }
    }
    pub fn initial_checked(&self) -> bool {
        self.value
            .as_ref()
            .and_then(Value::as_bool)
            .unwrap_or(false)
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PiUiRowState {
    Running,
    Done,
    Error,
    #[default]
    Idle,
    Waiting,
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct PiUiRosterRow {
    pub id: String,
    pub label: String,
    pub state: Option<PiUiRowState>,
    pub detail: Option<String>,
    pub parent: Option<String>,
    pub depth: Option<u32>,
    pub actions: Vec<PiUiAction>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PiUiTurnRole {
    User,
    #[default]
    Assistant,
    Tool,
    Note,
    Error,
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct PiUiTurn {
    pub role: PiUiTurnRole,
    pub text: String,
    pub streaming: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PiUiItemState {
    #[default]
    Pending,
    Active,
    Done,
    Skipped,
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct PiUiChecklistItem {
    pub id: String,
    pub text: String,
    pub state: PiUiItemState,
}

/// Typed blocks inside a panel. Unknown kinds parse as [`PiUiSection::Unknown`]
/// and are skipped, never rejected.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum PiUiSection {
    Status {
        #[serde(default)]
        id: Option<String>,
        #[serde(default)]
        text: String,
        #[serde(default)]
        tone: Option<PiUiTone>,
    },
    Markdown {
        #[serde(default)]
        id: Option<String>,
        #[serde(default)]
        text: String,
    },
    Log {
        #[serde(default)]
        id: Option<String>,
        #[serde(default)]
        text: Option<String>,
        #[serde(default)]
        lines: Vec<String>,
    },
    Meta {
        #[serde(default)]
        id: Option<String>,
        #[serde(default)]
        text: String,
    },
    Form {
        #[serde(default)]
        id: Option<String>,
        #[serde(default)]
        fields: Vec<PiUiField>,
        #[serde(default)]
        actions: Vec<PiUiAction>,
        #[serde(default)]
        placement: Option<PiUiPlacement>,
    },
    Turns {
        #[serde(default)]
        id: Option<String>,
        #[serde(default)]
        turns: Vec<PiUiTurn>,
    },
    Roster {
        #[serde(default)]
        id: Option<String>,
        #[serde(default)]
        rows: Vec<PiUiRosterRow>,
    },
    Progress {
        #[serde(default)]
        id: Option<String>,
        #[serde(default)]
        label: Option<String>,
        #[serde(default)]
        done: Option<f64>,
        #[serde(default)]
        total: Option<f64>,
    },
    Checklist {
        #[serde(default)]
        id: Option<String>,
        #[serde(default)]
        items: Vec<PiUiChecklistItem>,
    },
    Table {
        #[serde(default)]
        id: Option<String>,
        #[serde(default)]
        columns: Vec<String>,
        #[serde(default)]
        rows: Vec<Vec<String>>,
    },
    Diff {
        #[serde(default)]
        id: Option<String>,
        #[serde(default, rename = "unifiedDiff")]
        unified_diff: String,
    },
    #[serde(other)]
    Unknown,
}

impl PiUiSection {
    pub fn id(&self) -> Option<&str> {
        match self {
            Self::Status { id, .. }
            | Self::Markdown { id, .. }
            | Self::Log { id, .. }
            | Self::Meta { id, .. }
            | Self::Form { id, .. }
            | Self::Turns { id, .. }
            | Self::Roster { id, .. }
            | Self::Progress { id, .. }
            | Self::Checklist { id, .. }
            | Self::Table { id, .. }
            | Self::Diff { id, .. } => id.as_deref(),
            Self::Unknown => None,
        }
    }
}

/// Typed view of one element; kind-specific fields are optional.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct PiUiElement {
    pub id: String,
    pub ns: String,
    pub kind: PiUiKind,
    pub placement: PiUiPlacement,
    pub title: Option<String>,
    pub text: Option<String>,
    pub lines: Vec<String>,
    #[serde(deserialize_with = "lenient_sections")]
    pub sections: Vec<PiUiSection>,
    pub actions: Vec<PiUiAction>,
    pub durable: bool,
    /// roster
    pub rows: Vec<PiUiRosterRow>,
    /// progress
    pub done: Option<f64>,
    pub total: Option<f64>,
    pub phase: Option<String>,
    /// form
    pub fields: Vec<PiUiField>,
    /// diff
    pub unified_diff: Option<String>,
    /// Extension-specific extras (`mode`, `startedAt`, `jobId`, ...).
    #[serde(flatten)]
    pub extra: serde_json::Map<String, Value>,
}

/// One malformed section must not hide the rest of the panel.
fn lenient_sections<'de, D>(deserializer: D) -> Result<Vec<PiUiSection>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw = Option::<Vec<Value>>::deserialize(deserializer)?.unwrap_or_default();
    Ok(raw
        .into_iter()
        .map(|value| serde_json::from_value(value).unwrap_or(PiUiSection::Unknown))
        .collect())
}

impl PiUiElement {
    /// `ns:id`, the key actions address.
    pub fn key(&self) -> String {
        format!("{}:{}", self.ns, self.id)
    }

    /// The first form section, if any (sheets).
    pub fn form(&self) -> Option<(&[PiUiField], &[PiUiAction])> {
        self.sections.iter().find_map(|section| match section {
            PiUiSection::Form {
                fields, actions, ..
            } => Some((fields.as_slice(), actions.as_slice())),
            _ => None,
        })
    }

    pub fn action(&self, id: &str) -> Option<&PiUiAction> {
        self.actions.iter().find(|action| action.id == id)
    }

    pub fn extra_str(&self, key: &str) -> Option<&str> {
        self.extra.get(key).and_then(Value::as_str)
    }

    /// Something the user answers or works in (a question sheet, a picker,
    /// the btw thread, a report they opened): a prompt in the panel on the
    /// composer. Everything else is the agent's own work (the advisor, Jev,
    /// fleets, background jobs), recorded in the transcript as a tool row.
    pub fn is_prompt(&self) -> bool {
        matches!(self.placement, PiUiPlacement::Sheet | PiUiPlacement::Screen)
    }

    /// Footer chips, neither a prompt nor a transcript row.
    pub fn is_status(&self) -> bool {
        self.placement == PiUiPlacement::Status || self.kind == PiUiKind::Status
    }

    /// The element reports a failure: an error tone, or a row that failed.
    pub fn failed(&self) -> bool {
        self.rows
            .iter()
            .any(|row| row.state == Some(PiUiRowState::Error))
            || self.sections.iter().any(|section| match section {
                PiUiSection::Status { tone, .. } => *tone == Some(PiUiTone::Error),
                PiUiSection::Roster { rows, .. } => rows
                    .iter()
                    .any(|row| row.state == Some(PiUiRowState::Error)),
                _ => false,
            })
    }

    /// Plain-text rendering (port of `renderElementText`): the fallback for
    /// kinds and sections a UI does not draw natively.
    pub fn text_lines(&self) -> Vec<String> {
        let mut out = Vec::new();
        if let Some(title) = &self.title {
            out.push(title.clone());
        }
        if let Some(text) = &self.text
            && self.title.as_ref() != Some(text)
        {
            out.push(text.clone());
        }
        out.extend(self.rows.iter().map(roster_row_text));
        if self.kind == PiUiKind::Progress
            && let Some(total) = self.total.filter(|total| *total > 0.0)
        {
            out.push(format!(
                "{}{}/{}",
                self.phase
                    .as_ref()
                    .map(|phase| format!("{phase} "))
                    .unwrap_or_default(),
                self.done.unwrap_or(0.0),
                total
            ));
        }
        for section in &self.sections {
            out.extend(section_text(section));
        }
        out.extend(self.lines.iter().cloned());
        if self.kind == PiUiKind::Diff
            && let Some(diff) = &self.unified_diff
        {
            out.extend(diff.lines().map(str::to_owned));
        }
        out
    }
}

pub fn roster_row_text(row: &PiUiRosterRow) -> String {
    let indent = "  ".repeat(row.depth.unwrap_or(0) as usize);
    let state = row
        .state
        .and_then(|state| serde_json::to_value(state).ok())
        .and_then(|value| value.as_str().map(|s| format!("[{s}] ")))
        .unwrap_or_default();
    let detail = row
        .detail
        .as_ref()
        .map(|detail| format!(" — {detail}"))
        .unwrap_or_default();
    format!("{indent}{state}{}{detail}", row.label)
}

/// Plain text of one section (port of `renderSectionText`).
pub fn section_text(section: &PiUiSection) -> Vec<String> {
    let number = |n: Option<f64>| n.map(|n| n.to_string());
    match section {
        PiUiSection::Status { text, .. } | PiUiSection::Meta { text, .. } => vec![text.clone()],
        PiUiSection::Markdown { text, .. } => text.lines().map(str::to_owned).collect(),
        PiUiSection::Log { text, lines, .. } => {
            if !lines.is_empty() {
                lines.clone()
            } else {
                text.iter()
                    .flat_map(|text| text.lines().map(str::to_owned))
                    .collect()
            }
        }
        PiUiSection::Turns { turns, .. } => turns
            .iter()
            .map(|turn| {
                let role = serde_json::to_value(turn.role)
                    .ok()
                    .and_then(|v| v.as_str().map(str::to_owned))
                    .unwrap_or_default();
                format!("{role}: {}", turn.text)
            })
            .collect(),
        PiUiSection::Roster { rows, .. } => rows.iter().map(roster_row_text).collect(),
        PiUiSection::Progress {
            label, done, total, ..
        } => vec![format!(
            "{} {}/{}",
            label.as_deref().unwrap_or("progress"),
            number(*done).unwrap_or_else(|| "0".into()),
            number(*total).unwrap_or_else(|| "?".into())
        )],
        PiUiSection::Checklist { items, .. } => items
            .iter()
            .map(|item| {
                let mark = match item.state {
                    PiUiItemState::Done => "[x]",
                    PiUiItemState::Active => "[>]",
                    PiUiItemState::Skipped => "[-]",
                    _ => "[ ]",
                };
                format!("{mark} {}", item.text)
            })
            .collect(),
        PiUiSection::Table { columns, rows, .. } => std::iter::once(columns.join(" | "))
            .chain(rows.iter().map(|row| row.join(" | ")))
            .collect(),
        PiUiSection::Diff { unified_diff, .. } => unified_diff.lines().map(str::to_owned).collect(),
        PiUiSection::Form { fields, .. } => fields
            .iter()
            .map(|field| {
                let label = field.label.clone().unwrap_or_else(|| field.id.clone());
                if field.options.is_empty() {
                    label
                } else {
                    format!(
                        "{label}: {}",
                        field
                            .options
                            .iter()
                            .map(|option| option.label.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    )
                }
            })
            .collect(),
        PiUiSection::Unknown => vec![],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn context_breakdown_parses_the_extension_payload() {
        let mut ui = ExtensionUiState::default();
        assert_eq!(ContextBreakdown::of(&ui), None);
        ui.channels.insert(
            CONTEXT_WINDOW_CHANNEL.into(),
            json!({"v": 1, "seq": 3, "window": 1_000_000, "future": true,
                   "measured": {"tokens": 69_300, "source": "provider"}, "trailingTokens": 2_000,
                   "categories": {"systemTools": {"tokens": 24_500, "count": 11}, "messages": {"tokens": 8_900}},
                   "deferred": {"mcpTools": {"tokens": 72_000, "count": 124}},
                   "autocompact": {"enabled": true, "atTokens": 967_000, "bufferTokens": 33_000, "source": "native"},
                   "mcp": {"total": 144, "servers": [{"name": "dev", "count": 20, "tokens": 6_100, "declared": 3}]},
                   "estimate": {"method": "chars/4", "scale": 1.07, "calibrated": true}}),
        );
        let b = ContextBreakdown::of(&ui).unwrap();
        assert_eq!(b.used(), Some(71_300));
        assert_eq!(b.categories.system_tools.count, Some(11));
        assert_eq!(b.categories.skills, ContextCategory::default());
        assert_eq!(b.deferred.mcp_tools.tokens, 72_000);
        assert_eq!(b.autocompact.unwrap().buffer_tokens, 33_000);
        assert_eq!(b.mcp.servers[0].declared, 3);
        ui.channels.insert(CONTEXT_WINDOW_CHANNEL.into(), json!({}));
        assert_eq!(
            ContextBreakdown::of(&ui),
            None,
            "no window: nothing to draw"
        );
        ui.channels.insert(
            CONTEXT_WINDOW_CHANNEL.into(),
            json!({"window": 10, "measured": {"tokens": null, "source": "compaction"}}),
        );
        assert_eq!(ContextBreakdown::of(&ui).unwrap().used(), None);
    }

    #[test]
    fn workflow_state_v2_groups_by_phase_and_v1_reads_ungrouped() {
        let mut ui = ExtensionUiState::default();
        ui.channels.insert(
            WORKFLOW_STATE_CHANNEL.into(),
            json!({"v": 2, "run": {"id": "r", "agents": 4, "tokens": 170_800},
                   "phases": [{"title": "Trace", "state": "active", "done": 1, "total": 4,
                               "running": 3, "queued": 1, "tokens": 9, "agents": 3, "startedAt": 5}],
                   "tasks": [{"id": "0", "phaseIndex": 0, "tools": 7, "tokens": 9}]}),
        );
        let state = WorkflowState::of(&ui).unwrap();
        assert!(state.grouped());
        let run = state.run.as_ref().unwrap();
        assert_eq!((run.agents, run.tokens), (4, 170_800));
        let phase = &state.phases[0];
        assert_eq!(
            (phase.running, phase.queued, phase.agents, phase.started_at),
            (3, 1, 3, Some(5))
        );
        assert_eq!(
            (state.tasks[0].phase_index, state.tasks[0].tools),
            (Some(0), Some(7))
        );

        ui.channels.insert(
            WORKFLOW_STATE_CHANNEL.into(),
            json!({"v": 1, "phases": [{"title": "Find"}], "tasks": [{"id": "0"}]}),
        );
        let state = WorkflowState::of(&ui).unwrap();
        assert!(!state.grouped());
        assert_eq!(state.tasks[0].phase_index, None);
        assert_eq!(state.phases[0].agents, 0);
    }

    #[test]
    fn workflow_state_v3_carries_the_resource_gate_and_v2_without_it_still_parses() {
        let mut ui = ExtensionUiState::default();
        ui.channels.insert(
            WORKFLOW_STATE_CHANNEL.into(),
            json!({
                "v": 3,
                "run": {"id": "wf-1", "status": "running", "gate": {
                    "state": "waiting",
                    "reason": "free memory 2.1 GB is below the 4 GB floor",
                    "waiting": 3,
                    "since": 1_760_000_000_000u64,
                    "metrics": {"memAvailableGb": 2.1, "swapUsedPct": 12.5, "load1": 7.2, "diskFreeGb": 40.0}
                }},
                "tasks": [{"id": "0", "state": "queued"}]
            }),
        );
        let state = WorkflowState::of(&ui).expect("parses");
        let gate = state.run.as_ref().unwrap().gate.as_ref().expect("gate");
        assert_eq!(gate.state, "waiting");
        assert_eq!(gate.reason, "free memory 2.1 GB is below the 4 GB floor");
        assert_eq!(gate.waiting, 3);
        assert_eq!(gate.since, Some(1_760_000_000_000));
        let metrics = gate.metrics.as_ref().expect("metrics");
        assert_eq!(
            (
                metrics.mem_available_gb,
                metrics.swap_used_pct,
                metrics.load1,
                metrics.disk_free_gb,
            ),
            (2.1, 12.5, 7.2, 40.0)
        );
        // Round trip keeps the gate.
        let back: WorkflowState =
            serde_json::from_value(serde_json::to_value(&state).unwrap()).unwrap();
        assert_eq!(back, state);
        // A gate state a future extension adds is kept verbatim, not an error.
        ui.channels.insert(
            WORKFLOW_STATE_CHANNEL.into(),
            json!({"v": 3, "run": {"id": "wf-1", "status": "running",
                   "gate": {"state": "paused-for-disk", "reason": "r"}}}),
        );
        let state = WorkflowState::of(&ui).expect("parses");
        assert_eq!(
            state.run.as_ref().unwrap().gate.as_ref().unwrap().state,
            "paused-for-disk"
        );
        // A v2 payload (no gate) still parses, and reads as no gate.
        ui.channels.insert(
            WORKFLOW_STATE_CHANNEL.into(),
            json!({"v": 2, "run": {"id": "wf-1", "status": "running", "agents": 4}}),
        );
        let state = WorkflowState::of(&ui).expect("parses");
        assert_eq!(state.run.as_ref().unwrap().gate, None);
        assert!(state.grouped());
    }

    #[test]
    fn workflow_state_parses_the_extension_payload_and_tolerates_drift() {
        let mut ui = ExtensionUiState::default();
        assert_eq!(WorkflowState::of(&ui), None);
        ui.channels.insert(
            WORKFLOW_STATE_CHANNEL.into(),
            json!({
                "v": 1,
                "updatedAt": 5,
                "run": {"id": "wf-1", "name": "review", "goal": "g", "status": "running", "startedAt": 1},
                "progress": {"total": 3, "done": 1, "running": 1, "retrying": 1},
                "tasks": [
                    {"id": "0", "label": "a", "state": "retrying", "attempt": 2, "maxAttempts": 3,
                     "nextAttemptAt": 9, "error": {"code": "AGENT_RATE_LIMIT", "message": "429", "retriable": true}},
                    {"id": "node:join", "label": "join", "state": "blocked-on-something-new", "dependsOn": ["a"]},
                    {"id": "2", "label": "c", "state": "cached", "cached": true, "reusedFrom": 7, "futureField": 1}
                ],
                "recovery": {"actions": [{"id": "stop", "label": "Stop", "runId": "wf-1", "confirm": true}]},
                "somethingNew": {"x": 1}
            }),
        );
        let state = WorkflowState::of(&ui).expect("parses");
        assert!(state.is_live());
        assert_eq!(state.tasks[0].state, WorkflowTaskState::Retrying);
        assert_eq!(
            state.tasks[0].error.as_ref().unwrap().code,
            "AGENT_RATE_LIMIT"
        );
        assert_eq!(state.tasks[1].state, WorkflowTaskState::Other);
        assert_eq!(state.tasks[1].depends_on, vec!["a".to_string()]);
        assert_eq!(state.tasks[2].reused_from, Some(7));
        let stop = state.action("stop").expect("offered");
        assert_eq!(
            WorkflowState::send(stop),
            ExtensionUiAction {
                element_id: WORKFLOW_STATE_ELEMENT.into(),
                action_id: "stop".into(),
                value: None,
                row_id: Some("wf-1".into()),
            }
        );
        // Round trip keeps what it understood.
        let back: WorkflowState =
            serde_json::from_value(serde_json::to_value(&state).unwrap()).unwrap();
        assert_eq!(back, state);
        // An idle payload (only interrupted runs) is not live.
        ui.channels.insert(
            WORKFLOW_STATE_CHANNEL.into(),
            json!({"v": 1, "run": null, "recovery": {"resumable": true,
                "interrupted": [{"runId": "wf-0", "name": "old", "status": "interrupted", "endedAt": 3, "done": 2, "total": 5}]}}),
        );
        let idle = WorkflowState::of(&ui).unwrap();
        assert!(!idle.is_live());
        assert_eq!(idle.recovery.interrupted[0].run_id, "wf-0");
    }

    #[test]
    fn the_fleet_reads_from_the_watched_roster_with_its_start_times() {
        let mut state = ExtensionUiState {
            connected: true,
            ..Default::default()
        };
        state.elements = vec![json!({
            "id": "fleet", "ns": "subagents", "kind": "roster", "placement": "pinned",
            "rows": [
                {"id": "j1", "label": "audit", "state": "running"},
                {"id": "j1/c1", "label": "grep", "state": "waiting", "depth": 1},
                {"id": "j2", "label": "scan", "state": "done"}
            ]
        })];
        state.channels.insert(
            FLEET_CHANNEL.into(),
            json!({"entries": [{"key": "j1", "startedAt": 1_000}]}),
        );
        assert_eq!(
            state.running_fleet(),
            vec![
                FleetJob {
                    row: "j1".into(),
                    label: "audit".into(),
                    started_at_ms: Some(1_000),
                },
                FleetJob {
                    row: "j1/c1".into(),
                    label: "grep".into(),
                    started_at_ms: None,
                },
            ]
        );
        // Watched but no roster: nothing runs, whatever text lingers.
        state.elements.clear();
        state.widgets = vec![ExtensionWidget {
            key: FLEET_TEXT_WIDGET.into(),
            lines: vec!["[running] audit".into()],
            placement: None,
        }];
        assert!(state.running_fleet().is_empty());
    }

    #[test]
    fn the_fleet_reads_from_its_text_mirror_while_unwatched() {
        let state = ExtensionUiState {
            widgets: vec![ExtensionWidget {
                key: FLEET_TEXT_WIDGET.into(),
                lines: vec![
                    "Sub-agents · 2 running".into(),
                    "[running] audit (reviewer) \u{2014} opus · 12k tokens".into(),
                    "  [waiting] grep".into(),
                    "[done] scan \u{2014} 3k tokens".into(),
                    "[running] audit (reviewer)".into(),
                ],
                placement: None,
            }],
            ..Default::default()
        };
        let jobs = state.running_fleet();
        let rows: Vec<(&str, &str)> = jobs
            .iter()
            .map(|job| (job.row.as_str(), job.label.as_str()))
            .collect();
        assert_eq!(
            rows,
            vec![
                ("audit (reviewer)", "audit (reviewer)"),
                ("grep", "grep"),
                ("audit (reviewer) #2", "audit (reviewer)"),
            ]
        );
        assert!(jobs.iter().all(|job| job.started_at_ms.is_none()));
        assert!(ExtensionUiState::default().running_fleet().is_empty());
    }

    #[test]
    fn running_background_jobs_count_only_running_bash_entries() {
        let mut ui = ExtensionUiState::default();
        assert_eq!(ui.running_background_jobs(), 0);
        ui.channels.insert(
            BASH_BG_CHANNEL.into(),
            serde_json::json!({"entries": [
                {"key": "bg-1", "state": "running"},
                {"key": "bg-2", "state": "completed"},
                {"key": "bg-3", "state": "running"},
            ]}),
        );
        assert_eq!(ui.running_background_jobs(), 2);
    }

    #[test]
    fn only_a_sheet_blocks() {
        let mut state = ExtensionUiState::default();
        assert!(!state.has_blocking_sheet());
        state.elements = vec![
            json!({"id": "s", "kind": "panel", "placement": "screen"}),
            json!({"id": "f", "kind": "status", "placement": "footer"}),
            json!({"id": "n", "kind": "panel"}),
        ];
        assert!(!state.has_blocking_sheet());
        state
            .elements
            .push(json!({"id": "q", "ns": "ask-user", "kind": "panel", "placement": "sheet"}));
        assert!(state.has_blocking_sheet());
    }

    #[test]
    fn the_compact_status_travels_as_a_camel_case_channel() {
        let status = CompactStatus {
            enabled: true,
            threshold_tokens: Some(134_464),
            compacting: true,
            reason: Some("threshold".into()),
            started_at_ms: Some(5),
            tokens_after: None,
            error: None,
        };
        let value = serde_json::to_value(&status).unwrap();
        assert_eq!(value["thresholdTokens"], 134_464);
        assert_eq!(value["startedAtMs"], 5);
        let mut ui = ExtensionUiState::default();
        assert_eq!(CompactStatus::of(&ui), None);
        ui.channels.insert(COMPACT_CHANNEL.into(), value);
        assert_eq!(CompactStatus::of(&ui), Some(status));
        ui.channels.insert(COMPACT_CHANNEL.into(), json!({}));
        assert_eq!(CompactStatus::of(&ui), Some(CompactStatus::default()));
        ui.channels.insert(COMPACT_CHANNEL.into(), json!("nope"));
        assert_eq!(CompactStatus::of(&ui), None);
    }

    #[test]
    fn ask_user_sheet_parses_with_unknown_sections_and_fields_skipped() {
        let el: PiUiElement = serde_json::from_value(json!({
            "id":"question","ns":"ask-user","kind":"panel","placement":"sheet",
            "title":"Question 1/2: Pick one",
            "sections":[
                {"kind":"progress","id":"step","label":"Question 1/2","done":0,"total":2},
                {"kind":"form","id":"ask-form","fields":[
                    {"id":"selection","kind":"select","searchable":true,
                     "options":[{"value":"A","label":"A","description":"first"}]},
                    {"id":"comment","kind":"textarea"},
                    {"id":"future","kind":"slider"}
                ],"actions":[{"id":"submit","label":"Submit","variant":"primary"}]},
                {"kind":"hologram","text":"from the future"},
                {"kind":"markdown","id":"context","text":"**why**"}
            ],
            "actions":[{"id":"skip","label":"Skip"},{"id":"cancel","label":"Cancel"}],
            "futureField": 7
        }))
        .unwrap();
        assert_eq!(el.key(), "ask-user:question");
        assert_eq!(el.placement, PiUiPlacement::Sheet);
        assert_eq!(el.sections.len(), 4);
        assert_eq!(el.sections[2], PiUiSection::Unknown);
        let (fields, actions) = el.form().unwrap();
        assert_eq!(fields[0].kind, PiUiFieldKind::Select);
        assert_eq!(fields[2].kind, PiUiFieldKind::Unknown);
        assert_eq!(actions[0].variant, Some(PiUiVariant::Primary));
        assert_eq!(el.extra["futureField"], 7);
        assert!(el.text_lines().iter().any(|line| line == "**why**"));
    }

    #[test]
    fn roster_and_status_elements_render_text_fallbacks() {
        let el: PiUiElement = serde_json::from_value(json!({
            "id":"jobs","ns":"bash","kind":"roster","placement":"pinned",
            "title":"Background bash · 1 running",
            "rows":[{"id":"j1","label":"npm test","state":"running","detail":"3s",
                     "actions":[{"id":"kill","label":"Kill","variant":"danger","confirm":"Kill this job?"}]}]
        }))
        .unwrap();
        assert_eq!(
            el.text_lines(),
            vec!["Background bash · 1 running", "[running] npm test — 3s"]
        );
        assert_eq!(
            el.rows[0].actions[0].confirm.as_deref(),
            Some("Kill this job?")
        );
    }

    #[test]
    fn prompts_are_sheets_and_screens_and_the_rest_is_the_agents_work() {
        let el = |value: Value| serde_json::from_value::<PiUiElement>(value).unwrap();
        let ask = el(json!({"id":"question","ns":"ask-user","kind":"panel","placement":"sheet"}));
        let btw = el(json!({"id":"thread","ns":"btw","kind":"panel","placement":"screen"}));
        let advisor = el(
            json!({"id":"panel","ns":"advisor-live-panel","kind":"panel",
            "placement":"pinned","sections":[{"kind":"status","text":"failed","tone":"error"}]}),
        );
        let jobs = el(
            json!({"id":"jobs","ns":"bash","kind":"roster","placement":"pinned",
            "rows":[{"id":"j1","label":"npm test","state":"done"}]}),
        );
        let mode = el(json!({"id":"mode","ns":"plan","kind":"status","placement":"status"}));
        assert!(ask.is_prompt() && btw.is_prompt());
        assert!(!advisor.is_prompt() && !jobs.is_prompt() && !mode.is_prompt());
        assert!(mode.is_status() && !jobs.is_status());
        assert!(advisor.failed());
        assert!(!jobs.failed());
    }

    #[test]
    fn state_tolerates_missing_and_future_fields() {
        let state: ExtensionUiState =
            serde_json::from_value(json!({"revision":3,"somethingNew":true})).unwrap();
        assert_eq!(state.revision, 3);
        assert!(state.is_empty());
        let notice: ExtensionNotice =
            serde_json::from_value(json!({"seq":1,"message":"hi","level":"critical"})).unwrap();
        assert_eq!(notice.level, NoticeLevel::Other);
    }
}
