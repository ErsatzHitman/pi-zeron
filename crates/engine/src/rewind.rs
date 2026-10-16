//! Rewind a chat to one of its user messages: the code, the conversation, or
//! both, like Claude Code's checkpoints. Pi does the work (its `zeron-rewind`
//! extension: file snapshots per user message, and its own tree navigation);
//! Zeron then cuts its transcript to match.
//!
//! Only in a normal main session ([`Chat::is_top_level`]: not a side chat, not
//! a chat an agent started, not an orchestrator) that runs Pi, on the chat's
//! host, while no turn runs, and only to a message Pi was seen to receive
//! (its `harnessRefs` entry, recorded as Pi took it).
//!
//! Order: Pi first, then the transcript, so a failure leaves both as they
//! were. Pi answers the side command on the [`REWIND_CHANNEL`] host channel,
//! matched by a nonce.

use std::time::Duration;

use serde::{Deserialize, Serialize};
use zeron_doc::{MessagePart, MessageRole};
use zeron_proto::{Chat, HarnessId, REWIND_CHANNEL, REWIND_COMMAND};

use crate::doc_host::DocHost;
use crate::sessions::SessionsEngine;
use crate::workspace_host::WorkspaceHost;

const ANSWER_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RewindMode {
    /// Code and conversation.
    Both,
    Conversation,
    Code,
}

impl RewindMode {
    fn as_str(self) -> &'static str {
        match self {
            Self::Both => "both",
            Self::Conversation => "conversation",
            Self::Code => "code",
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RewindParams {
    pub chat_id: String,
    /// The user message to rewind to: it and everything after it go.
    pub message_id: String,
    pub mode: RewindMode,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RewindOutcome {
    /// The conversation went back to before the message.
    pub conversation: bool,
    /// Files put back, and files the agent had created that were deleted.
    pub restored: u32,
    pub deleted: u32,
    /// Files that could not be restored.
    pub failed: Vec<String>,
}

/// Whether `chat` may rewind at all: a normal main session running Pi.
pub fn rewindable(chat: &Chat) -> bool {
    chat.is_top_level()
        && chat
            .config
            .as_ref()
            .is_some_and(|c| c.harness == HarnessId::Pi)
}

pub(crate) async fn rewind(
    workspace: &WorkspaceHost,
    sessions: &SessionsEngine,
    docs: &DocHost,
    p: RewindParams,
) -> Result<RewindOutcome, String> {
    let chat = workspace
        .chat(&p.chat_id)
        .map_err(|e| e.to_string())?
        .ok_or("This chat no longer exists.")?;
    if !rewindable(&chat) {
        return Err("Rewind is only available in main Pi sessions.".into());
    }
    if chat.device_id != docs.device_id() {
        return Err("Rewind runs on the device that hosts this chat.".into());
    }
    if sessions.turn_in_flight(&p.chat_id) {
        return Err("Pi is still working. Rewind once it has finished.".into());
    }
    let handle = docs.open(&p.chat_id).map_err(|e| e.to_string())?;
    let is_user = handle
        .doc()
        .read_entries()
        .map_err(|e| e.to_string())?
        .iter()
        .any(|e| e.id == p.message_id && e.role == MessageRole::User);
    if !is_user {
        return Err("That message is no longer in this chat.".into());
    }
    let target = handle.doc().harness_ref(&p.message_id).ok_or(
        "This message can't be rewound: it was sent before rewind was available, or never reached Pi.",
    )?;

    // Watch before asking, so the answer cannot slip past.
    let (mut ui, _guard) = sessions.watch_extension_ui(&p.chat_id);
    let nonce = uuid::Uuid::new_v4().simple().to_string();
    let action = zeron_proto::extension_ui::ExtensionUiAction {
        element_id: zeron_proto::SIDE_COMMAND_ELEMENT.into(),
        action_id: "run".into(),
        value: Some(serde_json::json!({
            "text": format!("/{REWIND_COMMAND} {target} {} {nonce}", p.mode.as_str()),
        })),
        row_id: None,
    };
    sessions
        .extension_ui_action(&p.chat_id, action)
        .map_err(|_| "Pi isn't running for this chat. Send a message, then rewind.".to_owned())?;
    let answer = tokio::time::timeout(ANSWER_TIMEOUT, async {
        loop {
            if let Some(answer) = ui.borrow_and_update().channels.get(REWIND_CHANNEL)
                && answer["nonce"] == nonce.as_str()
            {
                return Ok(answer.clone());
            }
            if ui.changed().await.is_err() {
                return Err("Pi stopped before it answered.".to_owned());
            }
        }
    })
    .await
    .map_err(|_| {
        "Pi did not answer. Is the zeron-rewind extension installed in Pi's extensions folder?"
            .to_owned()
    })??;
    if answer["ok"] != true {
        return Err(answer["error"]
            .as_str()
            .unwrap_or("Pi could not rewind.")
            .to_owned());
    }
    let outcome = RewindOutcome {
        conversation: answer["conversation"] == true,
        restored: answer["restored"].as_u64().unwrap_or(0) as u32,
        deleted: answer["deleted"].as_u64().unwrap_or(0) as u32,
        failed: answer["failed"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|f| f.as_str().map(str::to_owned))
            .collect(),
    };
    if outcome.conversation {
        handle
            .rewind_to(&p.message_id)
            .map_err(|e| format!("Pi rewound, but the transcript could not be cut: {e}"))?;
        // The sidebar preview follows the last message left.
        let preview = handle.doc().read_entries().ok().and_then(|entries| {
            entries.iter().rev().find_map(|e| {
                e.parts.iter().rev().find_map(|part| match part {
                    MessagePart::Text { text, .. } if !text.trim().is_empty() => Some(text.clone()),
                    _ => None,
                })
            })
        });
        if let Some(preview) = preview {
            workspace.note_message(&p.chat_id, &preview);
        }
    }
    Ok(outcome)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chat(harness: HarnessId) -> Chat {
        serde_json::from_value(serde_json::json!({
            "id": "chat-1", "deviceId": "dev", "title": null, "archived": false,
            "cwd": null, "branch": null, "checkoutId": null,
            "config": {"harness": harness, "model": null, "reasoning": null, "sandbox": "workspace-write"},
            "lastMessagePreview": null, "lastMessageAt": null,
            "createdAt": "2026-10-16T00:00:00Z",
        }))
        .unwrap()
    }

    #[test]
    fn only_main_pi_sessions_rewind() {
        assert!(rewindable(&chat(HarnessId::Pi)));
        assert!(!rewindable(&chat(HarnessId::ClaudeCode)));
        let mut side = chat(HarnessId::Pi);
        side.parent_chat_id = Some("parent".into());
        assert!(!rewindable(&side), "side chats and agent-started chats");
        let mut root = chat(HarnessId::Pi);
        root.id = format!("{}1", zeron_proto::orchestrator::ROOT_CHAT_PREFIX);
        assert!(!rewindable(&root), "orchestrator roots");
    }

    #[test]
    fn modes_read_as_the_command_takes_them() {
        let p: RewindParams = serde_json::from_value(
            serde_json::json!({"chatId":"c","messageId":"m","mode":"conversation"}),
        )
        .unwrap();
        assert_eq!(p.mode.as_str(), "conversation");
    }
}
