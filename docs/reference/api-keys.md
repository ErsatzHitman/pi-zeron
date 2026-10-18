# API keys

Settings → API keys holds every provider key that Zeron and the Pi it hosts use:

- Pi models;
- Pi extensions (the OpenCode Go key router, the decisions gate, memory, uniichat, the web tools);
- MCP servers such as video inspection;
- dictation and voice mode.

You add, edit, switch and turn off keys in one place, and a change takes effect on the next request, with no `/reload` and no restart.

## The page

The page opens with a **Web search & fetch** box: Pi's `web_search` and `web_fetch` tools (the web-tools extension, calling DeepSeek's Search and Fetch APIs) use your DeepSeek API key. The box is always shown, key or not. It shows the active DeepSeek key masked, or that there is none yet, and **Add key** or **Update key** opens the same form as a provider row. The key is the DeepSeek provider's, so it also appears in the DeepSeek row and reaches Pi's `auth.json` like any model key.

Providers that have a key, or a Pi login, are listed first. **Add a provider's key** reveals the rest:

| Provider | Used by |
| --- | --- |
| OpenRouter | Pi models, decisions gate, video inspection, voice mode |
| OpenCode Go | Pi models. Several keys rotate as quota runs out. |
| Anthropic, xAI | Pi models |
| DeepSeek | Pi models, web search & fetch |
| OpenAI | Pi models, dictation, voice mode |
| Google Gemini | Pi models, voice mode (real-time) |
| Groq | Pi models, dictation |
| Mistral | Pi models, voice mode |
| NVIDIA | Memory embeddings |
| Deepgram | Voice mode |
| DashScope | Voice mode (real-time) |

A provider holds any number of labelled keys, and one of them is active (marked **Active**). Each key has these actions:

| Action | Effect |
| --- | --- |
| Use | Makes the key active. |
| Turn off / Turn on | A turned-off key is never used. If the active key is off, the first enabled key is used. |
| Edit | Renames the key, or replaces its secret. Leaving the secret empty keeps the current one. |
| Remove | Asks first (**Remove?**), then deletes the key. |

The page only ever shows a key's label, id and last four characters. Secrets go to the engine once, typed into a masked field, and the engine never returns them in a reply or writes them to a log.

On OpenCode Go, each key also shows what the key router last saw: Working, Out of quota (with when it resets), Cooling down, or Rejected. The key the router is on now is marked **In use now (rotated)**.

Every OpenCode Go key, disabled ones too, is also an account row under Pi in Settings → Accounts, with its own 5-hour, weekly and monthly quota from `GET https://opencode.ai/zen/go/v1/usage` (the key as its bearer token). The key Pi uses now (the router's pick, else the active key) is the row in use, and the composer's usage ring and counters show it while the session runs an `opencode-go/…` model. A rejected key reads "Key rejected", one without the Go plan "No OpenCode Go subscription on this key". The usage cache keeps the windows and a digest, never the key.

## Where keys live

The store is `<pi agent dir>/api-keys.json`, beside Pi's `auth.json`. The Pi agent directory is `$PI_CODING_AGENT_DIR`, else `~/.pi/agent`. Only you can read the file (mode 0600), and the engine is its only writer. Each write goes to a new file that then replaces the old one, so a reader never sees a half-written store.

The first time the page opens and no store exists, the literal API keys already in Pi's `auth.json` are imported, labelled "From Pi".

## How a change reaches each reader

| Reader | How it sees a change |
| --- | --- |
| Pi models | After every change, the engine writes each Pi provider's active key into Pi's `auth.json`, escaped the way Pi reads config values. Pi re-reads that file on every request, so running chats switch keys on their next request. A provider you signed in to with OAuth keeps that login and is never overwritten. When the last key is removed, the `auth.json` entry is dropped only if Zeron wrote it. |
| Pi extensions | `lib/api-keys.ts` in the Pi repo checks the file on every call and re-reads it only when the file has changed. If the file fails to parse, the last good copy is kept. Lookup order: this store, then the extension's own legacy file, then `auth.json`, then environment variables. |
| Video inspection MCP | `video_inspection/openrouter.py` reads the OpenRouter key from the store for each request, the same way. A server configured in `mcp.json` with a fixed key only sees a change after it reconnects (`/mcp reconnect`). |
| Dictation, voice mode | Each dictation and each call reads its provider's current key (Gemini's is `google` here). A key saved on Settings → Voice is added here and made active, and removing it there removes it here. `stt-keys.json` keeps a copy and is the fallback. |
| OpenCode Go router | Rotates through the provider's enabled keys and writes its per-key health, by id only, to `api-keys-health.json`. See the router's README in the Pi repo. |

From a terminal, `agent/bin/api-key.mjs` in the Pi repo lists, adds and switches keys in the same store.

## Implementation

- `zeron_engine::api_keys` owns the store, the projection into `auth.json`, the import and the stat-cached `Reader` that voice features use.
- The engine serves the RPC methods `ListApiKeys`, `AddApiKey`, `UpdateApiKey`, `SetActiveApiKey` and `RemoveApiKey`. The local engine answers them and never forwards them to another machine.
- Writes are serialized in-process. Writes to `auth.json` go through the same lock Pi uses.
- A key's id is the first eight hex digits of `sha256(secret)`. Pasted secrets lose surrounding quotes, whitespace and control characters.
- `zeron-ui::settings::api_keys` is the page. Its debug selectors are `api-keys-add-{provider}`, `api-keys-key-{provider}-{id}[-use|-toggle|-edit|-remove]`, `api-keys-form`, `api-keys-save`, `api-keys-show-all`, and `api-keys-web-tools` for the Web search & fetch box with `api-keys-web-tools-key` for its button.
- Session logs redact OpenAI, OpenRouter, NVIDIA, Groq, Google and Hugging Face key prefixes.
