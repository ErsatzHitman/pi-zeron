//! Window-owned voice mode, run on this device by `zeron_voice::call`. What
//! the user says is transcribed and sent to the coding agent in their chat
//! as a prompt ([`agent`]); the agent works exactly as it does for typed
//! prompts, and its own messages are read aloud as it writes them
//! ([`narrator`]). A call survives navigation between threads; only an
//! explicit end, losing the engine it reports to or a provider failure
//! closes it.
use crate::orb::OrbState;
use crate::state::EngineHandle;
use gpui::{App, Context, Task};
use gpui_tokio::Tokio;
use zeron_proto::voice::*;
use zeron_voice::call;
mod agent;
mod narrator;
mod permissions;

pub(crate) use agent::AgentBridge;

/// Every local call reuses one generation: events never cross calls
/// because the controller's epoch guards them.
const GENERATION: u64 = 1;

pub struct VoiceController {
    pub phase: VoicePhase,
    pub chat_id: Option<String>,
    pub reason: Option<VoiceRejection>,
    /// The words for a failure no rejection names (a provider's answer).
    pub failure: Option<String>,
    pub snapshot: Option<VoiceSnapshot>,
    /// The newest speaker turn's live text, then its final.
    pub caption: zeron_voice_session::Caption,
    pub microphone_level: u16,
    pub speaker_level: u16,
    /// The full-window stage is presented over the shell.
    pub stage_open: bool,
    pub host_name: Option<String>,
    speaker_last_loud: Option<std::time::Instant>,
    /// When the session first became active; drives the stage's call timer.
    pub active_since: Option<std::time::Instant>,
    engine: Option<EngineHandle>,
    controls: Option<tokio::sync::mpsc::Sender<call::Command>>,
    epoch: u64,
    task: Option<Task<()>>,
    cancellation: tokio_util::sync::CancellationToken,
    /// What the user said is being transcribed.
    transcribing: bool,
    /// The coding agent prompts go to, and the chat voice reads aloud.
    agent: Option<AgentBridge>,
    pub agent_chat: Option<String>,
    agent_seen: zeron_proto::view::Indicator,
    agent_watch: Option<gpui::Subscription>,
    /// Watches `agent_chat` and passes the agent's messages to the voice.
    narration: Option<Task<()>>,
    notices: u64,
    /// This call played its start sound and owes its end sound.
    cued: bool,
}
impl Default for VoiceController {
    fn default() -> Self {
        Self {
            phase: VoicePhase::Closed,
            chat_id: None,
            reason: None,
            failure: None,
            snapshot: None,
            caption: Default::default(),
            microphone_level: 0,
            speaker_level: 0,
            stage_open: false,
            host_name: None,
            speaker_last_loud: None,
            active_since: None,
            engine: None,
            controls: None,
            epoch: 0,
            task: None,
            cancellation: tokio_util::sync::CancellationToken::new(),
            transcribing: false,
            agent: None,
            agent_chat: None,
            agent_seen: zeron_proto::view::Indicator::None,
            agent_watch: None,
            narration: None,
            notices: 0,
            cued: false,
        }
    }
}
impl VoiceController {
    /// Voice can be offered once Settings → Voice has its keys; a live call
    /// or its failure stays.
    pub fn offered(&self, cx: &App) -> bool {
        self.is_live() || self.reason.is_some() || crate::dictation::call_config(cx).is_ok()
    }

    /// A session is underway (including its preparation and native stop).
    pub fn is_live(&self) -> bool {
        matches!(self.phase, VoicePhase::Checking) || self.phase.replaces_composer()
    }

    /// Start a call. `agent` is where prompts go and what is read aloud;
    /// `engine` is the connection it reports to.
    pub(crate) fn start(
        &mut self,
        config: call::Config,
        engine: Option<EngineHandle>,
        agent: Option<AgentBridge>,
        cx: &mut Context<Self>,
    ) {
        self.cancel(cx);
        // The call sounds as it is placed, well before the microphone opens.
        crate::sound::play_voice(true);
        self.cued = true;
        self.engine = engine;
        self.phase = VoicePhase::Checking;
        self.chat_id = None;
        self.reason = None;
        self.failure = None;
        let chat = agent
            .as_ref()
            .and_then(|agent| agent.state.read(cx).selected_chat.clone());
        if let Some(agent) = &agent {
            self.agent_watch =
                Some(cx.observe(&agent.state, |voice, _, cx| voice.agent_changed(cx)));
        }
        self.agent = agent;
        // The open chat is read from the start: its history stays unread.
        if let Some(chat) = chat {
            self.follow(chat, None, cx);
        }
        let epoch = self.epoch;
        self.reduce(
            VoiceEvent::Snapshot {
                snapshot: VoiceSnapshot {
                    session_id: format!("local-{epoch}"),
                    chat_id: format!("{ORCHESTRATOR_CHAT_PREFIX}local"),
                    generation: GENERATION,
                    phase: VoicePhase::Starting,
                    muted: false,
                    playing: false,
                    work: VoiceWork::Idle,
                    reason: None,
                    voice: Some(config.voice.clone()),
                    voices: Vec::new(),
                },
            },
            cx,
        );
        let cancellation = self.cancellation.clone();
        let (controls, control_rx) = tokio::sync::mpsc::channel(16);
        self.controls = Some(controls);
        // Meters outnumber everything else; keep room for them.
        let (events, mut event_rx) = tokio::sync::mpsc::channel(512);
        let query = Tokio::spawn(cx, async move {
            permissions::microphone().await.map_err(Failure::Rejected)?;
            let stop = call::Stop::default();
            let run = call::run(config, control_rx, events, stop.clone());
            tokio::pin!(run);
            tokio::select! {
                result = &mut run => result.map_err(Failure::Call),
                _ = cancellation.cancelled() => {
                    stop.stop();
                    (&mut run).await.map_err(Failure::Call)
                }
            }
        });
        self.task = Some(cx.spawn(async move |this, cx| {
            while let Some(event) = event_rx.recv().await {
                if this
                    .update(cx, |controller, cx| {
                        if controller.epoch == epoch {
                            controller.on_call(event, cx);
                        }
                    })
                    .is_err()
                {
                    break;
                }
            }
            let result = query
                .await
                .unwrap_or(Err(Failure::Rejected(VoiceRejection::Protocol)));
            let _ = this.update(cx, |controller, cx| {
                if controller.epoch != epoch {
                    return;
                }
                match result {
                    Ok(()) => controller.phase = VoicePhase::Closed,
                    Err(Failure::Rejected(reason)) => {
                        controller.phase = VoicePhase::Failed;
                        controller.reason = Some(reason);
                    }
                    Err(Failure::Call(error)) => {
                        controller.phase = VoicePhase::Failed;
                        controller.reason = Some(match error {
                            call::CallError::Devices(_) => VoiceRejection::DeviceUnavailable,
                            call::CallError::Provider(_) => VoiceRejection::Unsupported,
                        });
                        controller.failure = Some(error.to_string());
                    }
                }
                controller.reset_session();
                cx.notify();
            });
        }));
        cx.notify();
    }

    /// Ending now sounds: a hang-up or a connected call ending. A failure to
    /// connect only explains itself.
    fn end_sounds(&self) -> bool {
        self.cued && (self.active_since.is_some() || self.reason.is_none())
    }

    fn reset_session(&mut self) {
        if self.end_sounds() {
            crate::sound::play_voice(false);
        }
        self.cued = false;
        self.snapshot = None;
        self.caption.clear();
        self.microphone_level = 0;
        self.speaker_level = 0;
        self.controls = None;
        self.task = None;
        self.stage_open = false;
        self.active_since = None;
        self.transcribing = false;
        self.agent = None;
        self.agent_watch = None;
        self.narration = None;
    }

    /// End the session (any phase). The provider stop runs in the background.
    pub fn cancel(&mut self, cx: &mut Context<Self>) {
        self.epoch = self.epoch.wrapping_add(1);
        self.cancellation.cancel();
        self.cancellation = tokio_util::sync::CancellationToken::new();
        self.reset_session();
        self.phase = VoicePhase::Closed;
        self.chat_id = None;
        self.agent_chat = None;
        self.reason = None;
        self.failure = None;
        self.engine = None;
        cx.notify();
    }

    pub fn set_stage_open(&mut self, open: bool, cx: &mut Context<Self>) {
        let open = open && self.is_live();
        if self.stage_open != open {
            self.stage_open = open;
            cx.notify();
        }
    }

    /// Forget a failure once it has been seen.
    pub fn dismiss_reason(&mut self, cx: &mut Context<Self>) {
        if self.reason.take().is_some() {
            self.failure = None;
            if self.phase == VoicePhase::Failed {
                self.phase = VoicePhase::Closed;
            }
            cx.notify();
        }
    }

    /// The call reported to a connection that is gone or was replaced.
    pub fn engine_lost(&self, engine: Option<&EngineHandle>) -> bool {
        self.engine
            .as_ref()
            .is_some_and(|mine| engine.is_none_or(|e| !mine.same_connection(e)))
    }
    pub fn muted(&self) -> bool {
        self.snapshot.as_ref().is_some_and(|s| s.muted)
    }
    pub fn toggle_mute(&mut self, cx: &mut Context<Self>) {
        let muted = self.muted();
        if let Some(snapshot) = &mut self.snapshot {
            snapshot.muted = !muted;
        }
        cx.notify();
        self.command(call::Command::Mute(!muted), cx);
    }

    /// Stop reading aloud, dropping what was left to say.
    pub fn interrupt(&mut self, cx: &mut Context<Self>) {
        self.command(call::Command::Interrupt, cx);
    }

    fn command(&mut self, command: call::Command, cx: &mut Context<Self>) {
        if let Some(controls) = &self.controls
            && controls.try_send(command).is_err()
        {
            self.cancel(cx);
            self.reason = Some(VoiceRejection::Overflow);
        }
    }

    /// The utterance the caption shows (a speaker turn), if the provider names it.
    pub fn caption_item(&self) -> Option<&str> {
        self.caption.item()
    }

    pub fn orb_state(&self) -> OrbState {
        orb_state(self.phase, self.snapshot.as_ref())
    }

    /// Normalized 0…1 microphone loudness; zero while muted.
    pub fn microphone_level(&self) -> f32 {
        if self.muted() {
            return 0.0;
        }
        f32::from(self.microphone_level) / f32::from(u16::MAX)
    }

    /// Normalized 0…1 speaker peak; the orb smooths each channel separately.
    pub fn speaker_level(&self) -> f32 {
        f32::from(self.speaker_level) / f32::from(u16::MAX)
    }

    fn session_id(&self) -> Option<String> {
        self.snapshot.as_ref().map(|s| s.session_id.clone())
    }

    /// A call event, reduced into the same view state every surface reads.
    fn on_call(&mut self, event: call::Event, cx: &mut Context<Self>) {
        let Some(session_id) = self.session_id() else {
            return;
        };
        let transcript = |item_id: String, role: VoiceRole, text: String| VoiceEvent::Final {
            transcript: VoiceTranscript {
                session_id: session_id.clone(),
                item_id,
                role,
                text,
                promoted_message_id: None,
            },
        };
        match event {
            call::Event::Ready => {
                if let Some(mut snapshot) = self.snapshot.clone() {
                    snapshot.phase = VoicePhase::Active;
                    self.reduce(VoiceEvent::Snapshot { snapshot }, cx);
                }
            }
            call::Event::Transcribing(transcribing) => {
                self.transcribing = transcribing;
                self.sync_work(cx);
            }
            call::Event::User { turn, text } => {
                self.reduce(
                    transcript(format!("user-{turn}"), VoiceRole::User, text.clone()),
                    cx,
                );
                self.send_prompt(text, cx);
            }
            // Each message read aloud builds one caption, a sentence at a time.
            call::Event::Speaking { item, text } => {
                let item = format!("agent-{item}");
                let text = if self.caption_item() == Some(item.as_str()) {
                    format!(" {text}")
                } else {
                    text
                };
                self.reduce(
                    VoiceEvent::Partial {
                        generation: GENERATION,
                        item_id: Some(item),
                        text,
                    },
                    cx,
                );
            }
            call::Event::Interrupted => {}
            call::Event::Levels {
                microphone,
                speaker,
            } => self.reduce(
                VoiceEvent::Levels {
                    generation: GENERATION,
                    // Speech RMS sits far below its peak; lift it to the
                    // range the orb's microphone channel expects.
                    microphone: level(microphone * 4.0),
                    speaker: level(speaker),
                },
                cx,
            ),
            call::Event::Notice(text) => self.notice(text, false, cx),
        }
    }

    /// Caption a failure the call survives; `speak` also says it.
    fn notice(&mut self, text: String, speak: bool, cx: &mut Context<Self>) {
        self.notices += 1;
        let item = format!("notice-{}", self.notices);
        if speak {
            self.command(
                call::Command::Say {
                    item: item.clone(),
                    text: text.clone(),
                },
                cx,
            );
        }
        let Some(session_id) = self.session_id() else {
            return;
        };
        self.reduce(
            VoiceEvent::Final {
                transcript: VoiceTranscript {
                    session_id,
                    item_id: item,
                    role: VoiceRole::Assistant,
                    text,
                    promoted_message_id: None,
                },
            },
            cx,
        );
    }

    /// The orb's work state: the agent waiting on the user, or work underway
    /// (the agent running, or what the user said being transcribed).
    fn sync_work(&mut self, cx: &mut Context<Self>) {
        let agent = self.agent_seen;
        let work = match agent {
            zeron_proto::view::Indicator::AwaitingInput => VoiceWork::AwaitingInput,
            zeron_proto::view::Indicator::Working => VoiceWork::Working,
            _ if self.transcribing => VoiceWork::Working,
            _ => VoiceWork::Idle,
        };
        if let Some(snapshot) = &mut self.snapshot
            && snapshot.work != work
        {
            snapshot.work = work;
            cx.notify();
        }
    }

    pub fn reduce(&mut self, event: VoiceEvent, cx: &mut Context<Self>) {
        match event {
            VoiceEvent::Snapshot { mut snapshot } => {
                if let Some(previous) = &self.snapshot {
                    snapshot.playing = previous.playing;
                }
                if self.chat_id.is_none() {
                    self.chat_id = Some(snapshot.chat_id.clone());
                }

                if self.chat_id.as_deref() != Some(snapshot.chat_id.as_str()) {
                    return;
                }
                if self.snapshot.as_ref().is_some_and(|old| {
                    old.generation != snapshot.generation || old.session_id != snapshot.session_id
                }) {
                    return;
                }
                self.phase = snapshot.phase;
                if self.phase == VoicePhase::Active && self.active_since.is_none() {
                    self.active_since = Some(std::time::Instant::now());
                }
                self.snapshot = Some(snapshot);
            }
            VoiceEvent::Partial {
                generation,
                item_id,
                text,
            } if self
                .snapshot
                .as_ref()
                .is_some_and(|s| s.generation == generation) =>
            {
                if !self.caption.partial(item_id, &text) {
                    self.cancel(cx);
                    self.phase = VoicePhase::Failed;
                    self.reason = Some(VoiceRejection::Overflow);
                }
            }
            VoiceEvent::Levels {
                generation,
                microphone,
                speaker,
            } if self
                .snapshot
                .as_ref()
                .is_some_and(|s| s.generation == generation) =>
            {
                self.microphone_level = microphone;
                self.speaker_level = speaker;
                if let Some(snapshot) = &mut self.snapshot {
                    let threshold = if snapshot.playing { 328 } else { 655 };
                    if speaker >= threshold {
                        snapshot.playing = true;
                        self.speaker_last_loud = Some(std::time::Instant::now());
                    } else if self
                        .speaker_last_loud
                        .is_none_or(|t| t.elapsed() >= std::time::Duration::from_millis(250))
                    {
                        snapshot.playing = false;
                    }
                }
            }
            // The final segment keeps showing as the caption until the next
            // item starts; the canonical copy is already in the transcript.
            VoiceEvent::Final { transcript }
                if self
                    .snapshot
                    .as_ref()
                    .is_some_and(|s| s.session_id == transcript.session_id) =>
            {
                self.caption.complete(&transcript);
            }
            VoiceEvent::Closed { generation, reason }
                if self
                    .snapshot
                    .as_ref()
                    .is_some_and(|s| s.generation == generation) =>
            {
                self.phase = if reason.is_some() {
                    VoicePhase::Failed
                } else {
                    VoicePhase::Closed
                };
                self.reason = reason;
                self.reset_session();
            }
            _ => return,
        }
        cx.notify();
    }
    pub fn reason_text(&self) -> String {
        if let Some(failure) = &self.failure {
            return failure.clone();
        }
        match self.reason {
            Some(VoiceRejection::DeviceUnavailable) => {
                "Check microphone permission and your audio devices."
            }
            Some(VoiceRejection::MicrophonePermissionDenied) => {
                "Allow Zeron to use the microphone in System Settings → Privacy & Security → Microphone."
            }
            Some(VoiceRejection::MicrophoneMetadataMissing) => {
                "Microphone setup is missing. Rebuild or reinstall Zeron, then restart it."
            }
            Some(VoiceRejection::Busy) => "Another window already owns the voice session.",
            Some(_) => "Voice could not connect. Try again in a moment.",
            None => "",
        }
        .to_owned()
    }
}

/// A 0–1 level as the orb's 16-bit meter.
fn level(value: f32) -> u16 {
    (value.clamp(0.0, 1.0) * f32::from(u16::MAX)) as u16
}

/// Why a call ended early.
enum Failure {
    Rejected(VoiceRejection),
    Call(call::CallError),
}

/// Call-timer text: `m:ss`, or `h:mm:ss` past the hour.
pub fn format_elapsed(seconds: u64) -> String {
    let (hours, minutes, seconds) = (seconds / 3600, seconds / 60 % 60, seconds % 60);
    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes}:{seconds:02}")
    }
}

/// Shared with the mobile apps so every orb reads the call the same way.
pub use zeron_voice_session::orb_state;

impl Drop for VoiceController {
    fn drop(&mut self) {
        self.cancellation.cancel();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::AppContext;

    fn snapshot() -> VoiceSnapshot {
        VoiceSnapshot {
            session_id: "one".into(),
            chat_id: "chat".into(),
            generation: 2,
            phase: VoicePhase::Active,
            muted: false,
            playing: false,
            work: VoiceWork::Idle,
            reason: None,
            voice: None,
            voices: Vec::new(),
        }
    }

    #[gpui::test]
    fn voice_is_offered_once_its_keys_are_saved(cx: &mut gpui::TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        cx.update(|cx| {
            crate::settings::init(crate::settings::UiSettings::default(), dir.path(), cx);
            crate::dictation::init(dir.path().into(), cx);
        });
        let voice = cx.new(|_| VoiceController::default());
        voice.update(cx, |voice, cx| {
            assert!(!voice.offered(cx), "no key, no voice");
            // A failure explains itself before the trigger goes away.
            voice.reason = Some(VoiceRejection::DeviceUnavailable);
            assert!(voice.offered(cx));
            voice.reason = None;
            voice.phase = VoicePhase::Checking;
            assert!(voice.offered(cx), "a call in progress keeps its trigger");
        });
    }

    #[test]
    fn only_a_hang_up_or_a_connected_call_sounds_its_end() {
        let mut voice = VoiceController::default();
        assert!(!voice.end_sounds(), "no call was placed");
        voice.cued = true;
        assert!(voice.end_sounds(), "hanging up while connecting");
        voice.reason = Some(VoiceRejection::Busy);
        assert!(!voice.end_sounds(), "a failure to connect stays quiet");
        voice.active_since = Some(std::time::Instant::now());
        assert!(voice.end_sounds(), "a connected call that drops");
    }

    #[gpui::test]
    fn voice_reducer_ignores_stale_and_foreign_events(cx: &mut gpui::TestAppContext) {
        let voice = cx.new(|_| VoiceController::default());
        voice.update(cx, |voice, cx| {
            voice.chat_id = Some("chat".into());
            let snapshot = snapshot();
            voice.reduce(
                VoiceEvent::Snapshot {
                    snapshot: snapshot.clone(),
                },
                cx,
            );
            voice.reduce(
                VoiceEvent::Partial {
                    generation: 1,
                    item_id: None,
                    text: "old".into(),
                },
                cx,
            );
            assert!(voice.caption.text().is_empty());
            voice.reduce(
                VoiceEvent::Partial {
                    generation: 2,
                    item_id: None,
                    text: "current".into(),
                },
                cx,
            );
            let mut foreign = snapshot;
            foreign.chat_id = "other".into();
            foreign.generation = 3;
            voice.reduce(VoiceEvent::Snapshot { snapshot: foreign }, cx);
            assert_eq!(voice.snapshot.as_ref().unwrap().generation, 2);
            voice.reduce(
                VoiceEvent::Closed {
                    generation: 1,
                    reason: None,
                },
                cx,
            );
            assert!(voice.is_live());
            voice.set_stage_open(true, cx);
            assert!(voice.stage_open);
            voice.cancel(cx);
            assert!(!voice.is_live());
            assert!(!voice.stage_open);
            assert!(voice.caption.text().is_empty());
            // A closed session never reopens the stage.
            voice.set_stage_open(true, cx);
            assert!(!voice.stage_open);
        });
    }

    /// A user's final can land after the answer started streaming: the
    /// caption stays on the answer and keeps every streamed word.
    #[gpui::test]
    fn late_user_final_keeps_the_streaming_answer_captioned(cx: &mut gpui::TestAppContext) {
        let voice = cx.new(|_| VoiceController::default());
        voice.update(cx, |voice, cx| {
            voice.chat_id = Some("chat".into());
            voice.reduce(
                VoiceEvent::Snapshot {
                    snapshot: snapshot(),
                },
                cx,
            );
            let partial = |item: &str, text: &str| VoiceEvent::Partial {
                generation: 2,
                item_id: Some(item.into()),
                text: text.into(),
            };
            voice.reduce(partial("user", "Open the test"), cx);
            voice.reduce(partial("answer", "Sure, "), cx);
            voice.reduce(
                VoiceEvent::Final {
                    transcript: VoiceTranscript {
                        session_id: "one".into(),
                        item_id: "user".into(),
                        role: VoiceRole::User,
                        text: "Open the test.".into(),
                        promoted_message_id: None,
                    },
                },
                cx,
            );
            voice.reduce(partial("answer", "opening it."), cx);
            assert_eq!(voice.caption.text(), "Sure, opening it.");
            assert_eq!(voice.caption_item(), Some("answer"));
        });
    }

    /// Call events drive the same caption, orb and call clock as before.
    #[gpui::test]
    fn call_events_become_captions_levels_and_work(cx: &mut gpui::TestAppContext) {
        let voice = cx.new(|_| VoiceController::default());
        voice.update(cx, |voice, cx| {
            voice.phase = VoicePhase::Checking;
            voice.reduce(
                VoiceEvent::Snapshot {
                    snapshot: VoiceSnapshot {
                        session_id: "local-0".into(),
                        chat_id: format!("{ORCHESTRATOR_CHAT_PREFIX}local"),
                        generation: GENERATION,
                        phase: VoicePhase::Starting,
                        ..snapshot()
                    },
                },
                cx,
            );
            assert_eq!(voice.orb_state(), OrbState::Connecting);
            voice.on_call(call::Event::Ready, cx);
            assert_eq!(voice.phase, VoicePhase::Active);
            assert!(voice.active_since.is_some());
            assert_eq!(voice.orb_state(), OrbState::Listening);
            voice.on_call(
                call::Event::User {
                    turn: 1,
                    text: "Run the tests.".into(),
                },
                cx,
            );
            assert_eq!(voice.caption.text(), "Run the tests.");
            voice.on_call(call::Event::Transcribing(true), cx);
            assert_eq!(voice.orb_state(), OrbState::Working);
            voice.on_call(call::Event::Transcribing(false), cx);
            // The agent's message is captioned as it is read, one sentence
            // after another.
            for text in ["I'm reading the tests.", "Running them now."] {
                voice.on_call(
                    call::Event::Speaking {
                        item: "a1/t1".into(),
                        text: text.into(),
                    },
                    cx,
                );
            }
            assert_eq!(
                voice.caption.text(),
                "I'm reading the tests. Running them now."
            );
            voice.on_call(
                call::Event::Levels {
                    microphone: 0.0,
                    speaker: 0.5,
                },
                cx,
            );
            assert_eq!(voice.orb_state(), OrbState::Composing);
            // A notice is its own caption.
            voice.on_call(call::Event::Notice("Deepgram is rate limiting.".into()), cx);
            assert_eq!(voice.caption.text(), "Deepgram is rate limiting.");
        });
    }

    #[gpui::test]
    fn provider_failure_closes_the_stage_and_keeps_its_reason(cx: &mut gpui::TestAppContext) {
        let voice = cx.new(|_| VoiceController::default());
        voice.update(cx, |voice, cx| {
            voice.chat_id = Some("chat".into());
            voice.reduce(
                VoiceEvent::Snapshot {
                    snapshot: snapshot(),
                },
                cx,
            );
            voice.set_stage_open(true, cx);
            voice.reduce(
                VoiceEvent::Closed {
                    generation: 2,
                    reason: Some(VoiceRejection::DeviceUnavailable),
                },
                cx,
            );
            assert_eq!(voice.phase, VoicePhase::Failed);
            assert!(!voice.stage_open);
            assert!(!voice.reason_text().is_empty());
            voice.failure = Some("The OpenRouter API key was rejected.".into());
            assert_eq!(voice.reason_text(), "The OpenRouter API key was rejected.");
            voice.dismiss_reason(cx);
            assert_eq!(voice.phase, VoicePhase::Closed);
            assert!(voice.failure.is_none());
        });
    }

    #[test]
    fn call_timer_reads_like_a_call() {
        assert_eq!(format_elapsed(0), "0:00");
        assert_eq!(format_elapsed(75), "1:15");
        assert_eq!(format_elapsed(3600 + 62), "1:01:02");
    }

    #[test]
    fn visual_state_keeps_muting_and_playback_orthogonal() {
        let mut s = snapshot();
        s.muted = true;
        s.playing = true;
        assert_eq!(orb_state(s.phase, Some(&s)), OrbState::Composing);
        s.playing = false;
        assert_eq!(orb_state(s.phase, Some(&s)), OrbState::Breathing);
        s.muted = false;
        assert_eq!(orb_state(s.phase, Some(&s)), OrbState::Listening);
        s.work = VoiceWork::Working;
        assert_eq!(orb_state(s.phase, Some(&s)), OrbState::Working);
        s.work = VoiceWork::AwaitingInput;
        assert_eq!(orb_state(s.phase, Some(&s)), OrbState::Solving);
        assert_eq!(
            orb_state(VoicePhase::Starting, Some(&s)),
            OrbState::Connecting
        );
        assert_eq!(level(2.0), u16::MAX);
        assert_eq!(level(-1.0), 0);
    }
}
