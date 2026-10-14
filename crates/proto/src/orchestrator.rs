//! The orchestrator: a root chat whose Pi agent runs other Pi sessions
//! through the Zeron MCP server. The root and every session below it (the
//! sessions it starts, and theirs) list in the sidebar's Orchestrator tab;
//! the root never takes a row in the session list.
use crate::Chat;

/// Id prefix of an orchestrator's root chat.
pub const ROOT_CHAT_PREFIX: &str = "orchestrator-";
/// Title of a new root chat.
pub const ROOT_CHAT_TITLE: &str = "Orchestrator";
/// Levels a tree may grow below its root: the root's sessions are level 1,
/// the sessions they start level 2, and so on.
pub const MAX_DEPTH: usize = 4;

/// Appended to the root agent's system prompt (Pi `--append-system-prompt`).
pub const ROOT_PROMPT: &str = "\
# Zeron orchestrator

You are the orchestrator in Zeron. You do not edit files or run project \
commands yourself: you hand every task to Pi sessions that work in the \
right directory, follow them, and report back.

- Find the directory first. The user may name a project (zeron_list_projects \
lists them with their paths) or a folder such as D:\\OpenTalk.
- Start a session with zeron_create_chat: harness \"pi\", the project or cwd, \
a short title naming the task, and a prompt that states the task completely \
(the session sees nothing of this conversation). Use zeron_create_chats to \
start independent tasks together. Several sessions may work in one directory; \
give each its own task.
- Follow up with zeron_send_message, wait with zeron_wait_for_turn, read \
results with zeron_read_chat (include_tools false for a summary), and answer \
a session's questions with zeron_respond_to_input or ask the user.
- Sessions you start can start their own sub-sessions; they report to you \
through their own replies.
- Keep the user informed in a few lines: what you started, where, and what \
came back.
";

pub fn is_root_chat(chat_id: &str) -> bool {
    chat_id.starts_with(ROOT_CHAT_PREFIX)
}

/// A fresh root chat id.
pub fn new_root_chat_id() -> String {
    format!("{ROOT_CHAT_PREFIX}{}", uuid::Uuid::new_v4())
}

/// `chat_id` and its ancestors, root first. Stops at a parent that is not in
/// `chats` and at a cycle; empty when `chat_id` itself is unknown.
pub fn lineage<'a>(chats: &'a [Chat], chat_id: &str) -> Vec<&'a Chat> {
    let mut path: Vec<&Chat> = Vec::new();
    let mut next = Some(chat_id);
    while let Some(id) = next {
        if path.iter().any(|c| c.id == id) {
            break;
        }
        let Some(chat) = chats.iter().find(|c| c.id == id) else {
            break;
        };
        path.push(chat);
        next = chat.parent_chat_id.as_deref();
    }
    path.reverse();
    path
}

/// The orchestrator root `chat_id` belongs to (itself, for a root).
pub fn root_of<'a>(chats: &'a [Chat], chat_id: &str) -> Option<&'a Chat> {
    lineage(chats, chat_id)
        .first()
        .copied()
        .filter(|root| is_root_chat(&root.id))
}

/// How far below its orchestrator root `chat_id` is (0 for the root), or
/// `None` outside an orchestrator tree.
pub fn depth(chats: &[Chat], chat_id: &str) -> Option<usize> {
    let path = lineage(chats, chat_id);
    path.first()
        .filter(|root| is_root_chat(&root.id))
        .map(|_| path.len() - 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chat(id: &str, parent: Option<&str>) -> Chat {
        serde_json::from_value(serde_json::json!({
            "id": id, "deviceId": "dev", "title": null, "archived": false,
            "cwd": null, "branch": null, "checkoutId": null, "config": null,
            "lastMessagePreview": null, "lastMessageAt": null,
            "createdAt": "2026-10-14T00:00:00Z", "parentChatId": parent,
        }))
        .unwrap()
    }

    #[test]
    fn a_tree_hangs_off_its_root() {
        let root = new_root_chat_id();
        let chats = vec![
            chat(&root, None),
            chat("child", Some(&root)),
            chat("grandchild", Some("child")),
            chat("plain", None),
            chat("side", Some("plain")),
            chat("loop-a", Some("loop-b")),
            chat("loop-b", Some("loop-a")),
        ];
        assert!(is_root_chat(&root));
        let ids: Vec<&str> = lineage(&chats, "grandchild")
            .iter()
            .map(|c| c.id.as_str())
            .collect();
        assert_eq!(ids, [root.as_str(), "child", "grandchild"]);
        assert_eq!(depth(&chats, &root), Some(0));
        assert_eq!(depth(&chats, "grandchild"), Some(2));
        assert_eq!(
            root_of(&chats, "child").map(|c| c.id.as_str()),
            Some(root.as_str())
        );
        assert_eq!(depth(&chats, "side"), None, "a side chat of a plain chat");
        assert_eq!(depth(&chats, "missing"), None);
        assert_eq!(lineage(&chats, "loop-a").len(), 2, "a cycle ends the walk");
    }
}
