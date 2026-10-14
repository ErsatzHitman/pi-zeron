# Voice mode

Voice mode is a spoken conversation with an assistant that can hand work to Pi. The user talks, the assistant answers out loud, and coding requests go to the agent in the user's chat. Three cloud stages run on this device, with no Zeron server in between:

1. **Speech to text:** OpenRouter, Deepgram or Mistral.
2. **Reply:** an OpenRouter chat model, streamed, with tools.
3. **Voice:** an OpenRouter speech model, streamed back as PCM.

## Starting a call

When the draft is empty, the composer's send button is the voice orb. Typing, attaching or dictating morphs it back into the send arrow, and clearing the draft brings the orb back. The orb only appears once Settings → Voice has the keys it needs. The sidebar microphone also starts a call.

Pressing the orb starts a call and opens the full-window stage. The call keeps running when the user moves to another chat or to Settings. Esc hides the stage; the orb (or the sidebar orb) brings it back. The call ends when the user hangs up, the engine connection is lost, or a provider rejects a key or runs out of credit.

## Settings → Voice → Voice mode

| Setting | Effect |
| --- | --- |
| OpenRouter API key | Required. Used for the reply and the voice, and for speech to text when OpenRouter transcribes. |
| Speech to text | OpenRouter (default), Deepgram or Mistral. Deepgram and Mistral each need their own key. |
| Transcription model | OpenRouter: `openai/whisper-large-v3-turbo` (default), `openai/gpt-4o-mini-transcribe`, `mistralai/voxtral-mini-transcribe`, `deepgram/nova-3`. Deepgram: `nova-3`, `nova-2`. Mistral: `voxtral-mini-latest`. The dictation language setting is sent as a hint. |
| Reply model | Fast chat models that call tools, cheapest first: `google/gemini-2.5-flash-lite` (default), `openai/gpt-4.1-nano`, `openai/gpt-oss-120b`, `mistralai/mistral-small-3.2-24b-instruct`, `anthropic/claude-haiku-5.5`, `openai/gpt-4.1-mini`, `meta-llama/llama-3.3-70b-instruct`. |
| Voice model / Voice | Kokoro 82M (`hexgrad/kokoro-82m`, default voice `af_heart`) or Deepgram Aura 2 (`deepgram/aura-2`). Changing the model resets the voice to that model's first voice. |
| Reply after a pause | 0.5, 0.8 (default), 1.2 or 2 seconds of silence ends the user's turn. |
| Interrupt by speaking | On by default. Talking over the assistant stops it. Turn this off if speaker echo keeps interrupting it; the microphone is then ignored while the assistant speaks. |

The microphone is the one chosen for dictation. Keys are stored in `stt-keys.json` in Zeron's data directory, next to the dictation keys and never in `ui-settings.json`. Removing a key hides the orb.

## Turn taking

Turn detection is local. Every 20 ms frame's energy is compared with a noise floor that adapts to the room.

- **Starting a turn:** three voiced frames start a turn, and the 300 ms before them are kept so the first syllable survives.
- **Confirming a turn:** a burst shorter than 300 ms of voiced audio is dropped as a cough or click.
- **Ending a turn:** the chosen pause ends the turn, and a turn is cut at 30 seconds. The finished turn is resampled to 16 kHz and sent to the transcriber as one WAV file.

There is no echo canceller, so while the assistant speaks the detector needs louder and longer speech: a higher threshold and eight voiced frames. When speech starts, playback ducks to 25%. If the speech is confirmed, the assistant stops mid-sentence. If it is dropped, the volume comes back.

When an interruption arrives before the assistant has spoken, the user's words carry into the next turn rather than being lost. Headphones give the cleanest calls.

## Replies and speech

The reply streams from OpenRouter. Each complete sentence is cleaned for speech (Markdown, code, links and lists removed; abbreviations do not split sentences) and sent to the speech model while the rest of the reply is still arriving. Speech returns as 16-bit PCM, normally at 24 kHz, and is resampled to the speaker's rate.

The conversation keeps the last 40 messages. A turn may make at most four model round trips, so a tool loop cannot run away. Captions on the stage show both sides as they happen.

## Handing work to Pi

The assistant cannot read files or run commands. It has three tools that act on the chat the user had selected when the call started, or on the chat a hand-off opened:

| Tool | What it does |
| --- | --- |
| `ask_agent` | Sends one self-contained instruction through the composer's normal send path, exactly as if the user had typed it. It refuses when the composer holds an unsent draft, so nothing the user wrote is overwritten. |
| `agent_status` | Reports whether the agent is working, waiting for the user, idle or errored, with up to 1,500 characters of its latest reply. |
| `stop_agent` | Interrupts the agent's current run. It is only used when the user asks. |

The agent works in the background. When it finishes, needs an answer or fails, the assistant is told and says so in a sentence or two, quoting up to 600 characters of the reply for context. The orb shows work in progress while the agent runs, and the stage's transcript button opens the agent's chat.

## Cost

Each turn makes one transcription, one or more chat requests and one speech request per sentence. The table uses OpenRouter list prices from 14 October 2026, at ₹95.32 to the US dollar (14 October 2026).

| Stage | Model | Price | Per minute of conversation* |
| --- | --- | --- | --- |
| Speech to text | `openai/whisper-large-v3-turbo` | $0.0002 per audio minute | ₹0.01 |
| Speech to text | `deepgram/nova-3` | $0.0043 per audio minute | ₹0.20 |
| Speech to text | `mistralai/voxtral-mini-transcribe` | $0.003 per audio minute | ₹0.14 |
| Reply | `google/gemini-2.5-flash-lite` | $0.10 / $0.40 per million tokens in / out | ₹0.11 |
| Voice | `hexgrad/kokoro-82m` | $0.62 per million characters | ₹0.03 |
| Voice | `deepgram/aura-2` | $30 per million characters | ₹1.28 |

\*Assumes the user talks for 30 seconds, four turns of about 2,500 prompt tokens each (instructions, tools and history), and 450 spoken characters a minute.

Calling Deepgram or Mistral directly costs the same as going through OpenRouter:

- Deepgram lists Nova-3 pre-recorded audio at $0.0043 a minute ($0.0052 for multilingual) and Aura-2 at $0.030 per 1,000 characters.
- Mistral doesn't publish a per-minute price for `voxtral-mini-latest` on its own pages; OpenRouter lists it at $0.003 a minute.

With the defaults, a call costs about ₹0.15 a minute, or ₹9 an hour. Aura 2 makes it about nine times dearer. Prices and exchange rates change, so check each provider's pricing page.

## Privacy

Audio is held in memory only. Each finished turn goes to the chosen transcriber over HTTPS. The transcript, the conversation so far and the tool results go to OpenRouter for the reply, and each spoken sentence goes to OpenRouter for the voice. Nothing is written to disk, and nothing reaches the coding-agent host except the instructions `ask_agent` sends, which appear in the chat like typed messages. A rejected key (401 or 403) or exhausted credit (402) ends the call and says why. Other provider failures are announced and the call carries on.

## Implementation

`zeron_voice::call` owns the call:

- `audio`: the capture and playback thread, with gain and meters.
- `turns`: turn detection.
- `providers`: the three HTTP stages and the model catalog.
- `speech`: the sentence splitter and Markdown stripping.
- `mod.rs`: the turn loop, history and tool rounds.

`zeron-ui::voice` maps call events onto the existing voice view state, so the orb, the stage and the captions are unchanged. `zeron-ui::voice::agent` bridges the tools to the composer, the app state and the transcript. Settings live in `zeron-ui::dictation::model::voice_mode`.

## Verification

The live suite (`crates/voice/src/call/live.rs`) makes real requests and runs in the **Voice mode live** workflow when the `OPENROUTER_API_KEY` repository secret is set (`DEEPGRAM_API_KEY` and `MISTRAL_API_KEY` add the direct providers). It checks that:

- every voice model speaks;
- every OpenRouter transcriber, plus Deepgram and Mistral directly, hears that speech;
- every reply model streams and calls `ask_agent`;
- the default model confirms aloud once the tool answers, using the same message format a call uses.


`cargo test -p zeron-voice --lib` covers:

- Turn detection: start, confirm, drop, playback thresholds, the noise floor and the 30-second cap.
- Sentence splitting and speech cleanup.
- Provider request and response parsing, including streamed tool calls and fatal status codes.

`cargo test -p zeron-ui --lib` covers:

- Voice mode settings and keys.
- The controller's event reduction.
- The agent tools.
- The send-slot orb ↔ arrow morph.

Routine CI makes no network calls.
