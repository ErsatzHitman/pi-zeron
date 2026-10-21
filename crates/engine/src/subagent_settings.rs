//! Settings → General → Default subagent: the agent (a Pi model and its
//! thinking level) every child an agent starts runs on unless the call names
//! its own: `subagent_start` tasks, dynamic-workflow agents and delegated
//! sessions, in every session (a plain chat, an orchestrator's root, its
//! members and their sub-sessions). They live in Pi's
//! `<agent dir>/subagents.json` as `defaultModel` and `defaultThinkingLevel`,
//! which those extensions re-read at each launch, so a change reaches running
//! chats at their next child. A value the user clears is written as an explicit
//! `null`, never left out: `subagent_start` reads a missing key as Pi's own
//! built-in default (deepseek-flash at max thinking), while `null` is what makes
//! children run on their session's own model and thinking level. Up to two
//! fallbacks (`fallbackModels`, each a model and an optional level) are tried in
//! order when the default cannot run a child: a model on an almost used-up
//! account, or a child that fails before it does any work. Every other key in
//! the file is kept. A child cannot run a reserved model (Claude Fable, GPT
//! Astra), so neither can the default or a fallback.
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use zeron_proto::reserved_models;

use crate::advisor_settings::spec;

pub const FILE: &str = "subagents.json";
/// Pi's thinking levels, least first.
pub const THINKING_LEVELS: [&str; 7] = ["off", "minimal", "low", "medium", "high", "xhigh", "max"];
/// How many fallbacks the default may have.
pub const MAX_FALLBACKS: usize = 2;

/// Serialises the engine's writers.
static WRITE_LOCK: Mutex<()> = Mutex::new(());

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct SubagentSettings {
    /// A `provider/id` spec; None is each child's own default (its parent
    /// session's model).
    pub model: Option<String>,
    /// One of [`THINKING_LEVELS`]; None is the delegating session's own level.
    pub thinking: Option<String>,
    /// Tried in order when `model` cannot run a child; at most
    /// [`MAX_FALLBACKS`], none repeating the default or each other.
    pub fallbacks: Vec<SubagentFallback>,
}

/// One fallback: a `provider/id` and the level it runs at, None for the
/// default's own.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct SubagentFallback {
    pub model: String,
    pub thinking: Option<String>,
}

impl SubagentSettings {
    /// Checked and tidied: a well-formed model and a known level, and
    /// fallbacks that are well formed, each once, and no more than allowed.
    fn normalized(self) -> Result<Self, String> {
        let model = match self
            .model
            .as_deref()
            .map(str::trim)
            .filter(|m| !m.is_empty())
        {
            Some(model) => Some(checked_model(model)?),
            None => None,
        };
        let thinking = checked_level(self.thinking.as_deref())?;
        let mut fallbacks: Vec<SubagentFallback> = Vec::new();
        for fallback in self.fallbacks {
            let named = fallback.model.trim();
            if named.is_empty() {
                continue;
            }
            let fallback = SubagentFallback {
                model: checked_model(named)?,
                thinking: checked_level(fallback.thinking.as_deref())?,
            };
            // Falling back to the default, or to an earlier fallback, would
            // only run the model that just failed again.
            let repeats = model.as_deref() == Some(fallback.model.as_str())
                || fallbacks.iter().any(|f| f.model == fallback.model);
            if !repeats {
                fallbacks.push(fallback);
            }
        }
        if fallbacks.len() > MAX_FALLBACKS {
            return Err(format!(
                "A default subagent has at most {MAX_FALLBACKS} fallbacks"
            ));
        }
        Ok(Self {
            model,
            thinking,
            fallbacks,
        })
    }

    /// The default, then each fallback.
    fn models(&self) -> impl Iterator<Item = &str> {
        self.model
            .as_deref()
            .into_iter()
            .chain(self.fallbacks.iter().map(|f| f.model.as_str()))
    }
}

fn checked_model(model: &str) -> Result<String, String> {
    spec(model).ok_or_else(|| format!("{model} is not a Pi model (provider/id)"))
}

fn checked_level(level: Option<&str>) -> Result<Option<String>, String> {
    match level.map(str::trim).filter(|t| !t.is_empty()) {
        Some(level) => level_of(level).map(Some).ok_or_else(|| {
            format!(
                "{level} is not a thinking level ({})",
                THINKING_LEVELS.join(", ")
            )
        }),
        None => Ok(None),
    }
}

fn level_of(text: &str) -> Option<String> {
    THINKING_LEVELS
        .iter()
        .find(|level| **level == text)
        .map(|level| (*level).to_owned())
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

/// What the file sets, as Pi reads it: an ill-formed value is no value, and
/// so is a fallback that repeats the default or an earlier one, or comes
/// after the first [`MAX_FALLBACKS`].
fn from_root(root: &Map<String, Value>) -> SubagentSettings {
    let model = root
        .get("defaultModel")
        .and_then(Value::as_str)
        .and_then(spec);
    let mut fallbacks: Vec<SubagentFallback> = Vec::new();
    for entry in root
        .get("fallbackModels")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let (named, level) = match entry {
            Value::String(named) => (Some(named.as_str()), None),
            Value::Object(fields) => (
                fields.get("model").and_then(Value::as_str),
                fields.get("thinkingLevel").and_then(Value::as_str),
            ),
            _ => (None, None),
        };
        let Some(fallback_model) = named.and_then(spec) else {
            continue;
        };
        if model.as_deref() == Some(fallback_model.as_str())
            || fallbacks.iter().any(|f| f.model == fallback_model)
        {
            continue;
        }
        fallbacks.push(SubagentFallback {
            model: fallback_model,
            thinking: level.and_then(|level| level_of(level.trim())),
        });
        if fallbacks.len() == MAX_FALLBACKS {
            break;
        }
    }
    SubagentSettings {
        model,
        thinking: root
            .get("defaultThinkingLevel")
            .and_then(Value::as_str)
            .and_then(|level| level_of(level.trim())),
        fallbacks,
    }
}

/// The default agent; none when `subagents.json` is missing or unreadable.
pub fn read(agent_dir: &Path) -> SubagentSettings {
    match load(&path(agent_dir)) {
        Ok(root) => root.as_ref().map(from_root).unwrap_or_default(),
        Err(error) => {
            tracing::warn!(%error, "subagent settings: reading subagents.json failed");
            SubagentSettings::default()
        }
    }
}

/// Save `settings`, creating the file when needed, and return what it now
/// sets. A file that is not a JSON object is refused, never overwritten, and
/// so is a reserved model, as the default or as a fallback.
pub fn write(agent_dir: &Path, settings: SubagentSettings) -> Result<SubagentSettings, String> {
    let settings = settings.normalized()?;
    if let Some(model) = settings
        .models()
        .find(|model| reserved_models::reserved_family(model).is_some())
    {
        return Err(reserved_models::refusal(model, "A subagent"));
    }
    let _lock = WRITE_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
    let path = path(agent_dir);
    let mut root = load(&path)?.unwrap_or_default();
    let before = root.clone();
    // A cleared value is written as `null`, not removed: Pi's built-in default
    // applies to a MISSING key, so removing it would put a reset child back on
    // deepseek-flash at max thinking instead of its session's own model.
    for (key, value) in [
        ("defaultModel", &settings.model),
        ("defaultThinkingLevel", &settings.thinking),
    ] {
        root.insert(
            key.into(),
            value.clone().map(Value::String).unwrap_or(Value::Null),
        );
    }
    if settings.fallbacks.is_empty() {
        root.remove("fallbackModels");
    } else {
        let fallbacks = settings
            .fallbacks
            .iter()
            .map(|fallback| {
                let mut entry = Map::new();
                entry.insert("model".into(), fallback.model.clone().into());
                if let Some(level) = &fallback.thinking {
                    entry.insert("thinkingLevel".into(), level.clone().into());
                }
                Value::Object(entry)
            })
            .collect();
        root.insert("fallbackModels".into(), Value::Array(fallbacks));
    }
    if root != before {
        save(&path, &root)?;
    }
    Ok(from_root(&root))
}

/// Pretty JSON with a trailing newline, renamed into place so a launching
/// child never reads half a file. Windows will not replace a file another
/// process holds open: write it in place then.
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

    fn settings(model: Option<&str>, thinking: Option<&str>) -> SubagentSettings {
        SubagentSettings {
            model: model.map(str::to_owned),
            thinking: thinking.map(str::to_owned),
            fallbacks: Vec::new(),
        }
    }

    fn fallback(model: &str, thinking: Option<&str>) -> SubagentFallback {
        SubagentFallback {
            model: model.to_owned(),
            thinking: thinking.map(str::to_owned),
        }
    }

    fn file_json(dir: &Path) -> Value {
        serde_json::from_str(&std::fs::read_to_string(path(dir)).unwrap()).unwrap()
    }

    #[test]
    fn a_missing_file_is_no_default_and_a_save_records_that_explicitly() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(read(dir.path()), SubagentSettings::default());
        // Nothing chosen is still written, as `null`: that is the difference
        // between "the user cleared it" and "no file, use Pi's own default".
        write(dir.path(), SubagentSettings::default()).unwrap();
        assert_eq!(
            file_json(dir.path()),
            serde_json::json!({ "defaultModel": null, "defaultThinkingLevel": null }),
            "cleared keys are written as null, which Pi reads as the session's own model"
        );
        assert_eq!(read(dir.path()), SubagentSettings::default());
        let saved = write(dir.path(), settings(Some(" openai/gpt-5.5 "), Some("high"))).unwrap();
        assert_eq!(saved, settings(Some("openai/gpt-5.5"), Some("high")));
        assert_eq!(read(dir.path()), saved);
        assert!(
            std::fs::read_to_string(path(dir.path()))
                .unwrap()
                .ends_with("}\n")
        );
    }

    /// The audit finding: clearing the default removed the keys, which Pi's
    /// `subagent_start` reads as its own built-in default, so a reset in Zeron
    /// left children on deepseek-flash at max thinking.
    #[test]
    fn a_cleared_default_is_written_as_null_and_reads_back_as_no_default() {
        let dir = tempfile::tempdir().unwrap();
        let mut set = settings(Some("opencode-go/deepseek-v4.1-flash"), Some("max"));
        set.fallbacks = vec![fallback("zai/glm-5", None)];
        write(dir.path(), set).unwrap();
        assert_eq!(
            write(dir.path(), SubagentSettings::default()).unwrap(),
            SubagentSettings::default()
        );
        let saved = file_json(dir.path());
        assert_eq!(saved["defaultModel"], Value::Null, "cleared, not removed");
        assert_eq!(
            saved["defaultThinkingLevel"],
            Value::Null,
            "cleared, not removed"
        );
        assert!(saved.get("fallbackModels").is_none(), "no fallback is left");
        assert_eq!(read(dir.path()), SubagentSettings::default());
    }

    #[test]
    fn a_save_keeps_every_other_key_and_clearing_writes_null() {
        let dir = tempfile::tempdir().unwrap();
        let original = serde_json::json!({
            "defaultModel": "opencode-go/deepseek-v4.1-flash",
            "defaultThinkingLevel": "max",
            "worktreeWrites": true,
            "profiles": { "scout": { "model": "x/y" } }
        });
        std::fs::write(path(dir.path()), original.to_string()).unwrap();
        assert_eq!(
            read(dir.path()),
            settings(Some("opencode-go/deepseek-v4.1-flash"), Some("max"))
        );
        write(
            dir.path(),
            settings(Some("anthropic/claude-sonnet-5"), None),
        )
        .unwrap();
        let mut expected = original.clone();
        expected["defaultModel"] = "anthropic/claude-sonnet-5".into();
        // Not removed: Pi would fall back to its own deepseek-flash at max.
        expected["defaultThinkingLevel"] = Value::Null;
        assert_eq!(file_json(dir.path()), expected);
        write(dir.path(), SubagentSettings::default()).unwrap();
        assert_eq!(
            file_json(dir.path()),
            serde_json::json!({
                "defaultModel": null,
                "defaultThinkingLevel": null,
                "worktreeWrites": true,
                "profiles": { "scout": { "model": "x/y" } }
            })
        );
    }

    #[test]
    fn two_fallbacks_are_saved_in_order_and_read_back() {
        let dir = tempfile::tempdir().unwrap();
        let mut wanted = settings(Some("opencode-go/deepseek-flash"), Some("max"));
        wanted.fallbacks = vec![
            fallback(" openai/gpt-5.5 ", Some("high")),
            fallback("", None),
            fallback("zai/glm-5", None),
        ];
        let saved = write(dir.path(), wanted).unwrap();
        assert_eq!(
            saved.fallbacks,
            [
                fallback("openai/gpt-5.5", Some("high")),
                fallback("zai/glm-5", None)
            ],
            "an empty slot is dropped"
        );
        assert_eq!(
            file_json(dir.path())["fallbackModels"],
            serde_json::json!([
                { "model": "openai/gpt-5.5", "thinkingLevel": "high" },
                { "model": "zai/glm-5" }
            ])
        );
        assert_eq!(read(dir.path()), saved);
        // Clearing them removes the key, nothing else.
        let mut cleared = saved.clone();
        cleared.fallbacks.clear();
        write(dir.path(), cleared).unwrap();
        assert!(file_json(dir.path()).get("fallbackModels").is_none());
        assert_eq!(
            file_json(dir.path())["defaultModel"],
            "opencode-go/deepseek-flash"
        );
    }

    #[test]
    fn a_fallback_that_repeats_a_model_is_dropped_and_a_third_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let mut repeats = settings(Some("openai/gpt-5.5"), None);
        repeats.fallbacks = vec![
            fallback("openai/gpt-5.5", None),
            fallback("zai/glm-5", None),
            fallback("zai/glm-5", Some("low")),
        ];
        assert_eq!(
            write(dir.path(), repeats).unwrap().fallbacks,
            [fallback("zai/glm-5", None)]
        );
        let mut three = settings(None, None);
        three.fallbacks = vec![
            fallback("a/one", None),
            fallback("b/two", None),
            fallback("c/three", None),
        ];
        let error = write(dir.path(), three).unwrap_err();
        assert!(error.contains("at most 2"), "{error}");
        // Pi's own reading: a string is a model, the first two count.
        std::fs::write(
            path(dir.path()),
            r#"{ "fallbackModels": ["a/one", { "model": "bad" }, { "model": "b/two", "thinkingLevel": "low" }, "c/three"] }"#,
        )
        .unwrap();
        assert_eq!(
            read(dir.path()).fallbacks,
            [fallback("a/one", None), fallback("b/two", Some("low"))]
        );
    }

    #[test]
    fn a_bad_model_level_or_reserved_model_is_refused_and_nothing_is_written() {
        let dir = tempfile::tempdir().unwrap();
        let reserved_fallback = SubagentSettings {
            fallbacks: vec![fallback("openai/gpt-6-astra", None)],
            ..settings(Some("openai/gpt-5.5"), None)
        };
        let bad_fallback_level = SubagentSettings {
            fallbacks: vec![fallback("zai/glm-5", Some("ludicrous"))],
            ..settings(None, None)
        };
        for bad in [
            settings(Some("gpt-5.5"), None),
            settings(Some("openai/gpt-5.5"), Some("ludicrous")),
            settings(Some("anthropic/claude-fable-5"), Some("high")),
            settings(Some("openai/gpt-6-astra-mini"), None),
            reserved_fallback,
            bad_fallback_level,
        ] {
            assert!(write(dir.path(), bad.clone()).is_err(), "{bad:?}");
            assert!(!path(dir.path()).exists());
        }
        let error =
            write(dir.path(), settings(Some("anthropic/claude-fable-5"), None)).unwrap_err();
        assert!(error.contains("reserved"), "{error}");
        // A level alone is fine: children keep their own model at that level.
        write(dir.path(), settings(None, Some("low"))).unwrap();
        assert_eq!(read(dir.path()), settings(None, Some("low")));
        assert_eq!(
            file_json(dir.path()),
            serde_json::json!({ "defaultModel": null, "defaultThinkingLevel": "low" }),
            "no model is said explicitly, or Pi would run its own at that level"
        );
    }

    #[test]
    fn a_file_that_is_not_a_json_object_is_never_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        for text in ["{ \"defaultModel\": ", "[1, 2]"] {
            std::fs::write(path(dir.path()), text).unwrap();
            assert_eq!(read(dir.path()), SubagentSettings::default());
            let error = write(dir.path(), settings(Some("openai/gpt-5.5"), None)).unwrap_err();
            assert!(error.contains(FILE), "{error}");
            assert_eq!(std::fs::read_to_string(path(dir.path())).unwrap(), text);
        }
    }
}
