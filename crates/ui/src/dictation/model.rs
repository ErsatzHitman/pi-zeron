//! Settings → Voice: the speech-to-text provider, its API key, model,
//! language, microphone, filler-word cleanup and the trigger phrase, then
//! voice mode's providers ([`voice_mode`]). Keys live in `stt-keys.json`
//! beside the settings file, never in `ui-settings.json`.
use crate::{
    composer::{ComposerInput, ComposerInputEvent},
    popover,
    settings::{self, widgets},
    theme::Theme,
};
use gpui::{
    AnyElement, App, Context, Entity, Global, Subscription, Task, Window, div, prelude::*, px,
};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};
use zeron_voice::cloud::{Config, Provider};

mod voice_mode;
pub(crate) use voice_mode::call_config;

const KEYS_FILE: &str = "stt-keys.json";
/// Auto-detect, then fixed ISO-639-1 hints.
const LANGUAGES: &[(Option<&str>, &str)] = &[
    (None, "Auto-detect"),
    (Some("en"), "English"),
    (Some("hi"), "Hindi"),
    (Some("es"), "Spanish"),
    (Some("fr"), "French"),
    (Some("de"), "German"),
    (Some("pt"), "Portuguese"),
    (Some("ja"), "Japanese"),
    (Some("zh"), "Chinese"),
];

struct VoiceGlobal {
    card: Entity<VoiceCard>,
}
impl Global for VoiceGlobal {}
pub(crate) fn init(root: PathBuf, cx: &mut App) {
    let card = cx.new(|cx| VoiceCard::new(root.join(KEYS_FILE), cx));
    cx.set_global(VoiceGlobal { card });
}
/// Dictation is on once the selected provider has a key.
pub(crate) fn enabled(cx: &App) -> bool {
    config(cx).is_some()
}
/// What the dictation worker needs, or `None` without a key.
pub(crate) fn config(cx: &App) -> Option<Config> {
    let saved = settings::current(cx);
    let api_key = cx
        .try_global::<VoiceGlobal>()?
        .card
        .read(cx)
        .keys
        .get(saved.stt_provider.key_name())?
        .clone();
    Some(Config {
        provider: saved.stt_provider,
        api_key,
        model: saved.stt_model.clone(),
        language: saved.stt_language.clone(),
        remove_fillers: saved.stt_remove_fillers,
        trigger_phrase: saved.stt_trigger_phrase.clone(),
        live: saved.stt_live,
    })
}
pub(crate) fn card(cx: &mut App) -> Entity<VoiceCard> {
    if !cx.has_global::<VoiceGlobal>() {
        init(std::env::temp_dir().join("zeron-voice-test"), cx);
    }
    cx.global::<VoiceGlobal>().card.clone()
}

fn load_keys(path: &Path) -> BTreeMap<String, String> {
    std::fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<BTreeMap<String, String>>(&bytes).ok())
        .unwrap_or_default()
        .into_iter()
        .filter(|(_, key)| !key.trim().is_empty())
        .collect()
}
fn save_keys(path: &Path, keys: &BTreeMap<String, String>) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let bytes = serde_json::to_vec_pretty(keys)?;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    std::io::Write::write_all(&mut options.open(path)?, &bytes)
}

pub(crate) struct VoiceCard {
    scroll: widgets::PageScroll,
    keys_path: PathBuf,
    keys: BTreeMap<String, String>,
    key_input: Entity<ComposerInput>,
    phrase_input: Entity<ComposerInput>,
    /// Voice mode's OpenRouter key, and its Deepgram or Mistral key.
    openrouter_input: Entity<ComposerInput>,
    voice_key_input: Entity<ComposerInput>,
    /// The real-time model's provider key.
    realtime_key_input: Entity<ComposerInput>,
    /// Why the typed trigger phrase was not saved.
    phrase_problem: Option<&'static str>,
    error: Option<String>,
    inputs: Inputs,
    provider_select: widgets::SelectState,
    model_select: widgets::SelectState,
    language_select: widgets::SelectState,
    input_select: widgets::SelectState,
    trigger_select: widgets::SelectState,
    voice: voice_mode::Selects,
    _subscriptions: Vec<Subscription>,
}
/// Microphones as last enumerated, refreshed off the UI thread while the page
/// is visible so newly connected devices appear.
#[derive(Default)]
struct Inputs {
    devices: Vec<zeron_voice::InputDevice>,
    default: Option<String>,
    checked: Option<Instant>,
    task: Option<Task<()>>,
}
const INPUT_REFRESH: Duration = Duration::from_secs(3);
impl VoiceCard {
    fn new(keys_path: PathBuf, cx: &mut Context<Self>) -> Self {
        let key_input = cx.new(|cx| {
            ComposerInput::new("Paste an API key", cx)
                .with_single_line()
                .with_masked()
        });
        let phrase = settings::current(cx).stt_trigger_phrase;
        let phrase_input = cx.new(|cx| {
            let mut input = ComposerInput::new("Off", cx).with_single_line();
            input.set_text(phrase, cx);
            input
        });
        let key_sub = cx.subscribe(&key_input, |this: &mut Self, _, event, cx| {
            if matches!(event, ComposerInputEvent::Submitted) {
                this.save_key(cx);
            }
        });
        let phrase_sub = cx.subscribe(&phrase_input, |this: &mut Self, _, event, cx| {
            if matches!(event, ComposerInputEvent::Submitted) {
                this.save_phrase(cx);
            }
        });
        let key_field = |cx: &mut Context<Self>| {
            cx.new(|cx| {
                ComposerInput::new("Paste an API key", cx)
                    .with_single_line()
                    .with_masked()
            })
        };
        let openrouter_input = key_field(cx);
        let voice_key_input = key_field(cx);
        let realtime_key_input = key_field(cx);
        let openrouter_sub = cx.subscribe(&openrouter_input, |this: &mut Self, _, event, cx| {
            if matches!(event, ComposerInputEvent::Submitted) {
                this.save_openrouter_key(cx);
            }
        });
        let voice_key_sub = cx.subscribe(&voice_key_input, |this: &mut Self, _, event, cx| {
            if matches!(event, ComposerInputEvent::Submitted) {
                this.save_voice_key(cx);
            }
        });
        let realtime_key_sub =
            cx.subscribe(&realtime_key_input, |this: &mut Self, _, event, cx| {
                if matches!(event, ComposerInputEvent::Submitted) {
                    this.save_realtime_key(cx);
                }
            });
        Self {
            scroll: widgets::PageScroll::default(),
            keys: load_keys(&keys_path),
            keys_path,
            key_input,
            phrase_input,
            openrouter_input,
            voice_key_input,
            realtime_key_input,
            phrase_problem: None,
            error: None,
            inputs: Inputs::default(),
            provider_select: widgets::SelectState::default(),
            model_select: widgets::SelectState::default(),
            language_select: widgets::SelectState::default(),
            input_select: widgets::SelectState::default(),
            trigger_select: widgets::SelectState::default(),
            voice: voice_mode::Selects::default(),
            _subscriptions: vec![
                key_sub,
                phrase_sub,
                openrouter_sub,
                voice_key_sub,
                realtime_key_sub,
            ],
        }
    }

    fn set_key(&mut self, key: Option<String>, cx: &mut Context<Self>) {
        let name = settings::current(cx).stt_provider.key_name();
        let input = self.key_input.clone();
        self.store_key(name, key, &input, cx);
    }

    /// Save or remove the key called `name`, clearing `input` once saved.
    fn store_key(
        &mut self,
        name: &str,
        key: Option<String>,
        input: &Entity<ComposerInput>,
        cx: &mut Context<Self>,
    ) {
        let mut keys = self.keys.clone();
        match key {
            Some(key) => keys.insert(name.to_owned(), key),
            None => keys.remove(name),
        };
        match save_keys(&self.keys_path, &keys) {
            Ok(()) => {
                self.keys = keys;
                input.update(cx, |input, cx| input.set_text("", cx));
            }
            Err(_) => {
                self.error =
                    Some("Could not save the API key. Check folder permissions and retry.".into())
            }
        }
        // The composer shows its microphone and voice orb only while their
        // keys are saved.
        cx.refresh_windows();
        cx.notify();
    }
    fn save_key(&mut self, cx: &mut Context<Self>) {
        let key = self.key_input.read(cx).text().trim().to_owned();
        if !key.is_empty() {
            self.set_key(Some(key), cx);
        }
    }
    fn save_phrase(&mut self, cx: &mut Context<Self>) {
        let phrase = self.phrase_input.read(cx).text().trim().to_owned();
        self.phrase_problem = zeron_voice::trigger_problem(&phrase);
        if self.phrase_problem.is_none() {
            settings::update(settings::SavePolicy::Immediate, cx, |s| {
                s.stt_trigger_phrase = phrase
            });
        }
        cx.notify();
    }
}
impl popover::ScrollRailHost for VoiceCard {
    fn rail_bar(&mut self) -> &mut popover::MenuScrollbarState {
        self.scroll.rail_bar()
    }
    fn rail_scroll(&self) -> Option<gpui::ScrollHandle> {
        self.scroll.rail_scroll()
    }
}
impl VoiceCard {
    fn refresh_inputs(&mut self, cx: &mut Context<Self>) {
        if self.inputs.task.is_some()
            || self
                .inputs
                .checked
                .is_some_and(|at| at.elapsed() < INPUT_REFRESH)
        {
            return;
        }
        let scan = cx.background_executor().spawn(async {
            (
                zeron_voice::input_devices(),
                zeron_voice::default_input_device(),
            )
        });
        self.inputs.task = Some(cx.spawn(async move |this, cx| {
            let (devices, default) = scan.await;
            this.update(cx, |this, cx| {
                let changed = this.inputs.devices != devices || this.inputs.default != default;
                this.inputs.devices = devices;
                this.inputs.default = default;
                this.inputs.checked = Some(Instant::now());
                if changed {
                    cx.notify();
                }
            })
            .ok();
            // A static settings page otherwise has no reason to render again,
            // so checking the age only in render misses hot-plugged devices.
            // Invalidate once when the scan expires. Only a visible card starts
            // the next scan, so leaving Settings does not keep polling devices.
            cx.background_executor().timer(INPUT_REFRESH).await;
            this.update(cx, |this, cx| {
                this.inputs.task = None;
                cx.notify();
            })
            .ok();
        }));
    }

    /// Options for the microphone select and the index of the current one. A
    /// saved device that is unplugged stays listed so the choice is visible;
    /// recording falls back to the system default meanwhile.
    fn input_options(&self, saved: Option<&str>) -> (Vec<widgets::SelectOption>, usize) {
        let default_name = self.inputs.default.as_ref().and_then(|id| {
            self.inputs
                .devices
                .iter()
                .find(|d| &d.id == id)
                .map(|d| d.name.clone())
        });
        let mut options = vec![match default_name {
            Some(name) => widgets::SelectOption::new("System default").detail(name),
            None => widgets::SelectOption::new("System default"),
        }];
        options.extend(
            self.inputs
                .devices
                .iter()
                .map(|d| widgets::SelectOption::new(d.name.clone())),
        );
        let selected = match saved {
            None => 0,
            Some(id) => match self.inputs.devices.iter().position(|d| d.id == id) {
                Some(ix) => ix + 1,
                None => {
                    options.push(widgets::SelectOption::new("Disconnected microphone"));
                    options.len() - 1
                }
            },
        };
        (options, selected)
    }
}

/// A title, an optional detail line, and the control on the right.
fn row(
    theme: &Theme,
    first: bool,
    title: &str,
    meta: Option<String>,
    control: impl IntoElement,
) -> gpui::Div {
    widgets::card_row(theme, first)
        .child(
            div()
                .flex_1()
                .min_w(px(160.0))
                .flex()
                .flex_col()
                .child(widgets::row_title(theme, title.to_owned()))
                .children(
                    meta.map(|meta| widgets::meta_line(theme, vec![meta.into_any_element()])),
                ),
        )
        .child(control)
}

/// A title and detail line above a text field and its action.
fn field_row(
    theme: &Theme,
    first: bool,
    title: &str,
    meta: AnyElement,
    input: &Entity<ComposerInput>,
    actions: Vec<AnyElement>,
) -> gpui::Div {
    widgets::card_row(theme, first)
        .flex_col()
        .items_start()
        .gap(px(8.0))
        .child(
            div()
                .w_full()
                .child(widgets::row_title(theme, title.to_owned()))
                .child(widgets::meta_line(theme, vec![meta])),
        )
        .child(
            div()
                .w_full()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(8.0))
                .child(popover::dialog_field(input.clone().into_any_element()).flex_1())
                .children(actions),
        )
}

impl Render for VoiceCard {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx).for_settings_surface();
        let saved = settings::current(cx);
        let provider = saved.stt_provider;

        let provider_control =
            widgets::select("voice-provider", "Provider", &theme, |card: &mut Self| {
                &mut card.provider_select
            })
            .options(
                Provider::ALL
                    .iter()
                    .map(|p| widgets::SelectOption::new(p.label())),
                Provider::ALL
                    .iter()
                    .position(|p| *p == provider)
                    .unwrap_or(0),
            )
            .width(200.0)
            .on_select(move |_, ix, _, cx| {
                let Some(chosen) = Provider::ALL.get(ix).copied() else {
                    return;
                };
                if chosen != provider {
                    settings::update(settings::SavePolicy::Immediate, cx, |s| {
                        s.stt_provider = chosen;
                        s.stt_model = None;
                    });
                    cx.refresh_windows();
                }
                cx.notify();
            })
            .render(&self.provider_select, cx);

        let has_key = self.keys.contains_key(provider.key_name());
        let save_key = widgets::text_action(&theme, widgets::ActionTone::Outlined, "Save")
            .id("voice-key-save")
            .tab_index(0)
            .role(gpui::Role::Button)
            .aria_label("Save the API key")
            .focus_visible(|s| s.border_2().border_color(theme.accent))
            .on_click(cx.listener(|card, _, _, cx| card.save_key(cx)))
            .into_any_element();
        let remove_key = has_key.then(|| {
            widgets::text_action(&theme, widgets::ActionTone::Quiet, "Remove")
                .id("voice-key-remove")
                .tab_index(0)
                .role(gpui::Role::Button)
                .aria_label("Remove the API key")
                .focus_visible(|s| s.border_2().border_color(theme.accent))
                .on_click(cx.listener(|card, _, _, cx| card.set_key(None, cx)))
                .into_any_element()
        });
        let key_row = field_row(
            &theme,
            false,
            "API key",
            if has_key {
                "Saved on this device. Paste a new key to replace it.".into_any_element()
            } else {
                format!("Add a {} key to turn on dictation.", provider.label()).into_any_element()
            },
            &self.key_input,
            std::iter::once(save_key).chain(remove_key).collect(),
        );

        let models = provider.models();
        let model_control = widgets::select("voice-model", "Model", &theme, |card: &mut Self| {
            &mut card.model_select
        })
        .options(
            models.iter().map(|m| widgets::SelectOption::new(*m)),
            saved
                .stt_model
                .as_deref()
                .and_then(|m| models.iter().position(|known| *known == m))
                .unwrap_or(0),
        )
        .width(200.0)
        .on_select(move |_, ix, _, cx| {
            let model = models.get(ix).map(|m| (*m).to_owned());
            settings::update(settings::SavePolicy::Immediate, cx, |s| s.stt_model = model);
            cx.notify();
        })
        .render(&self.model_select, cx);

        let language_control =
            widgets::select("voice-language", "Language", &theme, |card: &mut Self| {
                &mut card.language_select
            })
            .options(
                LANGUAGES
                    .iter()
                    .map(|(_, name)| widgets::SelectOption::new(*name)),
                LANGUAGES
                    .iter()
                    .position(|(code, _)| *code == saved.stt_language.as_deref())
                    .unwrap_or(0),
            )
            .width(200.0)
            .on_select(move |_, ix, _, cx| {
                let language = LANGUAGES
                    .get(ix)
                    .and_then(|(code, _)| code.map(str::to_owned));
                settings::update(settings::SavePolicy::Immediate, cx, |s| {
                    s.stt_language = language
                });
                cx.notify();
            })
            .render(&self.language_select, cx);

        self.refresh_inputs(cx);
        let (input_options, input_selected) = self.input_options(saved.dictation_input.as_deref());
        let ids: Vec<Option<String>> = std::iter::once(None)
            .chain(self.inputs.devices.iter().map(|d| Some(d.id.clone())))
            .chain(std::iter::once(saved.dictation_input.clone()))
            .collect();
        let microphone_control = widgets::select(
            "voice-microphone",
            "Microphone",
            &theme,
            |card: &mut Self| &mut card.input_select,
        )
        .options(input_options, input_selected)
        .width(200.0)
        .on_select(move |_, ix, _, cx| {
            let input = ids.get(ix).cloned().flatten();
            settings::update(settings::SavePolicy::Immediate, cx, |s| {
                s.dictation_input = input
            });
            cx.notify();
        })
        .render(&self.input_select, cx);

        let trigger_control = widgets::select(
            "voice-trigger",
            "Start dictation",
            &theme,
            |card: &mut Self| &mut card.trigger_select,
        )
        .options(
            ["Hold to talk", "Tap to start and stop"]
                .into_iter()
                .map(widgets::SelectOption::new),
            usize::from(saved.dictation_tap),
        )
        .width(200.0)
        .on_select(move |_, ix, _, cx| {
            settings::update(settings::SavePolicy::Immediate, cx, |s| {
                s.dictation_tap = ix == 1
            });
            cx.notify();
        })
        .render(&self.trigger_select, cx);
        let trigger_meta = if saved.dictation_tap {
            "Click the microphone or press the shortcut to start, and again to stop"
        } else {
            "Hold the microphone or the shortcut while you speak"
        };

        let fillers = saved.stt_remove_fillers;
        fn toggle_fillers(cx: &mut App) {
            settings::update(settings::SavePolicy::Immediate, cx, |s| {
                s.stt_remove_fillers = !s.stt_remove_fillers
            });
        }
        let fillers_switch = widgets::toggle_switch(&theme, fillers, "voice-fillers")
            .id("voice-fillers")
            .cursor_pointer()
            .tab_index(0)
            .role(gpui::Role::Switch)
            .aria_label("Remove filler words")
            .aria_toggled(if fillers {
                gpui::Toggled::True
            } else {
                gpui::Toggled::False
            })
            .focus_visible(|s| s.border_2().border_color(theme.accent).opacity(1.0))
            .on_click(cx.listener(move |_, _, _, cx| {
                toggle_fillers(cx);
                cx.notify();
            }))
            .on_key_down(cx.listener(move |_, event: &gpui::KeyDownEvent, _, cx| {
                if matches!(event.keystroke.key.as_str(), "enter" | "space") {
                    cx.stop_propagation();
                    toggle_fillers(cx);
                    cx.notify();
                }
            }))
            .on_a11y_action(gpui::AccessibleAction::Click, move |_, _, cx| {
                toggle_fillers(cx);
            });

        let live = saved.stt_live;
        fn toggle_live(cx: &mut App) {
            settings::update(settings::SavePolicy::Immediate, cx, |s| {
                s.stt_live = !s.stt_live
            });
        }
        let live_switch = widgets::toggle_switch(&theme, live, "voice-live")
            .id("voice-live")
            .cursor_pointer()
            .tab_index(0)
            .role(gpui::Role::Switch)
            .aria_label("Live transcription")
            .aria_toggled(if live {
                gpui::Toggled::True
            } else {
                gpui::Toggled::False
            })
            .focus_visible(|s| s.border_2().border_color(theme.accent).opacity(1.0))
            .on_click(cx.listener(move |_, _, _, cx| {
                toggle_live(cx);
                cx.notify();
            }))
            .on_key_down(cx.listener(move |_, event: &gpui::KeyDownEvent, _, cx| {
                if matches!(event.keystroke.key.as_str(), "enter" | "space") {
                    cx.stop_propagation();
                    toggle_live(cx);
                    cx.notify();
                }
            }))
            .on_a11y_action(gpui::AccessibleAction::Click, move |_, _, cx| {
                toggle_live(cx);
            });

        let save_phrase = widgets::text_action(&theme, widgets::ActionTone::Outlined, "Save")
            .id("voice-phrase-save")
            .tab_index(0)
            .role(gpui::Role::Button)
            .aria_label("Save the trigger phrase")
            .focus_visible(|s| s.border_2().border_color(theme.accent))
            .on_click(cx.listener(|card, _, _, cx| card.save_phrase(cx)))
            .into_any_element();
        let phrase_meta = match self.phrase_problem {
            Some(problem) => div()
                .text_color(theme.danger)
                .child(problem)
                .into_any_element(),
            None if saved.stt_trigger_phrase.trim().is_empty() => {
                "Off. Type a phrase to rewrite dictations that end with it.".into_any_element()
            }
            None => format!(
                "End a dictation with “{}” to have {} rewrite it",
                saved.stt_trigger_phrase.trim(),
                provider.label()
            )
            .into_any_element(),
        };
        let phrase_row = field_row(
            &theme,
            false,
            "Trigger phrase",
            phrase_meta,
            &self.phrase_input,
            vec![save_phrase],
        );

        let voice_mode = self.render_voice_mode(&theme, &saved, cx);
        crate::toast::take_error(&mut self.error, cx);
        let scrollbar = popover::rail(self, "voice-page-scrollbar", &theme, cx);
        div()
            .id("voice-page-host")
            .relative()
            .size_full()
            .on_hover(cx.listener(|this, hovered: &bool, _, cx| {
                if this.scroll.set_list_hovered(*hovered) {
                    cx.notify();
                }
            }))
            .child(
                crate::edge_fade::edge_faded(
                    16.0,
                    true,
                    true,
                    div()
                        .id("voice-page")
                        .size_full()
                        .overflow_y_scroll()
                        .track_scroll(&self.scroll.scroll)
                        .child(
                            widgets::page_column()
                                .child(widgets::page_header(&theme, "Voice", None))
                                .child(
                                    widgets::section_card(&theme)
                                        .child(row(
                                            &theme,
                                            true,
                                            "Provider",
                                            Some("Transcribes each recording in the cloud".into()),
                                            provider_control,
                                        ))
                                        .child(key_row)
                                        .child(row(&theme, false, "Model", None, model_control))
                                        .child(row(
                                            &theme,
                                            false,
                                            "Language",
                                            None,
                                            language_control,
                                        ))
                                        .child(row(
                                            &theme,
                                            false,
                                            "Microphone",
                                            None,
                                            microphone_control,
                                        ))
                                        .child(row(
                                            &theme,
                                            false,
                                            "Start dictation",
                                            Some(trigger_meta.into()),
                                            trigger_control,
                                        ))
                                        .child(row(
                                            &theme,
                                            false,
                                            "Live transcription",
                                            Some(
                                                "Shows each phrase as you pause; the full \
                                                 transcript replaces it when you stop"
                                                    .into(),
                                            ),
                                            live_switch,
                                        )),
                                )
                                .child(
                                    widgets::section_card(&theme)
                                        .child(row(
                                            &theme,
                                            true,
                                            "Remove filler words",
                                            Some("Drops “um”, “uh” and repeated words".into()),
                                            fillers_switch,
                                        ))
                                        .child(phrase_row),
                                )
                                .child(voice_mode),
                        ),
                )
                .fade_overflow_y(&self.scroll.scroll),
            )
            .children(scrollbar)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[gpui::test]
    fn microphone_choice_survives_disconnection(cx: &mut gpui::TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        cx.update(|cx| settings::init(settings::UiSettings::default(), dir.path(), cx));
        let card = cx.update(card);
        card.update(cx, |card, _| {
            card.inputs.devices = vec![
                zeron_voice::InputDevice {
                    id: "coreaudio:built-in".into(),
                    name: "MacBook Pro Microphone".into(),
                },
                zeron_voice::InputDevice {
                    id: "coreaudio:usb".into(),
                    name: "USB Microphone".into(),
                },
            ];
            let (options, selected) = card.input_options(None);
            assert_eq!((options.len(), selected), (3, 0));
            assert_eq!(card.input_options(Some("coreaudio:usb")).1, 2);
            let (options, selected) = card.input_options(Some("coreaudio:gone"));
            assert_eq!((options.len(), selected), (4, 3));
        });
    }
    #[gpui::test]
    fn dictation_turns_on_with_a_key_for_the_selected_provider(cx: &mut gpui::TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        cx.update(|cx| settings::init(settings::UiSettings::default(), dir.path(), cx));
        cx.update(|cx| assert!(!enabled(cx)));
        let card = cx.update(card);
        card.update(cx, |card, cx| {
            card.key_input
                .update(cx, |input, cx| input.set_text(" sk-test ", cx));
            card.save_key(cx);
        });
        cx.update(|cx| {
            let config = config(cx).unwrap();
            assert_eq!(config.api_key, "sk-test");
            assert_eq!(config.provider, Provider::OpenAi);
            assert!(config.remove_fillers);
            assert_eq!(config.trigger_phrase, zeron_voice::DEFAULT_TRIGGER_PHRASE);
        });
        let keys = dir.path().join(KEYS_FILE);
        assert_eq!(
            load_keys(&keys).get("openai").map(String::as_str),
            Some("sk-test")
        );
        // Another provider needs its own key; the first one is kept.
        cx.update(|cx| {
            settings::update(settings::SavePolicy::Immediate, cx, |s| {
                s.stt_provider = Provider::Groq
            });
            assert!(!enabled(cx));
        });
        card.update(cx, |card, cx| card.set_key(None, cx));
        assert_eq!(load_keys(&keys).len(), 1);
        cx.update(|cx| {
            settings::update(settings::SavePolicy::Immediate, cx, |s| {
                s.stt_provider = Provider::OpenAi
            });
            assert!(enabled(cx));
        });
        card.update(cx, |card, cx| card.set_key(None, cx));
        cx.update(|cx| assert!(!enabled(cx)));
        assert!(load_keys(&keys).is_empty());
    }
    #[gpui::test]
    fn an_unusable_trigger_phrase_is_not_saved(cx: &mut gpui::TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        cx.update(|cx| settings::init(settings::UiSettings::default(), dir.path(), cx));
        let card = cx.update(card);
        card.update(cx, |card, cx| {
            card.phrase_input
                .update(cx, |input, cx| input.set_text("go", cx));
            card.save_phrase(cx);
            assert!(card.phrase_problem.is_some());
            assert_eq!(
                settings::current(cx).stt_trigger_phrase,
                zeron_voice::DEFAULT_TRIGGER_PHRASE
            );
            card.phrase_input
                .update(cx, |input, cx| input.set_text("", cx));
            card.save_phrase(cx);
            assert!(card.phrase_problem.is_none());
            assert_eq!(settings::current(cx).stt_trigger_phrase, "");
        });
    }
}
