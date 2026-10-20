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

fn key_in(root: &Path, id: &str) -> PathBuf {
    root.join(format!("{:x}.json", Sha256::digest(id.as_bytes())))
}

#[derive(Clone)]
pub(super) struct Store {
    root: PathBuf,
    agent: PathBuf,
    /// Where the map lived before it moved out of Pi's settings directory
    /// (`<agent>/zeron-sessions`); still read so existing chats resume.
    previous: PathBuf,
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
        // Zeron's own files belong in Zeron's home, not in Pi's settings
        // directory (which users keep in git). A redirected Pi (an explicit
        // or `PI_CODING_AGENT_DIR` agent dir: tests, containers) keeps them
        // beside it so nothing escapes to the real home.
        let redirected = agent.is_some() || std::env::var_os("PI_CODING_AGENT_DIR").is_some();
        let agent = agent_dir(agent);
        let previous = agent.join("zeron-sessions");
        Self {
            root: root.unwrap_or_else(|| {
                if redirected {
                    previous.clone()
                } else {
                    home.join(".zeron").join("pi-sessions")
                }
            }),
            agent,
            previous,
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
        key_in(&self.root, id)
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
        if self.previous != self.root
            && let Some(file) = json_file(&key_in(&self.previous, id))["sessionFile"].as_str()
        {
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
        let mut truncated = false;
        for root in roots {
            if let Some(file) = find_in_root(root, id, &mut truncated)? {
                return Ok(file.canonicalize()?);
            }
        }
        Err(HarnessError::Protocol(if truncated {
            format!(
                "native session file for {id} was not found (only the first {HEADER_SCAN_LIMIT} session files were opened)"
            )
        } else {
            format!("native session file for {id} was not found")
        }))
    }
}

/// Session files whose headers are opened when no file name carries the id.
const HEADER_SCAN_LIMIT: usize = 20_000;
/// Directory entries listed under one root before the search gives up.
const WALK_LIMIT: usize = 200_000;

/// The session `id` under `root`. Pi names its files `<timestamp>_<id>.jsonl`,
/// so a file whose name carries the id is opened first, and only when none
/// does are the other files' headers read (a file renamed by hand still
/// resolves). The walk lists directories, which is cheap; opening a file is
/// not, so the opens are what stay bounded. Sets `truncated` when the header
/// scan left files unopened.
fn find_in_root(
    root: PathBuf,
    id: &str,
    truncated: &mut bool,
) -> Result<Option<PathBuf>, HarnessError> {
    // Pi stores session files under per-project directories. No symlink traversal.
    let mut dirs = vec![(root, 0)];
    let mut seen = 0;
    let mut others = Vec::new();
    while let Some((dir, depth)) = dirs.pop() {
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.flatten() {
            seen += 1;
            if seen > WALK_LIMIT {
                return Err(HarnessError::Protocol(format!(
                    "Pi session search exceeded {WALK_LIMIT} entries"
                )));
            }
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            let path = entry.path();
            if kind.is_dir() && depth < 2 {
                dirs.push((path, depth + 1));
            } else if kind.is_file() && path.extension().is_some_and(|e| e == "jsonl") {
                let named = path
                    .file_name()
                    .is_some_and(|name| name.to_string_lossy().contains(id));
                if named {
                    // Its header was just asked; the fallback need not repeat it.
                    if matches_id(&path, id) {
                        return Ok(Some(path));
                    }
                } else if others.len() < HEADER_SCAN_LIMIT {
                    others.push(path);
                } else {
                    *truncated = true;
                }
            }
        }
    }
    Ok(others.into_iter().find(|path| matches_id(path, id)))
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

    /// A store whose native sessions live under `<dir>/agent/sessions`.
    fn store_with_sessions(dir: &Path) -> (Store, PathBuf) {
        let mut store = Store::new(Some(dir.join("index")), None);
        store.agent = dir.join("agent");
        store.legacy = dir.join("legacy.json");
        let project = store.agent.join("sessions/--project--");
        std::fs::create_dir_all(&project).unwrap();
        (store, project)
    }

    fn write_session(path: &Path, id: &str) {
        std::fs::write(path, format!("{{\"type\":\"session\",\"id\":\"{id}\"}}\n")).unwrap();
    }

    #[test]
    fn resolve_prefers_files_whose_name_contains_the_id() {
        let dir = tempfile::tempdir().unwrap();
        let (store, project) = store_with_sessions(dir.path());
        for n in 0..300 {
            write_session(
                &project.join(format!("2026-01-01T00_{n}-decoy.jsonl")),
                &format!("decoy-{n}"),
            );
        }
        // A copy under another name carries the id in its header only: the
        // file named for the id is the one to find, whatever the listing order.
        write_session(&project.join("copy.jsonl"), "wanted-id");
        let named = project.join("2026-10-18T09-00-00_wanted-id.jsonl");
        write_session(&named, "wanted-id");
        assert_eq!(
            store.resolve("wanted-id", dir.path()).unwrap(),
            named.canonicalize().unwrap()
        );
    }

    #[test]
    fn resolve_falls_back_to_the_header_when_no_name_carries_the_id() {
        let dir = tempfile::tempdir().unwrap();
        let (store, project) = store_with_sessions(dir.path());
        write_session(&project.join("a.jsonl"), "other");
        let renamed = project.join("renamed-by-hand.jsonl");
        write_session(&renamed, "wanted-id");
        assert_eq!(
            store.resolve("wanted-id", dir.path()).unwrap(),
            renamed.canonicalize().unwrap()
        );
        // The name alone proves nothing: its header must name the id too.
        write_session(&project.join("2026_liar-id.jsonl"), "someone-else");
        assert!(store.resolve("liar-id", dir.path()).is_err());
    }

    #[test]
    fn resolve_over_the_header_limit_still_finds_a_name_match() {
        let dir = tempfile::tempdir().unwrap();
        let (store, project) = store_with_sessions(dir.path());
        for n in 0..HEADER_SCAN_LIMIT + 50 {
            std::fs::write(project.join(format!("{n}.jsonl")), "{}\n").unwrap();
        }
        let named = project.join("2026_wanted-id.jsonl");
        write_session(&named, "wanted-id");
        assert_eq!(
            store.resolve("wanted-id", dir.path()).unwrap(),
            named.canonicalize().unwrap()
        );
        // With no name to go by, the search says what it did not open instead
        // of failing as a protocol breach.
        let message = store
            .resolve("unknown-id", dir.path())
            .unwrap_err()
            .to_string();
        assert!(message.contains("was not found"), "{message}");
        assert!(message.contains("first 20000"), "{message}");
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

    #[test]
    fn a_map_entry_left_in_the_agent_dir_still_resolves() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        std::fs::write(&path, "{\"type\":\"session\",\"id\":\"moved-id\"}
").unwrap();
        let mut old = Store::new(Some(dir.path().join("agent/zeron-sessions")), None);
        old.agent = dir.path().join("agent");
        old.remember("moved-id", &path).unwrap();
        let mut store = Store::new(Some(dir.path().join("zeron/pi-sessions")), None);
        store.agent = dir.path().join("agent");
        store.previous = dir.path().join("agent/zeron-sessions");
        store.legacy = dir.path().join("legacy.json");
        assert_eq!(
            store.resolve("moved-id", dir.path()).unwrap(),
            path.canonicalize().unwrap()
        );
    }
}
