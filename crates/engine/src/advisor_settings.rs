//! Settings → General → Advisor: the model Pi's advisor consults and up to
//! two fallbacks, tried in order when it is unavailable. They live in Pi's
//! `<agent dir>/loop.json` under `roster.advisor`, which the advisor
//! extension re-reads on every consultation, so a change reaches running
//! chats at their next one. Only `model` and `failover` are written; the
//! advisor's `thinking` and every other key in the file are kept. The advisor
//! is a child an agent launches, so a reserved model (Claude Fable, GPT
//! Astra) cannot be saved as its model or a fallback.
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use zeron_proto::reserved_models;

pub const FILE: &str = "loop.json";
pub const MAX_FALLBACKS: usize = 2;
/// The advisor Pi runs when `loop.json` names none.
pub const DEFAULT_MODEL: &str = "anthropic/claude-opus-5";
/// Pi's own fallbacks for [`DEFAULT_MODEL`], kept while the file names that
/// model without a `failover` of its own.
const DEFAULT_FAILOVER: &[&str] = &["opencode/claude-opus-5"];

/// Serialises the engine's writers.
static WRITE_LOCK: Mutex<()> = Mutex::new(());

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct AdvisorSettings {
    /// A `provider/id` spec; None is Pi's default roster.
    pub model: Option<String>,
    /// Tried in order when the model is unavailable; none without a model.
    pub fallbacks: Vec<String>,
}

impl AdvisorSettings {
    /// Checked and tidied: a well-formed model, and at most two fallbacks
    /// that repeat neither it nor each other.
    fn normalized(self) -> Result<Self, String> {
        let Some(model) = self
            .model
            .as_deref()
            .map(str::trim)
            .filter(|m| !m.is_empty())
        else {
            return Ok(Self::default());
        };
        let model = spec(model).ok_or_else(|| not_a_model(model))?;
        let mut fallbacks: Vec<String> = Vec::new();
        for fallback in &self.fallbacks {
            let fallback = fallback.trim();
            if fallback.is_empty() {
                continue;
            }
            let fallback = spec(fallback).ok_or_else(|| not_a_model(fallback))?;
            if fallback == model || fallbacks.contains(&fallback) {
                continue;
            }
            if fallbacks.len() == MAX_FALLBACKS {
                break;
            }
            fallbacks.push(fallback);
        }
        Ok(Self {
            model: Some(model),
            fallbacks,
        })
    }
}

fn not_a_model(text: &str) -> String {
    format!("{text} is not a Pi model (provider/id)")
}

/// A Pi model id from Zeron's catalog as a `loop.json` spec. Zeron already
/// lists Pi's models as `provider/id` (`openrouter/x-ai/grok-4` keeps its
/// inner slash), the shape the roster takes, so this only trims the id and
/// checks it as Pi does: a provider, a slash, an id, no spaces.
pub fn spec(model_id: &str) -> Option<String> {
    let id = model_id.trim();
    let (provider, rest) = id.split_once('/')?;
    (!provider.is_empty() && !rest.is_empty() && !id.chars().any(char::is_whitespace))
        .then(|| id.to_owned())
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

/// What Pi's advisor runs on, as the file sets it.
fn from_root(root: &Map<String, Value>) -> AdvisorSettings {
    let advisor = root.get("roster").and_then(|roster| roster.get("advisor"));
    // Pi ignores an entry without a well-formed model.
    let Some(model) = advisor
        .and_then(|advisor| advisor.get("model"))
        .and_then(Value::as_str)
        .and_then(spec)
    else {
        return AdvisorSettings::default();
    };
    let fallbacks = match advisor
        .and_then(|advisor| advisor.get("failover"))
        .filter(|failover| !failover.is_null())
    {
        Some(failover) => failover
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .filter_map(spec)
            .collect(),
        // Pi keeps its own fallbacks for its own model only.
        None if model == DEFAULT_MODEL => {
            DEFAULT_FAILOVER.iter().map(|s| (*s).to_owned()).collect()
        }
        None => Vec::new(),
    };
    AdvisorSettings {
        model: Some(model),
        fallbacks,
    }
    .normalized()
    .unwrap_or_default()
}

/// The advisor's settings; Pi's default when `loop.json` is missing or
/// cannot be read.
pub fn read(agent_dir: &Path) -> AdvisorSettings {
    match load(&path(agent_dir)) {
        Ok(root) => root.as_ref().map(from_root).unwrap_or_default(),
        Err(error) => {
            tracing::warn!(%error, "advisor settings: reading loop.json failed");
            AdvisorSettings::default()
        }
    }
}

/// Why `settings` cannot be saved: the advisor is a child an agent launches,
/// so neither its model nor a fallback may be a reserved one (Claude Fable,
/// GPT Astra; see [`reserved_models`]). A `loop.json` already naming one is
/// still read as it is, so the settings can show it and let it be replaced.
fn refuse_reserved(settings: &AdvisorSettings) -> Result<(), String> {
    for model in settings.model.iter().chain(&settings.fallbacks) {
        if reserved_models::reserved_family(model).is_some() {
            return Err(reserved_models::refusal(model, "The advisor"));
        }
    }
    Ok(())
}

/// Save `settings` into `loop.json`, creating it when needed, and return
/// what the file now sets. A file that is not a JSON object is refused,
/// never overwritten, and so is a reserved model or fallback.
pub fn write(agent_dir: &Path, settings: AdvisorSettings) -> Result<AdvisorSettings, String> {
    let settings = settings.normalized()?;
    refuse_reserved(&settings)?;
    let _lock = WRITE_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
    let path = path(agent_dir);
    let mut root = load(&path)?.unwrap_or_default();
    let before = root.clone();
    apply(&mut root, &settings)?;
    if root != before {
        save(&path, &root)?;
    }
    Ok(from_root(&root))
}

/// Set or clear `roster.advisor.model` and `.failover`, dropping the
/// `advisor` and `roster` objects when nothing is left in them.
fn apply(root: &mut Map<String, Value>, settings: &AdvisorSettings) -> Result<(), String> {
    let Value::Object(roster) = root
        .entry("roster")
        .or_insert_with(|| Value::Object(Map::new()))
    else {
        return Err(format!("roster in {FILE} is not an object"));
    };
    let Value::Object(advisor) = roster
        .entry("advisor")
        .or_insert_with(|| Value::Object(Map::new()))
    else {
        return Err(format!("roster.advisor in {FILE} is not an object"));
    };
    match &settings.model {
        Some(model) => {
            advisor.insert("model".into(), model.clone().into());
            advisor.insert("failover".into(), settings.fallbacks.clone().into());
        }
        None => {
            advisor.remove("model");
            advisor.remove("failover");
        }
    }
    if advisor.is_empty() {
        roster.remove("advisor");
    }
    if roster.is_empty() {
        root.remove("roster");
    }
    Ok(())
}

/// Pretty JSON with a trailing newline, renamed into place so the advisor
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

    fn settings(model: Option<&str>, fallbacks: &[&str]) -> AdvisorSettings {
        AdvisorSettings {
            model: model.map(str::to_owned),
            fallbacks: fallbacks.iter().map(|s| (*s).to_owned()).collect(),
        }
    }

    fn file_json(dir: &Path) -> Value {
        serde_json::from_str(&std::fs::read_to_string(path(dir)).unwrap()).unwrap()
    }

    #[test]
    fn a_catalog_id_is_already_a_spec() {
        assert_eq!(
            spec(" anthropic/claude-opus-5 ").as_deref(),
            Some("anthropic/claude-opus-5")
        );
        assert_eq!(
            spec("openrouter/x-ai/grok-4").as_deref(),
            Some("openrouter/x-ai/grok-4")
        );
        assert_eq!(
            spec("ollama/llama3.1:8b").as_deref(),
            Some("ollama/llama3.1:8b")
        );
        for bad in ["default", "", "/id", "provider/", "a b/c"] {
            assert_eq!(spec(bad), None, "{bad}");
        }
    }

    #[test]
    fn a_missing_file_is_pis_default_and_a_save_creates_it() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(read(dir.path()), AdvisorSettings::default());
        // Saving the default writes nothing.
        write(dir.path(), AdvisorSettings::default()).unwrap();
        assert!(!path(dir.path()).exists());
        let saved = write(
            dir.path(),
            settings(Some("openai/gpt-5.5"), &["anthropic/claude-opus-5"]),
        )
        .unwrap();
        assert_eq!(
            saved,
            settings(Some("openai/gpt-5.5"), &["anthropic/claude-opus-5"])
        );
        let text = std::fs::read_to_string(path(dir.path())).unwrap();
        assert!(text.ends_with("}\n"));
        assert_eq!(read(dir.path()), saved);
    }

    #[test]
    fn a_save_keeps_thinking_and_every_other_key() {
        let dir = tempfile::tempdir().unwrap();
        let original = serde_json::json!({
            "maxRounds": 7,
            "roleTools": { "advisor": ["read", "grep"] },
            "roster": {
                "implementer": { "model": "opencode-go/deepseek-v4-flash", "thinking": "max" },
                "advisor": { "model": "anthropic/claude-opus-5", "thinking": "high" }
            }
        });
        std::fs::write(path(dir.path()), original.to_string()).unwrap();
        // The default model without a failover keeps Pi's own.
        assert_eq!(
            read(dir.path()),
            settings(Some(DEFAULT_MODEL), DEFAULT_FAILOVER)
        );
        write(
            dir.path(),
            settings(Some("openai/gpt-5.5"), &["xai/grok-4"]),
        )
        .unwrap();
        let mut expected = original.clone();
        expected["roster"]["advisor"] = serde_json::json!({
            "model": "openai/gpt-5.5",
            "thinking": "high",
            "failover": ["xai/grok-4"]
        });
        assert_eq!(file_json(dir.path()), expected);
        // Clearing leaves the thinking level and the rest.
        assert_eq!(
            write(dir.path(), settings(None, &["xai/grok-4"])).unwrap(),
            AdvisorSettings::default()
        );
        expected["roster"]["advisor"] = serde_json::json!({ "thinking": "high" });
        assert_eq!(file_json(dir.path()), expected);
    }

    #[test]
    fn clearing_drops_empty_objects() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(path(dir.path()), r#"{"maxRounds": 3}"#).unwrap();
        write(dir.path(), settings(Some("openai/gpt-5.5"), &[])).unwrap();
        assert_eq!(
            file_json(dir.path())["roster"]["advisor"],
            serde_json::json!({ "model": "openai/gpt-5.5", "failover": [] })
        );
        write(dir.path(), AdvisorSettings::default()).unwrap();
        assert_eq!(file_json(dir.path()), serde_json::json!({ "maxRounds": 3 }));
    }

    #[test]
    fn fallbacks_are_two_at_most_without_repeats() {
        let dir = tempfile::tempdir().unwrap();
        let saved = write(
            dir.path(),
            settings(
                Some("openai/gpt-5.5"),
                &[
                    " ",
                    "openai/gpt-5.5",
                    "xai/grok-4",
                    "xai/grok-4",
                    "google/gemini-3",
                    "groq/kimi",
                ],
            ),
        )
        .unwrap();
        assert_eq!(
            saved,
            settings(Some("openai/gpt-5.5"), &["xai/grok-4", "google/gemini-3"])
        );
        // No model, no fallbacks.
        assert_eq!(
            AdvisorSettings::normalized(settings(Some(" "), &["xai/grok-4"])).unwrap(),
            AdvisorSettings::default()
        );
        assert!(write(dir.path(), settings(Some("gpt-5.5"), &[])).is_err());
        assert!(write(dir.path(), settings(Some("openai/gpt-5.5"), &["grok"])).is_err());
    }

    #[test]
    fn a_reserved_model_or_fallback_is_refused_and_nothing_is_written() {
        let dir = tempfile::tempdir().unwrap();
        for (model, fallbacks) in [
            ("anthropic/claude-fable-5", &[][..]),
            ("openai/gpt-6-astra-mini", &[][..]),
            ("openai/gpt-5.5", &["opencode/claude-fable-5"][..]),
            ("openai/gpt-5.5", &["xai/grok-4", "openai/astra-2"][..]),
        ] {
            let error = write(dir.path(), settings(Some(model), fallbacks)).unwrap_err();
            assert!(error.starts_with("The advisor cannot start on "), "{error}");
            assert!(error.contains("reserved"), "{error}");
            assert!(!path(dir.path()).exists());
        }
        // Lookalikes are fine.
        write(dir.path(), settings(Some("local/astral-7b"), &[])).unwrap();
    }

    #[test]
    fn a_reserved_model_already_in_the_file_is_reported_as_it_is() {
        let dir = tempfile::tempdir().unwrap();
        let text = r#"{"roster":{"advisor":{"model":"anthropic/claude-fable-5","failover":["opencode/claude-fable-5"]}}}"#;
        std::fs::write(path(dir.path()), text).unwrap();
        assert_eq!(
            read(dir.path()),
            settings(
                Some("anthropic/claude-fable-5"),
                &["opencode/claude-fable-5"]
            )
        );
        // Saving it back is refused; clearing it is not.
        let kept = read(dir.path());
        assert!(write(dir.path(), kept).is_err());
        assert_eq!(std::fs::read_to_string(path(dir.path())).unwrap(), text);
        write(dir.path(), AdvisorSettings::default()).unwrap();
        assert_eq!(read(dir.path()), AdvisorSettings::default());
    }

    #[test]
    fn a_file_that_is_not_json_is_never_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        for text in ["{ \"roster\": ", "[1, 2]"] {
            std::fs::write(path(dir.path()), text).unwrap();
            assert_eq!(read(dir.path()), AdvisorSettings::default());
            let error = write(dir.path(), settings(Some("openai/gpt-5.5"), &[])).unwrap_err();
            assert!(error.contains(FILE), "{error}");
            assert_eq!(std::fs::read_to_string(path(dir.path())).unwrap(), text);
        }
        std::fs::write(path(dir.path()), r#"{"roster": 5}"#).unwrap();
        assert!(write(dir.path(), settings(Some("openai/gpt-5.5"), &[])).is_err());
        assert_eq!(
            std::fs::read_to_string(path(dir.path())).unwrap(),
            r#"{"roster": 5}"#
        );
    }
}
