//! Voice mode: a spoken conversation through three cloud stages. This device
//! listens and decides when the user has finished (`turns`), the turn is
//! transcribed (`providers::transcribe`), the reply streams from an
//! OpenRouter chat model and each sentence is voiced as soon as it ends
//! (`speech`, `providers::speak`). Talking over the assistant ducks it, then
//! interrupts it. Tools are answered by the host through [`Event::Tool`].
use serde_json::{Value, json};
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::sync::{Notify, mpsc, oneshot};

mod audio;
mod providers;
mod speech;
mod turns;

pub use providers::{
    Failure, OPENROUTER_KEY, REPLY_MODELS, SPEECH_MODELS, SpeechModel, Stt, SttProvider, Tool,
    speech_model,
};

/// How often the loop reads the microphone and the meters.
const TICK: Duration = Duration::from_millis(20);
/// Meters reach the UI at this cadence.
const LEVEL_EVERY: u32 = 3;
/// Playback volume while the user may be talking over it.
const DUCKED: f32 = 0.25;
/// Model round trips per turn, so a tool loop cannot run away.
const MAX_ROUNDS: usize = 4;
/// Messages of conversation kept besides the instructions.
const HISTORY: usize = 40;
const TOOL_TIMEOUT: Duration = Duration::from_secs(30);

const INSTRUCTIONS: &str = "You are Zeron's voice assistant, talking with the user out loud. \
Everything you write is spoken by a text-to-speech voice, so answer in short, natural spoken \
sentences: usually one to three. Never use Markdown, lists, code blocks, tables, emoji or URLs; \
describe code in words instead of reading it out. If you did not catch something, ask briefly. \
The user may interrupt you; that is fine. Reply in the user's language.";

/// Everything one call needs.
#[derive(Clone, Debug)]
pub struct Config {
    pub stt: Stt,
    /// OpenRouter key for the reply and the voice.
    pub openrouter_key: String,
    pub reply_model: String,
    pub speech_model: String,
    pub voice: String,
    /// Talking over the assistant interrupts it.
    pub barge_in: bool,
    /// The pause that ends the user's turn.
    pub end_silence: Duration,
    /// Microphone id; `None` is the system default.
    pub input: Option<String>,
    /// Added to the built-in instructions (what the tools are for).
    pub instructions: String,
    pub tools: Vec<Tool>,
}

/// Something the host should know or do.
#[derive(Debug)]
pub enum Event {
    /// The devices are open: the call is live.
    Ready,
    /// The assistant is working out a reply (transcribing, thinking, or
    /// waiting for a tool) and not yet speaking.
    Thinking(bool),
    /// What the user said in turn `turn`.
    User {
        turn: u64,
        text: String,
    },
    /// The assistant's reply as it streams.
    AssistantDelta {
        turn: u64,
        text: String,
    },
    AssistantDone {
        turn: u64,
        text: String,
    },
    /// The user talked over the assistant and it stopped.
    Interrupted,
    /// Microphone and speaker loudness, 0–1.
    Levels {
        microphone: f32,
        speaker: f32,
    },
    Tool(ToolCall),
    /// A failed turn the call survives, worded for the user.
    Notice(String),
}

/// A tool the model called. Answer through `reply` (a short text result).
#[derive(Debug)]
pub struct ToolCall {
    pub name: String,
    pub arguments: Value,
    pub reply: oneshot::Sender<String>,
}

#[derive(Debug)]
pub enum Command {
    Mute(bool),
    /// Stop the assistant mid-reply.
    Interrupt,
    /// Tell the user something (an agent's progress) when the line is quiet.
    Announce(String),
}

/// Stops a call, or one turn of it.
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

/// What every turn shares.
struct Shared {
    config: Config,
    client: reqwest::Client,
    devices: audio::Devices,
    events: mpsc::Sender<Event>,
    history: Mutex<Vec<Value>>,
    /// A fatal provider failure ends the call.
    fatal: Mutex<Option<Failure>>,
}

impl Shared {
    fn emit(&self, event: Event) {
        // Levels and deltas may be dropped under pressure; nothing else is
        // sent often enough to fill the channel.
        let _ = self.events.try_send(event);
    }
    fn remember(&self, message: Value) {
        let mut history = self.history.lock().unwrap_or_else(|e| e.into_inner());
        history.push(message);
        let excess = history.len().saturating_sub(HISTORY);
        if excess > 0 {
            history.drain(..excess);
            // A tool result must not lead the history without its call.
            while history
                .first()
                .is_some_and(|m| m.get("role").and_then(Value::as_str) == Some("tool"))
            {
                history.remove(0);
            }
        }
    }
    fn messages(&self) -> Vec<Value> {
        let mut system = INSTRUCTIONS.to_owned();
        if !self.config.instructions.is_empty() {
            system.push(' ');
            system.push_str(&self.config.instructions);
        }
        std::iter::once(json!({ "role": "system", "content": system }))
            .chain(
                self.history
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .iter()
                    .cloned(),
            )
            .collect()
    }
}

/// What starts a turn: something the user said, or news to pass on.
enum Prompt {
    Spoken {
        audio: Vec<f32>,
        rate: u32,
        earlier: String,
    },
    Announcement(String),
}

/// How a turn ended.
enum Outcome {
    Done,
    /// Interrupted before the assistant said anything: what the user said
    /// carries into their next turn, which continues the thought.
    Unanswered(String),
}

/// Run one call until `stop`, a fatal failure, or the command channel closes.
pub async fn run(
    config: Config,
    mut commands: mpsc::Receiver<Command>,
    events: mpsc::Sender<Event>,
    stop: Stop,
) -> Result<(), CallError> {
    if config.openrouter_key.trim().is_empty() {
        return Err(CallError::Provider(
            "Add an OpenRouter API key in Settings → Voice.".into(),
        ));
    }
    if config.stt.api_key.trim().is_empty() {
        return Err(CallError::Provider(format!(
            "Add a {} API key in Settings → Voice.",
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
        history: Mutex::new(Vec::new()),
        fatal: Mutex::new(None),
    });
    shared.emit(Event::Ready);

    let mut turns = turns::Turns::new(mic_rate, end_silence);
    let mut muted = false;
    let mut mic = Vec::new();
    let mut tick = tokio::time::interval(TICK);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut ticks = 0u32;
    let mut turn_number = 0u64;
    let mut announcements: Vec<String> = Vec::new();
    // The turn in flight: its stop, and its outcome when it ends.
    let mut current: Option<(Stop, oneshot::Receiver<Outcome>)> = None;
    // A turn interrupted before it answered leaves its words here.
    let mut unanswered = String::new();
    // The user may be speaking over the reply (ducked, not yet interrupted).
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
                    interrupt(&shared, &mut current);
                }
                Some(Command::Announce(text)) => announcements.push(text),
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
                // A finished turn frees the line.
                if let Some((_, done)) = &mut current {
                    match done.try_recv() {
                        Ok(outcome) => {
                            if let Outcome::Unanswered(words) = outcome {
                                unanswered = join(&unanswered, &words);
                            }
                            current = None;
                        }
                        Err(oneshot::error::TryRecvError::Closed) => current = None,
                        Err(oneshot::error::TryRecvError::Empty) => {}
                    }
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
                                // Speaking again before the reply began means
                                // the user had not finished: answer both.
                                interrupt(&shared, &mut current);
                                ducked = false;
                            }
                            turns::TurnEvent::Dropped => {
                                if ducked {
                                    ducked = false;
                                    shared.devices.set_gain(1.0);
                                }
                            }
                            turns::TurnEvent::Ended(audio) => {
                                if let Some((_, done)) = current.take() {
                                    if let Ok(Outcome::Unanswered(words)) = done.await {
                                        unanswered = join(&unanswered, &words);
                                    }
                                }
                                turn_number += 1;
                                let earlier = std::mem::take(&mut unanswered);
                                current = Some(start_turn(
                                    &shared,
                                    turn_number,
                                    Prompt::Spoken { audio, rate: mic_rate, earlier },
                                ));
                            }
                        }
                    }
                }
                // News waits for a quiet line.
                if current.is_none() && !turns.active() && !playing && !announcements.is_empty() {
                    turn_number += 1;
                    let text = announcements.drain(..).collect::<Vec<_>>().join(" ");
                    current = Some(start_turn(&shared, turn_number, Prompt::Announcement(text)));
                }
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
    if let Some((turn, _)) = current {
        turn.stop();
    }
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

/// Stop the turn in flight and silence it.
fn interrupt(shared: &Shared, current: &mut Option<(Stop, oneshot::Receiver<Outcome>)>) {
    let speaking = shared.devices.queued() > 0;
    if let Some((turn, _)) = current {
        turn.stop();
    }
    shared.devices.flush();
    shared.devices.set_gain(1.0);
    if speaking {
        shared.emit(Event::Interrupted);
    }
}

fn start_turn(
    shared: &Arc<Shared>,
    turn: u64,
    prompt: Prompt,
) -> (Stop, oneshot::Receiver<Outcome>) {
    let stop = Stop::default();
    let (done_tx, done) = oneshot::channel();
    let shared = shared.clone();
    let turn_stop = stop.clone();
    tokio::spawn(async move {
        shared.emit(Event::Thinking(true));
        let outcome = run_turn(&shared, turn, prompt, &turn_stop).await;
        shared.emit(Event::Thinking(false));
        let _ = done_tx.send(outcome);
    });
    (stop, done)
}

const ANNOUNCEMENT: &str = "(Automatic update, not spoken by the user. Tell the user briefly.)";

async fn run_turn(shared: &Arc<Shared>, turn: u64, prompt: Prompt, stop: &Stop) -> Outcome {
    let (user, automatic) = match prompt {
        Prompt::Spoken {
            audio,
            rate,
            earlier,
        } => {
            // Transcription finishes even if the user starts talking again:
            // their next turn continues this one and needs its words.
            let wav = match crate::resample::for_model(audio, rate) {
                Ok(samples) => crate::cloud::wav(&samples),
                Err(_) => return Outcome::Unanswered(earlier),
            };
            let heard = match providers::transcribe(&shared.client, &shared.config.stt, wav).await {
                Ok(text) => text,
                Err(failure) => {
                    report(shared, failure);
                    return Outcome::Unanswered(earlier);
                }
            };
            let text = join(&earlier, &heard);
            if text.is_empty() {
                return Outcome::Done;
            }
            if stop.stopped() {
                return Outcome::Unanswered(text);
            }
            shared.emit(Event::User {
                turn,
                text: text.clone(),
            });
            (text, false)
        }
        Prompt::Announcement(news) => (format!("{ANNOUNCEMENT} {news}"), true),
    };
    let mut messages = shared.messages();
    let base = messages.len();
    messages.push(json!({ "role": "user", "content": user }));
    let mut spoken = String::new();
    // Until the model answers or acts, the user's words are still open.
    let mut answered = false;
    let interrupted = run_rounds(
        shared,
        turn,
        stop,
        &mut messages,
        &mut spoken,
        &mut answered,
    )
    .await;
    if !spoken.is_empty() {
        shared.emit(Event::AssistantDone {
            turn,
            text: spoken.clone(),
        });
    }
    if !answered && !automatic {
        return Outcome::Unanswered(user);
    }
    // History keeps the exchange, tool calls included, without the system line.
    let mut exchange: Vec<Value> = messages.drain(base..).collect();
    let ends_in_text = exchange.last().is_some_and(|m| {
        m.get("role").and_then(Value::as_str) == Some("assistant") && m.get("tool_calls").is_none()
    });
    if !ends_in_text && !spoken.is_empty() {
        let mut text = spoken;
        if interrupted {
            text.push_str(" (interrupted)");
        }
        exchange.push(json!({ "role": "assistant", "content": text }));
    }
    for message in exchange {
        shared.remember(message);
    }
    Outcome::Done
}

/// Reply, run tools and reply again, until the model only talks. Returns
/// whether the turn was interrupted.
async fn run_rounds(
    shared: &Arc<Shared>,
    turn: u64,
    stop: &Stop,
    messages: &mut Vec<Value>,
    spoken: &mut String,
    answered: &mut bool,
) -> bool {
    for _ in 0..MAX_ROUNDS {
        // The voice runs beside the reply: each finished sentence is
        // synthesized while the next one streams.
        let (sentence_tx, mut sentence_rx) = mpsc::unbounded_channel::<String>();
        let voice_shared = shared.clone();
        let voice_stop = stop.clone();
        let voice = tokio::spawn(async move {
            let config = &voice_shared.config;
            while let Some(sentence) = sentence_rx.recv().await {
                if voice_stop.stopped() {
                    break;
                }
                let speaker_rate = voice_shared.devices.speaker_rate;
                let mut resampler = Resampler::new(speaker_rate);
                let result = providers::speak(
                    &voice_shared.client,
                    &config.openrouter_key,
                    &config.speech_model,
                    &config.voice,
                    &sentence,
                    |samples, rate| {
                        if !voice_stop.stopped() {
                            voice_shared.devices.play(&resampler.push(samples, rate));
                        }
                    },
                )
                .await;
                if let Err(failure) = result {
                    report(&voice_shared, failure);
                    break;
                }
            }
        });
        let mut sentences = speech::Sentences::default();
        let mut started = false;
        let mut streamed = String::new();
        let reply = {
            let reply = providers::reply(
                &shared.client,
                &shared.config.openrouter_key,
                &shared.config.reply_model,
                messages,
                &shared.config.tools,
                |delta| {
                    if !started {
                        started = true;
                        shared.emit(Event::Thinking(false));
                    }
                    streamed.push_str(delta);
                    shared.emit(Event::AssistantDelta {
                        turn,
                        text: delta.to_owned(),
                    });
                    for sentence in sentences.push(delta) {
                        let _ = sentence_tx.send(sentence);
                    }
                },
            );
            tokio::select! {
                reply = reply => Some(reply),
                _ = stop.wait() => None,
            }
        };
        let Some(reply) = reply else {
            // Talked over: keep what was said so far.
            voice.abort();
            if !streamed.trim().is_empty() {
                *answered = true;
                *spoken = join(spoken, &streamed);
            }
            return true;
        };
        if let Some(rest) = sentences.finish() {
            let _ = sentence_tx.send(rest);
        }
        drop(sentence_tx);
        let reply = match reply {
            Ok(reply) => reply,
            Err(failure) => {
                voice.abort();
                report(shared, failure);
                return false;
            }
        };
        if !reply.text.trim().is_empty() {
            *answered = true;
            *spoken = join(spoken, &reply.text);
        }
        if reply.tools.is_empty() {
            if !reply.text.trim().is_empty() {
                messages.push(json!({ "role": "assistant", "content": reply.text }));
            }
            // The last sentences finish downloading before the turn ends.
            tokio::select! {
                _ = voice => {}
                _ = stop.wait() => return true,
            }
            return false;
        }
        shared.emit(Event::Thinking(true));
        let calls: Vec<Value> = reply
            .tools
            .iter()
            .map(|t| {
                json!({
                    "id": t.id,
                    "type": "function",
                    "function": { "name": t.name, "arguments": t.arguments },
                })
            })
            .collect();
        let mut assistant = json!({ "role": "assistant", "tool_calls": calls });
        if !reply.text.is_empty() {
            assistant["content"] = json!(reply.text);
        }
        messages.push(assistant);
        // The request was understood and is being acted on.
        *answered = true;
        for tool in &reply.tools {
            let result = call_tool(shared, tool).await;
            messages.push(json!({
                "role": "tool",
                "tool_call_id": tool.id,
                "content": result,
            }));
        }
        if stop.stopped() {
            voice.abort();
            return true;
        }
        let _ = voice.await;
    }
    false
}

async fn call_tool(shared: &Shared, tool: &providers::RequestedTool) -> String {
    let arguments = serde_json::from_str(&tool.arguments).unwrap_or(Value::Null);
    let (reply, answer) = oneshot::channel();
    let call = ToolCall {
        name: tool.name.clone(),
        arguments,
        reply,
    };
    if shared.events.send(Event::Tool(call)).await.is_err() {
        return "The tool is unavailable.".into();
    }
    match tokio::time::timeout(TOOL_TIMEOUT, answer).await {
        Ok(Ok(result)) => result,
        _ => "The tool did not answer in time.".into(),
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
            reply_model: REPLY_MODELS[0].into(),
            speech_model: SPEECH_MODELS[0].id.into(),
            voice: SPEECH_MODELS[0].voices[0].into(),
            barge_in: true,
            end_silence: Duration::from_millis(800),
            input: None,
            instructions: String::new(),
            tools: Vec::new(),
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
        let error = run(config, commands_rx, events, Stop::default())
            .await
            .unwrap_err();
        assert!(error.to_string().contains("Deepgram"));
    }
}
