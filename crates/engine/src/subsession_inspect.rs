//! What the host knows about the work delegated under one chat, for the
//! chat's own agent (`InspectSubsessions`, behind the MCP's
//! `list_subsessions` / `inspect_subsession`): Pi sub-agents and background
//! shells from the live extension channels, workflow runs live and from
//! their journals under `~/.pi/workflows`, and the run journal's
//! sub-agent results and errors.
//!
//! Read-only and bounded: it never watches the chat's extension UI (a watch
//! flips Pi into its GUI mode), reads files from their tails, clips every
//! string, redacts secret-shaped tokens and caps the whole reply, so a tool
//! result cannot flood the agent's context.

use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use serde_json::{Map, Value, json};
use zeron_proto::agent::{AgentEvent, DoneStatus, ToolCall};
use zeron_proto::extension_ui::{
    BASH_BG_CHANNEL, ExtensionUiState, FLEET_CHANNEL, GOAL_CHANNEL, WorkflowState,
    WorkflowTaskState,
};

use crate::run_journal::RunJournal;

pub(crate) const DEFAULT_TAIL: usize = 40;
pub(crate) const MAX_TAIL: usize = 400;
/// Characters kept of any one string.
const TEXT_CHARS: usize = 600;
/// Characters kept of a prompt, result or sub-agent diagnostics.
const LONG_CHARS: usize = 4_000;
/// Items kept of any one array (its newest).
const ITEMS: usize = 20;
/// Run-journal events scanned back from the newest.
const SCAN_EVENTS: usize = 4_000;
/// Bytes read from the end of a log.
const TAIL_BYTES: u64 = 256 * 1024;
/// Journals larger than this are named, not parsed.
const MAX_JOURNAL_BYTES: u64 = 16 * 1024 * 1024;
/// Workflow runs listed from disk.
const RUNS_LISTED: usize = 10;
/// Errors listed in the overview.
const OVERVIEW_ERRORS: usize = 5;
const MAX_REPLY_BYTES: usize = 48 * 1024;

#[derive(Debug, Default, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct InspectParams {
    pub chat_id: String,
    /// Where the chat's agent works; the workflows extension keys its run
    /// journals by it.
    #[serde(default)]
    pub cwd: Option<String>,
    /// `subagent`, `workflow`, `background` or `events`; `None` with an `id`
    /// detects it, `None` without one is the overview.
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub tail: Option<usize>,
    /// The workflow agent (ordinal) whose prompt and transcript to include.
    #[serde(default)]
    pub agent: Option<u64>,
}

/// The reply for `params`, from the chat's live UI state, the run journal
/// and the workflow journals under `home`.
pub(crate) fn inspect(
    home: &Path,
    ui: &ExtensionUiState,
    journal: &RunJournal,
    params: &InspectParams,
) -> Value {
    let tail = params.tail.unwrap_or(DEFAULT_TAIL).clamp(1, MAX_TAIL);
    let cwd = params.cwd.as_deref().filter(|cwd| !cwd.is_empty());
    let workflows = home.join(".pi").join("workflows").join("projects");
    let reply = match (params.kind.as_deref(), params.id.as_deref()) {
        (Some("events"), _) => json!({
            "kind": "events",
            "found": true,
            "events": recent_events(journal, &params.chat_id, tail),
        }),
        (None, None) => overview(&workflows, ui, journal, &params.chat_id, cwd),
        (kind, Some(id)) => detail(&workflows, ui, journal, params, cwd, kind, id, tail),
        (Some(kind), None) => json!({
            "found": false,
            "error": format!("kind {kind} needs an id; list first"),
            "candidates": candidates(&workflows, ui, cwd),
        }),
    };
    cap_reply(reply)
}

fn overview(
    workflows: &Path,
    ui: &ExtensionUiState,
    journal: &RunJournal,
    chat_id: &str,
    cwd: Option<&str>,
) -> Value {
    let live = workflow_live(ui);
    let live_id = live.as_ref().and_then(|run| run["runId"].as_str().map(str::to_owned));
    let runs: Vec<Value> = cwd
        .map(|cwd| list_runs(workflows, cwd, RUNS_LISTED))
        .unwrap_or_default()
        .into_iter()
        .map(|mut run| {
            if live_id.is_some() && run["runId"].as_str() == live_id.as_deref() {
                run["live"] = json!(true);
            }
            run
        })
        .collect();
    let mut notes = Vec::new();
    if !ui.connected {
        notes.push(
            "The chat's Pi process is not connected: live sub-agent, workflow and shell state is \
             empty. Finished workflow runs are read from their journals, sub-agent results from \
             the run journal.",
        );
    }
    if cwd.is_none() {
        notes.push("No working directory was given, so workflow journals were not listed.");
    }
    json!({
        "kind": "overview",
        "found": true,
        "live": ui.connected,
        "subagents": entries(ui, FLEET_CHANNEL),
        "workflow": live,
        "workflowRuns": runs,
        "background": entries(ui, BASH_BG_CHANNEL),
        "goal": ui.channels.get(GOAL_CHANNEL).map(|goal| clip_value(goal, TEXT_CHARS)),
        "recentErrors": recent_events(journal, chat_id, OVERVIEW_ERRORS),
        "notes": notes,
    })
}

#[allow(clippy::too_many_arguments)]
fn detail(
    workflows: &Path,
    ui: &ExtensionUiState,
    journal: &RunJournal,
    params: &InspectParams,
    cwd: Option<&str>,
    kind: Option<&str>,
    id: &str,
    tail: usize,
) -> Value {
    let wants = |k: &str| kind.is_none_or(|kind| kind == k);
    if wants("subagent")
        && let Some(entry) = entry(ui, FLEET_CHANNEL, id)
    {
        return json!({
            "kind": "subagent",
            "found": true,
            "id": id,
            "live": entry,
            "results": pi_rows(journal, &params.chat_id, id, tail.min(10)),
        });
    }
    if wants("background")
        && let Some(entry) = entry(ui, BASH_BG_CHANNEL, id)
    {
        return json!({
            "kind": "background",
            "found": true,
            "id": id,
            "live": entry,
            "results": pi_rows(journal, &params.chat_id, id, tail.min(10)),
        });
    }
    if wants("workflow") && safe_run_id(id) {
        let live = workflow_live(ui).filter(|run| run["runId"].as_str() == Some(id));
        let journal_path = find_journal(workflows, cwd, id);
        if live.is_some() || journal_path.is_some() {
            let mut report = journal_path
                .map(|path| workflow_detail(&path, params.agent, tail))
                .unwrap_or_else(|| json!({}));
            report["kind"] = json!("workflow");
            report["found"] = json!(true);
            report["runId"] = json!(id);
            report["live"] = live.unwrap_or(Value::Null);
            return report;
        }
    }
    // A sub-agent or shell that already left the live roster: its delivered
    // result is still in the run journal.
    if kind.is_none_or(|kind| kind == "subagent" || kind == "background") {
        let results = pi_rows(journal, &params.chat_id, id, tail.min(10));
        if !results.is_empty() {
            return json!({
                "kind": kind.unwrap_or("subagent"),
                "found": true,
                "id": id,
                "live": Value::Null,
                "results": results,
                "notes": ["No longer running; this is what it delivered."],
            });
        }
    }
    json!({
        "found": false,
        "id": id,
        "candidates": candidates(workflows, ui, cwd),
    })
}

/// Ids an agent can inspect, for a miss.
fn candidates(workflows: &Path, ui: &ExtensionUiState, cwd: Option<&str>) -> Value {
    let keys = |channel: &str| -> Vec<String> {
        raw_entries(ui, channel)
            .iter()
            .filter_map(|entry| entry["key"].as_str().map(str::to_owned))
            .collect()
    };
    let mut runs: Vec<String> = workflow_live(ui)
        .and_then(|run| run["runId"].as_str().map(str::to_owned))
        .into_iter()
        .collect();
    for run in cwd.map(|cwd| list_runs(workflows, cwd, RUNS_LISTED)).unwrap_or_default() {
        if let Some(id) = run["runId"].as_str()
            && !runs.iter().any(|known| known == id)
        {
            runs.push(id.to_owned());
        }
    }
    json!({
        "subagents": keys(FLEET_CHANNEL),
        "workflows": runs,
        "background": keys(BASH_BG_CHANNEL),
    })
}

fn raw_entries<'a>(ui: &'a ExtensionUiState, channel: &str) -> &'a [Value] {
    ui.channels
        .get(channel)
        .and_then(|fleet| fleet["entries"].as_array())
        .map_or(&[], Vec::as_slice)
}

/// A fleet channel's entries, clipped.
fn entries(ui: &ExtensionUiState, channel: &str) -> Vec<Value> {
    raw_entries(ui, channel)
        .iter()
        .map(|entry| clip_value(entry, TEXT_CHARS))
        .collect()
}

/// The entry keyed `id` (or whose job id is `id`), clipped.
fn entry(ui: &ExtensionUiState, channel: &str, id: &str) -> Option<Value> {
    raw_entries(ui, channel)
        .iter()
        .find(|entry| {
            ["key", "id", "jobId"]
                .iter()
                .any(|field| entry[*field].as_str() == Some(id))
        })
        .map(|entry| clip_value(entry, LONG_CHARS))
}

/// The live workflow run, with the tasks that need a look: failed,
/// retrying or aborted ones and their errors.
fn workflow_live(ui: &ExtensionUiState) -> Option<Value> {
    let state = WorkflowState::of(ui)?;
    let run = state.run.as_ref()?;
    let troubled: Vec<Value> = state
        .tasks
        .iter()
        .filter(|task| {
            matches!(
                task.state,
                WorkflowTaskState::Failed | WorkflowTaskState::Retrying | WorkflowTaskState::Aborted
            )
        })
        .take(ITEMS)
        .map(|task| {
            json!({
                "id": task.id,
                "label": clip(&task.label, TEXT_CHARS),
                "phase": task.phase,
                "state": task.state,
                "attempt": task.attempt,
                "maxAttempts": task.max_attempts,
                "model": task.model,
                "error": task.error.as_ref().map(|error| json!({
                    "code": error.code,
                    "message": clip(&error.message, LONG_CHARS),
                    "retriable": error.retriable,
                })),
            })
        })
        .collect();
    Some(json!({
        "runId": run.id,
        "name": run.name,
        "goal": clip(&run.goal, TEXT_CHARS),
        "status": run.status,
        "statusReason": run.status_reason,
        "phase": run.phase,
        "startedAt": run.started_at,
        "endedAt": run.ended_at,
        "agents": run.agents,
        "tokens": run.tokens,
        "progress": state.progress,
        "phases": state.phases.iter().map(|phase| json!({
            "title": phase.title,
            "state": phase.state,
            "done": phase.done,
            "total": phase.total,
            "failed": phase.failed,
        })).collect::<Vec<_>>(),
        "troubledTasks": troubled,
        "resumable": state.recovery.resumable,
        "interrupted": clip_value(&json!(state.recovery.interrupted), TEXT_CHARS),
    }))
}

/// FNV-1a over UTF-16 code units: the workflows extension's `shortHash`.
fn short_hash(input: &str) -> String {
    let mut hash: u32 = 0x811c_9dc5;
    for unit in input.encode_utf16() {
        hash ^= u32::from(unit);
        hash = hash.wrapping_mul(0x0100_0193);
    }
    format!("{hash:08x}")
}

/// `projects/<slug>-<hash>` for `cwd`, as the workflows extension's
/// `projectDir` names it (the cwd string exactly as Pi saw it).
fn project_dir(workflows: &Path, cwd: &str) -> PathBuf {
    let trimmed = cwd.trim_end_matches(['/', '\\']);
    let base = trimmed
        .rsplit(['/', '\\'])
        .next()
        .filter(|base| !base.is_empty())
        .unwrap_or("project");
    let mut slug = String::new();
    for c in base.to_lowercase().chars() {
        if c.is_ascii_alphanumeric() {
            slug.push(c);
        } else if !slug.ends_with('-') {
            slug.push('-');
        }
    }
    let slug = slug.trim_matches('-');
    let slug = if slug.is_empty() { "project" } else { slug };
    workflows.join(format!("{slug}-{}", short_hash(cwd)))
}

/// The extension's own run-id rule; it also keeps a model-supplied id from
/// walking out of the journal directory.
fn safe_run_id(id: &str) -> bool {
    id.len() <= 200
        && id.chars().next().is_some_and(|c| c.is_ascii_alphanumeric())
        && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
}

/// Where a project's journals sit: the project directory, or its `runs`
/// folder in older layouts.
fn journal_dirs(dir: PathBuf) -> [PathBuf; 2] {
    let runs = dir.join("runs");
    [dir, runs]
}

fn find_journal(workflows: &Path, cwd: Option<&str>, run_id: &str) -> Option<PathBuf> {
    let file = format!("{run_id}.json");
    if let Some(cwd) = cwd {
        for dir in journal_dirs(project_dir(workflows, cwd)) {
            let path = dir.join(&file);
            if path.is_file() {
                return Some(path);
            }
        }
    }
    // A run started from another directory (or a cwd spelled differently).
    let projects = std::fs::read_dir(workflows).ok()?;
    projects.flatten().find_map(|project| {
        journal_dirs(project.path())
            .into_iter()
            .map(|dir| dir.join(&file))
            .find(|path| path.is_file())
    })
}

/// The newest `limit` run journals for `cwd`, summarised.
fn list_runs(workflows: &Path, cwd: &str, limit: usize) -> Vec<Value> {
    let mut journals: Vec<(std::time::SystemTime, PathBuf)> = journal_dirs(project_dir(workflows, cwd))
        .iter()
        .filter_map(|dir| std::fs::read_dir(dir).ok())
        .flat_map(|dir| dir.flatten())
        .filter_map(|entry| {
            let path = entry.path();
            // Exact `.json`: `.json.bak` copies sit beside them.
            if path.extension().is_none_or(|ext| ext != "json") {
                return None;
            }
            let modified = entry.metadata().ok()?.modified().ok()?;
            Some((modified, path))
        })
        .collect();
    journals.sort_by(|a, b| b.0.cmp(&a.0));
    journals
        .into_iter()
        .take(limit)
        .map(|(_, path)| match read_journal(&path) {
            Some(journal) => run_summary(&journal, &path),
            None => json!({
                "runId": path.file_stem().map(|stem| stem.to_string_lossy().into_owned()),
                "unreadable": true,
                "journalPath": path,
            }),
        })
        .collect()
}

fn read_journal(path: &Path) -> Option<Value> {
    let size = std::fs::metadata(path).ok()?.len();
    if size > MAX_JOURNAL_BYTES {
        return None;
    }
    // Rewritten on every transition: a torn read is `None`, not an error.
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

fn sibling(path: &Path, suffix: &str) -> PathBuf {
    let stem = path
        .file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .unwrap_or_default();
    path.with_file_name(format!("{stem}{suffix}"))
}

fn agents(journal: &Value) -> &[Value] {
    journal["agents"].as_array().map_or(&[], Vec::as_slice)
}

fn failed(agent: &Value) -> bool {
    agent["status"].as_str().is_some_and(|status| {
        matches!(status, "error" | "failed" | "aborted" | "timeout")
    }) || !agent["error"].is_null()
}

fn run_summary(journal: &Value, path: &Path) -> Value {
    let agents = agents(journal);
    let mut by_status: Map<String, Value> = Map::new();
    for agent in agents {
        let status = agent["status"].as_str().unwrap_or("unknown").to_owned();
        let count = by_status.get(&status).and_then(Value::as_u64).unwrap_or(0);
        by_status.insert(status, json!(count + 1));
    }
    let failed_agents: Vec<Value> = agents
        .iter()
        .filter(|agent| failed(agent))
        .take(10)
        .map(|agent| {
            json!({
                "ordinal": agent["ordinal"],
                "label": clip_value(&agent["label"], TEXT_CHARS),
                "model": agent["model"],
                "status": agent["status"],
                "errorCode": agent["errorCode"],
                "error": clip_value(&agent["error"], TEXT_CHARS),
            })
        })
        .collect();
    let log = sibling(path, ".log");
    json!({
        "runId": journal["runId"],
        "workflowName": journal["workflowName"],
        "status": journal["status"],
        "error": clip_value(&journal["error"], TEXT_CHARS),
        "startedAt": journal["startedAt"],
        "updatedAt": journal["updatedAt"],
        "completedAt": journal["completedAt"],
        "durationMs": journal["durationMs"],
        "agentsTotal": agents.len(),
        "agentsByStatus": by_status,
        "failedAgents": failed_agents,
        "journalPath": path,
        "logPath": log.is_file().then_some(log),
    })
}

/// One run in full: every agent's state and error, the log tail, and with
/// `agent` that agent's prompt, result and transcript tail.
fn workflow_detail(path: &Path, agent: Option<u64>, tail: usize) -> Value {
    let Some(journal) = read_journal(path) else {
        return json!({
            "journalPath": path,
            "unreadable": true,
            "logTail": tail_lines(&sibling(path, ".log"), tail),
        });
    };
    let mut report = run_summary(&journal, path);
    let all = agents(&journal);
    report["phases"] = clip_value(&journal["phases"], TEXT_CHARS);
    report["tokenUsage"] = journal["tokenUsage"].clone();
    report["result"] = clip_value(&journal["result"], LONG_CHARS);
    report["agents"] = json!(
        all.iter()
            .take(80)
            .map(|agent| json!({
                "ordinal": agent["ordinal"],
                "label": clip_value(&agent["label"], TEXT_CHARS),
                "model": agent["model"],
                "tier": agent["tier"],
                "status": agent["status"],
                "errorCode": agent["errorCode"],
                "error": clip_value(&agent["error"], LONG_CHARS),
                "recoverable": agent["recoverable"],
                "attempts": clip_value(&agent["attempts"], TEXT_CHARS),
                "tokens": agent["tokens"],
                "startedAt": agent["startedAt"],
                "endedAt": agent["endedAt"],
                "isolation": clip_value(&agent["isolation"], TEXT_CHARS),
                "resultPreview": clip_value(&agent["resultPreview"], TEXT_CHARS),
            }))
            .collect::<Vec<_>>()
    );
    if all.len() > 80 {
        report["agentsOmitted"] = json!(all.len() - 80);
    }
    let log = sibling(path, ".log");
    report["logTail"] = if log.is_file() {
        json!(tail_lines(&log, tail))
    } else {
        let logs = journal["logs"].as_array().map_or(&[][..], Vec::as_slice);
        json!(
            logs[logs.len().saturating_sub(tail)..]
                .iter()
                .map(|line| clip_value(line, TEXT_CHARS))
                .collect::<Vec<_>>()
        )
    };
    let history_dir = sibling(path, ".history");
    report["historyDir"] = json!(history_dir.is_dir().then_some(&history_dir));
    if let Some(ordinal) = agent {
        let found = all
            .iter()
            .find(|candidate| candidate["ordinal"].as_u64() == Some(ordinal));
        report["agent"] = match found {
            Some(found) => agent_detail(found, ordinal, &history_dir, tail),
            None => json!({ "ordinal": ordinal, "found": false }),
        };
    }
    report
}

fn agent_detail(agent: &Value, ordinal: u64, history_dir: &Path, tail: usize) -> Value {
    let sidecar = agent["historyPath"]
        .as_str()
        .map(PathBuf::from)
        .filter(|path| path.is_file())
        .or_else(|| Some(history_dir.join(format!("{ordinal}.json"))).filter(|p| p.is_file()));
    let history = sidecar
        .as_deref()
        .and_then(read_journal)
        .or_else(|| (!agent["history"].is_null()).then(|| agent["history"].clone()));
    let history: Vec<Value> = match history {
        Some(Value::Array(entries)) => entries[entries.len().saturating_sub(tail)..]
            .iter()
            .map(|entry| clip_value(entry, 1_500))
            .collect(),
        Some(Value::Object(object)) => object
            .get("messages")
            .or_else(|| object.get("entries"))
            .and_then(Value::as_array)
            .map(|entries| {
                entries[entries.len().saturating_sub(tail)..]
                    .iter()
                    .map(|entry| clip_value(entry, 1_500))
                    .collect()
            })
            .unwrap_or_default(),
        _ => Vec::new(),
    };
    json!({
        "ordinal": ordinal,
        "found": true,
        "label": clip_value(&agent["label"], TEXT_CHARS),
        "prompt": clip_value(&agent["prompt"], LONG_CHARS),
        "result": clip_value(&agent["result"], LONG_CHARS),
        "error": clip_value(&agent["error"], LONG_CHARS),
        "historyPath": sidecar,
        "history": history,
    })
}

/// The last `n` lines of `path`, reading at most [`TAIL_BYTES`] of it.
fn tail_lines(path: &Path, n: usize) -> Vec<String> {
    let Ok(mut file) = std::fs::File::open(path) else {
        return Vec::new();
    };
    let len = file.metadata().map(|meta| meta.len()).unwrap_or(0);
    let start = len.saturating_sub(TAIL_BYTES);
    if file.seek(SeekFrom::Start(start)).is_err() {
        return Vec::new();
    }
    let mut bytes = Vec::new();
    if file.read_to_end(&mut bytes).is_err() {
        return Vec::new();
    }
    let text = String::from_utf8_lossy(&bytes);
    let mut lines: Vec<&str> = text.lines().collect();
    // A cut first line is partial.
    if start > 0 && !lines.is_empty() {
        lines.remove(0);
    }
    lines[lines.len().saturating_sub(n)..]
        .iter()
        .map(|line| clip(line, TEXT_CHARS))
        .collect()
}

/// Pi extension rows (`subagent-result`, a shell's result, …) in the run
/// journal that mention `id`, newest first, with their delivered output.
fn pi_rows(journal: &RunJournal, chat_id: &str, id: &str, max: usize) -> Vec<Value> {
    let Ok(events) = journal.events_rev(chat_id) else {
        return Vec::new();
    };
    let mut results: HashMap<String, (bool, Option<String>)> = HashMap::new();
    let mut rows = Vec::new();
    for event in events.take(SCAN_EVENTS).flatten() {
        match event.1 {
            // Newest first: a result is read before its call.
            AgentEvent::ToolResult {
                id: call,
                is_error,
                output,
                ..
            } => {
                results.insert(call, (is_error, output));
            }
            AgentEvent::ToolCall {
                id: call,
                call:
                    ToolCall::Mcp {
                        server,
                        tool,
                        input,
                    },
            } if server == zeron_proto::view::PI_EXTENSION_SERVER
                && input
                    .as_ref()
                    .is_some_and(|input| input.to_string().contains(id)) =>
            {
                let (is_error, output) = results.remove(&call).unwrap_or((false, None));
                rows.push(json!({
                    "seq": event.0,
                    "tool": tool,
                    "input": input.as_ref().map(|input| clip_value(input, TEXT_CHARS)),
                    "isError": is_error,
                    "output": output.map(|output| clip(&output, LONG_CHARS)),
                }));
                if rows.len() >= max {
                    break;
                }
            }
            _ => {}
        }
    }
    rows
}

/// The chat's newest failures: error events, failed tool results (named by
/// their call) and runs that ended in an error or were interrupted.
fn recent_events(journal: &RunJournal, chat_id: &str, n: usize) -> Vec<Value> {
    let Ok(events) = journal.events_rev(chat_id) else {
        return Vec::new();
    };
    let mut out: Vec<Value> = Vec::new();
    // Failed results waiting for their call (read after them) to be named.
    let mut unnamed: HashMap<String, usize> = HashMap::new();
    for (seq, event) in events.take(SCAN_EVENTS).flatten() {
        match event {
            AgentEvent::Error { message } if out.len() < n => {
                out.push(json!({ "seq": seq, "type": "error", "message": clip(&message, LONG_CHARS) }));
            }
            AgentEvent::ToolResult {
                id,
                is_error: true,
                output,
                ..
            } if out.len() < n => {
                unnamed.insert(id.clone(), out.len());
                out.push(json!({
                    "seq": seq,
                    "type": "toolError",
                    "callId": id,
                    "output": output.map(|output| clip(&output, LONG_CHARS)),
                }));
            }
            AgentEvent::ToolCall { id, call } => {
                if let Some(index) = unnamed.remove(&id) {
                    out[index]["tool"] = json!(tool_name(&call));
                }
            }
            AgentEvent::Done {
                status: status @ (DoneStatus::Errored | DoneStatus::Interrupted),
                error,
                class,
                ..
            } if out.len() < n => {
                out.push(json!({
                    "seq": seq,
                    "type": "runEnded",
                    "status": status,
                    "error": error.map(|error| clip(&error, LONG_CHARS)),
                    "class": class,
                }));
            }
            _ => {}
        }
        if out.len() >= n && unnamed.is_empty() {
            break;
        }
    }
    out
}

fn tool_name(call: &ToolCall) -> String {
    match call {
        ToolCall::Mcp { server, tool, .. } => format!("{server}:{tool}"),
        ToolCall::Unknown { name, .. } => name.clone(),
        other => serde_json::to_value(other)
            .ok()
            .and_then(|value| value["kind"].as_str().or(value["type"].as_str()).map(str::to_owned))
            .unwrap_or_else(|| "tool".into()),
    }
}

fn clip(text: &str, max: usize) -> String {
    let text = zeron_harness::redact::redact_output(text);
    if text.chars().count() <= max {
        return text;
    }
    let mut out: String = text.chars().take(max).collect();
    out.push('…');
    out
}

/// `value` with every string clipped (and redacted) and every array cut to
/// its newest [`ITEMS`].
fn clip_value(value: &Value, max: usize) -> Value {
    match value {
        Value::String(text) => Value::String(clip(text, max)),
        Value::Array(items) => Value::Array(
            items[items.len().saturating_sub(ITEMS)..]
                .iter()
                .map(|item| clip_value(item, max))
                .collect(),
        ),
        Value::Object(object) => Value::Object(
            object
                .iter()
                // The workflow script is the run's source, not its state.
                .filter(|(key, _)| key.as_str() != "script")
                .map(|(key, item)| (key.clone(), clip_value(item, max)))
                .collect(),
        ),
        other => other.clone(),
    }
}

/// `reply`, or its clipped text once it outgrows [`MAX_REPLY_BYTES`].
fn cap_reply(reply: Value) -> Value {
    let text = reply.to_string();
    if text.len() <= MAX_REPLY_BYTES {
        return reply;
    }
    let mut cut = MAX_REPLY_BYTES;
    while !text.is_char_boundary(cut) {
        cut -= 1;
    }
    json!({
        "found": reply["found"],
        "kind": reply["kind"],
        "truncated": true,
        "note": "The report outgrew the reply cap; ask for less with a smaller tail, one agent, or one id.",
        "preview": &text[..cut],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_project_dir_matches_the_workflows_extension() {
        let root = Path::new("/w");
        // FNV-1a of "/x" over UTF-16 units.
        let mut hash: u32 = 0x811c_9dc5;
        for unit in [b'/' as u32, b'x' as u32] {
            hash ^= unit;
            hash = hash.wrapping_mul(0x0100_0193);
        }
        assert_eq!(short_hash("/x"), format!("{hash:08x}"));
        assert_eq!(short_hash(""), "811c9dc5");
        let dir = project_dir(root, "/home/me/My Repo.v2/");
        assert_eq!(
            dir.file_name().unwrap().to_string_lossy(),
            format!("my-repo-v2-{}", short_hash("/home/me/My Repo.v2/"))
        );
        let dir = project_dir(root, "C:\\Users\\me\\pi-zeron");
        assert!(dir.file_name().unwrap().to_string_lossy().starts_with("pi-zeron-"));
        let dir = project_dir(root, "/");
        assert!(dir.file_name().unwrap().to_string_lossy().starts_with("project-"));
    }

    #[test]
    fn a_run_id_cannot_leave_the_journal_directory() {
        assert!(safe_run_id("review-ms8st9vv-9eh2ch"));
        for bad in ["", "../x", "a/b", "a\\b", "-x", "a.json", &"a".repeat(201)] {
            assert!(!safe_run_id(bad), "{bad}");
        }
    }

    #[test]
    fn strings_and_arrays_are_clipped_and_the_script_dropped() {
        let value = json!({
            "script": "export const meta = {}",
            "text": "word ".repeat(10),
            "items": (0..30).collect::<Vec<_>>(),
        });
        let clipped = clip_value(&value, 10);
        assert!(clipped.get("script").is_none());
        assert_eq!(clipped["text"].as_str().unwrap().chars().count(), 11);
        let items = clipped["items"].as_array().unwrap();
        assert_eq!(items.len(), ITEMS);
        assert_eq!(items.last(), Some(&json!(29)), "the newest items stay");
    }

    #[test]
    fn a_failed_workflow_agent_shows_its_error_log_and_transcript() {
        let dir = tempfile::tempdir().unwrap();
        let workflows = dir.path();
        let cwd = "/home/me/proj";
        let project = project_dir(workflows, cwd);
        std::fs::create_dir_all(project.join("run-1.history")).unwrap();
        std::fs::write(
            project.join("run-1.json"),
            json!({
                "runId": "run-1",
                "workflowName": "review",
                "status": "error",
                "script": "secret source",
                "agents": [
                    {"ordinal": 0, "label": "find", "status": "done", "model": "m"},
                    {"ordinal": 1, "label": "verify", "status": "error", "model": "m",
                     "errorCode": "AGENT_EMPTY_OUTPUT", "error": "Subagent produced no assistant output",
                     "prompt": "Verify it"}
                ],
            })
            .to_string(),
        )
        .unwrap();
        std::fs::write(project.join("run-1.json.bak"), "{}").unwrap();
        std::fs::write(project.join("run-1.log"), "start\nagent 1 failed\n").unwrap();
        std::fs::write(
            project.join("run-1.history").join("1.json"),
            json!([{"role": "assistant", "errorMessage": "400 extra usage required"}]).to_string(),
        )
        .unwrap();

        let runs = list_runs(workflows, cwd, 10);
        assert_eq!(runs.len(), 1, "the .bak copy is not a run");
        assert_eq!(runs[0]["failedAgents"][0]["errorCode"], "AGENT_EMPTY_OUTPUT");
        assert_eq!(find_journal(workflows, Some("/elsewhere"), "run-1"), Some(project.join("run-1.json")));

        let report = workflow_detail(&project.join("run-1.json"), Some(1), 40);
        assert!(!report.to_string().contains("secret source"));
        assert_eq!(report["logTail"], json!(["start", "agent 1 failed"]));
        assert_eq!(report["agent"]["prompt"], "Verify it");
        assert!(report["agent"]["history"].to_string().contains("400 extra usage required"));
    }

    #[test]
    fn a_long_log_yields_only_its_last_lines() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("big.log");
        let lines: Vec<String> = (0..40_000).map(|i| format!("line {i}")).collect();
        std::fs::write(&path, lines.join("\n")).unwrap();
        assert_eq!(tail_lines(&path, 2), ["line 39998", "line 39999"]);
        assert!(tail_lines(&dir.path().join("missing.log"), 2).is_empty());
    }

    #[test]
    fn the_run_journal_yields_a_delivered_subagent_result_and_recent_errors() {
        let dir = tempfile::tempdir().unwrap();
        let journal = RunJournal::open(dir.path()).unwrap();
        let events = [
            AgentEvent::ToolCall {
                id: "c1".into(),
                call: ToolCall::Mcp {
                    server: zeron_proto::view::PI_EXTENSION_SERVER.into(),
                    tool: "subagent-result".into(),
                    input: Some(json!({"jobId": "j1", "state": "failed"})),
                },
            },
            AgentEvent::ToolResult {
                id: "c1".into(),
                is_error: true,
                output: Some("Stderr: boom".into()),
                diff: None,
            },
            AgentEvent::Error {
                message: "provider said no".into(),
            },
        ];
        for event in &events {
            journal.append("chat", event).unwrap();
        }
        let rows = pi_rows(&journal, "chat", "j1", 5);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["output"], "Stderr: boom");
        assert_eq!(rows[0]["isError"], true);
        assert!(pi_rows(&journal, "chat", "j2", 5).is_empty());

        let recent = recent_events(&journal, "chat", 5);
        assert_eq!(recent[0]["type"], "error");
        assert_eq!(recent[1]["type"], "toolError");
        assert_eq!(recent[1]["tool"], "pi:subagent-result");
    }

    #[test]
    fn the_overview_lists_live_work_and_a_miss_names_candidates() {
        let dir = tempfile::tempdir().unwrap();
        let journal = RunJournal::open(dir.path().join("journals")).unwrap();
        let mut ui = ExtensionUiState {
            connected: true,
            ..Default::default()
        };
        ui.channels.insert(
            FLEET_CHANNEL.into(),
            json!({"entries": [{"key": "j1", "state": "failed", "label": "fix tests",
                                "activity": ["ran cargo", "error: E0425"]}]}),
        );
        let params = |id: Option<&str>| InspectParams {
            chat_id: "chat".into(),
            cwd: Some("/home/me/proj".into()),
            id: id.map(str::to_owned),
            ..Default::default()
        };
        let overview = inspect(dir.path(), &ui, &journal, &params(None));
        assert_eq!(overview["subagents"][0]["key"], "j1");
        assert_eq!(overview["live"], true);

        let job = inspect(dir.path(), &ui, &journal, &params(Some("j1")));
        assert_eq!(job["kind"], "subagent");
        assert_eq!(job["live"]["activity"][1], "error: E0425");

        let miss = inspect(dir.path(), &ui, &journal, &params(Some("nope")));
        assert_eq!(miss["found"], false);
        assert_eq!(miss["candidates"]["subagents"], json!(["j1"]));
    }
}
