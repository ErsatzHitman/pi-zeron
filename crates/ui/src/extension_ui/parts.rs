//! Native renderings of PIUI sections and actions, shared by sheets, trays
//! and screens. Everything here draws from the theme's roles and the existing
//! controls; nothing knows which surface it sits on.

use std::cell::RefCell;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::rc::Rc;

use gpui::{AnyElement, App, Hsla, SharedString, Window, div, prelude::*, px};
use zeron_proto::extension_ui::{
    ExtensionUiAction, PiUiAction, PiUiChecklistItem, PiUiItemState, PiUiRosterRow, PiUiRowState,
    PiUiSection, PiUiTone, PiUiTurn, PiUiTurnRole, PiUiVariant,
};

use crate::icons::{self, icon};
use crate::theme::Theme;

pub(crate) const TEXT_SIZE: f32 = 12.5;
pub(crate) const TEXT_LINE: f32 = 17.0;
const META_SIZE: f32 = 11.5;
const GLYPH_SLOT: f32 = 14.0;
/// Logs and diffs show their tail; the host keeps the whole thing.
const LOG_TAIL: usize = 12;
const MARKDOWN_CACHE: usize = 48;

/// An action waiting on the user's confirmation (`PiUiAction::confirm`).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct PendingConfirm {
    pub action: ExtensionUiAction,
    pub prompt: String,
    pub danger: bool,
}

type SendFn = Rc<dyn Fn(ExtensionUiAction, &mut Window, &mut App)>;
type ConfirmFn = Rc<dyn Fn(Option<PendingConfirm>, &mut Window, &mut App)>;

/// How a rendered element talks back: send an action, or hold one for
/// confirmation first.
#[derive(Clone)]
pub(crate) struct Actions {
    pub send: SendFn,
    pub confirm: ConfirmFn,
    pub pending: Option<PendingConfirm>,
}

impl Actions {
    fn is_pending(&self, action: &ExtensionUiAction) -> bool {
        self.pending.as_ref().is_some_and(|p| p.action == *action)
    }
}

/// Parsed markdown by content, so a patched panel re-parses only what changed.
#[derive(Default, Clone)]
pub(crate) struct MarkdownCache(Rc<RefCell<HashMap<u64, crate::markdown::BlockTree>>>);

fn hash_text(text: &str) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut hasher);
    hasher.finish()
}

pub(crate) fn tone_color(theme: &Theme, tone: Option<PiUiTone>) -> Hsla {
    match tone {
        Some(PiUiTone::Success) => theme.success,
        Some(PiUiTone::Warning) => theme.warning,
        Some(PiUiTone::Error) => theme.danger,
        Some(PiUiTone::Busy) | Some(PiUiTone::Info) => theme.accent,
        Some(PiUiTone::Muted) | Some(PiUiTone::Unknown) | None => theme.text_muted,
    }
}

pub(crate) fn action_of(
    element: &str,
    row: Option<&str>,
    action_id: &str,
    value: Option<serde_json::Value>,
) -> ExtensionUiAction {
    ExtensionUiAction {
        element_id: element.to_owned(),
        action_id: action_id.to_owned(),
        value,
        row_id: row.map(str::to_owned),
    }
}

/// A compact text button for an element or row action. An action with a
/// `confirm` prompt asks first, inline, in [`confirm_row`].
pub(crate) fn action_button(
    theme: &Theme,
    id: SharedString,
    element: &str,
    row: Option<&str>,
    action: &PiUiAction,
    actions: &Actions,
) -> AnyElement {
    let danger = action.variant == Some(PiUiVariant::Danger);
    let primary = action.variant == Some(PiUiVariant::Primary);
    let request = action_of(
        element,
        row,
        &action.id,
        row.map(|r| serde_json::json!({ "rowId": r })),
    );
    let pending = actions.is_pending(&request);
    let label = if action.label.is_empty() {
        action.id.clone()
    } else {
        action.label.clone()
    };
    let color = if danger {
        theme.danger
    } else if primary {
        theme.text
    } else {
        theme.text_muted
    };
    let accent = theme.accent;
    let button = div()
        .id(id)
        .role(gpui::Role::Button)
        .aria_label(SharedString::from(label.clone()))
        .flex_none()
        .h(px(22.0))
        .px(px(7.0))
        .flex()
        .items_center()
        .rounded(px(5.0))
        .text_size(px(11.5))
        .font_weight(gpui::FontWeight::MEDIUM)
        .text_color(color)
        .when(pending, |el| el.bg(theme.element_hover))
        .child(SharedString::from(label));
    if action.disabled {
        return button.opacity(0.4).into_any_element();
    }
    let send = actions.send.clone();
    let confirm = actions.confirm.clone();
    let prompt = action.confirm.clone();
    button
        .cursor_pointer()
        .hover(|s| s.bg(theme.element_hover))
        .tab_index(0)
        .focus_visible(move |s| s.bg(accent.opacity(0.18)))
        .on_click(move |_, window, cx| match &prompt {
            Some(prompt) => confirm(
                Some(PendingConfirm {
                    action: request.clone(),
                    prompt: prompt.clone(),
                    danger,
                }),
                window,
                cx,
            ),
            None => send(request.clone(), window, cx),
        })
        .into_any_element()
}

/// The inline "Kill this job?  Cancel  Kill" row for a pending confirmation.
pub(crate) fn confirm_row(theme: &Theme, id: &str, actions: &Actions) -> Option<AnyElement> {
    let pending = actions.pending.clone()?;
    let cancel = actions.confirm.clone();
    let send = actions.send.clone();
    let confirm_clear = actions.confirm.clone();
    let accent = theme.accent;
    let go = pending.action.clone();
    Some(
        div()
            .flex()
            .items_center()
            .gap(px(6.0))
            .px(px(8.0))
            .py(px(4.0))
            .child(
                icon(icons::DANGER_TRIANGLE)
                    .size(px(13.0))
                    .text_color(if pending.danger {
                        theme.danger
                    } else {
                        theme.warning
                    }),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_size(px(TEXT_SIZE))
                    .text_color(theme.text)
                    .child(SharedString::from(pending.prompt.clone())),
            )
            .child(
                div()
                    .id(SharedString::from(format!("{id}-confirm-cancel")))
                    .role(gpui::Role::Button)
                    .h(px(22.0))
                    .px(px(7.0))
                    .flex()
                    .items_center()
                    .rounded(px(5.0))
                    .text_size(px(11.5))
                    .text_color(theme.text_muted)
                    .cursor_pointer()
                    .hover(|s| s.bg(theme.element_hover))
                    .tab_index(0)
                    .focus_visible(move |s| s.bg(accent.opacity(0.18)))
                    .on_click(move |_, window, cx| cancel(None, window, cx))
                    .child("Cancel"),
            )
            .child(
                div()
                    .id(SharedString::from(format!("{id}-confirm-go")))
                    .role(gpui::Role::Button)
                    .h(px(22.0))
                    .px(px(8.0))
                    .flex()
                    .items_center()
                    .rounded(px(5.0))
                    .text_size(px(11.5))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_color(if pending.danger {
                        gpui::white()
                    } else {
                        theme.on_solid
                    })
                    .bg(if pending.danger {
                        theme.danger_strong
                    } else {
                        theme.text
                    })
                    .cursor_pointer()
                    .hover(|s| s.opacity(0.9))
                    .tab_index(0)
                    .focus_visible(move |s| s.border_1().border_color(accent))
                    .on_click(move |_, window, cx| {
                        confirm_clear(None, window, cx);
                        send(go.clone(), window, cx);
                    })
                    .child("Confirm"),
            )
            .into_any_element(),
    )
}

fn row_glyph(
    theme: &Theme,
    key: String,
    state: Option<PiUiRowState>,
    view: gpui::EntityId,
    cx: &mut App,
) -> AnyElement {
    let slot = div()
        .flex_none()
        .w(px(GLYPH_SLOT))
        .h(px(TEXT_LINE))
        .flex()
        .items_center()
        .justify_center();
    match state {
        Some(PiUiRowState::Done) => slot
            .child(icon(icons::CHECK).size(px(12.0)).text_color(theme.success))
            .into_any_element(),
        Some(PiUiRowState::Error) => slot
            .child(
                icon(icons::CLOSE_CIRCLE)
                    .size(px(12.0))
                    .text_color(theme.danger),
            )
            .into_any_element(),
        Some(PiUiRowState::Running) => slot
            .child(crate::loaders::mini_glyph_spinner(
                key,
                2.0,
                theme.glyph,
                view,
                cx,
            ))
            .into_any_element(),
        Some(PiUiRowState::Waiting) => slot
            .child(
                icon(icons::CLOCK_CIRCLE)
                    .size(px(12.0))
                    .text_color(theme.text_faint),
            )
            .into_any_element(),
        _ => slot
            .child(div().size(px(5.0)).rounded_full().bg(theme.text_faint))
            .into_any_element(),
    }
}

/// Roster rows: state glyph, label, detail, row actions; nested rows indent.
pub(crate) fn roster(
    theme: &Theme,
    element: &str,
    rows: &[PiUiRosterRow],
    actions: &Actions,
    view: gpui::EntityId,
    cx: &mut App,
) -> AnyElement {
    let mut list = Vec::with_capacity(rows.len());
    for (ix, row) in rows.iter().enumerate() {
        let glyph = row_glyph(
            theme,
            format!("{element}-row-{ix}-state"),
            row.state,
            view,
            cx,
        );
        list.push((ix, row, glyph));
    }
    div()
        .flex()
        .flex_col()
        .children(list.into_iter().map(|(ix, row, glyph)| {
            let depth = row.depth.unwrap_or(0).min(4) as f32;
            div()
                .id(SharedString::from(format!("{element}-row-{ix}")))
                .min_h(px(26.0))
                .pl(px(8.0 + depth * 14.0))
                .pr(px(4.0))
                .flex()
                .items_center()
                .gap(px(6.0))
                .rounded(px(crate::queue::ROW_RADIUS))
                .hover(|s| s.bg(theme.element_hover))
                .child(glyph)
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .items_baseline()
                        .gap(px(6.0))
                        .text_size(px(TEXT_SIZE))
                        .child(
                            div()
                                .flex_none()
                                .max_w(gpui::relative(0.6))
                                .truncate()
                                .text_color(theme.text)
                                .child(SharedString::from(row.label.clone())),
                        )
                        .when_some(row.detail.clone(), |el, detail| {
                            el.child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .truncate()
                                    .text_size(px(META_SIZE))
                                    .text_color(theme.text_faint)
                                    .child(SharedString::from(detail)),
                            )
                        }),
                )
                .children(row.actions.iter().enumerate().map(|(aix, action)| {
                    action_button(
                        theme,
                        SharedString::from(format!("{element}-row-{ix}-action-{aix}")),
                        element,
                        Some(&row.id),
                        action,
                        actions,
                    )
                }))
        }))
        .into_any_element()
}

pub(crate) fn checklist(theme: &Theme, items: &[PiUiChecklistItem]) -> AnyElement {
    div()
        .flex()
        .flex_col()
        .children(items.iter().map(|item| {
            let (glyph, color, text_color): (AnyElement, Hsla, Hsla) = match item.state {
                PiUiItemState::Done => (
                    icon(icons::CHECK).size(px(12.0)).into_any_element(),
                    theme.success,
                    theme.text_faint,
                ),
                PiUiItemState::Active => (
                    div()
                        .size(px(7.0))
                        .rounded_full()
                        .bg(theme.accent)
                        .into_any_element(),
                    theme.accent,
                    theme.text,
                ),
                PiUiItemState::Skipped => (
                    icon(icons::CLOSE).size(px(11.0)).into_any_element(),
                    theme.text_faint,
                    theme.text_faint,
                ),
                _ => (
                    div()
                        .size(px(9.0))
                        .rounded_full()
                        .border_1()
                        .border_color(theme.text_faint)
                        .into_any_element(),
                    theme.text_faint,
                    theme.text_muted,
                ),
            };
            div()
                .min_h(px(24.0))
                .px(px(8.0))
                .flex()
                .items_start()
                .gap(px(6.0))
                .py(px(3.0))
                .child(
                    div()
                        .flex_none()
                        .w(px(GLYPH_SLOT))
                        .h(px(TEXT_LINE))
                        .flex()
                        .items_center()
                        .justify_center()
                        .text_color(color)
                        .child(glyph),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .text_size(px(TEXT_SIZE))
                        .line_height(px(TEXT_LINE))
                        .text_color(text_color)
                        .when(item.state == PiUiItemState::Done, |el| el.line_through())
                        .child(SharedString::from(item.text.clone())),
                )
        }))
        .into_any_element()
}

pub(crate) fn progress(
    theme: &Theme,
    label: Option<&str>,
    done: Option<f64>,
    total: Option<f64>,
) -> AnyElement {
    let fraction = match (done, total) {
        (Some(done), Some(total)) if total > 0.0 => (done / total).clamp(0.0, 1.0) as f32,
        _ => 0.0,
    };
    let count = match (done, total) {
        (Some(done), Some(total)) => Some(format!("{}/{}", done.round(), total.round())),
        _ => None,
    };
    div()
        .px(px(8.0))
        .py(px(4.0))
        .flex()
        .flex_col()
        .gap(px(4.0))
        .when(label.is_some() || count.is_some(), |el| {
            el.child(
                div()
                    .flex()
                    .justify_between()
                    .text_size(px(META_SIZE))
                    .text_color(theme.text_muted)
                    .child(SharedString::from(label.unwrap_or_default().to_owned()))
                    .children(count.map(SharedString::from)),
            )
        })
        .child(
            div()
                .h(px(4.0))
                .w_full()
                .rounded_full()
                .bg(theme.ink(0.08))
                .child(
                    div()
                        .h_full()
                        .w(gpui::relative(fraction))
                        .rounded_full()
                        .bg(theme.accent),
                ),
        )
        .into_any_element()
}

pub(crate) fn status_line(theme: &Theme, text: &str, tone: Option<PiUiTone>) -> AnyElement {
    div()
        .px(px(8.0))
        .py(px(3.0))
        .flex()
        .items_center()
        .gap(px(6.0))
        .child(
            div()
                .flex_none()
                .w(px(GLYPH_SLOT))
                .flex()
                .justify_center()
                .child(
                    div()
                        .size(px(6.0))
                        .rounded_full()
                        .bg(tone_color(theme, tone)),
                ),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .text_size(px(TEXT_SIZE))
                .line_height(px(TEXT_LINE))
                .text_color(theme.text_muted)
                .child(SharedString::from(text.to_owned())),
        )
        .into_any_element()
}

pub(crate) fn meta(theme: &Theme, text: &str) -> AnyElement {
    div()
        .px(px(8.0))
        .text_size(px(META_SIZE))
        .line_height(px(15.0))
        .text_color(theme.text_faint)
        .child(SharedString::from(text.to_owned()))
        .into_any_element()
}

/// Monospace lines, newest last; long logs keep their tail.
pub(crate) fn log(theme: &Theme, lines: &[String], tail: Option<usize>) -> AnyElement {
    let skip = tail.map_or(0, |tail| lines.len().saturating_sub(tail));
    div()
        .mx(px(8.0))
        .px(px(8.0))
        .py(px(6.0))
        .rounded(px(6.0))
        .bg(theme.ink(0.035))
        .font_family(theme.font_mono.clone())
        .text_size(px(11.5))
        .line_height(px(16.0))
        .text_color(theme.text_muted)
        .flex()
        .flex_col()
        .children(
            lines
                .iter()
                .skip(skip)
                .map(|line| div().truncate().child(SharedString::from(line.clone()))),
        )
        .into_any_element()
}

pub(crate) fn diff(theme: &Theme, text: &str) -> AnyElement {
    let lines: Vec<&str> = text.lines().collect();
    div()
        .mx(px(8.0))
        .px(px(8.0))
        .py(px(6.0))
        .rounded(px(6.0))
        .bg(theme.ink(0.035))
        .font_family(theme.font_mono.clone())
        .text_size(px(11.5))
        .line_height(px(16.0))
        .flex()
        .flex_col()
        .children(lines.into_iter().map(|line| {
            let color = if line.starts_with('+') && !line.starts_with("+++") {
                theme.success
            } else if line.starts_with('-') && !line.starts_with("---") {
                theme.danger
            } else if line.starts_with("@@") {
                theme.accent
            } else {
                theme.text_muted
            };
            div()
                .truncate()
                .text_color(color)
                .child(SharedString::from(line.to_owned()))
        }))
        .into_any_element()
}

pub(crate) fn table(theme: &Theme, columns: &[String], rows: &[Vec<String>]) -> AnyElement {
    let cell = |text: &str, head: bool| {
        div()
            .flex_1()
            .min_w_0()
            .truncate()
            .text_size(px(if head { META_SIZE } else { TEXT_SIZE }))
            .text_color(if head {
                theme.text_faint
            } else {
                theme.text_muted
            })
            .when(head, |el| el.font_weight(gpui::FontWeight::MEDIUM))
            .child(SharedString::from(text.to_owned()))
    };
    div()
        .px(px(8.0))
        .flex()
        .flex_col()
        .gap(px(2.0))
        .when(!columns.is_empty(), |el| {
            el.child(
                div()
                    .flex()
                    .gap(px(8.0))
                    .children(columns.iter().map(|c| cell(c, true))),
            )
        })
        .children(rows.iter().map(|row| {
            div()
                .flex()
                .gap(px(8.0))
                .children(row.iter().map(|c| cell(c, false)))
        }))
        .into_any_element()
}

pub(crate) fn markdown(
    theme: &Theme,
    key: &str,
    text: &str,
    window: &Window,
    cache: &MarkdownCache,
) -> AnyElement {
    markdown_sized(theme, key, text, window, cache, px(TEXT_SIZE + 0.5).into())
}

/// Markdown at `size` (a reply in a conversation reads at the chat's size).
fn markdown_sized(
    theme: &Theme,
    key: &str,
    text: &str,
    window: &Window,
    cache: &MarkdownCache,
    size: gpui::AbsoluteLength,
) -> AnyElement {
    let hash = hash_text(text);
    let mut trees = cache.0.borrow_mut();
    if trees.len() > MARKDOWN_CACHE && !trees.contains_key(&hash) {
        trees.clear();
    }
    let tree = trees
        .entry(hash)
        .or_insert_with(|| crate::markdown::parse_full(text));
    let options =
        crate::markdown::render::RenderOptions::settled(SharedString::from(format!("{key}-md")));
    div()
        .px(px(8.0))
        .text_size(size)
        .text_color(theme.text)
        .child(crate::markdown::render::render_tree(
            tree,
            &options,
            theme,
            window,
            &|_| None,
        ))
        .into_any_element()
}

/// A conversation (btw, a subagent's transcript), drawn like the chat's
/// own: the user's turns in the transcript's bubble, the replies as body text.
pub(crate) fn turns(
    theme: &Theme,
    key: &str,
    turns: &[PiUiTurn],
    window: &Window,
    cache: &MarkdownCache,
) -> AnyElement {
    div()
        .flex()
        .flex_col()
        .gap(px(12.0))
        .children(turns.iter().enumerate().map(|(ix, turn)| {
            let key = format!("{key}-turn-{ix}");
            match turn.role {
                PiUiTurnRole::User => div()
                    .w_full()
                    .px(px(8.0))
                    .flex()
                    .justify_end()
                    .child(
                        div()
                            .min_w_0()
                            .max_w(gpui::relative(0.8))
                            .bg(crate::theme::user_bubble_bg())
                            .rounded(px(Theme::BUBBLE_RADIUS))
                            .px(px(16.0))
                            .py(px(10.0))
                            .text_size(crate::typography::ui_rems(14.0))
                            .line_height(crate::typography::ui_rems(
                                crate::transcript::USER_LINE_HEIGHT,
                            ))
                            .text_color(theme.text)
                            .child(SharedString::from(turn.text.clone())),
                    )
                    .into_any_element(),
                PiUiTurnRole::Assistant => markdown_sized(
                    theme,
                    &key,
                    &turn.text,
                    window,
                    cache,
                    crate::typography::ui_rems(14.0).into(),
                ),
                PiUiTurnRole::Error => status_line(theme, &turn.text, Some(PiUiTone::Error)),
                PiUiTurnRole::Tool => div()
                    .px(px(8.0))
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .child(
                        icon(icons::TERMINAL)
                            .size(px(11.0))
                            .text_color(theme.text_faint),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .font_family(theme.font_mono.clone())
                            .text_size(px(11.5))
                            .text_color(theme.text_muted)
                            .child(SharedString::from(turn.text.clone())),
                    )
                    .into_any_element(),
                _ => meta(theme, &turn.text),
            }
        }))
        .into_any_element()
}

/// One section, or `None` for forms (each surface draws its own) and kinds a
/// newer extension invented (skipped, never fatal).
#[allow(clippy::too_many_arguments)]
pub(crate) fn section(
    theme: &Theme,
    element: &str,
    ix: usize,
    section: &PiUiSection,
    actions: &Actions,
    window: &Window,
    cache: &MarkdownCache,
    view: gpui::EntityId,
    cx: &mut App,
) -> Option<AnyElement> {
    let key = format!("{element}-s{ix}");
    Some(match section {
        PiUiSection::Status { text, tone, .. } if !text.is_empty() => {
            status_line(theme, text, *tone)
        }
        PiUiSection::Markdown { text, .. } if !text.is_empty() => {
            markdown(theme, &key, text, window, cache)
        }
        PiUiSection::Meta { text, .. } if !text.is_empty() => meta(theme, text),
        PiUiSection::Log { text, lines, .. } => {
            let lines: Vec<String> = if lines.is_empty() {
                text.iter()
                    .flat_map(|t| t.lines().map(str::to_owned))
                    .collect()
            } else {
                lines.clone()
            };
            if lines.is_empty() {
                return None;
            }
            log(theme, &lines, Some(LOG_TAIL))
        }
        PiUiSection::Turns { turns: list, .. } => turns(theme, &key, list, window, cache),
        PiUiSection::Roster { rows, .. } => roster(theme, element, rows, actions, view, cx),
        PiUiSection::Progress {
            label, done, total, ..
        } => progress(theme, label.as_deref(), *done, *total),
        PiUiSection::Checklist { items, .. } => checklist(theme, items),
        PiUiSection::Table { columns, rows, .. } => table(theme, columns, rows),
        PiUiSection::Diff { unified_diff, .. } if !unified_diff.is_empty() => {
            diff(theme, unified_diff)
        }
        _ => return None,
    })
}
