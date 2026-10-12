//! Sheets: elements placed `sheet` (ask-user, a subagent's question, the
//! model picker, Pi's own dialogs raised between turns). Like the question
//! wizard, a sheet takes the composer's place until it is answered or
//! dismissed; its form fields render with the composer's own controls.

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::time::Duration;

use gpui::{
    AnyElement, App, Context, Entity, Focusable as _, KeyDownEvent, SharedString, Subscription,
    Task, Window, div, prelude::*, px,
};
use zeron_proto::extension_ui::{
    PiUiAction, PiUiElement, PiUiField, PiUiFieldKind, PiUiPlacement, PiUiSection, PiUiVariant,
};

use super::parts::{self, Actions};
use crate::composer::{
    AUTO_ADVANCE_MS, COMPOSER_RADIUS, Composer, ComposerInput, ComposerInputEvent,
};
use crate::icons::{self, icon};
use crate::motion;
use crate::theme::Theme;

/// Long option lists (searchable or not) get a filter; all scroll in the sheet.
const SEARCH_THRESHOLD: usize = 8;
const TEXTAREA_HEIGHT: f32 = 76.0;

/// The answer being composed for one sheet. A new element, or the same one
/// re-set with different fields (the next step of a wizard), starts over.
pub(crate) struct SheetForm {
    key: String,
    signature: u64,
    picked: HashMap<String, Vec<String>>,
    checked: HashMap<String, bool>,
    inputs: HashMap<String, Entity<ComposerInput>>,
    filters: HashMap<String, Entity<ComposerInput>>,
    _subscriptions: Vec<Subscription>,
    /// Sent; drawn inert until the extension replaces or removes the sheet.
    submitted: bool,
    focus_pending: bool,
    auto_submit: Option<Task<()>>,
}

/// The form a sheet answers: its first `form` section, else top-level fields.
pub(crate) fn sheet_form(element: &PiUiElement) -> (Vec<PiUiField>, Vec<PiUiAction>) {
    match element.form() {
        Some((fields, actions)) => (fields.to_vec(), actions.to_vec()),
        None => (element.fields.clone(), Vec::new()),
    }
}

fn signature(element: &PiUiElement) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    element.key().hash(&mut hasher);
    element.title.hash(&mut hasher);
    serde_json::to_string(&sheet_form(element).0)
        .unwrap_or_default()
        .hash(&mut hasher);
    hasher.finish()
}

/// A short, human header for the extension that asks.
fn header_label(ns: &str) -> &str {
    match ns {
        "ask-user" => "Question",
        "subagents" => "Subagent question",
        "model-select" => "Models",
        "pi" => "Pi",
        other => other,
    }
}

/// "Question 2/3" from a wizard's step progress, shown as the counter chip.
fn step_label(element: &PiUiElement) -> Option<String> {
    element.sections.iter().find_map(|section| match section {
        PiUiSection::Progress {
            id,
            label,
            done,
            total,
            ..
        } if id.as_deref() == Some("step") => label.clone().or_else(|| {
            Some(format!(
                "{}/{}",
                done.unwrap_or(0.0) + 1.0,
                total.unwrap_or(1.0)
            ))
        }),
        _ => None,
    })
}

fn option_matches(option: &zeron_proto::extension_ui::PiUiOption, filter: &str) -> bool {
    let filter = filter.trim().to_lowercase();
    filter.is_empty()
        || [
            Some(&option.label),
            Some(&option.value),
            option.description.as_ref(),
        ]
        .into_iter()
        .flatten()
        .any(|text| text.to_lowercase().contains(&filter))
}

impl SheetForm {
    fn new(element: &PiUiElement, fields: &[PiUiField], cx: &mut Context<Composer>) -> Self {
        let mut form = Self {
            key: element.key(),
            signature: signature(element),
            picked: HashMap::new(),
            checked: HashMap::new(),
            inputs: HashMap::new(),
            filters: HashMap::new(),
            _subscriptions: Vec::new(),
            submitted: false,
            focus_pending: true,
            auto_submit: None,
        };
        for field in fields {
            match field.kind {
                PiUiFieldKind::Select | PiUiFieldKind::Multiselect => {
                    form.picked
                        .insert(field.id.clone(), field.initial_selection());
                    if field.options.len() >= SEARCH_THRESHOLD {
                        let filter =
                            cx.new(|cx| ComposerInput::new("Filter", cx).with_single_line());
                        form._subscriptions
                            .push(cx.subscribe(&filter, |_, _, event, cx| {
                                if matches!(event, ComposerInputEvent::Edited) {
                                    cx.notify();
                                }
                            }));
                        form.filters.insert(field.id.clone(), filter);
                    }
                }
                PiUiFieldKind::Checkbox => {
                    form.checked
                        .insert(field.id.clone(), field.initial_checked());
                }
                PiUiFieldKind::Text | PiUiFieldKind::Textarea | PiUiFieldKind::Unknown => {
                    let multiline = field.kind == PiUiFieldKind::Textarea;
                    let placeholder = field
                        .placeholder
                        .clone()
                        .or_else(|| field.label.clone())
                        .unwrap_or_else(|| "Type here".into());
                    let text = field.initial_text();
                    let input = cx.new(|cx| {
                        let input = ComposerInput::new(placeholder, cx);
                        let mut input = if multiline {
                            input.with_viewport_height(TEXTAREA_HEIGHT)
                        } else {
                            input.with_single_line()
                        };
                        input.set_text(text, cx);
                        input
                    });
                    form._subscriptions
                        .push(cx.subscribe(&input, move |this, _, event, cx| {
                            match event {
                                // Enter in a one-line field submits; a textarea
                                // keeps Enter for new lines (⌘/Ctrl+Enter submits).
                                ComposerInputEvent::Submitted if !multiline => {
                                    this.submit_extension_sheet(None, cx)
                                }
                                ComposerInputEvent::ModifiedSubmitted => {
                                    this.submit_extension_sheet(None, cx)
                                }
                                ComposerInputEvent::Edited => cx.notify(),
                                _ => {}
                            }
                        }));
                    form.inputs.insert(field.id.clone(), input);
                }
            }
        }
        form
    }

    fn text(&self, field: &str, cx: &App) -> String {
        self.inputs
            .get(field)
            .map(|input| input.read(cx).text().to_owned())
            .unwrap_or_default()
    }

    /// `{fields: {...}}` as PIUI submits a form.
    pub(crate) fn value(&self, fields: &[PiUiField], cx: &App) -> serde_json::Value {
        let mut values = serde_json::Map::new();
        for field in fields {
            let value = match field.kind {
                PiUiFieldKind::Select => match self.picked.get(&field.id).and_then(|p| p.first()) {
                    Some(choice) => serde_json::json!(choice),
                    None => continue,
                },
                PiUiFieldKind::Multiselect => {
                    serde_json::json!(self.picked.get(&field.id).cloned().unwrap_or_default())
                }
                PiUiFieldKind::Checkbox => {
                    serde_json::json!(self.checked.get(&field.id).copied().unwrap_or(false))
                }
                PiUiFieldKind::Text | PiUiFieldKind::Unknown => {
                    serde_json::json!(self.text(&field.id, cx).trim())
                }
                PiUiFieldKind::Textarea => serde_json::json!(self.text(&field.id, cx)),
            };
            values.insert(field.id.clone(), value);
        }
        serde_json::json!({ "fields": values })
    }

    /// A required field is still empty.
    fn incomplete(&self, fields: &[PiUiField], cx: &App) -> bool {
        fields
            .iter()
            .filter(|f| f.required)
            .any(|field| match field.kind {
                PiUiFieldKind::Select | PiUiFieldKind::Multiselect => self
                    .picked
                    .get(&field.id)
                    .is_none_or(|picked| picked.is_empty()),
                PiUiFieldKind::Checkbox => false,
                _ => self.text(&field.id, cx).trim().is_empty(),
            })
    }

    fn pick(&mut self, field: &PiUiField, value: &str) {
        let picked = self.picked.entry(field.id.clone()).or_default();
        if field.kind == PiUiFieldKind::Multiselect {
            match picked.iter().position(|v| v == value) {
                Some(at) => {
                    picked.remove(at);
                }
                None => picked.push(value.to_owned()),
            }
        } else {
            *picked = vec![value.to_owned()];
        }
    }

    fn is_picked(&self, field: &str, value: &str) -> bool {
        self.picked
            .get(field)
            .is_some_and(|picked| picked.iter().any(|v| v == value))
    }

    fn visible_options<'a>(
        &self,
        field: &'a PiUiField,
        cx: &App,
    ) -> Vec<(usize, &'a zeron_proto::extension_ui::PiUiOption)> {
        let filter = self
            .filters
            .get(&field.id)
            .map(|f| f.read(cx).text().to_owned())
            .unwrap_or_default();
        field
            .options
            .iter()
            .enumerate()
            .filter(|(_, option)| option_matches(option, &filter))
            .collect()
    }
}

/// The form's main action: `submit` if offered, else the first primary one.
fn primary_action(actions: &[PiUiAction]) -> Option<&PiUiAction> {
    actions.iter().find(|a| a.id == "submit").or_else(|| {
        actions
            .iter()
            .find(|a| a.variant == Some(PiUiVariant::Primary))
    })
}

/// The action Escape takes: `cancel`, else `close`.
fn dismiss_action(element: &PiUiElement, form_actions: &[PiUiAction]) -> Option<String> {
    element
        .actions
        .iter()
        .chain(form_actions)
        .map(|a| a.id.as_str())
        .find(|id| *id == "cancel")
        .or_else(|| {
            element
                .actions
                .iter()
                .chain(form_actions)
                .map(|a| a.id.as_str())
                .find(|id| *id == "close")
        })
        .map(str::to_owned)
}

impl Composer {
    /// The selected chat's first sheet, and how many are waiting.
    pub(crate) fn extension_sheet(&self, cx: &App) -> Option<(PiUiElement, usize)> {
        let state = self.state.read(cx);
        let chat = state.selected_chat.as_deref()?;
        let live = state.extension_ui.of(chat)?;
        let mut sheets = live
            .typed_elements()
            .into_iter()
            .filter(|element| element.placement == PiUiPlacement::Sheet);
        let first = sheets.next()?;
        Some((first, 1 + sheets.count()))
    }

    pub(crate) fn extension_actions(&self, cx: &mut Context<Self>) -> Actions {
        let state = self.state.clone();
        let view = cx.weak_entity();
        let confirm_view = view.clone();
        Actions {
            send: std::rc::Rc::new(move |action, _, cx| {
                state.update(cx, |state, cx| state.extension_ui_action(action, cx));
                view.update(cx, |composer, cx| {
                    composer.extension.pending = None;
                    cx.notify();
                })
                .ok();
            }),
            confirm: std::rc::Rc::new(move |pending, _, cx| {
                confirm_view
                    .update(cx, |composer, cx| {
                        composer.extension.pending = pending;
                        cx.notify();
                    })
                    .ok();
            }),
            pending: self.extension.pending.clone(),
        }
    }

    /// Send the sheet's form with `action` (its primary action when `None`).
    pub(crate) fn submit_extension_sheet(
        &mut self,
        action: Option<String>,
        cx: &mut Context<Self>,
    ) {
        let Some((element, _)) = self.extension_sheet(cx) else {
            return;
        };
        let (fields, actions) = sheet_form(&element);
        let Some(action) = action.or_else(|| primary_action(&actions).map(|a| a.id.clone())) else {
            return;
        };
        let Some(form) = self.extension.sheet.as_mut() else {
            return;
        };
        if form.key != element.key() || form.submitted {
            return;
        }
        let submits = actions.iter().any(|a| a.id == action);
        if submits && form.incomplete(&fields, cx) {
            return;
        }
        let value = submits.then(|| form.value(&fields, cx));
        form.submitted = true;
        form.auto_submit = None;
        let request = parts::action_of(&element.key(), None, &action, value);
        self.state
            .update(cx, |state, cx| state.extension_ui_action(request, cx));
        cx.notify();
    }

    fn pick_extension_option(&mut self, field: &PiUiField, value: &str, cx: &mut Context<Self>) {
        let Some((element, _)) = self.extension_sheet(cx) else {
            return;
        };
        let (fields, _) = sheet_form(&element);
        let Some(form) = self.extension.sheet.as_mut() else {
            return;
        };
        if form.submitted {
            return;
        }
        form.pick(field, value);
        // A one-question sheet answers on the pick, like the question wizard.
        if field.kind == PiUiFieldKind::Select && fields.len() == 1 {
            form.auto_submit = Some(cx.spawn(async move |this, cx| {
                cx.background_executor()
                    .timer(Duration::from_millis(AUTO_ADVANCE_MS))
                    .await;
                this.update(cx, |composer, cx| composer.submit_extension_sheet(None, cx))
                    .ok();
            }));
        }
        cx.notify();
    }

    fn on_extension_sheet_key(
        &mut self,
        event: &KeyDownEvent,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        let Some((element, _)) = self.extension_sheet(cx) else {
            return;
        };
        let (fields, actions) = sheet_form(&element);
        let typing = self.extension.sheet.as_ref().is_some_and(|form| {
            form.inputs
                .values()
                .chain(form.filters.values())
                .any(|input| input.read(cx).focus_handle(cx).is_focused(window))
        });
        let key = event.keystroke.key.as_str();
        if key == "escape" {
            if self.extension.pending.take().is_some() {
                cx.notify();
            } else if let Some(dismiss) = dismiss_action(&element, &actions) {
                self.submit_extension_sheet(Some(dismiss), cx);
            }
            cx.stop_propagation();
        } else if key == "enter" && !typing {
            self.submit_extension_sheet(None, cx);
            cx.stop_propagation();
        } else if let Ok(digit) = key.parse::<usize>()
            && (1..=9).contains(&digit)
            && !typing
            && !event.keystroke.modifiers.modified()
            && let Some(field) = fields
                .iter()
                .find(|f| matches!(f.kind, PiUiFieldKind::Select | PiUiFieldKind::Multiselect))
        {
            let value = self.extension.sheet.as_ref().and_then(|form| {
                form.visible_options(field, cx)
                    .get(digit - 1)
                    .map(|(_, o)| o.value.clone())
            });
            if let Some(value) = value {
                self.pick_extension_option(field, &value, cx);
            }
            cx.stop_propagation();
        }
    }

    /// The sheet standing in for the composer, or `None` with none open.
    pub(crate) fn render_extension_sheet(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let Some((element, waiting)) = self.extension_sheet(cx) else {
            self.extension.sheet = None;
            return None;
        };
        let (fields, form_actions) = sheet_form(&element);
        if self
            .extension
            .sheet
            .as_ref()
            .is_none_or(|form| form.key != element.key() || form.signature != signature(&element))
        {
            self.extension.pending = None;
            self.extension.sheet = Some(SheetForm::new(&element, &fields, cx));
        }
        let focus = self.extension.focus.clone();
        if let Some(form) = self.extension.sheet.as_mut()
            && std::mem::take(&mut form.focus_pending)
        {
            // The first text field takes the keyboard; otherwise the sheet
            // does, for digits, Enter and Escape.
            let target = fields
                .iter()
                .find_map(|f| form.inputs.get(&f.id))
                .map(|input| input.read(cx).focus_handle(cx))
                .unwrap_or_else(|| focus.clone());
            window.focus(&target, cx);
        }
        let theme = Theme::of(cx).clone();
        let key = element.key();
        let actions = self.extension_actions(cx);
        let markdown = self.extension.markdown.clone();
        let view = cx.entity_id();
        let (submitted, incomplete) = {
            let form = self.extension.sheet.as_ref()?;
            (form.submitted, form.incomplete(&fields, cx))
        };

        let counter =
            step_label(&element).or_else(|| (waiting > 1).then(|| format!("1 of {waiting}")));
        let header = div()
            .flex()
            .items_center()
            .gap(px(10.0))
            .child(
                div()
                    .text_size(crate::typography::ui_rems(10.5))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_color(theme.text_muted.opacity(0.6))
                    .child(SharedString::from(crate::popover::tracked_upper(
                        header_label(&element.ns),
                    ))),
            )
            .when_some(counter, |el, counter| {
                el.child(
                    div()
                        .h(px(20.0))
                        .px(px(6.0))
                        .flex()
                        .items_center()
                        .rounded(px(6.0))
                        .bg(crate::theme::ink(0.06))
                        .text_size(crate::typography::ui_rems(10.0))
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_color(theme.text_muted.opacity(0.6))
                        .child(SharedString::from(counter)),
                )
            });

        let mut body = div()
            .px(px(16.0))
            .pt(px(16.0))
            .flex()
            .flex_col()
            .child(header)
            .when_some(element.title.clone(), |el, title| {
                el.child(
                    div()
                        .mt(px(6.0))
                        .text_size(crate::typography::ui_rems(15.0))
                        .line_height(px(20.0))
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_color(theme.text)
                        .child(SharedString::from(title)),
                )
            })
            .when_some(
                element
                    .text
                    .clone()
                    .filter(|t| Some(t) != element.title.as_ref()),
                |el, text| {
                    el.child(
                        div()
                            .mt(px(4.0))
                            .text_size(crate::typography::ui_rems(12.5))
                            .line_height(px(17.0))
                            .text_color(theme.text_muted)
                            .child(SharedString::from(text)),
                    )
                },
            );

        // Context first (markdown, meta, status), then the form.
        let context: Vec<AnyElement> = element
            .sections
            .iter()
            .enumerate()
            .filter(|(_, section)| {
                !matches!(section, PiUiSection::Form { .. })
                    && !matches!(section, PiUiSection::Progress { id, .. } if id.as_deref() == Some("step"))
            })
            .filter_map(|(ix, section)| {
                parts::section(
                    &theme,
                    &key,
                    ix,
                    section,
                    &actions,
                    window,
                    &markdown,
                    view,
                    cx,
                )
            })
            .collect();
        if !context.is_empty() {
            body = body.child(
                div()
                    .id(SharedString::from(format!("{key}-context")))
                    .mt(px(10.0))
                    .mx(px(-8.0))
                    .max_h(window.viewport_size().height * 0.25)
                    .overflow_y_scroll()
                    .flex()
                    .flex_col()
                    .gap(px(6.0))
                    .children(context),
            );
        }

        for (fix, field) in fields.iter().enumerate() {
            body = body
                .child(self.render_sheet_field(&key, fix, field, submitted, &theme, window, cx));
        }

        // Footer: dismissals on the left, the form's actions on the right.
        let element_actions: Vec<AnyElement> = element
            .actions
            .iter()
            .filter(|a| !form_actions.iter().any(|f| f.id == a.id))
            .enumerate()
            .map(|(ix, action)| {
                let id = action.id.clone();
                crate::popover::btn_ghost(&theme, &label(action), format!("{key}-el-{ix}"))
                    .id(SharedString::from(format!("{key}-el-action-{ix}")))
                    .when(action.disabled || submitted, |el| el.opacity(0.4))
                    .when(!action.disabled && !submitted, |el| {
                        el.on_click(cx.listener(move |this, _, _, cx| {
                            this.submit_extension_sheet(Some(id.clone()), cx)
                        }))
                    })
                    .into_any_element()
            })
            .collect();
        let main_actions: Vec<AnyElement> = form_actions
            .iter()
            .enumerate()
            .map(|(ix, action)| {
                let id = action.id.clone();
                let blocked = action.disabled
                    || submitted
                    || (Some(action) == primary_action(&form_actions) && incomplete);
                let button = match action.variant {
                    Some(PiUiVariant::Primary) => {
                        crate::popover::btn_primary(&theme, &label(action)).px(px(16.0))
                    }
                    Some(PiUiVariant::Danger) => crate::popover::btn_danger(&theme, &label(action)),
                    _ => crate::popover::btn_ghost(
                        &theme,
                        &label(action),
                        format!("{key}-form-{ix}"),
                    ),
                };
                let button = if action.variant.is_none() && action.id == "submit" {
                    crate::popover::btn_primary(&theme, &label(action)).px(px(16.0))
                } else {
                    button
                };
                button
                    .id(SharedString::from(format!("{key}-form-action-{ix}")))
                    .when(blocked, |el| el.opacity(0.4))
                    .when(!blocked, |el| {
                        el.on_click(cx.listener(move |this, _, _, cx| {
                            this.submit_extension_sheet(Some(id.clone()), cx)
                        }))
                    })
                    .into_any_element()
            })
            .collect();
        let footer = div()
            .flex()
            .flex_row()
            .justify_between()
            .items_center()
            .gap(px(8.0))
            .px(px(16.0))
            .pb(px(16.0))
            .pt(px(12.0))
            .child(div().flex().gap(px(4.0)).children(element_actions))
            .child(div().flex().gap(px(6.0)).children(main_actions));

        let panel = div()
            .id("extension-sheet")
            .track_focus(&focus)
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                this.on_extension_sheet_key(event, window, cx)
            }))
            .occlude()
            .rounded(px(COMPOSER_RADIUS))
            .border_1()
            .border_color(theme.border)
            .bg(theme.composer_surface_bg())
            .when(!theme.is_frost(), |el| el.shadow_lg())
            .flex()
            .flex_col()
            .when(submitted, |el| el.opacity(0.75))
            .child(body)
            .children(parts::confirm_row(&theme, &key, &actions))
            .child(footer);
        Some(
            crate::frost::frosted(COMPOSER_RADIUS, crate::frost::MENU_BLUR, panel)
                .into_any_element(),
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn render_sheet_field(
        &self,
        key: &str,
        fix: usize,
        field: &PiUiField,
        submitted: bool,
        theme: &Theme,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(form) = self.extension.sheet.as_ref() else {
            return gpui::Empty.into_any_element();
        };
        let label_el = field.label.clone().map(|label| {
            div()
                .mb(px(6.0))
                .text_size(crate::typography::ui_rems(11.5))
                .font_weight(gpui::FontWeight::MEDIUM)
                .text_color(theme.text_muted)
                .child(SharedString::from(label))
        });
        let wrap = div().mt(px(12.0)).flex().flex_col().children(label_el);
        match field.kind {
            PiUiFieldKind::Select | PiUiFieldKind::Multiselect => {
                let multi = field.kind == PiUiFieldKind::Multiselect;
                let options = form.visible_options(field, cx);
                let numbered = fix == 0;
                let rows = options
                    .into_iter()
                    .enumerate()
                    .map(|(shown, (ix, option))| {
                        let picked = form.is_picked(&field.id, &option.value);
                        let hover_key = format!("{key}-f{fix}-o{ix}");
                        let field = field.clone();
                        let value = option.value.clone();
                        div()
                            .id(SharedString::from(hover_key.clone()))
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(12.0))
                            .px(px(14.0))
                            .py(px(9.0))
                            .rounded(px(12.0))
                            .border_1()
                            .border_color(if picked {
                                crate::theme::ink(0.16)
                            } else {
                                gpui::transparent_black()
                            })
                            .bg(if picked {
                                crate::theme::ink(0.09)
                            } else {
                                motion::hover_blend(
                                    &hover_key,
                                    crate::theme::ink(0.025),
                                    crate::theme::ink(0.06),
                                )
                            })
                            .on_hover(motion::hover_listener(hover_key.clone()))
                            .role(if multi {
                                gpui::Role::CheckBox
                            } else {
                                gpui::Role::RadioButton
                            })
                            .aria_label(SharedString::from(option.label.clone()))
                            .when(!submitted, |el| {
                                el.cursor_pointer()
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.pick_extension_option(&field, &value, cx)
                                    }))
                            })
                            .when(multi, |el| {
                                el.child(
                                    div()
                                        .flex_none()
                                        .size(px(16.0))
                                        .rounded(px(4.0))
                                        .border_1()
                                        .border_color(if picked {
                                            theme.text
                                        } else {
                                            theme.text_faint
                                        })
                                        .bg(if picked {
                                            theme.text
                                        } else {
                                            gpui::transparent_black()
                                        })
                                        .flex()
                                        .items_center()
                                        .justify_center()
                                        .when(picked, |el| {
                                            el.child(
                                                icon(icons::CHECK)
                                                    .size(px(11.0))
                                                    .text_color(theme.on_solid),
                                            )
                                        }),
                                )
                            })
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .flex()
                                    .flex_col()
                                    .gap(px(2.0))
                                    .child(
                                        div()
                                            .text_size(crate::typography::ui_rems(13.5))
                                            .font_weight(gpui::FontWeight::MEDIUM)
                                            .text_color(if picked {
                                                theme.text
                                            } else {
                                                theme.text.opacity(0.9)
                                            })
                                            .child(SharedString::from(
                                                if option.label.is_empty() {
                                                    option.value.clone()
                                                } else {
                                                    option.label.clone()
                                                },
                                            )),
                                    )
                                    .when_some(option.description.clone(), |el, description| {
                                        el.child(
                                            div()
                                                .text_size(crate::typography::ui_rems(12.0))
                                                .text_color(theme.text_muted.opacity(0.75))
                                                .child(SharedString::from(description)),
                                        )
                                    }),
                            )
                            .when(numbered && shown < 9, |el| {
                                el.child(
                                    div()
                                        .flex_none()
                                        .size(px(22.0))
                                        .flex()
                                        .items_center()
                                        .justify_center()
                                        .rounded(px(6.0))
                                        .bg(if picked {
                                            crate::theme::ink(0.16)
                                        } else {
                                            crate::theme::ink(0.05)
                                        })
                                        .text_size(crate::typography::ui_rems(11.0))
                                        .text_color(if picked {
                                            theme.text
                                        } else {
                                            theme.text_muted.opacity(0.6)
                                        })
                                        .child(SharedString::from(format!("{}", shown + 1))),
                                )
                            })
                    });
                let filter = form.filters.get(&field.id).cloned();
                wrap.when_some(filter, |el, filter| {
                    el.child(
                        div()
                            .mb(px(6.0))
                            .child(crate::popover::dialog_field(filter.into_any_element())),
                    )
                })
                .child(
                    div()
                        .id(SharedString::from(format!("{key}-f{fix}-options")))
                        .max_h(window.viewport_size().height * 0.4)
                        .overflow_y_scroll()
                        .flex()
                        .flex_col()
                        .gap(px(4.0))
                        .children(rows),
                )
                .into_any_element()
            }
            PiUiFieldKind::Checkbox => {
                let checked = form.checked.get(&field.id).copied().unwrap_or(false);
                let id = field.id.clone();
                let label = field
                    .label
                    .clone()
                    .or_else(|| field.placeholder.clone())
                    .unwrap_or_else(|| field.id.clone());
                div()
                    .mt(px(12.0))
                    .id(SharedString::from(format!("{key}-f{fix}-check")))
                    .role(gpui::Role::CheckBox)
                    .aria_label(SharedString::from(label.clone()))
                    .flex()
                    .items_center()
                    .gap(px(10.0))
                    .when(!submitted, |el| {
                        el.cursor_pointer()
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if let Some(form) = this.extension.sheet.as_mut() {
                                    let slot = form.checked.entry(id.clone()).or_default();
                                    *slot = !*slot;
                                    cx.notify();
                                }
                            }))
                    })
                    .child(
                        div()
                            .flex_none()
                            .size(px(16.0))
                            .rounded(px(4.0))
                            .border_1()
                            .border_color(if checked {
                                theme.text
                            } else {
                                theme.text_faint
                            })
                            .bg(if checked {
                                theme.text
                            } else {
                                gpui::transparent_black()
                            })
                            .flex()
                            .items_center()
                            .justify_center()
                            .when(checked, |el| {
                                el.child(
                                    icon(icons::CHECK).size(px(11.0)).text_color(theme.on_solid),
                                )
                            }),
                    )
                    .child(
                        div()
                            .text_size(crate::typography::ui_rems(13.0))
                            .text_color(theme.text)
                            .child(SharedString::from(label)),
                    )
                    .into_any_element()
            }
            _ => {
                let Some(input) = form.inputs.get(&field.id).cloned() else {
                    return gpui::Empty.into_any_element();
                };
                wrap.child(crate::popover::dialog_field(input.into_any_element()))
                    .into_any_element()
            }
        }
    }
}

fn label(action: &PiUiAction) -> String {
    if action.label.is_empty() {
        let mut chars = action.id.chars();
        chars
            .next()
            .map(|c| c.to_uppercase().collect::<String>() + chars.as_str())
            .unwrap_or_default()
    } else {
        action.label.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn element(value: serde_json::Value) -> PiUiElement {
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn a_sheet_reads_its_form_section_steps_and_dismissal() {
        let ask = element(
            json!({"id":"question","ns":"ask-user","kind":"panel","placement":"sheet",
            "title":"Question 2/3: Pick","sections":[
                {"kind":"progress","id":"step","label":"Question 2/3","done":1,"total":3},
                {"kind":"form","id":"ask-form","fields":[{"id":"selection","kind":"select",
                    "options":[{"value":"A","label":"A"}]}],
                 "actions":[{"id":"submit","label":"Submit","variant":"primary"},
                            {"id":"skip","label":"Skip question"}]}],
            "actions":[{"id":"cancel","label":"Cancel"}]}),
        );
        let (fields, actions) = sheet_form(&ask);
        assert_eq!(fields[0].id, "selection");
        assert_eq!(primary_action(&actions).unwrap().id, "submit");
        assert_eq!(step_label(&ask).as_deref(), Some("Question 2/3"));
        assert_eq!(dismiss_action(&ask, &actions).as_deref(), Some("cancel"));
        // Pi's own dialogs carry their fields the same way.
        let confirm = element(
            json!({"id":"dialog-1","ns":"pi","kind":"panel","placement":"sheet",
            "sections":[{"kind":"form","fields":[],"actions":[
                {"id":"yes","label":"Yes","variant":"primary"},{"id":"no","label":"No"}]}],
            "actions":[{"id":"cancel","label":"Cancel"}]}),
        );
        assert_eq!(primary_action(&sheet_form(&confirm).1).unwrap().id, "yes");
        // A re-set with new fields is a new form; the same fields are not.
        let mut next = ask.clone();
        assert_eq!(signature(&ask), signature(&next));
        next.sections[1] = serde_json::from_value(json!({"kind":"form","fields":[
            {"id":"answer","kind":"textarea"}]}))
        .unwrap();
        assert_ne!(signature(&ask), signature(&next));
    }

    #[test]
    fn options_filter_on_label_value_and_description() {
        let option = zeron_proto::extension_ui::PiUiOption {
            value: "anthropic/claude".into(),
            label: "Claude".into(),
            description: Some("fast reasoning".into()),
        };
        assert!(option_matches(&option, ""));
        assert!(option_matches(&option, "ANTHROPIC"));
        assert!(option_matches(&option, "reason"));
        assert!(!option_matches(&option, "gpt"));
        assert_eq!(header_label("subagents"), "Subagent question");
        assert_eq!(
            label(&PiUiAction {
                id: "dismiss".into(),
                ..Default::default()
            }),
            "Dismiss"
        );
    }
}
