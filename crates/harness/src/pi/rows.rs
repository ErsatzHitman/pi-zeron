//! The agent's own extension work as transcript rows. What the extensions
//! draw while the agent works on its own (the advisor's consultation, Jev's
//! verdict, a subagent fleet, background shells, notices) is not a prompt the
//! user answers, so it is recorded in the turn like any other tool call: one
//! `pi · <ns>` row per element, repainted as the element changes and settled
//! when it goes away or the turn ends. Prompts (sheets, screens) and footer
//! chips stay live state.
use serde_json::{Value, json};
use std::{
    collections::{HashMap, HashSet},
    time::{Duration, Instant},
};
use zeron_proto::{
    AgentEvent, ToolCall,
    extension_ui::{ExtensionUiState, NoticeLevel},
    view::{PI_EXTENSION_SERVER, is_goal_widget},
};

/// A row repaints at most this often while its element streams (the advisor
/// patches its panel every 100 ms); the last state always lands when it settles.
const REPAINT: Duration = Duration::from_secs(1);
/// Lines a row keeps.
const MAX_LINES: usize = 60;

struct Row {
    id: String,
    tool: String,
    input: Value,
    failed: bool,
    painted: Instant,
    dirty: bool,
}

/// One piece of the agent's work as it stands: `(key, tool, input, failed)`.
type Work = (String, String, Value, bool);

fn input(title: &str, mut lines: Vec<String>) -> Value {
    // A panel whose title is its status line repeats it in its body.
    while lines
        .first()
        .is_some_and(|line| line.trim() == title.trim())
    {
        lines.remove(0);
    }
    lines.truncate(MAX_LINES);
    json!({ "title": title, "lines": lines })
}

/// `lines` capped at [`MAX_LINES`], saying how many more there were.
fn capped(text: &str) -> Vec<String> {
    let lines: Vec<&str> = text.lines().collect();
    let mut kept: Vec<String> = lines
        .iter()
        .take(MAX_LINES)
        .map(|l| (*l).to_owned())
        .collect();
    if lines.len() > MAX_LINES {
        kept.push(format!("… {} more lines", lines.len() - MAX_LINES));
    }
    kept
}

/// What a notice row keeps as its message: enough to title it, not a dump.
const NOTICE_MESSAGE_BYTES: usize = 2048;

/// The non-prompt elements and plain widgets of `state`, in order.
fn work(state: &ExtensionUiState) -> Vec<Work> {
    let elements = state
        .typed_elements()
        .into_iter()
        .filter(|element| !element.is_prompt() && !element.is_status())
        // The todo list is the checklist above the composer (its `todo`
        // calls become `Todo` items), not a row as well.
        .filter(|element| element.ns != "todo")
        .map(|element| {
            let title = element
                .title
                .clone()
                .filter(|title| !title.trim().is_empty())
                .unwrap_or_else(|| element.ns.clone());
            let failed = element.failed();
            (
                element.key(),
                element.ns.clone(),
                input(&title, element.text_lines()),
                failed,
            )
        });
    let widgets = state
        .widgets
        .iter()
        .filter(|widget| !is_goal_widget(&widget.key))
        .map(|widget| {
            let title = widget
                .lines
                .first()
                .cloned()
                .filter(|line| !line.trim().is_empty())
                .unwrap_or_else(|| widget.key.clone());
            // The PIUI hub's plain copy of an element (`piui:<ns>:<id>`, drawn
            // while no client listens) continues that element's row.
            let (key, tool) = match widget
                .key
                .strip_prefix("piui:")
                .and_then(|key| Some((key, key.split_once(':')?.0)))
            {
                Some((key, ns)) => (key.to_owned(), ns.to_owned()),
                None => (format!("widget:{}", widget.key), widget.key.clone()),
            };
            (key, tool, input(&title, widget.lines.clone()), false)
        });
    elements.chain(widgets).collect()
}

fn call(id: &str, tool: &str, input: &Value) -> AgentEvent {
    AgentEvent::ToolCall {
        id: id.to_owned(),
        call: ToolCall::Mcp {
            server: PI_EXTENSION_SERVER.into(),
            tool: tool.to_owned(),
            input: Some(input.clone()),
        },
    }
}

fn result(id: String, failed: bool) -> AgentEvent {
    AgentEvent::ToolResult {
        id,
        is_error: failed,
        output: None,
        diff: None,
    }
}

#[derive(Default)]
pub(super) struct Rows {
    open: HashMap<String, Row>,
    /// What each settled row last showed: an element left up across turns
    /// (Jev's card until dismissed) gets no new row until it changes.
    settled: HashMap<String, Value>,
    next: u64,
    /// Distinguishes this run's rows from another process's in the same chat.
    run: String,
}

impl Rows {
    fn next_id(&mut self, what: &str) -> String {
        if self.run.is_empty() {
            self.run = uuid::Uuid::new_v4().simple().to_string()[..8].to_owned();
        }
        self.next += 1;
        format!("pi-ui-{}-{}-{what}", self.run, self.next)
    }

    /// The events that bring the turn's rows up to date with `state`.
    pub fn sync(&mut self, state: &ExtensionUiState, now: Instant) -> Vec<AgentEvent> {
        let mut events = Vec::new();
        let mut live = HashSet::new();
        let work = work(state);
        let present: HashSet<&str> = work.iter().map(|(key, ..)| key.as_str()).collect();
        self.settled.retain(|key, _| present.contains(key.as_str()));
        for (key, tool, input, failed) in work {
            if !self.open.contains_key(&key) && self.settled.get(&key) == Some(&input) {
                continue;
            }
            self.settled.remove(&key);
            live.insert(key.clone());
            if let Some(row) = self.open.get_mut(&key) {
                row.failed = failed;
                if row.input != input {
                    row.input = input;
                    row.dirty = true;
                }
                if row.dirty && now.duration_since(row.painted) >= REPAINT {
                    events.push(call(&row.id, &row.tool, &row.input));
                    row.painted = now;
                    row.dirty = false;
                }
                continue;
            }
            let id = self.next_id(&key);
            events.push(call(&id, &tool, &input));
            self.open.insert(
                key,
                Row {
                    id,
                    tool,
                    input,
                    failed,
                    painted: now,
                    dirty: false,
                },
            );
        }
        let mut gone: Vec<String> = self
            .open
            .keys()
            .filter(|key| !live.contains(*key))
            .cloned()
            .collect();
        gone.sort();
        for key in gone {
            if let Some(row) = self.open.remove(&key) {
                events.extend(self.settle(key, row));
            }
        }
        events
    }

    fn settle(&mut self, key: String, row: Row) -> Vec<AgentEvent> {
        self.settled.insert(key, row.input.clone());
        settle(row)
    }

    /// The turn ends: every open row gets its last content and settles. The
    /// next turn starts rows of its own for what is still running.
    pub fn settle_all(&mut self) -> Vec<AgentEvent> {
        let mut rows: Vec<(String, Row)> = self.open.drain().collect();
        rows.sort_by(|a, b| a.1.id.cmp(&b.1.id));
        rows.into_iter()
            .flat_map(|(key, row)| self.settle(key, row))
            .collect()
    }

    /// A notice during a turn: a settled row of its own, not a toast.
    pub fn notice(&mut self, message: &str, level: NoticeLevel) -> Vec<AgentEvent> {
        let id = self.next_id("notice");
        let mut short = message.len().min(NOTICE_MESSAGE_BYTES);
        while !message.is_char_boundary(short) {
            short -= 1;
        }
        let input = json!({
            "message": &message[..short],
            "lines": capped(message),
        });
        vec![
            call(&id, "notice", &input),
            result(id, level == NoticeLevel::Error),
        ]
    }
}

fn settle(row: Row) -> Vec<AgentEvent> {
    let mut events = Vec::new();
    if row.dirty {
        events.push(call(&row.id, &row.tool, &row.input));
    }
    events.push(result(row.id, row.failed));
    events
}

#[cfg(test)]
mod tests {
    use super::*;
    use zeron_proto::extension_ui::ExtensionWidget;

    fn state(elements: Vec<Value>) -> ExtensionUiState {
        ExtensionUiState {
            elements,
            ..Default::default()
        }
    }

    fn advisor(phase: &str) -> Value {
        json!({"id":"panel","ns":"advisor-live-panel","kind":"panel","placement":"pinned",
            "title":"Advisor","sections":[{"kind":"status","text":phase}]})
    }

    /// `(n-what, kind)` per event: the id without its run token.
    fn ids(events: &[AgentEvent]) -> Vec<(String, &'static str)> {
        let short = |id: &str| id.splitn(4, '-').nth(3).unwrap_or(id).to_owned();
        events
            .iter()
            .map(|event| match event {
                AgentEvent::ToolCall { id, .. } => (short(id), "call"),
                AgentEvent::ToolResult { id, .. } => (short(id), "result"),
                _ => (String::new(), "other"),
            })
            .collect()
    }

    #[test]
    fn the_agents_work_becomes_one_row_repainted_at_most_once_a_second_and_settled() {
        let start = Instant::now();
        let mut rows = Rows::default();
        let sheet = json!({"id":"question","ns":"ask-user","kind":"panel","placement":"sheet"});
        let status = json!({"id":"mode","ns":"plan","kind":"status","placement":"status"});
        let first = rows.sync(&state(vec![advisor("THINKING"), sheet, status]), start);
        assert_eq!(
            ids(&first),
            vec![("1-advisor-live-panel:panel".into(), "call")]
        );
        let AgentEvent::ToolCall {
            call:
                ToolCall::Mcp {
                    server,
                    tool,
                    input,
                },
            ..
        } = &first[0]
        else {
            panic!("a tool call")
        };
        assert_eq!(
            (server.as_str(), tool.as_str()),
            ("pi", "advisor-live-panel")
        );
        assert_eq!(input.as_ref().unwrap()["title"], "Advisor");
        assert_eq!(input.as_ref().unwrap()["lines"], json!(["THINKING"]));

        // A patch within the second waits; the next one after it paints.
        let soon = start + Duration::from_millis(100);
        assert!(rows.sync(&state(vec![advisor("READING")]), soon).is_empty());
        let later = start + Duration::from_millis(1100);
        let painted = rows.sync(&state(vec![advisor("ANSWERING")]), later);
        assert_eq!(
            ids(&painted),
            vec![("1-advisor-live-panel:panel".into(), "call")]
        );

        // Removed: its last content lands, then the row settles.
        let unpainted = later + Duration::from_millis(10);
        assert!(
            rows.sync(&state(vec![advisor("DONE")]), unpainted)
                .is_empty()
        );
        let gone = rows.sync(&state(vec![]), unpainted);
        assert_eq!(
            ids(&gone),
            vec![
                ("1-advisor-live-panel:panel".into(), "call"),
                ("1-advisor-live-panel:panel".into(), "result"),
            ]
        );
        assert!(rows.settle_all().is_empty());
    }

    #[test]
    fn a_turns_end_settles_open_rows_and_widgets_and_notices_are_rows_too() {
        let now = Instant::now();
        let mut rows = Rows::default();
        let mut live = state(vec![json!({"id":"jobs","ns":"bash","kind":"roster",
            "placement":"pinned","title":"Background bash · 1 running",
            "rows":[{"id":"j1","label":"npm test","state":"error"}]})]);
        live.widgets.push(ExtensionWidget {
            key: "jev-decompose".into(),
            lines: vec!["Jev".into(), "SELF_ONLY".into()],
            placement: None,
        });
        assert_eq!(rows.sync(&live, now).len(), 2);
        let settled = rows.settle_all();
        assert_eq!(settled.len(), 2);
        assert!(settled.iter().any(|event| matches!(
            event,
            AgentEvent::ToolResult { id, is_error: true, .. } if id.ends_with("bash:jobs")
        )));
        // What is left up unchanged gets no new row next turn; what changes
        // does, under a fresh id.
        assert!(rows.sync(&live, now).is_empty());
        live.widgets[0].lines.push("ORCHESTRATE".into());
        let next = rows.sync(&live, now);
        assert_eq!(ids(&next).len(), 1);
        assert!(ids(&next)[0].0.ends_with("widget:jev-decompose"));
        assert!(!ids(&next)[0].0.starts_with("2-"));

        let notice = rows.notice("Saved the plan", NoticeLevel::Error);
        assert!(matches!(
            &notice[0],
            AgentEvent::ToolCall { call: ToolCall::Mcp { tool, .. }, .. } if tool == "notice"
        ));
        assert!(matches!(
            &notice[1],
            AgentEvent::ToolResult { is_error: true, .. }
        ));
    }

    #[test]
    fn a_goal_status_widget_is_no_row() {
        let mut rows = Rows::default();
        let mut live = state(vec![]);
        live.widgets.push(ExtensionWidget {
            key: "minimal-status:goal".into(),
            lines: vec!["> goal  36.7k tok · steps 0/3 · 20s".into()],
            placement: None,
        });
        assert!(rows.sync(&live, Instant::now()).is_empty());
    }

    #[test]
    fn a_hub_copy_continues_its_elements_row_and_the_todo_list_is_no_row() {
        let now = Instant::now();
        let mut rows = Rows::default();
        let todo = json!({"id":"list","ns":"todo","kind":"panel","placement":"pinned","title":"Todos 0/1"});
        let first = rows.sync(&state(vec![advisor("THINKING"), todo]), now);
        assert_eq!(
            ids(&first),
            vec![("1-advisor-live-panel:panel".into(), "call")]
        );
        let mut copy = state(vec![]);
        copy.widgets.push(ExtensionWidget {
            key: "piui:advisor-live-panel:panel".into(),
            lines: vec!["Advisor".into(), "READING".into()],
            placement: None,
        });
        // Same row, repainted: no settle, no second row.
        let later = now + Duration::from_secs(2);
        assert_eq!(
            ids(&rows.sync(&copy, later)),
            vec![("1-advisor-live-panel:panel".into(), "call")]
        );
        let long = (0..70)
            .map(|n| n.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        let AgentEvent::ToolCall {
            call: ToolCall::Mcp { input, .. },
            ..
        } = &rows.notice(&long, NoticeLevel::Info)[0]
        else {
            panic!("a notice row")
        };
        let lines = input.as_ref().unwrap()["lines"].as_array().unwrap().clone();
        assert_eq!(lines.len(), MAX_LINES + 1);
        assert_eq!(lines[MAX_LINES], "… 10 more lines");
    }
}
