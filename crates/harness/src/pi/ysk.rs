//! You should know: a second pair of eyes on long Pi turns, modeled on
//! Claude Code's "You should know". Every [`EVERY`] tool steps of a turn a
//! throwaway reviewer Pi (no session, tools or extensions; the chat's model)
//! reads the conversation so far and, when something clears its bar, Zeron
//! docks one note above the composer: "Heads up" (a decision the agent made
//! without saying so) or "You should know" (how something works that
//! matters). Learn more opens it, Knew this already silences its topic in
//! this chat, Dismiss closes it, Chat in main session puts it in the
//! composer. A note left unanswered goes on the second prompt, and ignored
//! notes back reviews off (skip 1, 2, 4, up to 16). Off unless Settings →
//! General → Pi → You should know is on; the agent never waits for a review.
use std::path::Path;
use std::sync::Arc;

use serde::Deserialize;
use serde_json::{Value, json};

use super::{PiHarness, normalize};
use crate::HarnessError;
use zeron_proto::AgentEvent;

/// The reviewer's whole system prompt.
const INSTRUCTIONS: &str = include_str!("ysk.md");
/// A review every this many tool steps of a turn.
pub(super) const EVERY: u32 = 6;
/// The longest backoff, in skipped review slots.
const MAX_SKIP: u32 = 16;
/// How much of the conversation (from its end) the reviewer reads.
const TRANSCRIPT_BUDGET: usize = 40_000;
/// One tool result or argument list, at most.
const PART_BUDGET: usize = 1_200;
pub(super) const NS: &str = "zeron";
pub(super) const ID: &str = "ysk";
/// The element key a UI action names.
pub(super) const KEY: &str = "zeron:ysk";

/// Settings → General → Pi → You should know, read at each review.
pub type YskSetting = Arc<dyn Fn() -> bool + Send + Sync>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum Kind {
    HeadsUp,
    YouShouldKnow,
    None,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Note {
    pub kind: Kind,
    pub topic: String,
    pub title: String,
    pub body: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Shown {
    note: Note,
    expanded: bool,
    /// Prompts sent since it appeared, unanswered.
    prompts: u32,
}

/// What a note's action asks of the runner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Effect {
    /// Not a note action, or one for a note already gone.
    Ignored,
    /// The note changed or closed: publish it.
    Changed,
    /// Closed; put this text in the composer.
    Editor(String),
}

/// One chat's reviews: step count, backoff, known topics and the note.
#[derive(Debug, Default)]
pub(super) struct Ysk {
    steps: u32,
    in_flight: bool,
    skip: u32,
    /// The next backoff after an ignored note (0: none yet).
    backoff: u32,
    known: Vec<String>,
    shown: Option<Shown>,
}

impl Ysk {
    /// A turn started: its steps count from zero.
    pub fn turn_started(&mut self) {
        self.steps = 0;
    }

    /// A tool step ended; true when a review is due now.
    pub fn step(&mut self) -> bool {
        self.steps += 1;
        if self.steps % EVERY != 0 || self.in_flight || self.shown.is_some() {
            return false;
        }
        if self.skip > 0 {
            self.skip -= 1;
            return false;
        }
        self.in_flight = true;
        true
    }

    /// The review ended without a note (failed, or nothing cleared the bar).
    pub fn review_failed(&mut self) {
        self.in_flight = false;
    }

    /// A review answered; true when a note is now shown.
    pub fn answered(&mut self, note: Option<Note>) -> bool {
        self.in_flight = false;
        let Some(note) = note.filter(|n| n.kind != Kind::None && !n.title.trim().is_empty()) else {
            return false;
        };
        if self.shown.is_some() || self.known.iter().any(|t| *t == note.topic) {
            return false;
        }
        self.shown = Some(Shown {
            note,
            expanded: false,
            prompts: 0,
        });
        true
    }

    /// The user sent a prompt; true when an unanswered note went away (the
    /// second prompt), which backs reviews off.
    pub fn prompt_sent(&mut self) -> bool {
        let Some(shown) = &mut self.shown else {
            return false;
        };
        shown.prompts += 1;
        if shown.prompts < 2 {
            return false;
        }
        self.shown = None;
        self.backoff = if self.backoff == 0 {
            1
        } else {
            (self.backoff * 2).min(MAX_SKIP)
        };
        self.skip = self.backoff;
        true
    }

    /// A note action (`more`, `understood`, `knew`, `dismiss`, `chat`).
    pub fn action(&mut self, action: &str) -> Effect {
        let Some(shown) = &mut self.shown else {
            return Effect::Ignored;
        };
        match action {
            "more" => {
                shown.expanded = true;
                return Effect::Changed;
            }
            "dismiss" => {}
            "understood" => self.backoff = 0,
            "knew" => {
                self.backoff = 0;
                self.known.push(shown.note.topic.clone());
            }
            "chat" => {
                self.backoff = 0;
                let note = self.shown.take().expect("shown").note;
                return Effect::Editor(format!(
                    "{}: {}\n\n{}\n\n",
                    label(note.kind),
                    note.title,
                    note.body
                ));
            }
            _ => return Effect::Ignored,
        }
        self.shown = None;
        Effect::Changed
    }

    /// Topics the user knows, for the reviewer.
    pub fn known(&self) -> &[String] {
        &self.known
    }

    /// The pinned element for the note, if one is shown.
    pub fn element(&self) -> Option<Value> {
        let shown = self.shown.as_ref()?;
        let note = &shown.note;
        let action = |id: &str, label: &str| json!({"id":id,"label":label});
        let (text, actions) = if shown.expanded {
            (
                format!("**{}**\n\n{}", note.title, note.body),
                vec![
                    action("understood", "Understood"),
                    action("chat", "Chat in main session"),
                    action("knew", "Knew this already"),
                ],
            )
        } else {
            (
                format!("**{}**", note.title),
                vec![
                    action("more", "Learn more"),
                    action("knew", "Knew this already"),
                    action("dismiss", "Dismiss"),
                ],
            )
        };
        Some(json!({
            "id":ID, "ns":NS, "kind":"panel", "placement":"pinned",
            "title":label(note.kind),
            "sections":[{"kind":"markdown","id":"note","text":text}],
            "actions":actions,
        }))
    }
}

fn label(kind: Kind) -> &'static str {
    match kind {
        Kind::HeadsUp => "Heads up",
        _ => "You should know",
    }
}

/// The reviewer's answer as a note; anything unreadable is none.
pub(super) fn parse(answer: &str) -> Option<Note> {
    #[derive(Deserialize)]
    struct Answer {
        kind: Kind,
        #[serde(default)]
        topic: String,
        #[serde(default)]
        title: String,
        #[serde(default)]
        body: String,
    }
    let start = answer.find('{')?;
    let end = answer.rfind('}')?;
    let answer: Answer = serde_json::from_str(answer.get(start..=end)?).ok()?;
    if answer.kind == Kind::None {
        return None;
    }
    let topic = answer.topic.trim().to_lowercase();
    Some(Note {
        kind: answer.kind,
        topic: if topic.is_empty() {
            answer.title.trim().to_lowercase()
        } else {
            topic
        },
        title: answer.title.trim().chars().take(120).collect(),
        body: answer.body.trim().to_owned(),
    })
}

fn clip(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_owned();
    }
    let mut out: String = text.chars().take(max).collect();
    out.push_str(" …");
    out
}

/// Text parts of a Pi message's `content` (a string or a part list).
fn content_text(content: &Value) -> String {
    match content {
        Value::String(text) => text.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|part| match part["type"].as_str() {
                Some("text") => part["text"].as_str().map(str::to_owned),
                Some("toolCall") => Some(format!(
                    "[tool {} {}]",
                    part["name"].as_str().unwrap_or("?"),
                    clip(&part["arguments"].to_string(), PART_BUDGET)
                )),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// The conversation from `get_messages`, newest last, within the budget.
pub(super) fn transcript(messages: &Value) -> String {
    let mut lines: Vec<String> = messages
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|message| {
            let text = content_text(&message["content"]);
            let line = match message["role"].as_str()? {
                "user" => format!("USER: {text}"),
                "assistant" => format!("AGENT: {text}"),
                "toolResult" => format!(
                    "RESULT {}{}: {}",
                    message["toolName"].as_str().unwrap_or("?"),
                    if message["isError"] == true {
                        " (error)"
                    } else {
                        ""
                    },
                    clip(&text, PART_BUDGET)
                ),
                _ => return None,
            };
            (!text.trim().is_empty()).then_some(line)
        })
        .collect();
    let mut total = 0;
    let mut keep = lines.len();
    while keep > 0 && total + lines[keep - 1].len() <= TRANSCRIPT_BUDGET {
        keep -= 1;
        total += lines[keep].len();
    }
    let dropped = keep;
    let mut out = lines.split_off(keep);
    if dropped > 0 {
        out.insert(0, format!("({dropped} earlier messages left out)"));
    }
    out.join("\n\n")
}

/// The reviewer's request: the conversation and the topics already known.
pub(super) fn request(messages: &Value, known: &[String]) -> String {
    let known = if known.is_empty() {
        "none".to_owned()
    } else {
        known.join(", ")
    };
    format!(
        "Topics the user already knows: {known}\n\nThe conversation so far:\n\n{}\n\nAnswer with the JSON object only.",
        transcript(messages)
    )
}

/// Ask a throwaway Pi with only the reviewer instructions; its answer text.
pub(super) async fn review(
    harness: Arc<PiHarness>,
    cwd: String,
    model: Option<String>,
    prompt: String,
) -> Result<String, HarnessError> {
    let mut args: Vec<String> = [
        "--no-session",
        "--no-tools",
        "--no-extensions",
        "--no-skills",
        "--no-prompt-templates",
        "--no-context-files",
        "--thinking",
        "off",
        "--system-prompt",
        INSTRUCTIONS,
    ]
    .map(String::from)
    .into();
    if let Some(model) = model.filter(|m| m != "default") {
        args.extend(["--model".into(), model]);
    }
    if let Some(ledger) = super::usage_ledger(harness.agent_dir.clone()) {
        args.extend(["-e".into(), ledger.to_string_lossy().into_owned()]);
    }
    let mut process = harness.spawn(
        Path::new(&cwd),
        &args,
        None,
        false,
        Some("zeron-ysk"),
        false,
    )?;
    let result = async {
        let mut backlog = vec![];
        process
            .query(json!({"type":"prompt","message":prompt}), &mut backlog)
            .await?;
        let mut norm = normalize::Normalizer::default();
        let mut text = String::new();
        let mut backlog = std::collections::VecDeque::from(backlog);
        loop {
            let frame = match backlog.pop_front() {
                Some(frame) => frame,
                None => process.next().await?,
            };
            if frame["type"] == "agent_settled" {
                break;
            }
            for event in norm.map(&frame) {
                if let AgentEvent::TextDelta { text: delta } = event {
                    text.push_str(&delta);
                }
            }
        }
        Ok(text)
    }
    .await;
    process.shutdown(harness.kill_grace).await;
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn note(topic: &str) -> Note {
        Note {
            kind: Kind::HeadsUp,
            topic: topic.into(),
            title: "The agent skipped the migration".into(),
            body: "It left db/002.sql unapplied.".into(),
        }
    }

    #[test]
    fn a_review_is_due_every_sixth_step_one_at_a_time() {
        let mut ysk = Ysk::default();
        let due: Vec<bool> = (0..12).map(|_| ysk.step()).collect();
        // The sixth step starts a review; the twelfth waits for it.
        assert_eq!(due.iter().filter(|d| **d).count(), 1);
        assert!(due[5]);
        ysk.review_failed();
        ysk.turn_started();
        assert!(!(0..5).any(|_| ysk.step()));
        assert!(ysk.step());
    }

    #[test]
    fn the_answer_parses_or_is_none() {
        let ok = parse(
            "```json\n{\"kind\":\"you_should_know\",\"topic\":\"Rate-Limit\",\"title\":\"t\",\"body\":\"b\"}\n```",
        )
        .unwrap();
        assert_eq!(ok.kind, Kind::YouShouldKnow);
        assert_eq!(ok.topic, "rate-limit");
        assert_eq!(parse("{\"kind\":\"none\"}"), None);
        assert_eq!(parse("nothing to say"), None);
        assert_eq!(parse("{\"kind\":\"other\"}"), None);
    }

    #[test]
    fn knew_this_silences_the_topic_and_chat_fills_the_composer() {
        let mut ysk = Ysk::default();
        assert!(ysk.answered(Some(note("migration"))));
        let element = ysk.element().unwrap();
        assert_eq!(element["title"], "Heads up");
        assert_eq!(element["placement"], "pinned");
        assert_eq!(ysk.action("more"), Effect::Changed);
        let text = ysk.element().unwrap()["sections"][0]["text"].clone();
        assert!(text.as_str().unwrap().contains("db/002.sql"));
        assert_eq!(ysk.action("knew"), Effect::Changed);
        assert!(ysk.element().is_none());
        assert!(!ysk.answered(Some(note("migration"))), "a known topic");
        assert!(ysk.answered(Some(note("other"))));
        match ysk.action("chat") {
            Effect::Editor(text) => assert!(text.starts_with("Heads up: The agent skipped")),
            other => panic!("{other:?}"),
        }
        assert_eq!(ysk.action("dismiss"), Effect::Ignored);
    }

    #[test]
    fn ignored_notes_go_on_the_second_prompt_and_back_reviews_off() {
        let mut ysk = Ysk::default();
        let mut skips = vec![];
        for _ in 0..6 {
            ysk.answered(Some(note("x")));
            assert!(!ysk.prompt_sent());
            assert!(ysk.prompt_sent());
            skips.push(ysk.skip);
            ysk.skip = 0;
        }
        assert_eq!(skips, [1, 2, 4, 8, 16, 16]);
        ysk.answered(Some(note("x")));
        ysk.action("understood");
        assert_eq!(ysk.backoff, 0);
    }

    #[test]
    fn the_transcript_keeps_the_newest_messages() {
        let big = "x".repeat(30_000);
        let messages = json!([
            {"role":"user","content":big},
            {"role":"assistant","content":[{"type":"text","text":"Plan"},{"type":"toolCall","name":"bash","arguments":{"command":"ls"}}]},
            {"role":"toolResult","toolName":"bash","isError":true,"content":[{"type":"text","text":"denied"}]},
            {"role":"user","content":big},
        ]);
        let text = transcript(&messages);
        assert!(text.starts_with("(1 earlier messages left out)"));
        assert!(text.contains("[tool bash {\"command\":\"ls\"}]"));
        assert!(text.contains("RESULT bash (error): denied"));
        assert!(request(&messages, &["a".into()]).starts_with("Topics the user already knows: a"));
    }
}
