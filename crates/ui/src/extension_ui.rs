//! The selected chat's live extension UI: Pi's PIUI elements (question
//! sheets, pinned trays, status chips, screens) plus its plain `notify`,
//! `setStatus`, `setWidget`, `setTitle` and `set_editor_text` requests.
//!
//! The state lives on the chat's host engine (`WatchExtensionUi`, forwarded
//! to the host device for a remote chat) and never enters the chat doc.
//! Watching is what opts the chat's extensions in to rich UI: a host without
//! `extension-ui-v1` (or a client that never watches) keeps Pi on dialogs.
//! Actions go back with `ExtensionUiAction`.

use gpui::{App, Context, Task};
use zeron_proto::extension_ui::{ExtensionUiAction, ExtensionUiState, NoticeLevel};
use zeron_rpc::methods;

use crate::state::{AppState, EngineHandle};

mod parts;
mod pinned;
mod prompts;
mod screen;
mod sheet;

pub(crate) use prompts::Front;
pub(crate) use screen::{ScreenClosed, ScreenView, screen_icon};

/// What the composer keeps to present the selected chat's live elements.
pub(crate) struct ComposerExtension {
    pub sheet: Option<sheet::SheetForm>,
    pub trays: std::collections::HashMap<String, pinned::TrayState>,
    pub pending: Option<parts::PendingConfirm>,
    pub markdown: parts::MarkdownCache,
    /// The sheet's own focus, for digits, Enter and Escape.
    pub focus: gpui::FocusHandle,
    /// Per chat, the prompt tab the user picked (see `prompts`).
    pub picked: std::collections::HashMap<String, String>,
    /// Per chat, the prompts already known: a new one the user opened comes
    /// to the front, one that arrives behind another waits as a tab.
    pub seen: std::collections::HashMap<String, std::collections::HashSet<String>>,
    /// The screens docked in the composer's place, by element key.
    pub screen_views: std::collections::HashMap<String, gpui::Entity<ScreenView>>,
}

impl ComposerExtension {
    pub fn new(cx: &mut App) -> Self {
        Self {
            sheet: None,
            trays: Default::default(),
            pending: None,
            markdown: Default::default(),
            focus: cx.focus_handle(),
            picked: Default::default(),
            seen: Default::default(),
            screen_views: Default::default(),
        }
    }
}

/// What of a chat's state has already been acted on, so each notice, title
/// and editor text applies once, and a watch's opening frame replays none of
/// the history it carries.
#[derive(Debug, Default, Clone, Copy, PartialEq)]
struct Applied {
    notice: u64,
    title: u64,
    editor: u64,
}

impl Applied {
    fn of(state: &ExtensionUiState) -> Self {
        Self {
            notice: state.notices.iter().map(|n| n.seq).max().unwrap_or(0),
            title: state.title.as_ref().map_or(0, |t| t.seq),
            editor: state.editor_text.as_ref().map_or(0, |t| t.seq),
        }
    }
}

/// What a new frame asks of the client, beyond rendering it.
#[derive(Debug, Default, PartialEq)]
pub(crate) struct Effects {
    pub notices: Vec<(NoticeLevel, String)>,
    pub title: Option<String>,
    pub editor_text: Option<String>,
}

/// Effects of `next` after `applied`; `None` (an opening frame) yields none.
fn effects(applied: Option<Applied>, next: &ExtensionUiState) -> Effects {
    let Some(applied) = applied else {
        return Effects::default();
    };
    Effects {
        notices: next
            .notices
            .iter()
            .filter(|n| n.seq > applied.notice)
            .map(|n| (n.level, n.message.clone()))
            .collect(),
        title: next
            .title
            .as_ref()
            .filter(|t| t.seq > applied.title && !t.text.trim().is_empty())
            .map(|t| t.text.trim().to_owned()),
        editor_text: next
            .editor_text
            .as_ref()
            .filter(|t| t.seq > applied.editor)
            .map(|t| t.text.clone()),
    }
}

/// The selected chat's live UI, held on [`AppState`].
#[derive(Default)]
pub(crate) struct LiveExtensionUi {
    pub chat: Option<String>,
    pub state: ExtensionUiState,
    applied: Option<Applied>,
    /// Text an extension put in the input box, for the composer to take.
    pub editor_text: Option<String>,
    task: Option<Task<()>>,
}

impl LiveExtensionUi {
    /// The UI of `chat_id`, if it is the one being watched.
    pub fn of(&self, chat_id: &str) -> Option<&ExtensionUiState> {
        (self.chat.as_deref() == Some(chat_id)).then_some(&self.state)
    }

    /// Whether `chat_id`'s watch has delivered its first frame.
    pub fn ready_for(&self, chat_id: &str) -> bool {
        self.chat.as_deref() == Some(chat_id) && self.applied.is_some()
    }

    fn clear(&mut self) {
        *self = Self::default();
    }
}

fn toast_kind(level: NoticeLevel) -> crate::toast::ToastKind {
    match level {
        NoticeLevel::Warning => crate::toast::ToastKind::Warning,
        NoticeLevel::Error => crate::toast::ToastKind::Error,
        NoticeLevel::Info | NoticeLevel::Other => crate::toast::ToastKind::Info,
    }
}

impl AppState {
    /// Watch the selected chat's live UI when its host serves it.
    pub(crate) fn start_extension_ui_watch(&mut self, chat_id: &str, cx: &mut Context<Self>) {
        self.extension_ui.clear();
        let Some(handle) = self.engine().cloned() else {
            return;
        };
        let Some(device) = self
            .chats
            .iter()
            .find(|chat| chat.id == chat_id)
            .map(|chat| chat.device_id.clone())
        else {
            return;
        };
        if !self.device_supports(&device, zeron_proto::capabilities::EXTENSION_UI_V1) {
            return;
        }
        self.extension_ui.chat = Some(chat_id.to_owned());
        self.extension_ui.task = Some(spawn_watch(cx, handle, chat_id.to_owned(), device));
    }

    pub(crate) fn stop_extension_ui_watch(&mut self) {
        self.extension_ui.clear();
    }

    /// Start the selected chat's watch if selecting it could not (its row or
    /// its host's capabilities had not synced yet).
    pub(crate) fn ensure_extension_ui_watch(&mut self, cx: &mut Context<Self>) {
        if self.extension_ui.chat.is_none()
            && let Some(chat_id) = self.selected_chat.clone()
        {
            self.start_extension_ui_watch(&chat_id, cx);
        }
    }

    fn apply_extension_ui(
        &mut self,
        chat_id: &str,
        next: ExtensionUiState,
        cx: &mut Context<Self>,
    ) {
        if self.extension_ui.chat.as_deref() != Some(chat_id) {
            return;
        }
        let effects = effects(self.extension_ui.applied, &next);
        self.extension_ui.applied = Some(Applied::of(&next));
        self.extension_ui.state = next;
        for (level, message) in effects.notices {
            let kind = toast_kind(level);
            // Toasting mutates a global and refreshes windows: never mid-update.
            cx.defer(move |cx| crate::toast::show(cx, kind, message));
        }
        if let Some(title) = effects.title
            && let Some(engine) = self.engine().cloned()
        {
            let params =
                serde_json::json!({ "op": "renameChat", "chatId": chat_id, "title": title });
            cx.spawn(async move |_, _| {
                if let Err(error) = engine.client().call(methods::MUTATE, params).await {
                    tracing::debug!(%error, "extension title rename failed");
                }
            })
            .detach();
        }
        if effects.editor_text.is_some() {
            self.extension_ui.editor_text = effects.editor_text;
        }
        cx.notify();
    }

    /// Send `action` to the selected chat's extensions.
    pub(crate) fn extension_ui_action(&self, action: ExtensionUiAction, cx: &mut App) {
        let (Some(chat_id), Some(engine)) =
            (self.extension_ui.chat.clone(), self.engine().cloned())
        else {
            return;
        };
        let device = self
            .chats
            .iter()
            .find(|chat| chat.id == chat_id)
            .map(|chat| chat.device_id.clone());
        let mut params = serde_json::json!({ "chatId": chat_id, "action": action });
        if let Some(device) = device {
            params["targetDeviceId"] = device.into();
        }
        spawn_call(cx, async move {
            if let Err(error) = engine
                .client()
                .call(methods::EXTENSION_UI_ACTION, params)
                .await
            {
                Some(format!("{error}"))
            } else {
                None
            }
        });
    }
}

/// Run an RPC call off the UI thread and toast its failure.
fn spawn_call(cx: &mut App, call: impl std::future::Future<Output = Option<String>> + 'static) {
    cx.spawn(async move |cx| {
        if let Some(error) = call.await {
            cx.update(|cx| crate::toast::error(cx, error));
        }
    })
    .detach();
}

fn spawn_watch(
    cx: &mut Context<AppState>,
    handle: EngineHandle,
    chat_id: String,
    device: String,
) -> Task<()> {
    cx.spawn(async move |this, cx| {
        const RETRY_DELAY: std::time::Duration = std::time::Duration::from_secs(2);
        loop {
            // The host compares the target with its own id, so a local chat
            // is served locally and a remote one is relay-forwarded.
            let params = serde_json::json!({ "chatId": chat_id, "targetDeviceId": device });
            match handle
                .client()
                .subscribe_checked(methods::WATCH_EXTENSION_UI, params)
                .await
            {
                Ok(mut rx) => {
                    while let Some(value) = rx.recv().await {
                        let Ok(state) = serde_json::from_value::<ExtensionUiState>(value) else {
                            tracing::warn!("dropping malformed extension UI frame");
                            continue;
                        };
                        let alive =
                            this.update(cx, |app, cx| app.apply_extension_ui(&chat_id, state, cx));
                        if alive.is_err() {
                            return;
                        }
                    }
                }
                Err(error) => {
                    tracing::debug!(%chat_id, %error, "extension UI watch failed; retrying");
                }
            }
            // The stream ended (host restart, link drop): its elements are
            // gone with it until the next frame says otherwise.
            let alive = this.update(cx, |app, cx| {
                if app.extension_ui.chat.as_deref() == Some(chat_id.as_str()) {
                    app.extension_ui.state = ExtensionUiState::default();
                    app.extension_ui.applied = None;
                    cx.notify();
                }
            });
            if alive.is_err() {
                return;
            }
            cx.background_executor().timer(RETRY_DELAY).await;
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use zeron_proto::extension_ui::{ExtensionNotice, SequencedText};

    fn state(
        notices: &[(u64, &str)],
        title: Option<(u64, &str)>,
        editor: Option<(u64, &str)>,
    ) -> ExtensionUiState {
        ExtensionUiState {
            notices: notices
                .iter()
                .map(|(seq, message)| ExtensionNotice {
                    seq: *seq,
                    message: (*message).into(),
                    level: NoticeLevel::Warning,
                })
                .collect(),
            title: title.map(|(seq, text)| SequencedText {
                seq,
                text: text.into(),
            }),
            editor_text: editor.map(|(seq, text)| SequencedText {
                seq,
                text: text.into(),
            }),
            ..Default::default()
        }
    }

    #[test]
    fn an_opening_frame_replays_nothing_and_later_frames_apply_once() {
        let first = state(
            &[(1, "old")],
            Some((2, "Old title")),
            Some((3, "old draft")),
        );
        assert_eq!(effects(None, &first), Effects::default());
        let applied = Applied::of(&first);
        let next = state(
            &[(1, "old"), (4, "Saved the plan")],
            Some((5, "  Refactor  ")),
            Some((3, "old draft")),
        );
        assert_eq!(
            effects(Some(applied), &next),
            Effects {
                notices: vec![(NoticeLevel::Warning, "Saved the plan".into())],
                title: Some("Refactor".into()),
                editor_text: None,
            }
        );
        let blank = state(&[], Some((9, " ")), Some((10, "")));
        let effects = effects(Some(Applied::of(&next)), &blank);
        assert_eq!(effects.title, None);
        // Clearing the input box is a real request.
        assert_eq!(effects.editor_text.as_deref(), Some(""));
    }
}
