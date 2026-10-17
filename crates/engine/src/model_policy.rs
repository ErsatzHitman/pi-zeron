//! Session model restrictions: the models a session may not use, set before
//! it starts or at any point after. One file per chat under
//! `{data_dir}/model-policy`. A Pi run names its chat's file and every parent
//! chat's in `ZERON_MODEL_POLICY`, so an orchestrator's (or a side chat's
//! parent's) restrictions reach every chat below it, and Zeron's guard
//! extension reads them live in every Pi of the session: sub-agents,
//! workflows and members included. Global helpers (the advisor) are exempt
//! there. Nothing here is global: a chat with no file restricts nothing.
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

const DIR: &str = "model-policy";
/// Far above any real catalog's worth of picks; bounds the file.
const MAX_MODELS: usize = 1024;

/// The file's shape, as the guard reads it.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Policy {
    #[serde(default)]
    pub disabled: Vec<String>,
}

/// What a picker shows for a chat: its own list, and what its parents
/// disable (which it cannot lift).
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionPolicy {
    pub disabled: Vec<String>,
    pub inherited: Vec<String>,
}

/// FNV-1a: stable across Rust versions, unlike `DefaultHasher`.
fn fnv1a(text: &str) -> u64 {
    text.bytes().fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3)
    })
}

/// A chat id as a file name: itself when plainly safe, else its hash.
fn stem(chat_id: &str) -> String {
    let safe = !chat_id.is_empty()
        && chat_id.len() <= 128
        && chat_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    if safe {
        chat_id.to_owned()
    } else {
        format!("x{:016x}", fnv1a(chat_id))
    }
}

pub fn file(data_dir: &Path, chat_id: &str) -> PathBuf {
    data_dir.join(DIR).join(format!("{}.json", stem(chat_id)))
}

/// Trimmed, blank-free, without repeats (case-insensitive), bounded.
pub fn normalize(models: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for model in models {
        let model = model.trim();
        if model.is_empty() || out.iter().any(|seen| seen.eq_ignore_ascii_case(model)) {
            continue;
        }
        out.push(model.to_owned());
        if out.len() == MAX_MODELS {
            break;
        }
    }
    out
}

/// A chat's own disabled models (none when it never set any).
pub fn read(data_dir: &Path, chat_id: &str) -> Vec<String> {
    std::fs::read_to_string(file(data_dir, chat_id))
        .ok()
        .and_then(|text| serde_json::from_str::<Policy>(&text).ok())
        .map(|policy| normalize(policy.disabled))
        .unwrap_or_default()
}

/// Replace a chat's own list; an empty one removes its file.
pub fn write(
    data_dir: &Path,
    chat_id: &str,
    disabled: Vec<String>,
) -> std::io::Result<Vec<String>> {
    let disabled = normalize(disabled);
    let path = file(data_dir, chat_id);
    if disabled.is_empty() {
        return match std::fs::remove_file(&path) {
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => Err(error),
            _ => Ok(disabled),
        };
    }
    let dir = data_dir.join(DIR);
    std::fs::create_dir_all(&dir)?;
    let text = serde_json::to_vec(&Policy {
        disabled: disabled.clone(),
    })
    .map_err(std::io::Error::other)?;
    // Renamed into place so a guard never reads half a list. Windows will not
    // replace a file another process holds open: write it in place then.
    let tmp = dir.join(format!(".{}.tmp", stem(chat_id)));
    std::fs::write(&tmp, &text)?;
    if std::fs::rename(&tmp, &path).is_err() {
        let _ = std::fs::remove_file(&tmp);
        std::fs::write(&path, &text)?;
    }
    Ok(disabled)
}

/// The chats above `chat_id`, nearest first: a side chat's parent, a
/// member's orchestrator. Stops at a dangling or repeated id.
pub fn ancestors(chats: &[zeron_proto::Chat], chat_id: &str) -> Vec<String> {
    let parent_of = |id: &str| {
        chats
            .iter()
            .find(|chat| chat.id == id)
            .and_then(|chat| chat.parent_chat_id.clone())
    };
    let mut out: Vec<String> = Vec::new();
    let mut next = parent_of(chat_id);
    while let Some(id) = next.take() {
        if id == chat_id
            || out.contains(&id)
            || out.len() > zeron_proto::orchestrator::MAX_DEPTH + 1
        {
            break;
        }
        next = parent_of(&id);
        out.push(id);
    }
    out
}

/// A chat's lists for the picker.
pub fn session(data_dir: &Path, chat_id: &str, ancestors: &[String]) -> SessionPolicy {
    SessionPolicy {
        disabled: read(data_dir, chat_id),
        inherited: normalize(ancestors.iter().flat_map(|id| read(data_dir, id))),
    }
}

/// `ZERON_MODEL_POLICY` for a run of `chat_id`: its file, then its
/// ancestors'. Files that do not exist yet are named too, so a list set
/// mid-session reaches the running Pi.
pub fn env_value(data_dir: &Path, chat_id: &str, ancestors: &[String]) -> Option<String> {
    let files = std::iter::once(chat_id)
        .chain(ancestors.iter().map(String::as_str))
        .map(|id| file(data_dir, id));
    std::env::join_paths(files)
        .ok()
        .and_then(|joined| joined.into_string().ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chat(id: &str, parent: Option<&str>) -> zeron_proto::Chat {
        serde_json::from_value(serde_json::json!({
            "id": id,
            "deviceId": "d",
            "title": null,
            "archived": false,
            "cwd": null,
            "branch": null,
            "checkoutId": null,
            "config": null,
            "lastMessagePreview": null,
            "lastMessageAt": null,
            "createdAt": "2026-01-01T00:00:00Z",
            "parentChatId": parent,
        }))
        .unwrap()
    }

    #[test]
    fn a_chat_inherits_every_parents_list_and_keeps_its_own() {
        let dir = tempfile::tempdir().unwrap();
        let chats = [
            chat("root", None),
            chat("member", Some("root")),
            chat("leaf", Some("member")),
            chat("other", None),
        ];
        write(dir.path(), "root", vec!["openai/gpt-5.5".into()]).unwrap();
        write(
            dir.path(),
            "member",
            vec![
                " anthropic/claude-opus-4-5 ".into(),
                "OpenAI/GPT-5.5".into(),
                "".into(),
            ],
        )
        .unwrap();
        let up = ancestors(&chats, "leaf");
        assert_eq!(up, ["member", "root"]);
        let policy = session(dir.path(), "leaf", &up);
        assert!(policy.disabled.is_empty());
        assert_eq!(
            policy.inherited,
            ["anthropic/claude-opus-4-5", "OpenAI/GPT-5.5"]
        );
        assert_eq!(
            read(dir.path(), "member"),
            ["anthropic/claude-opus-4-5", "OpenAI/GPT-5.5"]
        );
        // Session-scoped: an unrelated chat restricts nothing.
        assert_eq!(
            session(dir.path(), "other", &ancestors(&chats, "other")),
            SessionPolicy::default()
        );
        // Cleared: the file goes, and clearing again is fine.
        write(dir.path(), "root", vec![]).unwrap();
        write(dir.path(), "root", vec![]).unwrap();
        assert!(!file(dir.path(), "root").exists());
    }

    #[test]
    fn the_run_names_its_own_file_then_its_parents() {
        let dir = tempfile::tempdir().unwrap();
        let value = env_value(dir.path(), "leaf", &["root".into()]).unwrap();
        let files: Vec<PathBuf> = std::env::split_paths(&value).collect();
        assert_eq!(files, [file(dir.path(), "leaf"), file(dir.path(), "root")]);
        // An id that is no file name still gets one of its own.
        let odd = file(dir.path(), "a/b:c");
        assert_eq!(odd.parent(), Some(dir.path().join(DIR).as_path()));
        assert_ne!(odd, file(dir.path(), "a/b:d"));
    }

    #[test]
    fn a_parent_loop_ends() {
        let chats = [chat("a", Some("b")), chat("b", Some("a"))];
        assert_eq!(ancestors(&chats, "a"), ["b"]);
    }
}
