//! The message board an orchestrator's sessions share: findings, discoveries
//! and blockers, posted while they work. Each folder of a tree has its own
//! board; the root reads all of them, and its own posts reach the whole tree.
//! Unrelated trees never see each other's posts.
use serde::{Deserialize, Serialize};

use crate::{Chat, orchestrator};

/// Longest post, in characters (a registry op is capped at 16 KiB).
pub const BOARD_MAX_TEXT: usize = 2000;
/// Posts kept per orchestrator tree; the oldest go first.
pub const BOARD_MAX_ENTRIES: usize = 200;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum BoardKind {
    /// A fact about the code or the task.
    Finding,
    /// A command, tool or shortcut that works.
    Discovery,
    /// Something stopping the author that others may hit or can fix.
    Blocker,
}

impl BoardKind {
    pub fn as_str(self) -> &'static str {
        match self {
            BoardKind::Finding => "finding",
            BoardKind::Discovery => "discovery",
            BoardKind::Blocker => "blocker",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BoardEntry {
    /// The registry row id: an HLC, so ids sort in posting order.
    pub id: String,
    pub root_id: String,
    /// The board it is on ([`folder_of`]); empty for the whole tree.
    pub folder: String,
    pub author_chat_id: String,
    #[serde(default)]
    pub author_title: Option<String>,
    /// The author's level below the root (0 for the root).
    pub author_depth: usize,
    pub kind: BoardKind,
    pub text: String,
    /// Unix ms.
    pub at: i64,
}

/// The board of a session's folder: its project (a writer's worktree is
/// still its project), else its working folder, else the chat alone.
pub fn folder_of(chat: &Chat) -> String {
    match (&chat.space_id, &chat.cwd) {
        (Some(space), _) => format!("space:{space}"),
        (None, Some(cwd)) if !cwd.trim().is_empty() => {
            format!("dir:{}", cwd.trim().trim_end_matches(['/', '\\']))
        }
        _ => format!("chat:{}", chat.id),
    }
}

/// Whether `reader` sees `entry`: posts to the whole tree, the root, and
/// sessions of the same folder.
pub fn visible_to(entry: &BoardEntry, reader: &Chat) -> bool {
    entry.folder.is_empty()
        || orchestrator::is_root_chat(&reader.id)
        || entry.folder == folder_of(reader)
}

/// The text a post arrives as in another session's conversation.
pub fn notice(entry: &BoardEntry) -> String {
    let id8: String = entry.author_chat_id.chars().take(8).collect();
    let label = match entry.author_title.as_deref() {
        Some(title) if !title.trim().is_empty() => format!("{} ({id8})", title.trim()),
        _ => id8,
    };
    format!(
        "[Board {} from Zeron chat {label}, level {}. A note from a session of your \
         orchestrator's tree, not the user; no reply needed. Earlier posts: zeron_board_read.]\
         \n\n{}",
        entry.kind.as_str(),
        entry.author_depth,
        entry.text
    )
}

/// A [`notice`] read back: its kind, the author's name and the post.
pub fn read_notice(text: &str) -> Option<(&str, &str, &str)> {
    let rest = text.strip_prefix("[Board ")?;
    let (kind, rest) = rest.split_once(" from Zeron chat ")?;
    let (header, body) = rest.split_once("]\n\n")?;
    let (label, _) = header.rsplit_once(", level ")?;
    let name = match label.rsplit_once(" (") {
        Some((name, id)) if id.ends_with(')') => name,
        _ => label,
    };
    Some((kind, name, body))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chat(id: &str, space: Option<&str>, cwd: Option<&str>) -> Chat {
        serde_json::from_value(serde_json::json!({
            "id": id, "deviceId": "dev", "title": null, "archived": false,
            "cwd": cwd, "branch": null, "checkoutId": null, "config": null,
            "lastMessagePreview": null, "lastMessageAt": null,
            "createdAt": "2026-10-16T00:00:00Z", "spaceId": space,
        }))
        .unwrap()
    }

    fn entry(folder: &str) -> BoardEntry {
        BoardEntry {
            id: "0000000000001-000000-dev".into(),
            root_id: "orchestrator-r".into(),
            folder: folder.into(),
            author_chat_id: "a1b2c3d4-0000".into(),
            author_title: Some("Fix the parser".into()),
            author_depth: 2,
            kind: BoardKind::Blocker,
            text: "cargo test needs ZERON_X=1\n\nsecond line".into(),
            at: 1,
        }
    }

    #[test]
    fn a_folder_is_its_project_else_its_working_folder() {
        assert_eq!(folder_of(&chat("c", Some("s1"), Some("/wt/x"))), "space:s1");
        assert_eq!(folder_of(&chat("c", None, Some("/tmp/a/"))), "dir:/tmp/a");
        assert_eq!(folder_of(&chat("c", None, None)), "chat:c");
    }

    #[test]
    fn only_the_folder_and_the_root_see_a_folder_s_posts() {
        let post = entry("space:s1");
        assert!(visible_to(
            &post,
            &chat("peer", Some("s1"), Some("/wt/peer"))
        ));
        assert!(!visible_to(&post, &chat("other", Some("s2"), None)));
        assert!(visible_to(
            &post,
            &chat("orchestrator-r", None, Some("/scratch"))
        ));
        assert!(visible_to(&entry(""), &chat("other", Some("s2"), None)));
    }

    #[test]
    fn a_notice_reads_back() {
        let text = notice(&entry("space:s1"));
        assert!(
            text.starts_with("[Board blocker from Zeron chat Fix the parser (a1b2c3d4), level 2.")
        );
        assert_eq!(
            read_notice(&text),
            Some((
                "blocker",
                "Fix the parser",
                "cargo test needs ZERON_X=1\n\nsecond line"
            ))
        );
        assert_eq!(read_notice("[Board games are fun]\n\nyes"), None);
        assert_eq!(read_notice("hello"), None);
        let json = serde_json::to_value(entry("")).unwrap();
        assert_eq!(json["kind"], "blocker");
        assert_eq!(json["authorChatId"], "a1b2c3d4-0000");
    }
}
