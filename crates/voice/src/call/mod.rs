//! Voice mode: the user speaks prompts and hears the coding agent's own
//! messages. This device listens and decides when the user has finished
//! (`turns`); each finished turn is transcribed (`providers::transcribe`) and
//! handed to the host, which sends it to the agent as a prompt. The host
//! passes back what the agent writes, a sentence at a time
//! ([`Command::Say`], split and cleaned by [`Reader`]); each sentence is voiced
//! (`providers::speak`) while the user is not talking. Talking over the voice
//! ducks it, then silences it.
//!
//! With [`Config::realtime`] set, one real-time model does all of that over a
//! single connection instead ([`realtime`]).
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::sync::{Notify, mpsc};

mod audio;
#[cfg(test)]
mod live;
mod providers;
mod realtime;
mod speech;
mod turns;

pub use providers::{
    Failure, OPENROUTER_KEY, SPEECH_MODELS, SpeechModel, Stt, SttProvider, speech_model,
};
pub use realtime::{Processing, Realtime, RealtimeModel, RealtimeProvider, Region, TurnTaking};
pub use speech::{MORE_IN_CHAT, Reader};

/// How often the loop reads the microphone and the meters.
const TICK: Duration = Duration::from_millis(20);
/// Meters reach the UI at this cadence.
const LEVEL_EVERY: u32 = 3;
/// Playback volume while the user may be talking over it.
const DUCKED: f32 = 0.25;

/// Everything one call needs.
#[derive(Clone, Debug)]
pub struct Config {
    pub stt: Stt,
    /// OpenRouter key for the voice.
    pub openrouter_key: String,
    pub speech_model: String,
    pub voice: String,
    /// Talking over the voice silences it.
    pub barge_in: bool,
    /// The pause that ends the user's turn.
    pub end_silence: Duration,
    /// Microphone id; `None` is the system default.
    pub input: Option<String>,
    /// A real-time model runs the call; the fields above that name the
    /// transcriber and the voice are then unused.
    pub realtime: Option<Realtime>,
}

/// Something the host should know or do.
#[derive(Debug)]
pub enum Event {
    /// The devices are open: the call is live.
    Ready,
    /// What the user just said is being transcribed.
    Transcribing(bool),
    /// What the user said in turn `turn`: the host sends it to the agent.
    User { turn: u64, text: String },
    /// Sentence `text` of narration `item` is being voiced.
    Speaking { item: String, text: String },
    /// The user talked over the voice and it stopped.
    Interrupted,
    /// What the user said, as a real-time model heard it: a caption only.
    Heard { item: String, text: String },
    /// More of what a real-time model is saying in answer `item`.
    Reply { item: String, delta: String },
    /// A real-time model hands this to the coding agent as a prompt.
    Prompt { turn: u64, text: String },
    /// Microphone and speaker loudness, 0–1.
    Levels { microphone: f32, speaker: f32 },
    /// A failure the call survives, worded for the user.
    Notice(String),
}

#[derive(Debug)]
pub enum Command {
    Mute(bool),
    /// Silence the voice and drop what it had left to say.
    Interrupt,
    /// Say one sentence of narration `item` once the user is not talking.
    Say {
        item: String,
        text: String,
    },
}

/// Stops a call.
#[derive(Clone, Default)]
pub struct Stop(Arc<StopInner>);
#[derive(Default)]
struct StopInner {
    stopped: AtomicBool,
    notify: Notify,
}
impl Stop {
    pub fn stop(&self) {
        self.0.stopped.store(true, Ordering::Release);
        self.0.notify.notify_waiters();
    }
    pub fn stopped(&self) -> bool {
        self.0.stopped.load(Ordering::Acquire)
    }
    pub async fn wait(&self) {
        loop {
            let notified = self.0.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.stopped() {
                return;
            }
            notified.await;
        }
    }
}

/// Why a call could not start or had to end, worded for the user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CallError {
    Devices(String),
    Provider(String),
}
impl std::fmt::Display for CallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Devices(m) | Self::Provider(m) => f.write_str(m),
        }
    }
}

/// What the listener and the voice share with the loop.
struct Shared {
    config: Config,
    client: reqwest::Client,
    devices: audio::Devices,
    events: mpsc::Sender<Event>,
    /// A fatal provider failure ends the call.
    fatal: Mutex<Option<Failure>>,
    /// The user is speaking: the voice waits, and a transcript waits for
    /// the rest of the thought.
    talking: AtomicBool,
    /// Bumped to silence the voice: sentences queued before it are dropped.
    hush: AtomicU64,
    hushed: Notify,
}

impl Shared {
    fn emit(&self, event: Event) {
        // Levels may be dropped under pressure; nothing else is sent often
        // enough to fill the channel.
        let _ = self.events.try_send(event);
    }

    fn talking(&self) -> bool {
        self.talking.load(Ordering::Acquire)
    }

    /// Stop the voice mid-sentence and drop what it had queued. Returns
    /// whether it was speaking.
    fn silence(&self) -> bool {
        let speaking = self.devices.queued() > 0;
        self.hush.fetch_add(1, Ordering::AcqRel);
        self.hushed.notify_waiters();
        self.devices.flush();
        self.devices.set_gain(1.0);
        speaking
    }

    /// Resolves once the voice is silenced after `hush`.
    async fn hushed_after(&self, hush: u64) {
        loop {
            let notified = self.hushed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.hush.load(Ordering::Acquire) != hush {
                return;
            }
            notified.await;
        }
    }
}

/// One sentence to voice, and the silence it belongs before.
struct Line {
    hush: u64,
    item: String,
    text: String,
}

/// Run one call until `stop`, a fatal failure, or the command channel closes.
pub async fn run(
    config: Config,
    mut commands: mpsc::Receiver<Command>,
    events: mpsc::Sender<Event>,
    stop: Stop,
) -> Result<(), CallError> {
    if let Some(rt) = config.realtime.clone() {
        return realtime::run(rt, config.input, config.barge_in, commands, events, stop).await;
    }
    if config.openrouter_key.trim().is_empty() {
        return Err(CallError::Provider(
            "Add an OpenRouter API key in Settings → API keys.".into(),
        ));
    }
    if config.stt.api_key.trim().is_empty() {
        return Err(CallError::Provider(format!(
            "Add a {} API key in Settings → API keys.",
            config.stt.provider.label()
        )));
    }
    let input = config.input.clone();
    let devices = tokio::task::spawn_blocking(move || audio::Devices::open(input))
        .await
        .map_err(|_| CallError::Devices("The audio devices did not open.".into()))?
        .map_err(|e| CallError::Devices(format!("{e}. Check your microphone and speakers.")))?;
    let mic_rate = devices.mic_rate;
    let end_silence = config.end_silence;
    let barge_in = config.barge_in;
    let shared = Arc::new(Shared {
        config,
        client: providers::client(),
        devices,
        events,
        fatal: Mutex::new(None),
        talking: AtomicBool::new(false),
        hush: AtomicU64::new(0),
        hushed: Notify::new(),
    });
    shared.emit(Event::Ready);

    let (heard_tx, heard) = mpsc::unbounded_channel();
    let listener = tokio::spawn(transcribe_turns(shared.clone(), heard, mic_rate));
    let (say_tx, lines) = mpsc::unbounded_channel();
    let voice = tokio::spawn(speak_lines(shared.clone(), lines));

    let mut turns = turns::Turns::new(mic_rate, end_silence);
    let mut muted = false;
    let mut mic = Vec::new();
    let mut tick = tokio::time::interval(TICK);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut ticks = 0u32;
    // The user may be speaking over the voice (ducked, not yet silenced).
    let mut ducked = false;

    let result = loop {
        tokio::select! {
            biased;
            _ = stop.wait() => break Ok(()),
            command = commands.recv() => match command {
                None => break Ok(()),
                Some(Command::Mute(value)) => {
                    muted = value;
                    if muted {
                        turns.reset();
                    }
                }
                Some(Command::Interrupt) => {
                    ducked = false;
                    if shared.silence() {
                        shared.emit(Event::Interrupted);
                    }
                }
                Some(Command::Say { item, text }) => {
                    let hush = shared.hush.load(Ordering::Acquire);
                    let _ = say_tx.send(Line { hush, item, text });
                }
            },
            _ = tick.tick() => {
                if shared.devices.failed() {
                    break Err(CallError::Devices(
                        "The microphone or speaker disconnected.".into(),
                    ));
                }
                if let Some(failure) = shared.fatal.lock().unwrap_or_else(|e| e.into_inner()).take() {
                    break Err(CallError::Provider(failure.message));
                }
                mic.clear();
                shared.devices.take_mic(&mut mic);
                let playing = shared.devices.queued() > 0;
                if muted || (playing && !barge_in) {
                    turns.reset();
                } else {
                    for event in turns.push(&mic, playing) {
                        match event {
                            turns::TurnEvent::Started if playing => {
                                ducked = true;
                                shared.devices.set_gain(DUCKED);
                            }
                            turns::TurnEvent::Started => {}
                            turns::TurnEvent::Confirmed => {
                                if std::mem::take(&mut ducked) && shared.silence() {
                                    shared.emit(Event::Interrupted);
                                }
                            }
                            turns::TurnEvent::Dropped => {
                                if std::mem::take(&mut ducked) {
                                    shared.devices.set_gain(1.0);
                                }
                            }
                            turns::TurnEvent::Ended(audio) => {
                                let _ = heard_tx.send(audio);
                            }
                        }
                    }
                }
                // Set after a finished turn is queued, so the listener sees
                // the turn before it sees the user stop.
                shared.talking.store(turns.active(), Ordering::Release);
                ticks = ticks.wrapping_add(1);
                if ticks.is_multiple_of(LEVEL_EVERY) {
                    let (microphone, speaker) = shared.devices.take_levels();
                    shared.emit(Event::Levels {
                        microphone: if muted { 0.0 } else { microphone },
                        speaker,
                    });
                }
            }
        }
    };
    listener.abort();
    voice.abort();
    shared.devices.flush();
    result
}

fn join(a: &str, b: &str) -> String {
    match (a.trim(), b.trim()) {
        ("", b) => b.to_owned(),
        (a, "") => a.to_owned(),
        (a, b) => format!("{a} {b}"),
    }
}

/// Transcribe finished turns in order, each becoming one prompt. A turn
/// that ends while the user is already speaking again waits for the rest,
/// so a pause mid-thought does not split one request in two.
async fn transcribe_turns(
    shared: Arc<Shared>,
    mut heard: mpsc::UnboundedReceiver<Vec<f32>>,
    rate: u32,
) {
    let mut turn = 0u64;
    while let Some(audio) = heard.recv().await {
        shared.emit(Event::Transcribing(true));
        let mut text = transcribe(&shared, audio, rate).await;
        while let Some(audio) = continuation(&shared, &mut heard).await {
            text = join(&text, &transcribe(&shared, audio, rate).await);
        }
        shared.emit(Event::Transcribing(false));
        if !text.is_empty() {
            turn += 1;
            // A prompt must not be dropped: wait for room.
            let _ = shared.events.send(Event::User { turn, text }).await;
        }
    }
}

/// The next part of what the user is saying, if they are still saying it.
async fn continuation(
    shared: &Shared,
    heard: &mut mpsc::UnboundedReceiver<Vec<f32>>,
) -> Option<Vec<f32>> {
    loop {
        if let Ok(audio) = heard.try_recv() {
            return Some(audio);
        }
        if !shared.talking() {
            return heard.try_recv().ok();
        }
        tokio::time::sleep(TICK).await;
    }
}

async fn transcribe(shared: &Shared, audio: Vec<f32>, rate: u32) -> String {
    let Ok(samples) = crate::resample::for_model(audio, rate) else {
        return String::new();
    };
    let wav = crate::cloud::wav(&samples);
    match providers::transcribe(&shared.client, &shared.config.stt, wav).await {
        Ok(text) => text,
        Err(failure) => {
            report(shared, failure);
            String::new()
        }
    }
}

/// Voice sentences in order, each once the user is not talking. A silence
/// drops what was queued before it and cuts the sentence being voiced.
async fn speak_lines(shared: Arc<Shared>, mut lines: mpsc::UnboundedReceiver<Line>) {
    while let Some(line) = lines.recv().await {
        let current = || shared.hush.load(Ordering::Acquire) == line.hush;
        while current() && shared.talking() {
            tokio::time::sleep(TICK).await;
        }
        if !current() {
            continue;
        }
        shared.emit(Event::Speaking {
            item: line.item,
            text: line.text.clone(),
        });
        let config = &shared.config;
        let mut resampler = Resampler::new(shared.devices.speaker_rate);
        let speech = providers::speak(
            &shared.client,
            &config.openrouter_key,
            &config.speech_model,
            &config.voice,
            &line.text,
            |samples, rate| {
                if current() {
                    shared.devices.play(&resampler.push(samples, rate));
                }
            },
        );
        let result = tokio::select! {
            result = speech => result,
            _ = shared.hushed_after(line.hush) => Ok(()),
        };
        if let Err(failure) = result {
            report(&shared, failure);
        }
    }
}

/// Tell the user about a failure; a fatal one ends the call.
fn report(shared: &Shared, failure: Failure) {
    if failure.fatal {
        *shared.fatal.lock().unwrap_or_else(|e| e.into_inner()) = Some(failure);
    } else {
        shared.emit(Event::Notice(failure.message));
    }
}

/// Streams speech at its own rate into the speaker's, continuously across
/// chunks (linear interpolation; speech only).
struct Resampler {
    to: u32,
    position: f64,
    last: Option<f32>,
}

impl Resampler {
    fn new(to: u32) -> Self {
        Self {
            to,
            position: 0.0,
            last: None,
        }
    }

    fn push(&mut self, input: &[f32], from: u32) -> Vec<f32> {
        if from == self.to || from == 0 {
            return input.to_vec();
        }
        // Positions count from the previous chunk's last sample at -1.
        let step = from as f64 / self.to as f64;
        let mut out = Vec::with_capacity(input.len() * self.to as usize / from as usize + 2);
        if input.is_empty() {
            return Vec::new();
        }
        let sample = |i: isize, last: Option<f32>| -> f32 {
            if i < 0 {
                last.unwrap_or(input[0])
            } else {
                input[(i as usize).min(input.len() - 1)]
            }
        };
        while self.position <= (input.len() - 1) as f64 {
            let index = self.position.floor();
            let fraction = (self.position - index) as f32;
            let a = sample(index as isize, self.last);
            let b = sample(index as isize + 1, self.last);
            out.push(a + (b - a) * fraction);
            self.position += step;
        }
        self.position -= input.len() as f64;
        self.last = input.last().copied();
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn streamed_resampling_is_continuous_across_chunks() {
        let input: Vec<f32> = (0..2400).map(|i| (i as f32 * 0.01).sin()).collect();
        let whole = Resampler::new(48_000).push(&input, 24_000);
        let mut chunked = Resampler::new(48_000);
        let mut parts = Vec::new();
        for chunk in input.chunks(333) {
            parts.extend(chunked.push(chunk, 24_000));
        }
        assert_eq!(parts.len(), whole.len());
        for (a, b) in parts.iter().zip(&whole) {
            assert!((a - b).abs() < 1e-4);
        }
        assert_eq!(whole.len(), 4799);
        let same = Resampler::new(24_000).push(&input, 24_000);
        assert_eq!(same.len(), input.len());
    }

    #[test]
    fn words_join_without_stray_spaces() {
        assert_eq!(join("", " hi "), "hi");
        assert_eq!(join("open the", "tests"), "open the tests");
        assert_eq!(join("done", ""), "done");
    }

    #[tokio::test]
    async fn stop_wakes_every_waiter_even_if_stopped_first() {
        let stop = Stop::default();
        let waiter = tokio::spawn({
            let stop = stop.clone();
            async move { stop.wait().await }
        });
        tokio::task::yield_now().await;
        stop.stop();
        tokio::time::timeout(Duration::from_secs(1), waiter)
            .await
            .unwrap()
            .unwrap();
        tokio::time::timeout(Duration::from_secs(1), stop.wait())
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn a_call_without_keys_explains_what_to_add() {
        let (_commands, commands_rx) = mpsc::channel(1);
        let (events, _events_rx) = mpsc::channel(1);
        let config = Config {
            stt: Stt::default(),
            openrouter_key: String::new(),
            speech_model: SPEECH_MODELS[0].id.into(),
            voice: SPEECH_MODELS[0].voices[0].into(),
            barge_in: true,
            end_silence: Duration::from_millis(800),
            input: None,
            realtime: None,
        };
        let error = run(config.clone(), commands_rx, events.clone(), Stop::default())
            .await
            .unwrap_err();
        assert!(error.to_string().contains("OpenRouter"));
        let (_commands, commands_rx) = mpsc::channel(1);
        let config = Config {
            openrouter_key: "key".into(),
            stt: Stt {
                provider: SttProvider::Deepgram,
                ..Stt::default()
            },
            ..config
        };
        let error = run(config.clone(), commands_rx, events, Stop::default())
            .await
            .unwrap_err();
        assert!(error.to_string().contains("Deepgram"));
        // A real-time call needs only its provider's key.
        let (_commands, commands_rx) = mpsc::channel(1);
        let (events, _events_rx) = mpsc::channel(1);
        let config = Config {
            realtime: Some(Realtime {
                provider: RealtimeProvider::Gemini,
                api_key: " ".into(),
                model: "gemini-3.1-flash-live-preview".into(),
                voice: "Puck".into(),
                region: Region::International,
                turns: TurnTaking::Balanced,
            }),
            ..config
        };
        let error = run(config, commands_rx, events, Stop::default())
            .await
            .unwrap_err();
        assert!(error.to_string().contains("Gemini API key"), "{error}");
    }
}
