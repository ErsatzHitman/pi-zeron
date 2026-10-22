# Usage limits

Settings → Usage limits shows the plan rate limits of **every** connected
subscription on this machine in one list: the plan each login is on, every
window the provider reports (how much of it is used and when it resets), and
the engine's reason when a probe failed or a login has no quota view. It is
the consolidated view of the numbers Settings → Providers shows per provider
and the composer footer ring shows for the running session.

Two neighbouring pages are deliberately different:

| Page | Shows |
| --- | --- |
| Settings → Usage limits | Plan rate limits (5-hour / weekly / monthly windows) per subscription. |
| Settings → Usage | Tokens and spend per Pi provider and model, from the sessions on this machine (`GetUsageReport`) — accounting, not quotas. |
| Settings → Providers | Per-provider accounts, login, Switch and Forget. It keeps the device switcher. |

## Where it lives

The page is the settings section **Usage limits** (slug `usage-limits`). It
opens from the Settings sidebar, from a link, or with
`ZERON_OPEN_ROUTE=settings/usage-limits`. The old `limits` alias resolves here too. As
with the other sections, the choice is remembered in `ui-settings.json`
(`settingsSection`).

## What it shows

One card per subscription, most-used window first:

- the product name behind the login (Claude, ChatGPT (Codex), OpenCode Go,
  Cline, GitHub Copilot, Nous Portal, Grok, or the provider's own name);
- the plan badge (`plan_label`), e.g. `Copilot Pro`, `OpenCode Go`,
  `ClinePass`, when the provider reports one;
- **In use now** on the login the session actually runs on;
- the account identity (email, display name, organization, account id);
- every reported window as a bar with its percentage **used** and its reset
  time. Bars use the same thresholds as Providers: indigo below 80 %, amber
  from 80 %, red from 95 %. A window whose reset time has passed reads 0 %
  with "reset, refreshing" until the next probe;
- the engine's reason when there are no windows or the last probe failed
  ("API keys have no plan usage", "No usage view for this login", "Rate
  limited by Anthropic — retrying in 2m", "Signed out — sign in again");
- when the windows were last fetched ("checked just now" / "checked 12m ago"
  / "never checked").

The card never rewrites a window's label: the raw engine label
(`Session`, `Week`, `5h`, `Month`, `Premium`, …) is kept so this page and the
Providers meters can never disagree.

Below the subscriptions, two secondary groups:

- **Connected, no usage view** — Pi logins the provider reports no quota for;
- **Other agent logins** — the other agents detected on this device (Claude
  Code, Codex, Cursor, Grok, Devin, OpenCode, Hermes), shown read-only. Zeron
  runs Pi sessions, so switching or forgetting an account stays in
  Settings → Providers.

An empty list reads "No subscriptions connected. Connect one in Settings →
Providers."

## Which logins are covered

The engine probes the same logins it lists for Settings → Providers; the
page is only a view of that snapshot.

| Login | Provider id | Windows |
| --- | --- | --- |
| Claude | Pi `anthropic` | Session (5h), Week |
| ChatGPT (Codex) | Pi `openai-codex`, OpenCode `openai`, Hermes `openai-codex` | Session (5h), Week |
| OpenCode Go | Pi `opencode-go` (one row per key in `api-keys.json`) | 5h, Week, Month |
| Cline | Pi `clinepass` | 5h, Week, Month |
| GitHub Copilot | Pi / OpenCode `github-copilot` | Premium / Chat / Completions (monthly reset) |
| Nous Portal | Hermes `nous` | Month |
| Cursor | Cursor | Month (billing cycle) |
| Grok | Grok | Day / Week / Month |
| Devin | Devin | Day, Week |
| Antigravity | Antigravity | none — not listed at all |

Not covered, and shown as unknown rather than fine:

- Pi `openai` (an OpenAI OAuth login) has no usage probe;
- a raw API key whose provider has no quota endpoint ("API keys have no plan
  usage") — that is not the same as "nothing used";
- a login whose token expired or that the engine could not reach keeps its
  last good windows and shows the engine's reason beside them.

Alerts (Pi's own guard and `usage-alerts.json`) are documented in
[docs/usage-alerts.md](../usage-alerts.md); the coverage table there is the
alerting contract, and this page is the UI view of the same probes.

## Ordering

Cards are ordered by the most-used window still current, then by name, then
by id. Unlike the Providers cards — which keep the engine's slot order so a
Switch never moves a row — this is a diagnostic list, so the tightest quota
is at the top.

## Refresh

Opening the page forces exactly one `ListAgentAccounts` probe per visit (the
Refresh button forces another). There is no timer.

- The page first paints the shared accounts snapshot if one is in the
  session (`AccountsSnapshotCache`, shared with Settings → Providers and
  the composer footer), or the engine's plain list — last known usage, no
  network — then replaces it with the probed list. A failed revalidation
  keeps the painted rows and reports the failure as a toast.
- The engine throttles forced probes: an account probed in the last 30
  seconds is skipped, and a failing account is backed off (Retry-After is
  honoured; a rate limit backs off up to an hour, a rejected token 10
  minutes, and new credentials lift the backoff early).
- Last-good windows persist to `agent-accounts/usage-cache.json`, so the
  first paint after a restart is instant. The engine's own alert poll
  re-probes every 5 minutes, so a plain list is never far behind.

Because the page, Providers and the footer all read the same
`ListAgentAccounts` snapshot, a Switch made in Providers is visible here and
in the footer on the next paint.

## This device

The page reads the machine this window's engine is attached to. A remote
machine's logins are not shown; device switching lives on Settings →
Providers (`targetDeviceId`). Switching or forgetting an account also stays
there.

## Implementation

- `zeron-ui::settings::usage_limits` is the page. It reuses the Providers
  helpers (`current_window`, `format_reset`, `usage_color`, `usage_level`,
  `reports_usage`, `force_usage_for`) so the meters cannot drift, and reads
  and writes the shared `AccountsSnapshotCache`.
- The debug selectors are `usage-limits-refresh` and
  `usage-limits-page-scrollbar`.
- The section is `SettingsSection::UsageLimits` in `zeron-ui::shell`
  (slug `usage-limits`, label "Usage limits").
- No engine change is needed: `ListAgentAccounts` already returns each
  account's `plan_label`, `usage_windows`, `usage_fetched_at` and
  `usage_error`. A new provider probe appears on this page automatically.
