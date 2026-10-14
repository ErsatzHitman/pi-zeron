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
pub const GOAL_CHANNEL: &str = "pi-goal:status";

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
    /// Nothing to draw.
    pub fn is_empty(&self) -> bool {
        self.elements.is_empty()
            && self.channels.is_empty()
            && self.statuses.is_empty()
            && self.widgets.is_empty()
    }

    /// Typed views of the elements; ones that fail to parse are skipped.
    pub fn typed_elements(&self) -> Vec<PiUiElement> {
        self.elements
            .iter()
            .filter_map(|value| serde_json::from_value(value.clone()).ok())
            .collect()
    }
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
        self.value.as_ref().and_then(Value::as_bool).unwrap_or(false)
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
        PiUiSection::Diff { unified_diff, .. } => {
            unified_diff.lines().map(str::to_owned).collect()
        }
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
        assert_eq!(el.rows[0].actions[0].confirm.as_deref(), Some("Kill this job?"));
    }

    #[test]
    fn prompts_are_sheets_and_screens_and_the_rest_is_the_agents_work() {
        let el = |value: Value| serde_json::from_value::<PiUiElement>(value).unwrap();
        let ask = el(json!({"id":"question","ns":"ask-user","kind":"panel","placement":"sheet"}));
        let btw = el(json!({"id":"thread","ns":"btw","kind":"panel","placement":"screen"}));
        let advisor = el(json!({"id":"panel","ns":"advisor-live-panel","kind":"panel",
            "placement":"pinned","sections":[{"kind":"status","text":"failed","tone":"error"}]}));
        let jobs = el(json!({"id":"jobs","ns":"bash","kind":"roster","placement":"pinned",
            "rows":[{"id":"j1","label":"npm test","state":"done"}]}));
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
