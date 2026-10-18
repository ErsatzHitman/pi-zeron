//! Settings → General → Default subagent: the Pi model and thinking level
//! every child an agent starts runs on unless the call names its own
//! (`subagent_start` tasks, dynamic-workflow agents, delegated sessions), in
//! every kind of session. The model is chosen with the composer's picker held
//! to Pi (see [`Pickers::new_for_pi_model`]); the engine keeps both in Pi's
//! `subagents.json`, which those children re-read at each launch.

use gpui::{Context, Entity, Subscription, Task, div, prelude::*, px};
use zeron_engine::advisor_settings;
use zeron_engine::registry::TitleSettings;
use zeron_engine::subagent_settings::{SubagentSettings, THINKING_LEVELS};
use zeron_proto::HarnessId;
use zeron_proto::reserved_models;
use zeron_rpc::methods;

use crate::pickers::{Pickers, TitleModelPicked};
use crate::popover::Loadable;
use crate::settings::widgets;
use crate::state::AppState;
use crate::theme::Theme;

const DESCRIPTION: &str = "The agent sub-agents, workflow agents and delegated sessions run on \
     when the task does not name one, in every session.";

const THINKING_DESCRIPTION: &str = "Their thinking level, unless the task sets one.";

const BLOCKED: &str = "Blocked: Claude Fable and GPT Astra class models are reserved for \
     sessions you start. Pick another model.";

/// The thinking menu: the model's own default, then Pi's levels.
const MODEL_DEFAULT: &str = "Model default";

pub struct DefaultSubagentCard {
    state: Entity<AppState>,
    settings: Loadable<SubagentSettings>,
    picker: Entity<Pickers>,
    thinking_select: widgets::SelectState,
    error: Option<String>,
    task: Option<Task<()>>,
    _picked: Subscription,
    _state: Subscription,
}

impl DefaultSubagentCard {
    pub fn new(state: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        let picker = {
            let state = state.clone();
            cx.new(|cx| Pickers::new_for_pi_model(state, None, "Session's own model", cx))
        };
        let picked = cx.subscribe(
            &picker,
            |this: &mut Self, _, event: &TitleModelPicked, cx| {
                // Pi's catalog ids are already `provider/id` specs.
                if let Some(model) = event.0.model.as_deref().and_then(advisor_settings::spec) {
                    this.change(|settings| settings.model = Some(model), cx);
                }
            },
        );
        // Settings can open before the engine connects: load once it does.
        let state_sub = cx.observe(&state, |this: &mut Self, _, cx| {
            if matches!(this.settings, Loadable::Idle) {
                this.call(None, cx);
            }
        });
        let mut card = Self {
            state,
            settings: Loadable::Idle,
            picker,
            thinking_select: widgets::SelectState::default(),
            error: None,
            task: None,
            _picked: picked,
            _state: state_sub,
        };
        card.call(None, cx);
        card
    }

    fn change(&mut self, change: impl FnOnce(&mut SubagentSettings), cx: &mut Context<Self>) {
        if let Some(mut settings) = self.settings.ready().cloned() {
            change(&mut settings);
            self.call(Some(settings), cx);
        }
    }

    fn show_in_picker(&self, settings: &SubagentSettings, cx: &mut Context<Self>) {
        let model = settings.model.clone();
        self.picker.update(cx, |picker, cx| {
            picker.set_title_settings(
                TitleSettings {
                    harness: Some(HarnessId::Pi),
                    model,
                },
                cx,
            )
        });
    }

    /// `GetSubagentSettings`, or `SetSubagentSettings` when saving; either
    /// reply is what the device's `subagents.json` now sets.
    fn call(&mut self, save: Option<SubagentSettings>, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let (method, params) = match save {
            Some(settings) => (
                methods::SET_SUBAGENT_SETTINGS,
                serde_json::to_value(settings).unwrap_or_default(),
            ),
            None => (methods::GET_SUBAGENT_SETTINGS, serde_json::json!({})),
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
                    serde_json::from_value::<SubagentSettings>(v).map_err(|e| e.to_string())
                });
            this.update(cx, |card, cx| {
                match result {
                    Ok(settings) => {
                        card.show_in_picker(&settings, cx);
                        card.settings = Loadable::Ready(settings);
                        card.error = None;
                    }
                    // The engine's refusal (a reserved model, say) goes to
                    // the toast, and the picker goes back to what is saved.
                    Err(error) if matches!(card.settings, Loadable::Ready(_)) => {
                        if let Some(settings) = card.settings.ready().cloned() {
                            card.show_in_picker(&settings, cx);
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

/// The thinking menu's row for `level`: 0 is the model's default.
fn thinking_index(level: Option<&str>) -> usize {
    level
        .and_then(|level| THINKING_LEVELS.iter().position(|l| *l == level))
        .map_or(0, |ix| ix + 1)
}

/// The level a thinking menu row stands for.
fn thinking_at(ix: usize) -> Option<String> {
    ix.checked_sub(1)
        .and_then(|ix| THINKING_LEVELS.get(ix))
        .map(|level| (*level).to_owned())
}

fn is_blocked(model: Option<&str>) -> bool {
    model.is_some_and(|m| reserved_models::reserved_family(m).is_some())
}

impl Render for DefaultSubagentCard {
    fn render(&mut self, _: &mut gpui::Window, cx: &mut Context<Self>) -> impl IntoElement {
        crate::toast::take_error(&mut self.error, cx);
        let theme = Theme::of(cx).for_settings_surface();
        let block = widgets::section_card(&theme).mt_0();
        let title = |label: &str, description: &'static str| {
            div()
                .flex_1()
                .min_w(px(160.0))
                .child(widgets::row_title(&theme, label.to_owned()))
                .child(widgets::meta_line(
                    &theme,
                    vec![div().child(description).into_any_element()],
                ))
        };
        let Some(settings) = self.settings.ready().cloned() else {
            let line = match &self.settings {
                Loadable::Error(error) => error.clone(),
                _ => "Loading…".into(),
            };
            return widgets::section(
                &theme,
                "Default subagent",
                block.child(
                    widgets::card_row(&theme, true)
                        .child(title("Model", DESCRIPTION))
                        .child(div().text_color(theme.text_muted).child(line)),
                ),
            );
        };
        let reset = widgets::text_action(&theme, widgets::ActionTone::Quiet, "Reset")
            .id("default-subagent-reset")
            .tab_index(0)
            .role(gpui::Role::Button)
            .aria_label("Children use their session's own model")
            .focus_visible(|s| s.border_2().border_color(theme.accent))
            .on_click(
                cx.listener(|card, _, _, cx| card.change(|settings| settings.model = None, cx)),
            );
        let mut model_title = title("Model", DESCRIPTION);
        if is_blocked(settings.model.as_deref()) {
            model_title = model_title.child(widgets::meta_line(
                &theme,
                vec![
                    div()
                        .text_color(theme.danger)
                        .child(BLOCKED)
                        .into_any_element(),
                ],
            ));
        }
        let model_row = widgets::card_row(&theme, true).child(model_title).child(
            div()
                .flex_none()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(4.0))
                .when(settings.model.is_some(), |el| el.child(reset))
                .child(self.picker.clone()),
        );
        let thinking = widgets::select(
            "default-subagent-thinking",
            "Thinking",
            &theme,
            |card: &mut Self| &mut card.thinking_select,
        )
        .options(
            std::iter::once(MODEL_DEFAULT)
                .chain(THINKING_LEVELS)
                .map(widgets::SelectOption::new),
            thinking_index(settings.thinking.as_deref()),
        )
        .width(150.0)
        .on_select(|card, ix, _, cx| {
            card.change(|settings| settings.thinking = thinking_at(ix), cx)
        })
        .render(&self.thinking_select, cx);
        let thinking_row = widgets::card_row(&theme, false)
            .child(title("Thinking", THINKING_DESCRIPTION))
            .child(thinking);
        widgets::section(
            &theme,
            "Default subagent",
            block.child(model_row).child(thinking_row),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_thinking_menu_maps_rows_to_levels_and_back() {
        assert_eq!(thinking_index(None), 0);
        assert_eq!(thinking_at(0), None, "row 0 is the model's default");
        for (ix, level) in THINKING_LEVELS.iter().enumerate() {
            assert_eq!(thinking_index(Some(level)), ix + 1);
            assert_eq!(thinking_at(ix + 1).as_deref(), Some(*level));
        }
        assert_eq!(thinking_index(Some("ludicrous")), 0);
        assert_eq!(thinking_at(99), None);
    }

    #[test]
    fn a_reserved_default_shows_as_blocked() {
        assert!(is_blocked(Some("anthropic/claude-fable-5")));
        assert!(!is_blocked(Some("anthropic/claude-sonnet-5")));
        assert!(!is_blocked(None));
    }
}
