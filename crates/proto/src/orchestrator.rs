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
/// The run's own chat title: the label of its task.
pub const ENV_CHAT_TITLE: &str = "ZERON_CHAT_TITLE";
/// The chat that started this one, and its title.
pub const ENV_PARENT_CHAT_ID: &str = "ZERON_PARENT_CHAT_ID";
pub const ENV_PARENT_TITLE: &str = "ZERON_PARENT_TITLE";
/// The orchestrator root of the run's tree (its own id, for a root).
pub const ENV_ROOT_CHAT_ID: &str = "ZERON_ROOT_CHAT_ID";
/// Every key [`Place::env`] may set, for clearing a stale inherited one.
pub const PLACE_ENV: [&str; 6] = [
    ENV_ROLE,
    ENV_TREE_DEPTH,
    ENV_CHAT_TITLE,
    ENV_PARENT_CHAT_ID,
    ENV_PARENT_TITLE,
    ENV_ROOT_CHAT_ID,
];
/// Titles go into env and prompts as one short line.
const MAX_LABEL_CHARS: usize = 120;

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

/// Who a run is and who it answers to: its role, the label of its task (its
/// chat title), the chat that started it and its orchestrator. A run gets
/// it in its env ([`Place::env`]) and, for Pi, in its prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Place {
    pub role: Role,
    pub chat_id: String,
    pub title: Option<String>,
    pub parent_id: Option<String>,
    pub parent_title: Option<String>,
    /// The orchestrator root; `None` outside a tree.
    pub root_id: Option<String>,
}

impl Place {
    /// The place of `chat_id`, given it and its ancestors (any order).
    pub fn of(chats: &[Chat], chat_id: &str) -> Self {
        let chat = chats.iter().find(|c| c.id == chat_id);
        let parent_id = chat.and_then(|c| c.parent_chat_id.clone());
        let parent_title = parent_id
            .as_deref()
            .and_then(|id| chats.iter().find(|c| c.id == id))
            .and_then(|c| c.title.clone());
        let role = Role::of(chats, chat_id);
        let root_id = match role {
            Role::Chat => None,
            Role::Root => Some(chat_id.to_owned()),
            Role::Member { .. } => root_of(chats, chat_id).map(|root| root.id.clone()),
        };
        Place {
            role,
            chat_id: chat_id.to_owned(),
            title: chat.and_then(|c| c.title.clone()).and_then(label),
            parent_id,
            parent_title: parent_title.and_then(label),
            root_id,
        }
    }

    /// The process env that carries this place ([`Role::env`] plus ids and
    /// titles). Empty for a plain top-level chat.
    pub fn env(&self) -> Vec<(&'static str, String)> {
        let mut env = self.role.env();
        if self.role == Role::Chat && self.parent_id.is_none() {
            return env;
        }
        let optional = [
            (ENV_CHAT_TITLE, &self.title),
            (ENV_PARENT_CHAT_ID, &self.parent_id),
            (ENV_PARENT_TITLE, &self.parent_title),
            (ENV_ROOT_CHAT_ID, &self.root_id),
        ];
        env.extend(
            optional
                .into_iter()
                .filter_map(|(key, value)| value.clone().map(|value| (key, value))),
        );
        env
    }

    /// The place an MCP server's env carries (see [`Role::from_env`]).
    pub fn from_env(env: &std::collections::BTreeMap<String, String>) -> Self {
        let get = |key: &str| env.get(key).cloned().and_then(label);
        let role = Role::from_env(env);
        let chat_id = env.get("ZERON_CHAT_ID").cloned().unwrap_or_default();
        Place {
            role,
            root_id: get(ENV_ROOT_CHAT_ID)
                .or_else(|| (role == Role::Root).then(|| chat_id.clone())),
            chat_id,
            title: get(ENV_CHAT_TITLE),
            parent_id: get(ENV_PARENT_CHAT_ID),
            parent_title: get(ENV_PARENT_TITLE),
        }
    }

    /// The level below the root: 0 for the root, `None` outside a tree.
    pub fn depth(&self) -> Option<usize> {
        match self.role {
            Role::Chat => None,
            Role::Root => Some(0),
            Role::Member { depth } => Some(depth),
        }
    }

    /// One paragraph telling the run where it is, for its prompt. `None` for
    /// a root (its own prompt says) and a plain top-level chat.
    pub fn brief(&self) -> Option<String> {
        let me = match &self.title {
            Some(title) => format!("\"{title}\" (chat {})", self.chat_id),
            None => format!("chat {}", self.chat_id),
        };
        let parent = self
            .parent_id
            .as_deref()
            .map(|id| match &self.parent_title {
                Some(title) => format!("\"{title}\" (chat {id})"),
                None => format!("chat {id}"),
            });
        match self.role {
            Role::Root => None,
            Role::Chat => parent.map(|parent| {
                format!(
                    "You are a side chat ({me}) of {parent}, outside any orchestrator's tree. \
                     Your task is your first message."
                )
            }),
            Role::Member { depth } => {
                let root = self.root_id.as_deref().unwrap_or("unknown");
                let parent = match parent {
                    Some(_) if depth == 1 => "the orchestrator itself".to_owned(),
                    Some(parent) => format!("the session {parent}"),
                    None => "unknown".to_owned(),
                };
                Some(format!(
                    "Your place: you are the session {me}, at level {depth} of the tree of \
                     orchestrator chat {root}. Your parent, who started you, is {parent}. Your \
                     task is your first message; your title is its label."
                ))
            }
        }
    }
}

/// A title as one line of at most [`MAX_LABEL_CHARS`]; `None` when blank.
fn label(text: String) -> Option<String> {
    let line: String = text
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .filter(|c| !c.is_control())
        .take(MAX_LABEL_CHARS)
        .collect();
    (!line.is_empty()).then_some(line)
}

/// Appended to a member's system prompt.
pub fn member_prompt(place: &Place) -> String {
    let depth = place.depth().unwrap_or(1);
    let sessions = if depth >= MAX_DEPTH {
        "You cannot start sessions; do the work here."
    } else {
        "Start a sub-session with zeron_create_chat only for an independent task in a \
         different folder, or when parallel work clearly pays."
    };
    MEMBER_PROMPT
        .replace("{place}", &place.brief().unwrap_or_default())
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
        titled(id, parent, None)
    }

    fn titled(id: &str, parent: Option<&str>, title: Option<&str>) -> Chat {
        serde_json::from_value(serde_json::json!({
            "id": id, "deviceId": "dev", "title": title, "archived": false,
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
    fn every_run_knows_its_task_parent_and_orchestrator() {
        let root = new_root_chat_id();
        let chats = vec![
            titled(&root, None, Some("Orchestrator")),
            titled("child", Some(&root), Some("Fix the parser")),
            titled("grandchild", Some("child"), Some("Write\nthe  tests")),
            titled("plain", None, Some("Notes")),
            titled("side", Some("plain"), None),
        ];
        let grandchild = Place::of(&chats, "grandchild");
        assert_eq!(grandchild.role, Role::Member { depth: 2 });
        assert_eq!(
            grandchild.title.as_deref(),
            Some("Write the tests"),
            "one line"
        );
        assert_eq!(grandchild.parent_id.as_deref(), Some("child"));
        assert_eq!(grandchild.parent_title.as_deref(), Some("Fix the parser"));
        assert_eq!(grandchild.root_id.as_deref(), Some(root.as_str()));
        let brief = grandchild.brief().unwrap();
        assert!(
            brief.contains("\"Write the tests\" (chat grandchild)"),
            "{brief}"
        );
        assert!(brief.contains("level 2"), "{brief}");
        assert!(
            brief.contains(&format!("orchestrator chat {root}")),
            "{brief}"
        );
        assert!(
            brief.contains("the session \"Fix the parser\" (chat child)"),
            "{brief}"
        );
        let child = Place::of(&chats, "child");
        assert!(child.brief().unwrap().contains("the orchestrator itself"));
        assert_eq!(
            Place::of(&chats, &root).root_id.as_deref(),
            Some(root.as_str())
        );
        assert_eq!(Place::of(&chats, &root).brief(), None);
        // A side chat outside a tree knows its parent; a plain chat gets nothing.
        let side = Place::of(&chats, "side");
        assert!(
            side.brief()
                .unwrap()
                .contains("side chat (chat side) of \"Notes\"")
        );
        assert!(
            side.env()
                .iter()
                .any(|(k, v)| *k == ENV_PARENT_CHAT_ID && v == "plain")
        );
        assert!(Place::of(&chats, "plain").env().is_empty());
        assert_eq!(Place::of(&chats, "plain").brief(), None);
        // The env carries it whole to the MCP server and the Pi process.
        for id in [root.as_str(), "child", "grandchild", "side"] {
            let place = Place::of(&chats, id);
            let mut env: std::collections::BTreeMap<String, String> = place
                .env()
                .into_iter()
                .map(|(key, value)| (key.to_owned(), value))
                .collect();
            env.insert("ZERON_CHAT_ID".into(), id.into());
            assert_eq!(Place::from_env(&env), place, "{id}");
            assert!(
                env.keys()
                    .all(|k| k == "ZERON_CHAT_ID" || PLACE_ENV.contains(&k.as_str()))
            );
        }
        let long = "x".repeat(500);
        assert_eq!(label(long).unwrap().len(), MAX_LABEL_CHARS);
        assert_eq!(label(" \n ".into()), None);
    }

    #[test]
    fn prompts_fill_in_the_tree_limits() {
        let at = |depth| Place {
            role: Role::Member { depth },
            chat_id: "c".into(),
            title: Some("Task".into()),
            parent_id: Some("p".into()),
            parent_title: None,
            root_id: Some("orchestrator-r".into()),
        };
        let member = member_prompt(&at(2));
        assert!(member.contains("Your place: you are the session \"Task\" (chat c), at level 2"));
        assert!(member.contains("You are 2 levels below the root; the limit is 4."));
        assert!(member.contains("zeron_create_chat only for an independent task"));
        assert!(!member.contains('{'), "{member}");
        assert!(member_prompt(&at(MAX_DEPTH)).contains("You cannot start sessions"));
        assert!(ROOT_SYSTEM_PROMPT.starts_with("You are the orchestrator in Zeron"));
        assert_eq!(
            ROOT_TOOLS,
            "zeron_*,read,ls,find,grep,ask_user,todo,goal_*,memory_search,memory_read,memory_status"
        );
    }
}
