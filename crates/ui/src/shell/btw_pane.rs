//! btw: a side chat in a small floating pane. The Session row's `+` opens
//! it at the window's bottom left; its header drags it anywhere, over the
//! sidebar, the transcript and the panes alike, and it stays where it was
//! left for the next side chat. It is a fixed size: the thread scrolls
//! inside it. It runs its parent's model (no picker). The history button
//! lists the conversation's btw asides, so settled ones stay one click away.
//! Like Claude Code's side chat, its prompt has no attachments or
//! microphone (it grows to more lines as it fills) and it shows only the
//! answers: while a reply works, its trailer ("Puzzling…") stands in for
//! the thinking and tool rows.

use super::*;

use std::cell::Cell;
use std::rc::Rc;

use zeron_proto::ChatIndicator;

/// The pane's size, unless the window is smaller.
pub(super) const BTW_WIDTH: f32 = 360.0;
pub(super) const BTW_HEIGHT: f32 = 460.0;
/// Its room from the window's edges where it opens, and the least it keeps
/// once dragged.
const BTW_MARGIN: f32 = 12.0;
const HEADER_HEIGHT: f32 = 34.0;
/// The composer's own side padding. The bar's host bleeds through it, so
/// the prompt pill sits [`BAR_INSET`] in from the pane's edges.
const COMPOSER_PAD: f32 = 16.0;
const BAR_INSET: f32 = 8.0;

/// The pane's size in a `viewport`-sized window: its own, or the window's
/// room inside the margins.
fn pane_size(viewport: (f32, f32)) -> (f32, f32) {
    (
        BTW_WIDTH.min(viewport.0 - 2.0 * BTW_MARGIN).max(0.0),
        BTW_HEIGHT.min(viewport.1 - 2.0 * BTW_MARGIN).max(0.0),
    )
}

/// Where a `size`d pane's top left sits: where it was dragged to, else the
/// bottom left, kept whole inside the window's margins.
fn pane_origin(dragged: Option<(f32, f32)>, size: (f32, f32), viewport: (f32, f32)) -> (f32, f32) {
    let right = (viewport.0 - size.0 - BTW_MARGIN).max(BTW_MARGIN);
    let bottom = (viewport.1 - size.1 - BTW_MARGIN).max(BTW_MARGIN);
    let (x, y) = dragged.unwrap_or((BTW_MARGIN, bottom));
    (x.clamp(BTW_MARGIN, right), y.clamp(BTW_MARGIN, bottom))
}

pub(super) struct BtwPane {
    /// The conversation it opened from (its panel key).
    key: String,
    /// The side chat it shows (`Shell::side_chats`).
    pub(super) side_chat: u64,
    open: bool,
    /// The conversation's side chats are listed in place of the thread.
    history_open: bool,
    /// The bar's painted height, for the room left to the thread.
    bar_height: Rc<Cell<f32>>,
    /// While the header is held: the press's offset into the pane.
    grab: Option<(f32, f32)>,
    /// Closed by its cross: the side chat goes with it, Pi process and all.
    delete_on_close: bool,
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

/// The conversation's btw asides: its unarchived btw chats, most recent
/// activity first. The sessions its agent started through the Zeron MCP
/// name the same parent, but they are sessions (the sidebar and the session
/// panel list them), never asides.
fn side_chat_history(
    chats: &[zeron_proto::Chat],
    chat: &zeron_proto::Chat,
) -> Vec<zeron_proto::Chat> {
    let mut rows: Vec<_> = chats
        .iter()
        .filter(|row| {
            !row.archived
                && row.is_btw()
                && row.parent_chat_id.is_some()
                && row.parent_chat_id == chat.parent_chat_id
        })
        .cloned()
        .collect();
    rows.sort_by_key(|row| std::cmp::Reverse(row.last_message_at.unwrap_or(row.created_at)));
    rows
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
        let mut chat = forked_child(&parent);
        chat.id = format!("{}{}", zeron_proto::BTW_CHAT_PREFIX, chat.id);
        let id = self.build_side_chat_tab(chat, true, cx);
        self.show_btw(key, id, cx);
    }

    /// The selection bar's "Send to /btw": the pane opens (or comes forward)
    /// with `text` quoted in its prompt, ready for the question about it.
    pub(super) fn send_to_btw(&mut self, text: &str, cx: &mut Context<Self>) {
        self.open_btw_pane(cx);
        let key = self.panel_key(cx);
        let Some(id) = self
            .btw
            .as_ref()
            .filter(|pane| pane.open && pane.key == key)
            .map(|pane| pane.side_chat)
        else {
            return;
        };
        if let Some(tab) = self.side_chats.get(&id) {
            tab.composer
                .update(cx, |composer, cx| composer.quote_into_input(text, cx));
        }
        cx.notify();
    }

    /// Show side chat `id` in the pane of conversation `key`.
    fn show_btw(&mut self, key: String, id: u64, cx: &mut Context<Self>) {
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
            bar_height: Rc::new(Cell::new(0.0)),
            grab: None,
            delete_on_close: false,
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

    /// Close the pane for good (its cross): once the exit has played, its
    /// side chat is deleted, which also ends its Pi process and takes it off
    /// the session lists. One kept as a right-pane tab stays.
    pub(super) fn delete_btw_pane(&mut self, cx: &mut Context<Self>) -> bool {
        if let Some(pane) = self.btw.as_mut().filter(|pane| pane.open) {
            pane.delete_on_close = true;
        }
        self.close_btw_pane(cx)
    }

    /// The engine chat id behind side chat tab `id`, when it is a minted btw
    /// chat no right-pane tab shows: the one a closing pane may delete.
    pub(super) fn deletable_btw_chat(&self, id: u64, cx: &App) -> Option<String> {
        let attached = self
            .right_tabs
            .values()
            .any(|tabs| tabs.contains(&RightSurface::SideChat(id)));
        let state = self.side_chats.get(&id)?.state.read(cx);
        (!attached && !state.side_chat_unsaved())
            .then(|| state.selected_chat.clone())
            .flatten()
            .filter(|chat| chat.starts_with(zeron_proto::BTW_CHAT_PREFIX))
    }

    /// Delete btw chat `chat_id` on the engine: its row, its transcript and
    /// its Pi process. Detached, so a mutate in flight is not cancelled.
    fn delete_btw_chat(&mut self, chat_id: String, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        cx.spawn(async move |_, _| {
            let params = serde_json::json!({ "op": "deleteChat", "chatId": chat_id });
            if let Err(err) = engine.client().call(methods::MUTATE, params).await {
                tracing::warn!(error = %err, "btw: deleting the closed side chat failed");
            }
        })
        .detach();
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

    /// Until its first send, the pane's side chat `id` runs its parent's
    /// model: it takes the open conversation's config as it is now.
    pub(super) fn btw_follow_parent_model(&mut self, id: u64, cx: &mut Context<Self>) {
        let Some(config) = self
            .state
            .read(cx)
            .selected_chat_row()
            .map(|parent| parent.config.clone())
        else {
            return;
        };
        if let Some(tab) = self.side_chats.get(&id) {
            tab.state.update(cx, |state, cx| {
                state.inherit_side_chat_config(config, cx);
            });
        }
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
            let delete = self.btw.take().is_some_and(|pane| pane.delete_on_close);
            let doomed = if delete {
                self.deletable_btw_chat(id, cx)
            } else {
                None
            };
            if let Some(chat_id) = doomed {
                self.side_chats.remove(&id);
                self.delete_btw_chat(chat_id, cx);
            } else {
                self.release_btw_chat(id, cx);
            }
            return None;
        }
        (reveal > 0.001).then_some((id, reveal))
    }

    /// The pane, its prompt bar inside it, floating over the window at its
    /// bottom left or where its header was dragged to.
    pub(super) fn render_btw_pane(
        &mut self,
        id: u64,
        reveal: f32,
        viewport: gpui::Size<Pixels>,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let tab = self.side_chats.get(&id)?;
        let (state, transcript, composer) = (
            tab.state.clone(),
            tab.transcript.clone(),
            tab.composer.clone(),
        );
        let (history_open, bar_height) = self
            .btw
            .as_ref()
            .map(|pane| (pane.history_open, pane.bar_height.clone()))?;
        let dragged = self.btw_dragged;
        let theme = Theme::of(cx).for_popup();
        self.btw_follow_parent_model(id, cx);
        let (chat, status) = {
            let state = state.read(cx);
            let chat = state.selected_chat_row()?.clone();
            let status = state.display_status_for(&chat, Utc::now());
            (chat, status)
        };
        let window = (f32::from(viewport.width), f32::from(viewport.height));
        let (width, height) = pane_size(window);
        let (x, y) = pane_origin(dragged, (width, height), window);
        // A pane-sized thread: narrow gutters, nothing above it to clear, and
        // sized to its content rather than holding a runway for the reply.
        transcript.update(cx, |transcript, cx| {
            transcript.set_side_gutter(12.0, cx);
            transcript.set_top_inset(0.0, cx);
            transcript.set_rail_enabled(false, cx);
            transcript.set_fit_content(true, cx);
            // Its answers only: "Puzzling…" stands in for the work.
            transcript.set_answers_only(true, cx);
        });
        let bar_width = width - 2.0 * (BAR_INSET - COMPOSER_PAD);
        composer.update(cx, |composer, cx| {
            composer.set_dock_frame(crate::composer_dock::DockFrame::settled(true), cx);
            composer.set_available_width(bar_width, cx);
            composer.set_model_picker_hidden(true, cx);
            composer.set_minimal(true, cx);
        });
        // The thread's room as the last frame measured the bar.
        let body_h = height - 2.0 - HEADER_HEIGHT - bar_height.get().max(48.0) - BAR_INSET;
        let content = transcript.read(cx).content_height();
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
        // The header drags the pane: a press keeps its offset into the pane,
        // and the window-wide listeners below move it until the release.
        let header = div()
            .id("session-btw-header")
            .debug_selector(|| "session-btw-header".into())
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
            .cursor_grab()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, event: &MouseDownEvent, _, _| {
                    if let Some(pane) = this.btw.as_mut() {
                        pane.grab = Some((
                            f32::from(event.position.x) - x,
                            f32::from(event.position.y) - y,
                        ));
                    }
                }),
            )
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
                header_button("session-btw-close", icons::CLOSE, "Close", false, &theme).on_click(
                    cx.listener(|this, _, _, cx| {
                        this.delete_btw_pane(cx);
                    }),
                ),
            );
        let drag = {
            let shell = cx.entity().downgrade();
            gpui::canvas(
                |_, _, _| {},
                move |_, _, window, _| {
                    let moving = shell.clone();
                    window.on_mouse_event(move |event: &gpui::MouseMoveEvent, phase, _, cx| {
                        if phase != gpui::DispatchPhase::Bubble {
                            return;
                        }
                        let _ = moving.update(cx, |this, cx| {
                            let Some((dx, dy)) = this.btw.as_ref().and_then(|pane| pane.grab)
                            else {
                                return;
                            };
                            if event.pressed_button == Some(MouseButton::Left) {
                                this.btw_dragged = Some((
                                    f32::from(event.position.x) - dx,
                                    f32::from(event.position.y) - dy,
                                ));
                            } else if let Some(pane) = this.btw.as_mut() {
                                pane.grab = None;
                            }
                            cx.notify();
                        });
                    });
                    let release = shell.clone();
                    window.on_mouse_event(move |_: &gpui::MouseUpEvent, phase, _, cx| {
                        if phase == gpui::DispatchPhase::Bubble {
                            let _ = release.update(cx, |this, _| {
                                if let Some(pane) = this.btw.as_mut() {
                                    pane.grab = None;
                                }
                            });
                        }
                    });
                },
            )
            .absolute()
            .size_0()
        };

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
            // The thread fades at its edges only once it scrolls.
            let clipped = content > body_h + 0.5;
            let hint = (content <= 0.0).then(|| {
                div()
                    .absolute()
                    .top(px(10.0))
                    .left(px(10.0))
                    .right(px(10.0))
                    .p(px(10.0))
                    .rounded(px(8.0))
                    .bg(theme.ink(0.04))
                    .text_size(crate::typography::ui_rems(12.0))
                    .line_height(crate::typography::ui_rems(18.0))
                    .text_color(theme.text_faint)
                    .child(SharedString::from(
                        "Ask about this session without touching the main thread. \
                         It sees the session so far, can read files and search the web, \
                         and nothing here is added to it.",
                    ))
            });
            div()
                .size_full()
                .relative()
                .children(hint)
                .child(crate::edge_fade::edge_faded(
                    Theme::TRANSCRIPT_FADE_BAND,
                    clipped,
                    clipped,
                    div().size_full().child(transcript.clone()),
                ))
                .children(pill)
                .into_any_element()
        };

        // The bar trails the pane by a beat.
        let bar_t = ((reveal - 0.25) / 0.75).clamp(0.0, 1.0);
        let bar = div()
            .id("session-btw-bar")
            .debug_selector(|| "session-btw-bar".into())
            .flex_none()
            .relative()
            .w_full()
            .pb(px(BAR_INSET))
            .top(px(8.0 * (1.0 - bar_t)))
            .opacity(bar_t)
            .child(
                gpui::canvas(
                    move |bounds, _, _| bar_height.set(f32::from(bounds.size.height) - BAR_INSET),
                    |_, _, _, _| {},
                )
                .absolute()
                .inset_0(),
            )
            // A bare prompt bar: no attachments, so no drop zone.
            .child(
                div()
                    .relative()
                    .w(px(bar_width))
                    .ml(px(BAR_INSET - COMPOSER_PAD))
                    .child(composer),
            );

        // A fixed size: the thread takes what the header and the bar leave.
        let card = div()
            .w(px(width))
            .h(px(height))
            .rounded(px(popover::CARD_RADIUS))
            .overflow_hidden()
            .border_1()
            .border_color(theme.border)
            .bg(popover::surface_bg(&theme))
            .when(!theme.is_frost(), |card| card.shadow_lg())
            .relative()
            .flex()
            .flex_col()
            .child(header)
            .child(div().flex_1().min_h_0().child(body))
            .child(bar)
            .child(drag);
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

        Some(
            div()
                .absolute()
                .left(px(x))
                .top(px(y + 6.0 * (1.0 - reveal)))
                .w(px(width))
                .child(pane)
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
        let rows = side_chat_history(&self.state.read(cx).chats, chat);
        let list = div()
            .id("session-btw-list")
            .size_full()
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn it_opens_at_the_bottom_left_and_stays_on_screen_where_dragged() {
        let window = (1600.0, 900.0);
        let size = pane_size(window);
        assert_eq!(size, (BTW_WIDTH, BTW_HEIGHT));
        assert_eq!(
            pane_origin(None, size, window),
            (BTW_MARGIN, 900.0 - BTW_HEIGHT - BTW_MARGIN)
        );
        assert_eq!(
            pane_origin(Some((700.0, 200.0)), size, window),
            (700.0, 200.0)
        );
        // Dragged past an edge, it stops inside the margins.
        assert_eq!(
            pane_origin(Some((-50.0, 2000.0)), size, window),
            (BTW_MARGIN, 900.0 - BTW_HEIGHT - BTW_MARGIN)
        );
        assert_eq!(
            pane_origin(Some((5000.0, -9.0)), size, window),
            (1600.0 - BTW_WIDTH - BTW_MARGIN, BTW_MARGIN)
        );
        // A small window shrinks it to its room.
        assert_eq!(pane_size((300.0, 400.0)), (276.0, 376.0));
    }

    fn chat(id: &str, parent: Option<&str>, archived: bool, minute: u32) -> zeron_proto::Chat {
        serde_json::from_value(serde_json::json!({
            "id": id, "deviceId": "dev", "title": id, "archived": archived,
            "config": null, "createdAt": "2026-10-14T00:00:00Z", "parentChatId": parent,
            "lastMessageAt": format!("2026-10-14T00:{minute:02}:00Z"),
        }))
        .unwrap()
    }

    #[test]
    fn the_history_lists_the_conversation_s_btw_asides_only() {
        let chats = vec![
            chat("main", None, false, 0),
            // The pane's open aside (its parent is the conversation).
            chat("btw-now", Some("main"), false, 5),
            chat("btw-older", Some("main"), false, 2),
            chat("btw-archived", Some("main"), true, 3),
            // Sessions the agent started through the Zeron MCP name the same
            // parent: never asides.
            chat("mcp-worker", Some("main"), false, 4),
            // Another conversation's asides stay out too.
            chat("btw-elsewhere", Some("other"), false, 6),
        ];
        let now = chats.iter().find(|c| c.id == "btw-now").unwrap().clone();
        let history = side_chat_history(&chats, &now);
        let ids: Vec<&str> = history.iter().map(|row| row.id.as_str()).collect();
        assert_eq!(ids, ["btw-now", "btw-older"], "newest first, no workers");
    }
}
