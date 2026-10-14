//! btw: a side chat in a small pane beside the session panel, the card's
//! width, with its own prompt bar under it. The Session row's `+` opens it.
//! It floats at the card's left (in the card's place while the card is
//! away) and never opens the right pane; "Open in sidebar" hands the chat to
//! one. The history button lists the conversation's side chats, so settled
//! ones stay one click away.

use super::*;

use std::cell::Cell;
use std::rc::Rc;

use zeron_proto::ChatIndicator;

use super::session_info::{SESSION_CARD_INSET, SESSION_CARD_WIDTH};

/// The room between the pane and the card, and between the pane and its bar.
pub(super) const BTW_GAP: f32 = 8.0;
const HEADER_HEIGHT: f32 = 34.0;
/// The least the pane opens at, whatever the card's height.
const MIN_HEIGHT: f32 = 220.0;
/// The composer's own side padding. The bar's host bleeds through it, so the
/// prompt pill lines up with the pane.
const COMPOSER_PAD: f32 = 16.0;

pub(super) struct BtwPane {
    /// The conversation it opened from (its panel key).
    key: String,
    /// The side chat it shows (`Shell::side_chats`).
    pub(super) side_chat: u64,
    open: bool,
    /// The conversation's side chats are listed in place of the thread.
    history_open: bool,
    /// The height it opened at: the card's, latched so the pane and its
    /// prompt do not move while the card's sections come and go.
    height: f32,
    /// The bar's painted height, for the room left to the pane.
    bar_height: Rc<Cell<f32>>,
}

/// A fresh side chat off `parent`. Nothing is written until its first send
/// mints it (see [`Shell::create_child_chat`]).
pub(super) fn forked_child(parent: &zeron_proto::Chat) -> zeron_proto::Chat {
    let mut chat = parent.clone();
    chat.id = uuid::Uuid::new_v4().to_string();
    chat.parent_chat_id = Some(parent.id.clone());
    chat.title = None;
    chat.archived = false;
    chat.created_at = Utc::now();
    chat.last_message_at = None;
    chat.last_message_preview = None;
    chat.last_seen_at = None;
    chat.harness_session_id = None;
    chat.harness_session_cwd = None;
    chat.room_gen = Some(2);
    chat
}

/// A header button: an icon with its tooltip.
fn header_button(
    id: &'static str,
    glyph: &'static str,
    tip: &'static str,
    active: bool,
    theme: &Theme,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .debug_selector(move || id.to_string())
        .role(gpui::Role::Button)
        .aria_label(tip)
        .size(px(22.0))
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(5.0))
        .cursor_pointer()
        .when(active, |button| button.bg(crate::theme::ink(0.08)))
        .hover(|style| style.bg(crate::theme::ink(0.08)))
        .tooltip(crate::settings::widgets::text_tooltip(tip))
        .child(icon(glyph).size(px(13.0)).text_color(theme.text_muted))
}

impl Shell {
    /// The Session row's `+`: a new side chat in the pane or, with one
    /// already open for this conversation, its prompt focused.
    pub(super) fn open_btw_pane(&mut self, cx: &mut Context<Self>) {
        let key = self.panel_key(cx);
        if let Some(pane) = self
            .btw
            .as_mut()
            .filter(|pane| pane.key == key && pane.open)
        {
            pane.history_open = false;
            let id = pane.side_chat;
            self.focus_btw(id, cx);
            return;
        }
        let Some(parent) = self.state.read(cx).selected_chat_row().cloned() else {
            self.show_side_chat_error("Start a conversation before creating a side chat.", cx);
            return;
        };
        let id = self.build_side_chat_tab(forked_child(&parent), true, cx);
        self.show_btw(key, id, cx);
    }

    /// Show side chat `id` in the pane of conversation `key`.
    fn show_btw(&mut self, key: String, id: u64, cx: &mut Context<Self>) {
        let height = self
            .btw
            .as_ref()
            .filter(|pane| pane.key == key)
            .map(|pane| pane.height)
            .unwrap_or_else(|| self.session_panel.card_height().max(MIN_HEIGHT));
        if let Some(old) = self.btw.take()
            && old.side_chat != id
        {
            self.release_btw_chat(old.side_chat, cx);
        }
        self.btw = Some(BtwPane {
            key,
            side_chat: id,
            open: true,
            history_open: false,
            height,
            bar_height: Rc::new(Cell::new(0.0)),
        });
        self.focus_btw(id, cx);
    }

    fn focus_btw(&mut self, id: u64, cx: &mut Context<Self>) {
        if let Some(tab) = self.side_chats.get(&id) {
            tab.composer.update(cx, |composer, cx| {
                composer.focus_pending = true;
                cx.notify();
            });
        }
        cx.notify();
    }

    /// Close the pane: it plays its exit, then lets its side chat go.
    pub(super) fn close_btw_pane(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(pane) = self.btw.as_mut().filter(|pane| pane.open) else {
            return false;
        };
        pane.open = false;
        cx.notify();
        true
    }

    /// Whether the pane holds side chat `id`.
    pub(super) fn btw_holds(&self, id: u64) -> bool {
        self.btw.as_ref().is_some_and(|pane| pane.side_chat == id)
    }

    /// Forget the pane at once, its chat moving elsewhere.
    pub(super) fn drop_btw_pane(&mut self) {
        self.btw = None;
        self.btw_shown = false;
        self.btw_tween = None;
    }

    /// The pane's side chat once nothing shows it: dropped unless an unsent
    /// draft is worth keeping (reopening the chat restores it), as when its
    /// right-pane tab closes.
    fn release_btw_chat(&mut self, id: u64, cx: &App) {
        let attached = self
            .right_tabs
            .values()
            .any(|tabs| tabs.contains(&RightSurface::SideChat(id)));
        if !attached
            && self.side_chats.get(&id).is_none_or(|side| {
                side.state.read(cx).side_chat_unsaved() || !side.composer.read(cx).has_draft(cx)
            })
        {
            self.side_chats.remove(&id);
        }
    }

    /// Hand the pane's chat to the right pane, where it opens as a tab.
    pub(super) fn btw_to_sidebar(&mut self, cx: &mut Context<Self>) {
        let Some(pane) = self.btw.take() else {
            return;
        };
        self.drop_btw_pane();
        let chat = self
            .side_chats
            .get(&pane.side_chat)
            .and_then(|tab| tab.state.read(cx).selected_chat_row().cloned());
        if let Some(chat) = chat {
            self.open_side_chat(chat, pane.key, cx);
        }
        cx.notify();
    }

    /// Show one of the conversation's side chats in the pane. One already
    /// open as a right-pane tab is brought forward there instead.
    fn btw_show_chat(&mut self, chat: zeron_proto::Chat, cx: &mut Context<Self>) {
        let Some(key) = self.btw.as_ref().map(|pane| pane.key.clone()) else {
            return;
        };
        let existing = self.side_chat_tab_for(&chat.id, cx);
        if let Some(id) = existing
            && self
                .right_tabs
                .values()
                .any(|tabs| tabs.contains(&RightSurface::SideChat(id)))
        {
            self.close_btw_pane(cx);
            self.open_side_chat(chat, key, cx);
            return;
        }
        let id = match existing {
            Some(id) => id,
            None => self.build_side_chat_tab(chat, false, cx),
        };
        self.show_btw(key, id, cx);
    }

    /// This frame's pane, if it shows for the conversation on screen: its
    /// side chat and its reveal, 0 → 1. It eases in on the resize clock and
    /// out at the quicker exit pace; once a closed pane's exit has played,
    /// its side chat goes.
    pub(super) fn btw_frame(&mut self, cx: &mut Context<Self>) -> Option<(u64, f32)> {
        // A side chat that went away (deleted, or the engine lost) takes the
        // pane with it.
        if self
            .btw
            .as_ref()
            .is_some_and(|pane| !self.side_chats.contains_key(&pane.side_chat))
        {
            self.btw = None;
        }
        let key = self.panel_key(cx);
        let shown = self
            .btw
            .as_ref()
            .is_some_and(|pane| pane.open && pane.key == key);
        let target = if shown { 1.0 } else { 0.0 };
        if shown != self.btw_shown {
            let from = self.eval_tween(self.btw_tween, 1.0 - target);
            self.btw_tween = Some(if shown {
                WidthTween::new(from, target)
            } else {
                WidthTween::exit(from, target)
            });
            self.btw_shown = shown;
        }
        let reveal = self.eval_tween(self.btw_tween, target);
        let (id, open) = self
            .btw
            .as_ref()
            .filter(|pane| pane.key == key)
            .map(|pane| (pane.side_chat, pane.open))?;
        if !open && reveal <= 0.001 {
            self.btw = None;
            self.release_btw_chat(id, cx);
            return None;
        }
        (reveal > 0.001).then_some((id, reveal))
    }

    /// The pane and its prompt bar, placed from the main column's right edge
    /// at the card's left; `card_reveal` is the card's reveal, so while the card is
    /// away the pane takes its place. `max_height` is the room above the
    /// window's bottom chrome.
    pub(super) fn render_btw_pane(
        &mut self,
        id: u64,
        reveal: f32,
        card_reveal: f32,
        max_height: f32,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let tab = self.side_chats.get(&id)?;
        let (state, transcript, composer) = (
            tab.state.clone(),
            tab.transcript.clone(),
            tab.composer.clone(),
        );
        let (history_open, latched, bar_height) = self
            .btw
            .as_ref()
            .map(|pane| (pane.history_open, pane.height, pane.bar_height.clone()))?;
        let theme = Theme::of(cx).for_popup();
        let (chat, status) = {
            let state = state.read(cx);
            let chat = state.selected_chat_row()?.clone();
            let status = state.display_status_for(&chat, Utc::now());
            (chat, status)
        };
        // A pane-sized thread: narrow gutters, and nothing above it to clear.
        transcript.update(cx, |transcript, cx| {
            transcript.set_side_gutter(12.0, cx);
            transcript.set_top_inset(0.0, cx);
            transcript.set_rail_enabled(false, cx);
        });
        composer.update(cx, |composer, cx| {
            composer.set_dock_frame(crate::composer_dock::DockFrame::settled(true), cx);
            composer.set_available_width(SESSION_CARD_WIDTH + 2.0 * COMPOSER_PAD, cx);
        });
        let bar_h = match bar_height.get() {
            height if height > 0.0 => height,
            _ => 120.0,
        };
        let height = latched.min((max_height - bar_h - BTW_GAP).max(160.0));
        let working = status == ChatIndicator::Working;

        let glyph = if working {
            crate::loaders::mini_glyph_spinner(
                "session-btw-working",
                2.0,
                theme.glyph,
                cx.entity_id(),
                cx,
            )
            .into_any_element()
        } else {
            icon(icons::CHAT_ROUND_LINE)
                .size(px(14.0))
                .text_color(theme.text_muted)
                .into_any_element()
        };
        let title: SharedString = chat
            .title
            .clone()
            .filter(|title| !title.trim().is_empty())
            .unwrap_or_else(|| "btw".into())
            .into();
        let header = div()
            .flex_none()
            .h(px(HEADER_HEIGHT))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(4.0))
            .pl(px(12.0))
            .pr(px(6.0))
            .border_b_1()
            .border_color(theme.border)
            .child(
                div()
                    .size(px(16.0))
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(glyph),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .ml(px(4.0))
                    .truncate()
                    .text_size(crate::typography::ui_rems(13.0))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_color(theme.text)
                    .child(title),
            )
            .child(
                header_button(
                    "session-btw-history",
                    icons::CLOCK_CIRCLE,
                    "Side chats",
                    history_open,
                    &theme,
                )
                .on_click(cx.listener(|this, _, _, cx| {
                    if let Some(pane) = this.btw.as_mut() {
                        pane.history_open = !pane.history_open;
                    }
                    cx.notify();
                })),
            )
            .child(
                header_button(
                    "session-btw-sidebar",
                    icons::EXPAND_ARROWS,
                    "Open in sidebar",
                    false,
                    &theme,
                )
                .on_click(cx.listener(|this, _, _, cx| this.btw_to_sidebar(cx))),
            )
            .child(
                header_button("session-btw-close", icons::CLOSE, "Close", false, &theme).on_click(
                    cx.listener(|this, _, _, cx| {
                        this.close_btw_pane(cx);
                    }),
                ),
            );

        let body = if history_open {
            self.render_btw_history(&chat, &theme, cx)
        } else {
            let pill = transcript.read(cx).jump_button_shown().then(|| {
                div()
                    .absolute()
                    .bottom(px(8.0))
                    .left_0()
                    .right_0()
                    .flex()
                    .justify_center()
                    .child(self.jump_pill("btw-jump", "btw-jump-pill", transcript.clone(), cx))
            });
            div()
                .flex_1()
                .min_h_0()
                .relative()
                .child(crate::edge_fade::edge_faded(
                    Theme::TRANSCRIPT_FADE_BAND,
                    true,
                    true,
                    div().size_full().child(transcript),
                ))
                .children(pill)
                .into_any_element()
        };

        let card = div()
            .w(px(SESSION_CARD_WIDTH))
            .h(px(height))
            .rounded(px(popover::CARD_RADIUS))
            .overflow_hidden()
            .border_1()
            .border_color(theme.border)
            .bg(popover::surface_bg(&theme))
            .when(!theme.is_frost(), |card| card.shadow_lg())
            .flex()
            .flex_col()
            .child(header)
            .child(body);
        let pane = div()
            .id("session-btw")
            .debug_selector(|| "session-btw".into())
            .occlude()
            .opacity(reveal)
            // Escape closes an idle pane; a working one keeps its run.
            .on_key_down(cx.listener(move |this, event: &gpui::KeyDownEvent, _, cx| {
                if event.keystroke.key == "escape" && !working && this.close_btw_pane(cx) {
                    cx.stop_propagation();
                }
            }))
            .child(crate::frost::frosted(
                popover::CARD_RADIUS,
                crate::frost::MENU_BLUR,
                card,
            ));

        // The bar trails the pane by a beat.
        let bar_t = ((reveal - 0.25) / 0.75).clamp(0.0, 1.0);
        let probe = gpui::canvas(
            move |bounds, _, _| bar_height.set(f32::from(bounds.size.height)),
            |_, _, _, _| {},
        )
        .absolute()
        .inset_0();
        let bar = div()
            .id("session-btw-bar")
            .debug_selector(|| "session-btw-bar".into())
            .relative()
            .w(px(SESSION_CARD_WIDTH))
            .top(px(8.0 * (1.0 - bar_t)))
            .opacity(bar_t)
            .child(probe)
            .child(
                self.chat_dropzone(("btw-dropzone", id), composer.clone(), cx)
                    .relative()
                    .w(px(SESSION_CARD_WIDTH + 2.0 * COMPOSER_PAD))
                    .ml(px(-COMPOSER_PAD))
                    .child(composer)
                    .child(Self::attachment_drop_overlay(&theme))
                    .child(Self::mention_drop_overlay(&theme)),
            );

        let right =
            SESSION_CARD_INSET + (SESSION_CARD_WIDTH + BTW_GAP) * card_reveal.clamp(0.0, 1.0);
        Some(
            div()
                .absolute()
                .top(px(Theme::TITLEBAR_HEIGHT + 12.0 - 6.0 * (1.0 - reveal)))
                .right(px(right))
                .w(px(SESSION_CARD_WIDTH))
                .flex()
                .flex_col()
                .gap(px(BTW_GAP))
                .child(pane)
                .child(bar)
                .into_any_element(),
        )
    }

    /// The conversation's side chats, most recent activity first: a click
    /// shows one in the pane, a right-click opens its chat menu.
    fn render_btw_history(
        &mut self,
        chat: &zeron_proto::Chat,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let now = Utc::now();
        let rows: Vec<zeron_proto::Chat> = {
            let state = self.state.read(cx);
            let mut rows: Vec<_> = state
                .chats
                .iter()
                .filter(|row| {
                    !row.archived
                        && row.parent_chat_id.is_some()
                        && row.parent_chat_id == chat.parent_chat_id
                })
                .cloned()
                .collect();
            rows.sort_by_key(|row| {
                std::cmp::Reverse(row.last_message_at.unwrap_or(row.created_at))
            });
            rows
        };
        let list = div()
            .id("session-btw-list")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .p(px(popover::CARD_INSET))
            .flex()
            .flex_col()
            .gap(px(popover::MENU_GAP));
        if rows.is_empty() {
            return list
                .child(
                    div()
                        .px(px(9.0))
                        .py(px(5.0))
                        .text_size(crate::typography::ui_rems(12.0))
                        .text_color(theme.text_muted)
                        .child("No other side chats yet"),
                )
                .into_any_element();
        }
        let muted = theme.text_muted.opacity(0.7);
        list.children(rows.into_iter().map(|row| {
            let current = row.id == chat.id;
            let title = row
                .title
                .clone()
                .or_else(|| row.last_message_preview.clone())
                .unwrap_or_else(|| "btw".into());
            let ago = zeron_proto::view::format_time_ago(
                row.last_message_at.unwrap_or(row.created_at),
                now,
            );
            let id = SharedString::from(format!("session-btw-history-{}", row.id));
            let chat_id = row.id.clone();
            div()
                .id(id.clone())
                .debug_selector(move || id.to_string())
                .role(gpui::Role::Button)
                .aria_label(SharedString::from(title.clone()))
                .flex()
                .flex_row()
                .items_center()
                .gap(px(8.0))
                .h(px(28.0))
                .px(px(9.0))
                .rounded(px(popover::MENU_ITEM_RADIUS))
                .cursor_pointer()
                .text_size(crate::typography::ui_rems(13.0))
                .text_color(theme.text)
                .when(current, |row| row.bg(crate::theme::ink(0.05)))
                .hover(|style| style.bg(crate::theme::ink(0.05)))
                .child(
                    icon(icons::CHAT_ROUND_LINE)
                        .size(px(14.0))
                        .flex_none()
                        .text_color(theme.text_muted),
                )
                .child(div().flex_1().min_w_0().truncate().child(title))
                .child(
                    div()
                        .flex_none()
                        .text_size(crate::typography::ui_rems(11.0))
                        .text_color(muted)
                        .child(ago),
                )
                .on_click(cx.listener(move |this, _, _, cx| this.btw_show_chat(row.clone(), cx)))
                .on_mouse_down(
                    MouseButton::Right,
                    cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                        cx.stop_propagation();
                        this.chat_menu.open(ChatMenuState {
                            chat_id: chat_id.clone(),
                            tab: None,
                            position: event.position,
                            page: ChatMenuPage::Root,
                        });
                        cx.notify();
                    }),
                )
        }))
        .into_any_element()
    }
}
