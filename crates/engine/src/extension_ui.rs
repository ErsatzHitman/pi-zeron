//! Live extension UI per chat, held by the host engine (the device running
//! the chat's harness). See [`zeron_proto::extension_ui`].
//!
//! This is live state, like a terminal: it never enters the chat doc (Pi's
//! advisor panel alone patches ten times a second). A UI reads it with the
//! `WatchExtensionUi` stream, forwarded to the host device when the chat runs
//! elsewhere, and acts with `ExtensionUiAction`.
//!
//! Watchers decide whether the harness opts in to rich extension UI at all:
//! while none is connected, Pi's extensions stay on plain dialogs, which every
//! client (an older desktop, the phone) answers through the transcript.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use tokio::sync::{mpsc, watch};
use zeron_harness::ExtensionUiPort;
use zeron_proto::extension_ui::{ExtensionUiAction, ExtensionUiState};

/// A watcher that drops and comes back (chat switch, reconnect, a window
/// reload) within this window does not bounce the extensions between
/// elements and dialogs.
const RELEASE_GRACE: Duration = Duration::from_secs(3);

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[derive(Default)]
pub(crate) struct ExtensionUiHub {
    chats: Mutex<HashMap<String, Arc<ChatUi>>>,
}

struct ChatUi {
    state: watch::Sender<ExtensionUiState>,
    watchers: AtomicUsize,
    watched: watch::Sender<bool>,
    /// The live run's action inbox; replaced by each new run.
    actions: Mutex<Option<mpsc::UnboundedSender<ExtensionUiAction>>>,
}

impl ChatUi {
    fn new() -> Self {
        Self {
            state: watch::channel(ExtensionUiState::default()).0,
            watchers: AtomicUsize::new(0),
            watched: watch::channel(false).0,
            actions: Mutex::new(None),
        }
    }
}

/// Keeps a chat watched; dropping the last one (after [`RELEASE_GRACE`])
/// tells the harness nobody renders its elements any more.
pub(crate) struct WatcherGuard {
    chat: Arc<ChatUi>,
}

impl Drop for WatcherGuard {
    fn drop(&mut self) {
        if self.chat.watchers.fetch_sub(1, Ordering::AcqRel) != 1 {
            return;
        }
        let chat = self.chat.clone();
        let release = async move {
            tokio::time::sleep(RELEASE_GRACE).await;
            if chat.watchers.load(Ordering::Acquire) == 0 {
                chat.watched.send_if_modified(|watched| std::mem::replace(watched, false));
            }
        };
        match tokio::runtime::Handle::try_current() {
            Ok(runtime) => {
                runtime.spawn(release);
            }
            Err(_) => {
                self.chat.watched.send_replace(false);
            }
        }
    }
}

impl ExtensionUiHub {
    fn chat(&self, chat_id: &str) -> Arc<ChatUi> {
        lock(&self.chats)
            .entry(chat_id.to_owned())
            .or_insert_with(|| Arc::new(ChatUi::new()))
            .clone()
    }

    /// The port handed to a new run of `chat_id`: it publishes into the
    /// chat's state, reads the chat's watched flag, and becomes the target of
    /// actions (a previous run's inbox is dropped).
    pub fn port(&self, chat_id: &str) -> ExtensionUiPort {
        let chat = self.chat(chat_id);
        let (actions, inbox) = mpsc::unbounded_channel();
        *lock(&chat.actions) = Some(actions);
        ExtensionUiPort {
            state: Some(chat.state.clone()),
            actions: Some(inbox),
            watched: Some(chat.watched.subscribe()),
        }
    }

    /// Start watching `chat_id`. The receiver yields the current state first.
    pub fn watch(&self, chat_id: &str) -> (watch::Receiver<ExtensionUiState>, WatcherGuard) {
        let chat = self.chat(chat_id);
        if chat.watchers.fetch_add(1, Ordering::AcqRel) == 0 {
            chat.watched.send_if_modified(|watched| !std::mem::replace(watched, true));
        }
        let mut receiver = chat.state.subscribe();
        receiver.mark_changed();
        (receiver, WatcherGuard { chat })
    }

    #[cfg(test)]
    pub fn state(&self, chat_id: &str) -> ExtensionUiState {
        self.chat(chat_id).state.borrow().clone()
    }

    /// Deliver a UI action to the chat's live harness process.
    pub fn act(&self, chat_id: &str, action: ExtensionUiAction) -> Result<(), String> {
        let chat = self.chat(chat_id);
        let sender = lock(&chat.actions).clone();
        match sender {
            Some(sender) if sender.send(action).is_ok() => Ok(()),
            _ => Err("The agent for this chat is not running".into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test(start_paused = true)]
    async fn watchers_gate_the_port_with_a_grace_period_and_actions_reach_the_live_run() {
        let hub = ExtensionUiHub::default();
        let mut port = hub.port("chat");
        let mut watched = port.watched.take().unwrap();
        assert!(!*watched.borrow_and_update());
        let (mut state, first) = hub.watch("chat");
        assert!(*watched.borrow_and_update());
        let second = hub.watch("chat").1;
        drop(first);
        drop(second);
        // Still watched inside the grace window; a quick return keeps it so.
        tokio::time::sleep(RELEASE_GRACE / 2).await;
        assert!(*watched.borrow());
        let back = hub.watch("chat").1;
        tokio::time::sleep(RELEASE_GRACE * 2).await;
        assert!(*watched.borrow());
        drop(back);
        tokio::time::sleep(RELEASE_GRACE * 2).await;
        assert!(!*watched.borrow());

        // The run publishes; a watcher sees it.
        port.state.as_ref().unwrap().send_replace(ExtensionUiState {
            revision: 1,
            elements: vec![json!({"id":"jobs","ns":"bash"})],
            ..Default::default()
        });
        assert!(state.has_changed().unwrap());
        assert_eq!(state.borrow_and_update().revision, 1);
        assert_eq!(hub.state("chat").elements.len(), 1);

        let action = ExtensionUiAction {
            element_id: "bash:jobs".into(),
            action_id: "kill".into(),
            value: None,
            row_id: Some("j1".into()),
        };
        hub.act("chat", action.clone()).unwrap();
        assert_eq!(port.actions.as_mut().unwrap().recv().await.unwrap(), action);
        // A later run takes over the inbox; with none alive, acting fails.
        drop(port);
        assert!(hub.act("chat", action).is_err());
        assert!(hub.act("other", ExtensionUiAction::default()).is_err());
    }
}
