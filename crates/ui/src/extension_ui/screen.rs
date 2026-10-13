//! Screens: elements placed `screen` (the btw side thread, a subagent's
//! live viewer, a command's report). They dock in the composer's place, in
//! the prompt panel (see `prompts`), drawn like the question panel. The view
//! follows its element by key; when the extension removes it, it closes.

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
    /// What the body last held, to follow a growing thread to its end.
    tail: usize,
    /// Drawn in the composer's place rather than as a pane tab.
    docked: bool,
    /// The form's input takes the keyboard once it exists.
    autofocus: bool,
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
        "workspace" => icons::TUNING,
        "usage" | "clinepass" => icons::FILE_DATA,
        "workflows" => icons::FILE_MARKDOWN,
        "todo" => icons::CHECKLIST,
        "llama" => icons::HARD_DRIVE,
        "mcp" => icons::REMOTE_SERVER,
        _ => icons::WIDGET,
    }
}

/// Header actions that no longer fit beside the title (the usage dashboard's
/// period buttons) move to a wrapping toolbar under it.
const HEADER_ACTIONS: usize = 3;

fn actions_in_toolbar(element: &PiUiElement) -> bool {
    element.actions.len() > HEADER_ACTIONS
}

/// What a docked screen is, over its title, as the question panel's header
/// names what asks.
fn screen_kind(ns: &str) -> &str {
    match ns {
        "btw" => "Side question",
        "subagents" => "Subagent",
        "usage" | "clinepass" => "Usage",
        "workspace" => "Workspace",
        "workflows" => "Workflow",
        "todo" => "Todo",
        "llama" | "llama-models" => "Models",
        "mcp" | "mcp-servers" => "MCP servers",
        other => other,
    }
}

/// The action that closes a screen from its header's X, when its own
/// actions offer none.
fn close_action(element: &PiUiElement) -> Option<String> {
    let offered = element
        .actions
        .iter()
        .any(|action| action.id == "close" || action.id == "cancel");
    (!offered).then(|| {
        element
            .extra_str("closeActionId")
            .unwrap_or("close")
            .to_owned()
    })
}

/// How much a screen's body holds, to tell when it grew.
fn body_size(element: &PiUiElement) -> usize {
    element
        .sections
        .iter()
        .map(|section| match section {
            PiUiSection::Turns { turns, .. } => {
                turns.iter().map(|turn| turn.text.len() + 1).sum::<usize>()
            }
            PiUiSection::Markdown { text, .. } => text.len(),
            _ => 1,
        })
        .sum()
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
            tail: 0,
            docked: false,
            autofocus: false,
            _state: observe,
        }
    }

    /// Drawn in the composer's place, in the prompt panel.
    pub fn docked(mut self) -> Self {
        self.docked = true;
        self.autofocus = true;
        self
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

impl ScreenView {
    fn send_action(&mut self, action_id: &str, cx: &mut Context<Self>) {
        let request = parts::action_of(&self.key, None, action_id, None);
        self.state
            .update(cx, |state, cx| state.extension_ui_action(request, cx));
    }

    /// The screen in the composer's place: the question panel's header,
    /// the body scrolling under it, and its form as the panel's input row.
    fn render_docked(&mut self, window: &mut Window, cx: &mut Context<Self>) -> gpui::AnyElement {
        let theme = Theme::of(cx).clone();
        let Some(element) = self.element(cx) else {
            return gpui::Empty.into_any_element();
        };
        self.seen = true;
        self.sync_input(&element, cx);
        if self.autofocus
            && let Some((_, _, input)) = self.input.as_ref()
        {
            self.autofocus = false;
            let handle = input.read(cx).focus_handle(cx);
            window.focus(&handle, cx);
        }
        let size = body_size(&element);
        if size != self.tail {
            self.tail = size;
            self.scroll.scroll_to_bottom();
        }
        let actions = self.actions(cx);
        let view = cx.entity_id();
        let key = element.key();
        let form = composer_form(&element);
        let close = close_action(&element);
        // Escape stops a streaming reply first, then closes.
        let escape = form
            .as_ref()
            .and_then(|(_, form_actions)| {
                form_actions
                    .iter()
                    .find(|action| action.id == "cancel")
                    .map(|action| action.id.clone())
            })
            .or_else(|| close.clone());

        let toolbar = actions_in_toolbar(&element);
        let action_buttons = || {
            element.actions.iter().enumerate().map(|(ix, action)| {
                parts::action_button(
                    &theme,
                    SharedString::from(format!("{key}-screen-action-{ix}")),
                    &key,
                    None,
                    action,
                    &actions,
                )
            })
        };
        let title = screen_title(&element);
        let named = !title.eq_ignore_ascii_case(&element.ns);
        let header = div()
            .flex()
            .items_start()
            .gap(px(10.0))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .h(px(20.0))
                            .flex()
                            .items_center()
                            .gap(px(6.0))
                            .child(
                                icon(screen_icon(&element.ns))
                                    .size(px(12.0))
                                    .text_color(theme.text_muted.opacity(0.6)),
                            )
                            .child(
                                div()
                                    .text_size(crate::typography::ui_rems(10.5))
                                    .font_weight(gpui::FontWeight::MEDIUM)
                                    .text_color(theme.text_muted.opacity(0.6))
                                    .child(SharedString::from(crate::popover::tracked_upper(
                                        screen_kind(&element.ns),
                                    ))),
                            ),
                    )
                    .when(named, |el| {
                        el.child(
                            div()
                                .mt(px(6.0))
                                .truncate()
                                .text_size(crate::typography::ui_rems(15.0))
                                .line_height(px(20.0))
                                .font_weight(gpui::FontWeight::MEDIUM)
                                .text_color(theme.text)
                                .child(SharedString::from(title)),
                        )
                    })
                    .when_some(element.text.clone(), |el, text| {
                        el.child(
                            div()
                                .mt(px(4.0))
                                .text_size(crate::typography::ui_rems(12.5))
                                .line_height(px(17.0))
                                .text_color(theme.text_muted)
                                .child(SharedString::from(text)),
                        )
                    }),
            )
            .when(!toolbar, |el| {
                el.child(
                    div()
                        .flex_none()
                        .flex()
                        .items_center()
                        .gap(px(4.0))
                        .children(action_buttons()),
                )
            })
            .when_some(close, |el, close| {
                el.child(
                    div()
                        .id(SharedString::from(format!("{key}-screen-close")))
                        .flex_none()
                        .size(px(24.0))
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded(px(8.0))
                        .hover(|style| style.bg(crate::theme::ink(0.06)))
                        .cursor_pointer()
                        .role(gpui::Role::Button)
                        .aria_label("Close")
                        .on_click(cx.listener(move |this, _, _, cx| this.send_action(&close, cx)))
                        .child(
                            icon(icons::CLOSE)
                                .size(px(14.0))
                                .text_color(theme.text_muted),
                        ),
                )
            });
        let toolbar = toolbar.then(|| {
            div()
                .mt(px(10.0))
                .flex()
                .flex_wrap()
                .items_center()
                .gap(px(6.0))
                .children(action_buttons())
        });

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
        let body = (!body.is_empty()).then(|| {
            div()
                .id(SharedString::from(format!("{key}-screen-body")))
                .mt(px(10.0))
                .mx(px(-8.0))
                .max_h(window.viewport_size().height * 0.45)
                .overflow_y_scroll()
                .track_scroll(&self.scroll)
                .flex()
                .flex_col()
                .gap(px(8.0))
                .children(body)
        });

        // The form is the panel's input row, as the question panel's free
        // text: over a hairline, with its actions beside it.
        let composer = form.map(|(_, form_actions)| {
            let input = self.input.as_ref().map(|(_, _, input)| input.clone());
            div()
                .mt(px(12.0))
                .border_t_1()
                .border_color(crate::theme::hairline(0.06))
                .pt(px(12.0))
                .flex()
                .items_center()
                .gap(px(8.0))
                .when_some(input, |el, input| {
                    el.child(div().flex_1().min_w_0().px(px(4.0)).child(input))
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
                        crate::popover::btn_primary(&theme, &label).px(px(16.0))
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
            .on_key_down(cx.listener(move |this, event: &gpui::KeyDownEvent, _, cx| {
                if event.keystroke.key == "escape"
                    && let Some(escape) = escape.as_deref()
                {
                    if this.pending.take().is_some() {
                        cx.notify();
                    } else {
                        this.send_action(escape, cx);
                    }
                    cx.stop_propagation();
                }
            }))
            .w_full()
            .flex()
            .flex_col()
            .p(px(16.0))
            .child(header)
            .children(toolbar)
            .children(parts::confirm_row(&theme, &key, &actions))
            .children(body)
            .children(composer)
            .into_any_element()
    }
}

impl Render for ScreenView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.docked {
            return self.render_docked(window, cx);
        }
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

        let toolbar = actions_in_toolbar(&element);
        let action_buttons = || {
            element.actions.iter().enumerate().map(|(ix, action)| {
                parts::action_button(
                    &theme,
                    SharedString::from(format!("{key}-screen-action-{ix}")),
                    &key,
                    None,
                    action,
                    &actions,
                )
            })
        };
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
            .when(!toolbar, |el| el.children(action_buttons()));
        let toolbar = toolbar.then(|| {
            div()
                .flex_none()
                .px(px(12.0))
                .py(px(6.0))
                .flex()
                .flex_wrap()
                .items_center()
                .gap(px(6.0))
                .border_b_1()
                .border_color(theme.hairline(0.06))
                .children(action_buttons())
        });

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
            .children(toolbar)
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
    /// Screens dock in the prompt panel now; the pane tab stays for callers
    /// that want one.
    #[allow(dead_code)]
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
        assert!(!actions_in_toolbar(&viewer));
    }

    #[test]
    fn many_actions_wrap_into_a_toolbar_and_known_screens_get_icons() {
        let actions: Vec<_> = ["today", "week", "lastweek", "30d", "all"]
            .iter()
            .map(|p| json!({"id": format!("period:{p}"), "label": p}))
            .chain([
                json!({"id":"refresh","label":"Refresh"}),
                json!({"id":"close","label":"Close"}),
            ])
            .collect();
        let usage: PiUiElement = serde_json::from_value(json!({"id":"dashboard","ns":"usage",
            "kind":"panel","placement":"screen","title":"Usage · Today","sections":[],
            "actions": actions}))
        .unwrap();
        assert!(actions_in_toolbar(&usage));
        assert_eq!(screen_icon("usage"), icons::FILE_DATA);
        assert_eq!(screen_icon("workspace"), icons::TUNING);
        assert_eq!(screen_icon("workflows"), icons::FILE_MARKDOWN);
        assert_eq!(screen_icon("someone-else"), icons::WIDGET);
    }
}
