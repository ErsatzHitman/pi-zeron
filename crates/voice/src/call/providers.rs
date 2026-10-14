//! The three cloud stages of a call: speech-to-text (OpenRouter, Deepgram or
//! Mistral), the reply (OpenRouter chat, streamed) and its voice (OpenRouter
//! speech, streamed as PCM).
use serde_json::{Value, json};
use std::time::Duration;

const OPENROUTER: &str = "https://openrouter.ai/api/v1";
const TRANSCRIBE_TIMEOUT: Duration = Duration::from_secs(30);
const REPLY_TIMEOUT: Duration = Duration::from_secs(60);
const SPEECH_TIMEOUT: Duration = Duration::from_secs(45);
/// OpenRouter's PCM is 24 kHz mono s16le unless its content type says otherwise.
const DEFAULT_SPEECH_RATE: u32 = 24_000;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SttProvider {
    #[default]
    OpenRouter,
    Deepgram,
    Mistral,
}

impl SttProvider {
    pub const ALL: [Self; 3] = [Self::OpenRouter, Self::Deepgram, Self::Mistral];

    pub fn label(self) -> &'static str {
        match self {
            Self::OpenRouter => "OpenRouter",
            Self::Deepgram => "Deepgram",
            Self::Mistral => "Mistral",
        }
    }
    /// The key's name in the key file; OpenRouter shares the reply's key.
    pub fn key_name(self) -> &'static str {
        match self {
            Self::OpenRouter => OPENROUTER_KEY,
            Self::Deepgram => "deepgram",
            Self::Mistral => "mistral",
        }
    }
    pub fn models(self) -> &'static [&'static str] {
        match self {
            Self::OpenRouter => &[
                "openai/whisper-large-v3-turbo",
                "openai/gpt-4o-mini-transcribe",
                "mistralai/voxtral-mini-transcribe",
                "deepgram/nova-3",
            ],
            Self::Deepgram => &["nova-3", "nova-2"],
            Self::Mistral => &["voxtral-mini-latest"],
        }
    }
    pub fn default_model(self) -> &'static str {
        self.models()[0]
    }
}

/// The key file's name for the OpenRouter key (replies and voices).
pub const OPENROUTER_KEY: &str = "openrouter";

/// Fast chat models that call tools, cheapest first.
pub const REPLY_MODELS: &[&str] = &[
    "google/gemini-2.5-flash-lite",
    "openai/gpt-4.1-nano",
    "openai/gpt-oss-120b",
    "mistralai/mistral-small-3.2-24b-instruct",
    "anthropic/claude-haiku-5.5",
    "openai/gpt-4.1-mini",
    "meta-llama/llama-3.3-70b-instruct",
];

/// A text-to-speech model OpenRouter serves as PCM, and its voices.
pub struct SpeechModel {
    pub id: &'static str,
    pub label: &'static str,
    pub voices: &'static [&'static str],
}

pub const SPEECH_MODELS: &[SpeechModel] = &[
    SpeechModel {
        id: "hexgrad/kokoro-82m",
        label: "Kokoro 82M",
        voices: &[
            "af_heart",
            "af_bella",
            "af_nicole",
            "af_sarah",
            "af_sky",
            "af_nova",
            "am_adam",
            "am_michael",
            "am_echo",
            "am_eric",
            "am_liam",
            "am_onyx",
            "bf_emma",
            "bf_isabella",
            "bm_george",
            "bm_lewis",
            "hf_alpha",
            "hf_beta",
            "hm_omega",
            "hm_psi",
        ],
    },
    SpeechModel {
        id: "deepgram/aura-2",
        label: "Deepgram Aura 2",
        voices: &[
            "aura-2-thalia-en",
            "aura-2-asteria-en",
            "aura-2-luna-en",
            "aura-2-orion-en",
            "aura-2-arcas-en",
            "aura-2-apollo-en",
        ],
    },
];

pub fn speech_model(id: Option<&str>) -> &'static SpeechModel {
    id.and_then(|id| SPEECH_MODELS.iter().find(|m| m.id == id))
        .unwrap_or(&SPEECH_MODELS[0])
}

/// A failed request. `fatal` failures end the call: a rejected key or no
/// credit will not fix itself on the next turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failure {
    pub fatal: bool,
    pub message: String,
}

impl Failure {
    fn new(fatal: bool, message: impl Into<String>) -> Self {
        Self {
            fatal,
            message: message.into(),
        }
    }
}

pub(crate) fn status_failure(label: &str, status: u16) -> Failure {
    match status {
        401 | 403 => Failure::new(
            true,
            format!("The {label} API key was rejected. Check it in Settings → Voice."),
        ),
        402 => Failure::new(
            true,
            format!("Your {label} account is out of credit. Add credit, then start voice again."),
        ),
        429 => Failure::new(
            false,
            format!("{label} is rate limiting requests. Try again in a moment."),
        ),
        _ => Failure::new(false, format!("{label} could not answer (error {status}).")),
    }
}

fn send_failure(label: &str, e: reqwest::Error) -> Failure {
    Failure::new(
        false,
        if e.is_timeout() {
            format!("{label} took too long to answer.")
        } else {
            format!("Could not reach {label}. Check your connection.")
        },
    )
}

pub(crate) fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .build()
        .unwrap_or_default()
}

/// Speech-to-text settings for one call.
#[derive(Clone, Debug, Default)]
pub struct Stt {
    pub provider: SttProvider,
    pub api_key: String,
    pub model: Option<String>,
    /// ISO-639-1; `None` detects the language.
    pub language: Option<String>,
}

impl Stt {
    fn model(&self) -> &str {
        self.model
            .as_deref()
            .filter(|m| self.provider.models().contains(m))
            .unwrap_or(self.provider.default_model())
    }
}

/// Transcribe one turn (a 16 kHz mono WAV).
pub(crate) async fn transcribe(
    client: &reqwest::Client,
    stt: &Stt,
    wav: Vec<u8>,
) -> Result<String, Failure> {
    let label = stt.provider.label();
    let key = stt.api_key.trim();
    let request = match stt.provider {
        SttProvider::OpenRouter | SttProvider::Mistral => {
            let base = if stt.provider == SttProvider::Mistral {
                "https://api.mistral.ai/v1"
            } else {
                OPENROUTER
            };
            let mut fields = vec![("model", stt.model())];
            if stt.provider == SttProvider::OpenRouter {
                fields.push(("response_format", "json"));
            }
            fields.extend(stt.language.as_deref().map(|l| ("language", l)));
            client
                .post(format!("{base}/audio/transcriptions"))
                .bearer_auth(key)
                .header(
                    reqwest::header::CONTENT_TYPE,
                    format!("multipart/form-data; boundary={}", crate::cloud::BOUNDARY),
                )
                .body(crate::cloud::form(&fields, &wav))
        }
        SttProvider::Deepgram => client
            .post("https://api.deepgram.com/v1/listen")
            .query(&deepgram_query(stt.model(), stt.language.as_deref()))
            .header(reqwest::header::AUTHORIZATION, format!("Token {key}"))
            .header(reqwest::header::CONTENT_TYPE, "audio/wav")
            .body(wav),
    };
    let response = request
        .timeout(TRANSCRIBE_TIMEOUT)
        .send()
        .await
        .map_err(|e| send_failure(label, e))?;
    let status = response.status();
    if !status.is_success() {
        return Err(status_failure(label, status.as_u16()));
    }
    let body: Value = response.json().await.map_err(|_| {
        Failure::new(
            false,
            format!("{label} sent an answer Zeron could not read."),
        )
    })?;
    transcript_of(stt.provider, &body).ok_or_else(|| {
        Failure::new(
            false,
            format!("{label} sent an answer Zeron could not read."),
        )
    })
}

fn deepgram_query(model: &str, language: Option<&str>) -> Vec<(&'static str, String)> {
    let mut query = vec![
        ("model", model.to_owned()),
        ("smart_format", "true".to_owned()),
    ];
    match language {
        Some(language) => query.push(("language", language.to_owned())),
        None => query.push(("detect_language", "true".to_owned())),
    }
    query
}

fn transcript_of(provider: SttProvider, body: &Value) -> Option<String> {
    let text = match provider {
        SttProvider::Deepgram => body.pointer("/results/channels/0/alternatives/0/transcript")?,
        SttProvider::OpenRouter | SttProvider::Mistral => body.get("text")?,
    };
    Some(text.as_str()?.trim().to_owned())
}

/// A tool the reply model may call, in OpenAI function form.
#[derive(Clone, Debug)]
pub struct Tool {
    pub name: String,
    pub description: String,
    pub parameters: Value,
}

/// A tool call the reply asked for; `arguments` is the model's JSON text.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RequestedTool {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

/// What a streamed reply ended with.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Reply {
    pub text: String,
    pub tools: Vec<RequestedTool>,
}

/// Stream a reply, handing each text delta to `on_text` as it arrives.
pub(crate) async fn reply(
    client: &reqwest::Client,
    key: &str,
    model: &str,
    messages: &[Value],
    tools: &[Tool],
    mut on_text: impl FnMut(&str),
) -> Result<Reply, Failure> {
    let mut body = json!({
        "model": model,
        "messages": messages,
        "stream": true,
        "temperature": 0.6,
        "max_tokens": 600,
    });
    if !tools.is_empty() {
        body["tools"] = tools.iter().map(tool_json).collect();
    }
    let mut response = client
        .post(format!("{OPENROUTER}/chat/completions"))
        .bearer_auth(key.trim())
        .header("X-Title", "Zeron")
        .json(&body)
        .timeout(REPLY_TIMEOUT)
        .send()
        .await
        .map_err(|e| send_failure("OpenRouter", e))?;
    let status = response.status();
    if !status.is_success() {
        return Err(status_failure("OpenRouter", status.as_u16()));
    }
    let mut stream = ReplyStream::default();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|e| send_failure("OpenRouter", e))?
    {
        if stream.feed(&chunk, &mut on_text)? {
            break;
        }
    }
    Ok(stream.finish())
}

/// A tool as the chat API declares it. A tool without arguments leaves its
/// schema out: Gemini rejects an object schema with no properties.
fn tool_json(tool: &Tool) -> Value {
    let mut function = json!({
        "name": tool.name,
        "description": tool.description,
    });
    let empty = tool
        .parameters
        .get("properties")
        .and_then(Value::as_object)
        .is_none_or(|p| p.is_empty());
    if !empty {
        function["parameters"] = tool.parameters.clone();
    }
    json!({ "type": "function", "function": function })
}

/// Server-sent reply events, parsed as they arrive.
#[derive(Default)]
pub(crate) struct ReplyStream {
    line: Vec<u8>,
    reply: Reply,
}

impl ReplyStream {
    /// Feed bytes; `Ok(true)` once the stream said `[DONE]`.
    pub fn feed(&mut self, bytes: &[u8], on_text: &mut impl FnMut(&str)) -> Result<bool, Failure> {
        for &b in bytes {
            if b != b'\n' {
                self.line.push(b);
                continue;
            }
            let line = std::mem::take(&mut self.line);
            let line = String::from_utf8_lossy(&line);
            let line = line.trim_end_matches('\r');
            // Comments (`: OPENROUTER PROCESSING`) keep the connection alive.
            let Some(data) = line.strip_prefix("data:") else {
                continue;
            };
            let data = data.trim();
            if data == "[DONE]" {
                return Ok(true);
            }
            let Ok(event) = serde_json::from_str::<Value>(data) else {
                continue;
            };
            if let Some(error) = event.get("error") {
                let message = error
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown error");
                return Err(Failure::new(
                    false,
                    format!("OpenRouter stopped: {message}"),
                ));
            }
            let Some(delta) = event.pointer("/choices/0/delta") else {
                continue;
            };
            if let Some(text) = delta.get("content").and_then(Value::as_str)
                && !text.is_empty()
            {
                self.reply.text.push_str(text);
                on_text(text);
            }
            for call in delta
                .get("tool_calls")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                let index = call.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
                if index >= 16 {
                    continue;
                }
                while self.reply.tools.len() <= index {
                    self.reply.tools.push(RequestedTool::default());
                }
                let tool = &mut self.reply.tools[index];
                if let Some(id) = call.get("id").and_then(Value::as_str) {
                    tool.id = id.to_owned();
                }
                if let Some(name) = call.pointer("/function/name").and_then(Value::as_str) {
                    tool.name.push_str(name);
                }
                if let Some(arguments) = call.pointer("/function/arguments").and_then(Value::as_str)
                {
                    tool.arguments.push_str(arguments);
                }
            }
        }
        Ok(false)
    }

    pub fn finish(mut self) -> Reply {
        self.reply.tools.retain(|t| !t.name.is_empty());
        for (i, tool) in self.reply.tools.iter_mut().enumerate() {
            if tool.id.is_empty() {
                tool.id = format!("call_{i}");
            }
        }
        self.reply
    }
}

/// Synthesize `text`, handing mono `f32` samples at the returned rate to
/// `on_audio` as they stream in.
pub(crate) async fn speak(
    client: &reqwest::Client,
    key: &str,
    model: &str,
    voice: &str,
    text: &str,
    mut on_audio: impl FnMut(&[f32], u32),
) -> Result<(), Failure> {
    let mut response = client
        .post(format!("{OPENROUTER}/audio/speech"))
        .bearer_auth(key.trim())
        .header("X-Title", "Zeron")
        .json(&json!({
            "model": model,
            "input": text,
            "voice": voice,
            "response_format": "pcm",
        }))
        .timeout(SPEECH_TIMEOUT)
        .send()
        .await
        .map_err(|e| send_failure("OpenRouter", e))?;
    let status = response.status();
    if !status.is_success() {
        return Err(status_failure("OpenRouter", status.as_u16()));
    }
    let rate = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .and_then(pcm_rate)
        .unwrap_or(DEFAULT_SPEECH_RATE);
    let mut pcm = Pcm::default();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|e| send_failure("OpenRouter", e))?
    {
        let samples = pcm.decode(&chunk);
        if !samples.is_empty() {
            on_audio(&samples, rate);
        }
    }
    Ok(())
}

/// `rate=` from a PCM content type such as `audio/pcm;rate=24000`.
fn pcm_rate(content_type: &str) -> Option<u32> {
    content_type.split(';').find_map(|part| {
        let (name, value) = part.split_once('=')?;
        matches!(name.trim(), "rate" | "sample_rate" | "samplerate")
            .then(|| value.trim().parse().ok())
            .flatten()
            .filter(|rate| (8_000..=96_000).contains(rate))
    })
}

/// s16le bytes to samples, carrying an odd byte across chunks.
#[derive(Default)]
pub(crate) struct Pcm {
    carry: Option<u8>,
}

impl Pcm {
    pub fn decode(&mut self, bytes: &[u8]) -> Vec<f32> {
        let mut out = Vec::with_capacity(bytes.len() / 2 + 1);
        let mut bytes = bytes.iter().copied();
        if let Some(low) = self.carry.take() {
            match bytes.next() {
                Some(high) => out.push(i16::from_le_bytes([low, high]) as f32 / 32_768.0),
                None => {
                    self.carry = Some(low);
                    return out;
                }
            }
        }
        loop {
            match (bytes.next(), bytes.next()) {
                (Some(low), Some(high)) => {
                    out.push(i16::from_le_bytes([low, high]) as f32 / 32_768.0)
                }
                (Some(low), None) => {
                    self.carry = Some(low);
                    break;
                }
                _ => break,
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tools_without_arguments_leave_their_schema_out() {
        let tool = |parameters: Value| Tool {
            name: "agent_status".into(),
            description: "Status.".into(),
            parameters,
        };
        let bare = tool_json(&tool(json!({ "type": "object", "properties": {} })));
        assert_eq!(bare["type"], "function");
        assert_eq!(bare["function"]["name"], "agent_status");
        assert!(bare["function"].get("parameters").is_none());
        let schema = json!({
            "type": "object",
            "properties": { "message": { "type": "string" } },
            "required": ["message"],
        });
        let full = tool_json(&tool(schema.clone()));
        assert_eq!(full["function"]["parameters"], schema);
    }

    #[test]
    fn transcripts_are_read_from_each_provider() {
        let whisper = json!({ "text": " Open the tests. ", "usage": { "seconds": 2 } });
        assert_eq!(
            transcript_of(SttProvider::OpenRouter, &whisper).as_deref(),
            Some("Open the tests.")
        );
        assert_eq!(
            transcript_of(
                SttProvider::Mistral,
                &json!({ "model": "voxtral", "text": "Hi" })
            )
            .as_deref(),
            Some("Hi")
        );
        let deepgram = json!({
            "results": { "channels": [{ "alternatives": [{ "transcript": "Run it.", "confidence": 0.9 }] }] }
        });
        assert_eq!(
            transcript_of(SttProvider::Deepgram, &deepgram).as_deref(),
            Some("Run it.")
        );
        assert_eq!(transcript_of(SttProvider::Deepgram, &whisper), None);
    }

    #[test]
    fn deepgram_detects_the_language_unless_one_is_set() {
        let auto = deepgram_query("nova-3", None);
        assert!(auto.contains(&("detect_language", "true".into())));
        let hindi = deepgram_query("nova-3", Some("hi"));
        assert!(hindi.contains(&("language", "hi".into())));
        assert!(!hindi.iter().any(|(k, _)| *k == "detect_language"));
    }

    #[test]
    fn unknown_models_fall_back_and_keys_are_shared() {
        let stt = Stt {
            provider: SttProvider::Deepgram,
            model: Some("openai/whisper-large-v3-turbo".into()),
            ..Stt::default()
        };
        assert_eq!(stt.model(), "nova-3");
        assert_eq!(SttProvider::OpenRouter.key_name(), OPENROUTER_KEY);
        assert_eq!(speech_model(Some("nope")).id, "hexgrad/kokoro-82m");
        assert_eq!(
            speech_model(Some("deepgram/aura-2")).voices[0],
            "aura-2-thalia-en"
        );
    }

    #[test]
    fn failures_say_what_to_fix_and_which_end_the_call() {
        assert!(status_failure("OpenRouter", 401).fatal);
        assert!(status_failure("OpenRouter", 402).message.contains("credit"));
        let limited = status_failure("Deepgram", 429);
        assert!(!limited.fatal);
        assert!(limited.message.contains("rate limiting"));
    }

    #[test]
    fn streamed_text_and_tool_calls_are_assembled() {
        let mut stream = ReplyStream::default();
        let mut seen = String::new();
        let events = concat!(
            ": OPENROUTER PROCESSING\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"On \"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"it.\"}}]}\r\n\r\n",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",",
            "\"function\":{\"name\":\"ask_agent\",\"arguments\":\"{\\\"mess\"}}]}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,",
            "\"function\":{\"arguments\":\"age\\\":\\\"run tests\\\"}\"}}]}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}],\"usage\":{}}\n\n",
            "data: [DONE]\n\n",
        );
        // Split mid-line to prove chunk boundaries do not matter.
        let (a, b) = events.as_bytes().split_at(57);
        assert!(!stream.feed(a, &mut |t| seen.push_str(t)).unwrap());
        assert!(stream.feed(b, &mut |t| seen.push_str(t)).unwrap());
        let reply = stream.finish();
        assert_eq!(seen, "On it.");
        assert_eq!(reply.text, "On it.");
        assert_eq!(
            reply.tools,
            vec![RequestedTool {
                id: "call_1".into(),
                name: "ask_agent".into(),
                arguments: "{\"message\":\"run tests\"}".into(),
            }]
        );
    }

    #[test]
    fn a_mid_stream_error_fails_the_reply() {
        let mut stream = ReplyStream::default();
        let error = "data: {\"error\":{\"code\":\"server_error\",\"message\":\"overloaded\"},\"choices\":[{\"delta\":{\"content\":\"\"},\"finish_reason\":\"error\"}]}\n";
        let failure = stream.feed(error.as_bytes(), &mut |_| {}).unwrap_err();
        assert!(failure.message.contains("overloaded"));
        assert!(!failure.fatal);
    }

    #[test]
    fn pcm_survives_odd_chunk_boundaries_and_names_its_rate() {
        let mut pcm = Pcm::default();
        let bytes: Vec<u8> = [0i16, 16_384, -16_384]
            .iter()
            .flat_map(|s| s.to_le_bytes())
            .collect();
        let mut samples = pcm.decode(&bytes[..3]);
        samples.extend(pcm.decode(&bytes[3..]));
        assert_eq!(samples, vec![0.0, 0.5, -0.5]);
        assert_eq!(pcm_rate("audio/pcm;rate=16000"), Some(16_000));
        assert_eq!(pcm_rate("audio/pcm; sample_rate=24000"), Some(24_000));
        assert_eq!(pcm_rate("audio/pcm"), None);
        assert_eq!(pcm_rate("audio/pcm;rate=5"), None);
    }
}
