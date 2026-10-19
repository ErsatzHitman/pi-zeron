//! Settings → API keys: one store for every provider API key Zeron and the
//! Pi it hosts use, so a key is added, edited or switched in one place and
//! takes effect without a /reload or a restart.
//!
//! The store is `<pi agent dir>/api-keys.json` (0600), beside Pi's
//! `auth.json`, because its readers run there: Pi extensions (the OpenCode
//! Go key router, the decisions gate, memory, uniichat, web tools) and MCP
//! servers (video inspection) read it on every request through the Pi repo's
//! `lib/api-keys.ts` and `video_inspection/openrouter.py`, and Zeron's own
//! voice features read it per dictation and per call ([`Reader`]). Each
//! provider holds any number of labelled keys and one active key.
//!
//! The engine is the only writer. After every change it projects each Pi
//! provider's active key into Pi's `auth.json` (escaped as Pi reads config
//! values), which Pi re-reads on every request, so running chats switch key
//! on their next request. A provider signed in with OAuth there is never
//! overwritten. The OpenCode Go router's per-key health
//! (`api-keys-health.json`, ids only) is read back for the page's badges.
//!
//! Secrets never leave the engine in a reply: the page sees labels, ids and
//! the last four characters.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::SystemTime;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::EngineError;
use crate::agent_accounts::AgentAccounts;

pub const STORE_FILE: &str = "api-keys.json";
pub const HEALTH_FILE: &str = "api-keys-health.json";
/// The provider whose active key Pi's `web_search` and `web_fetch` tools
/// (the web-tools extension, DeepSeek's Search and Fetch APIs) use.
pub const WEB_TOOLS_PROVIDER: &str = "deepseek";

/// Providers the page always offers, with who uses their keys. `pi` marks
/// a Pi provider id: its active key is projected into Pi's `auth.json`.
pub const KNOWN: &[Known] = &[
    Known::pi(
        "openrouter",
        "OpenRouter",
        "Pi models, decisions gate, video inspection, voice mode, uniichat",
    ),
    Known::pi(
        "opencode-go",
        "OpenCode Go",
        "Pi models; several keys rotate as quota runs out",
    ),
    Known::pi("anthropic", "Anthropic", "Pi models"),
    Known::pi("openai", "OpenAI", "Pi models, dictation, voice mode"),
    Known::pi(
        "google",
        "Google Gemini",
        "Pi models, voice mode (real-time)",
    ),
    Known::pi("groq", "Groq", "Pi models, dictation"),
    Known::pi("mistral", "Mistral", "Pi models, voice mode"),
    Known::pi("xai", "xAI", "Pi models"),
    Known::pi("deepseek", "DeepSeek", "Pi models, web search & fetch"),
    Known::other("subq", "SubQ", "Decisions gate (SubQ Snap)"),
    Known::other("nvidia", "NVIDIA", "Memory embeddings"),
    Known::other("deepgram", "Deepgram", "Voice mode"),
    Known::other("dashscope", "DashScope", "Voice mode (real-time)"),
];

#[derive(Debug, Clone, Copy)]
pub struct Known {
    pub name: &'static str,
    pub label: &'static str,
    pub used_by: &'static str,
    pub pi: bool,
}

impl Known {
    const fn pi(name: &'static str, label: &'static str, used_by: &'static str) -> Self {
        Self {
            name,
            label,
            used_by,
            pi: true,
        }
    }
    const fn other(name: &'static str, label: &'static str, used_by: &'static str) -> Self {
        Self {
            name,
            label,
            used_by,
            pi: false,
        }
    }
    pub fn of(name: &str) -> Option<&'static Known> {
        KNOWN.iter().find(|k| k.name == name)
    }
}

// ── The store ───────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Store {
    pub version: u32,
    /// Bumped on every write.
    pub rev: u64,
    pub providers: BTreeMap<String, ProviderKeys>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ProviderKeys {
    pub active: Option<String>,
    pub keys: Vec<KeyEntry>,
}

#[derive(Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct KeyEntry {
    /// `sha256(secret)`, first 8 hex digits.
    pub id: String,
    pub label: String,
    /// The literal secret, never escaped.
    pub secret: String,
    pub added_at: u64,
    pub disabled: bool,
}

impl std::fmt::Debug for KeyEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KeyEntry")
            .field("id", &self.id)
            .field("label", &self.label)
            .field("secret", &mask(&self.secret))
            .field("added_at", &self.added_at)
            .field("disabled", &self.disabled)
            .finish()
    }
}

impl ProviderKeys {
    /// The active key if enabled, else the first enabled one.
    pub fn current(&self) -> Option<&KeyEntry> {
        self.active
            .as_deref()
            .and_then(|id| self.keys.iter().find(|k| k.id == id && !k.disabled))
            .or_else(|| self.keys.iter().find(|k| !k.disabled))
    }
}

impl Store {
    pub fn load(path: &Path) -> Result<Self, EngineError> {
        match std::fs::read(path) {
            Ok(bytes) => serde_json::from_slice(&bytes).map_err(|e| {
                EngineError::Other(format!(
                    "{} could not be parsed ({e}); fix or remove it to manage keys here.",
                    path.display()
                ))
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self {
                version: 1,
                ..Self::default()
            }),
            Err(e) => Err(e.into()),
        }
    }

    pub fn save(&mut self, path: &Path) -> Result<(), EngineError> {
        self.version = 1;
        self.rev += 1;
        let bytes = serde_json::to_vec_pretty(self)
            .map_err(|e| EngineError::Other(format!("serialize {}: {e}", path.display())))?;
        write_private(path, &bytes)
    }

    /// The current secret of `provider`.
    pub fn key(&self, provider: &str) -> Option<&str> {
        self.providers
            .get(provider)?
            .current()
            .map(|k| k.secret.as_str())
    }
}

/// Write `bytes` to `path` atomically, readable by its owner only.
fn write_private(path: &Path, bytes: &[u8]) -> Result<(), EngineError> {
    let dir = path
        .parent()
        .ok_or_else(|| EngineError::Other(format!("{} has no parent", path.display())))?;
    std::fs::create_dir_all(dir)?;
    let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        tmp.as_file()
            .set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    std::io::Write::write_all(&mut tmp, bytes)?;
    tmp.as_file().sync_all()?;
    tmp.persist(path).map_err(|e| EngineError::from(e.error))?;
    Ok(())
}

pub fn key_id(secret: &str) -> String {
    let digest = Sha256::digest(secret.as_bytes());
    digest[..4].iter().map(|b| format!("{b:02x}")).collect()
}

/// `…ab12`: what a page may show of a secret.
pub fn mask(secret: &str) -> String {
    let tail: String = secret
        .chars()
        .rev()
        .take(4)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    if secret.chars().count() <= 8 {
        "…".into()
    } else {
        format!("…{tail}")
    }
}

/// A pasted key: no surrounding space, quotes or control characters (a
/// stray control byte once made every request fail its header check).
pub fn sanitize(secret: &str) -> String {
    let bare: String = secret
        .chars()
        .filter(|c| !c.is_control() && !c.is_whitespace())
        .collect();
    bare.trim_matches(|c| c == '"' || c == '\'').to_owned()
}

/// A literal secret as a Pi config value: `$` → `$$`, a leading `!` →
/// `$!`, so Pi never expands or runs it.
pub fn escape_for_pi(secret: &str) -> String {
    let escaped = secret.replace('$', "$$");
    match escaped.strip_prefix('!') {
        Some(rest) => format!("$!{rest}"),
        None => escaped,
    }
}

/// The inverse of [`escape_for_pi`] for a value Zeron can read literally:
/// `None` for a `!command` or a `$VAR` reference.
pub fn unescape_from_pi(value: &str) -> Option<String> {
    if value.starts_with('!') {
        return None;
    }
    let mut out = String::with_capacity(value.len());
    let mut chars = value.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '$' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('$') => out.push('$'),
            Some('!') => out.push('!'),
            // `$NAME` / `${NAME}` read the environment: not a literal.
            Some(_) => return None,
            None => out.push('$'),
        }
    }
    Some(out)
}

// ── Health (written by the Pi key router) ───────────────────────────────────

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct HealthFile {
    pub providers: BTreeMap<String, ProviderHealth>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ProviderHealth {
    /// The key the router is on now.
    pub current: Option<String>,
    pub keys: BTreeMap<String, KeyHealth>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct KeyHealth {
    /// `ok`, `exhausted`, `cooldown`, `dead` or `unknown`.
    pub status: String,
    /// Unix ms when a blocked key is usable again.
    pub until: Option<u64>,
    pub last_error_class: Option<String>,
    pub reset_hint: Option<String>,
}

// ── What the page sees ──────────────────────────────────────────────────────

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ApiKeysView {
    /// Where the store lives, for the page's footnote.
    pub path: String,
    pub providers: Vec<ProviderView>,
    /// Why the store could not be read, when it could not.
    pub error: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ProviderView {
    pub name: String,
    pub label: String,
    pub used_by: String,
    /// Projected into Pi's `auth.json`.
    pub pi: bool,
    pub keys: Vec<KeyView>,
    /// Pi is signed in to this provider with OAuth: an API key here is
    /// not used by Pi.
    pub oauth: bool,
    /// Pi's `auth.json` holds a key Zeron did not set (a `/login`, a
    /// command or an environment reference), masked when literal.
    pub pi_login: Option<String>,
    /// The key the OpenCode Go router is on now, when it differs.
    pub router_current: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct KeyView {
    pub id: String,
    pub label: String,
    pub masked: String,
    pub disabled: bool,
    pub active: bool,
    pub added_at: u64,
    pub health: Option<KeyHealth>,
}

// ── RPC params ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct AddParams {
    pub provider: String,
    pub label: String,
    pub secret: String,
    /// Make it the active key (the first key of a provider always is).
    pub activate: bool,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct UpdateParams {
    pub provider: String,
    pub id: String,
    pub label: Option<String>,
    /// A replacement secret (the key is re-identified by it).
    pub secret: Option<String>,
    pub disabled: Option<bool>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct KeyRef {
    pub provider: String,
    pub id: String,
}

// ── The service ─────────────────────────────────────────────────────────────

/// The engine's key store: list, add, edit, switch and remove, each write
/// followed by the `auth.json` projection.
pub struct ApiKeys {
    agent_dir: PathBuf,
    accounts: Option<AgentAccounts>,
}

/// Serialises the engine's writers (the file has no other writer).
static WRITE_LOCK: Mutex<()> = Mutex::new(());

impl ApiKeys {
    /// The store beside the Pi these accounts manage.
    pub fn for_accounts(accounts: &AgentAccounts) -> Self {
        Self::new(accounts.pi_agent_dir(), Some(accounts.clone()))
    }

    pub fn new(agent_dir: PathBuf, accounts: Option<AgentAccounts>) -> Self {
        Self {
            agent_dir,
            accounts,
        }
    }

    pub fn store_path(&self) -> PathBuf {
        self.agent_dir.join(STORE_FILE)
    }

    fn auth_file(&self) -> PathBuf {
        self.agent_dir.join("auth.json")
    }

    pub fn list(&self) -> ApiKeysView {
        let path = self.store_path();
        let (store, error) = match Store::load(&path) {
            Ok(store) => (store, None),
            Err(e) => (Store::default(), Some(e.to_string())),
        };
        let health: HealthFile = std::fs::read(self.agent_dir.join(HEALTH_FILE))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        let auth: serde_json::Map<String, serde_json::Value> = std::fs::read(self.auth_file())
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        let mut names: Vec<String> = KNOWN.iter().map(|k| k.name.to_owned()).collect();
        for name in store.providers.keys().chain(auth.keys()) {
            if !names.contains(name)
                && (store.providers.contains_key(name)
                    || auth.get(name).and_then(|e| e["type"].as_str()) == Some("api_key"))
            {
                names.push(name.clone());
            }
        }
        let providers = names
            .into_iter()
            .map(|name| {
                let known = Known::of(&name);
                let keys = store.providers.get(&name).cloned().unwrap_or_default();
                let current = keys.current().map(|k| k.id.clone());
                let health = health.providers.get(&name);
                let entry = auth.get(&name);
                let oauth = entry.and_then(|e| e["type"].as_str()) == Some("oauth");
                let pi_login = entry
                    .filter(|e| e["type"].as_str() == Some("api_key"))
                    .and_then(|e| e["key"].as_str())
                    .filter(|value| {
                        unescape_from_pi(value)
                            .is_none_or(|secret| !keys.keys.iter().any(|k| k.secret == secret))
                    })
                    .map(|value| match unescape_from_pi(value) {
                        Some(secret) => mask(&secret),
                        None if value.starts_with('!') => "a command".into(),
                        None => "an environment variable".into(),
                    });
                ProviderView {
                    label: known.map_or_else(|| name.clone(), |k| k.label.to_owned()),
                    used_by: known.map_or_else(|| "Pi models".to_owned(), |k| k.used_by.to_owned()),
                    pi: known.is_none_or(|k| k.pi),
                    keys: keys
                        .keys
                        .iter()
                        .map(|k| KeyView {
                            id: k.id.clone(),
                            label: k.label.clone(),
                            masked: mask(&k.secret),
                            disabled: k.disabled,
                            active: current.as_deref() == Some(k.id.as_str()),
                            added_at: k.added_at,
                            health: health.and_then(|h| h.keys.get(&k.id)).cloned(),
                        })
                        .collect(),
                    oauth,
                    pi_login,
                    router_current: health
                        .and_then(|h| h.current.clone())
                        .filter(|id| Some(id) != current.as_ref()),
                    name,
                }
            })
            .collect();
        ApiKeysView {
            path: path.display().to_string(),
            providers,
            error,
        }
    }

    pub fn add(&self, p: AddParams) -> Result<ApiKeysView, EngineError> {
        let provider = provider_name(&p.provider)?;
        let secret = sanitize(&p.secret);
        if secret.len() < 8 {
            return Err(EngineError::Other(
                "That does not look like an API key.".into(),
            ));
        }
        let id = key_id(&secret);
        self.write(&provider, &[], |keys| {
            if keys.keys.iter().any(|k| k.id == id) {
                return Err(EngineError::Other("That key is already saved.".into()));
            }
            let label = p.label.trim();
            keys.keys.push(KeyEntry {
                id: id.clone(),
                label: if label.is_empty() {
                    format!("Key {}", keys.keys.len() + 1)
                } else {
                    label.chars().take(60).collect()
                },
                secret,
                added_at: now_ms(),
                disabled: false,
            });
            if p.activate || keys.current().is_none_or(|k| k.id == id) {
                keys.active = Some(id.clone());
            }
            Ok(())
        })
    }

    pub fn update(&self, p: UpdateParams) -> Result<ApiKeysView, EngineError> {
        let provider = provider_name(&p.provider)?;
        let mut replaced = Vec::new();
        let new_secret = p.secret.as_deref().map(sanitize).filter(|s| !s.is_empty());
        if new_secret.as_ref().is_some_and(|s| s.len() < 8) {
            return Err(EngineError::Other(
                "That does not look like an API key.".into(),
            ));
        }
        let old = Store::load(&self.store_path())?
            .providers
            .get(&provider)
            .and_then(|keys| keys.keys.iter().find(|k| k.id == p.id).cloned());
        if let (Some(old), Some(_)) = (&old, &new_secret) {
            replaced.push(old.secret.clone());
        }
        self.write(&provider, &replaced, |keys| {
            let was_active = keys.active.as_deref() == Some(p.id.as_str());
            if let Some(secret) = &new_secret {
                let id = key_id(secret);
                if id != p.id && keys.keys.iter().any(|k| k.id == id) {
                    return Err(EngineError::Other("That key is already saved.".into()));
                }
            }
            let entry = keys
                .keys
                .iter_mut()
                .find(|k| k.id == p.id)
                .ok_or_else(|| EngineError::Other("That key is gone.".into()))?;
            if let Some(label) = &p.label {
                let label = label.trim();
                if !label.is_empty() {
                    entry.label = label.chars().take(60).collect();
                }
            }
            if let Some(disabled) = p.disabled {
                entry.disabled = disabled;
            }
            if let Some(secret) = &new_secret {
                entry.id = key_id(secret);
                entry.secret = secret.clone();
                if was_active {
                    keys.active = Some(entry.id.clone());
                }
            }
            Ok(())
        })
    }

    pub fn set_active(&self, p: KeyRef) -> Result<ApiKeysView, EngineError> {
        let provider = provider_name(&p.provider)?;
        self.write(&provider, &[], |keys| {
            let entry = keys
                .keys
                .iter_mut()
                .find(|k| k.id == p.id)
                .ok_or_else(|| EngineError::Other("That key is gone.".into()))?;
            entry.disabled = false;
            keys.active = Some(p.id.clone());
            Ok(())
        })
    }

    pub fn remove(&self, p: KeyRef) -> Result<ApiKeysView, EngineError> {
        let provider = provider_name(&p.provider)?;
        let removed: Vec<String> = Store::load(&self.store_path())?
            .providers
            .get(&provider)
            .and_then(|keys| keys.keys.iter().find(|k| k.id == p.id))
            .map(|k| vec![k.secret.clone()])
            .unwrap_or_default();
        self.write(&provider, &removed, |keys| {
            keys.keys.retain(|k| k.id != p.id);
            if keys.active.as_deref() == Some(p.id.as_str()) {
                keys.active = keys.keys.iter().find(|k| !k.disabled).map(|k| k.id.clone());
            }
            Ok(())
        })
    }

    /// Apply `change` to `provider`, save, and project. `gone` are secrets
    /// the change retires: Pi's entry is removed only if it still holds one.
    fn write(
        &self,
        provider: &str,
        gone: &[String],
        change: impl FnOnce(&mut ProviderKeys) -> Result<(), EngineError>,
    ) -> Result<ApiKeysView, EngineError> {
        {
            let _guard = WRITE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            let path = self.store_path();
            let mut store = Store::load(&path)?;
            let keys = store.providers.entry(provider.to_owned()).or_default();
            change(keys)?;
            if keys.keys.is_empty() {
                store.providers.remove(provider);
            }
            store.save(&path)?;
            self.project(provider, store.key(provider), gone)?;
        }
        Ok(self.list())
    }

    /// Keep Pi's `auth.json` entry for `provider` on its active key.
    fn project(
        &self,
        provider: &str,
        secret: Option<&str>,
        gone: &[String],
    ) -> Result<(), EngineError> {
        if Known::of(provider).is_some_and(|k| !k.pi) {
            return Ok(());
        }
        let current = std::fs::read(self.auth_file())
            .ok()
            .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
            .and_then(|v| v.get(provider).cloned());
        let entry_type = current.as_ref().and_then(|e| e["type"].as_str());
        if entry_type == Some("oauth") {
            // Signed in with OAuth: that login wins in Pi; leave it.
            return Ok(());
        }
        let desired =
            secret.map(|s| serde_json::json!({ "type": "api_key", "key": escape_for_pi(s) }));
        match desired {
            Some(entry) if current.as_ref() != Some(&entry) => {
                self.write_auth(provider, Some(&entry))
            }
            Some(_) => Ok(()),
            None => {
                // No key left here: drop Pi's entry only if Zeron put it there.
                let held = current
                    .as_ref()
                    .and_then(|e| e["key"].as_str())
                    .and_then(unescape_from_pi);
                if entry_type == Some("api_key") && held.is_some_and(|k| gone.contains(&k)) {
                    self.write_auth(provider, None)
                } else {
                    Ok(())
                }
            }
        }
    }

    fn write_auth(
        &self,
        provider: &str,
        entry: Option<&serde_json::Value>,
    ) -> Result<(), EngineError> {
        match &self.accounts {
            // Under Pi's own lock, compare-before-write, never over a file
            // that does not parse.
            Some(accounts) => accounts.write_pi_api_key(provider, entry),
            None => crate::agent_accounts::merge_pi_auth_entry(&self.auth_file(), provider, entry),
        }
    }

    /// First run: the keys Pi already holds as literal API keys become the
    /// store's, so the page opens on what is in use.
    pub fn import_from_pi(&self) -> Result<(), EngineError> {
        let path = self.store_path();
        if path.exists() {
            return Ok(());
        }
        let auth: serde_json::Map<String, serde_json::Value> = match std::fs::read(self.auth_file())
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
        {
            Some(auth) => auth,
            None => return Ok(()),
        };
        let mut store = Store::default();
        for (provider, entry) in auth {
            if entry["type"].as_str() != Some("api_key") {
                continue;
            }
            let Some(secret) = entry["key"].as_str().and_then(unescape_from_pi) else {
                continue;
            };
            let secret = sanitize(&secret);
            if secret.len() < 8 {
                continue;
            }
            let id = key_id(&secret);
            store.providers.insert(
                provider,
                ProviderKeys {
                    active: Some(id.clone()),
                    keys: vec![KeyEntry {
                        id,
                        label: "From Pi".into(),
                        secret,
                        added_at: now_ms(),
                        disabled: false,
                    }],
                },
            );
        }
        if store.providers.is_empty() {
            return Ok(());
        }
        let _guard = WRITE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        if path.exists() {
            return Ok(());
        }
        store.save(&path)
    }
}

fn provider_name(name: &str) -> Result<String, EngineError> {
    let name = name.trim().to_ascii_lowercase();
    let ok = !name.is_empty()
        && name.len() <= 40
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
    if ok {
        Ok(name)
    } else {
        Err(EngineError::Other(
            "A provider name is letters, digits, '-', '_' or '.'.".into(),
        ))
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

// ── Reading it back, cheaply, per use ───────────────────────────────────────

/// A cached reader of the store for Zeron's own consumers (dictation, voice
/// mode): one `stat` per read, a re-parse only when the file changed, and
/// the last good parse kept if a write is caught half-done.
pub struct Reader {
    path: PathBuf,
    cache: Mutex<Option<(Stamp, Store)>>,
}

#[derive(Debug, Clone, PartialEq)]
struct Stamp {
    len: u64,
    modified: Option<SystemTime>,
    /// Every write renames a new file into place, so a new inode marks a
    /// change even where mtime is coarse and the length is unchanged.
    inode: u64,
}

impl Reader {
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            cache: Mutex::new(None),
        }
    }

    /// The store at Pi's default agent directory (`PI_CODING_AGENT_DIR`,
    /// else `~/.pi/agent`), as the engine on this device writes it.
    pub fn default_location() -> Self {
        Self::new(crate::agent_accounts::default_pi_agent_dir().join(STORE_FILE))
    }

    /// The current key of `provider`, read fresh if the file changed.
    pub fn key(&self, provider: &str) -> Option<String> {
        let meta = std::fs::metadata(&self.path).ok()?;
        let stamp = Stamp {
            len: meta.len(),
            modified: meta.modified().ok(),
            #[cfg(unix)]
            inode: std::os::unix::fs::MetadataExt::ino(&meta),
            #[cfg(not(unix))]
            inode: 0,
        };
        let mut cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
        if cache.as_ref().is_none_or(|(s, _)| *s != stamp)
            && let Ok(bytes) = std::fs::read(&self.path)
            && let Ok(store) = serde_json::from_slice::<Store>(&bytes)
        {
            *cache = Some((stamp, store));
        }
        cache.as_ref()?.1.key(provider).map(str::to_owned)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys(dir: &Path) -> ApiKeys {
        ApiKeys::new(dir.to_path_buf(), None)
    }

    fn auth(dir: &Path) -> serde_json::Value {
        serde_json::from_slice(&std::fs::read(dir.join("auth.json")).unwrap()).unwrap()
    }

    fn add(k: &ApiKeys, provider: &str, label: &str, secret: &str) -> ApiKeysView {
        k.add(AddParams {
            provider: provider.into(),
            label: label.into(),
            secret: secret.into(),
            activate: false,
        })
        .unwrap()
    }

    fn provider<'a>(view: &'a ApiKeysView, name: &str) -> &'a ProviderView {
        view.providers.iter().find(|p| p.name == name).unwrap()
    }

    #[test]
    fn a_key_is_saved_privately_and_pi_gets_it_escaped() {
        let dir = tempfile::tempdir().unwrap();
        let k = keys(dir.path());
        let view = add(
            &k,
            "openrouter",
            "Personal",
            " \"sk-or-$HOME-abcd1234\"\u{14} ",
        );
        let row = provider(&view, "openrouter");
        assert_eq!(row.keys.len(), 1);
        assert!(row.keys[0].active);
        assert_eq!(row.keys[0].masked, "…1234");
        assert_eq!(row.keys[0].id, key_id("sk-or-$HOME-abcd1234"));
        let store = Store::load(&k.store_path()).unwrap();
        assert_eq!(store.key("openrouter"), Some("sk-or-$HOME-abcd1234"));
        assert_eq!(store.rev, 1);
        assert_eq!(
            auth(dir.path())["openrouter"],
            serde_json::json!({"type": "api_key", "key": "sk-or-$$HOME-abcd1234"}),
            "Pi must not expand $HOME"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(k.store_path())
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        let json = serde_json::to_string(&view).unwrap();
        assert!(!json.contains("abcd1234"), "a reply never carries a secret");
        assert!(!format!("{:?}", store).contains("abcd1234"));
    }

    #[test]
    fn switching_the_active_key_switches_pis_key() {
        let dir = tempfile::tempdir().unwrap();
        let k = keys(dir.path());
        add(&k, "opencode-go", "A", "key-aaaaaaaa1");
        let view = add(&k, "opencode-go", "B", "key-bbbbbbbb2");
        let row = provider(&view, "opencode-go");
        assert!(
            row.keys[0].active && !row.keys[1].active,
            "the first key stays active"
        );
        let b = row.keys[1].id.clone();
        k.set_active(KeyRef {
            provider: "opencode-go".into(),
            id: b.clone(),
        })
        .unwrap();
        assert_eq!(auth(dir.path())["opencode-go"]["key"], "key-bbbbbbbb2");
        // Disabling the active key falls back to the next enabled one.
        k.update(UpdateParams {
            provider: "opencode-go".into(),
            id: b,
            disabled: Some(true),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(auth(dir.path())["opencode-go"]["key"], "key-aaaaaaaa1");
    }

    #[test]
    fn an_oauth_login_and_other_logins_are_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("auth.json"),
            serde_json::to_vec(&serde_json::json!({
                "anthropic": {"type": "oauth", "access": "tok", "refresh": "r", "expires": 1},
                "groq": {"type": "api_key", "key": "gsk-from-login-9999"}
            }))
            .unwrap(),
        )
        .unwrap();
        let k = keys(dir.path());
        let view = add(&k, "anthropic", "", "sk-ant-api-xxxx5555");
        assert!(provider(&view, "anthropic").oauth);
        assert_eq!(auth(dir.path())["anthropic"]["type"], "oauth");
        assert_eq!(auth(dir.path())["groq"]["key"], "gsk-from-login-9999");
        assert_eq!(provider(&view, "groq").pi_login.as_deref(), Some("…9999"));
        assert_eq!(provider(&view, "anthropic").keys[0].label, "Key 1");
    }

    #[test]
    fn removing_the_last_key_removes_only_what_zeron_wrote() {
        let dir = tempfile::tempdir().unwrap();
        let k = keys(dir.path());
        let view = add(&k, "openrouter", "x", "sk-or-1111aaaa");
        let id = provider(&view, "openrouter").keys[0].id.clone();
        k.remove(KeyRef {
            provider: "openrouter".into(),
            id,
        })
        .unwrap();
        assert!(auth(dir.path()).get("openrouter").is_none());
        // A key Pi got some other way survives Zeron removing its own.
        let view = add(&k, "openrouter", "x", "sk-or-2222bbbb");
        std::fs::write(
            dir.path().join("auth.json"),
            r#"{"openrouter": {"type": "api_key", "key": "!op read key"}}"#,
        )
        .unwrap();
        let id = provider(&view, "openrouter").keys[0].id.clone();
        k.remove(KeyRef {
            provider: "openrouter".into(),
            id,
        })
        .unwrap();
        assert_eq!(auth(dir.path())["openrouter"]["key"], "!op read key");
    }

    #[test]
    fn voice_providers_stay_out_of_pi() {
        let dir = tempfile::tempdir().unwrap();
        let k = keys(dir.path());
        let view = add(&k, "deepgram", "", "dg-key-12345678");
        assert!(!provider(&view, "deepgram").pi);
        assert!(!dir.path().join("auth.json").exists());
        let reader = Reader::new(k.store_path());
        assert_eq!(reader.key("deepgram").as_deref(), Some("dg-key-12345678"));
    }

    #[test]
    fn the_web_tools_key_is_always_listed() {
        let dir = tempfile::tempdir().unwrap();
        let k = keys(dir.path());
        let view = k.list();
        let row = provider(&view, WEB_TOOLS_PROVIDER);
        assert!(row.keys.is_empty() && row.pi);
        assert!(row.used_by.contains("Pi models"), "{}", row.used_by);
        assert!(
            row.used_by.contains("web search & fetch"),
            "{}",
            row.used_by
        );
        // Its key goes to Pi like any model key, where the extension reads it.
        let view = add(&k, WEB_TOOLS_PROVIDER, "", "sk-deepseek-abcd9876");
        assert_eq!(provider(&view, WEB_TOOLS_PROVIDER).keys[0].masked, "…9876");
        assert_eq!(
            auth(dir.path())[WEB_TOOLS_PROVIDER]["key"],
            "sk-deepseek-abcd9876"
        );
        assert_eq!(
            Store::load(&k.store_path())
                .unwrap()
                .key(WEB_TOOLS_PROVIDER),
            Some("sk-deepseek-abcd9876")
        );
    }

    #[test]
    fn the_reader_follows_every_write() {
        let dir = tempfile::tempdir().unwrap();
        let k = keys(dir.path());
        let reader = Reader::new(k.store_path());
        assert_eq!(reader.key("openai"), None);
        add(&k, "openai", "a", "sk-proj-aaaa0001");
        assert_eq!(reader.key("openai").as_deref(), Some("sk-proj-aaaa0001"));
        let view = add(&k, "openai", "b", "sk-proj-bbbb0002-longer");
        let b = provider(&view, "openai").keys[1].id.clone();
        k.set_active(KeyRef {
            provider: "openai".into(),
            id: b,
        })
        .unwrap();
        assert_eq!(
            reader.key("openai").as_deref(),
            Some("sk-proj-bbbb0002-longer")
        );
        // A torn write keeps the last good parse.
        std::fs::write(k.store_path(), b"{not json, and a different length").unwrap();
        assert_eq!(
            reader.key("openai").as_deref(),
            Some("sk-proj-bbbb0002-longer")
        );
    }

    #[test]
    fn a_replaced_secret_keeps_its_place_and_pi_follows() {
        let dir = tempfile::tempdir().unwrap();
        let k = keys(dir.path());
        let view = add(&k, "openrouter", "Work", "sk-or-old-0000");
        let id = provider(&view, "openrouter").keys[0].id.clone();
        let view = k
            .update(UpdateParams {
                provider: "openrouter".into(),
                id,
                label: Some("Work 2".into()),
                secret: Some("sk-or-new-1111".into()),
                ..Default::default()
            })
            .unwrap();
        let row = &provider(&view, "openrouter").keys[0];
        assert_eq!((row.label.as_str(), row.active), ("Work 2", true));
        assert_eq!(auth(dir.path())["openrouter"]["key"], "sk-or-new-1111");
    }

    #[test]
    fn pis_literal_keys_are_imported_once() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("auth.json"),
            r#"{"openrouter": {"type": "api_key", "key": "sk-or-a$$b-1234"},
                "groq": {"type": "api_key", "key": "$GROQ_KEY"},
                "anthropic": {"type": "oauth", "access": "x"}}"#,
        )
        .unwrap();
        let k = keys(dir.path());
        k.import_from_pi().unwrap();
        let store = Store::load(&k.store_path()).unwrap();
        assert_eq!(store.key("openrouter"), Some("sk-or-a$b-1234"));
        assert_eq!(store.providers.len(), 1, "references and OAuth stay Pi's");
        std::fs::write(k.store_path(), r#"{"version":1,"providers":{}}"#).unwrap();
        k.import_from_pi().unwrap();
        assert!(Store::load(&k.store_path()).unwrap().providers.is_empty());
    }

    #[test]
    fn pi_escaping_round_trips() {
        for secret in ["a$b", "!x", "$HOME", "${X}", "$$", "plain-key", "!$!"] {
            assert_eq!(
                unescape_from_pi(&escape_for_pi(secret)).as_deref(),
                Some(secret),
                "{secret}"
            );
        }
        assert_eq!(escape_for_pi("!x"), "$!x");
        assert_eq!(unescape_from_pi("!cmd"), None);
        assert_eq!(unescape_from_pi("$VAR"), None);
        assert_eq!(mask("short"), "…");
    }

    #[test]
    fn bad_input_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let k = keys(dir.path());
        assert!(
            k.add(AddParams {
                provider: "../etc".into(),
                secret: "sk-1234567890".into(),
                ..Default::default()
            })
            .is_err()
        );
        assert!(
            k.add(AddParams {
                provider: "openai".into(),
                secret: "short".into(),
                ..Default::default()
            })
            .is_err()
        );
        add(&k, "openai", "", "sk-1234567890");
        assert!(
            k.add(AddParams {
                provider: "openai".into(),
                secret: "sk-1234567890".into(),
                ..Default::default()
            })
            .is_err(),
            "the same key twice"
        );
    }
}
