//! Side chats nested under their parent in the sidebar's Sessions list
//! (docs/reference/sidebar-side-chats.md). A listed session with live side
//! chats wears a fold chip; open, its children hang below it on the
//! Orchestrator tab's tree rail, in the order they started. Children never
//! join the jump slots, cycling, pins or sections: the parent carries them.

use super::*;
use std::collections::HashMap;
use zeron_proto::view::attention_rank;
use zeron_proto::{Chat, ChatIndicator};

/// A child row: the compact sidebar row (`sidebar_row_height(true, ..)`).
pub(super) const CHILD_ROW_HEIGHT: f32 = 29.0;
/// Children sit one tree level in, on the Orchestrator tab's indent.
pub(super) const CHILD_INDENT: f32 = super::orchestrator::TREE_INDENT;
/// Past this many children an open parent lists the ones that matter and a
/// "Show N more" row.
pub(super) const CHILD_CAP: usize = 8;

/// One live child of a listed session.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct ChildEntry {
    pub id: String,
    pub created_at: DateTime<Utc>,
    /// Last activity, for choosing which children a capped list keeps.
    pub activity: DateTime<Utc>,
    pub status: ChatIndicator,
}

/// Live children by parent id, in creation order (id breaks ties), built
/// once per frame.
#[derive(Default)]
pub(super) struct ChildIndex {
    by_parent: HashMap<String, Vec<ChildEntry>>,
}

impl ChildIndex {
    pub(super) fn build(state: &AppState, now: DateTime<Utc>) -> Self {
        Self::from_chats(
            state
                .chats
                .iter()
                .filter(|chat| chat.parent_chat_id.is_some())
                .map(|chat| (chat, state.display_status_for(chat, now))),
        )
    }

    /// Archived chats, a chat naming itself, and hidden voice and
    /// orchestrator chats never nest.
    fn from_chats<'a>(chats: impl IntoIterator<Item = (&'a Chat, ChatIndicator)>) -> Self {
        let mut by_parent: HashMap<String, Vec<ChildEntry>> = HashMap::new();
        for (chat, status) in chats {
            let Some(parent) = chat.parent_chat_id.as_deref() else {
                continue;
            };
            if chat.archived
                || parent == chat.id
                || zeron_proto::voice::is_orchestrator_chat(&chat.id)
                || zeron_proto::orchestrator::is_root_chat(&chat.id)
            {
                continue;
            }
            by_parent
                .entry(parent.to_owned())
                .or_default()
                .push(ChildEntry {
                    id: chat.id.clone(),
                    created_at: chat.created_at,
                    activity: chat.last_message_at.unwrap_or(chat.created_at),
                    status,
                });
        }
        for kids in by_parent.values_mut() {
            kids.sort_by(|a, b| {
                a.created_at
                    .cmp(&b.created_at)
                    .then_with(|| a.id.cmp(&b.id))
            });
        }
        Self { by_parent }
    }

    pub(super) fn of(&self, parent: &str) -> &[ChildEntry] {
        self.by_parent.get(parent).map_or(&[], Vec::as_slice)
    }

    pub(super) fn has(&self, parent: &str) -> bool {
        self.by_parent.contains_key(parent)
    }
}

/// A child the user should see: running, asking, or failed.
pub(super) fn needs_attention(status: ChatIndicator) -> bool {
    matches!(
        status,
        ChatIndicator::Working | ChatIndicator::AwaitingInput | ChatIndicator::Errored
    )
}

/// The folded parent's dot: its most urgent child's status.
pub(super) fn rollup(kids: &[ChildEntry]) -> ChatIndicator {
    kids.iter()
        .map(|kid| kid.status)
        .min_by_key(|status| attention_rank(*status))
        .unwrap_or(ChatIndicator::Idle)
}

/// The children an open parent lists, in creation order, and how many it
/// hides. Past [`CHILD_CAP`] it keeps every child needing attention, the
/// selected one and the one open beside, then fills up with the most
/// recently active.
pub(super) fn visible_children(
    kids: &[ChildEntry],
    selected: Option<&str>,
    beside: Option<&str>,
    show_all: bool,
) -> (Vec<ChildEntry>, usize) {
    if show_all || kids.len() <= CHILD_CAP {
        return (kids.to_vec(), 0);
    }
    let mut keep: Vec<bool> = kids
        .iter()
        .map(|kid| {
            needs_attention(kid.status)
                || selected == Some(kid.id.as_str())
                || beside == Some(kid.id.as_str())
        })
        .collect();
    let mut kept = keep.iter().filter(|keep| **keep).count();
    let mut rest: Vec<usize> = (0..kids.len()).filter(|ix| !keep[*ix]).collect();
    rest.sort_by(|a, b| {
        kids[*b]
            .activity
            .cmp(&kids[*a].activity)
            .then_with(|| kids[*a].id.cmp(&kids[*b].id))
    });
    for ix in rest {
        if kept >= CHILD_CAP {
            break;
        }
        keep[ix] = true;
        kept += 1;
    }
    let shown: Vec<ChildEntry> = kids
        .iter()
        .zip(&keep)
        .filter(|(_, keep)| **keep)
        .map(|(kid, _)| kid.clone())
        .collect();
    let hidden = kids.len() - shown.len();
    (shown, hidden)
}

/// The children block under a parent row: a leading gap, then each row and
/// the gaps between them, so `n * (row + gap)`. "Show N more" counts as a row.
pub(super) fn block_height(shown: usize, more_row: bool) -> f32 {
    (shown + usize::from(more_row)) as f32 * (CHILD_ROW_HEIGHT + SIDEBAR_LIST_GAP)
}

/// A listed parent's children as this frame draws them.
pub(super) struct SidebarFamily {
    pub kids: Vec<ChildEntry>,
    pub more: usize,
    pub total: usize,
    pub open: bool,
    pub rollup: ChatIndicator,
    /// The block's height under the parent row; 0 folded or while the parent
    /// is dragged (it travels alone).
    pub height: f32,
}

/// One child slot: the tree rail down the indent gutter (ending at the row's
/// middle on the last child), a stub across to the row, then the row.
fn child_slot(row: AnyElement, last: bool, rail: gpui::Hsla) -> AnyElement {
    let middle = CHILD_ROW_HEIGHT / 2.0;
    let x = CHILD_INDENT / 2.0;
    div()
        .relative()
        .flex_none()
        .h(px(CHILD_ROW_HEIGHT))
        .pl(px(CHILD_INDENT))
        .child(
            div()
                .absolute()
                .left(px(x))
                .top(px(-SIDEBAR_LIST_GAP))
                .when(last, |el| el.h(px(middle + SIDEBAR_LIST_GAP)))
                .when(!last, |el| el.bottom_0())
                .w(px(1.0))
                .bg(rail),
        )
        .child(
            div()
                .absolute()
                .left(px(x))
                .top(px(middle))
                .w(px(CHILD_INDENT - x))
                .h(px(1.0))
                .bg(rail),
        )
        .child(row)
        .into_any_element()
}

impl Shell {
    /// The child chat open beside the main one: the right pane's active side
    /// chat tab, else the btw pane's chat.
    pub(super) fn beside_chat(&self, cx: &App) -> Option<String> {
        if matches!(self.route, Route::Chat)
            && self.right_pane_open(cx)
            && let RightSurface::SideChat(tab) = self.resolved_right_active(cx)
            && let Some(tab) = self.side_chats.get(&tab)
        {
            return tab.state.read(cx).selected_chat.clone();
        }
        if self.btw_shown
            && let Some(pane) = self.btw.as_ref()
            && let Some(tab) = self.side_chats.get(&pane.side_chat)
        {
            return tab.state.read(cx).selected_chat.clone();
        }
        None
    }

    /// The session being dragged in the sidebar, if any.
    pub(super) fn dragged_sidebar_chat(&self) -> Option<String> {
        self.sidebar_session_transfer
            .as_ref()
            .map(|drag| drag.payload.chat_id.clone())
            .or_else(|| {
                self.sidebar_session_return
                    .as_ref()
                    .map(|returning| returning.transfer.payload.chat_id.clone())
            })
            .or_else(|| {
                self.pinned_session_drag
                    .as_ref()
                    .map(|drag| drag.chat_id.clone())
            })
    }

    /// Whether `parent`'s children show. A manual fold or unfold wins;
    /// otherwise a parent opens, and stays open, once a child runs, asks,
    /// fails, is selected or is open beside.
    fn sidebar_children_open(
        &mut self,
        parent: &str,
        kids: &[ChildEntry],
        selected: Option<&str>,
        beside: Option<&str>,
    ) -> bool {
        if let Some(open) = self.sidebar_children_override.get(parent) {
            return *open;
        }
        if kids.iter().any(|kid| {
            needs_attention(kid.status)
                || selected == Some(kid.id.as_str())
                || beside == Some(kid.id.as_str())
        }) {
            self.sidebar_children_auto.insert(parent.to_owned());
        }
        self.sidebar_children_auto.contains(parent)
    }

    /// This frame's families for the listed `parents`.
    pub(super) fn sidebar_families<'a>(
        &mut self,
        parents: impl IntoIterator<Item = &'a str>,
        selected: Option<&str>,
        now: DateTime<Utc>,
        cx: &App,
    ) -> HashMap<String, SidebarFamily> {
        let index = ChildIndex::build(self.state.read(cx), now);
        self.sidebar_children_auto
            .retain(|parent| index.has(parent));
        self.sidebar_children_override
            .retain(|parent, _| index.has(parent));
        self.sidebar_children_all.retain(|parent| index.has(parent));
        let beside = self.beside_chat(cx);
        let dragging = self.dragged_sidebar_chat();
        let mut families = HashMap::new();
        for parent in parents {
            let kids = index.of(parent);
            if kids.is_empty() {
                continue;
            }
            let open = self.sidebar_children_open(parent, kids, selected, beside.as_deref());
            let (shown, more) = if open {
                visible_children(
                    kids,
                    selected,
                    beside.as_deref(),
                    self.sidebar_children_all.contains(parent),
                )
            } else {
                (Vec::new(), 0)
            };
            let height = if !open || dragging.as_deref() == Some(parent) {
                0.0
            } else {
                block_height(shown.len(), more > 0)
            };
            families.insert(
                parent.to_owned(),
                SidebarFamily {
                    total: kids.len(),
                    rollup: rollup(kids),
                    kids: shown,
                    more,
                    open,
                    height,
                },
            );
        }
        families
    }

    fn toggle_sidebar_children(&mut self, parent: &str, was_open: bool, cx: &mut Context<Self>) {
        if !self.reduced_motion {
            let (from, to) = if was_open { (1.0, 0.0) } else { (0.0, 1.0) };
            self.begin_sidebar_disclosure_motion(&format!("kids:{parent}"), from, to);
        }
        self.sidebar_children_override
            .insert(parent.to_owned(), !was_open);
        if was_open {
            self.sidebar_children_all.remove(parent);
        }
        // Like a header click: the height change is not a resort.
        self.sidebar_prev_order.clear();
        self.sidebar_resort.clear();
        self.sidebar_new_keys.clear();
        cx.notify();
    }

    /// The parent row's fold chip: chevron, child count and, folded, the
    /// most urgent child's dot.
    pub(super) fn render_children_chip(
        &self,
        parent: &str,
        family: &SidebarFamily,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let open = family.open;
        let selector = format!("sidebar-children-toggle-{parent}");
        let toggle_parent = parent.to_owned();
        let hover = theme.glass_hover();
        div()
            .id(SharedString::from(selector.clone()))
            .debug_selector(move || selector.clone())
            .flex_none()
            .h(px(16.0))
            .px(px(3.0))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(3.0))
            .rounded(px(4.0))
            .cursor_pointer()
            .hover(move |el| el.bg(hover))
            .role(gpui::Role::Button)
            .aria_label(if open {
                "Hide side chats"
            } else {
                "Show side chats"
            })
            .child(self.sidebar_disclosure_chevron(&format!("kids:{parent}"), open, theme))
            .child(
                div()
                    .text_size(crate::typography::ui_rems(10.0))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_color(theme.text_muted.opacity(0.7))
                    .child(SharedString::from(family.total.to_string())),
            )
            .when(!open && family.rollup != ChatIndicator::Idle, |el| {
                el.child(
                    div()
                        .size(px(6.0))
                        .flex_none()
                        .rounded_full()
                        .bg(spaces::status_dot_color(family.rollup, theme)),
                )
            })
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_click(cx.listener(move |this, _, _, cx| {
                cx.stop_propagation();
                this.toggle_sidebar_children(&toggle_parent, open, cx);
            }))
            .into_any_element()
    }

    /// A parent row with its open children under it.
    pub(super) fn with_sidebar_children(
        &mut self,
        row: AnyElement,
        parent: &str,
        family: &SidebarFamily,
        now: DateTime<Utc>,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        if family.height <= 0.0 {
            return row;
        }
        let rail = theme.hairline(0.12);
        let selected = self.state.read(cx).selected_chat.clone();
        let beside = self.beside_chat(cx);
        let count = family.kids.len() + usize::from(family.more > 0);
        let mut items = Vec::with_capacity(count);
        for (ix, kid) in family.kids.iter().enumerate() {
            let lit = selected.as_deref() == Some(kid.id.as_str())
                || beside.as_deref() == Some(kid.id.as_str());
            let child = self.render_sidebar_child_row(&kid.id, kid.status, lit, now, theme, cx);
            items.push(child_slot(child, ix + 1 == count, rail));
        }
        if family.more > 0 {
            let show_parent = parent.to_owned();
            let more = div()
                .id(SharedString::from(format!(
                    "sidebar-children-more-{parent}"
                )))
                .debug_selector({
                    let parent = parent.to_owned();
                    move || format!("sidebar-children-more-{parent}")
                })
                .h(px(CHILD_ROW_HEIGHT))
                .flex()
                .items_center()
                .px(px(Theme::SPACE_SM))
                .rounded(px(8.0))
                .cursor_pointer()
                .hover(|el| el.bg(theme.glass_hover()))
                .text_size(crate::typography::ui_rems(12.0))
                .text_color(theme.text_muted.opacity(0.7))
                .child(SharedString::from(format!("Show {} more", family.more)))
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.sidebar_children_all.insert(show_parent.clone());
                    cx.notify();
                }))
                .into_any_element();
            items.push(child_slot(more, true, rail));
        }
        div()
            .w_full()
            .flex()
            .flex_col()
            .child(row)
            .child(
                div()
                    .id(SharedString::from(format!("sidebar-children-{parent}")))
                    .debug_selector({
                        let parent = parent.to_owned();
                        move || format!("sidebar-children-{parent}")
                    })
                    .w_full()
                    .flex_none()
                    .flex()
                    .flex_col()
                    .gap(px(SIDEBAR_LIST_GAP))
                    .pt(px(SIDEBAR_LIST_GAP))
                    .h(px(family.height))
                    .overflow_hidden()
                    .children(items),
            )
            .into_any_element()
    }

    fn render_sidebar_child_row(
        &self,
        id: &str,
        status: ChatIndicator,
        selected: bool,
        now: DateTime<Utc>,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let row = {
            let state = self.state.read(cx);
            state.chats.iter().find(|chat| chat.id == id).map(|chat| {
                (
                    SharedString::from(transcript::single_line(&child_title(chat))),
                    SharedString::from(format_time_ago(
                        chat.last_message_at.unwrap_or(chat.created_at),
                        now,
                    )),
                    self.settings
                        .sidebar_show_harness
                        .then(|| chat.config.as_ref().map(|c| c.harness))
                        .flatten(),
                )
            })
        };
        let Some((title, time_ago, harness)) = row else {
            return div().into_any_element();
        };
        self.render_chat_row(
            id.to_owned(),
            title,
            time_ago,
            SharedString::default(),
            None,
            None,
            harness,
            status,
            selected,
            false,
            false,
            None,
            None,
            false,
            SidebarRowRole::Child,
            None,
            theme,
            cx,
        )
    }

    /// A child row's click: its parent becomes the open session (from any
    /// route) and the child opens beside it in the right pane, its composer
    /// taking focus. `as_session` (the secondary-modifier click) opens the
    /// child itself instead, as does a child whose parent is gone.
    pub(super) fn open_sidebar_child(
        &mut self,
        child_id: String,
        as_session: bool,
        cx: &mut Context<Self>,
    ) {
        let (child, parent) = {
            let state = self.state.read(cx);
            let child = state.chats.iter().find(|c| c.id == child_id).cloned();
            let parent = child
                .as_ref()
                .and_then(|c| c.parent_chat_id.clone())
                .filter(|parent| state.chats.iter().any(|c| &c.id == parent));
            (child, parent)
        };
        let (Some(child), Some(parent)) = (child, parent) else {
            self.open_chat(child_id, cx);
            return;
        };
        if as_session {
            self.open_chat(child_id, cx);
            return;
        }
        let selected = self.state.read(cx).selected_chat.clone();
        if !matches!(self.route, Route::Chat) || selected.as_deref() != Some(parent.as_str()) {
            self.open_chat(parent.clone(), cx);
        }
        self.open_side_chat(child, parent, cx);
        self.composer
            .update(cx, |composer, _| composer.focus_pending = false);
        if let Some(tab) = self.side_chat_tab_for(&child_id, cx)
            && let Some(tab) = self.side_chats.get(&tab)
        {
            tab.composer
                .update(cx, |composer, _| composer.focus_pending = true);
        }
        cx.notify();
    }

    /// The listed parent of a live child chat, if the chat is one.
    pub(super) fn sidebar_child_parent(&self, chat_id: &str, cx: &App) -> Option<String> {
        let state = self.state.read(cx);
        state
            .chats
            .iter()
            .find(|chat| chat.id == chat_id)
            .filter(|chat| !chat.archived)
            .and_then(|chat| chat.parent_chat_id.clone())
            .filter(|parent| parent != chat_id)
    }
}

/// A child titles itself on its first turn; until then its preview or a
/// placeholder stands in.
fn child_title(chat: &Chat) -> String {
    chat.title
        .clone()
        .or_else(|| chat.last_message_preview.clone())
        .unwrap_or_else(|| "Side chat".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chat(id: &str, parent: Option<&str>, created: i64) -> Chat {
        let at = DateTime::<Utc>::UNIX_EPOCH + chrono::TimeDelta::minutes(created);
        serde_json::from_value(serde_json::json!({
            "id": id, "deviceId": "mac", "title": null, "archived": false,
            "cwd": null, "branch": null, "checkoutId": null, "config": null,
            "lastMessagePreview": null, "lastMessageAt": null,
            "createdAt": at, "spaceId": null, "parentChatId": parent,
        }))
        .unwrap()
    }

    fn entry(id: &str, minutes: i64, status: ChatIndicator) -> ChildEntry {
        let at = DateTime::<Utc>::UNIX_EPOCH + chrono::TimeDelta::minutes(minutes);
        ChildEntry {
            id: id.into(),
            created_at: at,
            activity: at,
            status,
        }
    }

    fn ids(kids: &[ChildEntry]) -> Vec<&str> {
        kids.iter().map(|kid| kid.id.as_str()).collect()
    }

    #[test]
    fn child_index_skips_archived_self_parented_and_hidden_chats() {
        let mut archived = chat("gone", Some("p"), 1);
        archived.archived = true;
        let chats = vec![
            chat("p", None, 0),
            chat("b", Some("p"), 5),
            chat("a", Some("p"), 5),
            chat("first", Some("p"), 2),
            archived,
            chat("me", Some("me"), 3),
            chat("orphan", Some("missing"), 4),
        ];
        let index = ChildIndex::from_chats(chats.iter().map(|chat| (chat, ChatIndicator::Idle)));
        // Creation order, id breaking the tie.
        assert_eq!(ids(index.of("p")), ["first", "a", "b"]);
        assert!(index.of("me").is_empty());
        assert!(index.has("missing"));
        assert!(!index.has("first"));
    }

    #[test]
    fn rollup_picks_the_most_urgent_child() {
        assert_eq!(rollup(&[]), ChatIndicator::Idle);
        let kids = [
            entry("a", 0, ChatIndicator::Completed),
            entry("b", 1, ChatIndicator::Working),
            entry("c", 2, ChatIndicator::AwaitingInput),
        ];
        assert_eq!(rollup(&kids), ChatIndicator::AwaitingInput);
        assert_eq!(rollup(&kids[..2]), ChatIndicator::Working);
    }

    #[test]
    fn a_long_family_keeps_attention_selected_and_beside_then_the_newest() {
        let mut kids: Vec<ChildEntry> = (0..12)
            .map(|ix| entry(&format!("k{ix:02}"), ix, ChatIndicator::Idle))
            .collect();
        kids[0].status = ChatIndicator::Working;
        kids[1].status = ChatIndicator::AwaitingInput;
        let (shown, hidden) = visible_children(&kids, Some("k02"), Some("k03"), false);
        assert_eq!(hidden, 4);
        assert_eq!(
            ids(&shown),
            ["k00", "k01", "k02", "k03", "k08", "k09", "k10", "k11"]
        );
        let (all, hidden) = visible_children(&kids, None, None, true);
        assert_eq!((all.len(), hidden), (12, 0));
        let (few, hidden) = visible_children(&kids[..CHILD_CAP], None, None, false);
        assert_eq!((few.len(), hidden), (CHILD_CAP, 0));
    }

    #[test]
    fn the_child_block_is_rows_and_gaps() {
        assert_eq!(
            CHILD_ROW_HEIGHT,
            sidebar_row_height(true, true, false, false)
        );
        assert_eq!(block_height(0, false), 0.0);
        assert_eq!(block_height(1, false), 31.0);
        assert_eq!(block_height(3, true), 124.0);
    }

    #[test]
    fn attention_means_running_asking_or_failed() {
        assert!(needs_attention(ChatIndicator::Working));
        assert!(needs_attention(ChatIndicator::AwaitingInput));
        assert!(needs_attention(ChatIndicator::Errored));
        assert!(!needs_attention(ChatIndicator::Completed));
        assert!(!needs_attention(ChatIndicator::Idle));
    }
}
