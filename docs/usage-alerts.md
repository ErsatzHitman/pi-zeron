# Subscription usage alerts

When a signed-in (OAuth) subscription is almost used up, every session on that
account is told to finish its current safe step, write a handoff and end, and
no new work is started on it. Other accounts stay usable.

## One reader

Zeron has one usage reader: `AgentAccounts::list` (`crates/engine/src/agent_accounts.rs`,
probes in `agent_accounts/usage.rs`), the snapshot behind the composer footer
("5h 98% · wk 90% left", `crates/ui/src/account_usage.rs`) and Settings →
Accounts, with its cache (`agent-accounts/usage-cache.json`), throttle and
backoff. `crates/engine/src/usage_alerts.rs` polls it every 5 minutes (forced,
like the footer) and evaluates the result. Nothing else reads usage.

## Coverage

| Pi `auth.json` login | Kind | Usage read | Monitored |
|---|---|---|---|
| `anthropic` | OAuth | Claude `/api/oauth/usage` (5h, week) | yes |
| `openai-codex` | OAuth | Codex usage endpoint (5h, week) | yes |
| `github-copilot` | OAuth | Copilot premium-request quota (month) | yes |
| `openai` | OAuth | none | **no: unknown** |
| `clinepass` | OAuth | none | **no: unknown** |
| API keys (`opencode-go`, `openrouter`, `runinfra`, …) | API key | no plan windows | n/a |

Unsupported OAuth logins are listed in the snapshot's `unsupported` field. They
are unknown, not "fine": nothing warns before they run out.

## Semantics

- **Threshold.** Alert when `remaining% = (1 − used) × 100` is strictly below
  `T` in any reported window. `T` defaults to 10; set
  `ZERON_USAGE_ALERT_THRESHOLD` on the engine (0–100, `0` disables). Exactly
  10% left does not alert; 9.99% does.
- **Unknown never blocks.** No windows, a failed probe with no recent last-good
  numbers, or a snapshot older than 6 hours: no alert, nothing blocked.
- **Reset.** A window whose `resets_at` has passed is over: it cannot alert.
  The guard also ignores a snapshot file older than `validForSecs` (15 min), so
  a stopped engine never blocks work.
- **Scope.** Per active account per provider. A Pi session is affected only
  when its model's provider (`anthropic/…`, `openai-codex/…`,
  `github-copilot/…`) is the provider of the low account; a session left on
  Pi's own default model counts as the provider `defaultProvider` in Pi's
  `settings.json` names. That is the account
  Pi uses: Pi keeps one login per provider in `auth.json`, and the accounts
  snapshot marks that entry `active`. Saved, inactive slots never dispatch.
  Only this device's chats are told (another device has its own logins).
- **Dedup.** Alert key = harness + provider + account + window + reset time.
  Each chat is told once per key; the record (`usage-alerts-state.json`) lasts
  until the window resets (7 days without a reset time), so flapping around
  `T`, a failed probe or a restart never re-tells. A new window period is a new
  key.

## Delivery

| Surface | How it is told | New work |
|---|---|---|
| Main session, working | engine steer | guard blocks spawns on the low account |
| Main session, waiting on a question | engine note held for the turn's end | same |
| Main session, idle | not woken; told by the engine and the guard when its next turn starts | same |
| btw pane side chat | as a main session (it is a chat) | same |
| Orchestrator tree member | as a main session | same |
| Orchestrator root | a note listing affected members (idle root gets a turn, unless paused), even when the root runs on another provider | guard blocks `zeron_create_chat(s)` with a model on the low account; other providers allowed |
| Sub-agent / delegate / subprocess workflow agent (child Pi) | guard steers it once while it runs | guard blocks its own spawns |
| Advisor (child Pi) | guard tells it to answer briefly | `advisor` tool refused for a low session |
| Pi `/btw`, in-process workflow agents | the Pi config (`lib/usage-alerts.ts`) steers the in-process session | refused before start |
| Standalone Pi (not started by Zeron) | Pi config `usage-alerts.ts` reads the same file | same blocks |

The note: finish the step you are on as quickly as is safe, never abandon a
half-done action that would leave things broken (finish it or roll it back),
start nothing new, write a handoff to
`<cwd>/.pi/handoffs/usage-<provider>-<UTC stamp>.md` (goal, done, in progress
and its exact state, next steps, branches/commits/files, verify commands),
tell your sub-agents to wrap up, end the turn with the handoff path. Nothing
is aborted or killed.

## Where the snapshot is

`{data_dir}/usage-alerts.json` (default `~/.zeron/usage-alerts.json`). Zeron
names it in `ZERON_USAGE_ALERTS_FILE` for every Pi it starts; the guard's
preload restores it for children started without the parent's env. Readers
fall back to `$ZERON_DATA_DIR/usage-alerts.json`, then
`~/.zeron/usage-alerts.json`, so a standalone Pi needs no `~/.pi` change. The
file has no tokens or emails: provider, slot id, plan, window, remaining %,
reset time.

## Limits

- Pis started before this release run the old guard: they get the engine's
  chat notes but not the in-process checks until they restart.
- A `zeron_send_message` to an existing chat on a low account is not refused;
  that chat was already told and gets the note again when its next turn starts.
- A sub-agent started with no model resolves its model inside the Pi config
  (`subagent_start` checks the effective model); the guard alone only sees
  models named in the call.
- The guard tells a session at depth 0 when its next turn starts and the engine
  tells it while it works, so one chat can see the note twice around a turn
  boundary.
