//! Live round trips against the real providers, with no audio devices: an
//! agent's message is read the way a call reads it, a voice model speaks it
//! and a transcriber hears it back. Ignored by default; the "Voice mode live"
//! workflow runs them with keys from repository secrets: `OPENROUTER_API_KEY`
//! (required), `DEEPGRAM_API_KEY` and `MISTRAL_API_KEY` (each provider is
//! skipped without its key).
use super::{
    Reader, Resampler,
    providers::{self, SPEECH_MODELS, Stt, SttProvider},
};

const REQUEST: &str = "Please run the tests in the voice crate.";

fn key(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|k| !k.trim().is_empty())
}

fn openrouter() -> String {
    key("OPENROUTER_API_KEY").expect("OPENROUTER_API_KEY is required for the live tests")
}

/// `voice` of `model` saying `text`, at the transcriber's 16 kHz.
async fn say(
    client: &reqwest::Client,
    key: &str,
    model: &str,
    voice: &str,
    text: &str,
) -> Vec<f32> {
    let mut resampler = Resampler::new(crate::resample::MODEL_RATE);
    let mut audio = Vec::new();
    let mut rates = Vec::new();
    providers::speak(client, key, model, voice, text, |samples, rate| {
        rates.push(rate);
        audio.extend(resampler.push(samples, rate));
    })
    .await
    .unwrap_or_else(|e| panic!("{model} did not speak: {e:?}"));
    let peak = audio.iter().fold(0f32, |p, s| p.max(s.abs()));
    eprintln!(
        "{model}/{voice}: {:.2} s at {:?} Hz, peak {peak:.2}",
        audio.len() as f32 / crate::resample::MODEL_RATE as f32,
        rates.first()
    );
    assert!(
        audio.len() > crate::resample::MODEL_RATE as usize,
        "{model} spoke under a second"
    );
    assert!(peak > 0.05, "{model} spoke silence");
    audio
}

async fn hear(client: &reqwest::Client, stt: Stt, audio: &[f32]) -> String {
    let label = format!("{:?}", stt.provider);
    let text = providers::transcribe(client, &stt, crate::cloud::wav(audio))
        .await
        .unwrap_or_else(|e| panic!("{label} did not transcribe: {e:?}"));
    eprintln!("{label} heard: {text:?}");
    assert!(
        text.to_lowercase().contains("test"),
        "{label} misheard: {text:?}"
    );
    text
}

/// An agent's progress note with code and a table in it, as Pi writes them.
const AGENT_MESSAGE: &str =
    "I found the variable that sets the invoice color in `src/styles/invoice.css`.

```css
.invoice { color: red; }
```

| File | Change |
|---|---|
| invoice.css | red to blue |

I'll change it to blue, then run the tests.";

#[tokio::test]
#[ignore = "calls OpenRouter; run by the Voice mode live workflow"]
async fn openrouter_reads_an_agent_message_and_hears_it_back() {
    let key = openrouter();
    let client = reqwest::Client::new();

    // Every voice model speaks.
    for model in SPEECH_MODELS {
        say(&client, &key, model.id, model.voices[0], REQUEST).await;
    }

    // A call reads the agent's prose and skips its code and table…
    let mut reader = Reader::new(1200);
    let mut sentences = reader.push(AGENT_MESSAGE);
    sentences.extend(reader.finish());
    eprintln!("read aloud: {sentences:?}");
    assert_eq!(sentences.len(), 2, "{sentences:?}");
    assert!(
        sentences
            .iter()
            .all(|s| !s.contains('{') && !s.contains('|'))
    );

    // …and the default voice says it clearly enough to be heard back.
    let model = &SPEECH_MODELS[0];
    let mut audio = Vec::new();
    for sentence in &sentences {
        audio.extend(say(&client, &key, model.id, model.voices[0], sentence).await);
    }
    let heard = hear(
        &client,
        Stt {
            provider: SttProvider::OpenRouter,
            api_key: key.clone(),
            model: None,
            language: Some("en".into()),
        },
        &audio,
    )
    .await
    .to_lowercase();
    assert!(
        heard.contains("invoice") && heard.contains("blue"),
        "{heard:?}"
    );
}

#[tokio::test]
#[ignore = "calls OpenRouter; run by the Voice mode live workflow"]
async fn every_openrouter_transcriber_hears_speech() {
    let key = openrouter();
    let client = reqwest::Client::new();
    let model = &SPEECH_MODELS[0];
    let audio = say(&client, &key, model.id, model.voices[0], REQUEST).await;
    for stt_model in SttProvider::OpenRouter.models() {
        let stt = Stt {
            provider: SttProvider::OpenRouter,
            api_key: key.clone(),
            model: Some((*stt_model).into()),
            language: None,
        };
        eprintln!("{stt_model}:");
        hear(&client, stt, &audio).await;
    }
}

#[tokio::test]
#[ignore = "calls OpenRouter and Deepgram or Mistral; run by the Voice mode live workflow"]
async fn deepgram_and_mistral_hear_speech_directly() {
    let key = openrouter();
    let client = reqwest::Client::new();
    let model = &SPEECH_MODELS[0];
    let audio = say(&client, &key, model.id, model.voices[0], REQUEST).await;
    for (provider, env) in [
        (SttProvider::Deepgram, "DEEPGRAM_API_KEY"),
        (SttProvider::Mistral, "MISTRAL_API_KEY"),
    ] {
        let Some(api_key) = self::key(env) else {
            eprintln!("{provider:?}: skipped, {env} is not set");
            continue;
        };
        for language in [None, Some("en".to_owned())] {
            let stt = Stt {
                provider,
                api_key: api_key.clone(),
                model: None,
                language,
            };
            hear(&client, stt, &audio).await;
        }
    }
}
