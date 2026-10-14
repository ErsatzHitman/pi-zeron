//! Pinned trays and status chips. Elements placed `pinned` (todo, subagent
//! fleet, background bash, workflows, advisor, decision cards) and plain
//! `setWidget` blocks dock above the composer in the todo/queue tray stack;
//! `status` elements, `setStatus` texts and the footer channels become chips
//! in the session footer.

use std::time::Duration;

use gpui::{AnyElement, App, Context, SharedString, Window, div, prelude::*, px};
use zeron_proto::extension_ui::{
    ExtensionUiState, PiUiElement, PiUiKind, PiUiPlacement, PiUiRowState, PiUiSection,
};

use super::parts::{self, Actions};
use crate::composer::{Composer, QUEUE_COMPOSER_OVERLAP};
use crate::icons::{self, icon};
use crate::motion::{self, AnimationExt as _};
use crate::theme::Theme;

const HEADER_HEIGHT: f32 = 32.0;
const ROW_PAD_X: f32 = 8.0;
const SIDE_INSET: f32 = 16.0;
const CHIP_MAX_WIDTH: f32 = 200.0;

/// Per-element presentation, in memory for the app run (like the todo tray).
#[derive(Debug, Default, Clone)]
pub(crate) struct TrayState {
    /// The user's choice; `None` follows the element (open while it works).
    expanded: Option<bool>,
    epoch: u32,
}

/// Trays in publication order: elements, then plain widgets. `screens`:
/// screens show as trays that open their tab (the main conversation only).
fn tray_elements(state: &ExtensionUiState, screens: bool) -> Vec<PiUiElement> {
    state
        .typed_elements()
        .into_iter()
        .filter(|e| {
            (matches!(e.placement, PiUiPlacement::Pinned | PiUiPlacement::Inline)
                || (screens && e.placement == PiUiPlacement::Screen))
                && e.kind != PiUiKind::Status
        })
        .chain(
            state
                .widgets
                .iter()
                .filter(|w| !w.lines.is_empty())
                .map(|widget| PiUiElement {
                    id: widget.key.clone(),
                    ns: "widget".into(),
                    kind: PiUiKind::Widget,
                    placement: PiUiPlacement::Pinned,
                    title: Some(widget.key.clone()),
                    lines: widget.lines.clone(),
                    ..Default::default()
                }),
        )
        .collect()
}

fn tray_icon(element: &PiUiElement) -> &'static str {
    if element.placement == PiUiPlacement::Screen {
        return super::screen_icon(&element.ns);
    }
    match (element.ns.as_str(), element.kind) {
        ("todo", _) => icons::CHECKLIST,
        ("subagents", _) => icons::BOT,
        ("bash", _) => icons::TERMINAL,
        ("workflows", _) => icons::LIST,
        ("advisor-live-panel", _) => icons::MAGIC_STICK_3,
        ("decisions" | "jev", _) => icons::TUNING,
        ("btw", _) => icons::CHAT_ROUND_LINE,
        (_, PiUiKind::Roster) => icons::LIST,
        (_, PiUiKind::Log) => icons::TERMINAL,
        _ => icons::WIDGET,
    }
}

/// Rows of the element's roster (top-level or the first roster section).
fn roster_rows(element: &PiUiElement) -> Vec<&zeron_proto::extension_ui::PiUiRosterRow> {
    if !element.rows.is_empty() {
        return element.rows.iter().collect();
    }
    element
        .sections
        .iter()
        .find_map(|s| match s {
            PiUiSection::Roster { rows, .. } => Some(rows.iter().collect()),
            _ => None,
        })
        .unwrap_or_default()
}

/// Whether the element reports live work (it opens by default). A screen's
/// tray only previews it while it is busy; its tab holds the full view.
fn working(element: &PiUiElement) -> bool {
    if element.placement == PiUiPlacement::Screen {
        return element.extra_str("state") == Some("running")
            || element.sections.iter().any(|s| match s {
                PiUiSection::Status { tone, .. } => {
                    *tone == Some(zeron_proto::extension_ui::PiUiTone::Busy)
                }
                PiUiSection::Turns { turns, .. } => turns.iter().any(|t| t.streaming),
                _ => false,
            });
    }
    roster_rows(element).iter().any(|row| {
        matches!(
            row.state,
            Some(PiUiRowState::Running | PiUiRowState::Waiting)
        )
    }) || element.sections.iter().any(|s| {
        matches!(s, PiUiSection::Checklist { items, .. }
                if items.iter().any(|i| i.state != zeron_proto::extension_ui::PiUiItemState::Done))
    }) || element.kind != PiUiKind::Roster
}

/// What a footer chip says about an element still at work: `Background
/// bash · 2 running`, `Advisor · working`; `None` once it is idle.
fn busy_text(element: &PiUiElement) -> Option<String> {
    let running = roster_rows(element)
        .iter()
        .filter(|row| {
            matches!(
                row.state,
                Some(PiUiRowState::Running | PiUiRowState::Waiting)
            )
        })
        .count();
    let busy = running > 0
        || element.extra_str("state") == Some("running")
        || element.sections.iter().any(|s| {
            matches!(s, PiUiSection::Status { tone, .. }
                if *tone == Some(zeron_proto::extension_ui::PiUiTone::Busy))
        });
    if !busy {
        return None;
    }
    // A title that already counts its work (`Background bash · 1 running`)
    // gives its name only.
    let name = element
        .title
        .as_deref()
        .map(|title| title.split(" · ").next().unwrap_or(title).trim())
        .filter(|name| !name.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| zeron_proto::view::pi_extension_label(&element.ns).to_owned());
    Some(match running {
        0 => format!("{name} · working"),
        n => format!("{name} · {n} running"),
    })
}

/// The one line a collapsed tray shows after its title.
fn summary(element: &PiUiElement) -> Option<String> {
    if let Some(text) = element
        .text
        .clone()
        .filter(|t| Some(t) != element.title.as_ref())
    {
        return Some(text);
    }
    element.sections.iter().find_map(|s| match s {
        PiUiSection::Status { text, .. } if !text.is_empty() => Some(text.clone()),
        PiUiSection::Checklist { items, .. } => items
            .iter()
            .find(|i| i.state == zeron_proto::extension_ui::PiUiItemState::Active)
            .map(|i| i.text.clone()),
        _ => None,
    })
}

/// A status chip: its text, whether it reads as active, and the action a
/// click sends (`plan:mode` toggles).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Chip {
    pub key: String,
    pub text: String,
    /// What hovering says when it is more than the text: what a click does.
    pub hint: Option<String>,
    pub active: bool,
    pub action: Option<(String, String)>,
}

fn money(value: f64) -> String {
    if value >= 1.0 {
        format!("${value:.2}")
    } else {
        format!("${value:.3}")
    }
}

/// The footer's monochrome glyph for a chip, like the checkout and branch
/// labels beside it.
fn chip_icon(chip: &Chip) -> &'static str {
    let key = chip.key.to_ascii_lowercase();
    let text = chip_label(&chip.text).to_ascii_lowercase();
    if key == "plan:mode" {
        if text.starts_with("plan") {
            icons::CHECKLIST
        } else {
            icons::ACTION_BUILD
        }
    } else if key == "channel:footer" {
        icons::FAST_TIER
    } else if key.starts_with("status:zeron:") {
        icons::RESTART
    } else if key.contains("goal") {
        icons::CHECKLIST
    } else if key.starts_with("work:") {
        icons::CLOCK_CIRCLE
    } else if key.contains("mcp") || text.starts_with("mcp") {
        icons::REMOTE_SERVER
    } else {
        icons::WIDGET
    }
}

/// A chip's text without the emoji an extension leads it with for a
/// terminal: the footer draws its own icon.
fn chip_label(text: &str) -> &str {
    let label = text
        .trim_start_matches(|c: char| !c.is_ascii() && !c.is_alphanumeric())
        .trim_start();
    if label.is_empty() { text } else { label }
}

/// A plain status line as a chip reads it. pi-goal's terminal line
/// (`goal 36.7k tok · steps 0/3 · 20s`), shown when no goal channel arrives,
/// reads like the channel's chip.
fn status_text(key: &str, text: &str) -> String {
    match text.strip_prefix("goal ") {
        Some(detail) if key == "goal" => format!("Goal · {}", detail.trim()),
        _ => text.to_owned(),
    }
}

/// Chips for the session footer, in a stable order.
pub(crate) fn footer_chips(state: &ExtensionUiState) -> Vec<Chip> {
    let mut chips: Vec<Chip> = state
        .typed_elements()
        .into_iter()
        .filter(|e| e.kind == PiUiKind::Status || e.placement == PiUiPlacement::Status)
        .filter_map(|e| {
            let text = e.text.clone().or_else(|| e.title.clone())?;
            let plan = e.key() == "plan:mode";
            let hint = e
                .actions
                .first()
                .map(|a| {
                    if a.label.is_empty() {
                        a.id.clone()
                    } else {
                        a.label.clone()
                    }
                })
                .map(|label| format!("{text} — {label}"));
            Some(Chip {
                key: e.key(),
                hint,
                active: plan && e.extra_str("mode") == Some("plan"),
                action: e
                    .actions
                    .first()
                    .map(|a| (e.key(), a.id.clone()))
                    .or_else(|| plan.then(|| (e.key(), "toggle".to_owned()))),
                text,
            })
        })
        .collect();
    // Extensions with a status element often also set a plain status line for
    // clients without PIUI (plan mode's "!mode"); show the text once.
    let element_texts: Vec<String> = chips.iter().map(|c| c.text.clone()).collect();
    chips.extend(
        state
            .statuses
            .iter()
            .filter(|s| !s.text.trim().is_empty())
            .filter(|s| !element_texts.iter().any(|t| t == s.text.trim()))
            .map(|s| Chip {
                key: format!("status:{}", s.key),
                hint: None,
                text: status_text(&s.key, s.text.trim()),
                // Zeron's own chips (Pi retrying a request) stand out.
                active: s.key.starts_with("zeron:"),
                action: None,
            }),
    );
    // Work an extension goes on with between turns (background jobs, a
    // fleet, the advisor's review) has its row in the last turn; the footer
    // says it is still going.
    chips.extend(
        state
            .typed_elements()
            .into_iter()
            .filter(|e| matches!(e.placement, PiUiPlacement::Pinned | PiUiPlacement::Inline))
            .filter(|e| e.kind != PiUiKind::Status && e.ns != "todo")
            .filter_map(|e| {
                let text = busy_text(&e)?;
                Some(Chip {
                    key: format!("work:{}", e.key()),
                    hint: None,
                    text,
                    active: true,
                    action: None,
                })
            }),
    );
    if let Some(goal) = state.channels.get(zeron_proto::extension_ui::GOAL_CHANNEL)
        && goal["active"] == true
    {
        // The session panel's Goal section shows the goal; its plain
        // status line and element would say it again here.
        chips.retain(|chip| {
            chip.action.is_some() || !chip.key.to_ascii_lowercase().contains("goal")
        });
    }
    if let Some(footer) = state.channels.get("minimal-status:footer") {
        let mut parts = Vec::new();
        if let Some(tps) = footer["tokensPerSecond"].as_f64().filter(|t| *t > 0.0) {
            parts.push(format!("{tps:.0} tok/s"));
        }
        if let Some(cost) = footer["session"]["costUsd"].as_f64().filter(|c| *c > 0.0) {
            parts.push(money(cost));
        }
        if !parts.is_empty() {
            chips.push(Chip {
                key: "channel:footer".into(),
                hint: None,
                text: parts.join(" · "),
                active: false,
                action: None,
            });
        }
    }
    chips
}

impl Composer {
    fn live_extension_ui(&self, cx: &App) -> Option<ExtensionUiState> {
        let state = self.state.read(cx);
        let chat = state.selected_chat.as_deref()?;
        state.extension_ui.of(chat).cloned()
    }

    /// The pinned trays, outermost first. `below_queue`: a queue tray follows.
    /// Retired: the agent's own extension work is in the transcript as tool
    /// rows now; kept for a surface that wants the live view back.
    #[allow(dead_code)]
    pub(crate) fn render_extension_trays(
        &mut self,
        below_queue: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let Some(state) = self.live_extension_ui(cx) else {
            return Vec::new();
        };
        let elements = tray_elements(&state, !self.side_chat);
        if elements.is_empty() {
            return Vec::new();
        }
        let theme = Theme::of(cx).clone();
        let actions = self.extension_actions(cx);
        let markdown = self.extension.markdown.clone();
        let view = cx.entity_id();
        let inset = if below_queue {
            SIDE_INSET * 2.0
        } else {
            SIDE_INSET
        };
        let mut trays = Vec::with_capacity(elements.len());
        for element in elements {
            let key = element.key();
            let tray = self.extension.trays.entry(key.clone()).or_default();
            let expanded = tray.expanded.unwrap_or_else(|| working(&element));
            let epoch = tray.epoch;
            let header = self.extension_tray_header(&element, expanded, &actions, &theme, view, cx);
            let surface = crate::queue::queue_panel_surface(&theme, cx.has_active_drag())
                .child(header)
                .children(
                    actions
                        .pending
                        .as_ref()
                        .filter(|p| p.action.element_id == key)
                        .and_then(|_| parts::confirm_row(&theme, &key, &actions)),
                );
            let surface = if expanded {
                let body = tray_body(&element, &actions, &theme, window, &markdown, view, cx);
                surface.child(
                    div()
                        .mt(px(2.0))
                        .pb(px(6.0))
                        .child(
                            div()
                                .id(SharedString::from(format!("{key}-tray-body")))
                                .max_h(window.viewport_size().height * 0.3)
                                .overflow_y_scroll()
                                .flex()
                                .flex_col()
                                .gap(px(4.0))
                                .children(body),
                        )
                        .with_animation(
                            SharedString::from(format!("{key}-tray-{epoch}")),
                            motion::FADE_QUICK.animation(),
                            |el, t| el.opacity(t),
                        ),
                )
            } else {
                surface
            };
            trays.push(
                motion::fade_quick(
                    SharedString::from(format!("composer-ext-{key}")),
                    div()
                        .mx(px(inset))
                        .mb(px(-(Theme::SPACE_SM + QUEUE_COMPOSER_OVERLAP)))
                        .child(crate::frost::frosted(
                            crate::queue::PANEL_RADIUS,
                            crate::frost::MENU_BLUR,
                            surface,
                        )),
                )
                .into_any_element(),
            );
        }
        trays
    }

    fn extension_tray_header(
        &mut self,
        element: &PiUiElement,
        expanded: bool,
        actions: &Actions,
        theme: &Theme,
        _view: gpui::EntityId,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let key = element.key();
        let accent = theme.accent;
        let label = if expanded { "Collapse" } else { "Expand" };
        let title = element.title.clone().unwrap_or_else(|| element.ns.clone());
        let toggle_key = key.clone();
        let is_screen = element.placement == PiUiPlacement::Screen;
        let toggle = div()
            .id(SharedString::from(format!("{key}-tray-toggle")))
            .role(gpui::Role::Button)
            .aria_label(SharedString::from(format!("{label} {title}")))
            .flex_1()
            .min_w_0()
            .h(px(HEADER_HEIGHT))
            .px(px(ROW_PAD_X))
            .rounded(px(crate::queue::ROW_RADIUS))
            .flex()
            .items_center()
            .gap(px(8.0))
            .cursor_pointer()
            .hover(|s| s.bg(theme.element_hover))
            .tab_index(0)
            .focus_visible(move |s| s.bg(accent.opacity(0.14)))
            .on_click(cx.listener(move |this, _, window, cx| {
                if is_screen {
                    this.open_extension_screen(&toggle_key, window, cx);
                    return;
                }
                let tray = this.extension.trays.entry(toggle_key.clone()).or_default();
                tray.expanded = Some(!expanded);
                tray.epoch = tray.epoch.wrapping_add(1);
                cx.notify();
            }))
            .tooltip(crate::settings::widgets::text_tooltip(if is_screen {
                "Open"
            } else {
                label
            }))
            .tooltip_show_delay(Duration::from_millis(350))
            .child(
                icon(tray_icon(element))
                    .size(px(14.0))
                    .text_color(theme.text_muted),
            )
            .child(
                div()
                    .flex_none()
                    .max_w(gpui::relative(0.55))
                    .truncate()
                    .text_size(px(parts::TEXT_SIZE))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_color(theme.text)
                    .child(SharedString::from(title)),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_size(px(parts::TEXT_SIZE))
                    .text_color(theme.text_faint)
                    .when(!expanded || is_screen, |el| {
                        el.children(summary(element).map(SharedString::from))
                    }),
            )
            .child(
                icon(if is_screen {
                    icons::ARROW_UP_RIGHT
                } else if expanded {
                    icons::ALT_ARROW_DOWN
                } else {
                    icons::ALT_ARROW_UP
                })
                .size(px(13.0))
                .text_color(theme.text_muted.opacity(0.7)),
            );
        div()
            .flex()
            .items_center()
            .gap(px(2.0))
            .child(toggle)
            // A screen with a toolbar of actions keeps them in its tab: the
            // tray row has room for the title and a few buttons at most.
            .when(!is_screen || element.actions.len() <= 3, |el| {
                el.children(element.actions.iter().enumerate().map(|(ix, action)| {
                    parts::action_button(
                        theme,
                        SharedString::from(format!("{key}-tray-action-{ix}")),
                        &key,
                        None,
                        action,
                        actions,
                    )
                }))
            })
            .into_any_element()
    }

    /// Status chips for the session footer, or `None` without any.
    pub(crate) fn render_extension_footer(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let state = self.live_extension_ui(cx)?;
        let chips = footer_chips(&state);
        if chips.is_empty() {
            return None;
        }
        let theme = Theme::of(cx).clone();
        let accent = theme.accent;
        let app_state = self.state.clone();
        Some(
            div()
                .flex_shrink(1.0)
                .min_w_0()
                .max_w(gpui::relative(0.45))
                .overflow_hidden()
                .flex()
                .items_center()
                .gap(px(4.0))
                .children(chips.into_iter().map(|chip| {
                    // The footer's own labels (checkout, branch): a muted
                    // 12px icon and medium 12px text, no fill. Active chips
                    // read brighter rather than tinted.
                    let color = if chip.active {
                        theme.text.opacity(0.8)
                    } else {
                        theme.text_muted.opacity(0.6)
                    };
                    let text = chip_label(&chip.text).to_owned();
                    let chip_el = div()
                        .id(SharedString::from(format!("ext-chip-{}", chip.key)))
                        .flex_none()
                        .max_w(px(CHIP_MAX_WIDTH))
                        .h(px(20.0))
                        .px(px(8.0))
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(6.0))
                        .rounded(px(crate::pickers::FOOTER_CHIP_RADIUS))
                        .text_size(crate::typography::ui_rems(12.0))
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_color(color)
                        .child(
                            icon(chip_icon(&chip))
                                .size(px(12.0))
                                .flex_none()
                                .text_color(color),
                        )
                        .child(div().min_w_0().truncate().child(SharedString::from(text)))
                        .tooltip(crate::settings::widgets::text_tooltip(
                            chip.hint.clone().unwrap_or_else(|| chip.text.clone()),
                        ))
                        .tooltip_show_delay(Duration::from_millis(350));
                    match chip.action.clone() {
                        Some((element, action)) => {
                            let app_state = app_state.clone();
                            chip_el
                                .role(gpui::Role::Button)
                                .aria_label(SharedString::from(chip.text.clone()))
                                .cursor_pointer()
                                .hover(|s| {
                                    s.bg(theme.element_hover)
                                        .text_color(theme.text.opacity(0.8))
                                })
                                .tab_index(0)
                                .focus_visible(move |s| s.border_1().border_color(accent))
                                .on_click(move |_, _, cx| {
                                    let request = parts::action_of(&element, None, &action, None);
                                    app_state.update(cx, |state, cx| {
                                        state.extension_ui_action(request, cx)
                                    });
                                })
                                .into_any_element()
                        }
                        None => chip_el.into_any_element(),
                    }
                }))
                .into_any_element(),
        )
    }
}

/// What an open tray shows below its header.
#[allow(clippy::too_many_arguments)]
fn tray_body(
    element: &PiUiElement,
    actions: &Actions,
    theme: &Theme,
    window: &Window,
    markdown: &parts::MarkdownCache,
    view: gpui::EntityId,
    cx: &mut App,
) -> Vec<AnyElement> {
    let key = element.key();
    let mut body = Vec::new();
    match element.kind {
        PiUiKind::Roster => {
            body.push(parts::roster(theme, &key, &element.rows, actions, view, cx));
        }
        PiUiKind::Progress => body.push(parts::progress(
            theme,
            element.phase.as_deref(),
            element.done,
            element.total,
        )),
        PiUiKind::Markdown => {
            if let Some(text) = &element.text {
                body.push(parts::markdown(theme, &key, text, window, markdown));
            }
        }
        PiUiKind::Diff => {
            if let Some(diff) = &element.unified_diff {
                body.push(parts::diff(theme, diff));
            }
        }
        PiUiKind::Log | PiUiKind::Widget => {
            if !element.lines.is_empty() {
                body.push(parts::log(theme, &element.lines, Some(40)));
            }
        }
        _ => {
            if !element.lines.is_empty() {
                body.push(parts::log(theme, &element.lines, Some(40)));
            }
        }
    }
    for (ix, section) in element.sections.iter().enumerate() {
        if let Some(rendered) = parts::section(
            theme, &key, ix, section, actions, window, markdown, view, cx,
        ) {
            body.push(rendered);
        }
    }
    if body.is_empty() {
        // A kind this client does not draw natively: its plain text.
        let lines = element.text_lines();
        if !lines.is_empty() {
            body.push(parts::log(theme, &lines, Some(40)));
        }
    }
    body
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use zeron_proto::extension_ui::{ExtensionStatus, ExtensionWidget};

    #[test]
    fn trays_and_chips_come_from_elements_widgets_statuses_and_channels() {
        let mut state = ExtensionUiState {
            elements: vec![
                json!({"id":"jobs","ns":"bash","kind":"roster","placement":"pinned","title":"Background bash · 1 running",
                    "rows":[{"id":"j1","label":"npm test","state":"running"}]}),
                json!({"id":"mode","ns":"plan","kind":"status","placement":"status","text":"Plan","mode":"plan"}),
                json!({"id":"question","ns":"ask-user","kind":"panel","placement":"sheet"}),
                json!({"id":"thread","ns":"btw","kind":"panel","placement":"screen","title":"btw"}),
            ],
            widgets: vec![ExtensionWidget {
                key: "advisor".into(),
                lines: vec!["thinking".into()],
                placement: None,
            }],
            statuses: vec![
                ExtensionStatus {
                    key: "!mode".into(),
                    text: "Build".into(),
                },
                // pi-goal's plain status line, for clients without channels.
                ExtensionStatus {
                    key: "goal".into(),
                    text: "goal: ship it".into(),
                },
                ExtensionStatus {
                    key: "zeron:retry".into(),
                    text: "Retrying · attempt 1/3".into(),
                },
            ],
            ..Default::default()
        };
        state.channels.insert(
            "minimal-status:footer".into(),
            json!({"tokensPerSecond":41.6,"session":{"costUsd":0.0421}}),
        );
        state.channels.insert(
            "pi-goal:status".into(),
            json!({"active":true,"text":"ship it"}),
        );
        let trays: Vec<String> = tray_elements(&state, true)
            .iter()
            .map(|e| e.key())
            .collect();
        assert_eq!(trays, vec!["bash:jobs", "btw:thread", "widget:advisor"]);
        assert_eq!(tray_elements(&state, false).len(), 2);
        assert!(working(&tray_elements(&state, true)[0]));
        let chips = footer_chips(&state);
        let texts: Vec<&str> = chips.iter().map(|c| c.text.as_str()).collect();
        assert_eq!(
            texts,
            vec![
                "Plan",
                "Build",
                "Retrying · attempt 1/3",
                "Background bash · 1 running",
                "42 tok/s · $0.042"
            ]
        );
        assert!(chips[0].active);
        assert!(chips[2].active);
        assert_eq!(chips[0].action, Some(("plan:mode".into(), "toggle".into())));
        assert_eq!(chips[0].hint, None, "a toggle with no action of its own");
        assert_eq!(chips[1].action, None);
    }

    #[test]
    fn a_finished_roster_starts_collapsed_and_reports_its_summary() {
        let element: PiUiElement = serde_json::from_value(json!({"id":"fleet","ns":"subagents",
            "kind":"roster","placement":"pinned","title":"Subagents","text":"1 finished",
            "rows":[{"id":"a","label":"scout","state":"done"}]}))
        .unwrap();
        assert!(!working(&element));
        assert_eq!(summary(&element).as_deref(), Some("1 finished"));
        assert_eq!(tray_icon(&element), icons::BOT);
    }

    #[test]
    fn a_status_line_repeating_an_element_chip_shows_once() {
        let state = ExtensionUiState {
            elements: vec![
                json!({"id":"mode","ns":"plan","kind":"status","placement":"status",
                "text":"Build","mode":"build"}),
            ],
            statuses: vec![
                ExtensionStatus {
                    key: "!mode".into(),
                    text: "Build".into(),
                },
                ExtensionStatus {
                    key: "rtk".into(),
                    text: "RTK on".into(),
                },
            ],
            ..Default::default()
        };
        let texts: Vec<String> = footer_chips(&state).into_iter().map(|c| c.text).collect();
        assert_eq!(texts, vec!["Build", "RTK on"]);
    }

    #[test]
    fn a_screen_tray_previews_only_while_busy_and_wears_its_screen_icon() {
        let idle: PiUiElement = serde_json::from_value(json!({"id":"dashboard","ns":"usage",
            "kind":"panel","placement":"screen","title":"Usage · Today",
            "sections":[{"kind":"status","id":"total","text":"$0.00"}]}))
        .unwrap();
        assert!(!working(&idle));
        assert_eq!(tray_icon(&idle), icons::FILE_DATA);
        let streaming: PiUiElement = serde_json::from_value(json!({"id":"thread","ns":"btw",
            "kind":"panel","placement":"screen","title":"btw",
            "sections":[{"kind":"turns","id":"t","turns":[{"role":"assistant","text":"…","streaming":true}]}]}))
        .unwrap();
        assert!(working(&streaming));
    }

    #[test]
    fn a_goal_status_line_reads_like_the_goal_chip() {
        assert_eq!(
            status_text("goal", "goal 36.7k tok · steps 0/3 · 20s"),
            "Goal · 36.7k tok · steps 0/3 · 20s"
        );
        assert_eq!(status_text("goal", "goal: ship it"), "goal: ship it");
        assert_eq!(status_text("other", "goal 1"), "goal 1");
    }

    #[test]
    fn a_footer_chip_drops_its_terminal_emoji_and_wears_a_footer_icon() {
        let chip = |key: &str, text: &str| Chip {
            key: key.into(),
            text: text.into(),
            hint: None,
            active: false,
            action: None,
        };
        assert_eq!(
            chip_label("🔌 MCP: 8 servers enabled"),
            "MCP: 8 servers enabled"
        );
        assert_eq!(chip_label("$0.014"), "$0.014");
        assert_eq!(chip_label("Économie"), "Économie");
        assert_eq!(chip_label("✨"), "✨");
        assert_eq!(chip_icon(&chip("plan:mode", "Build")), icons::ACTION_BUILD);
        assert_eq!(chip_icon(&chip("plan:mode", "Plan")), icons::CHECKLIST);
        assert_eq!(
            chip_icon(&chip("status:mcp", "🔌 MCP: 8 servers enabled")),
            icons::REMOTE_SERVER
        );
        assert_eq!(
            chip_icon(&chip("channel:footer", "121 tok/s · $0.014")),
            icons::FAST_TIER
        );
        assert_eq!(
            chip_icon(&chip("status:zeron:retry", "Retrying")),
            icons::RESTART
        );
        assert_eq!(chip_icon(&chip("status:other", "Indexing")), icons::WIDGET);
    }
}
