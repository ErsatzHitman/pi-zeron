//! Settings → General → Default subagent: the Pi model and thinking level
//! every child an agent starts runs on unless the call names its own
//! (`subagent_start` tasks, dynamic-workflow agents, delegated sessions), in
//! every kind of session, and up to two fallbacks tried in order when that
//! model cannot run a child. Each model is chosen with the composer's picker
//! held to Pi (see [`Pickers::new_for_pi_model`]); the engine keeps them in
//! Pi's `subagents.json`, which those children re-read at each launch.

use gpui::{AnyElement, Context, Entity, Subscription, Task, div, prelude::*, px};
use zeron_engine::registry::TitleSettings;
use zeron_engine::subagent_settings::{
    MAX_FALLBACKS, SubagentFallback, SubagentSettings, THINKING_LEVELS,
};
use zeron_proto::HarnessId;
use zeron_proto::reserved_models;
use zeron_rpc::methods;

use crate::pickers::{Pickers, TitleModelPicked};
use crate::popover::Loadable;
use crate::settings::widgets;
use crate::state::{AppState, EngineHandle};
use crate::theme::Theme;

/// One picker per row: the model, then each fallback.
const ROWS: [&str; 1 + MAX_FALLBACKS] = ["Model", "Fallback 1", "Fallback 2"];

const DESCRIPTION: &str = "The agent sub-agents, workflow agents and delegated sessions run on \
     when the task does not name one, in every session.";

const THINKING_DESCRIPTION: &str = "Their thinking level, unless the task sets one.";

const FALLBACK_DESCRIPTION: &str = "Tried in order when the model above cannot start a child \
     (its account is almost used up) or the child fails before doing any work.";

const BLOCKED: &str = "Blocked: Claude Fable and GPT Astra class models are reserved for \
     sessions you start. Pick another model.";

/// The thinking menu: the delegating session's own level, then Pi's levels.
const MODEL_DEFAULT: &str = "Session's level";

/// A fallback's thinking menu: the default's level, then Pi's levels.
const DEFAULT_LEVEL: &str = "Default's level";

pub struct DefaultSubagentCard {
    state: Entity<AppState>,
    /// The device whose `subagents.json` is shown (`None` = this engine's own).
    device: Option<String>,
    settings: Loadable<SubagentSettings>,
    pickers: Vec<Entity<Pickers>>,
    thinking_select: widgets::SelectState,
    fallback_thinking_selects: [widgets::SelectState; MAX_FALLBACKS],
    error: Option<String>,
    task: Option<Task<()>>,
    _picked: Vec<Subscription>,
    _state: Subscription,
}

impl DefaultSubagentCard {
    pub fn new(state: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        let mut pickers = Vec::new();
        let mut picked = Vec::new();
        for row in 0..ROWS.len() {
            let unset = if row == 0 {
                "Session's own model"
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
                    // Pi's own default row clears the saved model.
                    let model = crate::settings::picked_pi_model(event.0.model.as_deref());
                    this.change(move |settings| set_row(settings, row, model), cx);
                },
            ));
            pickers.push(picker);
        }
        // Load on connection and whenever the selected device changes.
        let state_sub = cx.observe(&state, |this: &mut Self, state, cx| {
            let device = crate::settings::target_device(&state.read(cx));
            if device != this.device {
                this.device = device;
                this.settings = Loadable::Idle;
                this.load(cx);
            } else if matches!(this.settings, Loadable::Idle) {
                this.load(cx);
            }
        });
        let mut card = Self {
            device: crate::settings::target_device(&state.read(cx)),
            state,
            settings: Loadable::Idle,
            pickers,
            thinking_select: widgets::SelectState::default(),
            fallback_thinking_selects: Default::default(),
            error: None,
            task: None,
            _picked: picked,
            _state: state_sub,
        };
        card.load(cx);
        card
    }

    /// Re-read what the device sets: the settings page outlives visits to it,
    /// so what this card loaded when it opened can be well out of date.
    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        if !matches!(self.settings, Loadable::Idle | Loadable::Loading) {
            self.load(cx);
        }
    }

    /// Apply `change` to what the device sets *now* and save that: this card's
    /// copy can be stale (the page outlives visits, another window or device
    /// may have saved since), and writing it back whole would undo whatever
    /// else changed. Each change waits for the one before it, so two quick
    /// picks both land instead of the second cancelling the first.
    fn change(
        &mut self,
        change: impl FnOnce(&mut SubagentSettings) + 'static,
        cx: &mut Context<Self>,
    ) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let device = self.device.clone();
        let previous = self.task.take();
        self.task = Some(cx.spawn(async move |this, cx| {
            if let Some(previous) = previous {
                previous.await;
            }
            let outcome = match get(&engine, device.as_deref()).await {
                Ok(current) => {
                    let mut wanted = current.clone();
                    change(&mut wanted);
                    match set(&engine, wanted, device.as_deref()).await {
                        Ok(settings) => Outcome::Set(settings),
                        Err(error) => Outcome::Refused { error, current },
                    }
                }
                Err(error) => Outcome::Failed(error),
            };
            this.update(cx, |card, cx| {
                if card.device == device {
                    card.settled(outcome, cx);
                }
            })
            .ok();
        }));
        cx.notify();
    }

    /// A finished load or save, applied to the card and its pickers.
    fn settled(&mut self, outcome: Outcome, cx: &mut Context<Self>) {
        match outcome {
            Outcome::Set(settings) => {
                self.show_in_pickers(&settings, cx);
                self.settings = Loadable::Ready(settings);
                self.error = None;
            }
            // The engine's refusal (a reserved model, say) goes to the toast,
            // and the card shows what the device still sets.
            Outcome::Refused { error, current } => {
                self.show_in_pickers(&current, cx);
                self.settings = Loadable::Ready(current);
                self.error = Some(error);
            }
            Outcome::Failed(error) if matches!(self.settings, Loadable::Ready(_)) => {
                if let Some(settings) = self.settings.ready().cloned() {
                    self.show_in_pickers(&settings, cx);
                }
                self.error = Some(error);
            }
            Outcome::Failed(error) => self.settings = Loadable::Error(error),
        }
        cx.notify();
    }

    /// Each picker shows its row of `settings`.
    fn show_in_pickers(&self, settings: &SubagentSettings, cx: &mut Context<Self>) {
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

    /// `GetSubagentSettings`: the selected device's current settings.
    fn load(&mut self, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        if matches!(self.settings, Loadable::Idle) {
            self.settings = Loadable::Loading;
        }
        let device = self.device.clone();
        let previous = self.task.take();
        self.task = Some(cx.spawn(async move |this, cx| {
            if let Some(previous) = previous {
                previous.await;
            }
            let outcome = match get(&engine, device.as_deref()).await {
                Ok(settings) => Outcome::Set(settings),
                Err(error) => Outcome::Failed(error),
            };
            this.update(cx, |card, cx| {
                if card.device == device {
                    card.settled(outcome, cx);
                }
            })
            .ok();
        }));
        cx.notify();
    }
}

/// How a call to the engine ended.
enum Outcome {
    /// A load's reply, or a save the engine accepted: what the device sets.
    Set(SubagentSettings),
    /// A save the engine refused, with what the device set before it.
    Refused {
        error: String,
        current: SubagentSettings,
    },
    /// The call itself failed (no engine, a reply that does not parse).
    Failed(String),
}

/// The device's `subagents.json`, as the engine has it.
async fn get(engine: &EngineHandle, device: Option<&str>) -> Result<SubagentSettings, String> {
    engine
        .client()
        .call(
            methods::GET_SUBAGENT_SETTINGS,
            crate::settings::with_target_device(serde_json::json!({}), device),
        )
        .await
        .map_err(|e| e.to_string())
        .and_then(|v| serde_json::from_value(v).map_err(|e| e.to_string()))
}

/// Save `settings`; the reply is what the device now sets.
async fn set(
    engine: &EngineHandle,
    settings: SubagentSettings,
    device: Option<&str>,
) -> Result<SubagentSettings, String> {
    let params = crate::settings::with_target_device(
        serde_json::to_value(settings).unwrap_or_default(),
        device,
    );
    engine
        .client()
        .call(methods::SET_SUBAGENT_SETTINGS, params)
        .await
        .map_err(|e| e.to_string())
        .and_then(|v| serde_json::from_value(v).map_err(|e| e.to_string()))
}

/// The model a row shows: row 0 the default's, then each fallback's.
fn row_model(settings: &SubagentSettings, row: usize) -> Option<&str> {
    match row {
        0 => settings.model.as_deref(),
        row => settings.fallbacks.get(row - 1).map(|f| f.model.as_str()),
    }
}

/// Set a row's model, or clear it with `None`. Clearing the model clears its
/// fallbacks (children then run on their session's own model); clearing a
/// fallback moves the next one up. A fallback keeps its level when its model
/// changes.
fn set_row(settings: &mut SubagentSettings, row: usize, model: Option<String>) {
    match (row, model) {
        (0, None) => {
            settings.model = None;
            settings.fallbacks.clear();
        }
        (0, Some(model)) => settings.model = Some(model),
        (row, Some(model)) => match settings.fallbacks.get_mut(row - 1) {
            Some(slot) => slot.model = model,
            None => settings.fallbacks.push(SubagentFallback {
                model,
                thinking: None,
            }),
        },
        (row, None) => {
            if row - 1 < settings.fallbacks.len() {
                settings.fallbacks.remove(row - 1);
            }
        }
    }
}

/// Fallback rows on screen: one more than are set, once the model is set (a
/// fallback for the session's own model is never used), at most two.
fn shown_fallbacks(settings: &SubagentSettings) -> usize {
    match settings.model {
        Some(_) => (settings.fallbacks.len() + 1).min(MAX_FALLBACKS),
        None => 0,
    }
}

/// The thinking menu's row for `level`: 0 is the session's own level (or, for
/// a fallback, the default's level).
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
        let title = |label: &str, description: Option<&'static str>, blocked: bool| {
            let mut title = div()
                .flex_1()
                .min_w(px(160.0))
                .child(widgets::row_title(&theme, label.to_owned()));
            if let Some(description) = description {
                title = title.child(widgets::meta_line(
                    &theme,
                    vec![div().child(description).into_any_element()],
                ));
            }
            if blocked {
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
            title
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
                        .child(title(ROWS[0], Some(DESCRIPTION), false))
                        .child(div().text_color(theme.text_muted).child(line)),
                ),
            );
        };
        let reset = |row: usize, cx: &mut Context<Self>| {
            widgets::text_action(&theme, widgets::ActionTone::Quiet, "Reset")
                .id(("default-subagent-reset", row))
                .tab_index(0)
                .role(gpui::Role::Button)
                .aria_label(if row == 0 {
                    "Children use their session's own model"
                } else {
                    "Remove this fallback"
                })
                .focus_visible(|s| s.border_2().border_color(theme.accent))
                .on_click(cx.listener(move |card, _, _, cx| {
                    card.change(move |settings| set_row(settings, row, None), cx)
                }))
        };
        let controls = || {
            div()
                .flex_none()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(4.0))
        };
        let model_row = widgets::card_row(&theme, true)
            .child(title(
                ROWS[0],
                Some(DESCRIPTION),
                is_blocked(settings.model.as_deref()),
            ))
            .child(
                controls()
                    .when(settings.model.is_some(), |el| el.child(reset(0, cx)))
                    .child(self.pickers[0].clone()),
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
            card.change(move |settings| settings.thinking = thinking_at(ix), cx)
        })
        .render(&self.thinking_select, cx);
        let thinking_row = widgets::card_row(&theme, false)
            .child(title("Thinking", Some(THINKING_DESCRIPTION), false))
            .child(thinking);
        let fallback_rows: Vec<AnyElement> = (1..=shown_fallbacks(&settings))
            .map(|row| {
                let fallback = settings.fallbacks.get(row - 1);
                let mut control = controls();
                if let Some(fallback) = fallback {
                    let level = widgets::select(
                        format!("default-subagent-fallback-thinking-{row}"),
                        "Thinking",
                        &theme,
                        move |card: &mut Self| &mut card.fallback_thinking_selects[row - 1],
                    )
                    .options(
                        std::iter::once(DEFAULT_LEVEL)
                            .chain(THINKING_LEVELS)
                            .map(widgets::SelectOption::new),
                        thinking_index(fallback.thinking.as_deref()),
                    )
                    .width(140.0)
                    .on_select(move |card, ix, _, cx| {
                        card.change(
                            move |settings| {
                                if let Some(slot) = settings.fallbacks.get_mut(row - 1) {
                                    slot.thinking = thinking_at(ix);
                                }
                            },
                            cx,
                        )
                    })
                    .render(&self.fallback_thinking_selects[row - 1], cx);
                    control = control.child(reset(row, cx)).child(level);
                }
                widgets::card_row(&theme, false)
                    .child(title(
                        ROWS[row],
                        (row == 1).then_some(FALLBACK_DESCRIPTION),
                        is_blocked(row_model(&settings, row)),
                    ))
                    .child(control.child(self.pickers[row].clone()))
                    .into_any_element()
            })
            .collect();
        widgets::section(
            &theme,
            "Default subagent",
            block
                .child(model_row)
                .child(thinking_row)
                .children(fallback_rows),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with(model: Option<&str>, fallbacks: &[&str]) -> SubagentSettings {
        SubagentSettings {
            model: model.map(str::to_owned),
            thinking: Some("max".into()),
            fallbacks: fallbacks
                .iter()
                .map(|model| SubagentFallback {
                    model: (*model).to_owned(),
                    thinking: None,
                })
                .collect(),
        }
    }

    #[test]
    fn the_thinking_menu_maps_rows_to_levels_and_back() {
        assert_eq!(thinking_index(None), 0);
        assert_eq!(thinking_at(0), None, "row 0 is the session's own level");
        for (ix, level) in THINKING_LEVELS.iter().enumerate() {
            assert_eq!(thinking_index(Some(level)), ix + 1);
            assert_eq!(thinking_at(ix + 1).as_deref(), Some(*level));
        }
        assert_eq!(thinking_index(Some("ludicrous")), 0);
        assert_eq!(thinking_at(99), None);
    }

    #[test]
    fn rows_set_and_clear_the_model_and_its_fallbacks() {
        let mut settings = with(None, &[]);
        set_row(&mut settings, 0, Some("opencode-go/deepseek-flash".into()));
        set_row(&mut settings, 1, Some("openai/gpt-5.5".into()));
        set_row(&mut settings, 2, Some("zai/glm-5".into()));
        assert_eq!(
            settings,
            with(
                Some("opencode-go/deepseek-flash"),
                &["openai/gpt-5.5", "zai/glm-5"]
            )
        );
        // A changed fallback keeps its level.
        settings.fallbacks[0].thinking = Some("low".into());
        set_row(&mut settings, 1, Some("xai/grok-4".into()));
        assert_eq!(settings.fallbacks[0].model, "xai/grok-4");
        assert_eq!(settings.fallbacks[0].thinking.as_deref(), Some("low"));
        // Removing the first fallback moves the second up.
        let mut removed = settings.clone();
        set_row(&mut removed, 1, None);
        assert_eq!(row_model(&removed, 1), Some("zai/glm-5"));
        assert_eq!(row_model(&removed, 2), None);
        // Clearing the model clears its fallbacks but keeps the level.
        set_row(&mut settings, 0, None);
        assert_eq!(settings, with(None, &[]));
    }

    #[test]
    fn a_fallback_row_shows_once_the_row_above_is_set() {
        assert_eq!(shown_fallbacks(&with(None, &[])), 0);
        assert_eq!(shown_fallbacks(&with(Some("a/b"), &[])), 1);
        assert_eq!(shown_fallbacks(&with(Some("a/b"), &["c/d"])), 2);
        assert_eq!(shown_fallbacks(&with(Some("a/b"), &["c/d", "e/f"])), 2);
    }

    #[test]
    fn a_reserved_default_shows_as_blocked() {
        assert!(is_blocked(Some("anthropic/claude-fable-5")));
        assert!(!is_blocked(Some("anthropic/claude-sonnet-5")));
        assert!(!is_blocked(None));
    }
}
