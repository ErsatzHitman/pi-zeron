//! Pi's extension UI outside the transcript: the PIUI client (a port of the
//! user's `lib/piui-client.ts`) plus Pi's plain RPC UI requests.
//!
//! PIUI rides Pi's stock RPC: extensions publish their UI as JSON envelopes in
//! `notify` messages prefixed `PIUI `, and a client answers by running the
//! `/pi_ui_event <base64url JSON>` extension command. Nothing is published as
//! PIUI until a client says `hello`; until then (and after `bye`) extensions
//! use plain dialogs, `setWidget` and `setStatus`. See the user's `PIUI.md`.
use base64::Engine as _;
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    time::{Duration, Instant},
};
use tokio::sync::watch;

use crate::redact::strip_ansi;
use zeron_proto::extension_ui::{
    DurableElement, ExtensionNotice, ExtensionStatus, ExtensionUiAction, ExtensionUiState,
    ExtensionWidget, NOTICE_HISTORY, NoticeLevel, SequencedText,
};

const MARKER: &str = "PIUI ";
const PROTOCOL: u64 = 1;
/// The extension command a client invokes; hidden from the palette.
pub(super) const EVENT_COMMAND: &str = "pi_ui_event";
/// Incomplete chunk sets are dropped after this long; a snapshot repairs state.
const CHUNK_TTL: std::time::Duration = std::time::Duration::from_secs(60);
/// Bounded like notices: the host folds each durable element once.
const DURABLE_HISTORY: usize = 16;
/// Namespace of the sheets the host draws for Pi's own dialogs.
const DIALOG_NS: &str = "pi";
/// A between-turn notice this long opens as a report screen, not a toast.
const REPORT_LINES: usize = 4;

/// A plain Pi dialog (`select`, `confirm`, `input`, `editor`) that arrived
/// with no turn to answer it in, shown to the watching UI as a sheet.
struct HostDialog {
    key: String,
    id: Value,
    method: String,
    options: Vec<String>,
    deadline: Option<Instant>,
    element: Value,
}

/// What [`ExtensionUi::handle`] did with an `extension_ui_request`.
#[derive(Debug, PartialEq)]
pub(super) enum Handled {
    /// PIUI or plain UI state; nothing else to do.
    Consumed,
    /// A plain (non-PIUI) `notify`. Between turns it is recorded as a notice
    /// (a toast); during one it is not, and the turn records it as a row.
    Notice(String),
    /// A blocking dialog (`select`, `confirm`, `input`, `editor`).
    Dialog,
}

struct Chunks {
    parts: Vec<Option<String>>,
    received: usize,
    started: Instant,
}

#[derive(Default)]
pub(super) struct ExtensionUi {
    sink: Option<watch::Sender<ExtensionUiState>>,
    state: ExtensionUiState,
    /// `ns:id` of each element in `state.elements`, same order.
    keys: Vec<String>,
    last_seq: u64,
    chunks: HashMap<String, Chunks>,
    seq: u64,
    /// Published after the PIUI elements; a PIUI `reset` never clears them.
    dialogs: Vec<HostDialog>,
    /// Channels Zeron keeps itself (the compaction status). A PIUI `reset`
    /// never touches them; `closed()` drops them with the process.
    host: std::collections::BTreeMap<String, Value>,
    dialog_seq: u64,
    /// Long between-turn reports (`/lsp`, `/mcp` status), shown as closable
    /// screens: a toast is gone before it is read. `(key, element)`.
    reports: Vec<(String, Value)>,
    /// A turn is running: its notices become transcript rows, not toasts.
    in_turn: bool,
}

/// `state` without anything a process drew: its revision and one-shot requests.
fn one_shot_requests(state: &ExtensionUiState) -> ExtensionUiState {
    ExtensionUiState {
        revision: state.revision,
        notices: state.notices.clone(),
        title: state.title.clone(),
        editor_text: state.editor_text.clone(),
        durable: state.durable.clone(),
        ..Default::default()
    }
}

fn last_request_seq(state: &ExtensionUiState) -> u64 {
    let notices = state.notices.iter().map(|n| n.seq);
    let durable = state.durable.iter().map(|d| d.seq);
    let texts = [&state.title, &state.editor_text]
        .into_iter()
        .flatten()
        .map(|t| t.seq);
    notices.chain(durable).chain(texts).max().unwrap_or(0)
}

/// A `notify`'s level.
pub(super) fn notice_level(frame: &Value) -> NoticeLevel {
    match frame["notifyType"].as_str() {
        Some("warning") => NoticeLevel::Warning,
        Some("error") => NoticeLevel::Error,
        _ => NoticeLevel::Info,
    }
}

pub(super) fn element_key(element: &Value) -> String {
    format!(
        "{}:{}",
        element["ns"].as_str().unwrap_or(""),
        element["id"].as_str().unwrap_or("")
    )
}

/// The prompt text that delivers `event` to the extensions.
/// Whether an `input` dialog asks for a credential: Pi's dialogs carry no
/// such flag, so the title, message and placeholder are read for the usual
/// words (`/runinfra-key`, `/login` API keys, MCP tokens).
pub(super) fn asks_for_secret(frame: &Value) -> bool {
    // "token" alone also names counts ("token threshold"), so only the
    // credential phrases.
    const WORDS: [&str; 12] = [
        "api key",
        "api-key",
        "apikey",
        "password",
        "passphrase",
        "secret",
        "access token",
        "auth token",
        "bearer",
        "openrouter key",
        "license key",
        "private key",
    ];
    // Provider keys announce themselves by their prefix (`sk-or-v1-…`).
    let key_placeholder = frame["placeholder"]
        .as_str()
        .is_some_and(|text| text.trim_start().starts_with("sk-"));
    key_placeholder
        || ["title", "message", "placeholder"].iter().any(|key| {
            frame[*key].as_str().is_some_and(|text| {
                let text = strip_ansi(text).to_lowercase().replace('_', " ");
                WORDS.iter().any(|word| text.contains(word))
            })
        })
}

pub(super) fn encode_client_event(event: &Value) -> String {
    let json = serde_json::to_vec(event).expect("JSON value");
    format!(
        "/{EVENT_COMMAND} {}",
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(json)
    )
}

pub(super) fn hello() -> Value {
    json!({"type":"hello","protocol":PROTOCOL,"client":"zeron"})
}

/// `channels`: keep publishing data channels (fleets, shells, the goal) with
/// nobody watching, so the host still sees background work it must not end.
pub(super) fn bye() -> Value {
    json!({"type":"bye","channels":true})
}

/// A long notice as a closable report screen: `Key: value` lines as a table,
/// anything else as a log. `None` for a notice a toast holds.
fn report_element(id: &str, message: &str) -> Option<Value> {
    let lines: Vec<&str> = message
        .lines()
        .map(str::trim_end)
        .filter(|line| !line.trim().is_empty())
        .collect();
    if lines.len() < REPORT_LINES {
        return None;
    }
    let pair = |line: &str| {
        line.split_once(": ")
            .filter(|(key, _)| !key.trim().is_empty() && key.trim().len() <= 40)
            .map(|(key, value)| vec![key.trim().to_owned(), value.trim().to_owned()])
    };
    let (title, body) = match lines.split_first() {
        Some((first, rest)) if pair(first).is_none() && first.len() <= 80 => {
            (first.trim().trim_end_matches(':').to_owned(), rest.to_vec())
        }
        _ => ("Report".to_owned(), lines.clone()),
    };
    let rows: Option<Vec<Vec<String>>> = body.iter().map(|line| pair(line)).collect();
    let section = match rows {
        Some(rows) => json!({"kind":"table","id":"report","rows":rows}),
        None => json!({"kind":"log","id":"report","lines":body}),
    };
    Some(json!({
        "id":id, "ns":DIALOG_NS, "kind":"panel", "placement":"screen",
        "title":title, "sections":[section],
        "actions":[{"id":"close","label":"Close"}],
    }))
}

pub(super) fn action_event(action: &ExtensionUiAction) -> Value {
    let mut event = json!({
        "type":"action",
        "elementId":action.element_id,
        "actionId":action.action_id,
    });
    if let Some(value) = &action.value {
        event["value"] = value.clone();
    }
    if let Some(row) = &action.row_id {
        event["rowId"] = json!(row);
    }
    event
}

impl ExtensionUi {
    /// A chat's runs share one state. A new run keeps the last run's
    /// one-shot requests ([`Self::closed`]) and numbers its own after them, so
    /// a UI that applied those still applies these.
    pub fn new(sink: Option<watch::Sender<ExtensionUiState>>) -> Self {
        let state = sink
            .as_ref()
            .map(|sink| one_shot_requests(&sink.borrow()))
            .unwrap_or_default();
        let seq = last_request_seq(&state);
        Self {
            sink,
            state,
            seq,
            ..Default::default()
        }
    }

    pub fn state(&self) -> &ExtensionUiState {
        &self.state
    }

    /// Whether a turn is running (see [`Handled::Notice`]).
    pub fn set_in_turn(&mut self, in_turn: bool) {
        self.in_turn = in_turn;
    }

    /// A footer chip Zeron keeps itself (Pi retrying a failed request);
    /// `None` takes it away.
    pub fn host_status(&mut self, key: &str, text: Option<String>) {
        let statuses = &mut self.state.statuses;
        let at = statuses.iter().position(|status| status.key == key);
        match (text, at) {
            (Some(text), Some(at)) if statuses[at].text == text => return,
            (Some(text), Some(at)) => statuses[at].text = text,
            (Some(text), None) => statuses.push(ExtensionStatus {
                key: key.to_owned(),
                text,
            }),
            (None, Some(at)) => {
                statuses.remove(at);
            }
            (None, None) => return,
        }
        self.publish();
    }

    /// A channel Zeron keeps itself; `None` takes it away.
    pub fn host_channel(&mut self, name: &str, payload: Option<Value>) {
        match payload {
            Some(payload) if self.host.get(name) == Some(&payload) => return,
            Some(payload) => {
                self.host.insert(name.to_owned(), payload);
            }
            None if self.host.remove(name).is_some() => {}
            None => return,
        }
        self.publish();
    }

    fn publish(&mut self) {
        self.state.revision += 1;
        if let Some(sink) = &self.sink {
            let mut state = self.state.clone();
            state
                .channels
                .extend(self.host.iter().map(|(name, value)| (name.clone(), value.clone())));
            state
                .elements
                .extend(self.dialogs.iter().map(|dialog| dialog.element.clone()));
            state
                .elements
                .extend(self.reports.iter().map(|(_, element)| element.clone()));
            sink.send_replace(state);
        }
    }

    /// Show a plain dialog as a sheet; false for a method no sheet answers.
    pub fn open_dialog(&mut self, frame: &Value) -> bool {
        let method = frame["method"].as_str().unwrap_or("");
        if !matches!(method, "select" | "confirm" | "input" | "editor") {
            return false;
        }
        let options: Vec<String> = frame["options"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|v| v.as_str().map(str::to_owned))
            .collect();
        self.dialog_seq += 1;
        let id = format!("dialog-{}", self.dialog_seq);
        let submit = json!({"id":"submit","label":"Submit","variant":"primary"});
        let text_field = |kind: &str| {
            let mut field = json!({"id":"value","kind":kind});
            if let Some(placeholder) = frame["placeholder"].as_str() {
                field["placeholder"] = json!(placeholder);
            }
            if let Some(prefill) = frame["prefill"].as_str() {
                field["value"] = json!(prefill);
            }
            field
        };
        let (fields, actions) = match method {
            "select" => {
                // The value goes back verbatim (Pi matches it against its own
                // options); only the label loses terminal escapes.
                let choices: Vec<Value> = options
                    .iter()
                    .map(|o| json!({"value":o,"label":strip_ansi(o)}))
                    .collect();
                let searchable = choices.len() > 7;
                (
                    vec![json!({"id":"value","kind":"select","options":choices,
                        "searchable":searchable,"required":true})],
                    vec![submit],
                )
            }
            "confirm" => (
                vec![],
                vec![
                    json!({"id":"yes","label":"Yes","variant":"primary"}),
                    json!({"id":"no","label":"No","variant":"secondary"}),
                ],
            ),
            "input" => {
                let mut field = text_field("text");
                if asks_for_secret(frame) {
                    field["secret"] = json!(true);
                }
                (vec![field], vec![submit])
            }
            _ => (vec![text_field("textarea")], vec![submit]),
        };
        let mut element = json!({
            "id":id, "ns":DIALOG_NS, "kind":"panel", "placement":"sheet",
            "title":strip_ansi(frame["title"].as_str().unwrap_or("Pi")),
            "sections":[{"kind":"form","id":"dialog","fields":fields,"actions":actions}],
            "actions":[{"id":"cancel","label":"Cancel","variant":"secondary"}],
        });
        if let Some(message) = frame["message"].as_str().filter(|m| !m.is_empty()) {
            element["text"] = json!(strip_ansi(message));
        }
        self.dialogs.push(HostDialog {
            key: format!("{DIALOG_NS}:{id}"),
            id: frame["id"].clone(),
            method: method.to_owned(),
            options,
            deadline: frame["timeout"]
                .as_u64()
                .map(|ms| Instant::now() + Duration::from_millis(ms)),
            element,
        });
        self.publish();
        true
    }

    /// The `extension_ui_response` for an action on a dialog sheet, which
    /// closes it; `None` when the action answers no open dialog.
    pub fn answer_dialog(&mut self, action: &ExtensionUiAction) -> Option<Value> {
        let at = self
            .dialogs
            .iter()
            .position(|dialog| dialog.key == action.element_id)?;
        let dialog = &self.dialogs[at];
        let value = action
            .value
            .as_ref()
            .map(|value| value.get("fields").unwrap_or(value))
            .and_then(|fields| fields.get("value"))
            .and_then(Value::as_str);
        let mut response = json!({"type":"extension_ui_response","id":dialog.id});
        match (dialog.method.as_str(), action.action_id.as_str(), value) {
            ("confirm", "yes" | "no", _) => {
                response["confirmed"] = json!(action.action_id == "yes")
            }
            ("select", "submit", Some(value)) if dialog.options.iter().any(|o| o == value) => {
                response["value"] = json!(value)
            }
            ("input" | "editor", "submit", value) => {
                response["value"] = json!(value.unwrap_or(""))
            }
            (_, "cancel", _) => response["cancelled"] = json!(true),
            // Not an answer (an empty select): the sheet stays open.
            _ => return None,
        }
        self.dialogs.remove(at);
        self.publish();
        Some(response)
    }

    /// Close a report screen; false when the action is for anything else.
    pub fn close_report(&mut self, action: &ExtensionUiAction) -> bool {
        let before = self.reports.len();
        self.reports.retain(|(key, _)| *key != action.element_id);
        if self.reports.len() == before {
            return false;
        }
        self.publish();
        true
    }

    /// When the earliest open dialog times out.
    pub fn next_deadline(&self) -> Option<Instant> {
        self.dialogs.iter().filter_map(|d| d.deadline).min()
    }

    /// Cancel responses for the dialogs past their deadline (every dialog
    /// with `all`), closing their sheets.
    pub fn close_dialogs(&mut self, all: bool) -> Vec<Value> {
        let now = Instant::now();
        let (closed, open): (Vec<_>, Vec<_>) = std::mem::take(&mut self.dialogs)
            .into_iter()
            .partition(|d| all || d.deadline.is_some_and(|at| at <= now));
        self.dialogs = open;
        if !closed.is_empty() {
            self.publish();
        }
        closed
            .into_iter()
            .map(|d| json!({"type":"extension_ui_response","id":d.id,"cancelled":true}))
            .collect()
    }

    fn next_seq(&mut self) -> u64 {
        self.seq += 1;
        self.seq
    }

    /// Route one `extension_ui_request`.
    pub fn handle(&mut self, frame: &Value) -> Handled {
        let text = |key: &str| frame[key].as_str().unwrap_or("").to_owned();
        match frame["method"].as_str().unwrap_or("") {
            "notify" => {
                let message = text("message");
                if let Some(envelope) = message.strip_prefix(MARKER) {
                    // Never a toast or transcript text, even when malformed.
                    if let Ok(envelope) = serde_json::from_str::<Value>(envelope)
                        && self.accept(envelope)
                    {
                        self.publish();
                    }
                    return Handled::Consumed;
                }
                // Extensions colour terminal output (and OSC-8 link it); a GUI
                // shows the plain text.
                let message = strip_ansi(&message);
                if self.in_turn {
                    return Handled::Notice(message);
                }
                if let Some(element) =
                    report_element(&format!("report-{}", self.dialog_seq + 1), &message)
                {
                    self.dialog_seq += 1;
                    let key = format!("{DIALOG_NS}:report-{}", self.dialog_seq);
                    self.reports.push((key, element));
                    self.publish();
                    return Handled::Consumed;
                }
                let level = notice_level(frame);
                let seq = self.next_seq();
                self.state.notices.push(ExtensionNotice {
                    seq,
                    message: message.clone(),
                    level,
                });
                let excess = self.state.notices.len().saturating_sub(NOTICE_HISTORY);
                self.state.notices.drain(..excess);
                self.publish();
                Handled::Notice(message)
            }
            "setStatus" => {
                let key = text("statusKey");
                let statuses = &mut self.state.statuses;
                match frame["statusText"].as_str() {
                    Some(value) => {
                        // A chip draws its own frame: drop terminal box rails.
                        let value = strip_ansi(value)
                            .trim_matches(|c: char| {
                                c.is_whitespace() || ('\u{2500}'..='\u{257F}').contains(&c)
                            })
                            .to_owned();
                        match statuses.iter_mut().find(|s| s.key == key) {
                            Some(status) => status.text = value,
                            None => statuses.push(ExtensionStatus { key, text: value }),
                        }
                    }
                    None => statuses.retain(|s| s.key != key),
                }
                self.publish();
                Handled::Consumed
            }
            "setWidget" => {
                let key = text("widgetKey");
                let widgets = &mut self.state.widgets;
                match frame["widgetLines"].as_array() {
                    Some(lines) => {
                        let widget = ExtensionWidget {
                            key: key.clone(),
                            lines: lines
                                .iter()
                                .map(|line| strip_ansi(line.as_str().unwrap_or("")))
                                .collect(),
                            placement: frame["widgetPlacement"].as_str().map(str::to_owned),
                        };
                        match widgets.iter_mut().find(|w| w.key == key) {
                            Some(existing) => *existing = widget,
                            None => widgets.push(widget),
                        }
                    }
                    None => widgets.retain(|w| w.key != key),
                }
                self.publish();
                Handled::Consumed
            }
            "setTitle" => {
                let seq = self.next_seq();
                self.state.title = Some(SequencedText {
                    seq,
                    text: strip_ansi(&text("title")),
                });
                self.publish();
                Handled::Consumed
            }
            // Over RPC an extension reads the editor as "" and may write that
            // back (`getEditorText` then `setEditorText`): it would wipe what
            // the user typed.
            "set_editor_text" if frame["text"].as_str().is_none_or(str::is_empty) => {
                Handled::Consumed
            }
            "set_editor_text" => {
                let seq = self.next_seq();
                self.state.editor_text = Some(SequencedText {
                    seq,
                    text: text("text"),
                });
                self.publish();
                Handled::Consumed
            }
            "select" | "confirm" | "input" | "editor" => Handled::Dialog,
            // Unknown fire-and-forget UI: never a dialog, never transcript text.
            _ => Handled::Consumed,
        }
    }

    /// PIUI was turned off (`bye`) or never answered: its elements are no
    /// longer maintained by the extensions, so they must not linger.
    pub fn disconnect(&mut self) {
        if self.state.connected || !self.state.elements.is_empty() || !self.chunks.is_empty() {
            self.state.connected = false;
            self.state.elements.clear();
            self.keys.clear();
            // Channels stay: `bye` asks the hub to keep them current, and they
            // are how the host knows background work is still running.
            self.chunks.clear();
            self.publish();
        }
    }

    /// The Pi process ended: nothing it drew survives it. Its notices, title,
    /// editor text and durable elements stay: a UI applies each once by
    /// `seq`, and a run's last ones (a command's closing report, a draft put
    /// back) arrive just before the process ends, often within the UI's next
    /// frame.
    pub fn closed(&mut self) {
        self.state = one_shot_requests(&self.state);
        self.keys.clear();
        self.chunks.clear();
        self.dialogs.clear();
        self.host.clear();
        self.last_seq = 0;
        self.publish();
    }

    /// Feed one PIUI wire message; true when state changed.
    fn accept(&mut self, message: Value) -> bool {
        if message["op"] == "chunk" {
            return self.accept_chunk(&message);
        }
        self.apply(message)
    }

    fn accept_chunk(&mut self, chunk: &Value) -> bool {
        let now = Instant::now();
        self.chunks
            .retain(|_, entry| now.duration_since(entry.started) < CHUNK_TTL);
        let (Some(id), Some(i), Some(n), Some(data)) = (
            chunk["id"].as_str(),
            chunk["i"].as_u64(),
            chunk["n"].as_u64(),
            chunk["data"].as_str(),
        ) else {
            return false;
        };
        let (i, n) = (i as usize, n as usize);
        if n == 0 || i >= n || n > 4096 {
            return false;
        }
        let entry = self.chunks.entry(id.to_owned()).or_insert_with(|| Chunks {
            parts: vec![None; n],
            received: 0,
            started: now,
        });
        if entry.parts.len() != n || entry.parts[i].is_some() {
            return false;
        }
        entry.parts[i] = Some(data.to_owned());
        entry.received += 1;
        if entry.received < n {
            return false;
        }
        let entry = self.chunks.remove(id).expect("complete chunk set");
        let joined: String = entry.parts.into_iter().flatten().collect();
        // A corrupt envelope is dropped; the next snapshot repairs state.
        serde_json::from_str::<Value>(&joined).is_ok_and(|envelope| self.apply(envelope))
    }

    fn apply(&mut self, envelope: Value) -> bool {
        if let Some(instance) = envelope["instance"].as_str()
            && self.state.instance.as_deref() != Some(instance)
        {
            // Pi restarted (or this is the first message): sequence numbers
            // start over, and an earlier instance's elements are gone.
            if self.state.instance.is_some() {
                self.clear();
            }
            self.state.instance = Some(instance.to_owned());
            self.last_seq = 0;
        }
        if let Some(seq) = envelope["agentSeq"].as_u64() {
            if seq <= self.last_seq {
                return false;
            }
            self.last_seq = seq;
        }
        let key = |envelope: &Value| {
            format!(
                "{}:{}",
                envelope["ns"].as_str().unwrap_or(""),
                envelope["id"].as_str().unwrap_or("")
            )
        };
        match envelope["op"].as_str().unwrap_or("") {
            "hello" => self.state.connected = true,
            "reset" => self.clear(),
            "set" => {
                let element = envelope["el"].clone();
                if !element.is_object() {
                    return false;
                }
                let key = element_key(&element);
                match self.keys.iter().position(|k| *k == key) {
                    Some(at) => self.state.elements[at] = element,
                    None => {
                        self.keys.push(key);
                        self.state.elements.push(element);
                    }
                }
            }
            "patch" => {
                let key = key(&envelope);
                let Some(at) = self.keys.iter().position(|k| *k == key) else {
                    return false;
                };
                let (Some(target), Some(patch)) = (
                    self.state.elements[at].as_object_mut(),
                    envelope["patch"].as_object(),
                ) else {
                    return false;
                };
                for (field, value) in patch {
                    target.insert(field.clone(), value.clone());
                }
            }
            "append" => {
                let key = key(&envelope);
                let Some(at) = self.keys.iter().position(|k| *k == key) else {
                    return false;
                };
                let element = &mut self.state.elements[at];
                if !element["lines"].is_array() {
                    element["lines"] = json!([]);
                }
                if let Some(lines) = element["lines"].as_array_mut() {
                    lines.push(envelope["data"].clone());
                }
            }
            "remove" => {
                let key = key(&envelope);
                let Some(at) = self.keys.iter().position(|k| *k == key) else {
                    return false;
                };
                self.keys.remove(at);
                let element = self.state.elements.remove(at);
                if element["durable"] == true {
                    let seq = self.next_seq();
                    self.state.durable.push(DurableElement { seq, element });
                    let excess = self.state.durable.len().saturating_sub(DURABLE_HISTORY);
                    self.state.durable.drain(..excess);
                }
            }
            "channel" => {
                let Some(channel) = envelope["channel"].as_str() else {
                    return false;
                };
                self.state
                    .channels
                    .insert(channel.to_owned(), envelope["payload"].clone());
            }
            _ => return false,
        }
        true
    }

    fn clear(&mut self) {
        self.state.elements.clear();
        self.keys.clear();
        self.state.channels.clear();
    }

    /// Background work the extensions report through their data channels
    /// (subagent fleets, background shells, workflow runs). While any is
    /// live, retiring the idle Pi process would kill work nobody stopped.
    pub fn background_tasks(&self) -> usize {
        let count = |channel: &str| {
            let Some(payload) = self.state.channels.get(channel) else {
                return 0;
            };
            // A fleet lists its jobs; its `active` is the terminal's manage
            // mode, not work.
            if let Some(entries) = payload["entries"].as_array() {
                return entries
                    .iter()
                    .filter(|entry| matches!(entry["state"].as_str(), Some("running" | "queued")))
                    .count();
            }
            match &payload["active"] {
                Value::Number(n) => n.as_u64().unwrap_or(0) as usize,
                Value::Bool(true) => 1,
                Value::Array(items) => items.len(),
                _ => 0,
            }
        };
        // A goal loop waits on timers (`goal_wait`, backoff, check-ins).
        let goal = self
            .state
            .channels
            .get("pi-goal:status")
            .is_some_and(|goal| {
                goal["active"] == true
                    && matches!(goal["status"].as_str(), Some("active" | "waiting"))
            });
        // A workflow run publishes both its footer line and its full state; an
        // extension that only sends `workflow:state` still counts, once.
        let workflow_state = self
            .state
            .channels
            .get(zeron_proto::extension_ui::WORKFLOW_STATE_CHANNEL)
            .is_some_and(|state| state["run"].is_object() && state["run"]["endedAt"].is_null());
        count("subagents:fleet")
            + count("bash-bg:fleet")
            + count("workflow:progress").max(usize::from(workflow_state))
            + usize::from(goal)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_channels_publish_beside_piui_and_go_with_the_process() {
        let (tx, rx) = watch::channel(ExtensionUiState::default());
        let mut ui = ExtensionUi::new(Some(tx));
        ui.host_channel("zeron:compact", Some(json!({"enabled": true})));
        let revision = rx.borrow().revision;
        assert_eq!(rx.borrow().channels["zeron:compact"], json!({"enabled": true}));
        // The same payload again is no change.
        ui.host_channel("zeron:compact", Some(json!({"enabled": true})));
        assert_eq!(rx.borrow().revision, revision);
        // A PIUI reset keeps it.
        ui.handle(&notify(json!({"v":1,"agentSeq":1,"instance":"a","op":"reset"})));
        assert!(rx.borrow().channels.contains_key("zeron:compact"));
        ui.host_channel("zeron:compact", None);
        assert!(!rx.borrow().channels.contains_key("zeron:compact"));
        ui.host_channel("zeron:compact", Some(json!({})));
        ui.closed();
        assert!(rx.borrow().channels.is_empty());
    }

    fn notify(envelope: Value) -> Value {
        json!({"type":"extension_ui_request","id":"n","method":"notify",
               "message":format!("{MARKER}{envelope}"),"notifyType":"info"})
    }

    fn set(seq: u64, instance: &str, el: Value) -> Value {
        notify(json!({"v":1,"agentSeq":seq,"instance":instance,"op":"set","el":el}))
    }

    #[test]
    fn client_events_are_base64url_prompts() {
        let text = encode_client_event(&hello());
        let payload = text.strip_prefix("/pi_ui_event ").unwrap();
        assert!(!payload.contains(['+', '/', '=']));
        let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(payload)
            .unwrap();
        assert_eq!(
            serde_json::from_slice::<Value>(&decoded).unwrap(),
            json!({"type":"hello","protocol":1,"client":"zeron"})
        );
        let action = action_event(&ExtensionUiAction {
            element_id: "bash:jobs".into(),
            action_id: "kill".into(),
            value: None,
            row_id: Some("j3".into()),
        });
        assert_eq!(
            action,
            json!({"type":"action","elementId":"bash:jobs","actionId":"kill","rowId":"j3"})
        );
    }

    #[test]
    fn envelopes_apply_in_order_and_duplicates_or_stale_seqs_are_dropped() {
        let (tx, rx) = watch::channel(ExtensionUiState::default());
        let mut ui = ExtensionUi::new(Some(tx));
        let el = json!({"id":"list","ns":"todo","kind":"panel","placement":"pinned","title":"Todos 0/2"});
        assert_eq!(ui.handle(&set(1, "a", el.clone())), Handled::Consumed);
        assert_eq!(
            ui.handle(&notify(json!({"v":1,"agentSeq":2,"instance":"a","op":"patch",
                "ns":"todo","id":"list","patch":{"title":"Todos 1/2"}}))),
            Handled::Consumed
        );
        // A replay of seq 2 and an older seq change nothing.
        ui.handle(&notify(json!({"v":1,"agentSeq":2,"instance":"a","op":"patch",
            "ns":"todo","id":"list","patch":{"title":"stale"}})));
        ui.handle(&set(1, "a", json!({"id":"x","ns":"y","kind":"status","placement":"status"})));
        let state = rx.borrow().clone();
        assert_eq!(state.elements.len(), 1);
        assert_eq!(state.elements[0]["title"], "Todos 1/2");
        // append + channel
        ui.handle(&notify(json!({"v":1,"agentSeq":3,"instance":"a","op":"append",
            "ns":"todo","id":"list","data":"line"})));
        ui.handle(&notify(json!({"v":1,"agentSeq":4,"instance":"a","op":"channel",
            "channel":"bash-bg:fleet","payload":{"active":2}})));
        assert_eq!(ui.state().elements[0]["lines"], json!(["line"]));
        assert_eq!(ui.background_tasks(), 2);
    }

    #[test]
    fn a_runs_last_notice_and_draft_outlive_its_process_and_the_next_run_numbers_after_them() {
        let (tx, rx) = watch::channel(ExtensionUiState::default());
        let mut ui = ExtensionUi::new(Some(tx.clone()));
        let card = json!({"id":"card","ns":"x","kind":"panel","placement":"pinned"});
        ui.handle(&set(1, "a", card));
        let report = json!({"type":"extension_ui_request","method":"notify",
            "message":"Calibration complete","notifyType":"info"});
        ui.handle(&report);
        ui.handle(&json!({"method":"set_editor_text","text":"draft"}));
        ui.closed();
        let closed = rx.borrow().clone();
        assert!(closed.elements.is_empty());
        assert!(!closed.connected);
        assert_eq!(closed.notices[0].message, "Calibration complete");
        assert_eq!(closed.editor_text.as_ref().unwrap().text, "draft");
        let last = closed.editor_text.unwrap().seq;

        let mut next = ExtensionUi::new(Some(tx));
        let notice = json!({"type":"extension_ui_request","method":"notify",
            "message":"next run","notifyType":"info"});
        next.handle(&notice);
        let state = rx.borrow().clone();
        assert_eq!(state.notices.len(), 2);
        assert!(state.notices[1].seq > last);
        assert!(state.revision > closed.revision);
    }

    #[test]
    fn a_new_instance_clears_state_and_restarts_sequence_numbers() {
        let mut ui = ExtensionUi::new(None);
        ui.handle(&set(9, "a", json!({"id":"jobs","ns":"bash","kind":"roster","placement":"pinned"})));
        ui.handle(&set(1, "b", json!({"id":"list","ns":"todo","kind":"panel","placement":"pinned"})));
        let keys: Vec<_> = ui.state().elements.iter().map(element_key).collect();
        assert_eq!(keys, vec!["todo:list"]);
        assert_eq!(ui.state().instance.as_deref(), Some("b"));
    }

    #[test]
    fn chunks_reassemble_in_any_order_and_durable_removals_are_kept() {
        let mut ui = ExtensionUi::new(None);
        let envelope = json!({"v":1,"agentSeq":1,"instance":"a","op":"set","el":{
            "id":"thread","ns":"btw","kind":"panel","placement":"screen","durable":true,
            "title":"ñ".repeat(10)}})
        .to_string();
        let mid = envelope.char_indices().nth(envelope.chars().count() / 2).unwrap().0;
        let (first, second) = envelope.split_at(mid);
        let chunk = |i: u64, data: &str| {
            notify(json!({"v":1,"op":"chunk","id":"c1_a","i":i,"n":2,"data":data}))
        };
        ui.handle(&chunk(1, second));
        assert!(ui.state().elements.is_empty());
        ui.handle(&chunk(1, second));
        ui.handle(&chunk(0, first));
        assert_eq!(ui.state().elements.len(), 1);
        ui.handle(&notify(json!({"v":1,"agentSeq":2,"instance":"a","op":"remove","ns":"btw","id":"thread"})));
        assert!(ui.state().elements.is_empty());
        assert_eq!(ui.state().durable.len(), 1);
        assert_eq!(ui.state().durable[0].element["title"], "ñ".repeat(10));
    }

    #[test]
    fn idle_dialogs_become_sheets_answered_by_actions() {
        let (tx, rx) = watch::channel(ExtensionUiState::default());
        let mut ui = ExtensionUi::new(Some(tx));
        let act = |element: &str, action: &str, value: Option<Value>| ExtensionUiAction {
            element_id: element.into(),
            action_id: action.into(),
            value,
            row_id: None,
        };
        assert!(ui.open_dialog(&json!({"id":"s","method":"select","title":"Pick",
            "options":["a","b"]})));
        assert!(ui.open_dialog(&json!({"id":"c","method":"confirm","title":"Sure?",
            "message":"really"})));
        assert!(ui.open_dialog(&json!({"id":"e","method":"editor","title":"Edit","prefill":"x"})));
        assert!(!ui.open_dialog(&json!({"id":"n","method":"notify"})));
        // A PIUI reset leaves the host's sheets alone.
        ui.handle(&notify(json!({"v":1,"agentSeq":1,"instance":"a","op":"reset"})));
        let sheets = rx.borrow().typed_elements();
        assert_eq!(sheets.len(), 3);
        assert_eq!(sheets[0].key(), "pi:dialog-1");
        assert_eq!(sheets[1].text.as_deref(), Some("really"));
        assert_eq!(sheets[2].form().unwrap().0[0].initial_text(), "x");
        let pick = |value: &str| Some(json!({"fields":{"value":value}}));
        assert_eq!(ui.answer_dialog(&act("pi:dialog-1", "submit", pick("z"))), None);
        assert_eq!(
            ui.answer_dialog(&act("pi:dialog-1", "submit", pick("b"))),
            Some(json!({"type":"extension_ui_response","id":"s","value":"b"}))
        );
        assert_eq!(
            ui.answer_dialog(&act("pi:dialog-2", "no", None)),
            Some(json!({"type":"extension_ui_response","id":"c","confirmed":false}))
        );
        assert_eq!(ui.answer_dialog(&act("bash:jobs", "kill", None)), None);
        assert_eq!(
            ui.close_dialogs(true),
            vec![json!({"type":"extension_ui_response","id":"e","cancelled":true})]
        );
        assert!(rx.borrow().elements.is_empty());
        ui.open_dialog(&json!({"id":"t","method":"input","title":"t","timeout":0}));
        assert!(ui.next_deadline().is_some());
        assert_eq!(ui.close_dialogs(false).len(), 1);
    }

    #[test]
    fn plain_ui_requests_become_state_and_piui_never_becomes_a_notice() {
        let mut ui = ExtensionUi::new(None);
        assert_eq!(
            ui.handle(&json!({"method":"notify","message":"PIUI not json"})),
            Handled::Consumed
        );
        assert_eq!(
            ui.handle(&json!({"method":"notify","message":"saved","notifyType":"warning"})),
            Handled::Notice("saved".into())
        );
        ui.handle(&json!({"method":"setStatus","statusKey":"goal","statusText":"running"}));
        ui.handle(&json!({"method":"setStatus","statusKey":"mode","statusText":"Plan"}));
        ui.handle(&json!({"method":"setStatus","statusKey":"goal"}));
        ui.handle(&json!({"method":"setWidget","widgetKey":"btw","widgetLines":["a","b"]}));
        ui.handle(&json!({"method":"setTitle","title":"Refactor"}));
        ui.handle(&json!({"method":"set_editor_text","text":"draft"}));
        assert_eq!(ui.handle(&json!({"method":"editor","title":"x"})), Handled::Dialog);
        let state = ui.state();
        assert_eq!(state.notices.len(), 1);
        assert_eq!(state.notices[0].level, NoticeLevel::Warning);
        assert_eq!(state.statuses.len(), 1);
        assert_eq!(state.statuses[0].text, "Plan");
        assert_eq!(state.widgets[0].lines, vec!["a", "b"]);
        assert_eq!(state.title.as_ref().unwrap().text, "Refactor");
        assert_eq!(state.editor_text.as_ref().unwrap().text, "draft");
        ui.handle(&json!({"method":"setWidget","widgetKey":"btw"}));
        assert!(ui.state().widgets.is_empty());
        // Zeron's own chip comes and goes beside the extensions'.
        ui.host_status("zeron:retry", Some("Retrying · attempt 1/3".into()));
        assert_eq!(ui.state().statuses[1].text, "Retrying · attempt 1/3");
        ui.host_status("zeron:retry", None);
        assert_eq!(ui.state().statuses.len(), 1);
        // During a turn a notice is the turn's to record, never a toast.
        ui.set_in_turn(true);
        assert_eq!(
            ui.handle(&json!({"method":"notify","message":"in turn","notifyType":"error"})),
            Handled::Notice("in turn".into())
        );
        assert_eq!(ui.state().notices.len(), 1);
        assert_eq!(
            notice_level(&json!({"notifyType":"error"})),
            NoticeLevel::Error
        );
    }

    #[test]
    fn an_input_asking_for_a_credential_is_marked_secret() {
        let (tx, rx) = watch::channel(ExtensionUiState::default());
        let mut ui = ExtensionUi::new(Some(tx));
        let field = |rx: &watch::Receiver<ExtensionUiState>| {
            let sheet =
                serde_json::to_value(rx.borrow().elements.last().cloned().unwrap()).unwrap();
            sheet["sections"][0]["fields"][0].clone()
        };
        ui.open_dialog(&json!({"id":"k","method":"input","title":"RunInfra API key"}));
        assert_eq!(field(&rx)["secret"], true);
        ui.open_dialog(
            &json!({"id":"p","method":"input","title":"Login","placeholder":"PERSONAL_ACCESS_TOKEN"}),
        );
        assert_eq!(field(&rx)["secret"], true);
        ui.open_dialog(&json!({"id":"t","method":"input","title":"Token threshold"}));
        assert_eq!(field(&rx).get("secret"), None);
    }

    #[test]
    fn terminal_escapes_are_stripped_from_plain_ui_but_select_values_stay_verbatim() {
        let (tx, rx) = watch::channel(ExtensionUiState::default());
        let mut ui = ExtensionUi::new(Some(tx));
        let red = "\u{1b}[31mred\u{1b}[39m";
        let link = "\u{1b}]8;;https://x.test\u{7}docs\u{1b}]8;;\u{7}";
        assert_eq!(
            ui.handle(&json!({"method":"notify","message":format!("{red} {link}")})),
            Handled::Notice("red docs".into())
        );
        ui.handle(&json!({"method":"setStatus","statusKey":"mcp","statusText":red}));
        ui.handle(&json!({"method":"setWidget","widgetKey":"w","widgetLines":[red]}));
        ui.handle(&json!({"method":"setTitle","title":red}));
        let state = ui.state();
        assert_eq!(state.statuses[0].text, "red");
        assert_eq!(state.widgets[0].lines, vec!["red"]);
        assert_eq!(state.title.as_ref().unwrap().text, "red");

        ui.open_dialog(
            &json!({"id":"s","method":"select","title":red,"message":link,
            "options":[red, "plain"]}),
        );
        let sheet = serde_json::to_value(rx.borrow().elements.last().cloned().unwrap()).unwrap();
        assert_eq!(sheet["title"], "red");
        assert_eq!(sheet["text"], "docs");
        let choice = &sheet["sections"][0]["fields"][0]["options"][0];
        assert_eq!(choice["label"], "red");
        assert_eq!(choice["value"], red);
    }

    #[test]
    fn long_idle_notices_open_as_reports_and_fleets_count_their_running_jobs() {
        let (tx, rx) = watch::channel(ExtensionUiState::default());
        let mut ui = ExtensionUi::new(Some(tx));
        let report = "LSP servers\nrust: running\nts: stopped\npython: missing\n";
        assert_eq!(
            ui.handle(&json!({"method":"notify","message":report,"notifyType":"info"})),
            Handled::Consumed
        );
        assert!(ui.state().notices.is_empty());
        let screen = rx.borrow().typed_elements().remove(0);
        assert_eq!(screen.key(), "pi:report-1");
        assert_eq!(screen.title.as_deref(), Some("LSP servers"));
        assert_eq!(
            rx.borrow().elements[0]["sections"][0]["rows"][2],
            json!(["python", "missing"])
        );
        let close = ExtensionUiAction {
            element_id: "pi:report-1".into(),
            action_id: "close".into(),
            value: None,
            row_id: None,
        };
        assert!(ui.close_report(&close));
        assert!(rx.borrow().elements.is_empty());
        // Short notices stay toasts.
        assert_eq!(
            ui.handle(&json!({"method":"notify","message":"saved"})),
            Handled::Notice("saved".into())
        );
        // An empty editor write is the RPC read-back, not a draft.
        ui.handle(&json!({"method":"set_editor_text","text":""}));
        assert!(ui.state().editor_text.is_none());
        // A fleet's `active` is manage mode; its running entries are the work.
        let channel = |seq: u64, channel: &str, payload: Value| {
            notify(json!({"v":1,"agentSeq":seq,"instance":"a","op":"channel",
                "channel":channel,"payload":payload}))
        };
        ui.handle(&channel(1, "subagents:fleet", json!({"active":false,"entries":[
            {"key":"a","state":"running"},{"key":"b","state":"queued"},{"key":"c","state":"completed"}]})));
        ui.handle(&channel(
            2,
            "pi-goal:status",
            json!({"active":true,"status":"waiting"}),
        ));
        assert_eq!(ui.background_tasks(), 3);
        // A live workflow counts once whether it sends one channel or both;
        // a finished one keeps its state on the channel but is not work.
        ui.handle(&channel(
            3,
            "workflow:state",
            json!({"v":1,"run":{"id":"wf","status":"running","startedAt":1}}),
        ));
        assert_eq!(ui.background_tasks(), 4);
        ui.handle(&channel(4, "workflow:progress", json!({"active":true})));
        assert_eq!(ui.background_tasks(), 4);
        ui.handle(&channel(5, "workflow:progress", json!({"active":false})));
        ui.handle(&channel(
            6,
            "workflow:state",
            json!({"v":1,"run":{"id":"wf","status":"completed","startedAt":1,"endedAt":2}}),
        ));
        assert_eq!(ui.background_tasks(), 3);
        // Nobody watching: the channels still count.
        ui.disconnect();
        assert_eq!(ui.background_tasks(), 3);
        assert!(asks_for_secret(
            &json!({"title":"OpenRouter key","placeholder":"sk-or-v1-…"})
        ));
        assert!(!asks_for_secret(&json!({"title":"Token threshold"})));
    }
}
