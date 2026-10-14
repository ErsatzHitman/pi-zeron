# Desktop dictation

Settings → Voice turns on the composer microphone: choose a provider (OpenAI or Groq), paste its API key and press Save. The microphone appears once the selected provider has a key. Zeron ships no speech model and downloads none. Speaking prompts to Pi and hearing its messages read aloud is a separate feature: see [Voice mode](voice-mode.md).

Dictation is hold to talk: hold the microphone (or Enter/Space while it is focused) or the **Hold to dictate** shortcut (default Cmd/Ctrl+D), and release to transcribe. Releasing outside the microphone still counts; for the shortcut, letting go of its key or any of its modifiers ends the hold, since macOS does not deliver key-up for Command chords. A release within 300 ms is treated as a tap and explains hold to talk instead of transcribing. Assistive-technology Click cannot hold, so it starts and then finishes. Recording is limited to one minute. Release sends the recording to the provider and inserts the text into the selected draft range. It never invokes an agent or submits on its own. Explicit Send waits for finalization, then follows the normal Send/Queue path once. If no speech is recognized, the composer explains this and preserves the draft without sending. Esc cancels; cancelling or a failed/timed-out finalization never sends. Microphone permission is requested on first use.

## Settings

| Setting | Effect |
| --- | --- |
| Provider | OpenAI (`/v1/audio/transcriptions`) or Groq (`/openai/v1/audio/transcriptions`). |
| API key | Stored in `stt-keys.json` in Zeron's data directory (owner-only on macOS and Linux), one key per provider. Never written to `ui-settings.json`. |
| Model | OpenAI: `gpt-4o-mini-transcribe` (default), `gpt-4o-transcribe`, `whisper-1`. Groq: `whisper-large-v3-turbo` (default), `whisper-large-v3`. |
| Language | Auto-detect, or a fixed language sent as an ISO-639-1 hint. |
| Microphone | A connected input; a disconnected selection falls back to the system default. |
| Remove filler words | Drops hesitations ("um", "uh"), "like" and "you know" used as fillers, and stutters ("the the"), then repairs punctuation. Rule based, on this device. |
| Trigger phrase | Default "rewrite this concisely". A dictation that ends with it is rewritten by the provider's chat model (OpenAI `gpt-4o-mini`, Groq `llama-3.3-70b-versatile`). Words after the phrase are extra instructions: "…rewrite this concisely and make it formal". Saying only the phrase inserts nothing; a body under four words is inserted unchanged; a failed rewrite inserts the transcript. Empty turns it off. |

## Privacy and limits

Audio stays in a bounded, transient buffer on this desktop until release, then goes to the selected provider as one 16 kHz mono WAV over HTTPS. No audio files are written, and nothing is sent to the coding-agent host. Dictated text is an ordinary editable draft. Transcription allows 30 seconds and the rewrite 20; the composer gives up after 60.

## Implementation

`zeron-voice` owns CPAL capture, anti-aliased conversion to 16 kHz, a capture thread, and one background worker that uploads the recording (`cloud`), cleans it (`cleanup`) and applies the trigger phrase (`trigger`). `zeron-ui::dictation` owns permissions, settings, the key file and the transcription event interface. Composer inputs own draft-range/generation protection and grouped undo. A manual edit, selection move, IME update, focus leaving the composer, chat switch or queue-draft replacement invalidates the session.

On Windows, microphone access for desktop apps must be enabled in the system privacy settings. Linux releases dynamically link the system ALSA runtime (`libasound.so.2`).

## Verification

Deterministic editor fixtures cover partial replacement, Unicode, undo/redo, failure, stale events, cancellation, Send finalization, queue editing, navigation, focus and input-request takeover. `cargo test -p zeron-voice` covers capture/upload barriers for Stop, cancellation, failure, device startup, recording limits and busy admission, plus the WAV and multipart encoding, response parsing, filler cleanup and trigger-phrase matching. Routine CI makes no network calls.
