//! Cloud transcription: one recording goes to the provider's OpenAI-compatible
//! `/audio/transcriptions` endpoint as a 16 kHz WAV. The text then loses its
//! filler words and, when it ends with the trigger phrase, is rewritten by the
//! same provider's chat model. Nothing is stored. While recording, a live
//! preview sends each phrase as it ends ([`preview`]).
use crate::{cleanup, resample, trigger};
use anyhow::{Result, anyhow, bail};
use std::time::Duration;

const TRANSCRIBE_TIMEOUT: Duration = Duration::from_secs(30);
const REWRITE_TIMEOUT: Duration = Duration::from_secs(20);
pub(crate) const BOUNDARY: &str = "zeron-voice-7f3a9c1e5b";
/// A body this short is left as it is unless instructions say otherwise.
const MIN_REWRITE_WORDS: usize = 4;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Provider {
    #[default]
    OpenAi,
    Groq,
}

impl Provider {
    pub const ALL: [Self; 2] = [Self::OpenAi, Self::Groq];

    pub fn label(self) -> &'static str {
        match self {
            Self::OpenAi => "OpenAI",
            Self::Groq => "Groq",
        }
    }
    /// The key's name in the key file.
    pub fn key_name(self) -> &'static str {
        match self {
            Self::OpenAi => "openai",
            Self::Groq => "groq",
        }
    }
    pub fn models(self) -> &'static [&'static str] {
        match self {
            Self::OpenAi => &["gpt-4o-mini-transcribe", "gpt-4o-transcribe", "whisper-1"],
            Self::Groq => &["whisper-large-v3-turbo", "whisper-large-v3"],
        }
    }
    pub fn default_model(self) -> &'static str {
        self.models()[0]
    }
    fn base_url(self) -> &'static str {
        match self {
            Self::OpenAi => "https://api.openai.com/v1",
            Self::Groq => "https://api.groq.com/openai/v1",
        }
    }
    /// The shortest wait between live-preview requests. Groq's free tier
    /// allows about twenty transcriptions a minute, and the final transcript
    /// needs one of them.
    pub fn preview_gap(self) -> Duration {
        match self {
            Self::OpenAi => Duration::from_millis(800),
            Self::Groq => Duration::from_secs(4),
        }
    }
    fn rewrite_model(self) -> &'static str {
        match self {
            Self::OpenAi => "gpt-4o-mini",
            Self::Groq => "llama-3.3-70b-versatile",
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Config {
    pub provider: Provider,
    pub api_key: String,
    /// `None` is the provider's default model.
    pub model: Option<String>,
    /// ISO-639-1 code; `None` lets the provider detect the language.
    pub language: Option<String>,
    pub remove_fillers: bool,
    /// Empty turns the trigger phrase off.
    pub trigger_phrase: String,
    /// Show the text phrase by phrase while recording ([`preview`]).
    pub live: bool,
}

impl Config {
    fn model(&self) -> &str {
        self.model
            .as_deref()
            .filter(|m| self.provider.models().contains(m))
            .unwrap_or(self.provider.default_model())
    }
    fn english(&self) -> bool {
        self.language.as_deref().is_none_or(|l| l == "en")
    }
}

/// Called only on the dictation worker.
pub fn transcribe(config: &Config, samples: Vec<f32>, rate: u32) -> Result<String> {
    if samples.len() > rate as usize * crate::MAX_SECONDS {
        bail!("Recording exceeds one minute")
    }
    if samples.len() < rate as usize / 5 || samples.iter().all(|s| s.abs() < 0.0001) {
        return Ok(String::new());
    }
    let label = config.provider.label();
    if config.api_key.trim().is_empty() {
        bail!("Add a {label} API key in Settings → API keys.")
    }
    let wav = wav(&resample::for_model(samples, rate)?);
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(async {
            let client = reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(10))
                .build()?;
            let text = request(&client, config, wav).await?;
            Ok(match finish(config, &text) {
                Outcome::Text(text) => text,
                Outcome::Rewrite { body, instructions } => {
                    // A failed rewrite still inserts what was said.
                    let rewrite = rewritten(&client, config, &body, instructions.as_deref()).await;
                    rewrite.unwrap_or(body)
                }
            })
        })
}

/// A phrase of a recording that is still going on, for the live preview:
/// the text alone, fillers removed, no trigger phrase acted on. Called only
/// on the preview thread.
pub fn preview(config: &Config, samples: Vec<f32>, rate: u32) -> Result<String> {
    if samples.len() < rate as usize / 5 || config.api_key.trim().is_empty() {
        return Ok(String::new());
    }
    let wav = wav(&resample::for_model(samples, rate)?);
    let text = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(async {
            let client = reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(10))
                .build()?;
            request(&client, config, wav).await
        })?;
    Ok(if config.remove_fillers {
        cleanup::clean(&text, config.english())
    } else {
        text.trim().to_owned()
    })
}

async fn request(client: &reqwest::Client, config: &Config, wav: Vec<u8>) -> Result<String> {
    let label = config.provider.label();
    let response = client
        .post(format!(
            "{}/audio/transcriptions",
            config.provider.base_url()
        ))
        .bearer_auth(config.api_key.trim())
        .header(
            reqwest::header::CONTENT_TYPE,
            format!("multipart/form-data; boundary={BOUNDARY}"),
        )
        .body(multipart(config.model(), config.language.as_deref(), &wav))
        .timeout(TRANSCRIBE_TIMEOUT)
        .send()
        .await
        .map_err(|e| {
            if e.is_timeout() {
                anyhow!("{label} took too long to answer. Try again.")
            } else {
                anyhow!("Could not reach {label}. Check your connection.")
            }
        })?;
    let status = response.status();
    if !status.is_success() {
        bail!(status_message(label, status.as_u16()))
    }
    let body: serde_json::Value = response
        .json()
        .await
        .map_err(|_| anyhow!("{label} sent an answer Zeron could not read."))?;
    text_of(&body).ok_or_else(|| anyhow!("{label} sent an answer Zeron could not read."))
}

fn status_message(label: &str, status: u16) -> String {
    match status {
        401 | 403 => format!("The {label} API key was rejected. Check it in Settings → API keys."),
        429 => format!("{label} is rate limiting requests. Try again in a moment."),
        413 => "This recording is too large to send.".to_owned(),
        _ => format!("{label} could not transcribe this recording (error {status})."),
    }
}

fn text_of(body: &serde_json::Value) -> Option<String> {
    Some(body.get("text")?.as_str()?.trim().to_owned())
}

async fn rewritten(
    client: &reqwest::Client,
    config: &Config,
    body: &str,
    instructions: Option<&str>,
) -> Result<String> {
    let mut system =
        "Rewrite the user's dictated text concisely. Keep its meaning, its language and \
                      any names or technical terms. Reply with the rewritten text only."
            .to_owned();
    if let Some(instructions) = instructions {
        system.push_str(&format!(" Also follow these instructions: {instructions}"));
    }
    let response = client
        .post(format!("{}/chat/completions", config.provider.base_url()))
        .bearer_auth(config.api_key.trim())
        .json(&serde_json::json!({
            "model": config.provider.rewrite_model(),
            "temperature": 0,
            "messages": [
                { "role": "system", "content": system },
                { "role": "user", "content": body },
            ],
        }))
        .timeout(REWRITE_TIMEOUT)
        .send()
        .await?
        .error_for_status()?;
    let reply: serde_json::Value = response.json().await?;
    reply_of(&reply).ok_or_else(|| anyhow!("empty rewrite"))
}

fn reply_of(body: &serde_json::Value) -> Option<String> {
    let text = body.pointer("/choices/0/message/content")?.as_str()?.trim();
    (!text.is_empty()).then(|| text.to_owned())
}

#[derive(Debug, PartialEq, Eq)]
enum Outcome {
    /// Insert as it is.
    Text(String),
    /// Insert the rewrite, or the body when the rewrite fails.
    Rewrite {
        body: String,
        instructions: Option<String>,
    },
}

/// Filler cleanup, then the trigger phrase.
fn finish(config: &Config, text: &str) -> Outcome {
    let english = config.english();
    let clean = |t: &str| {
        if config.remove_fillers {
            cleanup::clean(t, english)
        } else {
            t.trim().to_owned()
        }
    };
    let text = clean(text);
    let Ok(phrase) = trigger::Trigger::check(&config.trigger_phrase, clean) else {
        return Outcome::Text(text);
    };
    match trigger::detect(&text, &phrase) {
        trigger::Detect::NoMatch => Outcome::Text(text),
        trigger::Detect::OnlyPhrase => Outcome::Text(String::new()),
        trigger::Detect::Match { body, instructions } => {
            if instructions.is_none() && trigger::word_count(&body) < MIN_REWRITE_WORDS {
                return Outcome::Text(body);
            }
            Outcome::Rewrite { body, instructions }
        }
    }
}

/// 16-bit PCM mono WAV at the model rate.
pub(crate) fn wav(samples: &[f32]) -> Vec<u8> {
    let rate = resample::MODEL_RATE;
    let data = (samples.len() * 2) as u32;
    let mut out = Vec::with_capacity(44 + data as usize);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes()); // PCM
    out.extend_from_slice(&1u16.to_le_bytes()); // mono
    out.extend_from_slice(&rate.to_le_bytes());
    out.extend_from_slice(&(rate * 2).to_le_bytes());
    out.extend_from_slice(&2u16.to_le_bytes());
    out.extend_from_slice(&16u16.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data.to_le_bytes());
    for s in samples {
        out.extend_from_slice(&((s.clamp(-1.0, 1.0) * i16::MAX as f32) as i16).to_le_bytes());
    }
    out
}

fn multipart(model: &str, language: Option<&str>, wav: &[u8]) -> Vec<u8> {
    let mut fields = vec![("model", model), ("response_format", "json")];
    fields.extend(language.map(|language| ("language", language)));
    form(&fields, wav)
}

/// A multipart body: text fields, then the recording as `audio.wav`.
pub(crate) fn form(fields: &[(&str, &str)], wav: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(wav.len() + 512);
    for (name, value) in fields {
        out.extend_from_slice(
            format!(
                "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n"
            )
            .as_bytes(),
        );
    }
    out.extend_from_slice(
        format!(
            "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"audio.wav\"\r\nContent-Type: audio/wav\r\n\r\n"
        )
        .as_bytes(),
    );
    out.extend_from_slice(wav);
    out.extend_from_slice(format!("\r\n--{BOUNDARY}--\r\n").as_bytes());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> Config {
        Config {
            api_key: "key".into(),
            remove_fillers: true,
            trigger_phrase: trigger::DEFAULT_PHRASE.into(),
            ..Config::default()
        }
    }

    #[test]
    fn wav_header_describes_16_bit_mono_at_16_khz() {
        let wav = wav(&[0.0, 1.0, -1.0]);
        assert_eq!(wav.len(), 44 + 6);
        assert_eq!(&wav[..4], b"RIFF");
        assert_eq!(u32::from_le_bytes(wav[4..8].try_into().unwrap()), 36 + 6);
        assert_eq!(&wav[8..16], b"WAVEfmt ");
        assert_eq!(u16::from_le_bytes([wav[22], wav[23]]), 1);
        assert_eq!(u32::from_le_bytes(wav[24..28].try_into().unwrap()), 16_000);
        assert_eq!(&wav[36..40], b"data");
        assert_eq!(i16::from_le_bytes([wav[46], wav[47]]), i16::MAX);
        assert_eq!(i16::from_le_bytes([wav[48], wav[49]]), -i16::MAX);
    }

    #[test]
    fn multipart_carries_the_model_language_and_file() {
        let body = String::from_utf8(multipart("whisper-1", Some("hi"), b"WAV")).unwrap();
        assert!(body.contains("name=\"model\"\r\n\r\nwhisper-1\r\n"));
        assert!(body.contains("name=\"language\"\r\n\r\nhi\r\n"));
        assert!(body.contains("filename=\"audio.wav\"\r\nContent-Type: audio/wav\r\n\r\nWAV\r\n"));
        assert!(body.ends_with(&format!("--{BOUNDARY}--\r\n")));
        let auto = String::from_utf8(multipart("whisper-1", None, b"")).unwrap();
        assert!(!auto.contains("language"));
    }

    #[test]
    fn answers_are_read_and_errors_name_the_fix() {
        assert_eq!(
            text_of(&serde_json::json!({ "text": " Hello there. " })).as_deref(),
            Some("Hello there.")
        );
        assert_eq!(text_of(&serde_json::json!({ "error": "x" })), None);
        assert_eq!(
            reply_of(&serde_json::json!({ "choices": [{ "message": { "content": "Short." } }] }))
                .as_deref(),
            Some("Short.")
        );
        assert!(status_message("Groq", 401).contains("Settings → API keys"));
        assert!(status_message("OpenAI", 429).contains("rate limiting"));
    }

    #[test]
    fn an_unknown_model_falls_back_to_the_provider_default() {
        let mut c = config();
        c.provider = Provider::Groq;
        c.model = Some("gpt-4o-transcribe".into());
        assert_eq!(c.model(), "whisper-large-v3-turbo");
        c.model = Some("whisper-large-v3".into());
        assert_eq!(c.model(), "whisper-large-v3");
    }

    #[test]
    fn fillers_go_and_the_trigger_phrase_asks_for_a_rewrite() {
        assert_eq!(
            finish(&config(), "Um, we ship on Friday."),
            Outcome::Text("We ship on Friday.".into())
        );
        assert_eq!(
            finish(
                &config(),
                "We should ship the release on Friday. Rewrite this concisely and make it formal."
            ),
            Outcome::Rewrite {
                body: "We should ship the release on Friday.".into(),
                instructions: Some("make it formal.".into())
            }
        );
        // Too short to rewrite, only the phrase, or the phrase turned off.
        assert_eq!(
            finish(&config(), "Ship it. Rewrite this concisely."),
            Outcome::Text("Ship it.".into())
        );
        assert_eq!(
            finish(&config(), "Rewrite this concisely."),
            Outcome::Text(String::new())
        );
        let off = Config {
            trigger_phrase: String::new(),
            remove_fillers: false,
            ..config()
        };
        assert_eq!(
            finish(&off, "Um, ship it. Rewrite this concisely."),
            Outcome::Text("Um, ship it. Rewrite this concisely.".into())
        );
    }
}
