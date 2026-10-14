//! The coding agent behind voice mode. The assistant cannot read or change
//! code itself: it hands requests to the agent in the user's chat through
//! the composer's own send path, answers questions about its progress, and
//! passes on when the agent finishes or needs the user.
use super::*;
use crate::{composer::Composer, state::AppState, transcript::Transcript};
use gpui::{Entity, WeakEntity};
use serde_json::{Value, json};
use zeron_proto::view::Indicator;

/// What the assistant drives: the app state, and the window's composer and
/// transcript.
#[derive(Clone)]
pub(crate) struct AgentBridge {
    pub state: Entity<AppState>,
    pub composer: WeakEntity<Composer>,
    pub transcript: WeakEntity<Transcript>,
}

pub(super) const INSTRUCTIONS: &str = "You are the voice of Zeron, a desktop app where a coding \
agent works in the user's projects. You cannot read files, run commands or change code yourself. \
For any coding, repository, file, terminal or computer task, call ask_agent with one complete, \
self-contained instruction in the user's own terms, then say in a few words that the agent is on \
it. The agent works in the background; an automatic update arrives when it finishes or needs the \
user, and you then tell the user what happened in a sentence or two. Use agent_status to answer \
questions about its progress or what it said, summarizing rather than reading its reply out. \
Call stop_agent only when the user asks to stop it.";

/// The agent's reply, as much of it as the assistant reads.
const REPLY_CHARS: usize = 1500;
/// What an update quotes of the agent's reply.
const NEWS_CHARS: usize = 600;

pub(super) fn tools() -> Vec<call::Tool> {
    let no_arguments = json!({ "type": "object", "properties": {} });
    vec![
        call::Tool {
            name: "ask_agent".into(),
            description: "Send an instruction or question to the coding agent in the user's \
                          current chat. It works in the background; this returns at once."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "message": {
                        "type": "string",
                        "description": "The complete instruction, as the user would type it.",
                    },
                },
                "required": ["message"],
            }),
        },
        call::Tool {
            name: "agent_status".into(),
            description: "Whether the coding agent is working, waiting for the user or done, \
                          and its latest reply."
                .into(),
            parameters: no_arguments.clone(),
        },
        call::Tool {
            name: "stop_agent".into(),
            description: "Stop the coding agent's current run.".into(),
            parameters: no_arguments,
        },
    ]
}

/// `text` cut to at most `max` characters, on a word.
fn clip(text: &str, max: usize) -> String {
    let text = text.trim();
    if text.chars().count() <= max {
        return text.to_owned();
    }
    let cut: String = text.chars().take(max).collect();
    let cut = cut.rsplit_once(' ').map_or(cut.as_str(), |(head, _)| head);
    format!("{cut}…")
}

impl VoiceController {
    pub(super) fn agent_indicator(&self, agent: &AgentBridge, cx: &App) -> Indicator {
        self.agent_chat.as_deref().map_or(Indicator::None, |chat| {
            agent.state.read(cx).indicator_for(chat, chrono::Utc::now())
        })
    }

    /// The agent's newest reply in its chat, if it answered the latest prompt.
    fn latest_reply(&self, agent: &AgentBridge, cx: &App) -> Option<String> {
        let chat = self.agent_chat.as_deref()?;
        let state = agent.state.read(cx);
        if state.selected_chat.as_deref() == Some(chat)
            && let Some(reply) = agent
                .transcript
                .upgrade()
                .and_then(|t| t.read(cx).latest_reply())
        {
            return Some(reply);
        }
        state
            .chats
            .iter()
            .find(|c| c.id == chat)?
            .last_message_preview
            .clone()
    }

    /// The app changed: tell the user when the agent finishes or needs them.
    pub(super) fn agent_changed(&mut self, cx: &mut Context<Self>) {
        let Some(agent) = self.agent.clone() else {
            return;
        };
        let now = self.agent_indicator(&agent, cx);
        let before = std::mem::replace(&mut self.agent_seen, now);
        if before == now {
            return;
        }
        self.sync_work(cx);
        let reply = || {
            self.latest_reply(&agent, cx)
                .map(|r| format!(" Its reply: {}", clip(&r, NEWS_CHARS)))
                .unwrap_or_default()
        };
        let news = match (before, now) {
            (Indicator::Working | Indicator::AwaitingInput, Indicator::None) => {
                Some(format!("The coding agent finished.{}", reply()))
            }
            (_, Indicator::AwaitingInput) => Some(format!(
                "The coding agent is waiting for the user's answer in the chat.{}",
                reply()
            )),
            (_, Indicator::Errored) => Some("The coding agent stopped with an error.".to_owned()),
            _ => None,
        };
        if let Some(news) = news {
            self.command(call::Command::Announce(news), cx);
        }
    }

    pub(super) fn run_tool(&mut self, call: call::ToolCall, cx: &mut Context<Self>) {
        let answer = match self.agent.clone() {
            None => "No coding agent is connected.".to_owned(),
            Some(agent) => match call.name.as_str() {
                "ask_agent" => self.ask_agent(&agent, &call.arguments, cx),
                "agent_status" => self.status(&agent, cx),
                "stop_agent" => self.stop_agent(&agent, cx),
                other => format!("There is no tool called {other}."),
            },
        };
        let _ = call.reply.send(answer);
    }

    fn ask_agent(
        &mut self,
        agent: &AgentBridge,
        arguments: &Value,
        cx: &mut Context<Self>,
    ) -> String {
        let message = arguments
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim()
            .to_owned();
        if message.is_empty() {
            return "No instruction was given.".into();
        }
        let Some(composer) = agent.composer.upgrade() else {
            return "The chat is not open.".into();
        };
        match composer.update(cx, |composer, cx| composer.send_spoken(message, cx)) {
            Ok(chat) => {
                self.agent_chat = Some(chat);
                self.agent_seen = self.agent_indicator(agent, cx);
                self.sync_work(cx);
                "Sent. The agent is working on it in the background; an update will follow \
                 when it finishes."
                    .into()
            }
            Err(reason) => reason,
        }
    }

    fn status(&self, agent: &AgentBridge, cx: &App) -> String {
        let Some(chat) = self.agent_chat.as_deref() else {
            return "No chat is open and the agent has not been asked anything yet.".into();
        };
        let state = agent.state.read(cx);
        let title = state
            .chats
            .iter()
            .find(|c| c.id == chat)
            .and_then(|c| c.title.clone())
            .unwrap_or_else(|| "Untitled chat".into());
        let doing = match state.indicator_for(chat, chrono::Utc::now()) {
            Indicator::Working => "working",
            Indicator::AwaitingInput => "waiting for the user's answer",
            Indicator::Errored => "stopped with an error",
            Indicator::None => "idle",
        };
        let reply = self
            .latest_reply(agent, cx)
            .map(|r| format!(" Latest reply: {}", clip(&r, REPLY_CHARS)))
            .unwrap_or_default();
        format!("Chat “{title}”: the agent is {doing}.{reply}")
    }

    fn stop_agent(&mut self, agent: &AgentBridge, cx: &mut Context<Self>) -> String {
        let Some(chat) = self.agent_chat.clone() else {
            return "There is no agent run to stop.".into();
        };
        if self.agent_indicator(agent, cx) == Indicator::None {
            return "The agent is not running.".into();
        }
        let Some(composer) = agent.composer.upgrade() else {
            return "The chat is not open.".into();
        };
        composer.update(cx, |composer, cx| composer.interrupt_chat(chat, cx));
        "Asked the agent to stop.".into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replies_are_clipped_on_a_word() {
        assert_eq!(clip("  short ", 10), "short");
        assert_eq!(clip("one two three four", 9), "one two…");
        assert_eq!(clip("supercalifragilistic", 5), "super…");
    }

    #[test]
    fn tools_describe_their_arguments() {
        let tools = tools();
        let names: Vec<_> = tools.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, ["ask_agent", "agent_status", "stop_agent"]);
        assert_eq!(tools[0].parameters["required"][0], "message");
        assert_eq!(tools[1].parameters["type"], "object");
    }
}
