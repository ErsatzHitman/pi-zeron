//! Settings → Voice → Voice mode: who transcribes the user (OpenRouter,
//! Deepgram or Mistral), which OpenRouter voice reads the agent's messages,
//! when a pause ends a prompt, and whether talking over the voice stops it.
use super::*;
use zeron_voice::call;

/// Pauses that end a turn, in milliseconds.
const PAUSES: &[(u32, &str)] = &[
    (500, "Short · 0.5 s"),
    (800, "Normal · 0.8 s"),
    (1200, "Long · 1.2 s"),
    (2000, "Very long · 2 s"),
];

#[derive(Default)]
pub(super) struct Selects {
    stt: widgets::SelectState,
    stt_model: widgets::SelectState,
    speech: widgets::SelectState,
    voice: widgets::SelectState,
    pause: widgets::SelectState,
}

/// What a voice call needs from Settings → Voice, or what to add first.
pub(crate) fn call_config(cx: &App) -> Result<call::Config, String> {
    let saved = settings::current(cx);
    let keys = &cx
        .try_global::<VoiceGlobal>()
        .ok_or_else(|| "Voice settings are unavailable.".to_owned())?
        .card
        .read(cx)
        .keys;
    let key = |name: &str| keys.get(name).cloned().unwrap_or_default();
    let openrouter_key = key(call::OPENROUTER_KEY);
    if openrouter_key.trim().is_empty() {
        return Err("Add an OpenRouter API key in Settings → Voice.".into());
    }
    let provider = saved.voice_stt_provider;
    let stt_key = key(provider.key_name());
    if stt_key.trim().is_empty() {
        return Err(format!(
            "Add a {} API key in Settings → Voice.",
            provider.label()
        ));
    }
    let speech = call::speech_model(saved.voice_speech_model.as_deref());
    let voice = saved
        .voice_name
        .as_deref()
        .filter(|v| speech.voices.contains(v))
        .unwrap_or(speech.voices[0])
        .to_owned();
    Ok(call::Config {
        stt: call::Stt {
            provider,
            api_key: stt_key,
            model: saved.voice_stt_model.clone(),
            language: saved.stt_language.clone(),
        },
        openrouter_key,
        speech_model: speech.id.to_owned(),
        voice,
        barge_in: saved.voice_barge_in,
        end_silence: Duration::from_millis(u64::from(saved.voice_pause_ms.clamp(300, 3000))),
        input: saved.dictation_input.clone(),
    })
}

impl VoiceCard {
    pub(super) fn save_openrouter_key(&mut self, cx: &mut Context<Self>) {
        let key = self.openrouter_input.read(cx).text().trim().to_owned();
        if !key.is_empty() {
            let input = self.openrouter_input.clone();
            self.store_key(call::OPENROUTER_KEY, Some(key), &input, cx);
        }
    }

    /// The Deepgram or Mistral key, for whichever transcribes voice mode.
    pub(super) fn save_voice_key(&mut self, cx: &mut Context<Self>) {
        let name = settings::current(cx).voice_stt_provider.key_name();
        let key = self.voice_key_input.read(cx).text().trim().to_owned();
        if !key.is_empty() && name != call::OPENROUTER_KEY {
            let input = self.voice_key_input.clone();
            self.store_key(name, Some(key), &input, cx);
        }
    }

    fn key_actions(
        &self,
        theme: &Theme,
        (save_id, remove_id): (&'static str, &'static str),
        name: &'static str,
        save: fn(&mut Self, &mut Context<Self>),
        input: Entity<ComposerInput>,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let save = widgets::text_action(theme, widgets::ActionTone::Outlined, "Save")
            .id(save_id)
            .tab_index(0)
            .role(gpui::Role::Button)
            .aria_label("Save the API key")
            .focus_visible(|s| s.border_2().border_color(theme.accent))
            .on_click(cx.listener(move |card, _, _, cx| save(card, cx)))
            .into_any_element();
        let remove = self.keys.contains_key(name).then(|| {
            widgets::text_action(theme, widgets::ActionTone::Quiet, "Remove")
                .id(remove_id)
                .tab_index(0)
                .role(gpui::Role::Button)
                .aria_label("Remove the API key")
                .focus_visible(|s| s.border_2().border_color(theme.accent))
                .on_click(cx.listener(move |card, _, _, cx| card.store_key(name, None, &input, cx)))
                .into_any_element()
        });
        std::iter::once(save).chain(remove).collect()
    }

    pub(super) fn render_voice_mode(
        &mut self,
        theme: &Theme,
        saved: &settings::UiSettings,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        let provider = saved.voice_stt_provider;
        let has_openrouter = self.keys.contains_key(call::OPENROUTER_KEY);
        let openrouter_actions = self.key_actions(
            theme,
            ("voice-mode-openrouter-save", "voice-mode-openrouter-remove"),
            call::OPENROUTER_KEY,
            Self::save_openrouter_key,
            self.openrouter_input.clone(),
            cx,
        );
        let openrouter_row = field_row(
            theme,
            true,
            "OpenRouter API key",
            if has_openrouter {
                "Saved on this device. The voice uses it.".into_any_element()
            } else {
                "Add an OpenRouter key to turn on voice mode.".into_any_element()
            },
            &self.openrouter_input,
            openrouter_actions,
        );

        let stt_control = widgets::select(
            "voice-mode-stt",
            "Speech to text",
            theme,
            |card: &mut Self| &mut card.voice.stt,
        )
        .options(
            call::SttProvider::ALL
                .iter()
                .map(|p| widgets::SelectOption::new(p.label())),
            call::SttProvider::ALL
                .iter()
                .position(|p| *p == provider)
                .unwrap_or(0),
        )
        .width(200.0)
        .on_select(move |_, ix, _, cx| {
            let Some(chosen) = call::SttProvider::ALL.get(ix).copied() else {
                return;
            };
            if chosen != provider {
                settings::update(settings::SavePolicy::Immediate, cx, |s| {
                    s.voice_stt_provider = chosen;
                    s.voice_stt_model = None;
                });
                cx.refresh_windows();
            }
            cx.notify();
        })
        .render(&self.voice.stt, cx);

        let stt_key_row = (provider != call::SttProvider::OpenRouter).then(|| {
            let name = provider.key_name();
            let actions = self.key_actions(
                theme,
                ("voice-mode-stt-key-save", "voice-mode-stt-key-remove"),
                name,
                Self::save_voice_key,
                self.voice_key_input.clone(),
                cx,
            );
            field_row(
                theme,
                false,
                &format!("{} API key", provider.label()),
                if self.keys.contains_key(name) {
                    "Saved on this device. Paste a new key to replace it.".into_any_element()
                } else {
                    format!("Add a {} key to transcribe with it.", provider.label())
                        .into_any_element()
                },
                &self.voice_key_input,
                actions,
            )
        });

        let stt_models = provider.models();
        let stt_model_control = widgets::select(
            "voice-mode-stt-model",
            "Transcription model",
            theme,
            |card: &mut Self| &mut card.voice.stt_model,
        )
        .options(
            stt_models.iter().map(|m| widgets::SelectOption::new(*m)),
            saved
                .voice_stt_model
                .as_deref()
                .and_then(|m| stt_models.iter().position(|known| *known == m))
                .unwrap_or(0),
        )
        .width(260.0)
        .on_select(move |_, ix, _, cx| {
            let model = stt_models.get(ix).map(|m| (*m).to_owned());
            settings::update(settings::SavePolicy::Immediate, cx, |s| {
                s.voice_stt_model = model
            });
            cx.notify();
        })
        .render(&self.voice.stt_model, cx);

        let speech = call::speech_model(saved.voice_speech_model.as_deref());
        let speech_control = widgets::select(
            "voice-mode-speech",
            "Voice model",
            theme,
            |card: &mut Self| &mut card.voice.speech,
        )
        .options(
            call::SPEECH_MODELS
                .iter()
                .map(|m| widgets::SelectOption::new(m.label).detail(m.id)),
            call::SPEECH_MODELS
                .iter()
                .position(|m| m.id == speech.id)
                .unwrap_or(0),
        )
        .width(260.0)
        .on_select(move |_, ix, _, cx| {
            let Some(model) = call::SPEECH_MODELS.get(ix) else {
                return;
            };
            settings::update(settings::SavePolicy::Immediate, cx, |s| {
                if s.voice_speech_model.as_deref() != Some(model.id) {
                    s.voice_speech_model = Some(model.id.to_owned());
                    s.voice_name = None;
                }
            });
            cx.notify();
        })
        .render(&self.voice.speech, cx);

        let voices = speech.voices;
        let voice_control =
            widgets::select("voice-mode-voice", "Voice", theme, |card: &mut Self| {
                &mut card.voice.voice
            })
            .options(
                voices.iter().map(|v| widgets::SelectOption::new(*v)),
                saved
                    .voice_name
                    .as_deref()
                    .and_then(|v| voices.iter().position(|known| *known == v))
                    .unwrap_or(0),
            )
            .width(260.0)
            .on_select(move |_, ix, _, cx| {
                let voice = voices.get(ix).map(|v| (*v).to_owned());
                settings::update(settings::SavePolicy::Immediate, cx, |s| {
                    s.voice_name = voice
                });
                cx.notify();
            })
            .render(&self.voice.voice, cx);

        let pause_control = widgets::select(
            "voice-mode-pause",
            "Send after a pause",
            theme,
            |card: &mut Self| &mut card.voice.pause,
        )
        .options(
            PAUSES
                .iter()
                .map(|(_, label)| widgets::SelectOption::new(*label)),
            PAUSES
                .iter()
                .position(|(ms, _)| *ms == saved.voice_pause_ms)
                .unwrap_or(1),
        )
        .width(200.0)
        .on_select(move |_, ix, _, cx| {
            if let Some((ms, _)) = PAUSES.get(ix) {
                settings::update(settings::SavePolicy::Immediate, cx, |s| {
                    s.voice_pause_ms = *ms
                });
            }
            cx.notify();
        })
        .render(&self.voice.pause, cx);

        let barge_in = saved.voice_barge_in;
        fn toggle_barge_in(cx: &mut App) {
            settings::update(settings::SavePolicy::Immediate, cx, |s| {
                s.voice_barge_in = !s.voice_barge_in
            });
        }
        let barge_in_switch = widgets::toggle_switch(theme, barge_in, "voice-mode-barge-in")
            .id("voice-mode-barge-in")
            .cursor_pointer()
            .tab_index(0)
            .role(gpui::Role::Switch)
            .aria_label("Interrupt by speaking")
            .aria_toggled(if barge_in {
                gpui::Toggled::True
            } else {
                gpui::Toggled::False
            })
            .focus_visible(|s| s.border_2().border_color(theme.accent).opacity(1.0))
            .on_click(cx.listener(move |_, _, _, cx| {
                toggle_barge_in(cx);
                cx.notify();
            }))
            .on_key_down(cx.listener(move |_, event: &gpui::KeyDownEvent, _, cx| {
                if matches!(event.keystroke.key.as_str(), "enter" | "space") {
                    cx.stop_propagation();
                    toggle_barge_in(cx);
                    cx.notify();
                }
            }))
            .on_a11y_action(gpui::AccessibleAction::Click, move |_, _, cx| {
                toggle_barge_in(cx);
            });

        widgets::section(
            theme,
            "Voice mode",
            widgets::section_card(theme)
                .child(openrouter_row)
                .child(row(
                    theme,
                    false,
                    "Speech to text",
                    Some("Transcribes what you say and sends it to the agent".into()),
                    stt_control,
                ))
                .children(stt_key_row)
                .child(row(
                    theme,
                    false,
                    "Transcription model",
                    None,
                    stt_model_control,
                ))
                .child(row(
                    theme,
                    false,
                    "Voice model",
                    Some("Reads the agent's messages aloud".into()),
                    speech_control,
                ))
                .child(row(theme, false, "Voice", None, voice_control))
                .child(row(
                    theme,
                    false,
                    "Send after a pause",
                    Some("How long a silence ends what you are saying".into()),
                    pause_control,
                ))
                .child(row(
                    theme,
                    false,
                    "Interrupt by speaking",
                    Some("Talk over the voice to stop it. Turn off if your speakers echo".into()),
                    barge_in_switch,
                )),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[gpui::test]
    fn voice_mode_needs_an_openrouter_key_and_its_transcriber_key(cx: &mut gpui::TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        cx.update(|cx| settings::init(settings::UiSettings::default(), dir.path(), cx));
        let card = cx.update(card);
        cx.update(|cx| assert!(call_config(cx).unwrap_err().contains("OpenRouter")));
        card.update(cx, |card, cx| {
            card.openrouter_input
                .update(cx, |input, cx| input.set_text(" sk-or ", cx));
            card.save_openrouter_key(cx);
        });
        cx.update(|cx| {
            // OpenRouter transcribes too, so one key is enough.
            let config = call_config(cx).unwrap();
            assert_eq!(config.openrouter_key, "sk-or");
            assert_eq!(config.stt.api_key, "sk-or");
            assert_eq!(config.speech_model, "hexgrad/kokoro-82m");
            assert_eq!(config.voice, "af_heart");
            assert!(config.barge_in);
            assert_eq!(config.end_silence, Duration::from_millis(800));
            settings::update(settings::SavePolicy::Immediate, cx, |s| {
                s.voice_stt_provider = call::SttProvider::Deepgram;
                s.voice_speech_model = Some("deepgram/aura-2".into());
                s.voice_name = Some("af_heart".into());
                s.voice_pause_ms = 10;
            });
            assert!(call_config(cx).unwrap_err().contains("Deepgram"));
        });
        card.update(cx, |card, cx| {
            card.voice_key_input
                .update(cx, |input, cx| input.set_text("dg", cx));
            card.save_voice_key(cx);
        });
        cx.update(|cx| {
            let config = call_config(cx).unwrap();
            assert_eq!(config.stt.api_key, "dg");
            // A voice the model lacks falls back to its first.
            assert_eq!(config.voice, "aura-2-thalia-en");
            assert_eq!(config.end_silence, Duration::from_millis(300));
        });
        let keys = load_keys(&dir.path().join(KEYS_FILE));
        assert_eq!(keys.get("openrouter").map(String::as_str), Some("sk-or"));
        assert_eq!(keys.get("deepgram").map(String::as_str), Some("dg"));
    }
}
