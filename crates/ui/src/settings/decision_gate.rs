//! Settings → General → Decision gate: the decision model Pi's decisions
//! gate asks before a plan with a writer launches — TypeSafe's Jev (through
//! OpenRouter) or SubQ Snap (on SubQ's own API, with a SubQ key from
//! Settings → API keys). The engine keeps the choice in Pi's
//! `decisions-model.json`, which the gate re-reads at every consult.

use gpui::{Context, Entity, Subscription, Task, div, prelude::*, px};
use zeron_engine::decision_settings::{DecisionModel, DecisionSettings};
use zeron_rpc::methods;

use crate::popover::Loadable;
use crate::settings::widgets;
use crate::state::AppState;
use crate::theme::Theme;

const DESCRIPTION: &str = "The decision model Pi asks how to split work before it starts \
     sub-agents that write. Both answer the same questions; SubQ Snap reads far more context.";

const NO_SUBQ_KEY: &str = "Add a SubQ key in Settings → API keys: the gate cannot ask SubQ \
     Snap without one, and lets launches through unchecked.";

pub struct DecisionGateCard {
    state: Entity<AppState>,
    settings: Loadable<DecisionSettings>,
    model_select: widgets::SelectState,
    error: Option<String>,
    task: Option<Task<()>>,
    _state: Subscription,
}

impl DecisionGateCard {
    pub fn new(state: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        // Settings can open before the engine connects: load once it does.
        let state_sub = cx.observe(&state, |this: &mut Self, _, cx| {
            if matches!(this.settings, Loadable::Idle) {
                this.call(None, cx);
            }
        });
        let mut card = Self {
            state,
            settings: Loadable::Idle,
            model_select: widgets::SelectState::default(),
            error: None,
            task: None,
            _state: state_sub,
        };
        card.call(None, cx);
        card
    }

    /// `GetDecisionSettings`, or `SetDecisionSettings` when saving; either
    /// reply is what the device's `decisions-model.json` now sets.
    fn call(&mut self, save: Option<DecisionModel>, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let (method, params) = match save {
            Some(model) => (
                methods::SET_DECISION_SETTINGS,
                serde_json::json!({ "model": model }),
            ),
            None => (methods::GET_DECISION_SETTINGS, serde_json::json!({})),
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
                .and_then(|v| reply(v).map_err(|e| e.to_string()));
            this.update(cx, |card, cx| {
                match result {
                    Ok(settings) => {
                        card.settings = Loadable::Ready(settings);
                        card.error = None;
                    }
                    Err(error) if matches!(card.settings, Loadable::Ready(_)) => {
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

/// A reply, `hasKey` included (it is never read from a request).
fn reply(value: serde_json::Value) -> serde_json::Result<DecisionSettings> {
    let has_key = value
        .get("hasKey")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    let mut settings: DecisionSettings = serde_json::from_value(value)?;
    settings.has_key = has_key;
    Ok(settings)
}

fn model_index(model: DecisionModel) -> usize {
    DecisionModel::ALL
        .iter()
        .position(|m| *m == model)
        .unwrap_or(0)
}

/// Snap with no SubQ key in the store: the gate has nothing to send.
fn missing_key(settings: &DecisionSettings) -> bool {
    settings.model == DecisionModel::Snap && !settings.has_key
}

impl Render for DecisionGateCard {
    fn render(&mut self, _: &mut gpui::Window, cx: &mut Context<Self>) -> impl IntoElement {
        crate::toast::take_error(&mut self.error, cx);
        let theme = Theme::of(cx).for_settings_surface();
        let block = widgets::section_card(&theme).mt_0();
        let mut title = div()
            .flex_1()
            .min_w(px(160.0))
            .child(widgets::row_title(&theme, "Model"))
            .child(widgets::meta_line(
                &theme,
                vec![div().child(DESCRIPTION).into_any_element()],
            ));
        let Some(settings) = self.settings.ready().cloned() else {
            let line = match &self.settings {
                Loadable::Error(error) => error.clone(),
                _ => "Loading…".into(),
            };
            return widgets::section(
                &theme,
                "Decision gate",
                block.child(
                    widgets::card_row(&theme, true)
                        .child(title)
                        .child(div().text_color(theme.text_muted).child(line)),
                ),
            );
        };
        if missing_key(&settings) {
            title = title.child(widgets::meta_line(
                &theme,
                vec![
                    div()
                        .text_color(theme.danger)
                        .child(NO_SUBQ_KEY)
                        .into_any_element(),
                ],
            ));
        }
        let select = widgets::select(
            "decision-gate-model",
            "Decision model",
            &theme,
            |card: &mut Self| &mut card.model_select,
        )
        .options(
            DecisionModel::ALL
                .iter()
                .map(|model| widgets::SelectOption::new(model.label())),
            model_index(settings.model),
        )
        .width(170.0)
        .on_select(|card, ix, _, cx| {
            if let Some(model) = DecisionModel::ALL.get(ix).copied() {
                card.call(Some(model), cx);
            }
        })
        .render(&self.model_select, cx);
        widgets::section(
            &theme,
            "Decision gate",
            block.child(widgets::card_row(&theme, true).child(title).child(select)),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_reply_reads_its_model_and_whether_the_key_is_there() {
        let snap = reply(serde_json::json!({ "model": "snap", "hasKey": false })).unwrap();
        assert_eq!(snap.model, DecisionModel::Snap);
        assert!(missing_key(&snap));
        let keyed = reply(serde_json::json!({ "model": "snap", "hasKey": true })).unwrap();
        assert!(!missing_key(&keyed));
        // Jev's key may be Pi's own OpenRouter login: never flagged.
        let jev = reply(serde_json::json!({ "model": "jev", "hasKey": false })).unwrap();
        assert!(!missing_key(&jev));
        assert_eq!(model_index(DecisionModel::Snap), 1);
        assert_eq!(model_index(DecisionModel::Jev), 0);
    }
}
