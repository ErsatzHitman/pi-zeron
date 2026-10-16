//! The capability map appended to every Pi chat's system prompt: what the
//! session is, how it relates to the chats around it, which Zeron features it
//! has and which it lacks. One generated text per surface (main chat, side
//! chat, orchestrator root, member; read-only or not), so every surface reads
//! the same facts each turn. Sub-agents get theirs from the guard
//! (`subagentNote` in `guard.mjs`).
use zeron_proto::orchestrator::{Place, Role};

/// What is true for every session, whatever its place.
const COMMON: &str = "\
How Zeron behaves:
- A message that arrives while you work steers you at your next step. Stop (or zeron_interrupt_chat) ends only the current turn and clears queued messages; your sub-agents and background shells keep running, so check or stop them.
- A question you ask (ask_user, an extension dialog) shows in place of the user's composer; if nobody is watching it waits. A dialog that comes back cancelled means the user interrupted you: stop and wait. To ask for a key, password or token, use an ask_user input whose text names the secret (Zeron masks it and keeps it out of history); never ask for a secret in chat.
- Zeron may block deletes (rm, Remove-Item, git clean, uninstalls, delete tools) and nested agent CLIs for you and your sub-agents. If a call is blocked, do not retry or rephrase it: say what must go and let the user do it.
- Pi sub-agents you start (subagent, delegate, workflows) are told whose sub-agents they are. They cannot use Zeron's sessions or board, and their charts and questions do not reach the user: brief them fully and relay what matters.
- The user may be listening in voice mode, and prompts may be dictated (an odd word may be a transcription slip). Lead with a short plain-language outcome; never leave the key result only in a table, code block or tool output.
- Slash commands (/reload, extension commands) are the user's to type. Never send /pi_ui_event or /pi_ui_completions; they are internal.
- If you have them: todo is the user's checklist in the session panel (keep it current); share_images is how the user sees a screenshot; workflow runs show in the panel, and one the user stopped is not a failure.";

/// The capability map for a session at `place`.
pub(crate) fn capability_map(place: &Place, read_only: bool) -> String {
    let me = match &place.title {
        Some(title) => format!("\"{title}\" (chat {})", place.chat_id),
        None => format!("chat {}", place.chat_id),
    };
    let parent = place
        .parent_id
        .as_deref()
        .map(|id| match &place.parent_title {
            Some(title) => format!("\"{title}\" (chat {id})"),
            None => format!("chat {id}"),
        });
    let mut out = vec!["# Zeron capability map".to_owned()];
    let (what, tools, lacks) = match (place.role, parent) {
        (Role::Root, _) => (
            format!(
                "You are the orchestrator {me}: the root of a tree of Pi sessions. The user talks to you."
            ),
            "zeron_* (start, message, read, wait for, interrupt and archive sessions; the tree's message board, which you read and post to tree-wide; zeron_whoami; zeron_show_visual), read/ls/find/grep, ask_user, todo, goal and memory tools.",
            "bash and edits: your sessions do the work.",
        ),
        (Role::Member { depth }, _) => (
            format!(
                "You are the session {me}, a member at level {depth} of the tree of orchestrator chat {}. Your place, parent and task are above.",
                place.root_id.as_deref().unwrap_or("unknown")
            ),
            "Pi's coding tools; zeron_* to read and message chats, zeron_whoami, the board (zeron_board_read, zeron_board_post) of your orchestrator's sessions in your folder, and zeron_create_chat for sessions below you while the depth limit allows.",
            "nothing outside your tree's board; the user rarely reads your chat, so your final reply is your report.",
        ),
        (Role::Chat, Some(parent)) => (
            format!(
                "You are a side chat (\"btw\") {me} of the chat {parent}, outside any orchestrator's tree. The user opened you beside that chat."
            ),
            "Pi's coding tools; zeron_* to read and message chats (zeron_read_chat and zeron_send_message reach your parent), zeron_whoami, zeron_show_visual.",
            "zeron_create_chat (side chats cannot start chats; ask your parent) and the board (it belongs to orchestrator trees).",
        ),
        (Role::Chat, None) => (
            format!(
                "You are a Zeron main chat {me}: a top-level Pi session the user talks to directly, in no orchestrator's tree."
            ),
            "Pi's coding tools; zeron_* to list, read and message other chats, start side chats (zeron_create_chat) and show visuals; zeron_whoami returns your place.",
            "the board (zeron_board_read, zeron_board_post): it belongs to orchestrator trees.",
        ),
    };
    out.push(what);
    out.push(format!("Zeron tools you have: {tools}"));
    out.push(format!("You do not have: {lacks}"));
    if read_only {
        out.push(
            "This session is read-only: edits, shell writes and starting sessions are blocked. You may read, use zeron_board_read and zeron_board_post, and reply with zeron_send_message. Finish by saying what you would change and where."
                .to_owned(),
        );
    }
    out.push(COMMON.to_owned());
    out.join("\n\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn place(role: Role, parent: Option<&str>) -> Place {
        Place {
            role,
            chat_id: "chat-1".into(),
            title: Some("Fix the parser".into()),
            parent_id: parent.map(str::to_owned),
            parent_title: parent.map(|_| "Plan".to_owned()),
            root_id: matches!(role, Role::Member { .. } | Role::Root)
                .then(|| "orchestrator-r".to_owned()),
        }
    }

    #[test]
    fn every_surface_says_what_it_is_and_what_it_lacks() {
        let main = capability_map(&place(Role::Chat, None), false);
        assert!(main.contains("Zeron main chat \"Fix the parser\" (chat chat-1)"));
        assert!(main.contains("You do not have: the board"));
        let side = capability_map(&place(Role::Chat, Some("p-1")), false);
        assert!(side.contains("side chat (\"btw\")"));
        assert!(side.contains("\"Plan\" (chat p-1)"));
        assert!(side.contains("You do not have: zeron_create_chat"));
        let member = capability_map(&place(Role::Member { depth: 2 }, Some("p-1")), false);
        assert!(member.contains("level 2 of the tree of orchestrator chat orchestrator-r"));
        assert!(member.contains("zeron_board_post"));
        let root = capability_map(&place(Role::Root, None), false);
        assert!(root.contains("You are the orchestrator"));
        for map in [&main, &side, &member, &root] {
            assert!(map.contains("How Zeron behaves:"));
            assert!(!map.contains("read-only"));
        }
    }

    #[test]
    fn a_read_only_session_is_told_up_front() {
        let map = capability_map(&place(Role::Member { depth: 1 }, Some("p")), true);
        assert!(map.contains("This session is read-only"));
        assert!(map.contains("zeron_send_message"));
    }
}
