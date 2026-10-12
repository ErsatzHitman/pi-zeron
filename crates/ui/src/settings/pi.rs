//! Settings → General → Pi: which Pi executable this device runs. The engine
//! never searches PATH for Pi (a stray global install is usually an old
//! release), so the path is chosen here, with `PI_EXECUTABLE` as the fallback.

use gpui::{AnyElement, Context, Entity, Subscription, Task, div, prelude::*, px};
use zeron_engine::registry::PiSettings;
use zeron_rpc::methods;

use crate::composer::{ComposerInput, ComposerInputEvent};
use crate::popover::{self, Loadable};
use crate::settings::widgets;
use crate::state::AppState;
use crate::theme::Theme;

pub struct PiSettingsCard {
    state: Entity<AppState>,
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
        // Settings can open before the engine connects: load once it does.
        let state_sub = cx.observe(&state, |this: &mut Self, _, cx| {
            if matches!(this.settings, Loadable::Idle) {
                this.call(None, cx);
            }
        });
        let mut card = Self {
            state,
            settings: Loadable::Idle,
            input,
            saving: false,
            error: None,
            task: None,
            _input: input_sub,
            _state: state_sub,
        };
        card.call(None, cx);
        card
    }

    fn save(&mut self, cx: &mut Context<Self>) {
        let text = self.input.read(cx).text().trim().to_owned();
        self.call(Some((!text.is_empty()).then_some(text)), cx);
    }

    /// `GetPiSettings`, or `SetPiSettings` when saving; either reply is the
    /// device's setting plus a fresh check of the Pi it resolves to.
    fn call(&mut self, save: Option<Option<String>>, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let saving = save.is_some();
        let (method, params) = match save {
            Some(executable) => (
                methods::SET_PI_SETTINGS,
                serde_json::json!({ "executable": executable }),
            ),
            None => (methods::GET_PI_SETTINGS, serde_json::json!({})),
        };
        if matches!(self.settings, Loadable::Idle) {
            self.settings = Loadable::Loading;
        }
        self.saving = saving;
        self.task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(method, params)
                .await
                .map_err(|e| e.to_string())
                .and_then(|v| serde_json::from_value::<PiSettings>(v).map_err(|e| e.to_string()));
            this.update(cx, |card, cx| {
                card.saving = false;
                match result {
                    Ok(settings) => {
                        let text = settings.executable.clone().unwrap_or_default();
                        card.input.update(cx, |input, cx| input.set_text(text, cx));
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
        let save: AnyElement =
            widgets::text_action(&theme, widgets::ActionTone::Outlined, "Save")
                .id("pi-executable-save")
                .tab_index(0)
                .role(gpui::Role::Button)
                .aria_label("Save the Pi executable")
                .focus_visible(|s| s.border_2().border_color(theme.accent))
                .when(self.saving, |el| el.opacity(0.5))
                .on_click(cx.listener(|card, _, _, cx| card.save(cx)))
                .into_any_element();
        widgets::section(
            &theme,
            "Pi",
            widgets::section_card(&theme).mt_0().child(
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
            ),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
