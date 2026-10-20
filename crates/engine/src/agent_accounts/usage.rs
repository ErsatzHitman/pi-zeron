//! Usage probes for Grok, Devin, OpenCode, Pi and Hermes — each the view the
//! agent's own CLI renders, with the same stale-while-revalidate cache,
//! [`ProbeError`] classification and backoff as the original providers.
//!
//! - **Grok**: `GET cli-chat-proxy.grok.com/v1/billing?format=credits`
//!   (`/usage` in the CLI) — the Grok Build share of the current period.
//!   Only a 401/403 refreshes, and only a SAVED slot (OIDC `refresh_token`
//!   grant against the entry's own issuer, single-flight); the live token is
//!   the CLI's to rotate.
//! - **Devin**: Connect-JSON `SeatManagementService/GetUserStatus` (what
//!   `devin auth status` calls) — daily/weekly quota, plus the identity the
//!   key file lacks, written back into the slot. Keys don't refresh.
//! - **ChatGPT logins** (OpenCode `openai`, Pi `openai-codex`, Hermes
//!   `openai-codex`): the Codex usage endpoint — the same token kind.
//! - **Claude logins** (Pi `anthropic`): Claude's `/api/oauth/usage`.
//! - **ClinePass** (Pi `clinepass`): Cline's
//!   `/api/v1/users/me/plan/usage-limits` — 5-hour, weekly and monthly
//!   percentages — with the plan's name from `/api/v1/users/me/plan`.
//! - **Copilot** (OpenCode / Pi `github-copilot`):
//!   `api.github.com/copilot_internal/user` — premium-request quota.
//! - **Nous** (Hermes `nous`): the portal's `/api/oauth/account` credits.
//! - **OpenCode Go** (Pi's `opencode-go` API keys, every key in
//!   `api-keys.json`): `GET opencode.ai/zen/go/v1/usage` with the key —
//!   5-hour, weekly and monthly percentages. One row per key.
//!
//! A rejected ChatGPT login of OpenCode / Pi is refreshed once (single-flight,
//! like Claude's): a saved slot in its own file, the live login in the
//! agent's store under the agent's own lock, which Pi re-reads before it
//! refreshes. Otherwise the session stayed "expired" until the agent ran.
//! Pi's Claude and ClinePass logins are refreshed the same way, the way Pi
//! itself does it ([`AgentAccounts::refresh_keyed_login`]): an idle one
//! lapses within hours and showed no usage until Pi next used it.
//! Other per-provider tokens are left to the agent (or Hermes' pool) — a
//! rejected one reads "switch to it to refresh" / "refreshes next time".

use super::stores::{
    DEVIN_SERVERS, GROK_ISSUERS, NOUS_PORTALS, Upstream, copilot_api_base, devin_api_key,
    trusted_base, upstream_of,
};
use super::*;

pub(super) const GROK_USAGE_URL: &str = "https://cli-chat-proxy.grok.com/v1/billing?format=credits";
pub(super) const NOUS_PORTAL: &str = "https://portal.nousresearch.com";
/// OpenCode Go's usage of the key it is called with: 5-hour, weekly and
/// monthly percentages with their reset times.
pub(super) const OPENCODE_GO_USAGE_URL: &str = "https://opencode.ai/zen/go/v1/usage";
/// Pi's provider id for OpenCode Go, and its key store's.
pub(super) const OPENCODE_GO: &str = "opencode-go";
/// Cline's API root, which ClinePass logs in, refreshes and reports on.
pub(super) const CLINE_API: &str = "https://api.cline.bot";
/// pi-clinepass's token lifetime less its refresh margin (55 − 5 minutes):
/// a refreshed entry carries the same `expires` Pi would write.
const CLINE_TOKEN_TTL_MS: i64 = 50 * 60 * 1000;
const DEVIN_STATUS_RPC: &str = "exa.seat_management_pb.SeatManagementService/GetUserStatus";
const DEVIN_DEFAULT_API_SERVER: &str = "https://server.codeium.com";
/// The CLI version the status call identifies as (the server wants one).
const DEVIN_CLI_VERSION: &str = "3000.11.3";

impl AgentAccounts {
    pub(super) async fn grok_usage(
        &self,
        slot: &Slot,
        is_active: bool,
    ) -> Result<UsageSnapshot, ProbeError> {
        let missing = ProbeError::NoCredentials {
            why: NoCredentials::Missing,
        };
        // A Grok slot is one issuer's token set.
        let access_token = str_field(&slot.credentials, "key").ok_or(missing)?;
        match self.grok_usage_request(&access_token).await {
            // Same rule as Claude: rotate slot-owned tokens only after the
            // endpoint REJECTED them; the live pair is the CLI's.
            Err(ProbeError::Unauthorized { status })
                if !is_active && self.inner.endpoints.allow_slot_refresh =>
            {
                match self.refresh_grok_slot(slot).await? {
                    Some(fresh) => self.grok_usage_request(&fresh).await,
                    None => Err(ProbeError::Unauthorized { status }),
                }
            }
            result => result,
        }
    }

    async fn grok_usage_request(&self, access_token: &str) -> Result<UsageSnapshot, ProbeError> {
        let body = probe_json(
            "grok",
            "usage",
            self.inner
                .http
                .get(&self.inner.endpoints.grok_usage)
                .bearer_auth(access_token)
                .header("X-XAI-Token-Auth", "xai-grok-cli"),
        )
        .await?;
        grok_usage_snapshot(&body).ok_or_else(|| schema_error("grok", &body))
    }

    /// Single-flight per slot, like Claude's: a second concurrent refresh of
    /// a single-use refresh token would revoke it. `Ok(None)` = in flight.
    async fn refresh_grok_slot(&self, slot: &Slot) -> Result<Option<String>, ProbeError> {
        if !lock(&self.inner.inflight_refreshes).insert(slot.id.clone()) {
            return Ok(None);
        }
        let _release = InflightGuard {
            set: &self.inner.inflight_refreshes,
            keys: vec![slot.id.clone()],
        };
        let missing = ProbeError::NoCredentials {
            why: NoCredentials::Missing,
        };
        let entry = &slot.credentials;
        let refresh_token = str_field(entry, "refresh_token").ok_or(missing.clone())?;
        let client_id = str_field(entry, "oidc_client_id").ok_or(missing)?;
        // The issuer comes from the credential file: the refresh token only
        // goes to an xAI host.
        let issuer = trusted_base(
            &str_field(entry, "oidc_issuer").unwrap_or_else(|| "https://auth.x.ai".to_string()),
            GROK_ISSUERS,
            self.inner.endpoints.allow_loopback_http,
        )
        .ok_or(ProbeError::UntrustedEndpoint)?;
        let body = probe_json(
            "grok",
            "refresh",
            self.inner
                .http
                .post(format!(
                    "{}/oauth2/token",
                    issuer.as_str().trim_end_matches('/')
                ))
                .form(&[
                    ("grant_type", "refresh_token"),
                    ("refresh_token", refresh_token.as_str()),
                    ("client_id", client_id.as_str()),
                ]),
        )
        .await
        .map_err(|error| match error {
            // invalid_grant: the saved login is dead — "sign in again".
            ProbeError::Http { status: 400, .. } => ProbeError::Unauthorized { status: 400 },
            other => other,
        })?;
        let access_token =
            str_field(&body, "access_token").ok_or_else(|| schema_error("grok", &body))?;
        let expires_in = body
            .get("expires_in")
            .and_then(|v| v.as_i64())
            .unwrap_or(3600);
        let mut credentials = slot.credentials.clone();
        if let Some(entry) = credentials.as_object_mut() {
            entry.insert("key".into(), serde_json::json!(access_token));
            if let Some(rotated) = str_field(&body, "refresh_token") {
                entry.insert("refresh_token".into(), serde_json::json!(rotated));
            }
            entry.insert(
                "expires_at".into(),
                serde_json::json!(
                    (Utc::now() + chrono::TimeDelta::seconds(expires_in)).to_rfc3339()
                ),
            );
        }
        let mut refreshed = slot.clone();
        refreshed.credentials = credentials;
        refreshed.saved_at = now_ms();
        if let Err(err) = self.write_slot(&refreshed) {
            tracing::warn!(slot = %slot.id, error = %err, "refreshed grok slot write failed");
        }
        Ok(Some(access_token))
    }

    /// Devin's quota view, which also names the account behind the key.
    pub(super) async fn devin_usage(&self, slot: &Slot) -> Result<UsageSnapshot, ProbeError> {
        let api_key = devin_api_key(&slot.credentials).ok_or(ProbeError::NoCredentials {
            why: NoCredentials::Missing,
        })?;
        // The server comes from the credential file: the key only goes to a
        // Devin / Windsurf host.
        let server = trusted_base(
            &str_field(&slot.credentials, "api_server_url")
                .unwrap_or_else(|| DEVIN_DEFAULT_API_SERVER.to_string()),
            DEVIN_SERVERS,
            self.inner.endpoints.allow_loopback_http,
        )
        .ok_or(ProbeError::UntrustedEndpoint)?;
        let server = server.as_str();
        let body = probe_json(
            "devin",
            "usage",
            self.inner
                .http
                .post(format!(
                    "{}/{DEVIN_STATUS_RPC}",
                    server.trim_end_matches('/')
                ))
                .header("Connect-Protocol-Version", "1")
                .json(&serde_json::json!({
                    "metadata": {
                        "apiKey": api_key,
                        "ideName": "devin-cli",
                        "ideVersion": DEVIN_CLI_VERSION,
                        "extensionName": "devin-cli",
                        "extensionVersion": DEVIN_CLI_VERSION,
                        "locale": "en_US",
                        "os": std::env::consts::OS,
                        "sessionId": "zeron-accounts",
                        "requestId": "1",
                    }
                })),
        )
        .await?;
        let (snapshot, identity) =
            devin_usage_snapshot(&body).ok_or_else(|| schema_error("devin", &body))?;
        // Adopt the probed identity (best-effort; a failed write re-probes).
        let mut updated = slot.clone();
        if let Some(email) = identity.email {
            updated.profile.email = email;
        }
        if identity.name.is_some() {
            updated.profile.display_name = identity.name;
        }
        if snapshot.plan_label.is_some() {
            updated.profile.plan = snapshot.plan_label.clone();
        }
        if updated.profile.email != slot.profile.email
            || updated.profile.display_name != slot.profile.display_name
            || updated.profile.plan != slot.profile.plan
        {
            updated.saved_at = now_ms();
            if let Err(err) = self.write_slot(&updated) {
                tracing::warn!(slot = %slot.id, error = %err, "devin identity write-back failed");
            }
        }
        Ok(snapshot)
    }

    /// A per-provider login (OpenCode / Pi entry, Hermes pool entry): probe
    /// the vendor behind it with its own token.
    pub(super) async fn keyed_usage(
        &self,
        harness: HarnessId,
        slot: &Slot,
        is_active: bool,
    ) -> Result<UsageSnapshot, ProbeError> {
        let missing = ProbeError::NoCredentials {
            why: NoCredentials::Missing,
        };
        let key = slot.store_key.as_deref().ok_or(missing.clone())?;
        let creds = &slot.credentials;
        if harness == HarnessId::Pi && key == OPENCODE_GO {
            let api_key = str_field(creds, "key").ok_or(missing)?;
            return self.opencode_go_usage(&api_key).await;
        }
        if harness == HarnessId::Hermes {
            if str_field(creds, "auth_type").as_deref() == Some("api_key") {
                return Err(ProbeError::NoCredentials {
                    why: NoCredentials::ApiKey,
                });
            }
            let access = str_field(creds, "access_token").ok_or(missing)?;
            return match key {
                "openai-codex" => {
                    let account = jwt_claims(&access)
                        .and_then(|c| {
                            c.get("https://api.openai.com/auth")
                                .and_then(|a| str_field(a, "chatgpt_account_id"))
                        })
                        .unwrap_or_default();
                    self.openai_usage("hermes", &access, &account).await
                }
                "nous" => {
                    // A pool entry's own portal is honoured only on a Nous host.
                    let portal = match str_field(creds, "portal_base_url") {
                        Some(raw) => trusted_base(
                            &raw,
                            NOUS_PORTALS,
                            self.inner.endpoints.allow_loopback_http,
                        )
                        .ok_or(ProbeError::UntrustedEndpoint)?
                        .as_str()
                        .to_string(),
                        None => self.inner.endpoints.nous_portal.clone(),
                    };
                    self.nous_usage(&portal, &access).await
                }
                _ => Err(ProbeError::NoCredentials {
                    why: NoCredentials::Unsupported,
                }),
            };
        }
        match upstream_of(harness, key) {
            Some(Upstream::OpenAi) => {
                let access = str_field(creds, "access").ok_or(missing)?;
                let account = str_field(creds, "accountId").unwrap_or_default();
                let cli = stores::cli_name(harness);
                match self.openai_usage(cli, &access, &account).await {
                    Err(ProbeError::Unauthorized { status })
                        if self.inner.endpoints.allow_slot_refresh =>
                    {
                        match self
                            .refresh_openai_login(harness, key, slot, &access, is_active)
                            .await?
                        {
                            Some((fresh, account)) => {
                                self.openai_usage(cli, &fresh, &account).await
                            }
                            None => Err(ProbeError::Unauthorized { status }),
                        }
                    }
                    result => result,
                }
            }
            Some(upstream @ (Upstream::Anthropic | Upstream::Cline)) => {
                let live = match is_active {
                    true => self.live_keyed_entry(harness, key),
                    false => None,
                };
                let entry = live.as_ref().unwrap_or(creds);
                let mut access = str_field(entry, "access").ok_or(missing)?;
                let refreshable = harness == HarnessId::Pi && self.inner.endpoints.allow_slot_refresh;
                // A lapsed token is refreshed before the call, not after its 401:
                // Anthropic's usage endpoint rate-limits eagerly.
                if refreshable
                    && stores::token_lapsed(entry)
                    && let Ok(Some(fresh)) = self
                        .refresh_keyed_login(harness, key, upstream, Some((slot, is_active)), &access)
                        .await
                {
                    access = str_field(&fresh, "access").unwrap_or(access);
                }
                let probe = |access: String| async move {
                    match upstream {
                        Upstream::Cline => self.cline_usage(&access).await,
                        _ => self.claude_usage_request(&access).await,
                    }
                };
                match probe(access.clone()).await {
                    Err(ProbeError::Unauthorized { status }) if refreshable => {
                        match self
                            .refresh_keyed_login(harness, key, upstream, Some((slot, is_active)), &access)
                            .await?
                            .and_then(|fresh| str_field(&fresh, "access"))
                        {
                            Some(fresh) if fresh != access => probe(fresh).await,
                            _ => Err(ProbeError::Unauthorized { status }),
                        }
                    }
                    result => result,
                }
            }
            Some(Upstream::Copilot) => {
                let token = str_field(creds, "refresh").ok_or(missing)?;
                // GitHub Enterprise: a validated plain host, or nothing is sent.
                let api = copilot_api_base(
                    creds,
                    &self.inner.endpoints.github_api,
                    self.inner.endpoints.allow_loopback_http,
                )
                .ok_or(ProbeError::UntrustedEndpoint)?;
                self.copilot_usage(&api, &token).await
            }
            None => Err(ProbeError::NoCredentials {
                why: NoCredentials::Unsupported,
            }),
        }
    }

    /// A rejected ChatGPT login's fresh `(access, accountId)`, or `None`
    /// while another refresh of it is in flight. The live login is re-read
    /// first: when the agent already rotated it, its new token is used as
    /// is. A refused refresh (400) means the login is dead.
    async fn refresh_openai_login(
        &self,
        harness: HarnessId,
        store_key: &str,
        slot: &Slot,
        rejected: &str,
        is_active: bool,
    ) -> Result<Option<(String, String)>, ProbeError> {
        let flight = format!("{}:{store_key}:{}", harness_slug(harness), slot.id);
        if !lock(&self.inner.inflight_refreshes).insert(flight.clone()) {
            return Ok(None);
        }
        let _release = InflightGuard {
            set: &self.inner.inflight_refreshes,
            keys: vec![flight],
        };
        let missing = ProbeError::NoCredentials {
            why: NoCredentials::Missing,
        };
        let entry = if is_active {
            self.live_keyed_entry(harness, store_key)
                .ok_or(missing.clone())?
        } else {
            slot.credentials.clone()
        };
        let account = |entry: &serde_json::Value| {
            str_field(entry, "accountId").unwrap_or_default()
        };
        if let Some(current) = str_field(&entry, "access").filter(|a| a != rejected) {
            return Ok(Some((current, account(&entry))));
        }
        let refresh_token = str_field(&entry, "refresh").ok_or(missing)?;
        let body = probe_json(
            stores::cli_name(harness),
            "refresh",
            self.inner
                .http
                .post(format!(
                    "{}/oauth/token",
                    self.inner.endpoints.openai_auth.trim_end_matches('/')
                ))
                .form(&[
                    ("grant_type", "refresh_token"),
                    ("refresh_token", refresh_token.as_str()),
                    ("client_id", oauth::OPENAI_CLIENT_ID),
                ]),
        )
        .await
        .map_err(|error| match error {
            ProbeError::Http { status: 400, .. } => ProbeError::Unauthorized { status: 400 },
            other => other,
        })?;
        let access = str_field(&body, "access_token")
            .ok_or_else(|| schema_error(stores::cli_name(harness), &body))?;
        let expires_in = body
            .get("expires_in")
            .and_then(|v| v.as_i64())
            .unwrap_or(3600);
        let mut fresh = entry.clone();
        if let Some(map) = fresh.as_object_mut() {
            map.insert("access".into(), serde_json::json!(access));
            map.insert(
                "refresh".into(),
                serde_json::json!(str_field(&body, "refresh_token").unwrap_or(refresh_token)),
            );
            map.insert("expires".into(), serde_json::json!(now_ms() + expires_in * 1000));
            let claimed = jwt_claims(&access).and_then(|c| {
                c.get("https://api.openai.com/auth")
                    .and_then(|a| str_field(a, "chatgpt_account_id"))
            });
            if let Some(id) = claimed {
                map.insert("accountId".into(), serde_json::json!(id));
            }
        }
        let written = if is_active {
            self.write_keyed_entry(harness, store_key, Some(&fresh))
        } else {
            let mut refreshed = slot.clone();
            refreshed.credentials = fresh.clone();
            refreshed.saved_at = now_ms();
            self.write_slot(&refreshed)
        };
        if let Err(err) = written {
            tracing::warn!(slot = %slot.id, error = %err, "refreshed chatgpt login write failed");
        }
        Ok(Some((access, account(&fresh))))
    }

    /// A lapsed or rejected Pi Claude / ClinePass login, refreshed once
    /// (single-flight) exactly as Pi refreshes it, and written back where it
    /// lives: Pi's `auth.json` under Pi's lock for the live login (`slot` is
    /// `None`, or is the active one), else the saved slot. The fresh entry,
    /// or `None` while another refresh of it is in flight. The live entry is
    /// re-read first: when Pi already rotated it, that entry is used as is.
    /// A refused refresh (400/401) means the login is dead.
    pub(super) async fn refresh_keyed_login(
        &self,
        harness: HarnessId,
        store_key: &str,
        upstream: Upstream,
        slot: Option<(&Slot, bool)>,
        rejected: &str,
    ) -> Result<Option<serde_json::Value>, ProbeError> {
        let saved = slot.filter(|(_, is_active)| !is_active).map(|(slot, _)| slot);
        let flight = format!(
            "{}:{store_key}:{}",
            harness_slug(harness),
            saved.map_or("live", |slot| slot.id.as_str())
        );
        if !lock(&self.inner.inflight_refreshes).insert(flight.clone()) {
            return Ok(None);
        }
        let _release = InflightGuard {
            set: &self.inner.inflight_refreshes,
            keys: vec![flight],
        };
        let missing = ProbeError::NoCredentials {
            why: NoCredentials::Missing,
        };
        let entry = match saved {
            Some(slot) => slot.credentials.clone(),
            None => self
                .live_keyed_entry(harness, store_key)
                .ok_or(missing.clone())?,
        };
        if let Some(current) = str_field(&entry, "access")
            && current != rejected
            && !stores::token_lapsed(&entry)
        {
            return Ok(Some(entry));
        }
        let refresh_token = str_field(&entry, "refresh").ok_or(missing)?;
        let provider = stores::cli_name(harness);
        let dead = |error| match error {
            ProbeError::Http { status: 400, .. } | ProbeError::Unauthorized { .. } => {
                ProbeError::Unauthorized { status: 400 }
            }
            other => other,
        };
        let (access, refresh, expires) = match upstream {
            Upstream::Anthropic => {
                let body = probe_json(
                    provider,
                    "refresh",
                    self.inner
                        .http
                        .post(&self.inner.endpoints.claude_loopback_token)
                        .header("Accept", "application/json")
                        .json(&serde_json::json!({
                            "grant_type": "refresh_token",
                            "client_id": CLAUDE_CLIENT_ID,
                            "refresh_token": refresh_token,
                        })),
                )
                .await
                .map_err(dead)?;
                let access = str_field(&body, "access_token")
                    .ok_or_else(|| schema_error(provider, &body))?;
                let expires_in = body
                    .get("expires_in")
                    .and_then(|v| v.as_i64())
                    .unwrap_or(3600);
                (
                    access,
                    str_field(&body, "refresh_token").unwrap_or(refresh_token),
                    // Pi's own margin: five minutes early.
                    now_ms() + expires_in * 1000 - 5 * 60 * 1000,
                )
            }
            Upstream::Cline => {
                let body = probe_json(
                    provider,
                    "refresh",
                    self.inner
                        .http
                        .post(format!(
                            "{}/api/v1/auth/refresh",
                            self.inner.endpoints.cline_api.trim_end_matches('/')
                        ))
                        .json(&serde_json::json!({
                            "granttype": "refresh_token",
                            "refreshToken": refresh_token,
                        })),
                )
                .await
                .map_err(dead)?;
                let tokens = body.get("data").filter(|d| d.is_object()).unwrap_or(&body);
                let access = str_field(tokens, "accessToken")
                    .ok_or_else(|| schema_error(provider, &body))?;
                // The chat API wants the `workos:` prefix a bare JWT lacks.
                let access = match access.starts_with("workos:") {
                    true => access,
                    false => format!("workos:{access}"),
                };
                (
                    access,
                    str_field(tokens, "refreshToken").unwrap_or(refresh_token),
                    now_ms() + CLINE_TOKEN_TTL_MS,
                )
            }
            Upstream::OpenAi | Upstream::Copilot => {
                return Err(ProbeError::NoCredentials {
                    why: NoCredentials::Unsupported,
                });
            }
        };
        let mut fresh = entry.clone();
        if let Some(map) = fresh.as_object_mut() {
            map.insert("access".into(), serde_json::json!(access));
            map.insert("refresh".into(), serde_json::json!(refresh));
            map.insert("expires".into(), serde_json::json!(expires));
        }
        let written = match saved {
            Some(slot) => {
                let mut refreshed = slot.clone();
                refreshed.credentials = fresh.clone();
                refreshed.saved_at = now_ms();
                self.write_slot(&refreshed)
            }
            None => self.write_keyed_entry(harness, store_key, Some(&fresh)),
        };
        if let Err(err) = written {
            tracing::warn!(store_key, error = %err, "refreshed login write failed");
        }
        Ok(Some(fresh))
    }

    /// ClinePass's plan limits: 5-hour, weekly and monthly percentages with
    /// their reset times, named by the plan (`Cline Pass (Monthly)`).
    async fn cline_usage(&self, access_token: &str) -> Result<UsageSnapshot, ProbeError> {
        let base = self.inner.endpoints.cline_api.trim_end_matches('/').to_string();
        let get = |path: &str| {
            self.inner
                .http
                .get(format!("{base}{path}"))
                .bearer_auth(access_token)
                .header("Accept", "application/json")
        };
        let body = probe_json(
            "clinepass",
            "usage",
            get("/api/v1/users/me/plan/usage-limits"),
        )
        .await?;
        let mut snapshot =
            cline_usage_snapshot(&body).ok_or_else(|| schema_error("clinepass", &body))?;
        // The plan's name is a nicety: its failure never fails the meters.
        if let Ok(plan) = probe_json("clinepass", "plan", get("/api/v1/users/me/plan")).await {
            let plan = plan.get("data").filter(|d| d.is_object()).unwrap_or(&plan);
            if let Some(name) = plan.get("plan").and_then(|p| str_field(p, "displayName")) {
                snapshot.plan_label = Some(name);
            }
        }
        Ok(snapshot)
    }

    async fn openai_usage(
        &self,
        provider: &'static str,
        access_token: &str,
        account_id: &str,
    ) -> Result<UsageSnapshot, ProbeError> {
        let body = probe_json(
            provider,
            "usage",
            self.inner
                .http
                .get(&self.inner.endpoints.codex_usage)
                .bearer_auth(access_token)
                .header("chatgpt-account-id", account_id),
        )
        .await?;
        codex_usage_snapshot(&body).ok_or_else(|| schema_error(provider, &body))
    }

    async fn opencode_go_usage(&self, api_key: &str) -> Result<UsageSnapshot, ProbeError> {
        let body = probe_json(
            "opencode-go",
            "usage",
            self.inner
                .http
                .get(&self.inner.endpoints.opencode_go_usage)
                .bearer_auth(api_key)
                .header("Accept", "application/json")
                .header("User-Agent", "zeron"),
        )
        .await?;
        opencode_go_usage_snapshot(&body, Utc::now())
            .ok_or_else(|| schema_error("opencode-go", &body))
    }

    /// Pi's OpenCode Go keys (`api-keys.json`, disabled ones too: each has
    /// its own quota), as in-memory slots the usage probe takes, with
    /// whether Pi uses that key now (the key router's pick, else the
    /// store's current key). Never written to disk: the slot carries the
    /// secret only for the probe; the cache keeps a digest.
    pub(super) fn opencode_go_keys(&self) -> Vec<(Slot, bool)> {
        let dir = &self.inner.config.pi_agent_dir;
        let Ok(store) = crate::api_keys::Store::load(&dir.join(crate::api_keys::STORE_FILE)) else {
            return Vec::new();
        };
        let Some(keys) = store.providers.get(OPENCODE_GO) else {
            return Vec::new();
        };
        let routed = std::fs::read(dir.join(crate::api_keys::HEALTH_FILE))
            .ok()
            .and_then(|raw| serde_json::from_slice::<serde_json::Value>(&raw).ok())
            .and_then(|health| str_field(&health["providers"][OPENCODE_GO], "current"))
            .filter(|id| keys.keys.iter().any(|k| &k.id == id && !k.disabled));
        let in_use = routed.or_else(|| keys.current().map(|k| k.id.clone()));
        keys.keys
            .iter()
            .filter(|key| !key.secret.is_empty())
            .map(|key| {
                let shown = crate::api_keys::mask(&key.secret);
                let email = match key.label.trim() {
                    "" => format!("Key {shown}"),
                    label => format!("{label} · {shown}"),
                };
                let slot = Slot {
                    id: format!("pi-{OPENCODE_GO}-{}", key.id),
                    harness: HarnessId::Pi,
                    account_key: format!("{OPENCODE_GO}:key:{}", key.id),
                    profile: SlotProfile {
                        email,
                        display_name: None,
                        organization: None,
                        plan: Some("OpenCode Go".into()),
                        auth_kind: AgentAuthKind::ApiKey,
                    },
                    credentials: serde_json::json!({ "key": key.secret }),
                    claude_config: None,
                    saved_at: 0,
                    created_at: None,
                    store_key: Some(OPENCODE_GO.into()),
                };
                (slot, in_use.as_deref() == Some(key.id.as_str()))
            })
            .collect()
    }

    async fn copilot_usage(&self, api: &str, token: &str) -> Result<UsageSnapshot, ProbeError> {
        let body = probe_json(
            "copilot",
            "usage",
            self.inner
                .http
                .get(format!(
                    "{}/copilot_internal/user",
                    api.trim_end_matches('/')
                ))
                .header("Authorization", format!("token {token}"))
                .header("Accept", "application/json")
                .header("User-Agent", "zeron"),
        )
        .await?;
        copilot_usage_snapshot(&body).ok_or_else(|| schema_error("copilot", &body))
    }

    async fn nous_usage(
        &self,
        portal: &str,
        access_token: &str,
    ) -> Result<UsageSnapshot, ProbeError> {
        let body = probe_json(
            "nous",
            "usage",
            self.inner
                .http
                .get(format!(
                    "{}/api/oauth/account",
                    portal.trim_end_matches('/')
                ))
                .bearer_auth(access_token)
                .header("Accept", "application/json"),
        )
        .await?;
        nous_usage_snapshot(&body).ok_or_else(|| schema_error("nous", &body))
    }
}

/// Grok's `/billing?format=credits`: the current period, with the Grok Build
/// product's share of it (the blended `creditUsagePercent` as fallback).
pub(super) fn grok_usage_snapshot(body: &serde_json::Value) -> Option<UsageSnapshot> {
    let config = body.get("config")?;
    let period = config.get("currentPeriod");
    let used_percent = config
        .get("productUsage")
        .and_then(|u| u.as_array())
        .and_then(|products| {
            products.iter().find_map(|p| {
                (p.get("product").and_then(|v| v.as_str()) == Some("GrokBuild"))
                    .then(|| p.get("usagePercent").and_then(|v| v.as_f64()))
                    .flatten()
            })
        })
        .or_else(|| config.get("creditUsagePercent").and_then(|v| v.as_f64()))?;
    let label = match period.and_then(|p| p.get("type")).and_then(|v| v.as_str()) {
        Some("USAGE_PERIOD_TYPE_DAILY") => "Day",
        Some("USAGE_PERIOD_TYPE_WEEKLY") => "Week",
        Some("USAGE_PERIOD_TYPE_MONTHLY") => "Month",
        _ => "Period",
    };
    Some(UsageSnapshot {
        windows: vec![AgentUsageWindow {
            label: label.to_string(),
            used_fraction: (used_percent / 100.0) as f32,
            resets_at: parse_when(
                period
                    .and_then(|p| p.get("end"))
                    .or_else(|| config.get("billingPeriodEnd")),
            ),
        }],
        plan_label: None,
    })
}

/// Who a Devin key belongs to, from its status reply.
#[derive(Debug, Default, PartialEq)]
pub(super) struct DevinIdentity {
    pub(super) email: Option<String>,
    pub(super) name: Option<String>,
}

/// Devin's `GetUserStatus`: daily/weekly quota (reported as REMAINING
/// percent; meters show used), the plan, and the identity.
pub(super) fn devin_usage_snapshot(
    body: &serde_json::Value,
) -> Option<(UsageSnapshot, DevinIdentity)> {
    let status = body.get("userStatus")?;
    let identity = DevinIdentity {
        email: str_field(status, "email"),
        name: str_field(status, "name").or_else(|| str_field(status, "displayName")),
    };
    let plan = status.get("planStatus");
    let mut windows = Vec::new();
    if let Some(plan) = plan {
        for (percent_key, reset_key, label) in [
            ("dailyQuotaRemainingPercent", "dailyQuotaResetAtUnix", "Day"),
            (
                "weeklyQuotaRemainingPercent",
                "weeklyQuotaResetAtUnix",
                "Week",
            ),
        ] {
            if let Some(remaining) = plan.get(percent_key).and_then(json_f64) {
                windows.push(AgentUsageWindow {
                    label: label.to_string(),
                    used_fraction: ((100.0 - remaining) / 100.0).clamp(0.0, 1.0) as f32,
                    resets_at: json_secs(plan.get(reset_key)),
                });
            }
        }
    }
    let info = plan.and_then(|p| p.get("planInfo"));
    let plan_label = devin_plan_label(
        info.and_then(|i| str_field(i, "teamsTier")).as_deref(),
        info.and_then(|i| str_field(i, "planName")).as_deref(),
    );
    // Identity alone is still worth keeping (the card gets its email).
    (identity.email.is_some() || !windows.is_empty()).then_some((
        UsageSnapshot {
            windows,
            plan_label,
        },
        identity,
    ))
}

/// "TEAMS_TIER_DEVIN_PRO" → "Devin Pro"; else the plan's own name.
pub(super) fn devin_plan_label(tier: Option<&str>, plan_name: Option<&str>) -> Option<String> {
    if let Some(rest) = tier.and_then(|t| t.strip_prefix("TEAMS_TIER_"))
        && !matches!(rest, "" | "UNSPECIFIED")
    {
        let words: Vec<String> = rest
            .split('_')
            .filter(|w| !w.is_empty())
            .map(|w| {
                let mut chars = w.chars();
                match chars.next() {
                    Some(first) => format!("{}{}", first, chars.as_str().to_lowercase()),
                    None => String::new(),
                }
            })
            .collect();
        let label = words.join(" ");
        return Some(if label.starts_with("Devin") {
            label
        } else {
            format!("Devin {label}")
        });
    }
    plan_name.map(|name| {
        if name.starts_with("Devin") {
            name.to_string()
        } else {
            format!("Devin {name}")
        }
    })
}

/// GitHub's `copilot_internal/user`: each metered quota snapshot (premium
/// requests on paid plans; chat/completions on Free) as a window. An
/// all-unlimited plan has no meters but still reports its plan.
pub(super) fn copilot_usage_snapshot(body: &serde_json::Value) -> Option<UsageSnapshot> {
    let resets_at = parse_when(body.get("quota_reset_date_utc"))
        .or_else(|| day_start(body.get("quota_reset_date")))
        .or_else(|| day_start(body.get("limited_user_reset_date")));
    let mut windows = Vec::new();
    if let Some(snapshots) = body.get("quota_snapshots").and_then(|s| s.as_object()) {
        for (key, label) in [
            ("premium_interactions", "Premium"),
            ("chat", "Chat"),
            ("completions", "Completions"),
        ] {
            let Some(snapshot) = snapshots.get(key) else {
                continue;
            };
            if snapshot.get("unlimited").and_then(|v| v.as_bool()) == Some(true) {
                continue;
            }
            let used = match snapshot.get("percent_remaining").and_then(json_f64) {
                Some(remaining) => (100.0 - remaining) / 100.0,
                None => {
                    let entitlement = snapshot.get("entitlement").and_then(json_f64)?;
                    let remaining = snapshot.get("remaining").and_then(json_f64)?;
                    if entitlement <= 0.0 {
                        continue;
                    }
                    1.0 - remaining / entitlement
                }
            };
            windows.push(AgentUsageWindow {
                label: label.to_string(),
                used_fraction: used.clamp(0.0, 1.0) as f32,
                resets_at,
            });
        }
    } else if let (Some(monthly), Some(left)) = (
        body.get("monthly_quotas").and_then(|m| m.as_object()),
        body.get("limited_user_quotas").and_then(|m| m.as_object()),
    ) {
        for (key, label) in [("chat", "Chat"), ("completions", "Completions")] {
            if let (Some(total), Some(remaining)) = (
                monthly.get(key).and_then(json_f64),
                left.get(key).and_then(json_f64),
            ) && total > 0.0
            {
                windows.push(AgentUsageWindow {
                    label: label.to_string(),
                    used_fraction: (1.0 - remaining / total).clamp(0.0, 1.0) as f32,
                    resets_at,
                });
            }
        }
    }
    let plan_label = str_field(body, "copilot_plan").map(|plan| {
        let name = match plan.as_str() {
            "individual" => "Pro".to_string(),
            "individual_pro" => "Pro+".to_string(),
            other => other
                .split('_')
                .map(|w| {
                    let mut c = w.chars();
                    c.next()
                        .map(|f| format!("{}{}", f.to_uppercase(), c.as_str()))
                        .unwrap_or_default()
                })
                .collect::<Vec<_>>()
                .join(" "),
        };
        format!("Copilot {name}")
    });
    (!windows.is_empty() || plan_label.is_some()).then_some(UsageSnapshot {
        windows,
        plan_label,
    })
}

/// The Nous portal's account: the subscription's monthly credits.
pub(super) fn nous_usage_snapshot(body: &serde_json::Value) -> Option<UsageSnapshot> {
    let subscription = body.get("subscription").filter(|s| s.is_object());
    let plan_label = subscription.and_then(|s| str_field(s, "plan")).map(|plan| {
        let mut chars = plan.chars();
        let plan = chars
            .next()
            .map(|f| format!("{}{}", f.to_uppercase(), chars.as_str()))
            .unwrap_or_default();
        format!("Nous {plan}")
    });
    let mut windows = Vec::new();
    if let Some(sub) = subscription
        && let Some(monthly) = sub.get("monthly_credits").and_then(json_f64)
        && let Some(remaining) = sub.get("credits_remaining").and_then(json_f64)
        && monthly > 0.0
    {
        windows.push(AgentUsageWindow {
            label: "Month".to_string(),
            used_fraction: (1.0 - remaining / monthly).clamp(0.0, 1.0) as f32,
            resets_at: parse_when(sub.get("current_period_end")),
        });
    }
    (body.get("user").is_some() || subscription.is_some()).then_some(UsageSnapshot {
        windows,
        plan_label,
    })
}

/// OpenCode Go's `/zen/go/v1/usage`: `{usage: {rolling, weekly, monthly}}`,
/// each `{status, percent (0-100), resetsAt (ISO)}`. The route's first
/// shape (`rollingUsage: {usagePercent, resetInSec}`, …) still reads, in
/// case an older deployment answers. `None` when no window is there.
pub(super) fn opencode_go_usage_snapshot(
    body: &serde_json::Value,
    now: DateTime<Utc>,
) -> Option<UsageSnapshot> {
    let current = body.get("usage").filter(|u| u.is_object());
    let windows: Vec<AgentUsageWindow> = [
        ("5h", "rolling", "rollingUsage"),
        ("Week", "weekly", "weeklyUsage"),
        ("Month", "monthly", "monthlyUsage"),
    ]
    .into_iter()
    .filter_map(|(label, new, old)| {
        let window = current
            .and_then(|u| u.get(new))
            .or_else(|| body.get(old))
            .filter(|w| w.is_object())?;
        let limited = str_field(window, "status").as_deref() == Some("rate-limited");
        let percent = window
            .get("percent")
            .or_else(|| window.get("usagePercent"))
            .and_then(json_f64)
            .or(limited.then_some(100.0))?;
        let resets_at = parse_when(window.get("resetsAt")).or_else(|| {
            let secs = window.get("resetInSec").and_then(json_f64)?;
            Some(now + chrono::TimeDelta::seconds(secs.max(0.0) as i64))
        });
        Some(AgentUsageWindow {
            label: label.to_string(),
            used_fraction: if limited {
                1.0
            } else {
                (percent / 100.0).clamp(0.0, 1.0) as f32
            },
            resets_at,
        })
    })
    .collect();
    (!windows.is_empty()).then(|| UsageSnapshot {
        windows,
        plan_label: Some("OpenCode Go".into()),
    })
}

/// ClinePass's `usage-limits`: `{data: {limits: [{type, percentUsed,
/// resetsAt}]}}`, `type` one of `five_hour`, `weekly`, `monthly`.
pub(super) fn cline_usage_snapshot(body: &serde_json::Value) -> Option<UsageSnapshot> {
    let limits = body
        .get("data")
        .unwrap_or(body)
        .get("limits")?
        .as_array()?;
    let windows: Vec<AgentUsageWindow> = [("5h", "five_hour"), ("Week", "weekly"), ("Month", "monthly")]
        .into_iter()
        .filter_map(|(label, kind)| {
            let limit = limits
                .iter()
                .find(|l| str_field(l, "type").as_deref() == Some(kind))?;
            let percent = limit.get("percentUsed").and_then(json_f64)?;
            Some(AgentUsageWindow {
                label: label.to_string(),
                used_fraction: (percent / 100.0).clamp(0.0, 1.0) as f32,
                resets_at: parse_when(limit.get("resetsAt")),
            })
        })
        .collect();
    (!windows.is_empty()).then(|| UsageSnapshot {
        windows,
        plan_label: Some("ClinePass".into()),
    })
}

/// A number arriving as JSON number or numeric string (proto3 int64s).
fn json_f64(value: &serde_json::Value) -> Option<f64> {
    match value {
        serde_json::Value::Number(n) => n.as_f64(),
        serde_json::Value::String(s) => s.parse().ok(),
        _ => None,
    }
}

/// Unix seconds as a number or proto3 int64 string.
fn json_secs(value: Option<&serde_json::Value>) -> Option<DateTime<Utc>> {
    DateTime::<Utc>::from_timestamp(json_f64(value?)? as i64, 0)
}

/// A bare `YYYY-MM-DD` as that day's UTC midnight.
fn day_start(value: Option<&serde_json::Value>) -> Option<DateTime<Utc>> {
    let day = chrono::NaiveDate::parse_from_str(value?.as_str()?, "%Y-%m-%d").ok()?;
    Some(day.and_hms_opt(0, 0, 0)?.and_utc())
}
