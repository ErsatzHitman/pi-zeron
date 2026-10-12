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
use std::{collections::HashMap, time::Instant};
use tokio::sync::watch;
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

/// What [`ExtensionUi::handle`] did with an `extension_ui_request`.
#[derive(Debug, PartialEq)]
pub(super) enum Handled {
    /// PIUI or plain UI state; nothing else to do.
    Consumed,
    /// A plain (non-PIUI) `notify`, already recorded as a notice.
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
}

pub(super) fn element_key(element: &Value) -> String {
    format!(
        "{}:{}",
        element["ns"].as_str().unwrap_or(""),
        element["id"].as_str().unwrap_or("")
    )
}

/// The prompt text that delivers `event` to the extensions.
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

pub(super) fn bye() -> Value {
    json!({"type":"bye"})
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
    pub fn new(sink: Option<watch::Sender<ExtensionUiState>>) -> Self {
        Self {
            sink,
            ..Default::default()
        }
    }

    #[cfg(test)]
    pub fn state(&self) -> &ExtensionUiState {
        &self.state
    }

    fn publish(&mut self) {
        self.state.revision += 1;
        if let Some(sink) = &self.sink {
            sink.send_replace(self.state.clone());
        }
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
                let level = match frame["notifyType"].as_str() {
                    Some("warning") => NoticeLevel::Warning,
                    Some("error") => NoticeLevel::Error,
                    _ => NoticeLevel::Info,
                };
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
                    Some(value) => match statuses.iter_mut().find(|s| s.key == key) {
                        Some(status) => status.text = value.into(),
                        None => statuses.push(ExtensionStatus {
                            key,
                            text: value.into(),
                        }),
                    },
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
                                .map(|line| line.as_str().unwrap_or("").to_owned())
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
                    text: text("title"),
                });
                self.publish();
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
            self.state.channels.clear();
            self.chunks.clear();
            self.publish();
        }
    }

    /// The Pi process ended: nothing it drew survives it.
    pub fn closed(&mut self) {
        let revision = self.state.revision;
        self.state = ExtensionUiState {
            revision,
            ..Default::default()
        };
        self.keys.clear();
        self.chunks.clear();
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
        let count = |channel: &str| match &self.state.channels.get(channel).map(|p| &p["active"]) {
            Some(Value::Number(n)) => n.as_u64().unwrap_or(0) as usize,
            Some(Value::Bool(true)) => 1,
            Some(Value::Array(items)) => items.len(),
            _ => 0,
        };
        count("subagents:fleet") + count("bash-bg:fleet") + count("workflow:progress")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    }
}
