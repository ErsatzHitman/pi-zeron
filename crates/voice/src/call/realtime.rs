//! Real-time voice: one model hears the user, thinks and answers aloud over a
//! single WebSocket (OpenAI Realtime, Gemini Live or Qwen-Omni Realtime).
//! The coding agent still does the work: the model hands requests to it with
//! the `send_to_agent` tool ([`Event::Prompt`]), and hears the agent's
//! messages back as text ([`Command::Say`]) to tell the user what matters.
//!
//! The provider detects turns and transcribes. This device only keeps the
//! model from hearing itself: with no echo canceller, the microphone is
//! sent as silence while the model speaks unless the local detector
//! ([`super::turns`]) hears the user talking over it.
use super::{
    CallError, Command, Event, Resampler, Stop, audio,
    providers::{Failure, status_failure},
    turns,
};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use futures::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::{
    collections::HashSet,
    time::{Duration, Instant},
};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::{self, Message, client::IntoClientRequest, http::HeaderValue};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
/// Every provider here speaks 24 kHz mono PCM16.
const OUTPUT_RATE: u32 = 24_000;
/// An agent message is passed on once it has been quiet this long.
const NARRATION_SETTLE: Duration = Duration::from_millis(1200);
/// The tool the model hands work to the coding agent with.
const SEND_TO_AGENT: &str = "send_to_agent";
/// How the agent's messages read to the model.
const AGENT_PREFIX: &str = "[Coding agent]";

const INSTRUCTIONS: &str = "You are the voice of Zeron, talking with the user about their coding work. \
A coding agent does the actual work in the user's chat: it reads and edits code, runs commands and answers questions about the project. \
Whenever the user asks for anything that needs the code, files, the terminal, the web or the agent, call send_to_agent with a complete instruction in the user's own words, then say in a few words that it is on it. \
Never do that work yourself and never make up results. \
Messages that start with [Coding agent] are the agent's own updates: tell the user what matters in one or two short sentences, without reading code, paths or long lists aloud. \
Answer small talk and simple questions yourself. Keep every reply short and natural.";

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Processing {
    /// Speech to text, the coding agent, then a voice reading its messages.
    #[default]
    Pipeline,
    /// One real-time model listens, answers and relays to the agent.
    Realtime,
}

impl Processing {
    pub const ALL: [Self; 2] = [Self::Realtime, Self::Pipeline];
    pub fn label(self) -> &'static str {
        match self {
            Self::Realtime => "Real-time voice",
            Self::Pipeline => "STT + LLM + TTS",
        }
    }
    pub fn detail(self) -> &'static str {
        match self {
            Self::Realtime => "One audio model listens, answers and speaks",
            Self::Pipeline => "Transcription, your agent, then a voice",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RealtimeProvider {
    #[default]
    OpenAi,
    Gemini,
    Qwen,
}

/// A real-time model and the voices it speaks with.
pub struct RealtimeModel {
    pub id: &'static str,
    pub label: &'static str,
    pub voices: &'static [&'static str],
}

const OPENAI_VOICES: &[&str] = &[
    "marin", "cedar", "alloy", "ash", "ballad", "coral", "echo", "sage", "shimmer", "verse",
];
const GEMINI_VOICES: &[&str] = &[
    "Puck", "Charon", "Kore", "Fenrir", "Aoede", "Leda", "Orus", "Zephyr",
];

const OPENAI_MODELS: &[RealtimeModel] = &[
    RealtimeModel {
        id: "gpt-realtime-2.1",
        label: "GPT Realtime 2.1",
        voices: OPENAI_VOICES,
    },
    RealtimeModel {
        id: "gpt-realtime-2",
        label: "GPT Realtime 2",
        voices: OPENAI_VOICES,
    },
    RealtimeModel {
        id: "gpt-realtime",
        label: "GPT Realtime",
        voices: OPENAI_VOICES,
    },
    RealtimeModel {
        id: "gpt-realtime-mini",
        label: "GPT Realtime mini",
        voices: OPENAI_VOICES,
    },
];
const GEMINI_MODELS: &[RealtimeModel] = &[
    RealtimeModel {
        id: "gemini-3.1-flash-live-preview",
        label: "Gemini 3.1 Flash Live",
        voices: GEMINI_VOICES,
    },
    RealtimeModel {
        id: "gemini-2.5-flash-native-audio-preview-12-2025",
        label: "Gemini 2.5 Flash Native Audio",
        voices: GEMINI_VOICES,
    },
];
const QWEN_MODELS: &[RealtimeModel] = &[
    RealtimeModel {
        id: "qwen3.8-omni-flash-realtime",
        label: "Qwen3.8 Omni Flash",
        voices: &["Tina", "longanlingxin"],
    },
    RealtimeModel {
        id: "qwen3.5-omni-plus-realtime",
        label: "Qwen3.5 Omni Plus",
        voices: &["Tina", "Ethan", "Cherry", "Serena", "Chelsie"],
    },
    RealtimeModel {
        id: "qwen3.5-omni-flash-realtime",
        label: "Qwen3.5 Omni Flash",
        voices: &["Tina", "Ethan", "Cherry", "Serena", "Chelsie"],
    },
];

impl RealtimeProvider {
    pub const ALL: [Self; 3] = [Self::OpenAi, Self::Gemini, Self::Qwen];

    pub fn label(self) -> &'static str {
        match self {
            Self::OpenAi => "OpenAI",
            Self::Gemini => "Google Gemini",
            Self::Qwen => "Qwen (Alibaba Cloud)",
        }
    }
    /// The key's name in the key file; OpenAI's is dictation's too.
    pub fn key_name(self) -> &'static str {
        match self {
            Self::OpenAi => "openai",
            Self::Gemini => "gemini",
            Self::Qwen => "dashscope",
        }
    }
    /// What the provider calls its key.
    pub fn key_label(self) -> &'static str {
        match self {
            Self::OpenAi => "OpenAI API key",
            Self::Gemini => "Gemini API key",
            Self::Qwen => "DashScope API key",
        }
    }
    pub fn models(self) -> &'static [RealtimeModel] {
        match self {
            Self::OpenAi => OPENAI_MODELS,
            Self::Gemini => GEMINI_MODELS,
            Self::Qwen => QWEN_MODELS,
        }
    }
    /// `id`, or the provider's first model.
    pub fn model(self, id: Option<&str>) -> &'static RealtimeModel {
        let models = self.models();
        id.and_then(|id| models.iter().find(|m| m.id == id))
            .unwrap_or(&models[0])
    }
    /// The rate the provider hears the microphone at.
    fn input_rate(self) -> u32 {
        match self {
            Self::OpenAi => 24_000,
            Self::Gemini | Self::Qwen => 16_000,
        }
    }
}

/// Where Alibaba Cloud serves Qwen.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Region {
    #[default]
    International,
    China,
}

impl Region {
    pub const ALL: [Self; 2] = [Self::International, Self::China];
    pub fn label(self) -> &'static str {
        match self {
            Self::International => "International (Singapore)",
            Self::China => "China (Beijing)",
        }
    }
    fn host(self) -> &'static str {
        match self {
            Self::International => "dashscope-intl.aliyuncs.com",
            Self::China => "dashscope.aliyuncs.com",
        }
    }
}

/// How readily the model takes its turn once the user pauses.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TurnTaking {
    /// Waits through longer pauses.
    Patient,
    #[default]
    Balanced,
    /// Answers as soon as the user pauses.
    Quick,
}

impl TurnTaking {
    pub const ALL: [Self; 3] = [Self::Patient, Self::Balanced, Self::Quick];
    pub fn label(self) -> &'static str {
        match self {
            Self::Patient => "Patient",
            Self::Balanced => "Balanced",
            Self::Quick => "Quick",
        }
    }
    fn silence_ms(self) -> u32 {
        match self {
            Self::Patient => 1200,
            Self::Balanced => 800,
            Self::Quick => 500,
        }
    }
}

/// Everything a real-time call needs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Realtime {
    pub provider: RealtimeProvider,
    pub api_key: String,
    pub model: String,
    pub voice: String,
    pub region: Region,
    pub turns: TurnTaking,
}

// ---------------------------------------------------------------------------
// Wire: each provider's messages
// ---------------------------------------------------------------------------

fn request(rt: &Realtime) -> Result<tungstenite::handshake::client::Request, CallError> {
    let url = match rt.provider {
        RealtimeProvider::OpenAi => {
            format!("wss://api.openai.com/v1/realtime?model={}", rt.model)
        }
        RealtimeProvider::Gemini => format!(
            "wss://generativelanguage.googleapis.com/ws/google.ai.generativelanguage.v1beta.GenerativeService.BidiGenerateContent?key={}",
            rt.api_key.trim()
        ),
        RealtimeProvider::Qwen => format!(
            "wss://{}/api-ws/v1/realtime?model={}",
            rt.region.host(),
            rt.model
        ),
    };
    let mut request = url
        .into_client_request()
        .map_err(|_| CallError::Provider("The voice model's address is invalid.".into()))?;
    if rt.provider != RealtimeProvider::Gemini {
        let bearer = HeaderValue::from_str(&format!("Bearer {}", rt.api_key.trim()))
            .map_err(|_| key_rejected(rt.provider))?;
        request.headers_mut().insert("Authorization", bearer);
    }
    Ok(request)
}

fn key_rejected(provider: RealtimeProvider) -> CallError {
    CallError::Provider(format!(
        "The {} was rejected. Check it in Settings → Voice.",
        provider.key_label()
    ))
}

fn tool_parameters(upper: bool) -> Value {
    let (object, string) = if upper {
        ("OBJECT", "STRING")
    } else {
        ("object", "string")
    };
    json!({
        "type": object,
        "properties": {
            "prompt": {
                "type": string,
                "description": "The complete request for the coding agent, in the user's words."
            }
        },
        "required": ["prompt"]
    })
}

const TOOL_DESCRIPTION: &str = "Send a request to the coding agent working in the user's chat. Use it for anything that needs the code, files, the terminal or the agent.";

/// The messages that configure a session, sent first.
fn setup(rt: &Realtime, barge_in: bool) -> Vec<Value> {
    let silence = rt.turns.silence_ms();
    match rt.provider {
        RealtimeProvider::OpenAi => {
            let eagerness = match rt.turns {
                TurnTaking::Patient => "low",
                TurnTaking::Balanced => "medium",
                TurnTaking::Quick => "high",
            };
            vec![json!({
                "type": "session.update",
                "session": {
                    "type": "realtime",
                    "model": rt.model,
                    "output_modalities": ["audio"],
                    "instructions": INSTRUCTIONS,
                    "audio": {
                        "input": {
                            "format": {"type": "audio/pcm", "rate": 24_000},
                            "noise_reduction": {"type": "far_field"},
                            "transcription": {"model": "gpt-4o-mini-transcribe"},
                            "turn_detection": {
                                "type": "semantic_vad",
                                "eagerness": eagerness,
                                "create_response": true,
                                "interrupt_response": barge_in
                            }
                        },
                        "output": {
                            "format": {"type": "audio/pcm", "rate": OUTPUT_RATE},
                            "voice": rt.voice
                        }
                    },
                    "tools": [{
                        "type": "function",
                        "name": SEND_TO_AGENT,
                        "description": TOOL_DESCRIPTION,
                        "parameters": tool_parameters(false)
                    }],
                    "tool_choice": "auto"
                }
            })]
        }
        RealtimeProvider::Gemini => {
            let mut detection = json!({
                // Stray sounds should not start a turn.
                "startOfSpeechSensitivity": "START_SENSITIVITY_LOW",
                "silenceDurationMs": silence
            });
            detection["endOfSpeechSensitivity"] = json!(match rt.turns {
                TurnTaking::Quick => "END_SENSITIVITY_HIGH",
                _ => "END_SENSITIVITY_LOW",
            });
            vec![json!({
                "setup": {
                    "model": format!("models/{}", rt.model),
                    "generationConfig": {
                        "responseModalities": ["AUDIO"],
                        "speechConfig": {
                            "voiceConfig": {"prebuiltVoiceConfig": {"voiceName": rt.voice}}
                        }
                    },
                    "systemInstruction": {"parts": [{"text": INSTRUCTIONS}]},
                    "tools": [{
                        "functionDeclarations": [{
                            "name": SEND_TO_AGENT,
                            "description": TOOL_DESCRIPTION,
                            "parameters": tool_parameters(true)
                        }]
                    }],
                    "realtimeInputConfig": {
                        "automaticActivityDetection": detection,
                        "activityHandling": if barge_in {
                            "START_OF_ACTIVITY_INTERRUPTS"
                        } else {
                            "NO_INTERRUPTION"
                        }
                    },
                    "inputAudioTranscription": {},
                    "outputAudioTranscription": {}
                }
            })]
        }
        RealtimeProvider::Qwen => {
            let threshold = if rt.turns == TurnTaking::Quick {
                0.5
            } else {
                0.6
            };
            let turn_detection = json!({
                "type": "server_vad",
                "threshold": threshold,
                "silence_duration_ms": silence,
                "prefix_padding_ms": 300
            });
            let tools = json!([{
                "type": "function",
                "name": SEND_TO_AGENT,
                "description": TOOL_DESCRIPTION,
                "parameters": tool_parameters(false)
            }]);
            let transcription = json!({"model": "qwen3-asr-flash-realtime"});
            // Qwen3.8 nests audio settings; Qwen3.5 keeps them flat.
            let session = if rt.model.starts_with("qwen3.8") {
                json!({
                    "modalities": ["text", "audio"],
                    "instructions": INSTRUCTIONS,
                    "turn_detection": turn_detection,
                    "input_audio_transcription": transcription,
                    "audio": {
                        "input": {"format": {
                            "type": "pcm", "sample_rate": 16_000,
                            "sample_format": "s16le", "channels": 1
                        }},
                        "output": {
                            "voice": rt.voice,
                            "format": {"type": "pcm", "sample_rate": OUTPUT_RATE}
                        }
                    },
                    "tools": tools
                })
            } else {
                json!({
                    "modalities": ["text", "audio"],
                    "voice": rt.voice,
                    "input_audio_format": "pcm",
                    "output_audio_format": "pcm",
                    "instructions": INSTRUCTIONS,
                    "turn_detection": turn_detection,
                    "input_audio_transcription": transcription,
                    "tools": tools
                })
            };
            vec![json!({"type": "session.update", "session": session})]
        }
    }
}

/// Microphone audio, PCM16 at [`RealtimeProvider::input_rate`].
fn audio_message(provider: RealtimeProvider, pcm: &[u8]) -> Value {
    let data = STANDARD.encode(pcm);
    match provider {
        RealtimeProvider::Gemini => json!({
            "realtimeInput": {"audio": {"data": data, "mimeType": "audio/pcm;rate=16000"}}
        }),
        RealtimeProvider::OpenAi | RealtimeProvider::Qwen => {
            json!({"type": "input_audio_buffer.append", "audio": data})
        }
    }
}

/// Text the model should respond to (the agent's messages).
fn text_messages(provider: RealtimeProvider, text: &str) -> Vec<Value> {
    match provider {
        RealtimeProvider::Gemini => vec![json!({"realtimeInput": {"text": text}})],
        RealtimeProvider::OpenAi | RealtimeProvider::Qwen => vec![
            json!({
                "type": "conversation.item.create",
                "item": {
                    "type": "message",
                    "role": "user",
                    "content": [{"type": "input_text", "text": text}]
                }
            }),
            json!({"type": "response.create"}),
        ],
    }
}

/// The answer to a tool call, and the request to carry on talking.
fn tool_result(provider: RealtimeProvider, call: &ToolCall, output: &str) -> Vec<Value> {
    match provider {
        RealtimeProvider::Gemini => vec![json!({
            "toolResponse": {"functionResponses": [{
                "id": call.id,
                "name": call.name,
                "response": {"result": output}
            }]}
        })],
        RealtimeProvider::OpenAi | RealtimeProvider::Qwen => vec![
            json!({
                "type": "conversation.item.create",
                "item": {
                    "type": "function_call_output",
                    "call_id": call.id,
                    "output": json!({"result": output}).to_string()
                }
            }),
            json!({"type": "response.create"}),
        ],
    }
}

/// Stop the answer being generated, if the provider can.
fn cancel_message(provider: RealtimeProvider) -> Option<Value> {
    match provider {
        RealtimeProvider::OpenAi | RealtimeProvider::Qwen => {
            Some(json!({"type": "response.cancel"}))
        }
        RealtimeProvider::Gemini => None,
    }
}

#[derive(Debug, Clone, PartialEq)]
struct ToolCall {
    id: String,
    name: String,
    args: Value,
}

#[derive(Debug, PartialEq)]
enum Incoming {
    Ready,
    /// The model's voice, 24 kHz.
    Audio(Vec<f32>),
    /// More of what the model is saying in answer `item`.
    Reply {
        item: String,
        delta: String,
    },
    /// What the user said.
    Heard {
        item: String,
        text: String,
    },
    /// The user started talking: the provider interrupts its answer.
    SpeechStarted,
    Tool(ToolCall),
    Failure(Failure),
}

/// Reads one provider's server messages.
struct Parser {
    provider: RealtimeProvider,
    /// Gemini streams what it heard in pieces.
    heard: String,
    turn: u64,
    calls: HashSet<String>,
}

impl Parser {
    fn new(provider: RealtimeProvider) -> Self {
        Self {
            provider,
            heard: String::new(),
            turn: 0,
            calls: HashSet::new(),
        }
    }

    fn parse(&mut self, message: &Value) -> Vec<Incoming> {
        match self.provider {
            RealtimeProvider::Gemini => self.gemini(message),
            RealtimeProvider::OpenAi | RealtimeProvider::Qwen => self.openai(message),
        }
    }

    fn tool(&mut self, id: &str, name: &str, args: Value) -> Option<Incoming> {
        (!id.is_empty() && self.calls.insert(id.to_owned())).then(|| {
            Incoming::Tool(ToolCall {
                id: id.to_owned(),
                name: name.to_owned(),
                args,
            })
        })
    }

    /// OpenAI's events, and Qwen's dialect of them.
    fn openai(&mut self, m: &Value) -> Vec<Incoming> {
        let text = |key: &str| m[key].as_str().unwrap_or_default().to_owned();
        let arguments = |raw: &Value| {
            raw.as_str()
                .and_then(|s| serde_json::from_str(s).ok())
                .unwrap_or(Value::Null)
        };
        match m["type"].as_str().unwrap_or_default() {
            "session.created" | "session.updated" => vec![Incoming::Ready],
            "response.output_audio.delta" | "response.audio.delta" => {
                vec![Incoming::Audio(decode_pcm16(&text("delta")))]
            }
            "response.output_audio_transcript.delta" | "response.audio_transcript.delta" => {
                let item = m["item_id"]
                    .as_str()
                    .or(m["response_id"].as_str())
                    .unwrap_or("reply")
                    .to_owned();
                vec![Incoming::Reply {
                    item,
                    delta: text("delta"),
                }]
            }
            "conversation.item.input_audio_transcription.completed" => {
                let heard = text("transcript").trim().to_owned();
                if heard.is_empty() {
                    return Vec::new();
                }
                vec![Incoming::Heard {
                    item: text("item_id"),
                    text: heard,
                }]
            }
            "input_audio_buffer.speech_started" => vec![Incoming::SpeechStarted],
            "response.function_call_arguments.done" => self
                .tool(&text("call_id"), &text("name"), arguments(&m["arguments"]))
                .into_iter()
                .collect(),
            // A call whose arguments event was missed still runs once.
            "response.done" => m["response"]["output"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|item| item["type"] == "function_call")
                .filter_map(|item| {
                    self.tool(
                        item["call_id"].as_str().unwrap_or_default(),
                        item["name"].as_str().unwrap_or_default(),
                        arguments(&item["arguments"]),
                    )
                })
                .collect(),
            "error" => {
                let error = &m["error"];
                let code = error["code"].as_str().unwrap_or_default();
                // Cancelling when nothing is being said is harmless.
                if code == "response_cancel_not_active" {
                    return Vec::new();
                }
                let message = error["message"]
                    .as_str()
                    .unwrap_or("The voice model reported an error.")
                    .to_owned();
                let fatal = matches!(
                    code,
                    "invalid_api_key" | "insufficient_quota" | "model_not_found"
                ) || error["type"] == "authentication_error";
                vec![Incoming::Failure(Failure { fatal, message })]
            }
            _ => Vec::new(),
        }
    }

    fn gemini(&mut self, m: &Value) -> Vec<Incoming> {
        let mut out = Vec::new();
        if m.get("setupComplete").is_some() {
            out.push(Incoming::Ready);
        }
        let content = &m["serverContent"];
        if let Some(heard) = content["inputTranscription"]["text"].as_str() {
            self.heard.push_str(heard);
        }
        if content["interrupted"] == true {
            out.push(Incoming::SpeechStarted);
        }
        let parts = content["modelTurn"]["parts"].as_array();
        let replying = parts.is_some() || content["outputTranscription"].is_object();
        if replying || content["turnComplete"] == true {
            self.settle_heard(&mut out);
        }
        for part in parts.into_iter().flatten() {
            if let Some(data) = part["inlineData"]["data"].as_str() {
                out.push(Incoming::Audio(decode_pcm16(data)));
            }
        }
        if let Some(said) = content["outputTranscription"]["text"].as_str() {
            out.push(Incoming::Reply {
                item: format!("turn-{}", self.turn),
                delta: said.to_owned(),
            });
        }
        if content["turnComplete"] == true {
            self.turn += 1;
        }
        for call in m["toolCall"]["functionCalls"]
            .as_array()
            .into_iter()
            .flatten()
        {
            out.extend(self.tool(
                call["id"].as_str().unwrap_or_default(),
                call["name"].as_str().unwrap_or_default(),
                call["args"].clone(),
            ));
        }
        if m.get("goAway").is_some() {
            out.push(Incoming::Failure(Failure {
                fatal: false,
                message:
                    "Gemini will end this voice session soon. Start a new one to keep talking."
                        .into(),
            }));
        }
        out
    }

    fn settle_heard(&mut self, out: &mut Vec<Incoming>) {
        let heard = std::mem::take(&mut self.heard);
        let heard = heard.trim();
        if !heard.is_empty() {
            out.push(Incoming::Heard {
                item: format!("turn-{}", self.turn),
                text: heard.to_owned(),
            });
        }
    }
}

fn decode_pcm16(data: &str) -> Vec<f32> {
    let Ok(bytes) = STANDARD.decode(data.trim()) else {
        return Vec::new();
    };
    bytes
        .chunks_exact(2)
        .map(|pair| f32::from(i16::from_le_bytes([pair[0], pair[1]])) / 32_768.0)
        .collect()
}

fn encode_pcm16(samples: &[f32]) -> Vec<u8> {
    samples
        .iter()
        .flat_map(|s| {
            let s = if s.is_finite() { *s } else { 0.0 };
            ((s.clamp(-1.0, 1.0) * 32_767.0) as i16).to_le_bytes()
        })
        .collect()
}

// ---------------------------------------------------------------------------
// The call
// ---------------------------------------------------------------------------

/// Run one real-time call until `stop`, a fatal failure or the command
/// channel closes. Same contract as [`super::run`].
pub(super) async fn run(
    rt: Realtime,
    input: Option<String>,
    barge_in: bool,
    mut commands: mpsc::Receiver<Command>,
    events: mpsc::Sender<Event>,
    stop: Stop,
) -> Result<(), CallError> {
    if rt.api_key.trim().is_empty() {
        return Err(CallError::Provider(format!(
            "Add a {} in Settings → Voice.",
            rt.provider.key_label()
        )));
    }
    let emit = |event: Event| {
        let _ = events.try_send(event);
    };
    let devices = tokio::task::spawn_blocking(move || audio::Devices::open(input))
        .await
        .map_err(|_| CallError::Devices("The audio devices did not open.".into()))?
        .map_err(|e| CallError::Devices(format!("{e}. Check your microphone and speakers.")))?;
    let request = request(&rt)?;
    let label = rt.provider.label();
    let (socket, _) = match tokio::time::timeout(
        CONNECT_TIMEOUT,
        tokio_tungstenite::connect_async(request),
    )
    .await
    {
        Err(_) => {
            return Err(CallError::Provider(format!(
                "{label} did not answer. Check your connection and try again."
            )));
        }
        Ok(Err(tungstenite::Error::Http(response))) => {
            return Err(CallError::Provider(
                status_failure(label, response.status().as_u16()).message,
            ));
        }
        Ok(Err(error)) => {
            return Err(CallError::Provider(format!(
                "Could not reach {label}: {error}"
            )));
        }
        Ok(Ok(socket)) => socket,
    };
    let (mut sink, mut stream) = socket.split();
    let frame_of = |value: Value| Message::Text(value.to_string());
    for message in setup(&rt, barge_in) {
        sink.send(frame_of(message))
            .await
            .map_err(|e| CallError::Provider(format!("Could not reach {label}: {e}")))?;
    }

    let provider = rt.provider;
    let mic_rate = devices.mic_rate;
    let mut parser = Parser::new(provider);
    let mut up = Resampler::new(provider.input_rate());
    let mut down = Resampler::new(devices.speaker_rate);
    // Only decides when the user talks over the model; the provider takes turns.
    let mut local = turns::Turns::new(mic_rate, Duration::from_millis(800));
    let mut ready = false;
    let mut muted = false;
    let mut prompts = 0u64;
    let mut narration: Option<(String, String, Instant)> = None;
    let mut mic = Vec::new();
    let mut tick = tokio::time::interval(super::TICK);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut ticks = 0u32;

    let result = loop {
        let mut outgoing: Vec<Value> = Vec::new();
        tokio::select! {
            biased;
            _ = stop.wait() => break Ok(()),
            command = commands.recv() => match command {
                None => break Ok(()),
                Some(Command::Mute(value)) => {
                    muted = value;
                    local.reset();
                }
                Some(Command::Interrupt) => {
                    devices.flush();
                    outgoing.extend(cancel_message(provider));
                    emit(Event::Interrupted);
                }
                Some(Command::Say { item, text }) => {
                    match &mut narration {
                        Some((current, buffer, at)) if *current == item => {
                            buffer.push(' ');
                            buffer.push_str(&text);
                            *at = Instant::now();
                        }
                        _ => {
                            if let Some((item, buffer, _)) = narration.take() {
                                outgoing.extend(narrate(provider, &item, &buffer));
                            }
                            narration = Some((item, text, Instant::now()));
                        }
                    }
                }
            },
            frame = stream.next() => {
                let message = match frame {
                    None => break Err(CallError::Provider(format!("{label} closed the voice session."))),
                    Some(Err(error)) => break Err(CallError::Provider(format!("The connection to {label} failed: {error}"))),
                    Some(Ok(message)) => message,
                };
                let value: Option<Value> = match message {
                    Message::Text(text) => serde_json::from_str(&text).ok(),
                    Message::Binary(bytes) => serde_json::from_slice(&bytes).ok(),
                    Message::Close(frame) => {
                        let reason = frame.map(|f| f.reason.to_string()).unwrap_or_default();
                        break Err(CallError::Provider(if reason.trim().is_empty() {
                            format!("{label} closed the voice session.")
                        } else {
                            format!("{label} closed the voice session: {reason}")
                        }));
                    }
                    _ => None,
                };
                let Some(value) = value else { continue };
                let mut fatal = None;
                for incoming in parser.parse(&value) {
                    match incoming {
                        Incoming::Ready => {
                            if !std::mem::replace(&mut ready, true) {
                                emit(Event::Ready);
                            }
                        }
                        Incoming::Audio(samples) => {
                            devices.play(&down.push(&samples, OUTPUT_RATE));
                        }
                        Incoming::Reply { item, delta } => emit(Event::Reply { item, delta }),
                        Incoming::Heard { item, text } => emit(Event::Heard { item, text }),
                        Incoming::SpeechStarted => {
                            if barge_in && devices.queued() > 0 {
                                devices.flush();
                                emit(Event::Interrupted);
                            }
                        }
                        Incoming::Tool(call) => {
                            let prompt = call.args["prompt"].as_str().unwrap_or_default().trim().to_owned();
                            let output = if call.name != SEND_TO_AGENT {
                                format!("There is no tool named {}.", call.name)
                            } else if prompt.is_empty() {
                                "Nothing was sent: give the agent a prompt.".to_owned()
                            } else {
                                prompts += 1;
                                // A prompt must not be dropped: wait for room.
                                let _ = events.send(Event::Prompt { turn: prompts, text: prompt }).await;
                                format!("Sent to the coding agent. Its updates will arrive as messages starting with {AGENT_PREFIX}.")
                            };
                            outgoing.extend(tool_result(provider, &call, &output));
                        }
                        Incoming::Failure(failure) if failure.fatal => {
                            fatal = Some(failure.message);
                        }
                        Incoming::Failure(failure) => emit(Event::Notice(failure.message)),
                    }
                }
                if let Some(message) = fatal {
                    break Err(CallError::Provider(message));
                }
            }
            _ = tick.tick() => {
                if devices.failed() {
                    break Err(CallError::Devices("The microphone or speaker disconnected.".into()));
                }
                mic.clear();
                devices.take_mic(&mut mic);
                let playing = devices.queued() > 0;
                if ready && !muted && !mic.is_empty() {
                    let mut talked_over = false;
                    for event in local.push(&mic, playing) {
                        if event == turns::TurnEvent::Confirmed && playing {
                            talked_over = true;
                        }
                    }
                    if talked_over && barge_in {
                        devices.flush();
                        outgoing.extend(cancel_message(provider));
                        emit(Event::Interrupted);
                    }
                    // The model must not hear itself through the speakers.
                    let forward = !playing || (barge_in && local.active());
                    if !forward {
                        mic.iter_mut().for_each(|s| *s = 0.0);
                    }
                    let samples = up.push(&mic, mic_rate);
                    if !samples.is_empty() {
                        outgoing.push(audio_message(provider, &encode_pcm16(&samples)));
                    }
                }
                if let Some((_, _, at)) = &narration
                    && at.elapsed() >= NARRATION_SETTLE
                    && let Some((item, buffer, _)) = narration.take()
                {
                    outgoing.extend(narrate(provider, &item, &buffer));
                }
                ticks = ticks.wrapping_add(1);
                if ticks.is_multiple_of(super::LEVEL_EVERY) {
                    let (microphone, speaker) = devices.take_levels();
                    emit(Event::Levels {
                        microphone: if muted { 0.0 } else { microphone },
                        speaker,
                    });
                }
            }
        }
        let mut failed = None;
        for message in outgoing {
            if let Err(error) = sink.send(frame_of(message)).await {
                failed = Some(error);
                break;
            }
        }
        if let Some(error) = failed {
            break Err(CallError::Provider(format!(
                "The connection to {label} failed: {error}"
            )));
        }
    };
    let _ = sink.send(Message::Close(None)).await;
    devices.flush();
    result
}

/// An agent message (or a notice) for the model to pass on.
fn narrate(provider: RealtimeProvider, item: &str, text: &str) -> Vec<Value> {
    let text = text.trim();
    if text.is_empty() {
        return Vec::new();
    }
    let prefix = if item.starts_with("notice-") {
        "[Zeron notice]"
    } else {
        AGENT_PREFIX
    };
    text_messages(provider, &format!("{prefix} {text}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(provider: RealtimeProvider) -> Realtime {
        let model = provider.models()[0];
        Realtime {
            provider,
            api_key: "key".into(),
            model: model.id.into(),
            voice: model.voices[0].into(),
            region: Region::International,
            turns: TurnTaking::Balanced,
        }
    }

    #[test]
    fn every_provider_offers_models_with_voices() {
        for provider in RealtimeProvider::ALL {
            assert!(!provider.models().is_empty());
            for model in provider.models() {
                assert!(!model.voices.is_empty(), "{}", model.id);
            }
            assert_eq!(provider.model(Some("nope")).id, provider.models()[0].id);
        }
    }

    #[test]
    fn each_session_offers_the_agent_tool() {
        let openai = &setup(&config(RealtimeProvider::OpenAi), true)[0];
        assert_eq!(openai["session"]["tools"][0]["name"], SEND_TO_AGENT);
        assert_eq!(openai["session"]["audio"]["output"]["voice"], "marin");
        assert_eq!(
            openai["session"]["audio"]["input"]["turn_detection"]["eagerness"],
            "medium"
        );
        let gemini = &setup(&config(RealtimeProvider::Gemini), false)[0];
        assert_eq!(
            gemini["setup"]["tools"][0]["functionDeclarations"][0]["name"],
            SEND_TO_AGENT
        );
        assert_eq!(
            gemini["setup"]["realtimeInputConfig"]["activityHandling"],
            "NO_INTERRUPTION"
        );
        assert_eq!(
            gemini["setup"]["model"],
            "models/gemini-3.1-flash-live-preview"
        );
        let qwen = &setup(&config(RealtimeProvider::Qwen), true)[0];
        assert_eq!(qwen["session"]["tools"][0]["name"], SEND_TO_AGENT);
        assert_eq!(qwen["session"]["audio"]["output"]["voice"], "Tina");
        let mut flat = config(RealtimeProvider::Qwen);
        flat.model = "qwen3.5-omni-flash-realtime".into();
        assert_eq!(setup(&flat, true)[0]["session"]["voice"], "Tina");
    }

    #[test]
    fn requests_carry_the_key_where_each_provider_wants_it() {
        let openai = request(&config(RealtimeProvider::OpenAi)).unwrap();
        assert_eq!(openai.headers()["Authorization"], "Bearer key");
        assert!(openai.uri().to_string().contains("model=gpt-realtime-2.1"));
        let gemini = request(&config(RealtimeProvider::Gemini)).unwrap();
        assert!(gemini.uri().to_string().ends_with("key=key"));
        assert!(gemini.headers().get("Authorization").is_none());
        let mut qwen = config(RealtimeProvider::Qwen);
        qwen.region = Region::China;
        let qwen = request(&qwen).unwrap();
        assert!(
            qwen.uri()
                .to_string()
                .starts_with("wss://dashscope.aliyuncs.com/")
        );
    }

    #[test]
    fn openai_events_become_audio_captions_and_one_tool_call() {
        let mut parser = Parser::new(RealtimeProvider::OpenAi);
        let audio = STANDARD.encode(encode_pcm16(&[0.5, -0.5]));
        let events = parser.parse(&json!({"type": "response.output_audio.delta", "delta": audio}));
        let [Incoming::Audio(samples)] = events.as_slice() else {
            panic!("{events:?}")
        };
        assert!((samples[0] - 0.5).abs() < 1e-3 && (samples[1] + 0.5).abs() < 1e-3);
        assert_eq!(
            parser.parse(
                &json!({"type": "conversation.item.input_audio_transcription.completed",
                "item_id": "i1", "transcript": " run the tests "})
            ),
            [Incoming::Heard {
                item: "i1".into(),
                text: "run the tests".into()
            }]
        );
        let call = json!({"type": "response.function_call_arguments.done", "call_id": "c1",
            "name": SEND_TO_AGENT, "arguments": "{\"prompt\":\"run the tests\"}"});
        let events = parser.parse(&call);
        let [Incoming::Tool(tool)] = events.as_slice() else {
            panic!("{events:?}")
        };
        assert_eq!(tool.args["prompt"], "run the tests");
        // The same call echoed in response.done runs once.
        let done = json!({"type": "response.done", "response": {"output": [
            {"type": "function_call", "call_id": "c1", "name": SEND_TO_AGENT, "arguments": "{}"}]}});
        assert!(parser.parse(&done).is_empty());
        assert!(
            parser
                .parse(&json!({"type": "error", "error": {"code": "response_cancel_not_active"}}))
                .is_empty()
        );
        let events = parser.parse(
            &json!({"type": "error", "error": {"code": "invalid_api_key", "message": "bad"}}),
        );
        let [Incoming::Failure(failure)] = events.as_slice() else {
            panic!("{events:?}")
        };
        assert!(failure.fatal);
    }

    #[test]
    fn gemini_settles_what_it_heard_before_it_answers() {
        let mut parser = Parser::new(RealtimeProvider::Gemini);
        assert_eq!(
            parser.parse(&json!({"setupComplete": {}})),
            [Incoming::Ready]
        );
        assert!(
            parser
                .parse(&json!({"serverContent": {"inputTranscription": {"text": "run the"}}}))
                .is_empty()
        );
        parser.parse(&json!({"serverContent": {"inputTranscription": {"text": " tests"}}}));
        let events =
            parser.parse(&json!({"serverContent": {"outputTranscription": {"text": "On it."}}}));
        assert_eq!(
            events,
            [
                Incoming::Heard {
                    item: "turn-0".into(),
                    text: "run the tests".into()
                },
                Incoming::Reply {
                    item: "turn-0".into(),
                    delta: "On it.".into()
                }
            ]
        );
        let events = parser.parse(&json!({"toolCall": {"functionCalls": [
            {"id": "f1", "name": SEND_TO_AGENT, "args": {"prompt": "run the tests"}}]}}));
        let [Incoming::Tool(call)] = events.as_slice() else {
            panic!("{events:?}")
        };
        assert_eq!(
            tool_result(RealtimeProvider::Gemini, call, "ok")[0]["toolResponse"]["functionResponses"]
                [0]["id"],
            "f1"
        );
        assert_eq!(
            parser.parse(&json!({"serverContent": {"interrupted": true}})),
            [Incoming::SpeechStarted]
        );
    }

    #[test]
    fn agent_messages_reach_the_model_as_text() {
        let messages = narrate(RealtimeProvider::OpenAi, "m1", " Tests pass. ");
        assert_eq!(
            messages[0]["item"]["content"][0]["text"],
            "[Coding agent] Tests pass."
        );
        assert_eq!(messages[1]["type"], "response.create");
        let gemini = narrate(RealtimeProvider::Gemini, "notice-1", "Offline");
        assert_eq!(gemini[0]["realtimeInput"]["text"], "[Zeron notice] Offline");
        assert!(narrate(RealtimeProvider::Qwen, "m1", "  ").is_empty());
    }
}
