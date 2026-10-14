//! The session panel's Todo section: whether the agent's checklist is worth a
//! section right now, and what its header says. Pure, so it is unit-tested
//! without a window; the rows are drawn by `Shell::render_todos`.

use gpui::SharedString;
use zeron_doc::{MessagePart, MessageRole, SessionMessageEntry};
use zeron_proto::{TodoItem, ToolCall};

use crate::todo_panel::TodoSummary;

/// A checklist that earns a section.
pub(super) struct TodoList {
    pub items: Vec<TodoItem>,
    pub summary: TodoSummary,
    /// The turn is running: in-progress items spin.
    pub live: bool,
}

impl TodoList {
    /// `None` when there is no list (or the agent cleared it). An unfinished
    /// list always shows: an abandoned plan is still pending work. A finished
    /// one shows only for the turn that finished it (`live` and written since
    /// the last user message), so the last tick is seen and last turn's
    /// checklist does not come back each time you send. `fresh` is only
    /// called for a finished list.
    pub fn of(
        items: Option<Vec<TodoItem>>,
        live: bool,
        fresh: impl FnOnce() -> bool,
    ) -> Option<Self> {
        let items = items.filter(|items| !items.is_empty())?;
        let summary = TodoSummary::of(&items);
        let show = !summary.finished() || (live && fresh());
        show.then_some(Self {
            items,
            summary,
            live,
        })
    }

    /// The header's trailing count: `· 2/5`.
    pub fn count_label(&self) -> SharedString {
        format!("· {}/{}", self.summary.done, self.summary.total).into()
    }
}

/// Whether the transcript's latest todo write came after its last user
/// message, i.e. during the turn in progress. Walks back only as far as that
/// message.
pub(super) fn written_this_turn(entries: &[SessionMessageEntry]) -> bool {
    for entry in entries.iter().rev() {
        match entry.role {
            MessageRole::User => return false,
            MessageRole::Assistant
                if entry.parts.iter().any(|part| {
                    matches!(
                        part,
                        MessagePart::Tool {
                            call: ToolCall::Todo { .. },
                            ..
                        }
                    )
                }) =>
            {
                return true;
            }
            _ => {}
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::todo_panel::assistant_with_todo;
    use zeron_proto::TodoStatus;

    fn list(spec: &str) -> Vec<TodoItem> {
        // x = completed, > = in progress, . = pending
        spec.chars()
            .enumerate()
            .map(|(i, c)| {
                TodoItem::new(
                    format!("item {i}"),
                    match c {
                        'x' => TodoStatus::Completed,
                        '>' => TodoStatus::InProgress,
                        _ => TodoStatus::Pending,
                    },
                )
            })
            .collect()
    }

    fn user() -> SessionMessageEntry {
        let mut entry = assistant_with_todo(Vec::new());
        entry.role = MessageRole::User;
        entry.parts.clear();
        entry
    }

    #[test]
    fn no_list_or_an_empty_one_has_no_section() {
        assert!(TodoList::of(None, true, || true).is_none());
        assert!(TodoList::of(Some(Vec::new()), true, || true).is_none());
    }

    #[test]
    fn unfinished_work_shows_live_or_idle() {
        assert!(TodoList::of(Some(list("x>.")), true, || false).is_some());
        // An abandoned list on an idle chat is still pending work.
        assert!(TodoList::of(Some(list("x..")), false, || false).is_some());
    }

    #[test]
    fn a_finished_list_shows_only_for_the_turn_that_finished_it() {
        assert!(TodoList::of(Some(list("xxx")), true, || true).is_some());
        // Idle: history, not work under way.
        assert!(TodoList::of(Some(list("xxx")), false, || true).is_none());
        // A new turn is running but the list is last turn's.
        assert!(TodoList::of(Some(list("xxx")), true, || false).is_none());
    }

    #[test]
    fn the_header_counts_done_over_total() {
        let todos = TodoList::of(Some(list("xx>..")), true, || true).unwrap();
        assert_eq!(todos.count_label().as_ref(), "· 2/5");
    }

    #[test]
    fn a_write_before_the_last_user_message_is_last_turns() {
        let write = || assistant_with_todo(list("xxx"));
        assert!(written_this_turn(&[user(), write()]));
        assert!(!written_this_turn(&[user(), write(), user()]));
        assert!(written_this_turn(&[write()]));
    }
}
