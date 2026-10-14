//! Host-local UUID -> native file lookup. Never rewrite Pi's conversation files.
use crate::HarnessError;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
};

/// Pi's `trust.json` decision for `cwd`: the nearest ancestor (or the
/// directory itself) with an explicit `true`/`false`, keyed by canonical path.
fn project_trusted(trust: &Value, cwd: &Path) -> bool {
    let canonical = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
    let text = canonical.display().to_string();
    // Node's realpath has no verbatim `\?\` prefix.
    let text = text.strip_prefix(r"\\?\").unwrap_or(&text).to_owned();
    let mut dir = Some(PathBuf::from(text));
    while let Some(current) = dir {
        let key = current.display().to_string();
        let decision = trust.as_object().and_then(|map| {
            map.iter()
                .find(|(path, _)| {
                    if cfg!(windows) {
                        path.eq_ignore_ascii_case(&key)
                    } else {
                        **path == key
                    }
                })
                .and_then(|(_, value)| value.as_bool())
        });
        if let Some(decision) = decision {
            return decision;
        }
        dir = current.parent().map(Path::to_path_buf);
    }
    false
}

#[derive(Clone)]
pub(super) struct Store {
    root: PathBuf,
    agent: PathBuf,
    legacy: PathBuf,
}
/// Pi's settings directory: an explicit override, else `PI_CODING_AGENT_DIR`,
/// else `~/.pi/agent`.
pub(super) fn agent_dir(explicit: Option<PathBuf>) -> PathBuf {
    explicit.unwrap_or_else(|| {
        crate::model_context::root(
            "PI_CODING_AGENT_DIR",
            crate::executable::home_or_current_dir().join(".pi/agent"),
        )
    })
}
impl Store {
    pub fn new(root: Option<PathBuf>, agent: Option<PathBuf>) -> Self {
        let home = crate::executable::home_or_current_dir();
        let agent = agent_dir(agent);
        Self {
            root: root.unwrap_or_else(|| agent.join("zeron-sessions")),
            agent,
            legacy: home.join(".pi/pi-acp/session-map.json"),
        }
    }
    /// Where Zeron keeps its own files: the session map, the command lists.
    pub(super) fn root(&self) -> &Path {
        &self.root
    }
    /// Whether a steering mode is already set in the settings Pi loads for
    /// `cwd` (global, then project).
    pub fn steering_mode_configured(&self, cwd: &Path) -> bool {
        self.settings_files(cwd)
            .iter()
            .any(|settings| json_file(settings).get("steeringMode").is_some())
    }
    /// Tokens Pi keeps free before it compacts `model` (`provider/id`), in
    /// the settings it loads for `cwd`.
    pub fn compaction_reserve(&self, cwd: &Path, model: Option<&str>) -> u64 {
        let files: Vec<Value> = self
            .settings_files(cwd)
            .iter()
            .map(|file| json_file(file))
            .collect();
        reserve_from(&files, model)
    }
    /// The settings files Pi actually loads for `cwd`: the global one, plus
    /// the project's only when the project is trusted (RPC mode never asks,
    /// so an undecided project stays untrusted).
    fn settings_files(&self, cwd: &Path) -> Vec<PathBuf> {
        let mut files = vec![self.agent.join("settings.json")];
        if project_trusted(&json_file(&self.agent.join("trust.json")), cwd) {
            files.push(cwd.join(".pi/settings.json"));
        }
        files
    }
    fn key(&self, id: &str) -> PathBuf {
        self.root
            .join(format!("{:x}.json", Sha256::digest(id.as_bytes())))
    }
    pub fn remember(&self, id: &str, file: &Path) -> Result<(), HarnessError> {
        if !file.is_absolute() {
            return Err(HarnessError::Protocol(
                "Pi returned a non-absolute session file".into(),
            ));
        }
        self.write(id, &json!({"sessionId":id,"sessionFile":file}))
    }
    fn write(&self, id: &str, value: &Value) -> Result<(), HarnessError> {
        std::fs::create_dir_all(&self.root)?;
        let path = self.key(id);
        if json_file(&path) == *value {
            return Ok(());
        }
        let temp = self.root.join(format!("{}.tmp", uuid::Uuid::new_v4()));
        std::fs::write(&temp, serde_json::to_vec(value).unwrap())?;
        let result = std::fs::rename(&temp, &path);
        if result.is_err() {
            let _ = std::fs::remove_file(&temp);
        }
        result.map_err(Into::into)
    }
    pub fn mark_submitted(&self, id: &str) -> Result<(), HarnessError> {
        let mut record = json_file(&self.key(id));
        if let Some(map) = record.as_object_mut() {
            if map.remove("emptyState").is_some() {
                self.write(id, &record)?;
            }
        }
        Ok(())
    }
    pub fn remember_empty(&self, state: &Value, cwd: &Path) -> Result<(), HarnessError> {
        let id = state["sessionId"]
            .as_str()
            .ok_or_else(|| HarnessError::Protocol("Pi omitted sessionId".into()))?;
        let mut record = json_file(&self.key(id));
        if record.is_object() {
            record["emptyState"] = state.clone();
            record["cwd"] = json!(cwd.canonicalize()?);
            self.write(id, &record)?;
        }
        Ok(())
    }
    pub fn resume_args(&self, id: &str, cwd: &Path) -> Result<Vec<String>, HarnessError> {
        match self.resolve(id, cwd) {
            Ok(file) => Ok(vec!["--session".into(), file.display().to_string()]),
            Err(error) => {
                let record = json_file(&self.key(id));
                let state = &record["emptyState"];
                let file = record["sessionFile"].as_str().map(Path::new);
                // Only our own post-ACK empty-session proof permits recreation.
                // Deleted history, custom extension entries, and uncertain crashed
                // prompts must never become a fresh conversation silently.
                if state.is_null()
                    || record["cwd"] != json!(cwd.canonicalize()?)
                    || !file.is_some_and(|p| {
                        p.is_absolute()
                            && p.metadata()
                                .is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound)
                    })
                {
                    return Err(error);
                }
                let mut args = vec![
                    "--session-id".into(),
                    id.into(),
                    "--session-dir".into(),
                    file.unwrap().parent().unwrap().display().to_string(),
                ];
                for (flag, value) in [
                    ("--provider", &state["model"]["provider"]),
                    ("--model", &state["model"]["id"]),
                    ("--thinking", &state["thinkingLevel"]),
                ] {
                    if let Some(value) = value.as_str() {
                        args.extend([flag.into(), value.into()]);
                    }
                }
                Ok(args)
            }
        }
    }
    pub fn resolve(&self, id: &str, cwd: &Path) -> Result<PathBuf, HarnessError> {
        let mut candidates = vec![];
        if let Some(file) = json_file(&self.key(id))["sessionFile"].as_str() {
            candidates.push(PathBuf::from(file));
        }
        if let Some(file) = json_file(&self.legacy)["sessions"][id]["sessionFile"].as_str() {
            candidates.push(PathBuf::from(file));
        }
        let mut roots = vec![self.agent.join("sessions")];
        for settings in self.settings_files(cwd) {
            if let Some(root) = json_file(&settings)["sessionDir"].as_str() {
                let path = if let Some(rest) = root.strip_prefix("~/") {
                    crate::executable::home_or_current_dir().join(rest)
                } else {
                    cwd.join(root)
                };
                roots.push(path);
            }
        }
        for file in candidates {
            if matches_id(&file, id) {
                return Ok(file.canonicalize()?);
            }
        }
        for root in roots {
            // Pi stores session files under per-project directories. No symlink traversal.
            let mut dirs = vec![(root, 0)];
            let mut seen = 0;
            while let Some((dir, depth)) = dirs.pop() {
                let Ok(entries) = std::fs::read_dir(dir) else {
                    continue;
                };
                for entry in entries.flatten() {
                    seen += 1;
                    if seen > 20000 {
                        return Err(HarnessError::Protocol(
                            "Pi session search exceeded 20000 entries".into(),
                        ));
                    }
                    let Ok(kind) = entry.file_type() else {
                        continue;
                    };
                    let path = entry.path();
                    if kind.is_dir() && depth < 2 {
                        dirs.push((path, depth + 1));
                    } else if kind.is_file()
                        && path.extension().is_some_and(|e| e == "jsonl")
                        && matches_id(&path, id)
                    {
                        return Ok(path.canonicalize()?);
                    }
                }
            }
        }
        Err(HarnessError::Protocol(format!(
            "native session file for {id} was not found"
        )))
    }
}
/// Pi's own reserve when no setting names one (`compaction.ts`).
const DEFAULT_RESERVE_TOKENS: u64 = 16_384;

/// Settings merge global then project: a later file beats an earlier one,
/// and a model's override beats the ordinary value.
fn reserve_from(files: &[Value], model: Option<&str>) -> u64 {
    let find = |value: &dyn Fn(&Value) -> Option<u64>| files.iter().rev().find_map(value);
    model
        .and_then(|model| {
            find(&|file| file["compaction"]["modelOverrides"][model]["reserveTokens"].as_u64())
        })
        .or_else(|| find(&|file| file["compaction"]["reserveTokens"].as_u64()))
        .unwrap_or(DEFAULT_RESERVE_TOKENS)
}
fn json_file(path: &Path) -> Value {
    std::fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or(Value::Null)
}
fn matches_id(path: &Path, id: &str) -> bool {
    use std::io::Read;
    let Ok(file) = std::fs::File::open(path) else {
        return false;
    };
    let mut line = String::new();
    if BufReader::new(file.take(65536))
        .read_line(&mut line)
        .is_err()
    {
        return false;
    }
    serde_json::from_str::<Value>(&line).is_ok_and(|v| v["type"] == "session" && v["id"] == id)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn the_compaction_reserve_follows_pi_settings() {
        assert_eq!(reserve_from(&[], None), 16_384);
        assert_eq!(reserve_from(&[Value::Null], Some("a/b")), 16_384);
        let global = json!({"compaction":{"enabled":true,"reserveTokens":65536}});
        assert_eq!(reserve_from(&[global.clone()], Some("a/b")), 65_536);
        let project = json!({"compaction":{"reserveTokens":8000}});
        assert_eq!(
            reserve_from(&[global.clone(), project.clone()], None),
            8_000
        );
        let overrides = json!({"compaction":{"modelOverrides":{"a/b":{"reserveTokens":1234}}}});
        assert_eq!(
            reserve_from(&[global.clone(), project, overrides.clone()], Some("a/b")),
            1_234
        );
        assert_eq!(reserve_from(&[overrides, global], Some("x/y")), 65_536);
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::new(Some(dir.path().join("index")), None);
        store.agent = dir.path().join("agent");
        std::fs::create_dir_all(&store.agent).unwrap();
        std::fs::write(
            store.agent.join("settings.json"),
            r#"{"compaction":{"reserveTokens":4000}}"#,
        )
        .unwrap();
        assert_eq!(store.compaction_reserve(dir.path(), None), 4_000);
    }
    #[test]
    fn only_proven_empty_local_sessions_can_be_recreated_and_submission_revokes_it() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::new(Some(dir.path().join("index")), None);
        store.agent = dir.path().join("agent");
        store.legacy = dir.path().join("legacy.json");
        let file = dir.path().join("session.jsonl");
        let state = json!({"sessionId":"empty","sessionFile":file,"model":{"provider":"mock","id":"test"},"thinkingLevel":"off"});
        store.remember("empty", &file).unwrap();
        assert!(store.resume_args("empty", dir.path()).is_err());
        store.remember_empty(&state, dir.path()).unwrap();
        assert_eq!(
            store.resume_args("empty", dir.path()).unwrap(),
            vec![
                "--session-id",
                "empty",
                "--session-dir",
                dir.path().to_str().unwrap(),
                "--provider",
                "mock",
                "--model",
                "test",
                "--thinking",
                "off"
            ]
        );
        let other = tempfile::tempdir().unwrap();
        assert!(store.resume_args("empty", other.path()).is_err());
        store.mark_submitted("empty").unwrap();
        assert!(store.resume_args("empty", dir.path()).is_err());
        std::fs::write(&file, "{\"type\":\"session\",\"id\":\"empty\"}\n").unwrap();
        assert_eq!(
            store.resume_args("empty", dir.path()).unwrap()[0],
            "--session"
        );
        std::fs::remove_file(&file).unwrap();
        assert!(
            store.resume_args("empty", dir.path()).is_err(),
            "missing persisted history must not be recreated"
        );
    }

    #[test]
    fn an_explicit_steering_mode_in_either_settings_file_is_respected() {
        let dir = tempfile::tempdir().unwrap();
        let agent = dir.path().join("agent");
        let cwd = dir.path().join("project");
        std::fs::create_dir_all(&agent).unwrap();
        std::fs::create_dir_all(cwd.join(".pi")).unwrap();
        let store = Store::new(Some(dir.path().join("index")), Some(agent.clone()));
        assert!(!store.steering_mode_configured(&cwd));
        std::fs::write(
            agent.join("settings.json"),
            r#"{"retry":{"enabled":false}}"#,
        )
        .unwrap();
        assert!(!store.steering_mode_configured(&cwd));
        std::fs::write(
            cwd.join(".pi/settings.json"),
            r#"{"steeringMode":"one-at-a-time"}"#,
        )
        .unwrap();
        // Pi ignores an untrusted project's settings, so Zeron must too.
        assert!(!store.steering_mode_configured(&cwd));
        let key = |path: &Path| {
            let text = path.canonicalize().unwrap().display().to_string();
            text.strip_prefix(r"\\?\").unwrap_or(&text).to_owned()
        };
        let trust = |entries: &[(&Path, bool)]| {
            let map: serde_json::Map<String, Value> = entries
                .iter()
                .map(|(path, decision)| (key(path), json!(decision)))
                .collect();
            std::fs::write(agent.join("trust.json"), Value::Object(map).to_string()).unwrap();
        };
        // The nearest explicit decision wins: a trusted parent folder...
        trust(&[(dir.path(), true)]);
        assert!(store.steering_mode_configured(&cwd));
        // ...unless the project itself is explicitly distrusted.
        trust(&[(dir.path(), true), (&cwd, false)]);
        assert!(!store.steering_mode_configured(&cwd));
        trust(&[(dir.path(), true)]);
        std::fs::remove_file(cwd.join(".pi/settings.json")).unwrap();
        std::fs::write(agent.join("settings.json"), r#"{"steeringMode":"all"}"#).unwrap();
        assert!(store.steering_mode_configured(&cwd));
    }

    #[test]
    fn resolves_legacy_and_native_files_and_rejects_wrong_identity() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session with spaces.jsonl");
        std::fs::write(&path, "{\"type\":\"session\",\"id\":\"old-id\"}\n").unwrap();
        // Windows expands short names and adds a verbatim prefix on canonicalization.
        let expected = path.canonicalize().unwrap();
        let mut store = Store::new(Some(dir.path().join("index")), None);
        store.agent = dir.path().join("agent");
        store.legacy = dir.path().join("legacy.json");
        std::fs::write(
            &store.legacy,
            json!({"version":1,"sessions":{"old-id":{"sessionFile":path}}}).to_string(),
        )
        .unwrap();
        assert_eq!(store.resolve("old-id", dir.path()).unwrap(), expected);
        store.remember("old-id", &path).unwrap();
        std::fs::remove_file(&store.legacy).unwrap();
        assert_eq!(store.resolve("old-id", dir.path()).unwrap(), expected);
        store.remember("wrong-id", &path).unwrap();
        assert!(store.resolve("wrong-id", dir.path()).is_err());
    }
}
