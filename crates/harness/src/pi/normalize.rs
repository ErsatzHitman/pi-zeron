use crate::redact::strip_ansi;
use serde_json::Value;
use std::collections::HashMap;
use zeron_proto::{AgentEvent, DoneStatus, ErrorClass, TodoItem, TodoStatus, ToolCall, ToolDiff};

/// The most images one `share_images` call shows (the extension's own cap).
const SHARED_IMAGES_MAX: usize = 8;

#[derive(Default)]
pub(super) struct Normalizer {
    text: HashMap<usize, String>,
    thinking: HashMap<usize, String>,
    tools: HashMap<String, (String, Value)>,
    pub error: Option<String>,
    /// What kind of failure `error` is; set and cleared with it.
    pub class: Option<ErrorClass>,
    /// The provider the last assistant message came from (names whose login
    /// an auth failure needs).
    pub provider: Option<String>,
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

/// The error text of an assistant message that failed without saying why.
const GENERIC_FAILURE: &str = "Pi provider failed";

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
        self.class = None;
        self.provider = None;
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
    /// The failure class, only for a run that ended in error.
    pub fn class(&self) -> Option<ErrorClass> {
        (self.status() == DoneStatus::Errored)
            .then(|| self.class.clone())
            .flatten()
    }
    /// Record `text` as the run's failure and classify it.
    pub fn fail(&mut self, text: String) {
        self.class = Some(classify(&text, self.provider.as_deref()));
        self.error = Some(text);
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
                if let Some(provider) = message["provider"].as_str().filter(|p| !p.is_empty()) {
                    self.provider = Some(provider.to_owned());
                }
                // A later successful message clears the failure and its
                // class together (a retried 429 stops reading as failed).
                self.error = None;
                self.class = None;
                if message["stopReason"] == "error" {
                    self.fail(
                        message["errorMessage"]
                            .as_str()
                            .unwrap_or(GENERIC_FAILURE)
                            .to_owned(),
                    );
                }
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
            // Pi's retries are exhausted: the last attempt's error is the
            // better description than the first message's, which may have
            // been generic. A cancelled retry says nothing about the cause.
            "auto_retry_end" if frame["success"] == false => {
                let last = string(frame, "finalError");
                if !last.is_empty()
                    && last != "Retry cancelled"
                    && self.error.as_deref().is_none_or(|e| e == GENERIC_FAILURE)
                {
                    self.fail(last.to_owned());
                }
            }
            "extension_error" => {
                self.class = None;
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
        // The web-tools extension's DeepSeek search and fetch.
        "web_search" => ToolCall::WebSearch {
            query: string(args, "query").into(),
        },
        "web_fetch" => ToolCall::WebFetch {
            url: string(args, "url").into(),
            prompt: None,
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
/// Whether `token` (digits) appears in `text` not touching other
/// alphanumerics, so `429` matches `HTTP 429:` but not `4290` or `x429`.
fn word(text: &str, token: &str) -> bool {
    text.match_indices(token).any(|(at, _)| {
        let before = text[..at].chars().next_back();
        let after = text[at + token.len()..].chars().next();
        !before.is_some_and(char::is_alphanumeric) && !after.is_some_and(char::is_alphanumeric)
    })
}

fn has_any(text: &str, needles: &[&str]) -> bool {
    needles.iter().any(|needle| text.contains(needle))
}

/// Classify a Pi failure's text. Pi has no structured error codes, so this
/// is a conservative keyword match (Pi's own retry vocabulary): anything
/// unrecognised is `Other`, and no hint is shown for it. Quota is checked
/// before rate limit because some quota errors arrive as 429.
pub(super) fn classify(text: &str, provider: Option<&str>) -> ErrorClass {
    classify_at(text, provider, now_ms())
}

fn classify_at(text: &str, provider: Option<&str>, now_ms: i64) -> ErrorClass {
    let t = text.to_lowercase();
    if has_any(
        &t,
        &["session not found", "no such session", "unknown session"],
    ) || (t.contains("session file") && has_any(&t, &["not found", "missing", "does not exist"]))
    {
        return ErrorClass::SessionGone;
    }
    if has_any(
        &t,
        &[
            "invalid api key",
            "incorrect api key",
            "no api key",
            "api key not found",
            "not logged in",
            "please log in",
            "please login",
            "/login",
            "invalid x-api-key",
            "authentication_error",
            "unauthorized",
            "invalid_grant",
            "token has expired",
        ],
    ) || (t.contains("oauth") && has_any(&t, &["expired", "refresh", "revoked"]))
        || word(&t, "401")
    {
        return ErrorClass::AuthRequired {
            provider: provider.map(str::to_owned),
        };
    }
    if has_any(
        &t,
        &[
            "insufficient_quota",
            "quota exceeded",
            "exceeded your current quota",
            "out of budget",
            "gousagelimiterror",
            "freeusagelimiterror",
            "usage limit reached",
            "monthly usage limit",
            "subscription_sharing_usage_limit_exceeded",
            "credit balance is too low",
            "insufficient credit",
            "payment required",
        ],
    ) || word(&t, "402")
    {
        return ErrorClass::QuotaExhausted {
            resets_at: parse_reset_ms(&t, now_ms),
        };
    }
    if has_any(
        &t,
        &["rate limit", "rate_limit", "ratelimit", "too many requests"],
    ) || word(&t, "429")
    {
        return ErrorClass::RateLimited {
            resets_at: parse_reset_ms(&t, now_ms),
        };
    }
    if has_any(
        &t,
        &[
            "overloaded",
            "server_busy",
            "servers are currently busy",
            "at capacity",
            "high demand",
            "service unavailable",
        ],
    ) || word(&t, "529")
        || word(&t, "503")
    {
        return ErrorClass::ProviderOverloaded;
    }
    ErrorClass::Other
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_millis() as i64)
}

/// When a limit clears (unix ms), from the unambiguous forms providers use:
/// `retry-after: 30`, `retry after 30 seconds`, `try again in 20m`,
/// `resets in 2h 10m`, `resets at 2026-01-02T03:04:05Z`. `None` when unsure.
fn parse_reset_ms(text: &str, now_ms: i64) -> Option<i64> {
    let t = text.to_lowercase();
    for prefix in ["resets at ", "reset at "] {
        if let Some(at) = t.find(prefix) {
            let stamp = t[at + prefix.len()..]
                .split_whitespace()
                .next()?
                .trim_end_matches(['.', ',', ')', ';']);
            return parse_rfc3339_ms(stamp);
        }
    }
    for (prefix, bare_seconds) in [
        ("retry-after:", true),
        ("retry-after ", true),
        ("retry after ", false),
        ("try again in ", false),
        ("resets in ", false),
        ("reset in ", false),
    ] {
        if let Some(at) = t.find(prefix) {
            let ms = parse_duration_ms(&t[at + prefix.len()..], bare_seconds)?;
            return Some(now_ms.saturating_add(ms));
        }
    }
    None
}

/// `30`, `30 seconds`, `20m`, `2h 10m`, `1h and 5 min`: a sum of number+unit
/// pairs at the start of `text`. A bare number is seconds only when allowed.
fn parse_duration_ms(text: &str, bare_seconds: bool) -> Option<i64> {
    let mut rest = text.trim_start();
    let mut total = 0f64;
    let mut pairs = 0;
    loop {
        let digits = rest
            .find(|c: char| !c.is_ascii_digit() && c != '.')
            .unwrap_or(rest.len());
        let Ok(n) = rest[..digits].parse::<f64>() else {
            break;
        };
        let after = rest[digits..].trim_start();
        let unit_len = after
            .find(|c: char| !c.is_ascii_alphabetic())
            .unwrap_or(after.len());
        let unit = &after[..unit_len];
        let secs = match unit {
            "s" | "sec" | "secs" | "second" | "seconds" => 1.0,
            "m" | "min" | "mins" | "minute" | "minutes" => 60.0,
            "h" | "hr" | "hrs" | "hour" | "hours" => 3600.0,
            "" if bare_seconds && pairs == 0 => 1.0,
            _ => break,
        };
        total += n * secs;
        pairs += 1;
        let tail = after[unit_len..].trim_start_matches(|c: char| c.is_whitespace() || c == ',');
        rest = tail.strip_prefix("and ").unwrap_or(tail);
    }
    (pairs > 0 && total.is_finite()).then(|| (total * 1000.0).round() as i64)
}

/// `2026-01-02T03:04:05Z` / `...+01:00` (optional fraction) as unix ms.
fn parse_rfc3339_ms(stamp: &str) -> Option<i64> {
    let b = stamp.as_bytes();
    let num = |from: usize, len: usize| -> Option<i64> {
        let part = stamp.get(from..from + len)?;
        part.bytes()
            .all(|c| c.is_ascii_digit())
            .then(|| part.parse().ok())
            .flatten()
    };
    if b.len() < 20 || b[4] != b'-' || b[7] != b'-' || !matches!(b[10], b'T' | b't' | b' ') {
        return None;
    }
    let (year, month, day) = (num(0, 4)?, num(5, 2)?, num(8, 2)?);
    let (hour, minute, second) = (num(11, 2)?, num(14, 2)?, num(17, 2)?);
    if !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || hour > 23
        || minute > 59
        || second > 60
    {
        return None;
    }
    let mut at = 19;
    let mut millis = 0;
    if b.get(at) == Some(&b'.') {
        let end = (at + 1..b.len())
            .find(|&i| !b[i].is_ascii_digit())
            .unwrap_or(b.len());
        let frac = &stamp[at + 1..end];
        if frac.is_empty() {
            return None;
        }
        millis = format!("{:0<3}", &frac[..frac.len().min(3)])
            .parse::<i64>()
            .ok()?;
        at = end;
    }
    let offset = match b.get(at)? {
        b'Z' | b'z' if at + 1 == b.len() => 0,
        sign @ (b'+' | b'-') if b.len() == at + 6 && b[at + 3] == b':' => {
            let secs = num(at + 1, 2)? * 3600 + num(at + 4, 2)? * 60;
            if *sign == b'+' { secs } else { -secs }
        }
        _ => return None,
    };
    // Days from the civil date (Howard Hinnant's algorithm).
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (month + if month > 2 { -3 } else { 9 }) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468;
    Some(((days * 86400 + hour * 3600 + minute * 60 + second - offset) * 1000) + millis)
}
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn kind(text: &str) -> &'static str {
        classify_at(text, Some("anthropic"), 0).slug()
    }

    #[test]
    fn classify_table() {
        for (text, want) in [
            (
                "insufficient_quota: You exceeded your current quota",
                "quotaExhausted",
            ),
            ("GoUsageLimitError: out of credits", "quotaExhausted"),
            ("Monthly usage limit reached", "quotaExhausted"),
            ("402 Payment Required", "quotaExhausted"),
            (
                "Your credit balance is too low to access the API",
                "quotaExhausted",
            ),
            ("429 Too Many Requests", "rateLimited"),
            ("rate_limit_error: slow down", "rateLimited"),
            // Quota wins over the 429 it arrives in.
            (
                "429 {\"error\":\"quota exceeded for this key\"}",
                "quotaExhausted",
            ),
            ("No API key found for anthropic", "authRequired"),
            ("401 Unauthorized", "authRequired"),
            ("OAuth token has expired", "authRequired"),
            ("Please log in with /login", "authRequired"),
            ("529 overloaded_error", "providerOverloaded"),
            ("503 Service Unavailable", "providerOverloaded"),
            ("session not found", "sessionGone"),
            ("Session file is missing", "sessionGone"),
            ("mock provider failure", "other"),
            ("fetch failed", "other"),
            ("port 4290 busy", "other"),
            ("request id 14011 failed", "other"),
            ("403 Forbidden", "other"),
            ("update your billing address", "other"),
        ] {
            assert_eq!(kind(text), want, "{text}");
        }
        assert_eq!(
            classify_at("No API key found", Some("anthropic"), 0),
            ErrorClass::AuthRequired {
                provider: Some("anthropic".into())
            }
        );
        assert_eq!(
            classify_at("No API key found", None, 0),
            ErrorClass::AuthRequired { provider: None }
        );
    }

    #[test]
    fn reset_times_are_parsed_only_when_unambiguous() {
        let now = 1_000_000;
        assert_eq!(
            parse_reset_ms("rate limited, retry after 30 seconds", now),
            Some(now + 30_000)
        );
        assert_eq!(parse_reset_ms("retry-after: 12", now), Some(now + 12_000));
        assert_eq!(
            parse_reset_ms("Try again in 20m.", now),
            Some(now + 1_200_000)
        );
        assert_eq!(
            parse_reset_ms("Usage limit reached, resets in 2h 10m", now),
            Some(now + (2 * 3600 + 10 * 60) * 1000)
        );
        assert_eq!(
            parse_reset_ms("limit resets at 2026-01-02T03:04:05Z.", now),
            Some(1_767_323_045_000)
        );
        assert_eq!(
            parse_reset_ms("resets at 2026-01-02T04:04:05+01:00", now),
            Some(1_767_323_045_000)
        );
        assert_eq!(parse_reset_ms("something went wrong", now), None);
        assert_eq!(parse_reset_ms("resets at tomorrow", now), None);
        assert_eq!(parse_reset_ms("retry after a while", now), None);
        assert_eq!(
            classify_at("429 rate limit exceeded, retry after 30 seconds", None, now),
            ErrorClass::RateLimited {
                resets_at: Some(now + 30_000)
            }
        );
    }

    fn assistant_end(stop: &str, err: Option<&str>) -> serde_json::Value {
        let mut message =
            json!({"role":"assistant","stopReason":stop,"provider":"openai-codex","content":[]});
        if let Some(err) = err {
            message["errorMessage"] = json!(err);
        }
        json!({"type":"message_end","message":message})
    }

    #[test]
    fn failure_class_follows_the_error() {
        let mut n = Normalizer::default();
        n.map(&assistant_end("error", Some("429 Too Many Requests")));
        assert_eq!(n.class(), Some(ErrorClass::RateLimited { resets_at: None }));
        // A successful retry clears both.
        n.map(&assistant_end("stop", None));
        assert_eq!(n.status(), DoneStatus::Completed);
        assert_eq!(n.class(), None);

        // The first message was generic; the exhausted retry says why.
        n.map(&assistant_end("error", None));
        assert_eq!(n.class(), Some(ErrorClass::Other));
        n.map(&json!({"type":"auto_retry_end","success":false,"attempt":3,"finalError":"401 Unauthorized"}));
        assert_eq!(
            n.class(),
            Some(ErrorClass::AuthRequired {
                provider: Some("openai-codex".into())
            })
        );

        // A cancelled retry, or a specific earlier error, is left alone.
        let mut n = Normalizer::default();
        n.map(&assistant_end("error", None));
        n.map(&json!({"type":"auto_retry_end","success":false,"attempt":1,"finalError":"Retry cancelled"}));
        assert_eq!(n.class(), Some(ErrorClass::Other));
        let mut n = Normalizer::default();
        n.map(&assistant_end("error", Some("insufficient_quota")));
        n.map(
            &json!({"type":"auto_retry_end","success":false,"attempt":1,"finalError":"overloaded"}),
        );
        assert_eq!(
            n.class(),
            Some(ErrorClass::QuotaExhausted { resets_at: None })
        );

        // No class off an aborted run, and reset forgets everything.
        let mut n = Normalizer::default();
        n.map(&assistant_end("error", Some("429")));
        n.map(&json!({"type":"agent_settled","aborted":true}));
        assert_eq!(n.class(), None);
        n.reset();
        assert!(n.class.is_none() && n.provider.is_none());
    }
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
    fn web_search_and_fetch_read_as_web_rows() {
        assert!(matches!(
            tool("web_search", &json!({"query":"rust 2024 edition","count":5})),
            ToolCall::WebSearch { query } if query == "rust 2024 edition"
        ));
        assert!(matches!(
            tool("web_fetch", &json!({"url":"https://example.com/a"})),
            ToolCall::WebFetch { url, prompt: None } if url == "https://example.com/a"
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
}
