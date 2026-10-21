//! Settings → General → Pi: which Pi executable this device runs, and whether
//! its agents may delete. The engine never searches PATH for Pi (a stray
//! global install is usually an old release), so the path is chosen here,
//! with `PI_EXECUTABLE` as the fallback. Block deletes switches the delete
//! guard every Pi chat, sub-chat and subagent loads.

use gpui::{AnyElement, Context, Entity, Subscription, Task, div, prelude::*, px};
use zeron_engine::registry::PiSettings;
use zeron_rpc::methods;

use crate::composer::{ComposerInput, ComposerInputEvent};
use crate::popover::{self, Loadable};
use crate::settings::widgets;
use crate::state::{AppState, EngineHandle};
use crate::theme::Theme;

/// What one engine call does; every reply is the device's settings.
#[derive(Debug, Clone, PartialEq)]
enum Request {
    Get,
    /// A re-read on opening Settings: keeps an executable typed but not saved.
    Refresh,
    Executable(Option<String>),
    BlockDeletes(bool),
    YouShouldKnow(bool),
}

pub struct PiSettingsCard {
    state: Entity<AppState>,
    /// The device whose `harness-prefs.json` (and Pi install) is shown
    /// (`None` = this engine's own).
    device: Option<String>,
    /// The engine `settings` came from: an account switch replaces it.
    engine: Option<EngineHandle>,
    settings: Loadable<PiSettings>,
    input: Entity<ComposerInput>,
    saving: bool,
    error: Option<String>,
    task: Option<Task<()>>,
    _input: Subscription,
    _state: Subscription,
}

impl PiSettingsCard {
    pub fn new(state: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        let input = cx.new(|cx| {
            ComposerInput::new("Path to pi, pi.cmd or a pi-dev.cmd wrapper", cx).with_single_line()
        });
        let input_sub = cx.subscribe(&input, |this: &mut Self, _, event, cx| {
            if matches!(event, ComposerInputEvent::Submitted) {
                this.save(cx);
            }
        });
        // Settings can open before the engine connects: load once it does. A
        // device switch is another Pi, and a replaced engine (an account
        // switch, a reconnect) another answer: reload through both.
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
                this.call(Request::Get, cx);
            } else if matches!(this.settings, Loadable::Idle) {
                this.call(Request::Get, cx);
            }
        });
        let mut card = Self {
            device: crate::settings::target_device(&state.read(cx)),
            engine: None,
            state,
            settings: Loadable::Idle,
            input,
            saving: false,
            error: None,
            task: None,
            _input: input_sub,
            _state: state_sub,
        };
        card.call(Request::Get, cx);
        card
    }

    /// Re-read what the device sets: the settings page outlives visits to it,
    /// so what this card loaded when it opened can be well out of date.
    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        if !self.saving && !matches!(self.settings, Loadable::Idle | Loadable::Loading) {
            self.call(Request::Refresh, cx);
        }
    }

    fn save(&mut self, cx: &mut Context<Self>) {
        let text = self.input.read(cx).text().trim().to_owned();
        self.call(Request::Executable((!text.is_empty()).then_some(text)), cx);
    }

    fn set_block_deletes(&mut self, on: bool, cx: &mut Context<Self>) {
        let forced = matches!(&self.settings, Loadable::Ready(s) if s.delete_guard_forced);
        if !self.saving && !forced {
            self.call(Request::BlockDeletes(on), cx);
        }
    }

    fn set_you_should_know(&mut self, on: bool, cx: &mut Context<Self>) {
        if !self.saving {
            self.call(Request::YouShouldKnow(on), cx);
        }
    }

    /// `GetPiSettings`, `SetPiSettings` saving the executable, or
    /// `SetDeleteGuard`, at the device this card shows; each reply is that
    /// device's settings plus a fresh check of the Pi it resolves to.
    fn call(&mut self, request: Request, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        self.engine = Some(engine.clone());
        let typed = self.input.read(cx).text().trim().to_owned();
        // Switching the guard keeps an executable typed but not saved yet, and
        // so does a re-read of a field holding unsaved text (after a failure
        // there is no saved value to fall back on: the reply fills the field).
        let keep_text = match &request {
            Request::BlockDeletes(_) | Request::YouShouldKnow(_) => true,
            Request::Refresh => matches!(
                &self.settings,
                Loadable::Ready(settings) if settings.executable.as_deref() != Some(typed.as_str())
            ),
            _ => false,
        };
        let (method, params) = match &request {
            Request::Get | Request::Refresh => (methods::GET_PI_SETTINGS, serde_json::json!({})),
            Request::Executable(executable) => (
                methods::SET_PI_SETTINGS,
                serde_json::json!({ "executable": executable }),
            ),
            Request::BlockDeletes(on) => (
                methods::SET_DELETE_GUARD,
                serde_json::json!({ "enabled": on }),
            ),
            Request::YouShouldKnow(on) => (
                methods::SET_YOU_SHOULD_KNOW,
                serde_json::json!({ "enabled": on }),
            ),
        };
        let params = crate::settings::with_target_device(params, self.device.as_deref());
        if matches!(self.settings, Loadable::Idle) {
            self.settings = Loadable::Loading;
        }
        self.saving = !matches!(request, Request::Get | Request::Refresh);
        let asked = engine.clone();
        self.task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(method, params)
                .await
                .map_err(|e| e.to_string())
                .and_then(|v| serde_json::from_value::<PiSettings>(v).map_err(|e| e.to_string()));
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
                card.saving = false;
                match result {
                    Ok(settings) => {
                        if !keep_text {
                            let text = settings.executable.clone().unwrap_or_default();
                            card.input.update(cx, |input, cx| input.set_text(text, cx));
                        }
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

/// The status line under the title: the verified Pi, or why it cannot run.
fn status(settings: &PiSettings) -> (String, bool) {
    match (&settings.version, &settings.resolved, &settings.error) {
        (Some(version), Some(path), _) => {
            let source = if settings.executable.is_some() {
                ""
            } else {
                " (from PI_EXECUTABLE)"
            };
            (format!("Pi {version} · {path}{source}"), false)
        }
        (_, _, Some(error)) => (error.clone(), true),
        _ => ("Checking…".into(), false),
    }
}

impl Render for PiSettingsCard {
    fn render(&mut self, _: &mut gpui::Window, cx: &mut Context<Self>) -> impl IntoElement {
        crate::toast::take_error(&mut self.error, cx);
        let theme = Theme::of(cx).for_settings_surface();
        let (line, failed): (String, bool) = match &self.settings {
            Loadable::Ready(settings) => status(settings),
            Loadable::Error(error) => (error.clone(), true),
            Loadable::Idle | Loadable::Loading => ("Loading…".into(), false),
        };
        let save: AnyElement = widgets::text_action(&theme, widgets::ActionTone::Outlined, "Save")
            .id("pi-executable-save")
            .tab_index(0)
            .role(gpui::Role::Button)
            .aria_label("Save the Pi executable")
            .focus_visible(|s| s.border_2().border_color(theme.accent))
            .when(self.saving, |el| el.opacity(0.5))
            .on_click(cx.listener(|card, _, _, cx| card.save(cx)))
            .into_any_element();
        let block_deletes = self.render_block_deletes(&theme, cx);
        let you_should_know = self.render_you_should_know(&theme, cx);
        widgets::section(
            &theme,
            "Pi",
            widgets::section_card(&theme)
                .mt_0()
                .child(
                    widgets::card_row(&theme, true)
                        .flex_col()
                        .items_start()
                        .gap(px(8.0))
                        .child(
                            div()
                                .w_full()
                                .child(widgets::row_title(&theme, "Executable"))
                                .child(widgets::meta_line(
                                    &theme,
                                    vec![
                                        div()
                                            .when(failed, |el| el.text_color(theme.danger))
                                            .child(line)
                                            .into_any_element(),
                                    ],
                                )),
                        )
                        .child(
                            div()
                                .w_full()
                                .flex()
                                .flex_row()
                                .items_center()
                                .gap(px(8.0))
                                .child(
                                    popover::dialog_field(self.input.clone().into_any_element())
                                        .flex_1()
                                        .font_family(theme.font_mono.clone())
                                        .text_size(crate::typography::ui_rems(13.0)),
                                )
                                .child(save),
                        ),
                )
                .child(block_deletes)
                .child(you_should_know),
        )
    }
}

impl PiSettingsCard {
    /// Block deletes: on until switched off; held on, and the switch
    /// inert, when the engine was started with `ZERON_BLOCK_DELETES`.
    fn render_block_deletes(&self, theme: &Theme, cx: &mut Context<Self>) -> gpui::Div {
        let (on, forced) = match &self.settings {
            Loadable::Ready(settings) => (settings.block_deletes, settings.delete_guard_forced),
            _ => (true, false),
        };
        let meta = block_deletes_meta(forced);
        let inert = forced || self.saving || !matches!(self.settings, Loadable::Ready(_));
        widgets::card_row(theme, false)
            .child(
                div()
                    .flex_1()
                    .min_w(px(160.0))
                    .child(widgets::row_title(theme, "Block deletes"))
                    .child(widgets::meta_line(
                        theme,
                        vec![div().child(meta).into_any_element()],
                    )),
            )
            .child(
                widgets::toggle_switch(theme, on, "pi-block-deletes")
                    .id("pi-block-deletes-toggle")
                    .debug_selector(|| "pi-block-deletes-toggle".into())
                    .tab_index(0)
                    .role(gpui::Role::Switch)
                    .aria_label("Block deletes")
                    .aria_toggled(if on {
                        gpui::Toggled::True
                    } else {
                        gpui::Toggled::False
                    })
                    .focus_visible(|s| s.border_2().border_color(theme.accent))
                    .when(inert, |el| el.opacity(0.5))
                    .when(!inert, |el| el.cursor_pointer())
                    .on_click(cx.listener(move |card, _, _, cx| {
                        card.set_block_deletes(!on, cx);
                    })),
            )
    }
}

impl PiSettingsCard {
    /// You should know: off until switched on.
    fn render_you_should_know(&self, theme: &Theme, cx: &mut Context<Self>) -> gpui::Div {
        let on = matches!(&self.settings, Loadable::Ready(s) if s.you_should_know);
        let inert = self.saving || !matches!(self.settings, Loadable::Ready(_));
        widgets::card_row(theme, false)
            .child(
                div()
                    .flex_1()
                    .min_w(px(160.0))
                    .child(widgets::row_title(theme, "You should know"))
                    .child(widgets::meta_line(
                        theme,
                        vec![div().child(YOU_SHOULD_KNOW_META).into_any_element()],
                    )),
            )
            .child(
                widgets::toggle_switch(theme, on, "pi-you-should-know")
                    .id("pi-you-should-know-toggle")
                    .debug_selector(|| "pi-you-should-know-toggle".into())
                    .tab_index(0)
                    .role(gpui::Role::Switch)
                    .aria_label("You should know")
                    .aria_toggled(if on {
                        gpui::Toggled::True
                    } else {
                        gpui::Toggled::False
                    })
                    .focus_visible(|s| s.border_2().border_color(theme.accent))
                    .when(inert, |el| el.opacity(0.5))
                    .when(!inert, |el| el.cursor_pointer())
                    .on_click(cx.listener(move |card, _, _, cx| {
                        card.set_you_should_know(!on, cx);
                    })),
            )
    }
}

const YOU_SHOULD_KNOW_META: &str = "Every 6 steps of a long turn, a reviewer reads the chat and \
     notes a decision the agent made without saying so, or something you should know. \
     Each review is an extra model call.";

fn block_deletes_meta(forced: bool) -> &'static str {
    if forced {
        "Forced on by ZERON_BLOCK_DELETES on this engine."
    } else {
        "Stops rm, del, Remove-Item, git clean, delete tools and similar in every Pi chat, \
         sub-chat and subagent. A guard against mistakes, not a sandbox."
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pull_request_test_support::{self as fixture, ScriptedRpc};

    /// An account switch (here: another engine on the same device) must load
    /// this engine's settings: what the old one said, its error included, is
    /// not the card's to keep.
    #[gpui::test]
    fn settings_reload_when_the_engine_is_replaced(cx: &mut gpui::TestAppContext) {
        let runtime = fixture::runtime();
        let _guard = runtime.enter();
        let _settings = fixture::settings(cx, Default::default());
        let failing = ScriptedRpc::new(|method, _| async move {
            assert_eq!(method, methods::GET_PI_SETTINGS);
            Err(zeron_rpc::RpcError::Failed("no engine".into()))
        });
        let working = ScriptedRpc::new(|method, _| async move {
            assert_eq!(method, methods::GET_PI_SETTINGS);
            zeron_rpc::RpcReply::value(&PiSettings {
                executable: Some("C:/pi/pi.cmd".into()),
                resolved: Some("C:/pi/pi.cmd".into()),
                version: Some("1.1.0".into()),
                ..Default::default()
            })
        });
        let state = cx.update(|cx| fixture::state(cx, Some(failing.client())));
        let card = cx.new(|cx| PiSettingsCard::new(state.clone(), cx));
        failing.settle(cx, &runtime, |cx| {
            card.read_with(cx, |card, _| matches!(card.settings, Loadable::Error(_)))
        });
        let working_engine = crate::state::EngineHandle::from_test_client(working.client());
        state.update(cx, |state, cx| {
            state.prepare_runtime_replacement(cx);
            state.set_test_engine(working_engine.clone());
            cx.notify();
        });
        working.settle(cx, &runtime, |cx| {
            card.read_with(cx, |card, _| {
                matches!(&card.settings, Loadable::Ready(settings)
                    if settings.executable.as_deref() == Some("C:/pi/pi.cmd"))
            })
        });
        card.read_with(cx, |card, cx| {
            assert_eq!(card.input.read(cx).text(), "C:/pi/pi.cmd");
            assert!(
                card.engine
                    .as_ref()
                    .is_some_and(|engine| engine.same_connection(&working_engine)),
                "the card answers to the engine that replaced the old one"
            );
        });
    }

    /// Opening Settings re-reads the device, but keeps a path typed and not
    /// saved yet.
    #[gpui::test]
    fn refresh_keeps_a_typed_executable(cx: &mut gpui::TestAppContext) {
        let runtime = fixture::runtime();
        let _guard = runtime.enter();
        let _settings = fixture::settings(cx, Default::default());
        let rpc = ScriptedRpc::new(|method, _| async move {
            assert_eq!(method, methods::GET_PI_SETTINGS);
            zeron_rpc::RpcReply::value(&PiSettings {
                executable: Some("C:/saved/pi.cmd".into()),
                ..Default::default()
            })
        });
        let state = cx.update(|cx| fixture::state(cx, Some(rpc.client())));
        let card = cx.new(|cx| PiSettingsCard::new(state.clone(), cx));
        rpc.settle(cx, &runtime, |cx| {
            card.read_with(cx, |card, _| matches!(card.settings, Loadable::Ready(_)))
        });
        card.update(cx, |card, cx| {
            card.input
                .update(cx, |input, cx| input.set_text("C:/typed/pi.cmd", cx));
            card.refresh(cx);
        });
        rpc.settle(cx, &runtime, |_| rpc.completed() >= 2);
        card.read_with(cx, |card, cx| {
            assert_eq!(card.input.read(cx).text(), "C:/typed/pi.cmd");
        });
    }

    #[test]
    fn status_names_the_verified_pi_or_the_failure() {
        let ok = PiSettings {
            executable: Some("C:/pi-dev.cmd".into()),
            resolved: Some("C:/pi-dev.cmd".into()),
            version: Some("1.1.0".into()),
            ..Default::default()
        };
        assert_eq!(status(&ok), ("Pi 1.1.0 · C:/pi-dev.cmd".into(), false));
        let env = PiSettings {
            executable: None,
            ..ok.clone()
        };
        assert!(status(&env).0.ends_with("(from PI_EXECUTABLE)"));
        let old = PiSettings {
            error: Some("Pi 0.87.1 at pi.cmd is too old: Zeron needs Pi 1.1.0 or newer".into()),
            ..Default::default()
        };
        assert_eq!(status(&old).1, true);
    }

    #[test]
    fn block_deletes_is_on_unless_switched_off() {
        assert!(PiSettings::default().block_deletes);
        // An engine predating the setting replies without it: on.
        let older: PiSettings = serde_json::from_value(serde_json::json!({
            "executable": "pi"
        }))
        .unwrap();
        assert!(older.block_deletes && !older.delete_guard_forced);
        assert!(block_deletes_meta(true).contains("ZERON_BLOCK_DELETES"));
        assert!(block_deletes_meta(false).contains("not a sandbox"));
    }
}
