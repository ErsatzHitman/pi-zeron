//! Subscription usage alerts: when a subscription's plan usage falls below a
//! threshold — a signed-in (OAuth) account or an API key that carries a quota
//! view, such as an OpenCode Go key — every session running on that account is
//! told to finish its current safe step, write a handoff and end, and no new
//! work is started on it.
//!
//! There is ONE usage reader: [`AgentAccounts::list`], the same snapshot the
//! composer footer ("5h 98% · wk 90% left", `zeron-ui` `account_usage`),
//! Settings → Providers and Settings → Usage limits render, with its cache,
//! throttle and backoff. This module only polls it (forced, like the footer's
//! 5-minute poll) and evaluates what it returns.
//!
//! Semantics, all pure and tested below:
//!
//! - **Threshold**: an alert when `remaining% = (1 - used) * 100` is strictly
//!   below `T` in ANY window the account reports (5h, week; Copilot's month).
//!   Default `T = 10`; `ZERON_USAGE_ALERT_THRESHOLD` (0–100, `0` disables).
//!   Exactly `T`% left does not alert.
//! - **Unknown never alerts or blocks**: no windows (an OAuth login whose
//!   provider has no probe, or an API key with no quota view), or a snapshot
//!   older than [`MAX_SNAPSHOT_AGE`] (the engine serves the last good probe
//!   through failures; past that age it is not trusted). An API key that DOES
//!   report windows is a subscription like any other and is evaluated the
//!   same way.
//! - **Reset**: a window whose `resets_at` has passed is reset — it cannot
//!   alert on old numbers.
//! - **Scope**: per active account per provider. A Pi session is affected
//!   only when its model's provider (`anthropic/…`, `openai-codex/…`,
//!   `github-copilot/…`, `clinepass/…`, `opencode-go/…`) is that account's
//!   provider; a session on Pi's own
//!   default model runs on the provider Pi's `settings.json` names. Saved,
//!   inactive slots are listed, never dispatched.
//! - **Dedup**: alert key = harness + provider + account + window + reset
//!   time. Each chat is told once per key; the record is kept until the
//!   window resets (or [`DELIVERED_TTL`] for windows with no reset time), so
//!   a value flapping around `T` or a failed/stale probe never re-tells.
//!   Recorded in `usage-alerts-state.json`, so a restart does not re-tell.
//!
//! Delivery (this device's chats only — another device has its own logins):
//! a Working chat gets a steer; one waiting on a question gets the note held
//! for its turn's end (it never answers the question); an idle chat is not
//! woken (it has nothing in flight, and a wake spends the scarce quota) and
//! is told when it next works. An orchestrator root is told about every
//! affected member of its tree (an idle root gets a turn, so it stops
//! dispatching) unless the user paused it.
//!
//! The Pi side: the snapshot is written to `{data_dir}/usage-alerts.json`
//! (no tokens, no emails) and named in `ZERON_USAGE_ALERTS_FILE` for every
//! Pi Zeron starts; the bundled guard (`zeron-harness` `pi/guard.mjs`)
//! carries it into every Pi below (sub-agents, delegates, workflow agents),
//! tells running ones, and blocks new work aimed at an affected account.
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use zeron_doc::SessionCommandPayload;
use zeron_proto::{
    AgentAccount, AgentAccountsSnapshot, AgentAuthKind, Chat, HarnessId, SessionStatus,
};

use crate::agent_accounts::AgentAccounts;
use crate::doc_host::DocHost;
use crate::sessions::SessionsEngine;
use crate::workspace_host::WorkspaceHost;

pub const FILE_NAME: &str = "usage-alerts.json";
pub const STATE_FILE_NAME: &str = "usage-alerts-state.json";
pub const ENV_THRESHOLD: &str = "ZERON_USAGE_ALERT_THRESHOLD";
pub const DEFAULT_THRESHOLD_PCT: f64 = 10.0;
/// The footer polls this often too.
const POLL_INTERVAL: Duration = Duration::from_secs(5 * 60);
/// First check after startup: the engine is busy reviving chats before that.
const FIRST_DELAY: Duration = Duration::from_secs(20);
/// A last-good snapshot older than this is unknown, not low.
pub const MAX_SNAPSHOT_AGE: chrono::Duration = chrono::Duration::hours(6);
/// How long a delivery is remembered for a window with no reset time.
pub const DELIVERED_TTL: chrono::Duration = chrono::Duration::days(7);
/// Floating point slack: 90% used must read as exactly 10% left.
const EPSILON: f64 = 1e-3;

/// The threshold from env: percent remaining, `None` when disabled.
pub fn threshold_from_env(value: Option<&str>) -> Option<f64> {
    let Some(raw) = value.map(str::trim).filter(|v| !v.is_empty()) else {
        return Some(DEFAULT_THRESHOLD_PCT);
    };
    match raw.trim_end_matches('%').parse::<f64>() {
        Ok(t) if t.is_finite() && t <= 0.0 => None,
        Ok(t) if t.is_finite() => Some(t.min(100.0)),
        _ => {
            tracing::warn!(
                value = raw,
                "{ENV_THRESHOLD} is not a number; using the default"
            );
            Some(DEFAULT_THRESHOLD_PCT)
        }
    }
}

fn harness_slug(harness: HarnessId) -> String {
    serde_json::to_value(harness)
        .ok()
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_else(|| format!("{harness:?}").to_ascii_lowercase())
}

/// One low window of one active account.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageAlert {
    pub key: String,
    pub harness: HarnessId,
    /// Pi's auth store (or API-key store) provider key — `anthropic`,
    /// `openai-codex`, `github-copilot`, `clinepass`, `opencode-go`;
    /// `None` for single-login harnesses.
    pub provider: Option<String>,
    pub account_id: String,
    pub plan: Option<String>,
    pub window: String,
    pub remaining_pct: f64,
    pub resets_at: Option<DateTime<Utc>>,
    pub fetched_at: Option<DateTime<Utc>>,
}

/// What is known about one active signed-in account.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Coverage {
    pub harness: HarnessId,
    pub provider: Option<String>,
    pub account_id: String,
    /// `ok`, `low` or `unknown`.
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Evaluation {
    pub alerts: Vec<UsageAlert>,
    pub coverage: Vec<Coverage>,
}

/// The windows the footer names alike.
pub fn window_name(label: &str) -> String {
    match label.trim().to_ascii_lowercase().as_str() {
        "session" | "5h" | "5-hour" | "five hour" => "5h".into(),
        "week" | "weekly" | "7d" => "wk".into(),
        "month" | "monthly" | "30d" => "mo".into(),
        "day" | "daily" | "24h" => "day".into(),
        other => other.to_owned(),
    }
}

/// Is `remaining_pct` below the threshold? Strict, with float slack.
pub fn below(remaining_pct: f64, threshold_pct: f64) -> bool {
    remaining_pct < threshold_pct - EPSILON
}

fn ms_to_time(ms: i64) -> Option<DateTime<Utc>> {
    DateTime::<Utc>::from_timestamp_millis(ms)
}

/// Evaluate the accounts snapshot. Pure.
pub fn evaluate(
    snapshot: &AgentAccountsSnapshot,
    now: DateTime<Utc>,
    threshold_pct: f64,
) -> Evaluation {
    let mut out = Evaluation::default();
    for account in snapshot.accounts.iter().filter(|a| a.active) {
        // An API key with no quota view (openrouter, runinfra, Devin's bare
        // key, …) is not a subscription: nothing to read, nothing to alert.
        // A key that DOES report windows — an OpenCode Go key — is one, and is
        // evaluated exactly like an OAuth login below.
        if account.auth_kind == Some(AgentAuthKind::ApiKey) && account.usage_windows.is_empty() {
            continue;
        }
        let coverage = |status: &str, reason: Option<String>| Coverage {
            harness: account.harness,
            provider: account.provider.clone(),
            account_id: account.id.clone(),
            status: status.into(),
            reason,
        };
        let fetched = account.usage_fetched_at.and_then(ms_to_time);
        if account.usage_windows.is_empty() {
            let why = account
                .usage_error
                .clone()
                .unwrap_or_else(|| "no usage windows reported".into());
            out.coverage.push(coverage("unknown", Some(why)));
            continue;
        }
        if fetched.is_none_or(|at| now - at > MAX_SNAPSHOT_AGE) {
            out.coverage.push(coverage(
                "unknown",
                Some("usage snapshot is stale (no successful probe in 6h)".into()),
            ));
            continue;
        }
        let low = low_windows(account, now, threshold_pct, fetched);
        out.coverage
            .push(coverage(if low.is_empty() { "ok" } else { "low" }, None));
        out.alerts.extend(low);
    }
    out
}

fn low_windows(
    account: &AgentAccount,
    now: DateTime<Utc>,
    threshold_pct: f64,
    fetched: Option<DateTime<Utc>>,
) -> Vec<UsageAlert> {
    let mut out = Vec::new();
    for window in &account.usage_windows {
        // Reset since: the numbers describe a window that is over.
        if window.resets_at.is_some_and(|at| at <= now) {
            continue;
        }
        let used = f64::from(window.used_fraction).clamp(0.0, 1.0);
        let remaining_pct = ((1.0 - used) * 100.0 * 100.0).round() / 100.0;
        if !below(remaining_pct, threshold_pct) {
            continue;
        }
        let name = window_name(&window.label);
        let key = format!(
            "{}:{}:{}:{}:{}",
            harness_slug(account.harness),
            account.provider.as_deref().unwrap_or("-"),
            account.id,
            name,
            window
                .resets_at
                .map_or_else(|| "-".to_owned(), |at| at.timestamp().to_string()),
        );
        out.push(UsageAlert {
            key,
            harness: account.harness,
            provider: account.provider.clone(),
            account_id: account.id.clone(),
            plan: account.plan_label.clone(),
            window: name,
            remaining_pct,
            resets_at: window.resets_at,
            fetched_at: fetched,
        });
    }
    out
}

/// The provider a chat's model runs on: `anthropic` of
/// `anthropic/claude-opus-5-5`. `None` for the harness default.
pub fn model_provider(model: Option<&str>) -> Option<String> {
    let model = model?.trim();
    let (provider, rest) = model.split_once('/')?;
    (!provider.is_empty() && !rest.is_empty()).then(|| provider.to_ascii_lowercase())
}

/// Is this the model Pi picks for itself (`default`), rather than one of a
/// provider Zeron named?
pub fn is_harness_default(model: Option<&str>) -> bool {
    model.is_none_or(|model| model.trim().eq_ignore_ascii_case("default"))
}

/// The provider Pi's own default model runs on: `defaultProvider` in its
/// `settings.json` (beside `auth.json`), else the `provider/` prefix of
/// `defaultModel`. `None` when Pi's settings name neither.
pub fn pi_default_provider(settings_file: &Path) -> Option<String> {
    let text = std::fs::read_to_string(settings_file).ok()?;
    let settings: serde_json::Value = serde_json::from_str(&text).ok()?;
    let named = |key: &str| {
        settings
            .get(key)
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|v| !v.is_empty())
    };
    let provider = match named("defaultProvider") {
        Some(provider) => provider.to_owned(),
        // An older Pi may only name the model; a `provider/id` names its own.
        None => return model_provider(named("defaultModel")),
    };
    Some(provider.to_ascii_lowercase())
}

/// Does `alert` concern a session of `harness` on `model`? A session on the
/// harness's own default model runs on `default_provider` (Pi's configured
/// one; there is no provider to read off a `default` model).
pub fn affects(
    alert: &UsageAlert,
    harness: HarnessId,
    model: Option<&str>,
    default_provider: Option<&str>,
) -> bool {
    if alert.harness != harness {
        return false;
    }
    match &alert.provider {
        // Single-login harness: its one active login is every session's.
        None => true,
        Some(provider) => {
            let served = match (model_provider(model), is_harness_default(model)) {
                (Some(served), _) => Some(served),
                (None, true) => default_provider.map(str::to_ascii_lowercase),
                (None, false) => None,
            };
            served.as_deref() == Some(provider.as_str())
        }
    }
}

// ───────────────────────────── the snapshot file

/// What `usage-alerts.json` holds. Read by the Pi guard; never secrets.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AlertsFile {
    pub version: u32,
    pub updated_at: DateTime<Utc>,
    /// The guard ignores a file older than this many seconds (the engine
    /// stopped), so a dead engine never blocks work forever.
    pub valid_for_secs: u64,
    pub threshold_pct: Option<f64>,
    pub alerts: Vec<UsageAlert>,
    pub coverage: Vec<Coverage>,
    /// Logins this reader cannot see usage for at all.
    pub unsupported: Vec<String>,
}

pub fn alerts_file(
    eval: &Evaluation,
    now: DateTime<Utc>,
    threshold_pct: Option<f64>,
    unsupported: Vec<String>,
) -> AlertsFile {
    AlertsFile {
        version: 1,
        updated_at: now,
        valid_for_secs: 3 * POLL_INTERVAL.as_secs(),
        threshold_pct,
        alerts: eval.alerts.clone(),
        coverage: eval.coverage.clone(),
        unsupported,
    }
}

/// Pi `auth.json` OAuth providers the reader has no usage probe for. Reads
/// entry names and `type` only. Every provider `agent_accounts/usage.rs`
/// probes must be listed here, or the file tells every Pi that a reader is
/// missing where there is one (`openai` genuinely has none).
pub fn unsupported_pi_oauth(auth_file: &Path) -> Vec<String> {
    const SUPPORTED: [&str; 4] = ["anthropic", "openai-codex", "github-copilot", "clinepass"];
    let Ok(text) = std::fs::read_to_string(auth_file) else {
        return Vec::new();
    };
    let Ok(serde_json::Value::Object(entries)) = serde_json::from_str::<serde_json::Value>(&text)
    else {
        return Vec::new();
    };
    entries
        .iter()
        .filter(|(name, entry)| {
            entry.get("type").and_then(|t| t.as_str()) == Some("oauth")
                && !SUPPORTED.contains(&name.as_str())
        })
        .map(|(name, _)| format!("pi:{name}"))
        .collect()
}

// ───────────────────────────── delivery

/// Who was told what, persisted: chat id → alert key → when the record may
/// be dropped (the window's reset, or the TTL).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Delivered {
    #[serde(default)]
    pub chats: BTreeMap<String, BTreeMap<String, DateTime<Utc>>>,
}

impl Delivered {
    pub fn has(&self, chat: &str, key: &str) -> bool {
        self.chats
            .get(chat)
            .is_some_and(|keys| keys.contains_key(key))
    }

    pub fn record(&mut self, chat: &str, alert: &UsageAlert, now: DateTime<Utc>) {
        let until = alert.resets_at.unwrap_or(now + DELIVERED_TTL);
        self.chats
            .entry(chat.to_owned())
            .or_default()
            .insert(alert.key.clone(), until);
    }

    /// Forget records whose window reset: a new period may alert again.
    pub fn prune(&mut self, now: DateTime<Utc>) {
        for keys in self.chats.values_mut() {
            keys.retain(|_, until| *until > now);
        }
        self.chats.retain(|_, keys| !keys.is_empty());
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeliveryKind {
    /// Into the running turn.
    Steer,
    /// Queued, held for the turn's end (a chat waiting on an answer).
    Held,
    /// A new turn (an idle orchestrator root).
    Run,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Delivery {
    pub chat_id: String,
    pub kind: DeliveryKind,
    pub alerts: Vec<UsageAlert>,
    /// Tree members (title or id) affected, for a root's note.
    pub members: Vec<String>,
    pub root: bool,
}

/// What a chat is running on and doing, as the planner needs it.
#[derive(Debug, Clone)]
pub struct ChatView {
    pub chat: Chat,
    pub status: Option<SessionStatus>,
    /// The model of its live/last run (else the chat's configured one).
    pub model: Option<String>,
    pub harness: HarnessId,
}

fn kind_for(status: Option<SessionStatus>, idle_runs: bool) -> Option<DeliveryKind> {
    match status {
        Some(SessionStatus::Working) => Some(DeliveryKind::Steer),
        Some(SessionStatus::AwaitingInput) => Some(DeliveryKind::Held),
        _ if idle_runs => Some(DeliveryKind::Run),
        _ => None,
    }
}

/// Plan who is told what. Pure. `default_provider` is the provider Pi's own
/// default model runs on; `paused(root)` says whether the user paused an
/// orchestrator.
pub fn plan(
    views: &[ChatView],
    alerts: &[UsageAlert],
    delivered: &Delivered,
    device_id: &str,
    default_provider: Option<&str>,
    paused: &dyn Fn(&str) -> bool,
) -> Vec<Delivery> {
    let mut out = Vec::new();
    if alerts.is_empty() {
        return out;
    }
    let chats: Vec<Chat> = views.iter().map(|v| v.chat.clone()).collect();
    let mut tree_hits: HashMap<String, (Vec<UsageAlert>, Vec<String>)> = HashMap::new();
    for view in views {
        let chat = &view.chat;
        if chat.archived || chat.device_id != device_id {
            continue;
        }
        let hits: Vec<UsageAlert> = alerts
            .iter()
            .filter(|a| affects(a, view.harness, view.model.as_deref(), default_provider))
            .cloned()
            .collect();
        if hits.is_empty() {
            continue;
        }
        let place = zeron_proto::orchestrator::Place::of(&chats, &chat.id);
        if let Some(root) = place.root_id.as_ref().filter(|root| **root != chat.id) {
            let entry = tree_hits.entry(root.clone()).or_default();
            for hit in &hits {
                if !entry.0.iter().any(|a| a.key == hit.key) {
                    entry.0.push(hit.clone());
                }
            }
            entry
                .1
                .push(chat.title.clone().unwrap_or_else(|| chat.id.clone()));
        }
        let fresh: Vec<UsageAlert> = hits
            .into_iter()
            .filter(|a| !delivered.has(&chat.id, &a.key))
            .collect();
        if fresh.is_empty() {
            continue;
        }
        let Some(kind) = kind_for(view.status, false) else {
            continue;
        };
        out.push(Delivery {
            chat_id: chat.id.clone(),
            kind,
            alerts: fresh,
            members: Vec::new(),
            root: false,
        });
    }
    // Roots: told about their tree's affected members.
    for (root_id, (hits, members)) in tree_hits {
        let Some(root) = views.iter().find(|v| v.chat.id == root_id) else {
            continue;
        };
        if root.chat.archived || root.chat.device_id != device_id {
            continue;
        }
        let fresh: Vec<UsageAlert> = hits
            .into_iter()
            .filter(|a| !delivered.has(&root_id, &tree_key(&a.key)))
            .collect();
        if fresh.is_empty() {
            continue;
        }
        let Some(kind) = kind_for(root.status, !paused(&root_id)) else {
            continue;
        };
        out.push(Delivery {
            chat_id: root_id,
            kind,
            alerts: fresh,
            members,
            root: true,
        });
    }
    out.sort_by(|a, b| a.chat_id.cmp(&b.chat_id));
    out
}

/// A root's record of its tree's alert (apart from an alert on its own run).
fn tree_key(key: &str) -> String {
    format!("tree:{key}")
}

fn describe(alert: &UsageAlert) -> String {
    let who = match &alert.provider {
        Some(p) => format!("{} `{p}`", harness_slug(alert.harness)),
        None => harness_slug(alert.harness),
    };
    let resets = alert
        .resets_at
        .map(|at| format!(", resets {}", at.format("%Y-%m-%d %H:%M UTC")))
        .unwrap_or_default();
    format!(
        "{who} account {}: {} window {:.1}% left{resets}",
        alert.account_id, alert.window, alert.remaining_pct
    )
}

/// Where the handoff goes: `<cwd>/.pi/handoffs/usage-<provider>-<stamp>.md`.
pub fn handoff_path_hint(alert: &UsageAlert, now: DateTime<Utc>) -> String {
    format!(
        ".pi/handoffs/usage-{}-{}.md",
        alert
            .provider
            .clone()
            .unwrap_or_else(|| harness_slug(alert.harness)),
        now.format("%Y%m%dT%H%MZ")
    )
}

pub const MARKER: &str = "[zeron:usage-alert]";

/// The note a session gets. Finish safely; never a hard stop.
pub fn render(delivery: &Delivery, now: DateTime<Utc>) -> String {
    let lines: Vec<String> = delivery
        .alerts
        .iter()
        .map(|a| format!("- {}", describe(a)))
        .collect();
    let path = delivery
        .alerts
        .first()
        .map(|a| handoff_path_hint(a, now))
        .unwrap_or_default();
    if delivery.root {
        return format!(
            "{MARKER} Subscription usage is low for sessions in your tree:\n{}\n\
             Affected sessions: {}.\n\
             They were told to finish their current safe step, write a handoff and end. \
             Do not start or send new work to sessions on that account (start new sessions \
             on another provider's model instead); unaffected accounts stay usable. \
             Collect each affected session's handoff path from its final reply.",
            lines.join("\n"),
            delivery.members.join(", ")
        );
    }
    format!(
        "{MARKER} The subscription this session runs on is almost used up:\n{}\n\
         Finish the step you are on now, as quickly as is safe: do not abandon a \
         half-done action that would leave things broken (finish it or roll it back), \
         but start nothing new — no new sub-agents, workflows, delegations or sessions \
         on this account. Then write a handoff to `{path}` in your working directory \
         (goal, what is done, what is in progress and its exact state, next steps, \
         branches/commits/files, commands to verify), tell any sub-agents you started \
         to wrap up the same way, and end your turn with the handoff path in your \
         final reply.",
        lines.join("\n")
    )
}

// ───────────────────────────── the poller

pub struct UsageAlertsConfig {
    pub data_dir: PathBuf,
    pub pi_auth_file: PathBuf,
    pub threshold_pct: Option<f64>,
}

#[derive(Clone)]
pub struct UsageAlerts {
    inner: Arc<Inner>,
}

struct Inner {
    config: UsageAlertsConfig,
    accounts: AgentAccounts,
    sessions: SessionsEngine,
    doc_host: DocHost,
    workspace: WorkspaceHost,
    delivered: Mutex<Delivered>,
    task: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl UsageAlerts {
    pub fn new(
        config: UsageAlertsConfig,
        accounts: AgentAccounts,
        sessions: SessionsEngine,
        doc_host: DocHost,
        workspace: WorkspaceHost,
    ) -> Self {
        let delivered = std::fs::read_to_string(config.data_dir.join(STATE_FILE_NAME))
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default();
        Self {
            inner: Arc::new(Inner {
                config,
                accounts,
                sessions,
                doc_host,
                workspace,
                delivered: Mutex::new(delivered),
                task: Mutex::new(None),
            }),
        }
    }

    pub fn file(&self) -> PathBuf {
        self.inner.config.data_dir.join(FILE_NAME)
    }

    /// Start polling (no-op outside a tokio runtime or when disabled).
    pub fn start(&self) {
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        if self.inner.config.threshold_pct.is_none() {
            // Disabled: leave no stale alerts behind for the guard.
            let _ = std::fs::remove_file(self.file());
            return;
        }
        let this = self.clone();
        let handle = runtime.spawn(async move {
            tokio::time::sleep(FIRST_DELAY).await;
            loop {
                this.tick().await;
                tokio::time::sleep(POLL_INTERVAL).await;
            }
        });
        *lock(&self.inner.task) = Some(handle);
    }

    pub fn shutdown(&self) {
        if let Some(task) = lock(&self.inner.task).take() {
            task.abort();
        }
    }

    /// One poll: read, evaluate, publish, deliver.
    pub async fn tick(&self) {
        let Some(threshold) = self.inner.config.threshold_pct else {
            return;
        };
        let snapshot = match self.inner.accounts.list(true).await {
            Ok(snapshot) => snapshot,
            Err(error) => {
                tracing::warn!(%error, "usage alerts: accounts unavailable");
                return;
            }
        };
        let now = Utc::now();
        let eval = evaluate(&snapshot, now, threshold);
        let unsupported = unsupported_pi_oauth(&self.inner.config.pi_auth_file);
        let file = alerts_file(&eval, now, Some(threshold), unsupported);
        if let Err(error) = write_json(&self.file(), &file) {
            tracing::warn!(%error, "usage alerts: snapshot write failed");
        }
        if !eval.alerts.is_empty() {
            tracing::warn!(
                alerts = eval.alerts.len(),
                "subscription usage below {threshold}%: telling affected sessions"
            );
        }
        self.deliver(&eval.alerts, now);
    }

    fn views(&self) -> Vec<ChatView> {
        let chats = self.inner.workspace.read_chats().unwrap_or_default();
        chats
            .into_iter()
            .map(|chat| {
                let last = self.inner.sessions.last_request(&chat.id);
                let config = chat.config.clone();
                let harness = last
                    .as_ref()
                    .and_then(|r| r.harness)
                    .or(config.as_ref().map(|c| c.harness))
                    .unwrap_or(HarnessId::Pi);
                let model = last.and_then(|r| r.model).or(config.and_then(|c| c.model));
                ChatView {
                    status: self
                        .inner
                        .sessions
                        .session_status(&chat.id)
                        .map(|s| s.status),
                    model,
                    harness,
                    chat,
                }
            })
            .collect()
    }

    fn deliver(&self, alerts: &[UsageAlert], now: DateTime<Utc>) {
        let mut delivered = lock(&self.inner.delivered);
        let before = delivered.clone();
        delivered.prune(now);
        let views = self.views();
        // Pi's `settings.json` sits beside the `auth.json` this reader has.
        let default_provider = pi_default_provider(
            &self
                .inner
                .config
                .pi_auth_file
                .with_file_name("settings.json"),
        );
        let workspace = self.inner.workspace.clone();
        let paused = move |root: &str| workspace.orchestrator_project(root).paused;
        let deliveries = plan(
            &views,
            alerts,
            &delivered,
            self.inner.doc_host.device_id(),
            default_provider.as_deref(),
            &paused,
        );
        for delivery in deliveries {
            let text = render(&delivery, now);
            let host = &self.inner.doc_host;
            let sent = match delivery.kind {
                DeliveryKind::Steer => host
                    .queue_command(
                        &delivery.chat_id,
                        SessionCommandPayload::Steer {
                            prompt: text,
                            message_id: Some(crate::new_id()),
                        },
                    )
                    .map(|_| ()),
                DeliveryKind::Held => host
                    .queue_message_with_behavior(&delivery.chat_id, &text, Vec::new(), true)
                    .map(|_| ()),
                DeliveryKind::Run => {
                    host.notify_parent(&delivery.chat_id, &text, zeron_proto::SessionStatus::Idle)
                }
            };
            match sent {
                Ok(()) => {
                    for alert in &delivery.alerts {
                        if delivery.root {
                            let mut tree = alert.clone();
                            tree.key = tree_key(&alert.key);
                            delivered.record(&delivery.chat_id, &tree, now);
                        } else {
                            delivered.record(&delivery.chat_id, alert, now);
                        }
                    }
                    tracing::info!(chat = %delivery.chat_id, kind = ?delivery.kind, "usage alert delivered");
                }
                Err(error) => {
                    tracing::warn!(chat = %delivery.chat_id, %error, "usage alert delivery failed");
                }
            }
        }
        if *delivered != before {
            let path = self.inner.config.data_dir.join(STATE_FILE_NAME);
            if let Err(error) = write_json(&path, &*delivered) {
                tracing::warn!(%error, "usage alerts: state write failed");
            }
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn write_json<T: Serialize>(path: &Path, value: &T) -> std::io::Result<()> {
    let bytes = serde_json::to_vec_pretty(value).map_err(std::io::Error::other)?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension(format!("json.tmp-{}", std::process::id()));
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)
}

/// Set of providers alerted, for logs and tests.
pub fn alerted_providers(alerts: &[UsageAlert]) -> HashSet<String> {
    alerts.iter().filter_map(|a| a.provider.clone()).collect()
}

#[cfg(test)]
#[path = "usage_alerts_tests.rs"]
mod tests;
