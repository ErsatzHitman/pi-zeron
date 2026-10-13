use serde_json::Value;
use std::collections::HashMap;
use zeron_proto::{AgentEvent, DoneStatus, ToolCall, ToolDiff};

#[derive(Default)]
pub(super) struct Normalizer {
    text: HashMap<usize, String>,
    thinking: HashMap<usize, String>,
    tools: HashMap<String, (String, Value)>,
    pub error: Option<String>,
    pub aborted: bool,
    pub window: Option<u64>,
}

/// A displayed custom message's chip input: its details, plus the message's
/// text as the row's title and lines (what the transcript keeps of it).
fn custom_input(details: &Value, text: &str) -> Option<Value> {
    let mut input = match details {
        Value::Object(details) => details.clone(),
        Value::Null => serde_json::Map::new(),
        other => {
            let mut map = serde_json::Map::new();
            map.insert("details".into(), other.clone());
            map
        }
    };
    let lines: Vec<&str> = text
        .lines()
        .map(str::trim_end)
        .filter(|line| !line.trim().is_empty())
        .collect();
    if let Some(first) = lines.first()
        && !input.contains_key("title")
    {
        let title = first.trim().trim_start_matches('#').trim();
        input.insert("title".into(), Value::from(title));
    }
    if !lines.is_empty() && !input.contains_key("lines") {
        input.insert("lines".into(), Value::from(lines));
    }
    (!input.is_empty()).then_some(Value::Object(input))
}

/// The text of a message's `content` (a string, or text parts).
fn content_text(content: &Value) -> String {
    match content {
        Value::String(text) => text.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter(|part| part["type"] == "text")
            .filter_map(|part| part["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}
impl Normalizer {
    pub fn reset(&mut self) {
        self.error = None;
        self.aborted = false;
        self.text.clear();
        self.thinking.clear();
    }
    pub fn status(&self) -> DoneStatus {
        if self.aborted {
            DoneStatus::Interrupted
        } else if self.error.is_some() {
            DoneStatus::Errored
        } else {
            DoneStatus::Completed
        }
    }
    pub fn map(&mut self, frame: &Value) -> Vec<AgentEvent> {
        let mut events = Vec::new();
        match string(frame, "type") {
            "message_start" if frame["message"]["role"] == "assistant" => {
                self.text.clear();
                self.thinking.clear();
            }
            // An extension's message meant for the user (`display: true`:
            // memory, workflow results, subagent deliveries) is a chip in the
            // transcript: its type, details and content.
            "message_end"
                if frame["message"]["role"] == "custom" && frame["message"]["display"] == true =>
            {
                let message = &frame["message"];
                let id = format!("pi-custom-{}", uuid::Uuid::new_v4());
                let text = content_text(&message["content"]);
                events.push(AgentEvent::ToolCall {
                    id: id.clone(),
                    call: ToolCall::Mcp {
                        server: zeron_proto::view::PI_EXTENSION_SERVER.into(),
                        tool: string(message, "customType").into(),
                        input: custom_input(&message["details"], &text),
                    },
                });
                events.push(AgentEvent::ToolResult {
                    id,
                    is_error: false,
                    output: (!text.is_empty()).then_some(text),
                    diff: None,
                });
            }
            "message_update" => {
                let update = &frame["assistantMessageEvent"];
                let index = update["contentIndex"].as_u64().unwrap_or(0) as usize;
                let delta = string(update, "delta");
                match string(update, "type") {
                    "text_delta" => {
                        self.text.entry(index).or_default().push_str(delta);
                        events.push(AgentEvent::TextDelta { text: delta.into() });
                    }
                    "thinking_delta" => {
                        self.thinking.entry(index).or_default().push_str(delta);
                        events.push(AgentEvent::ReasoningDelta { text: delta.into() });
                    }
                    _ => {}
                }
            }
            "message_end" if frame["message"]["role"] == "assistant" => {
                let message = &frame["message"];
                self.error = (message["stopReason"] == "error").then(|| {
                    message["errorMessage"]
                        .as_str()
                        .unwrap_or("Pi provider failed")
                        .to_owned()
                });
                self.aborted = message["stopReason"] == "aborted";
                if let Some(content) = message["content"].as_array() {
                    for (index, part) in content.iter().enumerate() {
                        let (cache, value, reasoning) = match string(part, "type") {
                            "text" => (&mut self.text, string(part, "text"), false),
                            "thinking" => (&mut self.thinking, string(part, "thinking"), true),
                            _ => continue,
                        };
                        let previous = cache.entry(index).or_default();
                        if let Some(suffix) = value
                            .strip_prefix(previous.as_str())
                            .filter(|s| !s.is_empty())
                        {
                            events.push(if reasoning {
                                AgentEvent::ReasoningDelta {
                                    text: suffix.into(),
                                }
                            } else {
                                AgentEvent::TextDelta {
                                    text: suffix.into(),
                                }
                            });
                        }
                        *previous = value.into();
                    }
                }
                let usage = &message["usage"];
                if usage.is_object() {
                    let n = |key| usage[key].as_u64().unwrap_or(0);
                    let input = n("input") + n("cacheRead") + n("cacheWrite");
                    events.push(AgentEvent::Usage {
                        input_tokens: input,
                        output_tokens: n("output"),
                    });
                    events.push(AgentEvent::ContextUsage {
                        tokens: Some(input + n("output")),
                        window: self.window,
                    });
                }
            }
            "tool_execution_start" => {
                let id = string(frame, "toolCallId").to_owned();
                let name = string(frame, "toolName");
                if !self.tools.contains_key(&id) {
                    let args = frame["args"].clone();
                    events.push(AgentEvent::ToolCall {
                        id: id.clone(),
                        call: tool(name, &args),
                    });
                    self.tools.insert(id, (name.into(), args));
                }
            }
            "tool_execution_end" => {
                let id = string(frame, "toolCallId").to_owned();
                let info = self.tools.remove(&id);
                let result = &frame["result"];
                let output = result["content"].as_array().map(|items| {
                    items
                        .iter()
                        .filter_map(|v| v["text"].as_str())
                        .collect::<Vec<_>>()
                        .join("\n")
                });
                let diff = info.as_ref().and_then(|(name, args)| {
                    if frame["isError"] == true {
                        return None;
                    }
                    match name.as_str() {
                        "edit" => Some(ToolDiff {
                            path: string(args, "path").into(),
                            old_text: Some(cap(&edit_text(args, "oldText"), 65536)),
                            new_text: cap(&edit_text(args, "newText"), 65536),
                        }),
                        "write" => Some(ToolDiff {
                            path: string(args, "path").into(),
                            old_text: None,
                            new_text: cap(string(args, "content"), 65536),
                        }),
                        _ => None,
                    }
                });
                events.push(AgentEvent::ToolResult {
                    id,
                    is_error: frame["isError"].as_bool().unwrap_or(false),
                    output: output.map(|s| cap(&s, 16384)),
                    diff,
                });
            }
            "compaction_end" => {
                if let Some(tokens) = frame["result"]["estimatedTokensAfter"].as_u64() {
                    events.push(AgentEvent::ContextUsage {
                        tokens: Some(tokens),
                        window: self.window,
                    });
                }
            }
            // Pi reports an aborted run here even when no assistant message
            // carried `stopReason: "aborted"` (aborted before the first token).
            "agent_settled" if frame["aborted"] == true => self.aborted = true,
            "extension_error" => {
                self.error = Some(string(frame, "error").into());
                events.push(AgentEvent::Error {
                    message: format!("Pi extension: {}", string(frame, "error")),
                });
            }
            _ => {}
        }
        events
    }
}
pub(super) fn string<'a>(v: &'a Value, key: &str) -> &'a str {
    v[key].as_str().unwrap_or("")
}
fn cap(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.into();
    }
    let mut at = limit;
    while !text.is_char_boundary(at) {
        at -= 1;
    }
    format!("{}\n… [truncated]", &text[..at])
}
/// Pi's `edit` takes `edits: [{oldText, newText}]`, applied against the
/// original file. The transcript shows them as one change, in order.
fn edit_text(args: &Value, key: &str) -> String {
    args["edits"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|edit| string(edit, key))
        .collect::<Vec<_>>()
        .join("\n")
}
fn tool(name: &str, args: &Value) -> ToolCall {
    match name {
        "bash" | "powershell" => ToolCall::Exec {
            command: string(args, "command").into(),
        },
        "read" => ToolCall::ReadFile {
            path: string(args, "path").into(),
        },
        "write" => ToolCall::WriteFile {
            path: string(args, "path").into(),
            content: args["content"].as_str().map(str::to_owned),
        },
        "edit" => ToolCall::EditFile {
            path: string(args, "path").into(),
            old_string: Some(edit_text(args, "oldText")),
            new_string: Some(edit_text(args, "newText")),
        },
        "grep" => ToolCall::Search {
            pattern: string(args, "pattern").into(),
            path: args["path"].as_str().map(str::to_owned),
        },
        "find" => ToolCall::Glob {
            pattern: string(args, "pattern").into(),
        },
        name if name.starts_with("zeron_") => ToolCall::Mcp {
            server: "zeron".into(),
            tool: name[6..].into(),
            input: Some(args.clone()),
        },
        _ => ToolCall::Unknown {
            name: name.into(),
            input: Some(args.clone()),
        },
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn final_message_reconciles_without_duplicate_text_and_retry_clears_error() {
        let mut n = Normalizer::default();
        n.map(&json!({"type":"message_update","assistantMessageEvent":{"type":"text_delta","contentIndex":0,"delta":"hel"}}));
        let events = n.map(&json!({"type":"message_end","message":{"role":"assistant","stopReason":"error","errorMessage":"temporary","content":[{"type":"text","text":"hello"}]}}));
        assert!(matches!(&events[0], AgentEvent::TextDelta {text} if text == "lo"));
        assert_eq!(n.status(), DoneStatus::Errored);
        n.map(&json!({"type":"message_end","message":{"role":"assistant","stopReason":"stop","content":[]}}));
        assert_eq!(n.status(), DoneStatus::Completed);
    }
    #[test]
    fn tools_preserve_identity_and_edit_diff() {
        let mut n = Normalizer::default();
        let start = json!({"type":"tool_execution_start","toolCallId":"t","toolName":"edit","args":{"path":"a","edits":[{"oldText":"old","newText":"new"},{"oldText":"x","newText":"y"}]}});
        assert_eq!(n.map(&start).len(), 1);
        assert!(n.map(&start).is_empty());
        let events=n.map(&json!({"type":"tool_execution_end","toolCallId":"t","isError":false,"result":{"content":[{"type":"text","text":"ok"}]}}));
        assert!(
            matches!(&events[0],AgentEvent::ToolResult{id,diff:Some(diff),..} if id=="t" && diff.new_text=="new\ny" && diff.old_text.as_deref()==Some("old\nx"))
        );
    }
    #[test]
    fn settled_abort_without_an_assistant_message_is_an_interruption() {
        let mut n = Normalizer::default();
        n.map(&json!({"type":"agent_settled","aborted":true}));
        assert_eq!(n.status(), DoneStatus::Interrupted);
    }

    #[test]
    fn displayed_custom_messages_become_chips_and_hidden_ones_vanish() {
        let mut n = Normalizer::default();
        let events = n.map(&json!({"type":"message_end","message":{"role":"custom",
            "customType":"workflow-result","display":true,"details":{"runId":"r1","status":"completed"},
            "content":[{"type":"text","text":"# Done"},{"type":"image","data":""}]}}));
        assert_eq!(events.len(), 2);
        let AgentEvent::ToolCall {
            id,
            call:
                ToolCall::Mcp {
                    server,
                    tool,
                    input,
                },
        } = &events[0]
        else {
            panic!("{events:?}");
        };
        assert!(id.starts_with("pi-custom-"));
        assert_eq!((server.as_str(), tool.as_str()), ("pi", "workflow-result"));
        assert_eq!(input.as_ref().unwrap()["status"], "completed");
        let AgentEvent::ToolResult { output, .. } = &events[1] else {
            panic!("{events:?}");
        };
        assert_eq!(output.as_deref(), Some("# Done"));
        assert!(
            n.map(&json!({"type":"message_end","message":{"role":"custom",
            "customType":"jev-decompose","display":false,"content":"x"}}))
                .is_empty()
        );
    }
}
