//! The session panel's Sub-agents section, kept pure so its shape is
//! testable without a window. It gathers everything working under the
//! open chat:
//!
//! - **Pi sub-agents**: the `subagents` extension's fleet roster, with its
//!   View and Kill actions and each job's start time (from the
//!   `subagents:fleet` channel) for a live elapsed timer.
//! - **Subagents** a harness spawned (the transcript's spawn chips).
//! - **Sub-sessions**: the chat this one hangs off, and its own side chats
//!   (forks, and chats an agent started through the Zeron MCP server).
//!
//! Running work leads; whatever has settled folds into Previous agents.
//!
//! The Background section is built here too: the bash commands Pi moved to
//! the background (the `bash-background` extension's `bash:jobs` roster,
//! with each job's start time from the `bash-bg:fleet` channel).

use chrono::{DateTime, Utc};
use gpui::SharedString;
use zeron_doc::{MessagePart, SubagentStatus};
use zeron_proto::extension_ui::{
    ExtensionUiState, PiUiAction, PiUiElement, PiUiKind, PiUiRowState,
};
use zeron_proto::{Chat, ChatIndicator};

use crate::state::AppState;

/// The Pi extension that publishes the fleet roster, and its live channel.
const FLEET_NS: &str = "subagents";
const FLEET_CHANNEL: &str = "subagents:fleet";
/// The extension that publishes background bash commands, and its channel.
const BASH_NS: &str = "bash";
const BASH_CHANNEL: &str = "bash-bg:fleet";

/// What a row opens or controls.
#[derive(Debug, Clone, PartialEq)]
pub(in crate::shell) enum AgentKind {
    /// A Pi sub-agent: the fleet element's key, the row's id, and the
    /// roster actions it offers.
    Fleet {
        element: String,
        row: String,
        view: Option<PiUiAction>,
        stop: Option<PiUiAction>,
    },
    /// A bash command Pi moved to the background: the roster element's
    /// key, the row's id and its Kill. Its output is not published, so the
    /// row opens nothing.
    Background {
        element: String,
        row: String,
        stop: Option<PiUiAction>,
    },
    /// A harness subagent's transcript doc.
    Subagent { doc_id: String, frozen: bool },
    /// A side chat of the open chat.
    SideChat {
        chat_id: String,
        device_id: String,
        change_request: Option<zeron_proto::ChangeRequestSummary>,
    },
    /// The chat the open one hangs off.
    Parent { chat_id: String },
}

#[derive(Debug, Clone, PartialEq)]
pub(in crate::shell) struct AgentRow {
    pub kind: AgentKind,
    pub title: SharedString,
    pub status: ChatIndicator,
    /// When running work started, for its live timer.
    pub started: Option<DateTime<Utc>>,
    /// A settled row's trailing label (a side chat's last activity).
    pub trailing: Option<SharedString>,
    /// The roster's detail line (model, tokens, latest activity).
    pub detail: Option<SharedString>,
}

impl AgentRow {
    /// A stable id for the row's elements.
    pub fn key(&self) -> String {
        match &self.kind {
            AgentKind::Fleet { row, .. } => format!("fleet-{row}"),
            AgentKind::Background { row, .. } => format!("bash-{row}"),
            AgentKind::Subagent { doc_id, .. } => format!("subagent-{doc_id}"),
            AgentKind::SideChat { chat_id, .. } => format!("chat-{chat_id}"),
            AgentKind::Parent { chat_id } => format!("parent-{chat_id}"),
        }
    }

    pub fn running(&self) -> bool {
        matches!(
            self.status,
            ChatIndicator::Working | ChatIndicator::AwaitingInput
        )
    }
}

/// The section's rows: sub-sessions (parent first, then side chats), the
/// sub-agents at work, and the settled ones.
#[derive(Debug, Default, Clone, PartialEq)]
pub(super) struct AgentList {
    pub sessions: Vec<AgentRow>,
    pub active: Vec<AgentRow>,
    pub previous: Vec<AgentRow>,
    /// Background commands, for their own section: not counted as
    /// sub-agents.
    pub background: Vec<AgentRow>,
}

impl AgentList {
    /// Work underway: running sub-agents and side chats. The parent link is
    /// a way back, not work under this chat.
    pub fn running(&self) -> usize {
        self.active.len()
            + self
                .sessions
                .iter()
                .filter(|row| row.running() && !matches!(row.kind, AgentKind::Parent { .. }))
                .count()
    }

    /// Whether Sub-agents earns its section: work underway, or the chat's
    /// way back to the one it hangs off.
    pub fn is_active(&self) -> bool {
        self.running() > 0
            || self
                .sessions
                .iter()
                .any(|row| matches!(row.kind, AgentKind::Parent { .. }))
    }

    /// When the longest-running work started, for the section's seat.
    pub fn since(&self) -> Option<DateTime<Utc>> {
        self.active.iter().filter_map(|row| row.started).min()
    }

    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.sessions.is_empty() && self.active.is_empty() && self.previous.is_empty()
    }
}

/// Whether Background earns its section: a command still running.
pub(super) fn background_active(rows: &[AgentRow]) -> bool {
    rows.iter().any(AgentRow::running)
}

/// When the longest-running background command started.
pub(super) fn background_since(rows: &[AgentRow]) -> Option<DateTime<Utc>> {
    rows.iter()
        .filter(|row| row.running())
        .filter_map(|row| row.started)
        .min()
}

/// The panel renders far more often than the chat list or the transcript
/// change, so what the Sub-agents list derives from them alone is kept
/// between renders (as `TodoCache` keeps the checklist). Only clock- and
/// session-free data lives here: a row's status and "5m ago" are read fresh
/// every render, since sessions have no revision to key on.
#[derive(Default)]
pub(super) struct AgentListCache {
    links: Option<(LinksKey, ChatLinks)>,
    subagents: Option<(SubagentsKey, Vec<SubagentRow>)>,
}

/// The chat, `AppState::link_roots_revision` and the chat count (the
/// revision moves whenever the list is replaced).
type LinksKey = (String, u64, usize);
/// The chat, `AppState::transcript_revision`, the entry count, and whether
/// the chat is the selected one (the transcript is only ever its).
type SubagentsKey = (String, u64, usize, bool);

/// Where a chat sits among the others, from the chat list alone.
struct ChatLinks {
    /// The orchestrator above a chat in a tree, when it is not the chat.
    root: Option<String>,
    /// How far below that orchestrator the chat is.
    level: usize,
    /// The chat's live side chats, most recent activity first.
    children: Vec<String>,
}

impl ChatLinks {
    fn scan(state: &AppState, chat_id: &str) -> Self {
        let root = zeron_proto::orchestrator::root_of(&state.chats, chat_id)
            .filter(|root| root.id != chat_id)
            .map(|root| root.id.clone());
        let level = zeron_proto::orchestrator::depth(&state.chats, chat_id).unwrap_or(0);
        let mut children: Vec<(DateTime<Utc>, &str)> = state
            .chats
            .iter()
            .filter(|chat| !chat.archived && chat.parent_chat_id.as_deref() == Some(chat_id))
            .filter(|chat| !chat.is_btw() && !tree_member(state, &chat.id))
            .map(|chat| {
                (
                    chat.last_message_at.unwrap_or(chat.created_at),
                    chat.id.as_str(),
                )
            })
            .collect();
        children.sort_by_key(|(activity, _)| std::cmp::Reverse(*activity));
        Self {
            root,
            level,
            children: children.into_iter().map(|(_, id)| id.to_owned()).collect(),
        }
    }
}

impl AgentListCache {
    fn links(&mut self, state: &AppState, chat_id: &str) -> &ChatLinks {
        let key = (
            chat_id.to_owned(),
            state.link_roots_revision,
            state.chats.len(),
        );
        let kept = self.links.take().filter(|(cached, _)| *cached == key);
        let (_, links) = self
            .links
            .insert(kept.unwrap_or_else(|| (key, ChatLinks::scan(state, chat_id))));
        links
    }

    fn subagents(&mut self, state: &AppState, chat_id: &str) -> &[SubagentRow] {
        let key = (
            chat_id.to_owned(),
            state.transcript_revision,
            state.transcript.len(),
            state.selected_chat.as_deref() == Some(chat_id),
        );
        let kept = self.subagents.take().filter(|(cached, _)| *cached == key);
        let (_, rows) = self
            .subagents
            .insert(kept.unwrap_or_else(|| (key, subagent_rows(state, chat_id))));
        rows
    }
}

pub(super) fn agent_list(state: &AppState, chat: &Chat, now: DateTime<Utc>) -> AgentList {
    agent_list_in(&mut AgentListCache::default(), state, chat, now)
}

/// [`agent_list`], reusing what `cache` kept from the last render.
pub(super) fn agent_list_in(
    cache: &mut AgentListCache,
    state: &AppState,
    chat: &Chat,
    now: DateTime<Utc>,
) -> AgentList {
    let mut list = AgentList::default();
    // In an orchestrator's tree, the way up names the orchestrator too, and
    // where this session sits below it.
    let links = cache.links(state, &chat.id);
    let level = links.level;
    let root = links
        .root
        .as_deref()
        .and_then(|id| state.chats.iter().find(|c| c.id == id));
    let way_up = |row: &Chat, label: String| AgentRow {
        kind: AgentKind::Parent {
            chat_id: row.id.clone(),
        },
        title: child_chat_title(row, "Untitled chat").into(),
        status: state.display_status_for(row, now),
        started: None,
        trailing: Some(label.into()),
        detail: None,
    };
    if let Some(root) = root.filter(|root| chat.parent_chat_id.as_deref() != Some(root.id.as_str()))
    {
        list.sessions.push(way_up(
            root,
            format!("Orchestrator · you are level {level}"),
        ));
    }
    if let Some(parent) = chat
        .parent_chat_id
        .as_deref()
        .and_then(|id| state.chats.iter().find(|c| c.id == id))
    {
        let label = if root.is_some_and(|root| root.id == parent.id) {
            format!("Orchestrator · you are level {level}")
        } else {
            "Parent".to_owned()
        };
        list.sessions.push(way_up(parent, label));
    }
    list.sessions.extend(
        links
            .children
            .iter()
            .filter_map(|id| state.chats.iter().find(|c| c.id == *id))
            .map(|child| child_chat_row(state, child, now)),
    );
    if let Some(ui) = state.extension_ui.of(&chat.id) {
        let elements = ui.typed_elements();
        for row in roster_agent_rows(ui, &elements, FLEET_NS, FLEET_CHANNEL, false) {
            if row.running() {
                list.active.push(row);
            } else {
                list.previous.push(row);
            }
        }
        list.background = roster_agent_rows(ui, &elements, BASH_NS, BASH_CHANNEL, true);
    }
    for row in cache.subagents(state, &chat.id) {
        let running = row.status == Some(SubagentStatus::Running);
        let agent = AgentRow {
            kind: AgentKind::Subagent {
                frozen: row.frozen(),
                doc_id: row.doc_id.clone(),
            },
            title: row.title.clone(),
            status: match row.status {
                Some(SubagentStatus::Running) => ChatIndicator::Working,
                Some(SubagentStatus::Done) => ChatIndicator::Completed,
                Some(SubagentStatus::Failed) => ChatIndicator::Errored,
                None => ChatIndicator::Idle,
            },
            started: running.then_some(row.spawned_at),
            trailing: None,
            detail: None,
        };
        if running {
            list.active.push(agent);
        } else {
            list.previous.push(agent);
        }
    }
    list
}

/// A roster's rows (the Pi fleet's, or the background commands'), in the
/// roster's own tree order. A job's start time comes from the extension's
/// channel; the roster only carries its state.
fn roster_agent_rows(
    ui: &ExtensionUiState,
    elements: &[PiUiElement],
    ns: &str,
    channel: &str,
    background: bool,
) -> Vec<AgentRow> {
    let Some(element) = elements
        .iter()
        .find(|e| e.ns == ns && e.kind == PiUiKind::Roster)
    else {
        return Vec::new();
    };
    let entries = ui
        .channels
        .get(channel)
        .and_then(|fleet| fleet["entries"].as_array().cloned())
        .unwrap_or_default();
    let started = |key: &str| {
        entries
            .iter()
            .find(|entry| entry["key"] == key)
            .and_then(|entry| entry["startedAt"].as_i64())
            .and_then(DateTime::<Utc>::from_timestamp_millis)
    };
    roster_rows(element, started, background)
}

fn roster_rows(
    element: &PiUiElement,
    started: impl Fn(&str) -> Option<DateTime<Utc>>,
    background: bool,
) -> Vec<AgentRow> {
    let key = element.key();
    element
        .rows
        .iter()
        .map(|row| {
            let action = |id: &str| {
                row.actions
                    .iter()
                    .find(|action| action.id == id && !action.disabled)
                    .cloned()
            };
            let status = match row.state {
                Some(PiUiRowState::Running) => ChatIndicator::Working,
                Some(PiUiRowState::Waiting) => ChatIndicator::AwaitingInput,
                Some(PiUiRowState::Done) => ChatIndicator::Completed,
                Some(PiUiRowState::Error) => ChatIndicator::Errored,
                _ => ChatIndicator::Idle,
            };
            let running = matches!(
                status,
                ChatIndicator::Working | ChatIndicator::AwaitingInput
            );
            let kind = if background {
                AgentKind::Background {
                    element: key.clone(),
                    row: row.id.clone(),
                    stop: action("kill"),
                }
            } else {
                AgentKind::Fleet {
                    element: key.clone(),
                    row: row.id.clone(),
                    view: action("view"),
                    stop: action("kill"),
                }
            };
            AgentRow {
                kind,
                title: row.label.clone().into(),
                status,
                started: running.then(|| started(&row.id)).flatten(),
                trailing: None,
                detail: row.detail.clone().map(Into::into),
            }
        })
        .collect()
}

/// A spawn chip of the open chat's transcript.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct SubagentRow {
    pub doc_id: String,
    pub title: SharedString,
    pub status: Option<SubagentStatus>,
    /// When the latest turn that spawned (or steered) it was written — the
    /// closest thing a subagent has to a last-updated time.
    pub spawned_at: DateTime<Utc>,
}

impl SubagentRow {
    /// A settled subagent is frozen: the tab reads its blob first.
    pub fn frozen(&self) -> bool {
        matches!(
            self.status,
            Some(SubagentStatus::Done) | Some(SubagentStatus::Failed)
        )
    }
}

/// The open chat's subagents, one row per subagent doc: running ones first,
/// longest-running leading, then the settled ones most recently updated first
/// (later spawns lead within one turn).
/// Only genuine spawn chips with a stamped doc ref qualify — the chip IS the
/// index (there is no listing endpoint), and a stray ref on a non-Agent tool
/// must not surface as a phantom subagent.
pub(super) fn subagent_rows(state: &AppState, chat_id: &str) -> Vec<SubagentRow> {
    if state.selected_chat.as_deref() != Some(chat_id) {
        return Vec::new();
    }
    let mut rows: Vec<SubagentRow> = Vec::new();
    // Each doc's slot in `rows`: a long transcript can steer the same
    // subagent many times.
    let mut slots: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for entry in &state.transcript {
        let spawned_at =
            DateTime::<Utc>::from_timestamp_millis(entry.created_at).unwrap_or_else(Utc::now);
        for part in &entry.parts {
            let MessagePart::Tool {
                call,
                subagent_ref: Some(doc_id),
                subagent_status,
                ..
            } = part
            else {
                continue;
            };
            if !call.is_subagent_spawn() {
                continue;
            }
            let row = SubagentRow {
                doc_id: doc_id.clone(),
                title: crate::transcript::subagent_tab_title(call),
                status: *subagent_status,
                spawned_at,
            };
            match slots.get(&row.doc_id) {
                // A reopened (steered) subagent updates its row in place.
                Some(&slot) => rows[slot] = row,
                None => {
                    slots.insert(row.doc_id.clone(), rows.len());
                    rows.push(row);
                }
            }
        }
    }
    // Stable sort over the reversed spawn order: ties keep the later spawn
    // on top.
    rows.reverse();
    rows.sort_by_key(|row| std::cmp::Reverse(row.spawned_at));
    // Running subagents lead, longest-running first: one started long ago is
    // buried under everything spawned since, and is the one to find and steer.
    let (mut running, settled): (Vec<_>, Vec<_>) = rows
        .into_iter()
        .partition(|row| row.status == Some(SubagentStatus::Running));
    // Ties keep spawn order, like the group's oldest-first order.
    running.reverse();
    running.sort_by_key(|row| row.spawned_at);
    running.extend(settled);
    running
}

/// A side chat's row. Which chats are listed, and in what order, is
/// [`ChatLinks::scan`]: the live (unarchived) children of the open chat, most
/// recent activity first — the same order the sidebar's Sessions list keeps.
/// An orchestrator's members are its tree (the Orchestrator tab), not
/// sub-agents of the session that started them. A btw aside is the user's own
/// question beside the chat, not work under it, so it is never listed.
fn child_chat_row(state: &AppState, chat: &Chat, now: DateTime<Utc>) -> AgentRow {
    let activity = chat.last_message_at.unwrap_or(chat.created_at);
    AgentRow {
        kind: AgentKind::SideChat {
            chat_id: chat.id.clone(),
            device_id: chat.device_id.clone(),
            change_request: state.change_request_for_chat(chat).cloned(),
        },
        title: child_chat_title(chat, "btw").into(),
        status: state.display_status_for(chat, now),
        started: None,
        trailing: Some(zeron_proto::view::format_time_ago(activity, now).into()),
        detail: None,
    }
}

/// A session in an orchestrator's tree, below its root.
fn tree_member(state: &AppState, chat_id: &str) -> bool {
    matches!(
        zeron_proto::orchestrator::Role::of(&state.chats, chat_id),
        zeron_proto::orchestrator::Role::Member { .. }
    )
}

/// A chat titles itself on its first turn; until then the preview or a
/// placeholder stands in.
pub(super) fn child_chat_title(chat: &Chat, placeholder: &str) -> String {
    chat.title
        .clone()
        .or_else(|| chat.last_message_preview.clone())
        .unwrap_or_else(|| placeholder.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use zeron_doc::{MessageRole, MessageStatus, SessionMessageEntry};
    use zeron_proto::ToolCall;

    fn chat(id: &str, parent: Option<&str>, minutes_ago: i64) -> Chat {
        serde_json::from_value(serde_json::json!({
            "id": id,
            "deviceId": "dev",
            "archived": false,
            "createdAt": Utc::now() - chrono::Duration::minutes(minutes_ago),
            "parentChatId": parent,
        }))
        .unwrap()
    }

    fn spawn(
        id: &str,
        name: &str,
        doc: Option<&str>,
        status: Option<SubagentStatus>,
    ) -> MessagePart {
        MessagePart::Tool {
            id: id.into(),
            call: ToolCall::Unknown {
                name: name.into(),
                input: Some(serde_json::json!({ "description": "verify the marker pipeline" })),
            },
            is_error: false,
            resolved: true,
            output: None,
            diff: None,
            output_ref: None,
            output_bytes: None,
            diff_ref: None,
            diff_stats: None,
            subagent_ref: doc.map(str::to_owned),
            subagent_status: status,
            subagent_tail: None,
        }
    }

    fn entry(id: &str, minutes_ago: i64, parts: Vec<MessagePart>) -> SessionMessageEntry {
        SessionMessageEntry {
            duration_ms: None,
            id: id.into(),
            role: MessageRole::Assistant,
            parts,
            created_at: (Utc::now() - chrono::Duration::minutes(minutes_ago)).timestamp_millis(),
            device_id: "dev".into(),
            status: Some(MessageStatus::Complete),
            continuation_of: None,
        }
    }

    #[test]
    fn subagent_rows_list_only_stamped_spawn_chips_of_the_selected_chat() {
        let mut state = AppState::new();
        state.selected_chat = Some("main".into());
        state.transcript = vec![entry(
            "e1",
            3,
            vec![
                spawn(
                    "t1",
                    "Agent: verify",
                    Some("main--sub--t1"),
                    Some(SubagentStatus::Running),
                ),
                // No doc ref yet: the engine stamps it asynchronously.
                spawn("t2", "Agent: later", None, None),
                // A stray ref on a non-spawn tool never surfaces.
                spawn(
                    "t3",
                    "Read",
                    Some("main--sub--t3"),
                    Some(SubagentStatus::Done),
                ),
                spawn(
                    "t4",
                    "Agent: done",
                    Some("main--sub--t4"),
                    Some(SubagentStatus::Done),
                ),
            ],
        )];
        let rows = subagent_rows(&state, "main");
        assert_eq!(
            rows.iter().map(|r| r.doc_id.as_str()).collect::<Vec<_>>(),
            ["main--sub--t1", "main--sub--t4"]
        );
        // The bare task, genus stripped — the same title the tab wears.
        assert_eq!(rows[0].title.as_ref(), "verify");
        assert!(rows[1].frozen());
        assert!(!rows[0].frozen());
        assert!((Utc::now() - rows[0].spawned_at).num_minutes() >= 2);
        assert!(subagent_rows(&state, "other").is_empty());
    }

    #[test]
    fn running_subagents_lead_longest_running_first() {
        let sub = |id: &str, status| {
            spawn(
                id,
                &format!("Agent: {id}"),
                Some(&format!("main--sub--{id}")),
                Some(status),
            )
        };
        let mut state = AppState::new();
        state.selected_chat = Some("main".into());
        state.transcript = vec![
            entry("e1", 90, vec![sub("old-run", SubagentStatus::Running)]),
            entry("e2", 60, vec![sub("old-done", SubagentStatus::Done)]),
            entry("e3", 30, vec![sub("mid-run", SubagentStatus::Running)]),
            entry("e4", 20, vec![sub("mid-fail", SubagentStatus::Failed)]),
            entry("e5", 10, vec![sub("new-done", SubagentStatus::Done)]),
            entry("e6", 5, vec![sub("new-run", SubagentStatus::Running)]),
            // `old-done` is steered again: its row moves up with the newer turn.
            entry("e7", 2, vec![sub("old-done", SubagentStatus::Done)]),
        ];
        let ids: Vec<_> = subagent_rows(&state, "main")
            .into_iter()
            .map(|r| r.doc_id)
            .collect();
        assert_eq!(
            ids,
            [
                "main--sub--old-run",
                "main--sub--mid-run",
                "main--sub--new-run",
                "main--sub--old-done",
                "main--sub--new-done",
                "main--sub--mid-fail",
            ]
        );
    }

    #[test]
    fn sub_sessions_list_the_parent_then_live_side_chats_newest_first() {
        let mut state = AppState::new();
        let mut archived = chat("old", Some("main"), 1);
        archived.archived = true;
        let mut titled = chat("b", Some("main"), 30);
        titled.title = Some("Investigate caching".into());
        let mut root = chat("root", None, 90);
        root.title = Some("Release prep".into());
        state.apply_chats(vec![
            root,
            chat("main", Some("root"), 60),
            chat("a", Some("main"), 5),
            titled,
            chat("unrelated", Some("elsewhere"), 2),
            archived,
        ]);
        let main = state.chats.iter().find(|c| c.id == "main").unwrap().clone();
        let list = agent_list(&state, &main, Utc::now());
        assert_eq!(
            list.sessions.iter().map(AgentRow::key).collect::<Vec<_>>(),
            ["parent-root", "chat-a", "chat-b"]
        );
        assert_eq!(list.sessions[0].title.as_ref(), "Release prep");
        assert_eq!(list.sessions[1].title.as_ref(), "btw");
        assert_eq!(list.sessions[2].title.as_ref(), "Investigate caching");
        assert_eq!(list.running(), 0);
        assert!(list.active.is_empty() && list.previous.is_empty());
    }

    #[test]
    fn a_btw_question_is_never_a_sub_agent() {
        let side = chat("side", Some("main"), 2);
        let btw = chat("btw-question", Some("main"), 1);
        assert!(btw.is_btw() && !side.is_btw());
        let mut state = AppState::new();
        state.apply_chats(vec![chat("main", None, 60), side, btw]);
        let main = state.chats.iter().find(|c| c.id == "main").unwrap().clone();
        let list = agent_list(&state, &main, Utc::now());
        assert_eq!(
            list.sessions.iter().map(AgentRow::key).collect::<Vec<_>>(),
            ["chat-side"],
            "the side chat is listed, the btw aside is not"
        );
    }

    #[test]
    fn the_agent_cache_rescans_only_when_a_revision_moves() {
        let mut state = AppState::new();
        state.apply_chats(vec![
            chat("main", None, 60),
            chat("a", Some("main"), 5),
            chat("b", Some("main"), 30),
        ]);
        let main = state.chats.iter().find(|c| c.id == "main").unwrap().clone();
        let keys = |list: &AgentList| list.sessions.iter().map(AgentRow::key).collect::<Vec<_>>();
        let mut cache = AgentListCache::default();
        let now = Utc::now();
        assert_eq!(
            keys(&agent_list_in(&mut cache, &state, &main, now)),
            ["chat-a", "chat-b"]
        );
        // Archiving without republishing the chat list: the kept links answer.
        state
            .chats
            .iter_mut()
            .find(|c| c.id == "a")
            .unwrap()
            .archived = true;
        assert_eq!(
            keys(&agent_list_in(&mut cache, &state, &main, now)),
            ["chat-a", "chat-b"]
        );
        // A republished list moves the revision, and the links are rescanned.
        state.link_roots_revision += 1;
        assert_eq!(
            keys(&agent_list_in(&mut cache, &state, &main, now)),
            ["chat-b"]
        );
        // The cache answers exactly what a fresh scan does.
        assert_eq!(
            agent_list_in(&mut cache, &state, &main, now),
            agent_list(&state, &main, now)
        );
    }

    #[test]
    fn the_agent_cache_keeps_titles_and_clocks_outside_the_kept_links() {
        let mut state = AppState::new();
        state.apply_chats(vec![chat("main", None, 60), chat("b", Some("main"), 5)]);
        let main = state.chats.iter().find(|c| c.id == "main").unwrap().clone();
        let mut cache = AgentListCache::default();
        let now = Utc::now();
        let before = agent_list_in(&mut cache, &state, &main, now);
        assert_eq!(before.sessions[0].title.as_ref(), "btw");
        // Renamed with the revision unmoved, and an hour later: the row is
        // still read fresh from the chat and the clock.
        state.chats.iter_mut().find(|c| c.id == "b").unwrap().title = Some("Renamed".into());
        let later = agent_list_in(&mut cache, &state, &main, now + chrono::Duration::hours(2));
        assert_eq!(later.sessions[0].title.as_ref(), "Renamed");
        assert_ne!(later.sessions[0].trailing, before.sessions[0].trailing);
    }

    #[test]
    fn the_agent_cache_rescans_the_transcript_only_when_it_changes() {
        let mut state = AppState::new();
        state.apply_chats(vec![chat("main", None, 10)]);
        state.selected_chat = Some("main".into());
        let spawned = |status| {
            vec![entry(
                "e1",
                3,
                vec![spawn(
                    "t1",
                    "Agent: verify",
                    Some("main--sub--t1"),
                    Some(status),
                )],
            )]
        };
        let main = state.chats[0].clone();
        let mut cache = AgentListCache::default();
        state.transcript = spawned(SubagentStatus::Running);
        let list = agent_list_in(&mut cache, &state, &main, Utc::now());
        assert_eq!((list.active.len(), list.previous.len()), (1, 0));
        // Same revision and length: the kept rows answer.
        state.transcript = spawned(SubagentStatus::Done);
        let list = agent_list_in(&mut cache, &state, &main, Utc::now());
        assert_eq!((list.active.len(), list.previous.len()), (1, 0));
        state.transcript_revision += 1;
        let list = agent_list_in(&mut cache, &state, &main, Utc::now());
        assert_eq!((list.active.len(), list.previous.len()), (0, 1));
    }

    #[test]
    fn subagent_rows_dedupe_reopened_spawns_in_place() {
        let doc = |n: usize| format!("main--sub--t{n}");
        let mut state = AppState::new();
        state.selected_chat = Some("main".into());
        // 200 subagents, each steered up to 24 more times across 5,000
        // turns: the last write to a doc is the one that shows.
        state.transcript = (0..5_000)
            .map(|n| {
                let status = if n < 4_800 {
                    SubagentStatus::Running
                } else {
                    SubagentStatus::Done
                };
                entry(
                    &format!("e{n}"),
                    10_000 - n as i64,
                    vec![spawn(
                        &format!("t{n}"),
                        "Agent: verify",
                        Some(&doc(n % 200)),
                        Some(status),
                    )],
                )
            })
            .collect();
        let rows = subagent_rows(&state, "main");
        assert_eq!(rows.len(), 200);
        let mut docs: Vec<_> = rows.iter().map(|row| row.doc_id.clone()).collect();
        docs.sort();
        docs.dedup();
        assert_eq!(docs.len(), 200);
        assert!(rows.iter().all(SubagentRow::frozen));
    }

    #[test]
    fn a_tree_session_names_its_orchestrator_and_level() {
        let mut state = AppState::new();
        let root = zeron_proto::orchestrator::new_root_chat_id();
        state.apply_chats(vec![
            chat(&root, None, 90),
            chat("member", Some(&root), 60),
            chat("nested", Some("member"), 30),
        ]);
        let row = |id: &str| state.chats.iter().find(|c| c.id == id).unwrap().clone();
        let nested = agent_list(&state, &row("nested"), Utc::now());
        assert_eq!(
            nested
                .sessions
                .iter()
                .map(AgentRow::key)
                .collect::<Vec<_>>(),
            [format!("parent-{root}"), "parent-member".to_owned()]
        );
        assert_eq!(
            nested.sessions[0].trailing.as_deref(),
            Some("Orchestrator · you are level 2")
        );
        assert_eq!(nested.sessions[1].trailing.as_deref(), Some("Parent"));
        let member = agent_list(&state, &row("member"), Utc::now());
        let way_up = member
            .sessions
            .iter()
            .filter(|row| matches!(row.kind, AgentKind::Parent { .. }))
            .count();
        assert_eq!(way_up, 1, "its parent is the orchestrator");
        assert_eq!(
            member.sessions[0].trailing.as_deref(),
            Some("Orchestrator · you are level 1")
        );
    }

    #[test]
    fn an_orchestrators_sessions_are_not_its_sub_agents() {
        let mut state = AppState::new();
        let root = zeron_proto::orchestrator::new_root_chat_id();
        let mut first = chat("first", Some(&root), 60);
        first.last_message_at = Some(Utc::now());
        state.apply_chats(vec![
            chat(&root, None, 90),
            first,
            chat("second", Some(&root), 50),
            chat("nested", Some("first"), 30),
            chat("btw-aside", Some(&root), 10),
        ]);
        let row = |id: &str| state.chats.iter().find(|c| c.id == id).unwrap().clone();
        let orchestrator = agent_list(&state, &row(&root), Utc::now());
        assert_eq!(
            orchestrator
                .sessions
                .iter()
                .map(AgentRow::key)
                .collect::<Vec<_>>(),
            Vec::<String>::new(),
            "the members it launched are its tree and a btw is an aside: neither is a sub-agent"
        );
        assert_eq!(orchestrator.running(), 0);
        let member = agent_list(&state, &row("first"), Utc::now());
        assert!(
            member
                .sessions
                .iter()
                .all(|row| matches!(row.kind, AgentKind::Parent { .. })),
            "nor are a member's own"
        );
    }

    #[test]
    fn a_parent_link_keeps_sub_agents_up_without_running_work() {
        let mut state = AppState::new();
        state.apply_chats(vec![chat("root", None, 90), chat("main", Some("root"), 60)]);
        let main = state.chats.iter().find(|c| c.id == "main").unwrap().clone();
        let list = agent_list(&state, &main, Utc::now());
        assert_eq!(list.running(), 0);
        assert!(list.is_active());
    }

    #[test]
    fn a_top_level_chat_with_only_settled_side_chats_is_not_active() {
        let mut state = AppState::new();
        state.apply_chats(vec![chat("main", None, 60), chat("a", Some("main"), 5)]);
        let main = state.chats.iter().find(|c| c.id == "main").unwrap().clone();
        let list = agent_list(&state, &main, Utc::now());
        assert_eq!(list.sessions.len(), 1);
        assert!(!list.is_active());
        assert_eq!(list.since(), None);
    }

    #[test]
    fn only_running_background_commands_are_active() {
        let row = |status, started| AgentRow {
            kind: AgentKind::Background {
                element: "bash:jobs".into(),
                row: "r".into(),
                stop: None,
            },
            title: "cmd".into(),
            status,
            started,
            trailing: None,
            detail: None,
        };
        let early = DateTime::<Utc>::from_timestamp(10, 0);
        let late = DateTime::<Utc>::from_timestamp(20, 0);
        assert!(!background_active(&[row(ChatIndicator::Completed, None)]));
        let rows = [
            row(ChatIndicator::Completed, None),
            row(ChatIndicator::Working, late),
            row(ChatIndicator::Working, early),
        ];
        assert!(background_active(&rows));
        assert_eq!(background_since(&rows), early);
    }

    #[test]
    fn pi_fleet_rows_split_into_active_and_previous_with_their_actions() {
        let mut state = AppState::new();
        state.apply_chats(vec![chat("main", None, 10)]);
        state.selected_chat = Some("main".into());
        state.extension_ui.chat = Some("main".into());
        let started = Utc::now() - chrono::Duration::seconds(41);
        state.extension_ui.state.elements = vec![serde_json::json!({
            "id": "fleet", "ns": "subagents", "kind": "roster", "placement": "pinned",
            "title": "Subagents · 1 active · 1 finished",
            "rows": [
                {"id": "j1", "label": "Architecture Scout", "state": "running",
                 "detail": "openai/gpt-5 · 1200 tokens",
                 "actions": [{"id": "view", "label": "View"},
                             {"id": "kill", "label": "Kill", "variant": "danger",
                              "confirm": "Kill this sub-agent?"},
                             {"id": "select", "label": "Select"}]},
                {"id": "j2", "label": "Test Runner", "state": "done",
                 "actions": [{"id": "view", "label": "View"}, {"id": "select", "label": "Select"}]}
            ]
        })];
        state.extension_ui.state.channels.insert(
            "subagents:fleet".into(),
            serde_json::json!({"entries": [
                {"key": "j1", "startedAt": started.timestamp_millis()},
                {"key": "j2", "startedAt": started.timestamp_millis()}
            ]}),
        );
        let main = state.chats[0].clone();
        let list = agent_list(&state, &main, Utc::now());
        assert_eq!(list.running(), 1);
        let scout = &list.active[0];
        assert_eq!(scout.title.as_ref(), "Architecture Scout");
        assert_eq!(scout.status, ChatIndicator::Working);
        assert_eq!(
            scout.started.map(|t| t.timestamp_millis()),
            Some(started.timestamp_millis())
        );
        let AgentKind::Fleet {
            element,
            row,
            view,
            stop,
        } = &scout.kind
        else {
            panic!("a fleet row");
        };
        assert_eq!((element.as_str(), row.as_str()), ("subagents:fleet", "j1"));
        assert!(view.is_some());
        assert_eq!(
            stop.as_ref().and_then(|s| s.confirm.as_deref()),
            Some("Kill this sub-agent?")
        );
        // A finished job settles under Previous agents, its timer gone and
        // nothing left to stop.
        let runner = &list.previous[0];
        assert_eq!(runner.status, ChatIndicator::Completed);
        assert_eq!(runner.started, None);
        assert!(matches!(&runner.kind, AgentKind::Fleet { stop: None, .. }));
        // Another chat's panel never shows this fleet.
        let mut other = main.clone();
        other.id = "other".into();
        assert!(agent_list(&state, &other, Utc::now()).is_empty());
    }

    #[test]
    fn background_commands_list_apart_from_sub_agents_with_their_kill() {
        let mut state = AppState::new();
        state.apply_chats(vec![chat("main", None, 10)]);
        state.selected_chat = Some("main".into());
        state.extension_ui.chat = Some("main".into());
        let started = Utc::now() - chrono::Duration::seconds(41);
        state.extension_ui.state.elements = vec![serde_json::json!({
            "id": "jobs", "ns": "bash", "kind": "roster", "placement": "pinned",
            "title": "Background bash · 1 running",
            "rows": [
                {"id": "bg-1", "label": "npm run dev", "state": "running",
                 "detail": "41s · ready",
                 "actions": [{"id": "kill", "label": "Kill", "variant": "danger",
                              "confirm": "Kill this job?"}]},
                {"id": "bg-2", "label": "cargo check", "state": "done", "actions": []}
            ]
        })];
        state.extension_ui.state.channels.insert(
            "bash-bg:fleet".into(),
            serde_json::json!({"entries": [
                {"key": "bg-1", "startedAt": started.timestamp_millis()}
            ], "active": false, "selected": ""}),
        );
        let main = state.chats[0].clone();
        let list = agent_list(&state, &main, Utc::now());
        // Commands are not sub-agents: the Sub-agents section stays empty.
        assert!(list.is_empty());
        assert_eq!(list.running(), 0);
        let [dev, check] = list.background.as_slice() else {
            panic!("two background rows");
        };
        assert_eq!(dev.title.as_ref(), "npm run dev");
        assert_eq!(dev.key(), "bash-bg-1");
        assert_eq!(dev.status, ChatIndicator::Working);
        assert_eq!(
            dev.started.map(|t| t.timestamp_millis()),
            Some(started.timestamp_millis())
        );
        let AgentKind::Background { element, row, stop } = &dev.kind else {
            panic!("a background row");
        };
        assert_eq!((element.as_str(), row.as_str()), ("bash:jobs", "bg-1"));
        assert_eq!(
            stop.as_ref().and_then(|s| s.confirm.as_deref()),
            Some("Kill this job?")
        );
        // A settled command has no timer and nothing to stop.
        assert_eq!(check.started, None);
        assert!(matches!(
            &check.kind,
            AgentKind::Background { stop: None, .. }
        ));
        let mut other = main.clone();
        other.id = "other".into();
        assert!(agent_list(&state, &other, Utc::now()).background.is_empty());
    }
}
