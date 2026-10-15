//! The agent's checklist, shown in the session panel's Todo section
//! (`shell/session_info/todos.rs`). There is no tray above the composer.
//!
//! Every harness that plans (Claude's TodoWrite, OpenCode, ACP and Codex
//! plans) emits the *whole* list on each update, so the section simply tracks
//! the latest `Todo` tool part of the selected chat.
//!
//! Most of this file is pure — which list is current, what the header says,
//! which rows a long list folds to — so it is unit-tested without a window.
//! [`status_glyph`] draws a row's status.

use std::ops::Range;

use gpui::{AnyElement, IntoElement, ParentElement as _, Styled, div, px};

use zeron_doc::{MessagePart, MessageRole, SessionMessageEntry};
use zeron_proto::{TodoItem, TodoStatus, ToolCall};

use crate::icons::{self, icon};
use crate::theme::Theme;

/// Lists longer than this fold to a [`FOCUS_WINDOW`] around the current item.
pub(crate) const FOLD_ABOVE: usize = 6;
pub(crate) const FOCUS_WINDOW: usize = 3;

/// The checklist the agent most recently wrote, or `None` when there is none
/// (or the latest write cleared it with an empty list).
///
/// `entries` is the selected chat's own transcript; subagent traffic lives in
/// separate docs and never reaches it. ACP plans reuse one tool id for every
/// update, so "latest part" — not "first part with this id" — is the contract.
pub(crate) fn latest_todo(entries: &[SessionMessageEntry]) -> Option<Vec<TodoItem>> {
    let items = entries
        .iter()
        .rev()
        .filter(|entry| entry.role == MessageRole::Assistant)
        .flat_map(|entry| entry.parts.iter().rev())
        .find_map(|part| match part {
            MessagePart::Tool {
                call: ToolCall::Todo { items },
                ..
            } => Some(items),
            _ => None,
        })?;
    (!items.is_empty()).then(|| items.clone())
}

/// What the collapsed header reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TodoSummary {
    pub total: usize,
    pub done: usize,
    /// First in-progress item.
    pub active: Option<usize>,
    /// First item not yet completed.
    pub next: Option<usize>,
}

impl TodoSummary {
    pub fn of(items: &[TodoItem]) -> Self {
        let mut summary = Self {
            total: items.len(),
            done: 0,
            active: None,
            next: None,
        };
        for (ix, item) in items.iter().enumerate() {
            match item.status() {
                TodoStatus::Completed => summary.done += 1,
                TodoStatus::InProgress => {
                    summary.active.get_or_insert(ix);
                    summary.next.get_or_insert(ix);
                }
                TodoStatus::Pending => {
                    summary.next.get_or_insert(ix);
                }
            }
        }
        summary
    }

    pub fn finished(&self) -> bool {
        self.total > 0 && self.done == self.total
    }

    /// The item the header names: what is being worked on, else what is next.
    pub fn headline(&self) -> Option<usize> {
        self.active.or(self.next)
    }
}

/// The slice of a long list that stays visible when folded: three items with
/// the current one in the middle (one of context before it, one after),
/// clamped to the list ends. A finished list shows its last three. Lists up to
/// [`FOLD_ABOVE`] are never folded.
pub(crate) fn focus_window(items: &[TodoItem]) -> Range<usize> {
    let total = items.len();
    if total <= FOLD_ABOVE {
        return 0..total;
    }
    let summary = TodoSummary::of(items);
    let focus = summary.headline().unwrap_or(total - 1);
    let start = focus.saturating_sub(1).min(total - FOCUS_WINDOW);
    start..start + FOCUS_WINDOW
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FoldSide {
    Earlier,
    Later,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TodoRow {
    /// An item, by its index in the agent's list.
    Item(usize),
    /// A fold toggle standing in for `count` hidden items (`open`: they are
    /// currently revealed, and the row hides them again).
    Fold {
        side: FoldSide,
        count: usize,
        open: bool,
    },
}

/// The rows of the expanded list, top to bottom. Items always keep the
/// agent's order; folds sit at the edges of the list they stand for.
pub(crate) fn rows(items: &[TodoItem], show_earlier: bool, show_later: bool) -> Vec<TodoRow> {
    let window = focus_window(items);
    let earlier = window.start;
    let later = items.len() - window.end;
    let mut out = Vec::with_capacity(items.len() + 2);
    if earlier > 0 {
        out.push(TodoRow::Fold {
            side: FoldSide::Earlier,
            count: earlier,
            open: show_earlier,
        });
        if show_earlier {
            out.extend((0..earlier).map(TodoRow::Item));
        }
    }
    out.extend(window.clone().map(TodoRow::Item));
    if later > 0 {
        if show_later {
            out.extend((window.end..items.len()).map(TodoRow::Item));
        }
        out.push(TodoRow::Fold {
            side: FoldSide::Later,
            count: later,
            open: show_later,
        });
    }
    out
}

/// Per-chat presentation state. In memory only — like the right-pane flags in
/// `shell::SessionPanels` it lasts for the app run, not across restarts.
#[derive(Debug, Default, Clone)]
pub(crate) struct TodoPanelState {
    show_earlier: bool,
    show_later: bool,
}

impl TodoPanelState {
    pub fn toggle_fold(&mut self, side: FoldSide) {
        match side {
            FoldSide::Earlier => self.show_earlier = !self.show_earlier,
            FoldSide::Later => self.show_later = !self.show_later,
        }
    }

    /// Whether the folded-away items above and below the focus window are
    /// currently revealed.
    pub fn folds(&self) -> (bool, bool) {
        (self.show_earlier, self.show_later)
    }
}

/// The latest list for the selected chat, recomputed only when its transcript
/// changes (the scan walks back from the end, but a chat with no todo walks all
/// of it, and the composer renders far more often than the transcript changes).
#[derive(Debug, Default)]
pub(crate) struct TodoCache {
    key: Option<(String, u64, usize)>,
    items: Option<Vec<TodoItem>>,
}

impl TodoCache {
    pub fn latest(
        &mut self,
        chat: &str,
        revision: u64,
        entries: &[SessionMessageEntry],
    ) -> Option<&[TodoItem]> {
        let key = (chat.to_owned(), revision, entries.len());
        if self.key.as_ref() != Some(&key) {
            self.items = latest_todo(entries);
            self.key = Some(key);
        }
        self.items.as_deref()
    }
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

/// check / active / pending. An in-progress item only animates while the turn
/// is live; on an idle chat it is a still ring (the agent is not working on
/// it), so a stopped run does not look busy forever.
pub(crate) fn status_glyph(
    key: String,
    cell_px: f32,
    status: TodoStatus,
    live: bool,
    theme: &Theme,
    view: gpui::EntityId,
    cx: &mut gpui::App,
) -> AnyElement {
    match status {
        TodoStatus::Completed => icon(icons::CHECK)
            .size(px(12.0))
            .text_color(theme.success)
            .into_any_element(),
        TodoStatus::InProgress if live => {
            crate::loaders::mini_glyph_spinner(key, cell_px, theme.glyph, view, cx)
                .into_any_element()
        }
        TodoStatus::InProgress => div()
            .size(px(10.0))
            .rounded_full()
            .border_1()
            .border_color(theme.accent)
            .flex()
            .items_center()
            .justify_center()
            .child(div().size(px(4.0)).rounded_full().bg(theme.accent))
            .into_any_element(),
        TodoStatus::Pending => div()
            .size(px(10.0))
            .rounded_full()
            .border_1()
            .border_color(theme.text_faint.opacity(0.7))
            .into_any_element(),
    }
}

/// An assistant entry whose only part writes `list`, for tests.
#[cfg(test)]
pub(crate) fn assistant_with_todo(list: Vec<TodoItem>) -> SessionMessageEntry {
    SessionMessageEntry {
        id: "m".into(),
        role: MessageRole::Assistant,
        parts: vec![MessagePart::Tool {
            id: "t".into(),
            call: ToolCall::Todo { items: list },
            is_error: false,
            resolved: true,
            output: None,
            diff: None,
            output_ref: None,
            output_bytes: None,
            diff_ref: None,
            diff_stats: None,
            subagent_ref: None,
            subagent_status: None,
            subagent_tail: None,
        }],
        created_at: 0,
        device_id: "d".into(),
        status: None,
        continuation_of: None,
        duration_ms: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folds_report_what_the_user_revealed() {
        let mut state = TodoPanelState::default();
        assert_eq!(state.folds(), (false, false));
        state.toggle_fold(FoldSide::Later);
        assert_eq!(state.folds(), (false, true));
    }

    fn items(spec: &str) -> Vec<TodoItem> {
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

    fn entry(role: MessageRole, parts: Vec<MessagePart>) -> SessionMessageEntry {
        SessionMessageEntry {
            id: "m".into(),
            role,
            parts,
            created_at: 0,
            device_id: "d".into(),
            status: None,
            continuation_of: None,
            duration_ms: None,
        }
    }

    fn todo_part(id: &str, list: Vec<TodoItem>) -> MessagePart {
        MessagePart::Tool {
            id: id.into(),
            call: ToolCall::Todo { items: list },
            is_error: false,
            resolved: true,
            output: None,
            diff: None,
            output_ref: None,
            output_bytes: None,
            diff_ref: None,
            diff_stats: None,
            subagent_ref: None,
            subagent_status: None,
            subagent_tail: None,
        }
    }

    #[test]
    fn latest_todo_is_the_last_write_across_entries() {
        let first = entry(MessageRole::Assistant, vec![todo_part("a", items("x.."))]);
        let second = entry(
            MessageRole::Assistant,
            vec![
                todo_part("b", items("xx.")),
                MessagePart::Text {
                    id: "t".into(),
                    text: "working".into(),
                },
                todo_part("c", items("xx>")),
            ],
        );
        assert_eq!(latest_todo(&[first, second]), Some(items("xx>")));
    }

    #[test]
    fn acp_plan_reusing_one_id_resolves_to_the_newest_segment() {
        let plan = zeron_proto::LIVE_PLAN_TOOL_ID;
        let old = entry(MessageRole::Assistant, vec![todo_part(plan, items(">.."))]);
        let new = entry(MessageRole::Assistant, vec![todo_part(plan, items("x>."))]);
        assert_eq!(latest_todo(&[old, new]), Some(items("x>.")));
    }

    #[test]
    fn an_empty_write_clears_and_user_rows_are_ignored() {
        let list = entry(MessageRole::Assistant, vec![todo_part("a", items("x."))]);
        let cleared = entry(MessageRole::Assistant, vec![todo_part("b", Vec::new())]);
        assert_eq!(latest_todo(&[list.clone(), cleared]), None);
        let user = entry(MessageRole::User, vec![todo_part("u", items("."))]);
        assert_eq!(latest_todo(&[user]), None);
        assert_eq!(latest_todo(&[list]), Some(items("x.")));
        assert_eq!(latest_todo(&[]), None);
    }

    #[test]
    fn summary_counts_and_names_the_current_item() {
        let s = TodoSummary::of(&items("xx>.."));
        assert_eq!(
            (s.total, s.done, s.active, s.next),
            (5, 2, Some(2), Some(2))
        );
        assert_eq!(s.headline(), Some(2));
        assert!(!s.finished());
        // Nothing started: the first unfinished item is the headline.
        let s = TodoSummary::of(&items("x..."));
        assert_eq!((s.active, s.headline()), (None, Some(1)));
        // An in-progress item later in the list still wins over an earlier
        // pending one: the agent is working on it.
        assert_eq!(TodoSummary::of(&items(".>.")).headline(), Some(1));
        let s = TodoSummary::of(&items("xxx"));
        assert!(s.finished());
        assert_eq!(s.headline(), None);
        assert!(!TodoSummary::of(&[]).finished());
    }

    #[test]
    fn short_lists_never_fold() {
        for n in 0..=FOLD_ABOVE {
            let list = items(&".".repeat(n));
            assert_eq!(focus_window(&list), 0..n);
            assert!(
                rows(&list, false, false)
                    .iter()
                    .all(|r| matches!(r, TodoRow::Item(_)))
            );
        }
    }

    #[test]
    fn long_lists_fold_to_three_around_the_current_item() {
        // Current item in the middle: one before, it, one after.
        assert_eq!(focus_window(&items("xxxx>.....")), 3..6);
        // At the start / end the window clamps instead of shrinking.
        assert_eq!(focus_window(&items(">.......")), 0..3);
        assert_eq!(focus_window(&items("xxxxxxx>")), 5..8);
        // No in-progress item: the first unfinished one is the focus.
        assert_eq!(focus_window(&items("xxxx.....")), 3..6);
        // Everything done: the tail.
        assert_eq!(focus_window(&items("xxxxxxxx")), 5..8);
        // Always exactly three when folded.
        for spec in ["xxxxxxx>", ">.......", "xx>.....", "xxxxxxxx", "........"] {
            assert_eq!(focus_window(&items(spec)).len(), FOCUS_WINDOW, "{spec}");
        }
    }

    #[test]
    fn rows_put_folds_at_the_edges_with_counts() {
        let list = items("xxxx>.....");
        assert_eq!(
            rows(&list, false, false),
            vec![
                TodoRow::Fold {
                    side: FoldSide::Earlier,
                    count: 3,
                    open: false
                },
                TodoRow::Item(3),
                TodoRow::Item(4),
                TodoRow::Item(5),
                TodoRow::Fold {
                    side: FoldSide::Later,
                    count: 4,
                    open: false
                },
            ]
        );
        // No fold on a side with nothing hidden.
        let head = rows(&items(">......."), false, false);
        assert!(matches!(head.first(), Some(TodoRow::Item(0))));
        assert!(matches!(
            head.last(),
            Some(TodoRow::Fold {
                side: FoldSide::Later,
                count: 5,
                ..
            })
        ));
    }

    #[test]
    fn expanding_a_fold_reveals_items_without_reordering() {
        let list = items("xxxx>.....");
        let order = |rows: &[TodoRow]| -> Vec<usize> {
            rows.iter()
                .filter_map(|r| match r {
                    TodoRow::Item(ix) => Some(*ix),
                    _ => None,
                })
                .collect()
        };
        for (earlier, later) in [(false, false), (true, false), (false, true), (true, true)] {
            let shown = order(&rows(&list, earlier, later));
            assert!(shown.windows(2).all(|w| w[0] < w[1]), "{earlier} {later}");
        }
        // Both open: the whole list, in order, and each toggle still present
        // so it can be closed again.
        let all = rows(&list, true, true);
        assert_eq!(order(&all), (0..list.len()).collect::<Vec<_>>());
        assert!(matches!(
            all.first(),
            Some(TodoRow::Fold { open: true, .. })
        ));
        assert!(matches!(all.last(), Some(TodoRow::Fold { open: true, .. })));
        // Earlier items appear after their fold row, later items before theirs.
        assert_eq!(all[1], TodoRow::Item(0));
        assert_eq!(all[all.len() - 2], TodoRow::Item(9));
    }

    #[test]
    fn folds_toggle_independently() {
        let mut state = TodoPanelState::default();
        state.toggle_fold(FoldSide::Later);
        assert!(state.show_later && !state.show_earlier);
        state.toggle_fold(FoldSide::Later);
        assert!(!state.show_later);
    }

    #[test]
    fn cache_rescans_only_when_the_transcript_changes() {
        let mut cache = TodoCache::default();
        let one = vec![entry(
            MessageRole::Assistant,
            vec![todo_part("a", items("x."))],
        )];
        assert_eq!(cache.latest("chat", 1, &one), Some(items("x.").as_slice()));
        // Same chat, revision and length: served from cache, not rescanned.
        assert_eq!(cache.latest("chat", 1, &one), Some(items("x.").as_slice()));
        let two = vec![
            one[0].clone(),
            entry(MessageRole::Assistant, vec![todo_part("b", items("xx"))]),
        ];
        assert_eq!(cache.latest("chat", 2, &two), Some(items("xx").as_slice()));
        // Another chat never reuses the first chat's list.
        assert_eq!(cache.latest("other", 2, &[]), None);
    }
}
