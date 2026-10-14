//! Live round trips against the real providers, with no audio devices: a
//! voice model speaks a request, a transcriber hears it, and the reply model
//! acts on it the way a call does. Ignored by default; the "Voice mode live"
//! workflow runs them with keys from repository secrets:
//! `OPENROUTER_API_KEY` (required), `DEEPGRAM_API_KEY` and `MISTRAL_API_KEY`
//! (each provider is skipped without its key).
use super::{
    Resampler,
    providers::{self, REPLY_MODELS, SPEECH_MODELS, Stt, SttProvider, Tool},
};
use serde_json::{Value, json};

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

fn ask_agent() -> Tool {
    Tool {
        name: "ask_agent".into(),
        description: "Send an instruction to the coding agent. Returns at once.".into(),
        parameters: json!({
            "type": "object",
            "properties": { "message": { "type": "string" } },
            "required": ["message"],
        }),
    }
}

fn status() -> Tool {
    Tool {
        name: "agent_status".into(),
        description: "What the coding agent is doing.".into(),
        parameters: json!({ "type": "object", "properties": {} }),
    }
}

#[tokio::test]
#[ignore = "calls OpenRouter; run by the Voice mode live workflow"]
async fn openrouter_speaks_hears_and_hands_work_to_the_agent() {
    let key = openrouter();
    let client = reqwest::Client::new();

    // Every voice model speaks; Whisper hears the default one.
    let mut spoken = Vec::new();
    for model in SPEECH_MODELS {
        spoken.push(say(&client, &key, model.id, model.voices[0], REQUEST).await);
    }
    let heard = hear(
        &client,
        Stt {
            provider: SttProvider::OpenRouter,
            api_key: key.clone(),
            model: None,
            language: Some("en".into()),
        },
        &spoken[0],
    )
    .await;

    // The default reply model calls the tool for coding work…
    let mut messages = vec![
        json!({
            "role": "system",
            "content": "You are a voice assistant. You cannot run code yourself: for any \
                        coding task call ask_agent with the instruction, then confirm in a \
                        few words.",
        }),
        json!({ "role": "user", "content": heard }),
    ];
    let tools = [ask_agent(), status()];
    let mut streamed = String::new();
    let reply = providers::reply(&client, &key, REPLY_MODELS[0], &messages, &tools, |t| {
        streamed.push_str(t)
    })
    .await
    .unwrap_or_else(|e| panic!("{} did not reply: {e:?}", REPLY_MODELS[0]));
    eprintln!("first round: {reply:?}");
    let call = reply
        .tools
        .iter()
        .find(|t| t.name == "ask_agent")
        .expect("the reply model asks the agent");
    let arguments: Value = serde_json::from_str(&call.arguments).expect("JSON arguments");
    assert!(arguments["message"].as_str().is_some_and(|m| !m.is_empty()));

    // …and, given the tool's answer in the call's own message format, speaks.
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
    messages.push(json!({ "role": "assistant", "tool_calls": calls }));
    for tool in &reply.tools {
        messages.push(json!({
            "role": "tool",
            "tool_call_id": tool.id,
            "content": "Sent. The agent is working on it in the background.",
        }));
    }
    let mut streamed = String::new();
    let reply = providers::reply(&client, &key, REPLY_MODELS[0], &messages, &tools, |t| {
        streamed.push_str(t)
    })
    .await
    .expect("second round");
    eprintln!("second round: {:?}", reply.text);
    assert!(!reply.text.trim().is_empty(), "no spoken confirmation");
    assert_eq!(streamed, reply.text);
}

#[tokio::test]
#[ignore = "calls OpenRouter; run by the Voice mode live workflow"]
async fn every_reply_model_streams_and_calls_tools() {
    let key = openrouter();
    let client = reqwest::Client::new();
    let messages = [
        json!({ "role": "system", "content": "For any coding task call ask_agent." }),
        json!({ "role": "user", "content": REQUEST }),
    ];
    let mut failures = Vec::new();
    for model in REPLY_MODELS {
        // The call declares an argument-free tool too, as voice mode does.
        let tools = [ask_agent(), status()];
        match providers::reply(&client, &key, model, &messages, &tools, |_| {}).await {
            Ok(reply) if reply.tools.iter().any(|t| t.name == "ask_agent") => {
                eprintln!("{model}: ok");
            }
            Ok(reply) => failures.push(format!("{model}: no tool call, said {:?}", reply.text)),
            Err(e) => failures.push(format!("{model}: {e:?}")),
        }
    }
    assert!(failures.is_empty(), "{failures:#?}");
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
