# Usage tracker: research and design

Status: research complete. This is the design the implementation follows.
Scope: a native Usage page in Zeron. It counts tokens and spend for every Pi
provider and model on whichever machine the window is attached to: the
laptop, or the Linux box running `zeron headless`.

## TL;DR

- **Source.** Pi already records usage. Every assistant message in
  `<agentDir>/sessions/**/*.jsonl` carries `provider`, `model` and
  `usage {input, output, cacheRead, cacheWrite, totalTokens, cost}`.
  On the Linux box that is 621 files and 61,349 priced or pricable messages
  across 9 providers. Zeron reads those files. It needs no live events and no
  new Pi hook.
- **Provider and model are free strings taken from the records.** There is no
  enum and no registry. A provider added later through `/login` or an extension
  appears the first time it is used. t3code's closed `UsageProviderKind` is the
  pattern to avoid.
- **Scanning is incremental.** A per-file cache stores size, mtime and resume
  offset. Only appended bytes are parsed. The index is persisted in the engine's
  data dir.
- **Counting rules.**
  - Fork copies are deduped by a stable key.
  - Totals are `input + cacheRead + cacheWrite + output`.
  - Recorded cost is trusted, and a recorded $0 is classified rather than
    believed: it becomes Free, Estimated or Unpriced.
- **The engine computes the report.** The page asks the active engine, so
  attaching the window to the box shows the box's usage. Nothing is synced or
  written to Loro.

## Method

One research workflow (run `wf_feb13ea9-367`). Six agents ran on Sonnet 5.5
(`claude-sonnet-5-5`) at high effort, two at a time:
- **Researchers** (read-only, non-overlapping):
  - t3code's server usage pipeline;
  - t3code's web and mobile usage UI;
  - how Pi records and prices usage;
  - where a tracker fits in Zeron.
- **One adversarial verifier** re-read the 25 most load-bearing claims against
  source. 24 held. One was overstated and is corrected below.
- **One synthesizer.**

I added a direct inventory of the Linux box's session files. Claims are marked
**verified** (read in source) or **assumption**.

## Verified behaviour

### Pi session files (Pi 1.2.0)

- **Location and layout**
  - Path: `<agentDir>/sessions/--<cwd encoded>--/<timestamp>_<sessionId>.jsonl`.
  - `agentDir` is `PI_CODING_AGENT_DIR`, else `~/.pi/agent`.
  - Pi also honours `PI_CODING_AGENT_SESSION_DIR`, `--session-dir` and
    `settings.sessionDir`.
- **Header** (line 1): `{type:"session", version:3, id, timestamp (ISO), cwd,
  parentSession?}`.
- **Every later entry**: `{type, id (8 hex, unique only within a lineage),
  parentId, timestamp (ISO, append time)}`.
- **Usage shape**: `{input, output, cacheRead, cacheWrite, cacheWrite1h?,
  reasoning?, totalTokens, cost:{input, output, cacheRead, cacheWrite, total}}`.
  - `reasoning` is a subset of `output`.
  - `cacheWrite1h` is a subset of `cacheWrite`.
  - Old sessions may store `cost` as a bare number.

Where usage lives:

| Entry | Usage | Provider / model |
|---|---|---|
| `message`, `role:"assistant"` | `message.usage` | `message.provider`, `message.responseModel ?? message.model` |
| `usage` (`kind` free text, e.g. `cache_warm`) | `entry.usage` | `entry.provider`, `entry.model` |
| `message`, `role:"toolResult"` | `message.usage` (rare) | none |
| `compaction`, `branch_summary` | optional `usage` | none |

- Pi's own session stats count all four kinds. The existing `/usage`
  extension counts only assistant messages and summaries.
- Records with no provider go into one "Tools & summaries" row.
- `tokensBefore` on a compaction is context size, not spend. Never sum it.

### Double counting

- **Forks.** `/fork`, `/clone` and RPC fork write a **new file** with a new
  session id and `parentSession`. The file holds verbatim copies of the earlier
  entries: same entry id, timestamp and usage. On the box, 216 of 535 files
  sampled by the researcher carry copies, so summing files naively overcounts.
- **Resume.** Resuming (`--session`) appends to the same file, so it creates no
  copies.
- **Delegate children.** `dlg-*` files are seeded with a header only, so their
  spend is disjoint.
- **Correction from the verifier.** The `/usage` extension keys records on
  `provider/model:message.timestamp:tokenSum` and credits the alphabetically
  first file. The research claimed that key collapses everything at `t=0`.
  That is overstated: `t` is 0 only when both timestamps are missing. The
  wrong-file credit is real.

### Spend that never reaches a session file

- Subagent children, workflow agents and the advisor run with `--no-session`
  or in process.
- Their spend shows up only in places a session scan doesn't read:
  - `custom_message` text (`subagent-result`);
  - workflow journals (`~/.pi/workflows/projects/*/<runId>.json`);
  - `toolResult.details.usage` (advisor).
- A session-only scan therefore undercounts those. v1 says so in its coverage
  line. The adapters are listed under "Later".

### Pricing in Pi

- **Where recorded cost comes from.** Each model's
  `cost {input, output, cacheRead, cacheWrite}` (USD per million) is in Pi's
  catalog, which `get_available_models` returns. The recorded cost was computed
  from those same rates (`calculateCost`).
- **Tiers.**
  - Tiers are keyed on `inputTokensAbove` over `input + cacheRead + cacheWrite`.
  - `cacheWrite1h` bills at 2× the input rate.
  - Service-tier multipliers are folded into the recorded cost and are not
    stored separately.
- **Zero recorded cost** has three meanings:
  - free models (`opencode/*-free`, `ox-alpha-free` are 100% $0);
  - deliberately zeroed providers (clinepass records $0);
  - missing prices (custom `models.json` entries without `cost`).
- **clinepass** keeps its own calibrated table in
  `<agentDir>/clinepass-prices.json`, shaped
  `{version, catalogVersion, calibratedAt, models:{"cline-pass/glm-5.3":
  {input, output, cacheRead, calibratedAt}}}`. Model ids match the session's
  `message.model`.
- **Subscription providers** still record an API-list-equivalent cost:
  Anthropic over OAuth, `openai-codex`, `github-copilot`. Pi's footer shows it
  as "(sub)".

### Box inventory (2026-10-15)

- 621 session files, 825 MB.
- 145,035 lines, of which 61,349 are assistant messages with `usage`.

| Provider | Models | Messages | Tokens | Recorded cost |
|---|---|---|---|---|
| anthropic | 7 | 29,924 | 4.93 B | $5,077.92 |
| opencode-go | 15 | 20,718 | 3.53 B | $60.89 |
| openai-codex | 7 | 5,982 | 1.21 B | $1,326.54 |
| clinepass | 2 | 3,167 | 518 M | **$0** (zeroed; own price table) |
| opencode | 10 | 971 | 119 M | $0 (free models) |
| cursor | 5 | 431 | 35 M | $108.04 (provider no longer registered) |
| runinfra | 1 | 142 | 29 M | $0.99 |
| openrouter | 3 | 12 | 0.8 M | $0.06 |
| openai | 1 | 2 | 0 | $0 |

freeflow is installed but has no sessions yet. It will appear on first use.

### t3code (inspiration)

- **Usage page.**
  - Built only from files on disk: transcripts for Claude, Codex, Grok and Pi,
    and stores or APIs for OpenCode, Antigravity and Cursor.
  - It never uses live turn events. Live per-turn usage goes to analytics only.
- **`UsageRecord`**: `{provider, timestampMs, model, rateModel?, sessionId,
  totals{uncachedInput, cachedInput, cacheCreation, output, reasoning},
  reportedCostUsd|null, speed, dedupeKey|null}`.
  - Buckets are `(day, hour?, provider, model, sourcePath?)`.
  - Costs are in micro-USD.
- **Incremental cache** (`usage-scan-cache-v5.json`): `size`, `mtime`, resume
  `offset`, and a 64-byte tail guard per transcript file.
  - A shrink, rewrite or guard mismatch forces a full reparse.
  - Rows survive file deletion for 92 days.
- **Pricing.**
  - LiteLLM's table plus user overrides, which always win.
  - `cost <= 0` is treated as unreported and estimated, which invents costs for
    free models.
- **New providers.** A new **model** is counted automatically. A new
  **provider** needs code in four places, because of the closed enum.
- **UI.**
  - Cost, Tokens and Limits tabs.
  - Period 24h / 7d / 30d / 90d.
  - A hero total with provider rows (colour dot, share, value) beside a
    provider chart.
  - A totals strip; share bars by token type.
  - A models table with a model dialog and a price-override editor.
- **What to keep and what to fix.**
  - Keep: the stale-while-loading dimming, stable provider colours, top-N plus
    "Other".
  - Fix: `$0.00` for sub-cent amounts. Show `<$0.01`.
  - Fix: twelve overlapping filled areas, which become mud with many providers.
  - Fix: the closed provider enum.

### Zeron today

- **No token or cost tracker.**
- **Existing "usage" surfaces answer other questions:**
  - plan and rate-limit meters (`settings/accounts.rs`, `account_usage.rs`,
    `AgentAccountsSnapshot`);
  - the context ring (`context_usage.rs`);
  - the Pi `/usage` extension screen.
- **`AgentEvent::Usage`** has no model, cost or timestamp, and is not
  persisted. It is useless for counting.
- **RPC.** Methods are `zeron_rpc::methods::*` strings handled in
  `EngineRpc::handle`.
  - Remote devices are reached by the generic `targetDeviceId` forwarder when a
    method is listed in `forwardable()`.
  - With the window attached to a machine over SSH, `state.engine()` already is
    that machine's engine.
- **Settings sections** are the `SettingsSection` enum in `shell.rs`.
  `ZERON_OPEN_ROUTE=settings/<slug>` opens one.
- **Charting.** `markdown::chart` draws bar, line and area charts natively with
  ticks, hover and legend toggles. It has no stacked bar.

## Decisions

| Question | Decision |
|---|---|
| Where it runs | Engine (`crates/engine/src/usage/`). The UI asks the active engine with `GetUsageReport`, which is forwardable to remote devices. |
| Roots scanned | `PI_CODING_AGENT_SESSION_DIR` and `<agentDir>/sessions`, deduped. Recursive, symlinks not followed, capped at 200k entries. |
| Entry kinds | Assistant messages, `usage` entries, `toolResult` usage, `compaction` and `branch_summary`. |
| Dedupe key | 64 bits of sha256 over `(entry.id, entry.timestamp, provider, model, input, output, cacheRead, cacheWrite, kind)`. Missing id: `message.timestamp + responseId`. |
| Credit | Lowest `(header timestamp, path)` wins. A fork is always created after its parent, so the original keeps the shared prefix. |
| Totals | `input + cacheRead + cacheWrite + output`. Reasoning and `cacheWrite1h` are never added. |
| Cost basis | **Recorded** (`cost.total > 0`), **Estimated** (`$0` recorded, a rate is known and non-zero), **Free** (`$0` recorded, every known rate is 0), **Unpriced** (no rate). Recorded and estimated are summed separately. |
| Rate sources, in order | `<agentDir>/<provider>-prices.json` (a provider's own table; clinepass ships one), then `models.json` model `cost`; a model named `*-free`/`:free`/`cline-free/*` is Free; then the same model under another route of that provider (`deepseek/deepseek-v4.1-flash` → `cline-pass/deepseek-v4.1-flash`, paid route first); then a rate learned from that model's recorded requests, then from the same model on another provider. Pi's built-in catalog cost is not probed in v1: built-in providers record their own cost. |
| Subscription spend | Recorded as API-equivalent and labelled so. It is never hidden. |
| Index | `<data_dir>/usage/usage-index-v1.json`, written atomically. Rows are positional arrays with interned strings. Rows of deleted session files are kept. |
| Time | Assistant messages use `message.timestamp` (ms). Other entries use the entry's ISO timestamp. Days are bucketed in the requester's UTC offset. |
| UI | Settings → Usage.<br>• Period: 24h / 7d / 30d / 90d / All.<br>• Metric: Cost / Tokens.<br>• Hero: total with provider rows, beside a provider line chart.<br>• Totals strip.<br>• Models table: `$x`, `~$x` (estimated), Free, Unpriced.<br>• Coverage line. |

## Edge cases

- **Torn last line.** A session being written can end without `\n`. Never
  advance the resume offset past the last newline.
- **Same-size rewrite or shrink.** Reparse the whole file.
- **Huge lines.** Images and tool output can make lines megabytes long.
  - Prefilter on the bytes `"usage"` before parsing JSON.
  - Deserialize into a projection struct, so content is never stored.
- **Model ids.** They can contain `/` (`openrouter/deepseek/...`). Provider is a
  separate field; never split.
- **`:thinking` suffix.** Strip it for grouping.
- **Test providers.** Drop `faux-provider` and `fake-provider`.
- **Aborted turns.** Drop records whose tokens and cost are all zero.
- **Empty sessions.** Files with no counted record are not sessions in the
  tally. On the box, 122 of 535 have no assistant message.
- **Windows paths.** Cwds can carry a `\\?\` prefix. Normalize before grouping.
- **Old peers.** A machine whose engine predates `GetUsageReport` answers
  `UnknownMethod`. The page says "Update Zeron on that machine".
- **Price changes.** Today's price is used for old estimated rows. Recorded
  rows are never re-priced.


## Every surface (v2)

Subagents, workflow agents and the advisor run Pi with `--no-session`, and many extensions (jev, decisions, memory, handoff, goal judge, btw, vision proxy) call models without writing anything. Research on 2026-10-15 found ~$595 of subagent spend on the box that v1 could not see.

- **Pi side:** the `00-usage-ledger.ts` extension in ErsatzHitman/pi appends one JSON line per LLM call to `<agentDir>/usage-ledger/<yyyy-mm>.jsonl`.
  - It hooks pi-ai's `AssistantMessageEventStream`, with a `message_end` safety net.
  - Every spawner that uses `--no-extensions` loads it with `-e`.
  - Calls that bypass pi-ai use `recordUsage()`.
  - Each line names its `surface`: `main`, `subagent`, `workflow`, `advisor`, `compaction`, `jev`, `ext:<name>`, and so on.
- **Zeron side:**
  - The scanner reads ledger files incrementally, like sessions.
  - A ledger call that a session file also holds is matched on provider, model, message timestamp and tokens, and is counted once (session first).
  - A cache warm is matched to its `usage` entry within 120 s.
  - `kept:true` lines (summaries Pi also persisted) are skipped.
- **Before the ledger (T0, its first `start` marker),** spend is recovered from what parents logged:
  - the `subagent-result` delivery text (`- Usage: …`, `- Launch model: …`);
  - advisor `toolResult.details.usage`;
  - workflow journals (each attempt of each agent that actually ran).

  From T0 on, these are ignored, because the ledger has the calls themselves.
- **Summaries:** a compaction or branch summary is credited to the model of the session's latest assistant message, since Pi stores its usage without one.
- **Zeron's own title runs** load the ledger extension by name and are labelled `zeron-titles`.
- **Not counted:**
  - non-Pi harness chats, which bill their own accounts;
  - model calls inside third-party MCP servers;
  - processes started without the extension by code that skips `withUsageLedger`.

## Later (not v1)

- **Adapters for spend outside session files**, each a visible toggle:
  - subagent-result text;
  - workflow journals (skip `cached:true` agents and `.bak` files);
  - advisor `details.usage`.
  - The durable fix is upstream: have those extensions append a
    `type:"usage"` entry per settled child.
- **"All machines"**: merge additive reports from several engines, deduped by
  `device_id`.
- **Price-override editor and model dialog** (t3code's
  `UsagePriceOverrides`).
- **Per-chat breakdown and a footer chip.**

## Test plan (CI only)

- **Scanner.**
  - Every entry kind.
  - `cost` given as a bare number or as an object.
  - Torn tail, then completion on rescan.
  - Shrink and rewrite force a full reparse.
  - Resumed scans equal a full scan.
  - A never-seen `acme-llm/x-1` provider appears with no code change.
  - Message text never reaches the index.
- **Dedupe.**
  - A parent plus a fork child counts the prefix once and credits the parent.
  - Distinct entries that share an id across sessions are both kept.
- **Pricing.**
  - Recorded, Estimated, Free and Unpriced.
  - Provider price file beats catalog, which beats learned rates.
  - Tier selection.
  - `cacheWrite1h` at 2× input.
- **Aggregation.**
  - Day buckets at a non-zero UTC offset.
  - Hour buckets for 24h.
  - Totals never add reasoning.
- **UI.**
  - Formatter goldens: `19.9B`, `76.7M`, `804K`, `<$0.01`, `<0.1%`.
  - The page renders Loading, Ready and Unsupported.
  - The settings route `settings/usage` opens.
