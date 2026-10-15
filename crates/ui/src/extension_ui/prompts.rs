//! The prompt panel: everything that asks the user to answer, pick or type
//! (the agent's questions, extension sheets, the btw thread and the reports
//! a command opens) takes the composer's place, one at a time. Several at
//! once queue behind each other as tabs above the panel; the front one is
//! the one shown. A panel nobody waits on (btw, a report) can step back to
//! the message box, so the chat can go on beside it: its tab toggles it.
//! The message box has no tab of its own.

use std::collections::HashSet;

use gpui::{
    AnyElement, App, Context, Entity, KeyDownEvent, SharedString, Window, div, prelude::*, px,
};
use zeron_proto::extension_ui::{PiUiElement, PiUiPlacement};

use super::screen::{ScreenView, screen_icon, screen_title};
use super::sheet::header_label;
use crate::composer::{COMPOSER_RADIUS, Composer};
use crate::icons::{self, icon};
use crate::motion;
use crate::theme::Theme;

/// The message box's own tab, offered while only non-blocking prompts wait.
pub(crate) const MESSAGE_TAB: &str = "message";
const QUESTION_PREFIX: &str = "question:";
/// Pi's own dialogs, drawn as sheets (`pi:dialog-N`).
const DIALOG_PREFIX: &str = "pi:dialog-";

/// What the composer shows in its place.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Front {
    /// The agent's question (the wizard).
    Question,
    /// A sheet or screen, by `ns:id`.
    Element(String),
    /// The message box, with the prompts' tabs above it.
    Message,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct PromptTab {
    /// `question:<request id>` or an element's `ns:id`.
    pub id: String,
    pub label: String,
    pub icon: &'static str,
    /// Someone waits on the answer (the agent's question, a sheet): the
    /// message box is not offered while one is open.
    pub blocking: bool,
}

/// The open prompts in arrival order: the agent's question first (it holds
/// the turn), then the extensions' sheets and screens as they were created.
pub(crate) fn prompt_tabs(
    question: Option<(&str, &str)>,
    elements: &[PiUiElement],
) -> Vec<PromptTab> {
    let question = question.map(|(request_id, header)| PromptTab {
        id: format!("{QUESTION_PREFIX}{request_id}"),
        label: if header.trim().is_empty() {
            "Question".into()
        } else {
            header.trim().to_owned()
        },
        icon: icons::CHAT_ROUND_LINE,
        blocking: true,
    });
    let elements =
        elements
            .iter()
            .filter(|element| element.is_prompt())
            .map(|element| match element.placement {
                PiUiPlacement::Sheet => PromptTab {
                    id: element.key(),
                    label: header_label(&element.ns).to_owned(),
                    icon: icons::CHAT_ROUND_LINE,
                    blocking: true,
                },
                _ => PromptTab {
                    id: element.key(),
                    label: screen_title(element),
                    icon: screen_icon(&element.ns),
                    blocking: false,
                },
            });
    question.into_iter().chain(elements).collect()
}

/// Whether the message box is offered as a tab beside `tabs`.
pub(crate) fn offers_message(tabs: &[PromptTab]) -> bool {
    !tabs.is_empty() && tabs.iter().all(|tab| !tab.blocking)
}

/// The tab shown: the one the user picked while it is still open, else the
/// first in line. `None` with nothing open.
pub(crate) fn front<'a>(tabs: &'a [PromptTab], picked: Option<&str>) -> Option<&'a str> {
    if tabs.is_empty() {
        return None;
    }
    if picked == Some(MESSAGE_TAB) && offers_message(tabs) {
        return Some(MESSAGE_TAB);
    }
    picked
        .and_then(|picked| tabs.iter().find(|tab| tab.id == picked))
        .or_else(|| tabs.first())
        .map(|tab| tab.id.as_str())
}

/// The pick after the open prompts changed from `known` to `tabs`. `known`
/// is `None` for a chat just opened: what was already up then waits behind
/// the message box. `covered` is the screen a Pi dialog came forward over.
pub(crate) fn repick(
    picked: Option<&str>,
    known: Option<&HashSet<String>>,
    tabs: &[PromptTab],
    covered: Option<&str>,
) -> Option<String> {
    if tabs.is_empty() {
        return None;
    }
    let Some(known) = known else {
        return offers_message(tabs)
            .then(|| MESSAGE_TAB.to_owned())
            .or_else(|| picked.map(str::to_owned));
    };
    let open = |id: &str| tabs.iter().any(|tab| tab.id == id);
    let picked = match picked {
        // The picked prompt went away: back to the screen its dialog came
        // over, else the message box when it is offered, else the first in
        // line.
        Some(id) if id != MESSAGE_TAB && !open(id) => covered
            .filter(|covered| open(covered))
            .map(str::to_owned)
            .or_else(|| offers_message(tabs).then(|| MESSAGE_TAB.to_owned())),
        other => other.map(str::to_owned),
    };
    // A new prompt comes to the front only over the message box; behind
    // another prompt it waits as a tab. A dialog Pi opens over a screen
    // answers what was just done there (btw's thinking picker, a model's
    // Load), so it comes forward.
    let fresh = tabs.iter().find(|tab| !known.contains(&tab.id));
    let on_screen = |picked: &str| tabs.iter().any(|tab| tab.id == picked && !tab.blocking);
    match (fresh, picked) {
        (Some(fresh), Some(picked)) if picked == MESSAGE_TAB => Some(fresh.id.clone()),
        (Some(fresh), Some(picked))
            if fresh.id.starts_with(DIALOG_PREFIX) && on_screen(&picked) =>
        {
            Some(fresh.id.clone())
        }
        (_, picked) => picked,
    }
}

impl Composer {
    /// The selected chat's live elements, if its UI is being watched.
    fn live_elements(&self, cx: &App) -> Vec<PiUiElement> {
        let state = self.state.read(cx);
        state
            .selected_chat
            .as_deref()
            .and_then(|chat| state.extension_ui.of(chat))
            .map(|live| live.typed_elements())
            .unwrap_or_default()
    }

    fn picked_prompt(&self, cx: &App) -> Option<String> {
        let chat = self.state.read(cx).selected_chat.as_deref()?;
        self.extension.picked.get(chat).cloned()
    }

    /// The open prompts, in line. A side chat's composer leaves screens to
    /// the main conversation.
    pub(crate) fn prompt_queue(&self, cx: &App) -> Vec<PromptTab> {
        let question = self.wizard.as_ref().map(|wizard| {
            let header = wizard
                .questions
                .first()
                .map(|q| q.header.as_str())
                .unwrap_or_default();
            (wizard.request_id.as_str(), header)
        });
        let mut elements = self.live_elements(cx);
        if self.side_chat {
            elements.retain(|element| element.placement == PiUiPlacement::Sheet);
        }
        prompt_tabs(question, &elements)
    }

    /// What takes the composer's place now, or `None` for the plain composer.
    pub(crate) fn front_prompt(&self, cx: &App) -> Option<Front> {
        let tabs = self.prompt_queue(cx);
        let picked = self.picked_prompt(cx);
        let id = front(&tabs, picked.as_deref())?;
        Some(if id == MESSAGE_TAB {
            Front::Message
        } else if id.starts_with(QUESTION_PREFIX) {
            Front::Question
        } else {
            Front::Element(id.to_owned())
        })
    }

    /// Follow the open prompts as they come and go (see [`repick`]).
    pub(crate) fn track_new_prompts(&mut self, cx: &mut Context<Self>) {
        let Some(chat_id) = self.state.read(cx).selected_chat.clone() else {
            return;
        };
        let tabs = self.prompt_queue(cx);
        // Until the watch's first frame the elements are unknown, not gone.
        let ready = self.state.read(cx).extension_ui.ready_for(&chat_id);
        if !ready && tabs.is_empty() {
            return;
        }
        let ids: HashSet<String> = tabs.iter().map(|tab| tab.id.clone()).collect();
        let before = self.extension.picked.get(&chat_id).cloned();
        let covered = self.extension.covered.get(&chat_id).cloned();
        let picked = repick(
            before.as_deref(),
            self.extension.seen.get(&chat_id),
            &tabs,
            covered.as_deref(),
        );
        // A dialog that came over a screen returns to it once answered.
        let over_screen = before
            .as_deref()
            .is_some_and(|before| tabs.iter().any(|tab| tab.id == before && !tab.blocking));
        match picked.as_deref() {
            Some(dialog) if dialog.starts_with(DIALOG_PREFIX) => {
                if over_screen && let Some(before) = before.clone() {
                    self.extension.covered.insert(chat_id.clone(), before);
                }
            }
            _ => {
                self.extension.covered.remove(&chat_id);
            }
        }
        // Back on the message box: the keyboard goes with it.
        if picked.as_deref() == Some(MESSAGE_TAB) && before.as_deref() != Some(MESSAGE_TAB) {
            self.focus_pending = true;
        }
        match picked {
            Some(picked) => self.extension.picked.insert(chat_id.clone(), picked),
            None => self.extension.picked.remove(&chat_id),
        };
        self.extension.seen.insert(chat_id, ids.clone());
        // A screen that went away takes its docked view with it, and a sheet
        // its form (a sheet re-raised under the same key starts afresh).
        self.extension
            .screen_views
            .retain(|key, _| ids.contains(key));
        if self
            .extension
            .sheet
            .as_ref()
            .is_some_and(|form| !ids.contains(form.key()))
        {
            self.extension.sheet = None;
        }
    }

    fn pick_prompt(&mut self, id: String, window: &mut Window, cx: &mut Context<Self>) {
        let Some(chat_id) = self.state.read(cx).selected_chat.clone() else {
            return;
        };
        if id == MESSAGE_TAB {
            self.focus_pending = true;
        } else if id.starts_with(QUESTION_PREFIX) {
            window.focus(&self.wizard_focus, cx);
        } else if let Some(form) = self.extension.sheet.as_mut() {
            form.refocus();
        }
        self.extension.picked.insert(chat_id, id);
        cx.notify();
    }

    /// Ctrl+Tab / Ctrl+Shift+Tab walk the tabs.
    pub(crate) fn on_prompt_tabs_key(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if event.keystroke.key != "tab" || !event.keystroke.modifiers.control {
            return;
        }
        let tabs = self.prompt_queue(cx);
        let mut ids: Vec<String> = tabs.iter().map(|tab| tab.id.clone()).collect();
        if offers_message(&tabs) {
            ids.push(MESSAGE_TAB.to_owned());
        }
        if ids.len() < 2 {
            return;
        }
        let picked = self.picked_prompt(cx);
        let current = front(&tabs, picked.as_deref())
            .unwrap_or_default()
            .to_owned();
        let at = ids.iter().position(|id| *id == current).unwrap_or(0);
        let next = if event.keystroke.modifiers.shift {
            (at + ids.len() - 1) % ids.len()
        } else {
            (at + 1) % ids.len()
        };
        let id = ids[next].clone();
        self.pick_prompt(id, window, cx);
        cx.stop_propagation();
    }

    /// The tabs above the panel: when more than one prompt is open, or a
    /// screen can step back to the message box (its tab toggles it).
    pub(crate) fn render_prompt_tabs(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let tabs = self.prompt_queue(cx);
        let message = offers_message(&tabs);
        if tabs.len() < 2 && !message {
            return None;
        }
        let theme = Theme::of(cx).clone();
        let picked = self.picked_prompt(cx);
        let current = front(&tabs, picked.as_deref())
            .unwrap_or_default()
            .to_owned();
        let position = tabs
            .iter()
            .position(|tab| tab.id == current)
            .map(|at| format!("{} of {}", at + 1, tabs.len()));
        let entries = tabs
            .into_iter()
            .map(|tab| (tab.id, tab.label, tab.icon, tab.blocking));
        let mut row: Vec<AnyElement> = Vec::new();
        for (ix, (id, label, glyph, blocking)) in entries.enumerate() {
            let active = id == current;
            let hover = format!("prompt-tab-{ix}");
            let color = if active { theme.text } else { theme.text_muted };
            row.push(
                div()
                    .id(SharedString::from(hover.clone()))
                    .flex_none()
                    .h(px(26.0))
                    .max_w(px(220.0))
                    .px(px(10.0))
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .rounded(px(9.0))
                    .border_1()
                    .border_color(if active {
                        crate::theme::ink(0.16)
                    } else {
                        gpui::transparent_black()
                    })
                    .bg(if active {
                        crate::theme::ink(0.09)
                    } else {
                        motion::hover_blend(
                            &hover,
                            crate::theme::ink(0.025),
                            crate::theme::ink(0.06),
                        )
                    })
                    .on_hover(motion::hover_listener(hover.clone()))
                    .cursor_pointer()
                    .role(gpui::Role::Tab)
                    .aria_label(SharedString::from(label.clone()))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        // A shown screen's tab steps back to the message box.
                        let pick = if active && !blocking && message {
                            MESSAGE_TAB.to_owned()
                        } else {
                            id.clone()
                        };
                        this.pick_prompt(pick, window, cx)
                    }))
                    .child(icon(glyph).size(px(12.0)).text_color(color))
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .text_size(crate::typography::ui_rems(12.0))
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_color(color)
                            .child(SharedString::from(label)),
                    )
                    // A question waiting behind another tab says so.
                    .when(blocking && !active, |el| {
                        el.child(
                            div()
                                .flex_none()
                                .size(px(5.0))
                                .rounded_full()
                                .bg(theme.accent),
                        )
                    })
                    .into_any_element(),
            );
        }
        Some(
            div()
                .id("prompt-tabs")
                .mx(px(10.0))
                .flex()
                .items_center()
                .gap(px(4.0))
                .child(
                    div()
                        .id("prompt-tabs-row")
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .items_center()
                        .gap(px(4.0))
                        .overflow_x_scroll()
                        .children(row),
                )
                .when_some(position, |el, position| {
                    el.child(
                        div()
                            .flex_none()
                            .pl(px(8.0))
                            .text_size(crate::typography::ui_rems(10.5))
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_color(theme.text_muted.opacity(0.6))
                            .child(SharedString::from(position)),
                    )
                })
                .into_any_element(),
        )
    }

    /// A screen (btw, a report) docked in the composer's place, on the same
    /// frosted surface as the composer it stands in for.
    pub(crate) fn render_extension_screen(
        &mut self,
        key: &str,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let element = self
            .live_elements(cx)
            .into_iter()
            .find(|element| element.key() == key && element.placement == PiUiPlacement::Screen)?;
        let chat_id = self.state.read(cx).selected_chat.clone()?;
        let docked = self
            .extension
            .screen_views
            .get(key)
            .filter(|view| view.read(cx).chat_id == chat_id)
            .cloned();
        let view: Entity<ScreenView> = match docked {
            Some(view) => view,
            None => {
                let state = self.state.clone();
                let title = screen_title(&element);
                let owned = key.to_owned();
                let view = cx.new(|cx| ScreenView::new(state, chat_id, owned, title, cx).docked());
                self.extension
                    .screen_views
                    .insert(key.to_owned(), view.clone());
                view
            }
        };
        let theme = Theme::of(cx).clone();
        let panel = div()
            .id("extension-screen")
            .occlude()
            .rounded(px(COMPOSER_RADIUS))
            .border_1()
            .border_color(theme.border)
            .bg(theme.composer_surface_bg())
            .when(!theme.is_frost(), |el| el.shadow_lg())
            .overflow_hidden()
            .child(view);
        Some(
            crate::frost::frosted(COMPOSER_RADIUS, crate::frost::MENU_BLUR, panel)
                .into_any_element(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn element(value: serde_json::Value) -> PiUiElement {
        serde_json::from_value(value).unwrap()
    }

    fn ids(tabs: &[PromptTab]) -> Vec<&str> {
        tabs.iter().map(|tab| tab.id.as_str()).collect()
    }

    fn btw() -> PiUiElement {
        element(json!({"id":"thread","ns":"btw","kind":"panel","placement":"screen","title":"btw"}))
    }

    fn ask() -> PiUiElement {
        element(json!({"id":"question","ns":"ask-user","kind":"panel","placement":"sheet"}))
    }

    fn known(tabs: &[PromptTab]) -> HashSet<String> {
        tabs.iter().map(|tab| tab.id.clone()).collect()
    }

    #[test]
    fn prompts_queue_in_arrival_order_and_the_agents_work_is_not_one() {
        let elements = vec![
            btw(),
            element(
                json!({"id":"panel","ns":"advisor-live-panel","kind":"panel","placement":"pinned"}),
            ),
            ask(),
            element(json!({"id":"mode","ns":"plan","kind":"status","placement":"status"})),
        ];
        let tabs = prompt_tabs(Some(("r1", "Database")), &elements);
        assert_eq!(
            ids(&tabs),
            vec!["question:r1", "btw:thread", "ask-user:question"]
        );
        assert_eq!(tabs[0].label, "Database");
        assert_eq!(tabs[2].label, "Question");
        assert!(tabs[0].blocking && !tabs[1].blocking && tabs[2].blocking);
        assert!(!offers_message(&tabs));
        // The first in line shows until the user picks another.
        assert_eq!(front(&tabs, None), Some("question:r1"));
        assert_eq!(front(&tabs, Some("btw:thread")), Some("btw:thread"));
        assert_eq!(front(&tabs, Some("gone:x")), Some("question:r1"));
        // The message box is no tab while someone waits on an answer.
        assert_eq!(front(&tabs, Some(MESSAGE_TAB)), Some("question:r1"));
    }

    #[test]
    fn a_screen_alone_offers_the_message_box_beside_it() {
        let tabs = prompt_tabs(None, &[btw()]);
        assert!(offers_message(&tabs));
        assert_eq!(front(&tabs, None), Some("btw:thread"));
        assert_eq!(front(&tabs, Some(MESSAGE_TAB)), Some(MESSAGE_TAB));
        assert_eq!(tabs[0].icon, icons::CHAT_ROUND_LINE);
        assert_eq!(front(&[], Some(MESSAGE_TAB)), None);
    }

    #[test]
    fn new_prompts_come_forward_over_the_message_box_and_queue_behind_a_prompt() {
        // Opening a chat: a screen already up waits behind the message box.
        let screen = prompt_tabs(None, &[btw()]);
        assert_eq!(
            repick(None, None, &screen, None).as_deref(),
            Some(MESSAGE_TAB)
        );
        // `/btw` typed in the message box comes to the front.
        let none = HashSet::new();
        assert_eq!(
            repick(Some(MESSAGE_TAB), Some(&none), &screen, None).as_deref(),
            Some("btw:thread")
        );
        // With nothing picked the first in line shows: the new btw.
        assert_eq!(repick(None, Some(&none), &screen, None), None);
        assert_eq!(front(&screen, None), Some("btw:thread"));
        // The agent asks while btw is in front: btw stays, the question waits.
        let both = prompt_tabs(None, &[btw(), ask()]);
        let picked = repick(Some("btw:thread"), Some(&known(&screen)), &both, None);
        assert_eq!(picked.as_deref(), Some("btw:thread"));
        assert_eq!(front(&both, picked.as_deref()), Some("btw:thread"));
        // btw closes: the question is next in line.
        let question = prompt_tabs(None, &[ask()]);
        let picked = repick(picked.as_deref(), Some(&known(&both)), &question, None);
        assert_eq!(
            front(&question, picked.as_deref()),
            Some("ask-user:question")
        );
        // The last prompt closes: back to the plain composer.
        assert_eq!(
            repick(picked.as_deref(), Some(&known(&question)), &[], None),
            None
        );
        // A screen closing beside another screen returns to the message box.
        let report = element(
            json!({"id":"dashboard","ns":"usage","kind":"panel","placement":"screen","title":"Usage"}),
        );
        let two = prompt_tabs(None, &[btw(), report.clone()]);
        let one = prompt_tabs(None, &[report]);
        assert_eq!(
            repick(Some("btw:thread"), Some(&known(&two)), &one, None).as_deref(),
            Some(MESSAGE_TAB)
        );
        // btw's thinking picker (a Pi dialog) comes over btw, then returns.
        let dialog = element(json!({"id":"dialog-1","ns":"pi","kind":"panel","placement":"sheet"}));
        let picker = prompt_tabs(None, &[btw(), dialog]);
        let picked = repick(Some("btw:thread"), Some(&known(&screen)), &picker, None);
        assert_eq!(picked.as_deref(), Some("pi:dialog-1"));
        assert_eq!(
            repick(
                picked.as_deref(),
                Some(&known(&picker)),
                &screen,
                Some("btw:thread")
            )
            .as_deref(),
            Some("btw:thread")
        );
    }
}
