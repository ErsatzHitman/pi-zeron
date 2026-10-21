//! Settings → General → Thread naming: which agent and model title new
//! threads, chosen with the composer's own model picker (title-bound mode).
//! The same model writes recaps, so their switch lives here too.

use gpui::{AnyElement, Context, Entity, Subscription, Task, div, prelude::*, px};
use zeron_engine::registry::TitleSettings;
use zeron_rpc::methods;

use crate::pickers::{Pickers, TitleModelPicked};
use crate::popover::Loadable;
use crate::settings::widgets;
use crate::state::{AppState, EngineHandle};
use crate::theme::Theme;

pub struct ThreadNamingCard {
    state: Entity<AppState>,
    /// The device whose titles are shown (`None` = this engine's own): they
    /// are written by the agent the composer runs.
    device: Option<String>,
    /// The engine `settings` came from: an account switch replaces it.
    engine: Option<EngineHandle>,
    settings: Loadable<TitleSettings>,
    picker: Entity<Pickers>,
    error: Option<String>,
    task: Option<Task<()>>,
    _picked: Subscription,
    _state: Subscription,
}

impl ThreadNamingCard {
    pub fn new(state: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        let picker = {
            let state = state.clone();
            cx.new(|cx| Pickers::new_for_titles(state, TitleSettings::default(), cx))
        };
        let picked = cx.subscribe(
            &picker,
            |this: &mut Self, _, event: &TitleModelPicked, cx| {
                this.call(Some(event.0.clone()), cx);
            },
        );
        // Settings can open before the engine connects: load once it does. A
        // device switch is another `harness-prefs.json`, and a replaced engine
        // (an account switch, a reconnect) another answer: reload through both.
        let state_sub = cx.observe(&state, |this: &mut Self, state, cx| {
            let (device, engine) = {
                let state = state.read(cx);
                (
                    crate::settings::target_device(&state),
                    state.engine().cloned(),
                )
            };
            if device != this.device
                || crate::settings::engine_replaced(this.engine.as_ref(), engine.as_ref())
            {
                this.device = device;
                this.settings = Loadable::Idle;
                this.call(None, cx);
            } else if matches!(this.settings, Loadable::Idle) {
                this.call(None, cx);
            }
        });
        let mut card = Self {
            device: crate::settings::target_device(&state.read(cx)),
            engine: None,
            state,
            settings: Loadable::Idle,
            picker,
            error: None,
            task: None,
            _picked: picked,
            _state: state_sub,
        };
        card.call(None, cx);
        card
    }

    /// Re-read what the device sets: the settings page outlives visits to it,
    /// so what this card loaded when it opened can be well out of date.
    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        if !matches!(self.settings, Loadable::Idle | Loadable::Loading) {
            self.call(None, cx);
        }
    }

    /// `GetTitleSettings`, or `SetTitleSettings` when saving; either reply is
    /// the device's authoritative settings.
    fn call(&mut self, save: Option<TitleSettings>, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        self.engine = Some(engine.clone());
        let (method, params) = match save {
            Some(settings) => (
                methods::SET_TITLE_SETTINGS,
                serde_json::to_value(settings).unwrap_or_default(),
            ),
            None => (methods::GET_TITLE_SETTINGS, serde_json::json!({})),
        };
        let params = crate::settings::with_target_device(params, self.device.as_deref());
        if matches!(self.settings, Loadable::Idle) {
            self.settings = Loadable::Loading;
        }
        let asked = engine.clone();
        self.task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(method, params)
                .await
                .map_err(|e| e.to_string())
                .and_then(|v| {
                    serde_json::from_value::<TitleSettings>(v).map_err(|e| e.to_string())
                });
            this.update(cx, |card, cx| {
                // A replaced engine owns the card now: this is another
                // runtime's answer, error included.
                if !card
                    .engine
                    .as_ref()
                    .is_some_and(|engine| engine.same_connection(&asked))
                {
                    return;
                }
                match result {
                    Ok(settings) => {
                        card.picker.update(cx, |picker, cx| {
                            picker.set_title_settings(settings.clone(), cx)
                        });
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

impl Render for ThreadNamingCard {
    fn render(&mut self, _: &mut gpui::Window, cx: &mut Context<Self>) -> impl IntoElement {
        crate::toast::take_error(&mut self.error, cx);
        let theme = Theme::of(cx).for_settings_surface();
        let follows_session = self
            .settings
            .ready()
            .is_some_and(|settings| settings.harness.is_none());
        let control: AnyElement = match &self.settings {
            Loadable::Ready(_) => div()
                .flex_none()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(4.0))
                .when(!follows_session, |row| {
                    row.child(
                        widgets::text_action(&theme, widgets::ActionTone::Quiet, "Reset")
                            .id("thread-naming-reset")
                            .tab_index(0)
                            .role(gpui::Role::Button)
                            .aria_label("Name threads with the session agent")
                            .focus_visible(|s| s.border_2().border_color(theme.accent))
                            .on_click(cx.listener(|card, _, _, cx| {
                                card.call(Some(TitleSettings::default()), cx)
                            })),
                    )
                })
                .child(self.picker.clone())
                .into_any_element(),
            Loadable::Error(error) => div()
                .text_color(theme.text_muted)
                .child(error.clone())
                .into_any_element(),
            Loadable::Idle | Loadable::Loading => div()
                .text_color(theme.text_muted)
                .child("Loading…")
                .into_any_element(),
        };
        let description = if follows_session {
            "Each thread is named by its own agent. Recaps use the same model."
        } else {
            "A small model keeps titles fast and cheap. Recaps use the same model."
        };
        let recap_enabled = crate::settings::current(cx).recap_enabled;
        widgets::section_card(&theme)
            .child(
                widgets::card_row(&theme, true)
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(160.0))
                            .child(widgets::row_title(&theme, "Thread naming"))
                            .child(widgets::meta_line(
                                &theme,
                                vec![div().child(description).into_any_element()],
                            )),
                    )
                    .child(control),
            )
            .child(
                widgets::card_row(&theme, false)
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(160.0))
                            .child(widgets::row_title(&theme, "Recap when you return"))
                            .child(widgets::meta_line(
                                &theme,
                                vec![
                                    div()
                                        .child(
                                            "A short summary after time away, while the agent \
                                             worked. /recap writes one any time.",
                                        )
                                        .into_any_element(),
                                ],
                            )),
                    )
                    .child(
                        widgets::toggle_switch(&theme, recap_enabled, "recap-enabled")
                            .id("recap-enabled-toggle")
                            .debug_selector(|| "recap-enabled-toggle".into())
                            .tab_index(0)
                            .role(gpui::Role::Switch)
                            .aria_label("Recap when you return")
                            .aria_toggled(if recap_enabled {
                                gpui::Toggled::True
                            } else {
                                gpui::Toggled::False
                            })
                            .focus_visible(|s| s.border_2().border_color(theme.accent))
                            .cursor_pointer()
                            .on_click(cx.listener(move |_, _, _, cx| {
                                crate::settings::update(
                                    crate::settings::SavePolicy::Immediate,
                                    cx,
                                    |settings| settings.recap_enabled = !recap_enabled,
                                );
                                cx.notify();
                            })),
                    ),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pull_request_test_support::{self as fixture, ScriptedRpc};

    /// Titles are written by the agent the composer runs: the card reads the
    /// composer's device, and a switch (another device, or the engine replaced
    /// by an account switch) re-reads that device's answer.
    #[gpui::test]
    fn titles_follow_the_composer_device_and_reload_on_a_switch(cx: &mut gpui::TestAppContext) {
        let runtime = fixture::runtime();
        let _guard = runtime.enter();
        let _settings = fixture::settings(cx, Default::default());
        let asked = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let rpc = ScriptedRpc::new({
            let asked = asked.clone();
            move |method, params| {
                let asked = asked.clone();
                async move {
                    assert_eq!(method, methods::GET_TITLE_SETTINGS);
                    let device = params
                        .get("targetDeviceId")
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_owned);
                    asked.lock().unwrap().push(device.clone());
                    zeron_rpc::RpcReply::value(&TitleSettings {
                        harness: None,
                        model: Some(device.unwrap_or_else(|| "this-device".into())),
                    })
                }
            }
        });
        let state = cx.update(|cx| fixture::state(cx, Some(rpc.client())));
        state.update(cx, |state, _| {
            state.local_device_id = Some("local".into());
            state.selected_device = Some("remote".into());
        });
        let card = cx.new(|cx| ThreadNamingCard::new(state.clone(), cx));
        rpc.settle(cx, &runtime, |cx| {
            card.read_with(cx, |card, _| {
                matches!(&card.settings, Loadable::Ready(settings)
                    if settings.model.as_deref() == Some("remote"))
            })
        });
        // Back to This PC: another `harness-prefs.json`, read without a target.
        state.update(cx, |state, cx| {
            state.selected_device = Some("local".into());
            cx.notify();
        });
        rpc.settle(cx, &runtime, |cx| {
            card.read_with(cx, |card, _| {
                matches!(&card.settings, Loadable::Ready(settings)
                    if settings.model.as_deref() == Some("this-device"))
            })
        });
        assert_eq!(
            asked.lock().unwrap().as_slice(),
            [Some("remote".to_owned()), None]
        );
    }
}
