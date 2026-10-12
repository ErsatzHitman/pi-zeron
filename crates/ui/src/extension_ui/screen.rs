//! Screens: elements placed `screen` (the btw side thread, a subagent's
//! live viewer) open as a right-pane tab, like a side chat or a subagent
//! transcript. The tab follows its element by key; when the extension
//! removes it, the tab closes itself.

use std::collections::HashSet;
use std::hash::{Hash, Hasher};

use gpui::{
    Context, Entity, EventEmitter, FocusHandle, Focusable, ScrollHandle, SharedString,
    Subscription, Window, div, prelude::*, px,
};
use zeron_proto::extension_ui::{
    PiUiElement, PiUiField, PiUiFieldKind, PiUiPlacement, PiUiSection, PiUiTone, PiUiVariant,
};

use super::parts::{self, Actions, MarkdownCache, PendingConfirm};
use crate::composer::{Composer, ComposerEvent, ComposerInput, ComposerInputEvent};
use crate::icons::{self, icon};
use crate::state::AppState;
use crate::theme::Theme;

/// The screen's element is gone: its tab should close.
pub(crate) struct ScreenClosed;

pub(crate) struct ScreenView {
    state: Entity<AppState>,
    pub chat_id: String,
    pub key: String,
    title: SharedString,
    /// The pinned composer form (btw's question box), rebuilt per form.
    input: Option<(u64, String, Entity<ComposerInput>)>,
    _input: Option<Subscription>,
    markdown: MarkdownCache,
    pending: Option<PendingConfirm>,
    scroll: ScrollHandle,
    focus: FocusHandle,
    seen: bool,
    _state: Subscription,
}

impl EventEmitter<ScreenClosed> for ScreenView {}

impl Focusable for ScreenView {
    fn focus_handle(&self, _: &gpui::App) -> FocusHandle {
        self.focus.clone()
    }
}

/// The screen's inline composer: a form section placed `pinned` (btw), or a
/// form section with one text field.
fn composer_form(
    element: &PiUiElement,
) -> Option<(Vec<PiUiField>, Vec<zeron_proto::extension_ui::PiUiAction>)> {
    element.sections.iter().find_map(|section| match section {
        PiUiSection::Form {
            fields,
            actions,
            placement,
            ..
        } if *placement == Some(PiUiPlacement::Pinned)
            || (fields.len() == 1
                && matches!(
                    fields[0].kind,
                    PiUiFieldKind::Text | PiUiFieldKind::Textarea
                )) =>
        {
            Some((fields.clone(), actions.clone()))
        }
        _ => None,
    })
}

fn form_signature(fields: &[PiUiField]) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    for field in fields {
        field.id.hash(&mut hasher);
        field.placeholder.hash(&mut hasher);
    }
    hasher.finish()
}

/// A screen's tab title.
pub(crate) fn screen_title(element: &PiUiElement) -> String {
    element
        .title
        .clone()
        .filter(|t| !t.trim().is_empty())
        .unwrap_or_else(|| element.ns.clone())
}

pub(crate) fn screen_icon(ns: &str) -> &'static str {
    match ns {
        "btw" => icons::CHAT_ROUND_LINE,
        "subagents" => icons::BOT,
        _ => icons::WIDGET,
    }
}

impl ScreenView {
    pub fn new(
        state: Entity<AppState>,
        chat_id: String,
        key: String,
        title: String,
        cx: &mut Context<Self>,
    ) -> Self {
        let observe = cx.observe(&state, |this: &mut Self, _, cx| {
            let present = this.element(cx).is_some();
            let ready = this.state.read(cx).extension_ui.ready_for(&this.chat_id);
            if present {
                this.seen = true;
            } else if this.seen && ready {
                cx.emit(ScreenClosed);
            }
            cx.notify();
        });
        Self {
            state,
            chat_id,
            key,
            title: title.into(),
            input: None,
            _input: None,
            markdown: MarkdownCache::default(),
            pending: None,
            scroll: ScrollHandle::new(),
            focus: cx.focus_handle(),
            seen: false,
            _state: observe,
        }
    }

    fn element(&self, cx: &gpui::App) -> Option<PiUiElement> {
        self.state
            .read(cx)
            .extension_ui
            .of(&self.chat_id)?
            .typed_elements()
            .into_iter()
            .find(|e| e.key() == self.key)
    }

    /// The owning extension (`ns` of `ns:id`).
    pub fn ns(&self) -> String {
        self.key.split(':').next().unwrap_or_default().to_owned()
    }

    pub fn title(&self, cx: &gpui::App) -> SharedString {
        self.element(cx)
            .map(|e| SharedString::from(screen_title(&e)))
            .unwrap_or_else(|| self.title.clone())
    }

    /// Whether the screen reports work in flight (a streaming reply, a live
    /// subagent), for the tab's spinner.
    pub fn working(&self, cx: &gpui::App) -> bool {
        self.element(cx).is_some_and(|e| {
            e.extra_str("state") == Some("running")
                || e.sections.iter().any(|s| match s {
                    PiUiSection::Status { tone, .. } => *tone == Some(PiUiTone::Busy),
                    PiUiSection::Turns { turns, .. } => turns.iter().any(|t| t.streaming),
                    _ => false,
                })
        })
    }

    fn actions(&self, cx: &mut Context<Self>) -> Actions {
        let state = self.state.clone();
        let view = cx.weak_entity();
        let confirm_view = view.clone();
        Actions {
            send: std::rc::Rc::new(move |action, _, cx| {
                state.update(cx, |state, cx| state.extension_ui_action(action, cx));
                view.update(cx, |screen, cx| {
                    screen.pending = None;
                    cx.notify();
                })
                .ok();
            }),
            confirm: std::rc::Rc::new(move |pending, _, cx| {
                confirm_view
                    .update(cx, |screen, cx| {
                        screen.pending = pending;
                        cx.notify();
                    })
                    .ok();
            }),
            pending: self.pending.clone(),
        }
    }

    fn sync_input(&mut self, element: &PiUiElement, cx: &mut Context<Self>) {
        let Some((fields, _)) = composer_form(element) else {
            self.input = None;
            self._input = None;
            return;
        };
        let Some(field) = fields
            .iter()
            .find(|f| matches!(f.kind, PiUiFieldKind::Text | PiUiFieldKind::Textarea))
        else {
            return;
        };
        let signature = form_signature(&fields);
        if self
            .input
            .as_ref()
            .is_some_and(|(sig, _, _)| *sig == signature)
        {
            return;
        }
        let placeholder = field
            .placeholder
            .clone()
            .unwrap_or_else(|| "Ask a side question".into());
        let input = cx.new(|cx| ComposerInput::new(placeholder, cx).with_single_line());
        self._input = Some(cx.subscribe(&input, |this, _, event, cx| {
            if matches!(
                event,
                ComposerInputEvent::Submitted | ComposerInputEvent::ModifiedSubmitted
            ) {
                this.send_form("submit", cx);
            }
        }));
        self.input = Some((signature, field.id.clone(), input));
    }

    fn send_form(&mut self, action_id: &str, cx: &mut Context<Self>) {
        let Some((_, field, input)) = self.input.clone() else {
            return;
        };
        let text = input.read(cx).text().trim().to_owned();
        let value = if action_id == "submit" || action_id == "send" {
            if text.is_empty() {
                return;
            }
            input.update(cx, |input, cx| input.set_text(String::new(), cx));
            Some(serde_json::json!({ "fields": { field: text } }))
        } else {
            None
        };
        let request = parts::action_of(&self.key, None, action_id, value);
        self.state
            .update(cx, |state, cx| state.extension_ui_action(request, cx));
    }
}

impl Render for ScreenView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx).clone();
        let Some(element) = self.element(cx) else {
            let selected =
                self.state.read(cx).selected_chat.as_deref() == Some(self.chat_id.as_str());
            return div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .p(px(24.0))
                .text_size(px(parts::TEXT_SIZE))
                .text_color(theme.text_faint)
                .child(if selected {
                    "This view has closed."
                } else {
                    "Open its conversation to follow this view."
                })
                .into_any_element();
        };
        self.seen = true;
        self.sync_input(&element, cx);
        let actions = self.actions(cx);
        let view = cx.entity_id();
        let key = element.key();
        let form = composer_form(&element);

        let header = div()
            .flex_none()
            .px(px(12.0))
            .py(px(8.0))
            .flex()
            .items_center()
            .gap(px(8.0))
            .border_b_1()
            .border_color(theme.hairline(0.06))
            .child(
                icon(screen_icon(&element.ns))
                    .size(px(14.0))
                    .text_color(theme.text_muted),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .truncate()
                            .text_size(px(13.0))
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_color(theme.text)
                            .child(SharedString::from(screen_title(&element))),
                    )
                    .when_some(element.text.clone(), |el, text| {
                        el.child(
                            div()
                                .truncate()
                                .text_size(px(11.5))
                                .text_color(theme.text_faint)
                                .child(SharedString::from(text)),
                        )
                    }),
            )
            .children(element.actions.iter().enumerate().map(|(ix, action)| {
                parts::action_button(
                    &theme,
                    SharedString::from(format!("{key}-screen-action-{ix}")),
                    &key,
                    None,
                    action,
                    &actions,
                )
            }));

        let mut body = Vec::new();
        for (ix, section) in element.sections.iter().enumerate() {
            if matches!(section, PiUiSection::Form { .. }) {
                continue;
            }
            if let Some(rendered) = parts::section(
                &theme,
                &key,
                ix,
                section,
                &actions,
                window,
                &self.markdown,
                view,
                cx,
            ) {
                body.push(rendered);
            }
        }
        if body.is_empty() && !element.lines.is_empty() {
            body.push(parts::log(&theme, &element.lines, None));
        }

        let composer = form.map(|(_, form_actions)| {
            let input = self.input.as_ref().map(|(_, _, input)| input.clone());
            div()
                .flex_none()
                .p(px(10.0))
                .border_t_1()
                .border_color(theme.hairline(0.06))
                .flex()
                .items_center()
                .gap(px(6.0))
                .when_some(input, |el, input| {
                    el.child(
                        crate::popover::dialog_field(input.into_any_element())
                            .flex_1()
                            .min_w_0(),
                    )
                })
                .children(form_actions.into_iter().enumerate().map(|(ix, action)| {
                    let id = action.id.clone();
                    let primary =
                        action.variant == Some(PiUiVariant::Primary) || action.id == "submit";
                    let label = if action.label.is_empty() {
                        action.id.clone()
                    } else {
                        action.label.clone()
                    };
                    let button = if primary {
                        crate::popover::btn_primary(&theme, &label)
                    } else {
                        crate::popover::btn_ghost(&theme, &label, format!("{key}-screen-form-{ix}"))
                    };
                    button
                        .id(SharedString::from(format!("{key}-screen-form-action-{ix}")))
                        .when(action.disabled, |el| el.opacity(0.4))
                        .when(!action.disabled, |el| {
                            el.on_click(cx.listener(move |this, _, _, cx| this.send_form(&id, cx)))
                        })
                        .into_any_element()
                }))
        });

        div()
            .id(SharedString::from(format!("{key}-screen")))
            .track_focus(&self.focus)
            .size_full()
            .flex()
            .flex_col()
            .child(header)
            .children(parts::confirm_row(&theme, &key, &actions))
            .child(
                div()
                    .id(SharedString::from(format!("{key}-screen-body")))
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .track_scroll(&self.scroll)
                    .py(px(10.0))
                    .px(px(4.0))
                    .flex()
                    .flex_col()
                    .gap(px(8.0))
                    .children(body),
            )
            .children(composer)
            .into_any_element()
    }
}

impl Composer {
    /// Ask the shell to open `key` (a screen of the selected chat) as a tab.
    pub(crate) fn open_extension_screen(
        &mut self,
        key: &str,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(chat_id) = self.state.read(cx).selected_chat.clone() else {
            return;
        };
        let title = self
            .state
            .read(cx)
            .extension_ui
            .of(&chat_id)
            .and_then(|state| {
                state
                    .typed_elements()
                    .into_iter()
                    .find(|e| e.key() == key)
                    .map(|e| screen_title(&e))
            })
            .unwrap_or_else(|| key.to_owned());
        cx.emit(ComposerEvent::OpenExtensionScreen {
            chat_id,
            key: key.to_owned(),
            title,
        });
    }

    /// Screens that appeared since the last frame open by themselves (the
    /// user asked for them: `/btw`, a fleet row's View). Ones already there
    /// when the chat was opened stay trays until clicked.
    pub(crate) fn open_new_extension_screens(&mut self, cx: &mut Context<Self>) {
        if self.side_chat {
            return;
        }
        let (chat_id, screens) = {
            let state = self.state.read(cx);
            let Some(chat_id) = state.selected_chat.clone() else {
                return;
            };
            if !state.extension_ui.ready_for(&chat_id) {
                return;
            }
            let Some(live) = state.extension_ui.of(&chat_id) else {
                return;
            };
            let screens: Vec<(String, String)> = live
                .typed_elements()
                .into_iter()
                .filter(|e| e.placement == PiUiPlacement::Screen)
                .map(|e| (e.key(), screen_title(&e)))
                .collect();
            (chat_id, screens)
        };
        let primed = self.extension.screens.contains_key(&chat_id);
        let known = self.extension.screens.entry(chat_id.clone()).or_default();
        let fresh: Vec<(String, String)> = screens
            .iter()
            .filter(|(key, _)| !known.contains(key))
            .cloned()
            .collect();
        *known = screens
            .into_iter()
            .map(|(key, _)| key)
            .collect::<HashSet<_>>();
        if !primed {
            return;
        }
        for (key, title) in fresh {
            cx.emit(ComposerEvent::OpenExtensionScreen {
                chat_id: chat_id.clone(),
                key,
                title,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn btw_has_a_pinned_composer_and_a_viewer_has_none() {
        let btw: PiUiElement =
            serde_json::from_value(json!({"id":"thread","ns":"btw","kind":"panel",
            "placement":"screen","title":"btw","sections":[
                {"kind":"meta","id":"btw-meta","text":"anthropic/claude · high"},
                {"kind":"turns","id":"btw-transcript","turns":[{"role":"user","text":"why?"},
                    {"role":"assistant","text":"because","streaming":true}]},
                {"kind":"form","id":"btw-composer","placement":"pinned",
                 "fields":[{"id":"q","kind":"text"}],
                 "actions":[{"id":"submit","label":"Send"},{"id":"cancel","label":"Stop"}]}]}))
            .unwrap();
        let (fields, actions) = composer_form(&btw).unwrap();
        assert_eq!(fields[0].id, "q");
        assert_eq!(actions.len(), 2);
        assert_eq!(screen_icon(&btw.ns), icons::CHAT_ROUND_LINE);
        let viewer: PiUiElement = serde_json::from_value(json!({"id":"viewer:7","ns":"subagents",
            "kind":"panel","placement":"screen","title":"sub-agent scout","jobId":"7","state":"running",
            "sections":[{"kind":"turns","id":"transcript","turns":[]}],
            "actions":[{"id":"close","label":"Close"},{"id":"kill","label":"Kill","variant":"danger"}]}))
        .unwrap();
        assert!(composer_form(&viewer).is_none());
        assert_eq!(screen_title(&viewer), "sub-agent scout");
    }
}
