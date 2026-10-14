//! The coding agent behind voice mode. What the user says goes to the agent
//! in their chat through the composer's own send path, exactly as if typed;
//! voice then follows that chat and reads the agent's messages aloud
//! ([`super::narrator`]) while the agent keeps working.
use super::*;
use crate::{composer::Composer, state::AppState};
use gpui::{Entity, WeakEntity};
use zeron_proto::view::Indicator;

/// How long a dropped watch waits before it subscribes again.
const RETRY: std::time::Duration = std::time::Duration::from_secs(2);

/// What voice drives: the app state and the window's composer.
#[derive(Clone)]
pub(crate) struct AgentBridge {
    pub state: Entity<AppState>,
    pub composer: WeakEntity<Composer>,
}

impl VoiceController {
    pub(super) fn agent_indicator(&self, agent: &AgentBridge, cx: &App) -> Indicator {
        self.agent_chat.as_deref().map_or(Indicator::None, |chat| {
            agent.state.read(cx).indicator_for(chat, chrono::Utc::now())
        })
    }

    /// The app changed: the orb shows whether the agent is working or
    /// waiting for the user.
    pub(super) fn agent_changed(&mut self, cx: &mut Context<Self>) {
        let Some(agent) = self.agent.clone() else {
            return;
        };
        let now = self.agent_indicator(&agent, cx);
        if std::mem::replace(&mut self.agent_seen, now) != now {
            self.sync_work(cx);
        }
    }

    /// Send what the user said to the agent as a prompt, then follow the
    /// chat it went to.
    pub(super) fn send_prompt(&mut self, text: String, cx: &mut Context<Self>) {
        let Some(agent) = self.agent.clone() else {
            return;
        };
        let sent = match agent.composer.upgrade() {
            Some(composer) => composer.update(cx, |composer, cx| composer.send_spoken(text, cx)),
            None => Err("The chat is not open.".to_owned()),
        };
        match sent {
            Ok((chat, message)) => {
                if self.agent_chat.as_deref() != Some(chat.as_str()) || self.narration.is_none() {
                    self.follow(chat, Some(message), cx);
                }
                self.agent_seen = self.agent_indicator(&agent, cx);
                self.sync_work(cx);
            }
            Err(reason) => self.notice(format!("That wasn't sent. {reason}"), true, cx),
        }
    }

    /// Read `chat` aloud from now on: watch its transcript and say what the
    /// agent writes. `anchor` is the prompt just sent there.
    pub(super) fn follow(&mut self, chat: String, anchor: Option<String>, cx: &mut Context<Self>) {
        self.agent_chat = Some(chat.clone());
        if let Some(agent) = self.agent.clone() {
            self.agent_seen = self.agent_indicator(&agent, cx);
        }
        let Some(engine) = self.engine.clone() else {
            self.narration = None;
            return;
        };
        let epoch = self.epoch;
        self.narration = Some(cx.spawn(async move |this, cx| {
            // The chat's transcript copy and what has been read of it.
            let mut state = (narrator::Narrator::new(anchor), Vec::new());
            loop {
                let params = serde_json::json!({ "chatId": chat });
                let watch = engine
                    .client()
                    .subscribe_checked(zeron_rpc::methods::WATCH_DOC_MESSAGES, params)
                    .await;
                let mut frames = match watch {
                    Ok(frames) => frames,
                    Err(err) => {
                        tracing::warn!(%chat, error = %err, "voice transcript watch failed");
                        cx.background_executor().timer(RETRY).await;
                        if this.update(cx, |_, _| {}).is_err() {
                            return;
                        }
                        continue;
                    }
                };
                while let Some(value) = frames.recv().await {
                    // Decoded and read off the UI thread: a chat's first copy
                    // can be large.
                    let (next, lines, applied) = cx
                        .background_executor()
                        .spawn(async move {
                            let (mut narrator, mut entries) = state;
                            let applied =
                                serde_json::from_value::<zeron_doc::TranscriptUpdate>(value)
                                    .ok()
                                    .is_some_and(|update| {
                                        zeron_doc::apply_transcript_frame(
                                            &mut entries,
                                            update.frame,
                                        )
                                        .is_ok()
                                    });
                            let lines = if applied {
                                narrator.update(&entries)
                            } else {
                                Vec::new()
                            };
                            ((narrator, entries), lines, applied)
                        })
                        .await;
                    state = next;
                    let alive = this.update(cx, |voice, cx| {
                        if voice.epoch != epoch {
                            return;
                        }
                        for line in lines {
                            let (item, text) = (line.item, line.text);
                            voice.command(call::Command::Say { item, text }, cx);
                        }
                    });
                    if alive.is_err() {
                        return;
                    }
                    // A copy that no longer applies resubscribes for a fresh
                    // one; the narrator remembers what it has read.
                    if !applied {
                        break;
                    }
                }
                cx.background_executor().timer(RETRY).await;
                if this.update(cx, |_, _| {}).is_err() {
                    return;
                }
            }
        }));
    }
}
