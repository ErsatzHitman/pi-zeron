# Voice mode

Voice mode lets the user talk to Pi instead of typing, and hear Pi's own messages read aloud while it works. Pi itself is unchanged. A spoken request is sent to Pi as a normal prompt, and Pi reasons, reads files, calls tools, edits code and runs tests exactly as it would for a typed prompt. Voice mode adds two things on top:

1. **Voice in:** what the user says is transcribed and sent to the chat.
2. **Voice out:** the messages Pi writes in that chat are read aloud as they appear. This includes the short notes it leaves between tool calls and its final answer.

Two cloud stages run on this device, with no Zeron server in between:

1. **Speech to text:** OpenRouter, Deepgram or Mistral.
2. **Voice:** an OpenRouter speech model, streamed back as PCM.

There is no assistant of its own in between. Everything spoken comes from Pi's messages. Voice mode never invents a status update, and it never reads Pi's thinking.

## An example

1. The user says: "Change the invoice mode color from red to blue."
2. The words are sent to the chat as a prompt, and Pi starts working.
3. Pi writes "I'm looking through the invoice component to find where the color is defined.", then reads files. The note is read aloud while Pi keeps working.
4. Pi writes "I found the variable controlling the invoice color. I'll update it from red to blue.", which is read aloud while Pi edits the file and runs the tests.
5. Pi writes its final answer, "Done! I've changed the invoice mode color to blue, and everything is working correctly.", and that is read aloud too.

Pi never waits for the voice. Nothing pauses it after a spoken update.

## Starting a call

When the draft is empty, the composer's send button is the voice orb. Typing, attaching or dictating morphs it back into the send arrow, and clearing the draft brings the orb back. The orb only appears once Settings → Voice has the keys it needs. The sidebar microphone also starts a call.

Pressing the orb starts a call and opens the full-window stage. The call keeps running when the user moves to another chat or to Settings. Esc hides the stage; the orb (or the sidebar orb) brings it back. The call ends when the user hangs up, the engine connection is lost, or a provider rejects a key or runs out of credit.

## Settings → Voice → Voice mode

**Processing** picks how a call runs:

- **STT + LLM + TTS** (default): the pipeline above. What the user says is transcribed and sent to Pi, and Pi's messages are read aloud.
- **Real-time voice**: one end-to-end audio model hears the user, answers aloud and hands work to Pi. See [Real-time voice](#real-time-voice).

Rows for the other mode are hidden. **Interrupt by speaking** applies to both.

| Setting (STT + LLM + TTS) | Effect |
| --- | --- |
| OpenRouter API key | Required. Used for the voice, and for speech to text when OpenRouter transcribes. |
| Speech to text | OpenRouter (default), Deepgram or Mistral. Deepgram and Mistral each need their own key. |
| Transcription model | OpenRouter: `openai/whisper-large-v3-turbo` (default), `openai/gpt-4o-mini-transcribe`, `mistralai/voxtral-mini-transcribe`, `deepgram/nova-3`. Deepgram: `nova-3`, `nova-2`. Mistral: `voxtral-mini-latest`. The dictation language setting is sent as a hint. |
| Voice model / Voice | Kokoro 82M (`hexgrad/kokoro-82m`, default voice `af_heart`) or Deepgram Aura 2 (`deepgram/aura-2`). Changing the model resets the voice to that model's first voice. |
| Send after a pause | 0.5, 0.8 (default), 1.2 or 2 seconds of silence ends what the user is saying. |
| Interrupt by speaking | On by default. Talking over the voice stops it. Turn this off if speaker echo keeps stopping it; the microphone is then ignored while the voice speaks. |

| Setting (Real-time voice) | Effect |
| --- | --- |
| Provider | OpenAI (default), Google Gemini, or Qwen (Alibaba Cloud). Changing it resets the model and the voice. |
| API key | The provider's key: OpenAI (`openai`, shared with OpenAI dictation), Gemini (`gemini`) or DashScope (`dashscope`). |
| Model | OpenAI: `gpt-realtime-2.1` (default), `gpt-realtime-2`, `gpt-realtime`, `gpt-realtime-mini`. Gemini: `gemini-3.1-flash-live-preview` (default), `gemini-2.5-flash-native-audio-preview-12-2025`. Qwen: `qwen3.8-omni-flash-realtime` (default), `qwen3.5-omni-plus-realtime`, `qwen3.5-omni-flash-realtime`. |
| Voice | The model's voices (OpenAI `marin` by default, Gemini `Puck`, Qwen `Tina`). Changing the model resets the voice. |
| Region | Qwen only: International (Singapore, `dashscope-intl.aliyuncs.com`) or China (Beijing, `dashscope.aliyuncs.com`), matching where the key was issued. |
| Turn-taking | Patient, Balanced (default) or Quick: how long the model waits after a pause before answering. OpenAI: semantic VAD eagerness low, medium or high. Gemini and Qwen: 1.2, 0.8 or 0.5 seconds of silence. |

The microphone is the one chosen for dictation. Keys are stored in `stt-keys.json` in Zeron's data directory, next to the dictation keys and never in `ui-settings.json`. Removing a key hides the orb.

## Speaking a prompt

Turn detection is local. Every 20 ms frame's energy is compared with a noise floor that adapts to the room.

- **Starting:** three voiced frames start a turn, and the 300 ms before them are kept so the first syllable survives.
- **Confirming:** a burst shorter than 300 ms of voiced audio is dropped as a cough or click.
- **Ending:** the chosen pause ends the turn, and a turn is cut at 30 seconds. The finished turn is resampled to 16 kHz and sent to the transcriber as one WAV file.

If the user starts speaking again before the transcript is back, the two parts become one prompt, so a pause mid-sentence does not split a request in two.

The transcript is sent through the composer's normal send path, exactly as if it were typed:

- It goes to the chat that is open. With no chat open, it starts a new one as the composer would.
- While Pi is busy, it joins the chat's queue like a typed message.
- If the message box holds an unsent draft, nothing is sent and the voice says so. Nothing the user typed is overwritten.

## What is read aloud

Voice mode follows the chat its prompts go to. When a call starts, that is the open chat. After a prompt is sent to another chat, voice mode follows that chat instead. Messages already in the chat when voice mode starts following it are not read.

| In the chat | Read aloud |
| --- | --- |
| Pi's text: notes between tool calls, and the final answer | Yes, a sentence at a time as it streams |
| Tool calls, tool output, diffs and logs | No |
| Thinking | No |
| Code blocks, tables and HTML inside a message | No; only the prose around them |
| A question Pi asks the user | "The agent asks: …", then "Answer it in the chat." |
| An error | "The agent stopped with an error. The details are in the chat." |

Each sentence is cleaned for speech before it is voiced:

- Markdown marks are removed.
- Links are read as their label.
- Bare web addresses are read as "a link".
- File paths are read as the file name: `src/styles/invoice.css` becomes "invoice.css".
- `snake_case` names are read as words.

A message is read up to about 1,200 characters; after that the voice says "The rest is in the chat." Sentences are voiced in order and only while the user is not talking. A sentence that arrives while the user speaks waits for them to finish.

There is no echo canceller, so while the voice speaks the detector needs louder and longer speech: a higher threshold and eight voiced frames. When speech starts, playback ducks to 25%. If the speech is confirmed, the voice stops mid-sentence and drops what it had queued, and what the user says becomes the next prompt. If it is dropped, the volume comes back. Pi's later messages are read as usual. Headphones give the cleanest calls.

The stage captions show what the user said and the message being read. The orb shows work in progress while Pi runs or a transcript is on its way, and the stage's transcript button opens the chat being followed.

## Cost

Each prompt makes one transcription, and each sentence read aloud makes one speech request. The table uses OpenRouter list prices from 14 October 2026, at ₹95.32 to the US dollar (14 October 2026).

| Stage | Model | Price | Per minute of call* |
| --- | --- | --- | --- |
| Speech to text | `openai/whisper-large-v3-turbo` | $0.0002 per audio minute | ₹0.005 |
| Speech to text | `deepgram/nova-3` | $0.0043 per audio minute | ₹0.10 |
| Speech to text | `mistralai/voxtral-mini-transcribe` | $0.003 per audio minute | ₹0.07 |
| Voice | `hexgrad/kokoro-82m` | $0.62 per million characters | ₹0.03 |
| Voice | `deepgram/aura-2` | $30 per million characters | ₹1.29 |

\*Assumes the user talks for 15 seconds a minute and 450 characters of Pi's messages are read aloud a minute.

Calling Deepgram or Mistral directly costs the same as going through OpenRouter:

- Deepgram lists Nova-3 pre-recorded audio at $0.0043 a minute ($0.0052 for multilingual) and Aura-2 at $0.030 per 1,000 characters.
- Mistral doesn't publish a per-minute price for `voxtral-mini-latest` on its own pages; OpenRouter lists it at $0.003 a minute.

With the defaults, a call costs about ₹0.03 a minute, or ₹2 an hour. Aura 2 makes it about forty times dearer. Pi's own model usage is billed as it is for typed prompts. Prices and exchange rates change, so check each provider's pricing page.

## Privacy

Audio is held in memory only. Each finished turn goes to the chosen transcriber over HTTPS, and each sentence read aloud goes to OpenRouter for the voice. The transcript reaches Pi as a normal message in the chat. Nothing is written to disk by voice mode.

A rejected key (401 or 403) or exhausted credit (402) ends the call and says why. Other provider failures are shown as a caption, and the call carries on.

## Real-time voice

One WebSocket per call goes straight to the provider: OpenAI Realtime, Gemini Live, or Qwen-Omni Realtime. The model detects turns, transcribes the user and speaks its answer, so replies start while the user is still on the line rather than after separate transcription and speech requests.

Pi still does the work. The model is told that it is the voice of Zeron and that a coding agent works in the user's chat. It gets one tool, `send_to_agent(prompt)`. A call to that tool sends the prompt to the followed chat exactly like a spoken prompt in the pipeline mode. The model answers small talk itself and never does or invents coding work.

Pi's messages reach the model as text, prefixed `[Coding agent]`. The narrator's sentences for one message are gathered until the message has been quiet for 1.2 seconds, then sent as one. The model tells the user what matters in a sentence or two. The captions show what the user said, as the provider transcribed it, and what the model is saying.

Nothing cancels the speaker's echo, so this device keeps the model from hearing itself. While the model's voice plays, the microphone is sent as silence. The exception is when Interrupt by speaking is on and the local turn detector hears the user talking over the voice. Then playback stops, the provider's answer is cancelled, and the user's audio goes through. Stray noise does not start a turn: OpenAI uses far-field noise reduction and semantic VAD, and Gemini uses low start-of-speech sensitivity.

Audio is PCM16. The microphone is sent at 24 kHz to OpenAI and 16 kHz to Gemini and Qwen. Every provider answers at 24 kHz.

## Implementation

`zeron_voice::call` owns the audio side of a call:

- `audio`: the capture and playback thread, with gain and meters.
- `turns`: turn detection.
- `providers`: speech to text, the voice, and the model catalog.
- `speech`: `Reader`, which turns a streamed Markdown message into speakable sentences, skipping code, tables and HTML.
- `mod.rs`: the call loop. It transcribes finished turns in order and reports them as `Event::User`, and it voices `Command::Say` sentences while the user is quiet.
- `realtime`: the real-time call. It keeps each provider's session setup, messages and event parsing together, reports `Event::Heard`, `Event::Reply` and `Event::Prompt`, and turns `Command::Say` into text for the model.

`zeron-ui::voice` maps call events onto the existing voice view state, so the orb, the stage and the captions are unchanged. The UI side is split across:

- `zeron-ui::voice::agent` sends each transcript through `Composer::send_spoken`. It also watches the followed chat's transcript (`WatchDocMessages`).
- `zeron-ui::voice::narrator` decides which new parts of that transcript to read, and passes them to the call one sentence at a time.
- `zeron-ui::dictation::model::voice_mode` holds the settings.

## Verification

The live suite (`crates/voice/src/call/live.rs`) makes real requests. It runs in the **Voice mode live** workflow when the `OPENROUTER_API_KEY` repository secret is set; `DEEPGRAM_API_KEY` and `MISTRAL_API_KEY` add the direct providers. It checks that:

- every voice model speaks;
- every OpenRouter transcriber, plus Deepgram and Mistral directly, hears that speech;
- an agent message with code and a table is read as its two prose sentences, and the transcriber hears them back.

`cargo test -p zeron-voice --lib` covers:

- Turn detection: start, confirm, drop, playback thresholds, the noise floor and the 30-second cap.
- Sentence splitting, the Markdown reader and speech cleanup.
- Provider request and response parsing, including fatal status codes.

`cargo test -p zeron-ui --lib` covers:

- Voice mode settings and keys.
- The controller's event reduction.
- The narrator: progress notes and final answers read once, history left unread, questions and errors announced.
- Spoken prompts never taking a draft.
- The send-slot orb ↔ arrow morph.

Routine CI makes no network calls.
