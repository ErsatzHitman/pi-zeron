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

/// Process env naming a run's place in a tree: `orchestrator`, `member`,
/// or unset for a plain chat. User extensions read it too.
pub const ENV_ROLE: &str = "ZERON_ROLE";
/// A member's level below its root.
pub const ENV_TREE_DEPTH: &str = "ZERON_TREE_DEPTH";

/// The root agent's whole system prompt (Pi `--system-prompt`): it plans and
/// delegates, and has no coding prompt to fall back on.
pub const ROOT_SYSTEM_PROMPT: &str = include_str!("orchestrator_root.md");
/// The root's tools (Pi `--tools`): Zeron's delegation tools, read-only
/// files, and the planning extensions. No bash, edits or MCP servers.
pub const ROOT_TOOLS: &str = "zeron_*,read,ls,find,grep,ask_user,todo,goal_*,\
memory_search,memory_read,memory_status";
const MEMBER_PROMPT: &str = include_str!("orchestrator_member.md");

/// Where a run sits relative to an orchestrator tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// A plain chat, outside any tree.
    Chat,
    /// An orchestrator's root.
    Root,
    /// A session in a tree, `depth` levels below its root.
    Member { depth: usize },
}

impl Role {
    /// The role of `chat_id`, given it and its ancestors (any order).
    pub fn of(chats: &[Chat], chat_id: &str) -> Self {
        if is_root_chat(chat_id) {
            return Role::Root;
        }
        match depth(chats, chat_id) {
            Some(depth) if depth > 0 => Role::Member { depth },
            _ => Role::Chat,
        }
    }

    /// The process env that carries this role.
    pub fn env(self) -> Vec<(&'static str, String)> {
        match self {
            Role::Chat => Vec::new(),
            Role::Root => vec![(ENV_ROLE, "orchestrator".into())],
            Role::Member { depth } => vec![
                (ENV_ROLE, "member".into()),
                (ENV_TREE_DEPTH, depth.to_string()),
            ],
        }
    }

    /// The role an MCP server's env carries. Without `ZERON_ROLE` (an
    /// older engine) a root is still known by its chat id.
    pub fn from_env(env: &std::collections::BTreeMap<String, String>) -> Self {
        match env.get(ENV_ROLE).map(String::as_str) {
            Some("orchestrator") => Role::Root,
            Some("member") => Role::Member {
                depth: env
                    .get(ENV_TREE_DEPTH)
                    .and_then(|depth| depth.parse().ok())
                    .unwrap_or(1),
            },
            _ if env.get("ZERON_CHAT_ID").is_some_and(|id| is_root_chat(id)) => Role::Root,
            _ => Role::Chat,
        }
    }
}

/// Appended to a member's system prompt.
pub fn member_prompt(depth: usize) -> String {
    let sessions = if depth >= MAX_DEPTH {
        "You cannot start sessions; do the work here."
    } else {
        "Start a sub-session with zeron_create_chat only for an independent task in a \
         different folder, or when parallel work clearly pays."
    };
    MEMBER_PROMPT
        .replace("{sessions}", sessions)
        .replace("{depth}", &depth.to_string())
        .replace("{max}", &MAX_DEPTH.to_string())
}

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

    #[test]
    fn a_run_knows_its_role_from_the_tree_and_from_its_env() {
        let root = new_root_chat_id();
        let chats = vec![
            chat(&root, None),
            chat("child", Some(&root)),
            chat("grandchild", Some("child")),
            chat("plain", None),
            chat("side", Some("plain")),
        ];
        assert_eq!(Role::of(&chats, &root), Role::Root);
        assert_eq!(Role::of(&[], &root), Role::Root, "known by its id alone");
        assert_eq!(Role::of(&chats, "grandchild"), Role::Member { depth: 2 });
        assert_eq!(Role::of(&chats, "plain"), Role::Chat);
        assert_eq!(Role::of(&chats, "side"), Role::Chat);
        for role in [Role::Chat, Role::Root, Role::Member { depth: 3 }] {
            let env = role
                .env()
                .into_iter()
                .map(|(key, value)| (key.to_owned(), value))
                .collect();
            assert_eq!(Role::from_env(&env), role);
        }
        // An older engine sends no role; the root's id still names it.
        let env = [("ZERON_CHAT_ID".to_owned(), root.clone())].into();
        assert_eq!(Role::from_env(&env), Role::Root);
    }

    #[test]
    fn prompts_fill_in_the_tree_limits() {
        let member = member_prompt(2);
        assert!(member.contains("You are 2 levels below the root; the limit is 4."));
        assert!(member.contains("zeron_create_chat only for an independent task"));
        assert!(!member.contains('{'), "{member}");
        assert!(member_prompt(MAX_DEPTH).contains("You cannot start sessions"));
        assert!(ROOT_SYSTEM_PROMPT.starts_with("You are the orchestrator in Zeron"));
        assert_eq!(
            ROOT_TOOLS,
            "zeron_*,read,ls,find,grep,ask_user,todo,goal_*,memory_search,memory_read,memory_status"
        );
    }
}
