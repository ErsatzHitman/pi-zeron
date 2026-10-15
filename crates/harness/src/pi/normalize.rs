use crate::redact::strip_ansi;
use serde_json::Value;
use std::collections::HashMap;
use zeron_proto::{AgentEvent, DoneStatus, TodoItem, TodoStatus, ToolCall, ToolDiff};

/// The most images one `share_images` call shows (the extension's own cap).
const SHARED_IMAGES_MAX: usize = 8;

#[derive(Default)]
pub(super) struct Normalizer {
    text: HashMap<usize, String>,
    thinking: HashMap<usize, String>,
    tools: HashMap<String, (String, Value)>,
    pub error: Option<String>,
    pub aborted: bool,
    pub window: Option<u64>,
}

/// A displayed custom message's chip: its input (the details, with a title
/// from the text's first line unless the details name one) and the body the
/// row expands to. Details that carry their own `lines` are the row's body,
/// so the text (often written for the model) is not shown twice.
fn custom_input(details: &Value, text: &str) -> (Option<Value>, Option<String>) {
    let mut input = match details {
        Value::Object(details) => details.clone(),
        Value::Null => serde_json::Map::new(),
        other => {
            let mut map = serde_json::Map::new();
            map.insert("details".into(), other.clone());
            map
        }
    };
    let mut body = text.trim_matches('\n');
    if !input.contains_key("title")
        && let Some(first) = body.lines().find(|line| !line.trim().is_empty())
    {
        let title = first.trim().trim_start_matches('#').trim();
        input.insert("title".into(), Value::from(title));
        let start = body.find(first).unwrap_or(0) + first.len();
        body = body[start..].trim_matches('\n');
    }
    let body = (!input.contains_key("lines") && !body.trim().is_empty()).then(|| body.to_owned());
    ((!input.is_empty()).then_some(Value::Object(input)), body)
}

/// Whether a displayed custom message reports a failure (a failed subagent,
/// a lost background shell).
fn custom_failed(details: &Value) -> bool {
    details["isError"] == true
        || matches!(
            details["state"].as_str().or(details["status"].as_str()),
            Some("failed" | "error" | "lost")
        )
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
                // Extensions colour their terminal output; a GUI shows text.
                let text = strip_ansi(&content_text(&message["content"]));
                let (input, body) = custom_input(&message["details"], &text);
                events.push(AgentEvent::ToolCall {
                    id: id.clone(),
                    call: ToolCall::Mcp {
                        server: zeron_proto::view::PI_EXTENSION_SERVER.into(),
                        tool: string(message, "customType").into(),
                        input,
                    },
                });
                events.push(AgentEvent::ToolResult {
                    id,
                    is_error: custom_failed(&message["details"]),
                    output: body.map(|body| cap(&body, 16384)),
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
                    // Pi's own footer (getAssistantUsage): an aborted or
                    // failed reply, or one that reports no tokens (a retried
                    // 429, a stop before the first token), says nothing
                    // about the context. `None` keeps the last measurement.
                    let tokens = Some(n("totalTokens"))
                        .filter(|t| *t > 0)
                        .unwrap_or(input + n("output"));
                    let measured = tokens > 0
                        && !matches!(message["stopReason"].as_str(), Some("aborted" | "error"));
                    events.push(AgentEvent::ContextUsage {
                        tokens: measured.then_some(tokens),
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
                // The todo extension's list is the checklist Zeron keeps above
                // the composer: the call becomes a `Todo` once its result says
                // what the list now is.
                if let Some(items) = info
                    .as_ref()
                    .filter(|(name, _)| name == "todo" && frame["isError"] != true)
                    .and_then(|_| todo_items(&result["details"]))
                {
                    events.push(AgentEvent::ToolCall {
                        id: id.clone(),
                        call: ToolCall::Todo { items },
                    });
                }
                // A page the agent published with Zeron's `html_render`
                // shows in the reply, like a generated image.
                let page = info
                    .as_ref()
                    .filter(|(name, _)| name.ends_with("_html_render") && frame["isError"] != true)
                    .and_then(|_| {
                        let page = &result["details"]["page"];
                        Some(AgentEvent::GeneratedImage {
                            id: format!("{id}:page"),
                            path: page["path"].as_str().filter(|p| !p.is_empty())?.into(),
                            name: page["title"].as_str().unwrap_or("Page").into(),
                            mime_type: "text/html".into(),
                        })
                    });
                // Images the agent chose to show with `share_images` follow
                // the call in the reply, as thumbnails.
                let shared: Vec<AgentEvent> = info
                    .as_ref()
                    .filter(|(name, _)| name == "share_images" && frame["isError"] != true)
                    .and_then(|_| result["details"]["images"].as_array())
                    .into_iter()
                    .flatten()
                    .take(SHARED_IMAGES_MAX)
                    .enumerate()
                    .filter_map(|(index, image)| {
                        Some(AgentEvent::GeneratedImage {
                            id: zeron_proto::shared_image_id(&id, index),
                            path: image["path"].as_str().filter(|p| !p.is_empty())?.into(),
                            name: image["name"].as_str().unwrap_or("Image").into(),
                            mime_type: image["mimeType"].as_str().unwrap_or("image/png").into(),
                        })
                    })
                    .collect();
                events.push(AgentEvent::ToolResult {
                    id,
                    is_error: frame["isError"].as_bool().unwrap_or(false),
                    output: output.map(|s| cap(&s, 16384)),
                    diff,
                });
                events.extend(page);
                events.extend(shared);
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
/// The list a `todo` call left, from its result's `details` (`{action,
/// tasks}`); `None` for a read (`list`, `get`) or a failed write, whose
/// `tasks` say nothing about the list.
fn todo_items(details: &Value) -> Option<Vec<TodoItem>> {
    if !matches!(
        details["action"].as_str(),
        Some("create" | "update" | "delete" | "clear")
    ) || details.get("error").is_some_and(|error| !error.is_null())
    {
        return None;
    }
    let tasks = details["tasks"].as_array()?;
    Some(
        tasks
            .iter()
            .filter(|task| task["status"] != "deleted")
            .map(|task| {
                TodoItem::new(
                    string(task, "subject"),
                    TodoStatus::parse(string(task, "status")),
                )
            })
            .collect(),
    )
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
/// Where a grep looks: its path, with the files it skips (`exclude` for
/// pi-fff, `glob` for Pi's grep) when it names them.
fn search_scope(args: &Value) -> Option<String> {
    let path = args["path"].as_str().filter(|path| !path.is_empty());
    let filter = args["exclude"]
        .as_str()
        .map(|exclude| format!("!{exclude}"))
        .or_else(|| args["glob"].as_str().map(str::to_owned))
        .filter(|filter| !filter.is_empty());
    match (path, filter) {
        (Some(path), Some(filter)) => Some(format!("{path} {filter}")),
        (path, filter) => path.map(str::to_owned).or(filter),
    }
}
/// What an LSP tool looks at: its file, its paths, else the workspace.
fn lsp_target(args: &Value) -> String {
    if let Some(path) = args["path"].as_str().filter(|p| !p.is_empty()) {
        return path.to_owned();
    }
    let paths: Vec<&str> = args["paths"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    if paths.is_empty() {
        args["root"].as_str().unwrap_or("workspace").to_owned()
    } else {
        paths.join(", ")
    }
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
        // `ffgrep`/`fffind` are pi-fff's names for its grep and find.
        "grep" | "ffgrep" => ToolCall::Search {
            pattern: string(args, "pattern").into(),
            path: search_scope(args),
        },
        "multi_grep" | "fff-multi-grep" => ToolCall::Search {
            pattern: args["patterns"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join(" | "),
            path: args["constraints"]
                .as_str()
                .or(args["path"].as_str())
                .map(str::to_owned),
        },
        // pi-fff's `path` carries the glob and its `pattern` a fuzzy query
        // that may be empty; Pi's own `path` is the folder searched.
        "find" | "fffind" => ToolCall::Glob {
            pattern: match (string(args, "pattern"), string(args, "path")) {
                ("", path) => path.into(),
                (pattern, "") => pattern.into(),
                (pattern, path) => format!("{pattern} in {path}"),
            },
        },
        "lsp_fix" if args["write"] == true => ToolCall::EditFile {
            path: string(args, "path").into(),
            old_string: None,
            new_string: None,
        },
        "lsp_diagnostics" | "lsp_fix" => ToolCall::Unknown {
            name: format!(
                "LSP: {} {}",
                if name == "lsp_fix" {
                    "fix"
                } else {
                    "diagnostics"
                },
                lsp_target(args)
            )
            .trim_end()
            .to_owned(),
            input: Some(args.clone()),
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
    fn context_tokens(events: &[AgentEvent]) -> Option<Option<u64>> {
        events.iter().find_map(|event| match event {
            AgentEvent::ContextUsage { tokens, .. } => Some(*tokens),
            _ => None,
        })
    }
    #[test]
    fn only_a_reply_that_measured_the_context_moves_the_context_circle() {
        let mut n = Normalizer::default();
        let end = |stop: &str, usage: serde_json::Value| json!({"type":"message_end","message":{"role":"assistant","stopReason":stop,"content":[],"usage":usage}});
        let real = json!({"input":1000,"output":200,"cacheRead":300,"cacheWrite":0});
        let zero = json!({"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"totalTokens":0});
        // A tool-call reply carries real usage.
        assert_eq!(
            context_tokens(&n.map(&end("toolUse", real.clone()))),
            Some(Some(1500))
        );
        // Pi's own total wins when it reports one.
        let total = json!({"input":10,"output":5,"totalTokens":4321});
        assert_eq!(
            context_tokens(&n.map(&end("stop", total))),
            Some(Some(4321))
        );
        // Aborted, failed and all-zero replies keep the last measurement.
        assert_eq!(
            context_tokens(&n.map(&end("aborted", zero.clone()))),
            Some(None)
        );
        assert_eq!(context_tokens(&n.map(&end("error", real))), Some(None));
        assert_eq!(context_tokens(&n.map(&end("stop", zero))), Some(None));
    }
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
        assert_eq!(input.as_ref().unwrap()["title"], "Done");
        let AgentEvent::ToolResult { output, .. } = &events[1] else {
            panic!("{events:?}");
        };
        // The title line is the chip's header, not its body too.
        assert_eq!(output.as_deref(), None);
        let events = n.map(&json!({"type":"message_end","message":{"role":"custom",
            "customType":"rtk-status","display":true,"details":{"state":"failed"},
            "content":"\u{1b}[1mRTK\u{1b}[0m: ON\nsaved 42%"}}));
        let AgentEvent::ToolCall {
            call: ToolCall::Mcp { input, .. },
            ..
        } = &events[0]
        else {
            panic!("{events:?}");
        };
        assert_eq!(input.as_ref().unwrap()["title"], "RTK: ON");
        assert!(
            matches!(&events[1], AgentEvent::ToolResult { is_error: true, output: Some(body), .. } if body == "saved 42%"),
            "{events:?}"
        );
        assert!(
            n.map(&json!({"type":"message_end","message":{"role":"custom",
            "customType":"jev-decompose","display":false,"content":"x"}}))
                .is_empty()
        );
    }

    #[test]
    fn todo_writes_become_the_checklist_and_fff_tools_read_as_searches() {
        let mut n = Normalizer::default();
        n.map(
            &json!({"type":"tool_execution_start","toolCallId":"t","toolName":"todo",
            "args":{"action":"update","id":1,"status":"completed"}}),
        );
        let events = n.map(&json!({"type":"tool_execution_end","toolCallId":"t","isError":false,
            "result":{"content":[{"type":"text","text":"ok"}],"details":{"action":"update","params":{},
            "nextId":3,"tasks":[{"id":1,"subject":"plan","status":"completed"},
            {"id":2,"subject":"ship","status":"in_progress"},{"id":3,"subject":"old","status":"deleted"}]}}}));
        let AgentEvent::ToolCall {
            id,
            call: ToolCall::Todo { items },
        } = &events[0]
        else {
            panic!("{events:?}");
        };
        assert_eq!(id, "t");
        assert_eq!(items.len(), 2);
        assert!(items[0].done);
        assert_eq!(items[1].status, Some(TodoStatus::InProgress));
        // A read says nothing about the list.
        n.map(&json!({"type":"tool_execution_start","toolCallId":"l","toolName":"todo","args":{"action":"list"}}));
        let events = n.map(
            &json!({"type":"tool_execution_end","toolCallId":"l","isError":false,
            "result":{"content":[],"details":{"action":"list","tasks":[]}}}),
        );
        assert!(
            matches!(&events[0], AgentEvent::ToolResult { .. }),
            "{events:?}"
        );

        assert!(matches!(
            tool("fffind", &json!({"pattern":"","path":"src/**"})),
            ToolCall::Glob { pattern } if pattern == "src/**"
        ));
        assert!(matches!(
            tool("multi_grep", &json!({"patterns":["a","b"]})),
            ToolCall::Search { pattern, .. } if pattern == "a | b"
        ));
        assert!(matches!(
            tool("lsp_diagnostics", &json!({"paths":["a.rs"]})),
            ToolCall::Unknown { name, .. } if name == "LSP: diagnostics a.rs"
        ));
    }

    #[test]
    fn shared_images_follow_their_tool_result_as_image_parts() {
        let mut n = Normalizer::default();
        n.map(
            &json!({"type":"tool_execution_start","toolCallId":"s","toolName":"share_images",
            "args":{"paths":["a.png","b.png"]}}),
        );
        let events = n.map(
            &json!({"type":"tool_execution_end","toolCallId":"s","isError":false,
            "result":{"content":[{"type":"text","text":"Shown to the user: a.png, b.png."}],
            "details":{"images":[
                {"path":"/w/a.png","name":"a.png","mimeType":"image/png"},
                {"path":"","name":"lost.png"},
                {"path":"/w/.pi/screenshots/b.jpg","name":"b.jpg","mimeType":"image/jpeg"}]}}}),
        );
        assert!(
            matches!(&events[0], AgentEvent::ToolResult { id, is_error: false, .. } if id == "s")
        );
        assert!(
            matches!(&events[1], AgentEvent::GeneratedImage { id, path, name, mime_type }
                if id == "s:share0" && path == "/w/a.png" && name == "a.png" && mime_type == "image/png"),
            "{events:?}"
        );
        assert!(
            matches!(&events[2], AgentEvent::GeneratedImage { id, path, mime_type, .. }
                if id == "s:share2" && path == "/w/.pi/screenshots/b.jpg" && mime_type == "image/jpeg"),
            "{events:?}"
        );
        assert_eq!(events.len(), 3);
        // A failed share shows nothing, and so does any other tool's `images`.
        n.map(&json!({"type":"tool_execution_start","toolCallId":"f","toolName":"share_images","args":{}}));
        let failed = n.map(
            &json!({"type":"tool_execution_end","toolCallId":"f","isError":true,
            "result":{"content":[{"type":"text","text":"Nothing to show."}],
            "details":{"images":[{"path":"/w/a.png","name":"a.png"}]}}}),
        );
        assert_eq!(failed.len(), 1);
        n.map(&json!({"type":"tool_execution_start","toolCallId":"o","toolName":"screenshot","args":{}}));
        let other = n.map(
            &json!({"type":"tool_execution_end","toolCallId":"o","isError":false,
            "result":{"content":[],"details":{"images":[{"path":"/w/a.png","name":"a.png"}]}}}),
        );
        assert_eq!(other.len(), 1);
    }

    #[test]
    fn a_published_page_follows_its_tool_result_as_a_page_part() {
        let mut n = Normalizer::default();
        n.map(
            &json!({"type":"tool_execution_start","toolCallId":"h","toolName":"zeron_html_render",
            "args":{"title":"Build times","html":"<p>x</p>"}}),
        );
        let events = n.map(
            &json!({"type":"tool_execution_end","toolCallId":"h","isError":false,
            "result":{"content":[{"type":"text","text":"{}"}],
            "details":{"page":{"path":"/u/abcd1234-page.html","title":"Build times"}}}}),
        );
        assert!(
            matches!(&events[0], AgentEvent::ToolResult { id, is_error: false, .. } if id == "h")
        );
        assert!(
            matches!(&events[1], AgentEvent::GeneratedImage { id, path, name, mime_type }
                if id == "h:page" && path == "/u/abcd1234-page.html" && name == "Build times" && mime_type == "text/html"),
            "{events:?}"
        );
        // A failed call publishes nothing.
        n.map(&json!({"type":"tool_execution_start","toolCallId":"f","toolName":"zeron_html_render","args":{}}));
        let events = n.map(
            &json!({"type":"tool_execution_end","toolCallId":"f","isError":true,
            "result":{"content":[{"type":"text","text":"title must not be empty"}],"details":{}}}),
        );
        assert_eq!(events.len(), 1);
    }
}
