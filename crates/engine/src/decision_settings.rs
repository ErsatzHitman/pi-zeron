//! Settings → General → Decision gate: which decision model Pi's decisions
//! gate asks before it lets a plan with a writer launch. Jev is TypeSafe's
//! `typesafe/jev-1.13` on OpenRouter, with the OpenRouter key. SubQ Snap is
//! `subq-snap-preview` on SubQ's own API, with a SubQ key (Settings → API
//! keys). Both are System One decision models with the same questions and
//! answers, so only the model changes. Kept in Pi's
//! `<agent dir>/decisions-model.json` as `{ "model": "jev" | "snap" }`, which
//! the gate re-reads at every consult, so a change applies to the next one in
//! every open chat.
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::api_keys;

pub const FILE: &str = "decisions-model.json";

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DecisionModel {
    /// TypeSafe's Jev, through OpenRouter.
    #[default]
    Jev,
    /// SubQ Snap, on SubQ's own API.
    Snap,
}

impl DecisionModel {
    pub const ALL: [Self; 2] = [Self::Jev, Self::Snap];

    pub fn label(self) -> &'static str {
        match self {
            Self::Jev => "Jev (TypeSafe)",
            Self::Snap => "SubQ Snap",
        }
    }

    /// The Settings → API keys provider whose active key the gate sends.
    pub fn key_provider(self) -> &'static str {
        match self {
            Self::Jev => "openrouter",
            Self::Snap => "subq",
        }
    }

    fn parse(text: &str) -> Option<Self> {
        match text.trim().to_ascii_lowercase().as_str() {
            "jev" => Some(Self::Jev),
            "snap" => Some(Self::Snap),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct DecisionSettings {
    pub model: DecisionModel,
    /// Set in replies: Settings → API keys holds an active key for `model`.
    /// Only a stored key counts here; the gate also accepts one from Pi's
    /// `auth.json` (OpenRouter) or the environment.
    #[serde(skip_deserializing)]
    pub has_key: bool,
}

pub fn path(agent_dir: &Path) -> PathBuf {
    agent_dir.join(FILE)
}

/// The chosen model; Jev when the file is missing, unreadable or names
/// something else.
pub fn read(agent_dir: &Path) -> DecisionSettings {
    let model = std::fs::read_to_string(path(agent_dir))
        .ok()
        .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
        .and_then(|value| value.get("model")?.as_str().and_then(DecisionModel::parse))
        .unwrap_or_default();
    with_key(agent_dir, model)
}

/// Save the choice and return what the file now sets.
pub fn write(agent_dir: &Path, settings: DecisionSettings) -> Result<DecisionSettings, String> {
    let failed = |error: std::io::Error| format!("{FILE} could not be saved: {error}");
    std::fs::create_dir_all(agent_dir).map_err(failed)?;
    let text = serde_json::to_string_pretty(&serde_json::json!({
        "model": settings.model,
        "updatedAt": chrono::Utc::now().to_rfc3339(),
    }))
    .map_err(|e| e.to_string())?;
    let mut tmp = tempfile::NamedTempFile::new_in(agent_dir).map_err(failed)?;
    std::io::Write::write_all(&mut tmp, format!("{text}\n").as_bytes()).map_err(failed)?;
    if tmp.persist(path(agent_dir)).is_err() {
        std::fs::write(path(agent_dir), format!("{text}\n")).map_err(failed)?;
    }
    Ok(read(agent_dir))
}

fn with_key(agent_dir: &Path, model: DecisionModel) -> DecisionSettings {
    let has_key = api_keys::Store::load(&agent_dir.join(api_keys::STORE_FILE))
        .ok()
        .is_some_and(|store| store.key(model.key_provider()).is_some());
    DecisionSettings { model, has_key }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jev_is_the_default_and_snap_is_saved_and_read_back() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(read(dir.path()).model, DecisionModel::Jev);
        let saved = write(
            dir.path(),
            DecisionSettings {
                model: DecisionModel::Snap,
                has_key: false,
            },
        )
        .unwrap();
        assert_eq!(saved.model, DecisionModel::Snap);
        let file: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(path(dir.path())).unwrap()).unwrap();
        assert_eq!(file["model"], "snap", "the name Pi's gate reads");
        for text in ["{ broken", r#"{ "model": "grok" }"#, "[]"] {
            std::fs::write(path(dir.path()), text).unwrap();
            assert_eq!(read(dir.path()).model, DecisionModel::Jev, "{text}");
        }
    }

    #[test]
    fn a_reply_says_whether_api_keys_holds_the_models_key() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!read(dir.path()).has_key);
        std::fs::write(
            dir.path().join(api_keys::STORE_FILE),
            serde_json::json!({
                "version": 1,
                "providers": { "subq": {
                    "active": "k1",
                    "keys": [{ "id": "k1", "label": "SubQ", "secret": "subq-0000" }]
                } }
            })
            .to_string(),
        )
        .unwrap();
        assert!(!read(dir.path()).has_key, "no OpenRouter key for Jev");
        let snap = write(
            dir.path(),
            DecisionSettings {
                model: DecisionModel::Snap,
                has_key: false,
            },
        )
        .unwrap();
        assert!(snap.has_key);
    }
}
