//! Settings → General → Advisor: the model Pi's advisor consults and up to
//! two fallbacks, each chosen with the composer's own model picker held to
//! Pi (see [`Pickers::new_for_pi_model`]). The engine keeps them in Pi's
//! `loop.json`, which the advisor re-reads on every consultation.

use gpui::{AnyElement, Context, Entity, Subscription, Task, div, prelude::*, px};
use zeron_engine::advisor_settings::{self, AdvisorSettings};
use zeron_engine::registry::TitleSettings;
use zeron_proto::HarnessId;
use zeron_proto::reserved_models;
use zeron_rpc::methods;

use crate::pickers::{Pickers, TitleModelPicked};
use crate::popover::Loadable;
use crate::settings::widgets;
use crate::state::AppState;
use crate::theme::Theme;

/// One row per picker: the model, then each fallback.
const ROWS: [&str; 1 + advisor_settings::MAX_FALLBACKS] = ["Model", "Fallback 1", "Fallback 2"];

const DESCRIPTION: &str = "The model your agent consults when it is stuck on something after \
     three failed tries. Fallbacks are tried in order when it is unavailable.";

const BLOCKED: &str = "Blocked: Claude Fable and GPT Astra class models are reserved for \
     sessions you start. Pick another model.";

pub struct AdvisorCard {
    state: Entity<AppState>,
    settings: Loadable<AdvisorSettings>,
    pickers: Vec<Entity<Pickers>>,
    error: Option<String>,
    task: Option<Task<()>>,
    _picked: Vec<Subscription>,
    _state: Subscription,
}

impl AdvisorCard {
    pub fn new(state: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        let mut pickers = Vec::new();
        let mut picked = Vec::new();
        for row in 0..ROWS.len() {
            let unset = if row == 0 {
                "Pi default (Claude Opus 5)"
            } else {
                "None"
            };
            let picker = {
                let state = state.clone();
                cx.new(|cx| Pickers::new_for_pi_model(state, None, unset, cx))
            };
            picked.push(cx.subscribe(
                &picker,
                move |this: &mut Self, _, event: &TitleModelPicked, cx| {
                    // Pi's catalog ids are already `provider/id` specs.
                    if let Some(model) = event.0.model.as_deref().and_then(advisor_settings::spec) {
                        this.set_row(row, Some(model), cx);
                    }
                },
            ));
            pickers.push(picker);
        }
        // Settings can open before the engine connects: load once it does.
        let state_sub = cx.observe(&state, |this: &mut Self, _, cx| {
            if matches!(this.settings, Loadable::Idle) {
                this.call(None, cx);
            }
        });
        let mut card = Self {
            state,
            settings: Loadable::Idle,
            pickers,
            error: None,
            task: None,
            _picked: picked,
            _state: state_sub,
        };
        card.call(None, cx);
        card
    }

    fn set_row(&mut self, row: usize, model: Option<String>, cx: &mut Context<Self>) {
        if let Some(settings) = self.settings.ready().cloned() {
            self.call(Some(with_row(settings, row, model)), cx);
        }
    }

    /// Each picker shows its row of `settings`.
    fn show_in_pickers(&self, settings: &AdvisorSettings, cx: &mut Context<Self>) {
        for (row, picker) in self.pickers.iter().enumerate() {
            let model = row_model(settings, row).map(str::to_owned);
            picker.update(cx, |picker, cx| {
                picker.set_title_settings(
                    TitleSettings {
                        harness: Some(HarnessId::Pi),
                        model,
                    },
                    cx,
                )
            });
        }
    }

    /// `GetAdvisorSettings`, or `SetAdvisorSettings` when saving; either
    /// reply is what the device's `loop.json` now sets.
    fn call(&mut self, save: Option<AdvisorSettings>, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let (method, params) = match save {
            Some(settings) => (
                methods::SET_ADVISOR_SETTINGS,
                serde_json::to_value(settings).unwrap_or_default(),
            ),
            None => (methods::GET_ADVISOR_SETTINGS, serde_json::json!({})),
        };
        if matches!(self.settings, Loadable::Idle) {
            self.settings = Loadable::Loading;
        }
        self.task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(method, params)
                .await
                .map_err(|e| e.to_string())
                .and_then(|v| {
                    serde_json::from_value::<AdvisorSettings>(v).map_err(|e| e.to_string())
                });
            this.update(cx, |card, cx| {
                match result {
                    Ok(settings) => {
                        card.show_in_pickers(&settings, cx);
                        card.settings = Loadable::Ready(settings);
                        card.error = None;
                    }
                    // The engine's refusal (a reserved model, say) goes to
                    // the toast, and the pickers go back to what is saved.
                    Err(error) if matches!(card.settings, Loadable::Ready(_)) => {
                        if let Some(settings) = card.settings.ready().cloned() {
                            card.show_in_pickers(&settings, cx);
                        }
                        card.error = Some(error);
                    }
                    Err(error) => card.settings = Loadable::Error(error),
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }
}

/// The model a row shows: row 0 the advisor's, then each fallback.
fn row_model(settings: &AdvisorSettings, row: usize) -> Option<&str> {
    match row {
        0 => settings.model.as_deref(),
        row => settings.fallbacks.get(row - 1).map(String::as_str),
    }
}

/// A reserved model (Claude Fable, GPT Astra) in a row: a `loop.json` can
/// still name one, but the advisor cannot run on it and the engine will not
/// save it again.
fn is_blocked(model: Option<&str>) -> bool {
    model.is_some_and(|m| reserved_models::reserved_family(m).is_some())
}

/// `settings` with a row set, or cleared with `None`. Clearing the model
/// clears its fallbacks; clearing a fallback moves the next one up.
fn with_row(mut settings: AdvisorSettings, row: usize, model: Option<String>) -> AdvisorSettings {
    match (row, model) {
        (0, None) => return AdvisorSettings::default(),
        (0, Some(model)) => settings.model = Some(model),
        (row, Some(model)) => match settings.fallbacks.get_mut(row - 1) {
            Some(slot) => *slot = model,
            None => settings.fallbacks.push(model),
        },
        (row, None) => {
            if row - 1 < settings.fallbacks.len() {
                settings.fallbacks.remove(row - 1);
            }
        }
    }
    settings
}

/// Rows on screen: the model always, a fallback once the row above it is
/// set (a fallback without a model is never used).
fn shown_rows(settings: &AdvisorSettings) -> usize {
    match settings.model {
        Some(_) => (2 + settings.fallbacks.len()).min(ROWS.len()),
        None => 1,
    }
}

impl Render for AdvisorCard {
    fn render(&mut self, _: &mut gpui::Window, cx: &mut Context<Self>) -> impl IntoElement {
        crate::toast::take_error(&mut self.error, cx);
        let theme = Theme::of(cx).for_settings_surface();
        let block = widgets::section_card(&theme).mt_0();
        let Some(settings) = self.settings.ready().cloned() else {
            let line = match &self.settings {
                Loadable::Error(error) => error.clone(),
                _ => "Loading…".into(),
            };
            return widgets::section(
                &theme,
                "Advisor",
                block.child(
                    widgets::card_row(&theme, true)
                        .child(
                            div()
                                .flex_1()
                                .min_w(px(160.0))
                                .child(widgets::row_title(&theme, ROWS[0]))
                                .child(widgets::meta_line(
                                    &theme,
                                    vec![div().child(DESCRIPTION).into_any_element()],
                                )),
                        )
                        .child(div().text_color(theme.text_muted).child(line)),
                ),
            );
        };
        let rows: Vec<AnyElement> = (0..shown_rows(&settings))
            .map(|row| {
                let set = row_model(&settings, row).is_some();
                let reset = widgets::text_action(&theme, widgets::ActionTone::Quiet, "Reset")
                    .id(("advisor-reset", row))
                    .tab_index(0)
                    .role(gpui::Role::Button)
                    .aria_label(if row == 0 {
                        "Use Pi's default advisor"
                    } else {
                        "Remove this fallback"
                    })
                    .focus_visible(|s| s.border_2().border_color(theme.accent))
                    .on_click(cx.listener(move |card, _, _, cx| card.set_row(row, None, cx)));
                let control = div()
                    .flex_none()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(4.0))
                    .when(set, |el| el.child(reset))
                    .child(self.pickers[row].clone());
                let mut title = div()
                    .flex_1()
                    .min_w(px(160.0))
                    .child(widgets::row_title(&theme, ROWS[row]));
                if row == 0 {
                    title = title.child(widgets::meta_line(
                        &theme,
                        vec![div().child(DESCRIPTION).into_any_element()],
                    ));
                }
                if is_blocked(row_model(&settings, row)) {
                    title = title.child(widgets::meta_line(
                        &theme,
                        vec![
                            div()
                                .text_color(theme.danger)
                                .child(BLOCKED)
                                .into_any_element(),
                        ],
                    ));
                }
                widgets::card_row(&theme, row == 0)
                    .child(title)
                    .child(control)
                    .into_any_element()
            })
            .collect();
        widgets::section(&theme, "Advisor", block.children(rows))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings(model: Option<&str>, fallbacks: &[&str]) -> AdvisorSettings {
        AdvisorSettings {
            model: model.map(str::to_owned),
            fallbacks: fallbacks.iter().map(|s| (*s).to_owned()).collect(),
        }
    }

    #[test]
    fn rows_set_and_clear_the_model_and_its_fallbacks() {
        let none = AdvisorSettings::default();
        let model = with_row(none.clone(), 0, Some("openai/gpt-5.5".into()));
        assert_eq!(model, settings(Some("openai/gpt-5.5"), &[]));
        let one = with_row(model, 1, Some("xai/grok-4".into()));
        let two = with_row(one.clone(), 2, Some("google/gemini-3".into()));
        assert_eq!(
            two,
            settings(Some("openai/gpt-5.5"), &["xai/grok-4", "google/gemini-3"])
        );
        assert_eq!(
            with_row(two.clone(), 1, Some("groq/kimi".into())).fallbacks,
            ["groq/kimi", "google/gemini-3"]
        );
        // Removing the first fallback moves the second up.
        assert_eq!(
            with_row(two.clone(), 1, None).fallbacks,
            ["google/gemini-3"]
        );
        assert_eq!(with_row(two, 0, None), none);
        assert_eq!(row_model(&one, 1), Some("xai/grok-4"));
        assert_eq!(row_model(&one, 2), None);
    }

    #[test]
    fn a_fallback_row_shows_once_the_row_above_is_set() {
        assert_eq!(shown_rows(&AdvisorSettings::default()), 1);
        assert_eq!(shown_rows(&settings(Some("openai/gpt-5.5"), &[])), 2);
        assert_eq!(
            shown_rows(&settings(Some("openai/gpt-5.5"), &["xai/grok-4"])),
            3
        );
        assert_eq!(
            shown_rows(&settings(
                Some("openai/gpt-5.5"),
                &["xai/grok-4", "groq/kimi"]
            )),
            3
        );
    }

    #[test]
    fn a_reserved_model_in_a_row_shows_as_blocked() {
        assert!(is_blocked(Some("anthropic/claude-fable-5")));
        assert!(is_blocked(Some("openai/gpt-6-astra-mini")));
        assert!(!is_blocked(Some("anthropic/claude-opus-5")));
        assert!(!is_blocked(None));
    }

    #[test]
    fn a_picked_pi_model_is_its_loop_json_spec() {
        // Zeron lists Pi's models as `provider/id` (harness/src/pi/catalog.rs),
        // which is what the roster takes.
        assert_eq!(
            advisor_settings::spec("openrouter/x-ai/grok-4").as_deref(),
            Some("openrouter/x-ai/grok-4")
        );
        assert_eq!(advisor_settings::spec("default"), None);
    }
}
