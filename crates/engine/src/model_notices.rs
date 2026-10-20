//! Notices the model is owed: what happened to its runtime while nobody was
//! talking to it (a killed Pi, a turn the engine could not resume). The
//! agent cannot report its own SIGKILL, so Zeron, the survivor, records the
//! failure on disk the moment it sees it and prefixes it to the next prompt
//! that chat's agent receives. Without this the transcript shows the user an
//! error card while the model resumes as if nothing happened.
//!
//! Host-local (`{data_dir}/model-notices/<chat>.json`), not in the chat doc:
//! the doc syncs to other devices, and the notice is about this host's agent.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::run_journal::sanitize_id;

/// Most notices kept per chat; older ones are dropped first.
const MAX_NOTICES: usize = 8;
/// Longest notice text kept; crash messages carry a stderr tail.
const MAX_NOTICE_CHARS: usize = 2000;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ModelNotice {
    pub id: String,
    pub at_ms: i64,
    pub text: String,
}

fn file(data_dir: &Path, chat_id: &str) -> PathBuf {
    data_dir
        .join("model-notices")
        .join(format!("{}.json", sanitize_id(chat_id)))
}

/// The chat's undelivered notices, oldest first. Unreadable or corrupt files
/// read as none: a notice is a courtesy, never a reason to fail a turn.
pub(crate) fn pending(data_dir: &Path, chat_id: &str) -> Vec<ModelNotice> {
    std::fs::read(file(data_dir, chat_id))
        .ok()
        .and_then(|raw| serde_json::from_slice(&raw).ok())
        .unwrap_or_default()
}

fn write(data_dir: &Path, chat_id: &str, notices: &[ModelNotice]) -> std::io::Result<()> {
    let path = file(data_dir, chat_id);
    if notices.is_empty() {
        return match std::fs::remove_file(&path) {
            Err(err) if err.kind() != std::io::ErrorKind::NotFound => Err(err),
            _ => Ok(()),
        };
    }
    let dir = path.parent().expect("notice file has a parent");
    std::fs::create_dir_all(dir)?;
    // Temp file + rename: a crash mid-write leaves the old list, not half a file.
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec(notices)?)?;
    std::fs::rename(&tmp, &path)
}

/// Record a notice for the chat's next prompt.
pub(crate) fn record(data_dir: &Path, chat_id: &str, at_ms: i64, text: &str) {
    let mut notices = pending(data_dir, chat_id);
    let trimmed = text.trim();
    let mut text: String = trimmed.chars().take(MAX_NOTICE_CHARS).collect();
    if text.len() < trimmed.len() {
        text.push_str(" [cut]");
    }
    notices.push(ModelNotice {
        id: uuid::Uuid::new_v4().to_string(),
        at_ms,
        text,
    });
    let excess = notices.len().saturating_sub(MAX_NOTICES);
    notices.drain(..excess);
    if let Err(err) = write(data_dir, chat_id, &notices) {
        tracing::warn!(chat = %chat_id, error = %err, "model notice not recorded");
    }
}

/// Drop the notices with these ids (they reached the model).
pub(crate) fn mark_delivered(data_dir: &Path, chat_id: &str, ids: &[String]) {
    if ids.is_empty() {
        return;
    }
    let mut notices = pending(data_dir, chat_id);
    notices.retain(|notice| !ids.contains(&notice.id));
    if let Err(err) = write(data_dir, chat_id, &notices) {
        tracing::warn!(chat = %chat_id, error = %err, "model notice not cleared");
    }
}

/// The prompt the agent receives: the notices, then the user's message.
pub(crate) fn wrap(notices: &[ModelNotice], prompt: &str) -> String {
    let list = notices
        .iter()
        .map(|notice| {
            let at = chrono::DateTime::from_timestamp_millis(notice.at_ms)
                .map(|at| at.with_timezone(&chrono::Local).format("%Y-%m-%d %H:%M:%S").to_string())
                .unwrap_or_default();
            format!("- {at}: {}", notice.text)
        })
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "[Message from Zeron (automatic notice). Not the user. Since your last reply, this \
         happened to your runtime:\n{list}\nAnything that was running then (tool calls, \
         background jobs, sub-agents) did not finish and its results are lost. Run git status \
         and check the worktree before relying on earlier work; redo only what is unfinished.]\n\n\
         ---\n\n{prompt}"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notices_persist_until_delivered_and_wrap_the_prompt() {
        let dir = tempfile::tempdir().unwrap();
        assert!(pending(dir.path(), "chat/../x").is_empty());

        record(dir.path(), "chat/../x", 1_000, "Pi exited unexpectedly (killed by SIGKILL)");
        record(dir.path(), "chat/../x", 2_000, "  second  ");
        let notices = pending(dir.path(), "chat/../x");
        assert_eq!(notices.len(), 2);
        assert_eq!(notices[1].text, "second");
        // Ids are sanitized into the store, never used as paths.
        assert!(dir.path().join("model-notices/chat____x.json").exists());

        let prompt = wrap(&notices, "continue");
        assert!(prompt.starts_with("[Message from Zeron (automatic notice). Not the user."));
        assert!(prompt.contains("killed by SIGKILL"));
        assert!(prompt.ends_with("---\n\ncontinue"));

        mark_delivered(dir.path(), "chat/../x", &[notices[0].id.clone()]);
        assert_eq!(pending(dir.path(), "chat/../x"), vec![notices[1].clone()]);
        mark_delivered(dir.path(), "chat/../x", &[notices[1].id.clone()]);
        assert!(pending(dir.path(), "chat/../x").is_empty());
        assert!(!dir.path().join("model-notices/chat____x.json").exists());
    }

    #[test]
    fn the_store_keeps_the_newest_notices_and_cuts_long_ones() {
        let dir = tempfile::tempdir().unwrap();
        for n in 0..(MAX_NOTICES + 3) {
            record(dir.path(), "c", n as i64, &format!("n{n}"));
        }
        let notices = pending(dir.path(), "c");
        assert_eq!(notices.len(), MAX_NOTICES);
        assert_eq!(notices[0].text, "n3");

        let ids: Vec<String> = notices.into_iter().map(|notice| notice.id).collect();
        mark_delivered(dir.path(), "c", &ids);
        record(dir.path(), "c", 0, &"x".repeat(MAX_NOTICE_CHARS + 50));
        let text = &pending(dir.path(), "c")[0].text;
        assert!(text.ends_with(" [cut]") && text.len() == MAX_NOTICE_CHARS + 6, "{}", text.len());
    }
}
