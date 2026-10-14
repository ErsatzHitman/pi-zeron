//! What voice mode reads aloud: the agent's own messages in the chat its
//! prompts go to, as they are written. The notes the agent leaves between
//! tool calls and its final answer are spoken a sentence at a time; tool
//! calls, thinking, code and history from before the call stay on screen.
use std::collections::{HashMap, HashSet};
use zeron_doc::{MessagePart, MessageRole, MessageStatus, SessionMessageEntry};
use zeron_voice::call;

/// About this many characters of one message are spoken; the rest is left
/// to the chat.
const MESSAGE_CHARS: usize = 1200;

/// One sentence to say, and the message part it comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Line {
    pub item: String,
    pub text: String,
}

/// Follows one chat's transcript and yields what is new to say.
pub(crate) struct Narrator {
    /// The prompt voice just sent here: what the agent writes after it is
    /// read even if the first copy of the chat arrives late.
    anchor: Option<String>,
    started: bool,
    /// Parts already read, or never to be read.
    done: HashSet<String>,
    /// Text parts still being written.
    reading: HashMap<String, Reading>,
}

struct Reading {
    reader: call::Reader,
    /// Bytes of the part already given to the reader.
    read: usize,
}

impl Narrator {
    pub fn new(anchor: Option<String>) -> Self {
        Self {
            anchor,
            started: false,
            done: HashSet::new(),
            reading: HashMap::new(),
        }
    }

    /// The chat's current transcript; returns the sentences to say now.
    pub fn update(&mut self, entries: &[SessionMessageEntry]) -> Vec<Line> {
        if !self.started {
            self.started = true;
            // Everything already there is history, except what answers the
            // prompt just sent.
            let history = self
                .anchor
                .as_ref()
                .and_then(|anchor| entries.iter().position(|e| &e.id == anchor))
                .map_or(entries.len(), |at| at + 1);
            for entry in &entries[..history] {
                for part in &entry.parts {
                    self.done.insert(key(entry, part));
                }
            }
        }
        let mut lines = Vec::new();
        for (index, entry) in entries.iter().enumerate() {
            if entry.role != MessageRole::Assistant {
                continue;
            }
            let followed = index + 1 < entries.len();
            for (at, part) in entry.parts.iter().enumerate() {
                let key = key(entry, part);
                if self.done.contains(&key) {
                    continue;
                }
                // A part is finished once anything follows it.
                let finished = followed
                    || at + 1 < entry.parts.len()
                    || entry.status != Some(MessageStatus::Streaming);
                let said = match part {
                    MessagePart::Text { text, .. } => {
                        let reading = self.reading.entry(key.clone()).or_insert_with(|| Reading {
                            reader: call::Reader::new(MESSAGE_CHARS),
                            read: 0,
                        });
                        let mut said = reading
                            .reader
                            .push(text.get(reading.read..).unwrap_or_default());
                        reading.read = reading.read.max(text.len());
                        if finished {
                            said.extend(reading.reader.finish());
                            self.reading.remove(&key);
                            self.done.insert(key.clone());
                        }
                        said
                    }
                    MessagePart::Input {
                        questions,
                        resolved,
                        ..
                    } => {
                        self.done.insert(key.clone());
                        if *resolved {
                            Vec::new()
                        } else {
                            questions
                                .iter()
                                .map(|q| {
                                    let asked = if q.question.trim().is_empty() {
                                        &q.header
                                    } else {
                                        &q.question
                                    };
                                    format!("The agent asks: {}", asked.trim())
                                })
                                .chain(["Answer it in the chat.".to_owned()])
                                .collect()
                        }
                    }
                    MessagePart::Error { .. } => {
                        self.done.insert(key.clone());
                        vec!["The agent stopped with an error. The details are in the chat.".into()]
                    }
                    // Tool calls, thinking, images and fork markers are not read.
                    _ => {
                        self.done.insert(key.clone());
                        Vec::new()
                    }
                };
                lines.extend(said.into_iter().map(|text| Line {
                    item: key.clone(),
                    text,
                }));
            }
        }
        lines
    }
}

fn key(entry: &SessionMessageEntry, part: &MessagePart) -> String {
    format!("{}/{}", entry.id, part.id())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(
        id: &str,
        role: MessageRole,
        parts: Vec<MessagePart>,
        streaming: bool,
    ) -> SessionMessageEntry {
        SessionMessageEntry {
            id: id.into(),
            role,
            parts,
            created_at: 0,
            device_id: "device".into(),
            status: Some(if streaming {
                MessageStatus::Streaming
            } else {
                MessageStatus::Complete
            }),
            continuation_of: None,
            duration_ms: None,
        }
    }

    fn text(id: &str, text: &str) -> MessagePart {
        MessagePart::Text {
            id: id.into(),
            text: text.into(),
        }
    }

    fn tool(id: &str) -> MessagePart {
        MessagePart::Tool {
            id: id.into(),
            call: zeron_proto::ToolCall::ReadFile {
                path: "src/invoice.css".into(),
            },
            is_error: false,
            resolved: true,
            output: None,
            diff: None,
            output_ref: None,
            output_bytes: None,
            diff_ref: None,
            diff_stats: None,
            subagent_ref: None,
            subagent_status: None,
            subagent_tail: None,
        }
    }

    fn said(lines: Vec<Line>) -> Vec<String> {
        lines.into_iter().map(|l| l.text).collect()
    }

    #[test]
    fn progress_notes_and_the_final_answer_are_read_as_they_are_written() {
        let prompt = entry(
            "u1",
            MessageRole::User,
            vec![text("p", "Change the invoice color from red to blue.")],
            false,
        );
        let mut narrator = Narrator::new(Some("u1".into()));
        assert!(narrator.update(std::slice::from_ref(&prompt)).is_empty());

        // The first note streams in; whole sentences are read at once.
        let mut reply = entry(
            "a1",
            MessageRole::Assistant,
            vec![text(
                "t1",
                "I'm looking through the invoice component. To find",
            )],
            true,
        );
        let mut chat = vec![prompt, reply.clone()];
        assert_eq!(
            said(narrator.update(&chat)),
            ["I'm looking through the invoice component."]
        );

        // A tool call finishes the note; the tool itself is not read.
        reply.parts = vec![
            text(
                "t1",
                "I'm looking through the invoice component. To find the color.",
            ),
            tool("c1"),
        ];
        chat[1] = reply.clone();
        assert_eq!(said(narrator.update(&chat)), ["To find the color."]);
        assert!(narrator.update(&chat).is_empty(), "nothing is read twice");

        // More work, then the answer, with its code left on screen.
        reply.parts.extend([
            text(
                "t2",
                "I found the variable. I'll update it from red to blue.",
            ),
            tool("c2"),
            text(
                "t3",
                "Done! The invoice is blue now.\n\n```css\n.invoice { color: blue; }\n```",
            ),
        ]);
        reply.status = Some(MessageStatus::Complete);
        chat[1] = reply;
        let lines = narrator.update(&chat);
        assert_eq!(
            said(lines.clone()),
            [
                "I found the variable.",
                "I'll update it from red to blue.",
                "Done! The invoice is blue now.",
            ]
        );
        assert_eq!(lines[0].item, "a1/t2");
        assert_eq!(lines[2].item, "a1/t3");
    }

    #[test]
    fn history_from_before_the_call_is_not_read() {
        let old = entry(
            "a0",
            MessageRole::Assistant,
            vec![text("t", "An old answer that was already on screen.")],
            false,
        );
        // Voice started on this chat: everything there is history.
        let mut narrator = Narrator::new(None);
        assert!(narrator.update(std::slice::from_ref(&old)).is_empty());
        let next = entry(
            "a1",
            MessageRole::Assistant,
            vec![text("t", "A new note after the call began.")],
            false,
        );
        assert_eq!(
            said(narrator.update(&[old.clone(), next])),
            ["A new note after the call began."]
        );

        // Voice moved to this chat with a prompt: its reply is read even if
        // the agent wrote it before the chat's first copy arrived.
        let prompt = entry("u1", MessageRole::User, vec![text("p", "Go")], false);
        let mut reply = entry(
            "a2",
            MessageRole::Assistant,
            vec![text("t", "Already working on it.")],
            true,
        );
        let mut narrator = Narrator::new(Some("u1".into()));
        let mut chat = vec![old, prompt, reply.clone()];
        assert!(
            narrator.update(&chat).is_empty(),
            "a streaming sentence waits for its end"
        );
        reply.status = Some(MessageStatus::Complete);
        chat[2] = reply;
        assert_eq!(said(narrator.update(&chat)), ["Already working on it."]);
    }

    #[test]
    fn questions_and_errors_are_announced() {
        let question: zeron_proto::UserInputQuestion = serde_json::from_value(serde_json::json!({
            "id": "q",
            "header": "Color",
            "question": "Which blue should I use?",
            "options": ["Navy", "Sky"],
        }))
        .unwrap();
        let mut narrator = Narrator::new(None);
        narrator.update(&[]);
        let asking = entry(
            "a1",
            MessageRole::Assistant,
            vec![
                MessagePart::Input {
                    id: "i".into(),
                    request_id: "r".into(),
                    questions: vec![question],
                    resolved: false,
                },
                MessagePart::Error {
                    id: "e".into(),
                    message: "exit status 1".into(),
                },
            ],
            false,
        );
        assert_eq!(
            said(narrator.update(&[asking])),
            [
                "The agent asks: Which blue should I use?",
                "Answer it in the chat.",
                "The agent stopped with an error. The details are in the chat.",
            ]
        );
    }
}
