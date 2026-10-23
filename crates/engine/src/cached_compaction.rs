//! Settings → General → Cached compaction: the cached-compaction extension's
//! own switches — whether a compaction replays the last request through the
//! prompt cache instead of resending the transcript, the context floor and
//! lead time of the idle compact-before-expiry timer, the cache retention the
//! session asks for, and whether the cost card and the footer countdown are
//! drawn. They live in Pi's `<agent dir>/cached-compaction.json`, which the
//! extension re-reads at every compaction (and at most every two seconds
//! otherwise), so a change reaches running chats without a restart. Every key
//! this module does not own — the extension's `log`, `strictPrefix`, and any
//! added later — is kept as it is. The idle timer's own switch (`preExpiry`)
//! rides with `compactBeforeExpiry`, exactly as the extension's
//! `/cached-compaction expiry on|off` writes both: a timer armed by one and
//! disarmed by the other would read as on and do nothing. Pi's
//! `settings.json` is never touched: Pi and several extensions write it, and a
//! second writer turns one toggle into a lost update.
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

pub const FILE: &str = "cached-compaction.json";
/// The prompt-cache retention the session asks for; `auto` picks per session
/// kind (long for a chat a human is watching, short for a child or a
/// headless run).
pub const RETENTIONS: [&str; 3] = ["auto", "short", "long"];
/// The context the idle pre-expiry timer never compacts below: a summary of a
/// shorter conversation costs more than it saves.
pub const MIN_CONTEXT_TOKENS: u64 = 1_000;
/// The shortest lead the timer accepts, so a reclaimed cache read is not
/// swallowed by the model call's own latency.
pub const MIN_LEAD_MS: u64 = 5_000;
/// The longest prompt-cache lifetime any provider reports (Anthropic's 1h
/// tier): a lead longer than the cache itself would fire immediately.
pub const MAX_LEAD_MS: u64 = 3_600_000;

/// Serialises the engine's writers.
static WRITE_LOCK: Mutex<()> = Mutex::new(());

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct CachedCompactionSettings {
    /// The master switch: false always compacts the way Pi does.
    pub enabled: bool,
    /// Compact while the chat is idle, shortly before the prompt cache
    /// expires, instead of waiting for the threshold.
    pub compact_before_expiry: bool,
    /// The idle pre-expiry timer never fires below this context size.
    pub min_context_tokens: u64,
    /// How long before the cache expires that timer fires.
    pub lead_ms: u64,
    /// One of [`RETENTIONS`].
    pub cache_retention: String,
    /// Append Pi's cached-vs-native cost comparison to the session.
    pub cost_card: bool,
    /// Publish the cache countdown Zeron's footer and session panel draw.
    pub footer_countdown: bool,
}

impl Default for CachedCompactionSettings {
    /// The defaults the extension itself uses, so a missing file and an unset
    /// card agree.
    fn default() -> Self {
        Self {
            enabled: true,
            compact_before_expiry: true,
            min_context_tokens: 50_000,
            lead_ms: 20_000,
            cache_retention: "auto".into(),
            cost_card: true,
            footer_countdown: true,
        }
    }
}

impl CachedCompactionSettings {
    /// Checked and tidied: a context floor no lower than [`MIN_CONTEXT_TOKENS`]
    /// and a lead inside [`MIN_LEAD_MS`]..=[`MAX_LEAD_MS`]; an unknown
    /// retention is refused rather than guessed at.
    fn normalized(mut self) -> Result<Self, String> {
        self.min_context_tokens = self.min_context_tokens.max(MIN_CONTEXT_TOKENS);
        self.lead_ms = self.lead_ms.clamp(MIN_LEAD_MS, MAX_LEAD_MS);
        self.cache_retention = checked_retention(&self.cache_retention)?;
        Ok(self)
    }
}

fn checked_retention(retention: &str) -> Result<String, String> {
    let retention = retention.trim();
    RETENTIONS
        .iter()
        .find(|known| **known == retention)
        .map(|known| (*known).to_owned())
        .ok_or_else(|| {
            format!(
                "{retention} is not a cache retention ({})",
                RETENTIONS.join(", ")
            )
        })
}

pub fn path(agent_dir: &Path) -> PathBuf {
    agent_dir.join(FILE)
}

/// The file's top-level object; None when there is no file yet.
fn load(path: &Path) -> Result<Option<Map<String, Value>>, String> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("{FILE} could not be read: {error}")),
    };
    if text.trim().is_empty() {
        return Ok(None);
    }
    match serde_json::from_str::<Value>(&text) {
        Ok(Value::Object(root)) => Ok(Some(root)),
        Ok(_) => Err(format!(
            "{FILE} is not a JSON object; fix or remove it first"
        )),
        Err(error) => Err(format!(
            "{FILE} is not valid JSON ({error}); fix or remove it first"
        )),
    }
}

/// What the file sets, as the extension reads it: a key that is missing or of
/// the wrong type is its default, and an unknown retention is `auto` (the
/// extension reports the mistake itself, and falls back the same way).
///
/// The limits are not applied here: the card shows what the file says, and a
/// save is what tidies it.
fn from_root(root: &Map<String, Value>) -> CachedCompactionSettings {
    let default = CachedCompactionSettings::default();
    let flag = |key: &str, fallback: bool| {
        root.get(key)
            .and_then(Value::as_bool)
            .unwrap_or(fallback)
    };
    let count = |key: &str, fallback: u64| {
        root.get(key).and_then(Value::as_u64).unwrap_or(fallback)
    };
    CachedCompactionSettings {
        enabled: flag("enabled", default.enabled),
        compact_before_expiry: flag("compactBeforeExpiry", default.compact_before_expiry),
        min_context_tokens: count("minContextTokens", default.min_context_tokens),
        lead_ms: count("leadMs", default.lead_ms),
        cache_retention: root
            .get("cacheRetention")
            .and_then(Value::as_str)
            .map(str::trim)
            .and_then(|retention| checked_retention(retention).ok())
            .unwrap_or(default.cache_retention),
        cost_card: flag("costCard", default.cost_card),
        footer_countdown: flag("footerCountdown", default.footer_countdown),
    }
}

/// The cached-compaction settings; the extension's defaults when the file is
/// missing or cannot be read.
pub fn read(agent_dir: &Path) -> CachedCompactionSettings {
    match load(&path(agent_dir)) {
        Ok(root) => root
            .as_ref()
            .map(from_root)
            .unwrap_or_else(CachedCompactionSettings::default),
        Err(error) => {
            tracing::warn!(%error, "cached compaction: reading cached-compaction.json failed");
            CachedCompactionSettings::default()
        }
    }
}

/// Save `settings` into `cached-compaction.json`, creating it when needed, and
/// return what the file now sets. The values are clamped to what the timer can
/// use, and an unknown retention is refused. A file that is not a JSON object
/// is refused, never overwritten; every key this module does not own is kept.
pub fn write(
    agent_dir: &Path,
    settings: CachedCompactionSettings,
) -> Result<CachedCompactionSettings, String> {
    let settings = settings.normalized()?;
    let _lock = WRITE_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
    let path = path(agent_dir);
    let mut root = load(&path)?.unwrap_or_default();
    let before = root.clone();
    apply(&mut root, &settings);
    if root != before {
        save(&path, &root)?;
    }
    Ok(from_root(&root))
}

/// Set the keys this card owns. The idle timer's `preExpiry` rides with
/// `compactBeforeExpiry`, as the extension's own `/cached-compaction expiry`
/// writes both. Anything else in the file — `log`, `strictPrefix`, a key a
/// later release adds — is left exactly as it is.
fn apply(root: &mut Map<String, Value>, settings: &CachedCompactionSettings) {
    for (key, value) in [
        ("enabled", Value::Bool(settings.enabled)),
        (
            "compactBeforeExpiry",
            Value::Bool(settings.compact_before_expiry),
        ),
        ("preExpiry", Value::Bool(settings.compact_before_expiry)),
        (
            "minContextTokens",
            Value::from(settings.min_context_tokens),
        ),
        ("leadMs", Value::from(settings.lead_ms)),
        (
            "cacheRetention",
            Value::String(settings.cache_retention.clone()),
        ),
        ("costCard", Value::Bool(settings.cost_card)),
        ("footerCountdown", Value::Bool(settings.footer_countdown)),
    ] {
        root.insert(key.into(), value);
    }
}

/// Pretty JSON with a trailing newline, renamed into place so the extension
/// never reads half a file. Windows will not replace a file another process
/// holds open: write it in place then.
fn save(path: &Path, root: &Map<String, Value>) -> Result<(), String> {
    let mut text = serde_json::to_string_pretty(root).map_err(|e| e.to_string())?;
    text.push('\n');
    let failed = |error: std::io::Error| format!("{FILE} could not be saved: {error}");
    let dir = path
        .parent()
        .ok_or_else(|| format!("{} has no parent", path.display()))?;
    std::fs::create_dir_all(dir).map_err(failed)?;
    let mut tmp = tempfile::NamedTempFile::new_in(dir).map_err(failed)?;
    std::io::Write::write_all(&mut tmp, text.as_bytes()).map_err(failed)?;
    tmp.as_file().sync_all().map_err(failed)?;
    if tmp.persist(path).is_err() {
        std::fs::write(path, &text).map_err(failed)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file_json(dir: &Path) -> Value {
        serde_json::from_str(&std::fs::read_to_string(path(dir)).unwrap()).unwrap()
    }

    fn settings(enabled: bool, min_context_tokens: u64, lead_ms: u64) -> CachedCompactionSettings {
        CachedCompactionSettings {
            enabled,
            min_context_tokens,
            lead_ms,
            ..CachedCompactionSettings::default()
        }
    }

    #[test]
    fn a_missing_file_is_the_extensions_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let defaults = CachedCompactionSettings::default();
        assert_eq!(read(dir.path()), defaults);
        assert!(defaults.enabled, "cached compaction is on by default");
        assert!(defaults.compact_before_expiry);
        assert_eq!(defaults.min_context_tokens, 50_000);
        assert_eq!(defaults.lead_ms, 20_000);
        assert_eq!(defaults.cache_retention, "auto");
        assert!(defaults.cost_card);
        assert!(defaults.footer_countdown);
        // Nothing was created by reading.
        assert!(!path(dir.path()).exists());
    }

    #[test]
    fn a_save_round_trips_and_creates_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let wanted = CachedCompactionSettings {
            enabled: false,
            compact_before_expiry: false,
            min_context_tokens: 120_000,
            lead_ms: 45_000,
            cache_retention: "long".into(),
            cost_card: false,
            footer_countdown: false,
        };
        assert_eq!(write(dir.path(), wanted.clone()).unwrap(), wanted);
        assert_eq!(read(dir.path()), wanted);
        assert_eq!(
            file_json(dir.path()),
            serde_json::json!({
                "enabled": false,
                "compactBeforeExpiry": false,
                "preExpiry": false,
                "minContextTokens": 120_000,
                "leadMs": 45_000,
                "cacheRetention": "long",
                "costCard": false,
                "footerCountdown": false
            })
        );
        assert!(
            std::fs::read_to_string(path(dir.path()))
                .unwrap()
                .ends_with("}\n")
        );
        // The file is camelCase, which is what the extension reads.
        let text = std::fs::read_to_string(path(dir.path())).unwrap();
        assert!(text.contains("\"minContextTokens\""));
        assert!(!text.contains("min_context_tokens"));
    }

    #[test]
    fn a_save_keeps_every_key_it_does_not_own() {
        let dir = tempfile::tempdir().unwrap();
        let original = serde_json::json!({
            "enabled": true,
            "compactBeforeExpiry": true,
            "preExpiry": true,
            "log": false,
            "strictPrefix": true,
            "somethingNewer": { "nested": [1, 2] }
        });
        std::fs::write(path(dir.path()), original.to_string()).unwrap();
        // Read sees the switches it owns and ignores the rest.
        assert!(read(dir.path()).enabled);
        write(dir.path(), settings(false, 80_000, 10_000)).unwrap();
        let saved = file_json(dir.path());
        assert_eq!(saved["enabled"], serde_json::json!(false));
        assert_eq!(saved["log"], serde_json::json!(false), "kept");
        assert_eq!(saved["strictPrefix"], serde_json::json!(true), "kept");
        assert_eq!(saved["somethingNewer"], original["somethingNewer"], "kept");
        assert_eq!(saved["minContextTokens"], serde_json::json!(80_000));
        assert_eq!(saved["leadMs"], serde_json::json!(10_000));
        // The idle timer's own switch rides with the one on the card, as the
        // extension's `/cached-compaction expiry` writes both.
        let mut off = settings(true, 80_000, 10_000);
        off.compact_before_expiry = false;
        write(dir.path(), off).unwrap();
        let saved = file_json(dir.path());
        assert_eq!(saved["compactBeforeExpiry"], serde_json::json!(false));
        assert_eq!(saved["preExpiry"], serde_json::json!(false));
        write(dir.path(), settings(true, 80_000, 10_000)).unwrap();
        let saved = file_json(dir.path());
        assert_eq!(saved["preExpiry"], serde_json::json!(true));
    }

    #[test]
    fn out_of_range_limits_are_clamped_on_save() {
        let dir = tempfile::tempdir().unwrap();
        let saved = write(dir.path(), settings(true, 0, 0)).unwrap();
        assert_eq!(saved.min_context_tokens, MIN_CONTEXT_TOKENS);
        assert_eq!(saved.lead_ms, MIN_LEAD_MS);
        assert_eq!(read(dir.path()), saved, "the file holds the clamped values");
        let saved = write(dir.path(), settings(true, 500, 10 * MAX_LEAD_MS)).unwrap();
        assert_eq!(saved.min_context_tokens, MIN_CONTEXT_TOKENS);
        assert_eq!(saved.lead_ms, MAX_LEAD_MS);
        // A value already inside the range is left alone.
        let saved = write(dir.path(), settings(true, 90_000, 30_000)).unwrap();
        assert_eq!((saved.min_context_tokens, saved.lead_ms), (90_000, 30_000));
    }

    #[test]
    fn an_unknown_retention_is_refused_and_the_file_is_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let wanted = CachedCompactionSettings {
            cache_retention: "forever".into(),
            ..CachedCompactionSettings::default()
        };
        let error = write(dir.path(), wanted).unwrap_err();
        assert!(error.contains("not a cache retention"), "{error}");
        assert!(error.contains("auto, short, long"), "{error}");
        assert!(!path(dir.path()).exists(), "nothing was written");
        // A file already holding one reads as `auto`, the extension's fallback.
        std::fs::write(
            path(dir.path()),
            serde_json::json!({ "cacheRetention": "forever", "enabled": false })
                .to_string(),
        )
        .unwrap();
        let read_back = read(dir.path());
        assert_eq!(read_back.cache_retention, "auto");
        assert!(!read_back.enabled, "the other keys still read");
        // Every known one is accepted, around whitespace.
        for retention in RETENTIONS {
            let saved = write(
                dir.path(),
                CachedCompactionSettings {
                    cache_retention: format!(" {retention} "),
                    ..CachedCompactionSettings::default()
                },
            )
            .unwrap();
            assert_eq!(saved.cache_retention, retention);
        }
    }

    /// The card can save twice in a row (a toggle, then a number); the second
    /// write must see the first, and no read may ever see half a file.
    #[test]
    fn concurrent_writes_serialise_and_never_leave_a_partial_file() {
        let dir = tempfile::tempdir().unwrap();
        let writes: Vec<CachedCompactionSettings> = (0..8)
            .map(|n| CachedCompactionSettings {
                enabled: n % 2 == 0,
                min_context_tokens: 10_000 + n * 1_000,
                lead_ms: 5_000 + n * 1_000,
                cache_retention: RETENTIONS[n as usize % RETENTIONS.len()].to_owned(),
                ..CachedCompactionSettings::default()
            })
            .collect();
        std::thread::scope(|scope| {
            for settings in writes.clone() {
                scope.spawn(|| write(dir.path(), settings).expect("a save"));
            }
            // A reader alongside the writers only ever sees whole files.
            scope.spawn(|| {
                for _ in 0..8 {
                    let text = std::fs::read_to_string(path(dir.path())).unwrap_or_default();
                    if text.is_empty() {
                        continue;
                    }
                    serde_json::from_str::<Value>(&text).expect("a whole file, never a partial");
                }
            });
        });
        let text = std::fs::read_to_string(path(dir.path())).unwrap();
        assert!(text.ends_with("}\n"));
        let read_back = read(dir.path());
        assert!(
            writes.contains(&read_back),
            "one writer's whole settings, not a mix: {read_back:?}"
        );
    }

    #[test]
    fn a_file_that_is_not_an_object_is_refused_and_kept() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(path(dir.path()), "[1, 2, 3]").unwrap();
        let error = write(dir.path(), CachedCompactionSettings::default()).unwrap_err();
        assert!(error.contains("not a JSON object"), "{error}");
        assert_eq!(std::fs::read_to_string(path(dir.path())).unwrap(), "[1, 2, 3]");
        // Reading it is the defaults, with a warning, never a failure.
        assert_eq!(read(dir.path()), CachedCompactionSettings::default());
    }

    #[test]
    fn a_wrongly_typed_key_reads_as_its_default() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            path(dir.path()),
            serde_json::json!({
                "enabled": "yes",
                "minContextTokens": -5,
                "leadMs": 12.5,
                "costCard": null
            })
            .to_string(),
        )
        .unwrap();
        let read_back = read(dir.path());
        assert_eq!(read_back, CachedCompactionSettings::default());
    }
}
