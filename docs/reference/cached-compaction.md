# Cached compaction settings

Settings → General → Cached compaction is the card for the cached-compaction
extension: a compaction that resends the exact last request plus one
`summarize` instruction, so the model reads its prompt cache (~1/10 the price)
instead of the whole conversation, and the summary sees the full tool output.
The card shows and edits the extension's own switches, in the file it reads.

Learn more: `docs/pi.md` (how a Pi chat runs), and the extension's own
documentation under `agent/extensions/cached-compaction/` in the Pi tree.

## The file

`<Pi agent dir>/cached-compaction.json` — `~/.pi/agent/cached-compaction.json`
unless `PI_CODING_AGENT_DIR` says otherwise. The engine writes it
(`GetCachedCompactionSettings` / `SetCachedCompactionSettings`) and the
extension re-reads it at every compaction, at most two seconds stale
otherwise, so a save reaches running chats at their next compaction without a
restart. A file that is not a JSON object is never overwritten: saving fails
until it is fixed or removed.

Pi's `settings.json` is never written: Pi and several extensions write it, and
a second writer turns one toggle into a lost update. The file follows the same
shared-file pattern as `subagents.json` (Settings → General → Default
subagent) and `loop.json` (Settings → General → Advisor).

Every key the card does not own — the extension's `log`, `strictPrefix`, and
any key a later release adds — is kept exactly as it is. Writes are pretty
JSON with a trailing newline, renamed into place, so the extension never reads
half a file.

## The rows

| Row | Key | Default | What it does |
| --- | --- | --- | --- |
| Enabled | `enabled` | `true` | Master switch. Off always compacts the way Pi does: a standalone request with the transcript serialized into it. |
| Compact before the cache expires | `compactBeforeExpiry` | `true` | Compact while the chat is idle, shortly before the prompt cache entry expires, instead of waiting for the context threshold. Never fires while a prompt is queued or the agent is working. Its own switch (`preExpiry`, the timer and countdown build) is written with it, exactly as `/cached-compaction expiry on\|off` writes both. |
| Minimum context | `minContextTokens` | `50000` | The idle timer never compacts a conversation smaller than this. Clamped to at least `1000` on save. |
| Lead time | `leadMs` | `20000` | How long before the cache entry expires the idle timer fires, in seconds on the card and milliseconds in the file. Clamped to 5s–3600s on save (3600s is the longest prompt-cache tier any provider reports). |
| Cache retention | `cacheRetention` | `auto` | `auto`, `short` or `long`. `auto` asks for `long` in a chat a human is watching and `short` in a sub-agent or headless run, which nobody is watching. An unknown value is refused on save; a file that already holds one reads as `auto`. |
| Cost card | `costCard` | `true` | Append the cached-vs-native cost comparison to the chat after each compaction. Both sides are estimates. |
| Footer countdown | `footerCountdown` | `true` | Publish the cache countdown that Zeron's composer footer chip and the session panel's Compaction section draw. |

The values on the card are what the extension will act on: a save is
normalized (the clamps above) and the reply is what the file now sets, so a
clamped value shows as clamped rather than as typed.

### Reading the switches

- A compaction only takes the cached path while the conversation is unchanged
  since the captured request. An edited message, a rewind, a `/tree` fork, a
  `/fork`, a model change or an expired cache entry falls back to Pi's own
  compaction, which resends the transcript. Off is the same as always-native.
- `long` retention bills cache writes at 2x on Anthropic. It pays back in a
  chat that stays warm and costs more in a one-off; `auto` is the safe choice.
- With no known cache lifetime (custom providers, most local models) there is
  no countdown and no pre-expiry compaction; the cached summary still applies.
  Declaring a lifetime in `models.json` under `promptCache` is the lever.

## From a terminal

The same values, without Zeron, through the extension's command in Pi:

```
/cached-compaction status                     cache now, next path and why, this session's log
/cached-compaction on | off                   master switch (`enabled`)
/cached-compaction min <tokens>               context floor (`minContextTokens`)
/cached-compaction expiry on | off            idle compact-before-expiry (`compactBeforeExpiry` + `preExpiry`)
/cached-compaction retention auto|short|long  cache tier (`cacheRetention`)
/cached-compaction log                        log path and the last decisions
```

`/cached-compaction` does not write `costCard` or `footerCountdown`; those are
the Zeron card's own rows. A hand edit of the file is picked up within two
seconds. The card re-reads on opening Settings, so a change made from a
terminal shows the next time the page is opened.

## RPC

| Method | Params | Reply |
| --- | --- | --- |
| `GetCachedCompactionSettings` | `{}` | the device's `CachedCompactionSettings` (the defaults when the file is missing) |
| `SetCachedCompactionSettings` | a whole `CachedCompactionSettings` | what the file now sets (normalized) |

Both are device-addressable (`targetDeviceId`): the file is on the device that
runs Pi, and the card follows the composer's device. `CachedCompactionSettings`
is camelCase — `enabled`, `compactBeforeExpiry`, `minContextTokens`, `leadMs`,
`cacheRetention`, `costCard`, `footerCountdown` — and a save replaces those
values, keeping every other key.
