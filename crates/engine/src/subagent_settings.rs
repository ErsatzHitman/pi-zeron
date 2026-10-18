//! Settings → General → Default subagent: the agent (a Pi model and its
//! thinking level) every child an agent starts runs on unless the call names
//! its own: `subagent_start` tasks, dynamic-workflow agents and delegated
//! sessions, in every session (a plain chat, an orchestrator's root, its
//! members and their sub-sessions). They live in Pi's
//! `<agent dir>/subagents.json` as `defaultModel` and `defaultThinkingLevel`,
//! which those extensions re-read at each launch, so a change reaches running
//! chats at their next child. Every other key in the file is kept. A child
//! cannot run a reserved model (Claude Fable, GPT Astra), so neither can the
//! default.
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use zeron_proto::reserved_models;

use crate::advisor_settings::spec;

pub const FILE: &str = "subagents.json";
/// Pi's thinking levels, least first.
pub const THINKING_LEVELS: [&str; 7] = ["off", "minimal", "low", "medium", "high", "xhigh", "max"];

/// Serialises the engine's writers.
static WRITE_LOCK: Mutex<()> = Mutex::new(());

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct SubagentSettings {
    /// A `provider/id` spec; None is each child's own default (its parent
    /// session's model).
    pub model: Option<String>,
    /// One of [`THINKING_LEVELS`]; None is Pi's default for the model.
    pub thinking: Option<String>,
}

impl SubagentSettings {
    /// Checked and tidied: a well-formed model and a known level.
    fn normalized(self) -> Result<Self, String> {
        let model = match self
            .model
            .as_deref()
            .map(str::trim)
            .filter(|m| !m.is_empty())
        {
            Some(model) => Some(
                spec(model).ok_or_else(|| format!("{model} is not a Pi model (provider/id)"))?,
            ),
            None => None,
        };
        let thinking = match self
            .thinking
            .as_deref()
            .map(str::trim)
            .filter(|t| !t.is_empty())
        {
            Some(level) => Some(level_of(level).ok_or_else(|| {
                format!(
                    "{level} is not a thinking level ({})",
                    THINKING_LEVELS.join(", ")
                )
            })?),
            None => None,
        };
        Ok(Self { model, thinking })
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

/// What the file sets, as Pi reads it: an ill-formed value is no value.
fn from_root(root: &Map<String, Value>) -> SubagentSettings {
    SubagentSettings {
        model: root
            .get("defaultModel")
            .and_then(Value::as_str)
            .and_then(spec),
        thinking: root
            .get("defaultThinkingLevel")
            .and_then(Value::as_str)
            .and_then(|level| level_of(level.trim())),
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
/// so is a reserved model.
pub fn write(agent_dir: &Path, settings: SubagentSettings) -> Result<SubagentSettings, String> {
    let settings = settings.normalized()?;
    if let Some(model) = settings
        .model
        .as_deref()
        .filter(|model| reserved_models::reserved_family(model).is_some())
    {
        return Err(reserved_models::refusal(model, "A subagent"));
    }
    let _lock = WRITE_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
    let path = path(agent_dir);
    let mut root = load(&path)?.unwrap_or_default();
    let before = root.clone();
    for (key, value) in [
        ("defaultModel", &settings.model),
        ("defaultThinkingLevel", &settings.thinking),
    ] {
        match value {
            Some(value) => root.insert(key.into(), value.clone().into()),
            None => root.remove(key),
        };
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
        }
    }

    fn file_json(dir: &Path) -> Value {
        serde_json::from_str(&std::fs::read_to_string(path(dir)).unwrap()).unwrap()
    }

    #[test]
    fn a_missing_file_is_no_default_and_a_save_creates_it() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(read(dir.path()), SubagentSettings::default());
        write(dir.path(), SubagentSettings::default()).unwrap();
        assert!(!path(dir.path()).exists(), "saving nothing writes nothing");
        let saved = write(dir.path(), settings(Some(" openai/gpt-5.5 "), Some("high"))).unwrap();
        assert_eq!(saved, settings(Some("openai/gpt-5.5"), Some("high")));
        assert_eq!(read(dir.path()), saved);
        assert!(
            std::fs::read_to_string(path(dir.path()))
                .unwrap()
                .ends_with("}\n")
        );
    }

    #[test]
    fn a_save_keeps_every_other_key_and_clearing_removes_only_its_own() {
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
        expected
            .as_object_mut()
            .unwrap()
            .remove("defaultThinkingLevel");
        assert_eq!(file_json(dir.path()), expected);
        write(dir.path(), SubagentSettings::default()).unwrap();
        assert_eq!(
            file_json(dir.path()),
            serde_json::json!({ "worktreeWrites": true, "profiles": { "scout": { "model": "x/y" } } })
        );
    }

    #[test]
    fn a_bad_model_level_or_reserved_model_is_refused_and_nothing_is_written() {
        let dir = tempfile::tempdir().unwrap();
        for bad in [
            settings(Some("gpt-5.5"), None),
            settings(Some("openai/gpt-5.5"), Some("ludicrous")),
            settings(Some("anthropic/claude-fable-5"), Some("high")),
            settings(Some("openai/gpt-6-astra-mini"), None),
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
