//! The tool catalog and its dispatch.
//!
//! Every tool is a thin composition of engine reads/writes from
//! [`Zeron`]; the only logic that lives here is argument resolution (chat
//! by prefix, project by path), sender attribution, and the "how do I
//! deliver a message to a chat in this state" choice the composer makes
//! for humans.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Context;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use zeron_doc::SessionCommandPayload;
use zeron_proto::{
    Chat, ChatConfig, HarnessId, ReasoningLevel, RunRequest, SandboxLevel, Session, SessionStatus,
    Space, UserInputAnswer, WorktreeSpec,
};

use crate::transcript::{RenderOptions, RenderedMessage, render_entries};
use crate::zeron::{HarnessInfo, TurnOutcome, Zeron, session_for, short};

/// Default and ceiling for the blocking waits.
const MAX_BATCH: usize = 32;
/// Longest first prompt or message, in characters (a JSON-Schema `maxLength`).
const MAX_PROMPT_CHARS: usize = 100_000;
const DEFAULT_WAIT: Duration = Duration::from_secs(600);
const MAX_WAIT: Duration = Duration::from_secs(3600);
/// A session row older than this is not trusted to still be working
/// (the UI's staleness window): a crashed host must not read as busy forever.
const SESSION_STALE: chrono::Duration = chrono::Duration::seconds(45);

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolDef {
    pub name: &'static str,
    pub description: &'static str,
    pub input_schema: Value,
}

pub struct Tools {
    zeron: Arc<Zeron>,
    // Remember successful sends on this MCP connection so wait_for_turn after
    // wait:false also waits for a newly created chat with no session row yet.
    pending_turns: tokio::sync::Mutex<HashMap<String, Arc<PendingTurn>>>,
    // Sessions being started on this connection, by orchestrator root: a
    // batch runs against one chats() snapshot and new rows fold in late, so
    // the live-session cap counts these on top of the snapshot.
    reserved: std::sync::Mutex<HashMap<String, Slots>>,
}

/// The orchestrator tree the calling chat is in, as its project says.
struct Tree {
    root: Chat,
    policy: zeron_proto::orchestrator_project::TreePolicy,
}

/// How long `create_chat` waits for a GitHub preflight before it starts a
/// writer unchecked.
const PREFLIGHT_WAIT: Duration = Duration::from_secs(20);

#[derive(Default)]
struct Slots {
    /// create_chat calls between their cap check and their return.
    held: usize,
    /// Sessions this connection created that the snapshot has not shown yet.
    unfolded: std::collections::HashSet<String>,
}

/// One session slot of a tree, held while it is being started.
struct Reservation<'a> {
    tools: &'a Tools,
    root: String,
    created: Option<String>,
}

impl Drop for Reservation<'_> {
    fn drop(&mut self) {
        let mut reserved = self.tools.reserved();
        if let Some(slots) = reserved.get_mut(&self.root) {
            slots.held = slots.held.saturating_sub(1);
            slots.unfolded.extend(self.created.take());
        }
    }
}

fn chat_key_schema(extra: Value) -> Value {
    let mut properties = json!({
        "chat": {
            "type": "string",
            "description": "Chat id, unique id prefix, or exact title."
        }
    });
    if let (Some(base), Some(more)) = (properties.as_object_mut(), extra.as_object()) {
        for (k, v) in more {
            base.insert(k.clone(), v.clone());
        }
    }
    json!({ "type": "object", "properties": properties, "required": ["chat"] })
}

fn device_schema() -> Value {
    json!({ "type": "string", "description": "Host device id or exact name. Omit for the local engine." })
}

fn catalog() -> Vec<ToolDef> {
    let mut tools = vec![
        ToolDef {
            name: "whoami",
            description: "Which chat and device this server speaks for, plus the engine's workspace mode. Call this first when you need to know your own chat id.",
            input_schema: json!({ "type": "object", "properties": {} }),
        },
        ToolDef {
            name: "list_devices",
            description: "Devices in this workspace (the local engine's device is flagged). Chats and projects are hosted on a device.",
            input_schema: json!({ "type": "object", "properties": {} }),
        },
        ToolDef {
            name: "list_projects",
            description: "Projects: folders on specific devices. Filter by device (id or name); omit for all projects. Use ids when names or paths repeat.",
            input_schema: json!({ "type": "object", "properties": { "device": device_schema() } }),
        },
        ToolDef {
            name: "register_project",
            description: "Register an existing folder as a project, so sessions can work in it. Pass a path (absolute, or starting with ~) or a short name like \"t3code\": a short name is looked for beside the projects you already have, at the top of each drive, and in ~/codebase, ~/code, ~/projects, ~/src, ~/dev and ~/repos. It registers a folder only when exactly one matches and never creates one; with several matches it lists them, and you ask the user which. In a git folder every writing session gets its own worktree; a folder that is not a git repository is shared by its writers, so run one writer at a time. Returns the project: use its id as `project` in create_chat. dry_run lists the matches without registering.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "project": { "type": "string", "description": "A path (absolute, or ~/...), or a short folder name such as t3code." },
                    "device": device_schema(),
                    "name": { "type": "string", "description": "Name to show for the project. Default: the folder's name." },
                    "dry_run": { "type": "boolean", "default": false, "description": "List the folders that match, register nothing." }
                },
                "required": ["project"]
            }),
        },
        ToolDef {
            name: "github_preflight",
            description: "Before a writing session starts in a GitHub project, check that it can push its branch and see CI: whether git's own credential may push (the one a writer pushes with), the gh login and its token's scopes, whether Actions is on, which workflows start for a push to a zeron/ branch (many start only for pull requests, so a draft pull request must be opened right after the first push), self-hosted runners, and a billing block on Actions that makes every job fail at once. Returns ok, blockers, warnings and a one-line verdict. Read-only; reports are reused for a few minutes (force: true checks again). Defaults to your own project.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "project": { "type": "string", "description": "Project id, path or name. Default: the project of your own chat." },
                    "cwd": { "type": "string", "description": "A checkout of that project. Default: the project folder." },
                    "branch": { "type": "string", "description": "The branch a writer would push; its triggers are checked. Default: a zeron/ branch." },
                    "force": { "type": "boolean", "default": false, "description": "Check again instead of reusing a recent report." }
                }
            }),
        },
        ToolDef {
            name: "list_harnesses",
            description: "Agent harnesses available on the chosen device; Zeron runs only Pi. Omit device for the local engine.",
            input_schema: json!({ "type": "object", "properties": { "device": device_schema() } }),
        },
        ToolDef {
            name: "list_models",
            description: "Models a harness offers on the chosen device (local engine when omitted). Model ids are harness-specific strings; pass one to create_chat.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "harness": { "type": "string", "description": "Harness id: \"pi\" (the only harness)." },
                    "device": device_schema()
                },
                "required": ["harness"]
            }),
        },
        ToolDef {
            name: "list_chats",
            description: "Chats with their project, host device, harness/model, live status, and last activity. Newest activity first.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "project": { "type": "string", "description": "Only chats in this project (id, path, or name)." },
                    "device": { "type": "string", "description": "Only chats hosted on this device (id or name)." },
                    "include_archived": { "type": "boolean", "default": false },
                    "parent": { "type": "string", "description": "Only chats created by this chat (id, prefix, or title) — e.g. your own id to list the chats you spawned." },
                    "limit": { "type": "integer", "minimum": 1, "maximum": 500, "default": 50 }
                }
            }),
        },
        ToolDef {
            name: "get_chat",
            description: "One chat's metadata, live status, and any question its agent is currently blocked on.",
            input_schema: chat_key_schema(json!({})),
        },
        ToolDef {
            name: "create_chat",
            description: "Create a standalone session visible in Sessions with kind chat, or a child with kind side. Omitted kind preserves the default: side with an origin/parent, standalone otherwise. A side chat cannot create chats, except in an orchestrator's tree, where sessions may start sessions up to 4 levels deep and default to their parent's harness. Select a project and/or device; catalogs are validated on that host. Set write: true for a session that changes code (in a git project it works in its own new worktree on a zeron/<name> branch, so parallel sessions never touch each other's files), false for one that only reads, searches and reports (every edit is blocked; it runs in the project folder). Claude Fable and GPT Astra class models are reserved for sessions the user starts: a session you create cannot use one, and when your own chat runs on one you must pass a different model. Optionally send a first prompt and wait for the reply. Returns the new chat id. For parallel delegation use create_chats, or leave wait=false on every launch and wait only after all chats have been started.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "kind": { "type": "string", "enum": ["chat", "side"], "description": "chat: standalone, no parent allowed. side: requires parent or origin. Omit for legacy defaults." },
                    "project": { "type": "string", "description": "Project id, path, or name. Belongs to a specific device; when device is supplied, search only there. Optional." },
                    "device": { "type": "string", "description": "Host device (id or name). With project, it must own that project. Without either, use the local engine." },
                    "parent": { "type": "string", "description": "Parent chat to record (id, prefix, or title). Defaults to the chat you are speaking from." },
                    "harness": { "type": "string", "description": "Harness id; Pi is the only harness, so omit it or pass \"pi\"." },
                    "model": { "type": "string", "description": "Model id from list_models on the chosen device. Omit for the harness default. Claude Fable and GPT Astra class models are reserved for the user's own sessions and are refused here; when your chat runs on one, always pass a different model." },
                    "reasoning": { "type": "string", "enum": THINKING_LEVELS, "description": "Thinking level: off, minimal, low, medium, high, xhigh or max. Omit for the model's default. Pi runs the closest level the model supports at or below it." },
                    "write": { "type": "boolean", "default": true, "description": "true: the session may change files, and in a git project gets its own worktree. false: read-only; every edit is blocked." },
                    "sandbox": { "type": "string", "enum": ["read-only", "workspace-write", "danger-full-access"], "default": "workspace-write", "description": "Usually leave unset and use write. read-only only with write false." },
                    "title": { "type": "string", "description": "Sidebar title. Otherwise the engine titles it from the first exchange." },
                    "branch": { "type": "string", "description": "With write true in a git project: the branch or commit the new worktree starts from (default: the project's current HEAD). Otherwise a branch label to record on the chat." },
                    "cwd": { "type": "string", "description": "Working directory (an existing folder or worktree path). Defaults to the project folder; without a project, a cwd that is a project's folder joins that project." },
                    "prompt": { "type": "string", "maxLength": MAX_PROMPT_CHARS, "description": "First message to send right away." },
                    "wait": { "type": "boolean", "default": false, "description": "With prompt: block until the first turn finishes and return the reply." },
                    "timeout_secs": { "type": "integer", "minimum": 1, "maximum": 3600, "default": 600 }
                }
            }),
        },
        ToolDef {
            name: "read_chat",
            description: "Read a chat's transcript as plain messages (newest window by default). Tool calls are summarized one per line; \"pi · …\" rows are Zeron's records of extension UI (advisor, sub-agents, jobs), not calls the chat made, and long rows are truncated.",
            input_schema: chat_key_schema(json!({
                "limit": { "type": "integer", "minimum": 1, "maximum": 500, "default": 40, "description": "How many messages, counted from the newest." },
                "offset": { "type": "integer", "minimum": 0, "default": 0, "description": "Skip this many newest messages (paging backwards)." },
                "include_reasoning": { "type": "boolean", "default": false },
                "include_tools": { "type": "boolean", "default": true }
            })),
        },
        ToolDef {
            name: "send_message",
            description: "Send a message to a chat. Messages are attributed to your chat. mode 'auto' starts a turn when the chat is idle, steers a running turn through its live mailbox (at the next supported input boundary without interrupting the agent). Mode 'queue' puts it in the chat's queue: a running turn takes queued text as steering; otherwise it waits for the turn to end. With wait=true, blocks until the turn finishes and returns the assistant's reply. For parallel work use send_messages, or send to all chats with wait=false before waiting.",
            input_schema: chat_key_schema(json!({
                "text": { "type": "string", "maxLength": MAX_PROMPT_CHARS },
                "mode": { "type": "string", "enum": ["auto", "run", "steer", "queue"], "default": "auto" },
                "wait": { "type": "boolean", "default": false },
                "timeout_secs": { "type": "integer", "minimum": 1, "maximum": 3600, "default": 600 }
            })),
        },
        ToolDef {
            name: "wait_for_turn",
            description: "Block until a chat is no longer working: returns completed, awaitingInput (answer with respond_to_input), errored, or timedOut, with the newest assistant message. A session whose status reads retrying (the provider failed, Pi retries by itself) counts as still working, so the wait continues. When errored, failure.class says why when known: retry later on rateLimited or providerOverloaded, ask the user on authRequired or quotaExhausted.",
            input_schema: chat_key_schema(json!({
                "timeout_secs": { "type": "integer", "minimum": 1, "maximum": 3600, "default": 600 }
            })),
        },
        ToolDef {
            name: "wait_for_turns",
            description: "Block until the FIRST of several chats is no longer working (a retrying session is still working), then return that chat's outcome (completed, awaitingInput, errored) with its newest assistant message, and where the others stand. Without chats it watches the chats you started that are still working or that you just sent work to. Call it again for the next one; timedOut means none finished in time.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "chats": {
                        "type": "array",
                        "items": { "type": "string" },
                        "maxItems": MAX_BATCH,
                        "description": "Chat ids, unique id prefixes, or exact titles. Omit for your own running sessions."
                    },
                    "timeout_secs": { "type": "integer", "minimum": 1, "maximum": 3600, "default": 600 }
                }
            }),
        },
        ToolDef {
            name: "interrupt_chat",
            description: "Stop the chat's running turn. Only the current turn ends and its queued messages are cleared; its sub-agents and background shells keep running.",
            input_schema: chat_key_schema(json!({})),
        },
        ToolDef {
            name: "respond_to_input",
            description: "Answer a question the chat's agent is blocked on (see get_chat / wait_for_turn pendingInput). Labels are the option strings, or free text for open questions.",
            input_schema: chat_key_schema(json!({
                "request_id": { "type": "string", "description": "The pending request id. Defaults to the chat's current pending question." },
                "answers": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "question_id": { "type": "string" },
                            "labels": { "type": "array", "items": { "type": "string" } }
                        },
                        "required": ["question_id", "labels"]
                    }
                }
            })),
        },
        ToolDef {
            name: "archive_chat",
            description: "Archive a chat (hide it from the active sidebar list); pass archived=false to restore. Use it to tidy up chats you created.",
            input_schema: chat_key_schema(json!({
                "archived": { "type": "boolean", "default": true }
            })),
        },
    ];
    for (name, single, description) in [
        (
            "create_chats",
            "create_chat",
            "Create multiple standalone or side chats concurrently (kind per request). Put each chat's prompt in its request to start all work together. Prefer this for parallel delegation, including harnesses that execute tool calls sequentially. Each request has create_chat arguments; wait defaults to false. Results preserve request order and include per-request errors; successful requests are not rolled back.",
        ),
        (
            "send_messages",
            "send_message",
            "Send messages to multiple independent chats concurrently. Prefer this to delegate parallel work to existing chats. Each request has send_message arguments; wait defaults to false. If wait=true, requests still run concurrently. Results preserve request order and include per-request errors; successful sends are not rolled back. Do not include dependent messages to the same chat.",
        ),
    ] {
        let item_schema = tools
            .iter()
            .find(|tool| tool.name == single)
            .unwrap()
            .input_schema
            .clone();
        tools.push(ToolDef {
            name, description,
            input_schema: json!({
                "type": "object",
                "properties": {"requests": {"type": "array", "minItems": 1, "maxItems": MAX_BATCH, "items": item_schema}},
                "required": ["requests"],
            }),
        });
    }
    tools.push(ToolDef {
        name: SHOW_VISUAL,
        description: SHOW_VISUAL_DESCRIPTION,
        input_schema: json!({
            "type": "object",
            "properties": {
                "script": {
                    "type": "string",
                    "description": "The Luau visual script: setup at the top level, ending with `return function() return ui.column { ... } end`."
                },
                "title": { "type": "string", "description": "Short name for the visual." },
                "assets": {
                    "type": "object",
                    "additionalProperties": { "type": "string" },
                    "description": "Files the script uses, by the name it uses them: { \"prism.glb\": \"models/prism.glb\", \"sky.hdr\": \"/tmp/sky.hdr\" }. Paths are on this machine, relative to your working directory. glTF models (.glb, or .gltf with everything embedded), PNG, JPEG, WebP, and Radiance .hdr images; at most 16 files, 8 MB each, 10 MB together."
                }
            },
            "required": ["script", "title"]
        }),
    });
    tools.push(ToolDef {
        name: BOARD_POST,
        description: "Post to your folder's message board: a finding (a fact about the code or task), a discovery (a command, tool or shortcut that works) or a blocker (something stopping you that others may hit or can fix). Only for a session in an orchestrator's tree. Each folder has its own board: the post reaches the other sessions of your folder and the orchestrator by itself (a running session at its next step; an idle one reads it when it next starts), never sessions in other folders. The orchestrator posts to one folder's board at a time, naming a session there. Post only what they need now; no progress reports.",
        input_schema: json!({
            "type": "object",
            "properties": {
                "kind": { "type": "string", "enum": ["finding", "discovery", "blocker"] },
                "text": { "type": "string", "maxLength": zeron_proto::board::BOARD_MAX_TEXT, "description": "The post, self-contained: name files, commands and errors exactly." },
                "session": { "type": "string", "description": "The orchestrator only (required there): a session of your tree, by id or title; the post goes on that session's folder board. Sessions omit it." }
            },
            "required": ["kind", "text"]
        }),
    });
    tools.push(ToolDef {
        name: BOARD_READ,
        description: "Read your folder's message board: the posts your folder's sessions and the orchestrator made there, oldest first (the orchestrator sees every folder's board). Call it when you start and after a resume or compaction. Pass since (the last id you saw) for only newer posts.",
        input_schema: json!({
            "type": "object",
            "properties": {
                "since": { "type": "string", "description": "An entry id: only posts after it." },
                "limit": { "type": "integer", "minimum": 1, "maximum": 100, "default": 30, "description": "The newest this many." }
            }
        }),
    });
    tools.push(ToolDef {
        name: PROJECT_READ,
        description: "Read your orchestrator project: the goal, the user's standing instructions for every session, whether the user paused it, and the project memory (facts the orchestrator saved). Only for a chat in an orchestrator's tree. Your prompt has them as of your start; call this for the latest.",
        input_schema: json!({ "type": "object", "properties": {} }),
    });
    tools.push(ToolDef {
        name: PROJECT_REMEMBER,
        description: "Orchestrator only: save a fact to the project memory, which every later session of the tree gets in its prompt. Save what holds for the whole project and is not in the code (a build command that works, a constraint the user gave, a decision made), one fact per note. Not for progress or a session's task.",
        input_schema: json!({
            "type": "object",
            "properties": {
                "text": { "type": "string", "maxLength": zeron_proto::orchestrator_project::NOTE_MAX_CHARS, "description": "The fact, self-contained." }
            },
            "required": ["text"]
        }),
    });
    tools.push(ToolDef {
        name: PROJECT_FORGET,
        description: "Orchestrator only: drop a project memory note that is wrong or no longer holds (its id from project_read).",
        input_schema: json!({
            "type": "object",
            "properties": { "id": { "type": "string" } },
            "required": ["id"]
        }),
    });
    tools.push(ToolDef {
        name: PROJECT_UPDATE,
        description: "Orchestrator only: set the project's goal (one line) or its standing instructions, when the user asks for it. The instructions replace the old ones whole, so read them first and keep what the user did not ask to change.",
        input_schema: json!({
            "type": "object",
            "properties": {
                "goal": { "type": "string", "maxLength": zeron_proto::orchestrator_project::GOAL_MAX_CHARS },
                "instructions": { "type": "string", "maxLength": zeron_proto::orchestrator_project::INSTRUCTIONS_MAX_CHARS }
            }
        }),
    });
    tools.push(ToolDef {
        name: MERGE_PREVIEW,
        description: "Orchestrator only. Read-only preview of merging one of your sessions' worktree branch into the lead branch: files it changed, commits ahead/behind, uncommitted edits in its worktree (these are NOT part of the merge) and whether the merge would conflict (git merge-tree). Changes nothing.",
        input_schema: chat_key_schema(json!({
            "base": {
                "type": "string",
                "description": "Lead ref to compare against. Default: the branch checked out in the session's project folder."
            }
        })),
    });
    for tool in &mut tools {
        forbid_extra(&mut tool.input_schema);
    }
    tools
}

/// Make every object schema with `properties` closed (`additionalProperties:
/// false`), nested array items included, to match the arg structs' serde
/// `deny_unknown_fields`. A schema that already says (the `assets` map) is
/// left alone.
fn forbid_extra(schema: &mut Value) {
    let Some(object) = schema.as_object_mut() else {
        return;
    };
    if object.contains_key("properties") && !object.contains_key("additionalProperties") {
        object.insert("additionalProperties".into(), json!(false));
    }
    if let Some(properties) = object.get_mut("properties").and_then(Value::as_object_mut) {
        properties.values_mut().for_each(forbid_extra);
    }
    if let Some(items) = object.get_mut("items") {
        forbid_extra(items);
    }
}

/// The project of the CALLING chat's orchestrator tree; only its root
/// changes it.
const PROJECT_READ: &str = "project_read";
const PROJECT_REMEMBER: &str = "project_remember";
const PROJECT_FORGET: &str = "project_forget";
const PROJECT_UPDATE: &str = "project_update";
/// Read-only merge preview of one of the orchestrator's sessions.
const MERGE_PREVIEW: &str = "merge_preview";

/// The message board of the CALLING chat's orchestrator tree, offered only
/// to an agent Zeron launched (`ZERON_CHAT_ID` set).
const BOARD_POST: &str = "board_post";
const BOARD_READ: &str = "board_read";
/// Tools that act for the calling chat.
const IN_CHAT_ONLY: [&str; 8] = [
    SHOW_VISUAL,
    BOARD_POST,
    BOARD_READ,
    PROJECT_READ,
    PROJECT_REMEMBER,
    PROJECT_FORGET,
    PROJECT_UPDATE,
    MERGE_PREVIEW,
];

/// Shows a page in the CALLING chat, so it is offered only to an agent that
/// Zeron launched (`ZERON_CHAT_ID` set).
const SHOW_VISUAL: &str = "show_visual";

const SHOW_VISUAL_DESCRIPTION: &str = "\
Show an interactive visual (chart, table, dashboard, diagram, simulation, 3D view) inline in this \
chat, above your final text reply; call it before writing that reply. Use it when a visual says more \
than prose. The reader already sees it, so the reply should not announce, locate, or restate it: add \
only what the visual doesn't say.\n\
\n\
A visual is a Luau script (Lua 5.1 plus `+=`, `continue`, and `if` expressions). Zeron runs it in a \
sandbox (no I/O or network; math.random is seeded, so data is the same on every render) and renders \
what it returns with Zeron's own native interface. Build data and state at the top level, then return \
a view function. Zeron calls the view again after every handler, so handlers just assign to state:\n\
\n\
local s = ui.state { metric = \"p95\" }\n\
local data = { { region = \"us-east\", p95 = 182, p99 = 240 }, { region = \"eu-west\", p95 = 140, p99 = 210 } }\n\
return function()\n\
  return ui.column { gap = 16,\n\
    ui.text { \"Latency by region\", size = \"lg\", weight = 600 },\n\
    ui.tabs { options = { \"p95\", \"p99\" }, value = s.metric, on_change = function(v) s.metric = v end },\n\
    ui.chart { type = \"bar\", height = 220, unit = \"ms\",\n\
      x = ui.map(data, function(d) return d.region end),\n\
      series = { { name = s.metric, values = ui.map(data, function(d) return d[s.metric] end) } } },\n\
  }\n\
end\n\
\n\
Zeron runs the script before showing it (setup, render, every draw, and each tab, toggle, slider, and \
button once) and returns the first error with its line; fix it and call again.\n\
\n\
Nodes are props tables: children are the array part, strings are plain text, nil and false children \
are skipped, and lists of nodes flatten (so ui.map results can be children). Unknown props are errors.\n\
- ui.column { gap = 8, align = \"start|center|end\", ... }, ui.row { gap = 12, align, justify = \
\"start|center|end|between\", wrap = true, ... }, ui.grid { columns = 3, gap = 16, ... }. Any node \
takes width = px or grow = true inside a row (sliders, charts, tables, canvases, and scenes fill a \
row's spare width by default), and key = \"name\" to keep its hover or camera when the tree changes \
shape. A row or column with on_click = function() end is clickable; hover = true gives \
it the hover wash.\n\
- ui.text { \"plain \", ui.span { \"bold\", weight = 600, color = \"accent\", mono = true }, size = \
\"xs|sm|body|lg|xl|2xl\", weight = 400-700, color, mono, italic, align, truncate }, or ui.text \"string\".\n\
- ui.stat { value = \"182 ms\", label = \"p95 latency\", delta = \"-12%\", tone = \"success\" }: a headline \
figure over a muted label; tone colors the delta.\n\
- ui.table { columns = { \"Region\", { label = \"p95\", align = \"end\" } }, rows = { { \"us-east\", 182 }, \
... } }: laid out like a markdown table (bold header, hairline row rules), numbers right-aligned; cells \
are strings, numbers, or nodes.\n\
- ui.chart { type = \"bar|line|area|scatter\", height = 220, x = { labels }, series = { { name, values = \
{ ... }, color } }, stacked, horizontal (bar), min, max, unit = \"ms\", prefix = \"$\", decimals, legend }. \
Scatter series take points = { { x, y }, ... } (x_unit, x_decimals). Axes, gridlines, the legend, and a \
hover readout are built in.\n\
- ui.rule {} is a hairline divider; ui.space { size = 16 }; ui.link { label, url = \"https://...\" } \
opens like a chat link.\n\
- Controls: ui.tabs { options = { ... }, value, on_change = function(option) end }, ui.toggle { label, \
value = bool, on_change = function(on) end }, ui.slider { label, value, min, max, step, unit, on_change \
= function(n) end }, ui.button { label, on_click = function() end, primary = bool }.\n\
- ui.canvas { height = 240, draw = function(g, w, h, t) ... end, animate = bool, on_click = \
function(x, y) end }: 2D drawing in pixels, origin top-left, w = the reply column's width. \
g:line(x1, y1, x2, y2, { color, width, dash }), g:polyline(pts, { color, width }), g:polygon(pts, \
{ fill, stroke, width }), g:rect(x, y, w, h, { fill, stroke, radius }), g:circle(x, y, r, { fill, \
stroke }), g:arc(x, y, r, a0, a1, { color } or { fill, inner = radius } for pie and donut sectors; \
radians clockwise from 3 o'clock), g:text(x, y, str, { size, color, weight, align = \
\"start|center|end\", baseline = \"top|middle|bottom\", mono }). pts are { x1, y1, x2, y2, ... } or \
{ { x, y }, ... }. g.pointer is { x, y, down } while the pointer is over the canvas (draw hover \
effects with it); t is seconds, and animate = true redraws every frame while visible.\n\
- ui.scene { height = 320, draw = function(s, t) ... end, orbit = true, spin = 0.3 (turntable \
radians per second), camera = { yaw = 0.6, pitch = 0.45, zoom = 1 }, animate = bool }: real 3D, y up, \
rendered on the GPU with depth, soft shadows from a key light, and bloom. Zeron frames the camera on \
your geometry and the reader drags to orbit. Solids (box, sphere, cylinder, surface, mesh) take \
material props: roughness (0 mirror to 1 matte), metal (0-1), glow (emits its color and blooms; \
use for lamps, beams, highlights), glass = true (refracts what's behind it; ior = 1.5, dispersion \
= 0-1 splits it into a rainbow, as a prism does). Build real forms from several primitives (legs, \
tops, rims), not single boxes, and give materials so surfaces read as wood, metal, or glass. s:box { at = { x, y, z }, size = { w, h, d } or n, color }, s:sphere { at, radius, color }, \
s:cylinder { at (base center), radius, height, color }, s:surface { x = { min, max }, z = { min, max }, \
f = function(x, z) return y end or values = { { ... } }, detail = 32, colors = { \"chart1\", \"chart3\" } \
(a ramp by height), wire = bool }, s:mesh { vertices = { { x, y, z }, ... }, faces = { { 1, 2, 3 }, ... }, \
color }, s:line(a, b, { color, width }), s:path(points, { color, width }), s:point(at, { radius, \
color, hint = \"shown on hover\" }), s:label(at, text, { color, size }), s:grid { size = 10, step = 1 }, \
s:axes { size = 1, labels = { \"x\", \"y\", \"z\" } }. Physical colors may be \"#rrggbb\" in scenes \
(a material's own color: gold, glass, wood), and only there.\n\
- Assets: pass real files in show_visual's assets (name -> path) and scripts use them by name. \
s:model { asset = \"prism.glb\", at, rotate = { x, y, z } degrees, scale, fit = 2 (center it and size \
its longest side), color and material props override the model's own } draws a glTF model with its \
materials and textures. Solids take texture = \"wood.jpg\" (multiplied by color; mesh takes uvs = { { u, v \
}, ... } per vertex). s:environment { asset = \"studio.hdr\", intensity = 1, background = false } lights \
the scene from an equirectangular image (an HDR is best) that metal and glass reflect and glass bends; \
background = true also draws it behind the scene. s:sprite { at, size = 48 (pixels), color, glow = 1, \
asset = \"flare.png\" } adds a camera-facing light (a flare or glint; a soft round glow without an \
asset) that blooms. ui.shader takes textures = { \"rock.jpg\", ... } as iTexture0-iTexture3 \
(repeating, mipmapped). Find or make assets with your own tools (download, convert KTX2 or EXR to PNG \
or HDR) before calling.\n\
- ui.shader { height = 360, code = [[ GLSL ]], buffers = { [[ GLSL ]], ... }, inputs = { speed = \
s.speed }, blend = \"alpha|light|opaque\", bloom = 0-3, orbit = bool, spin, camera = { yaw, pitch, \
zoom }, animate = true }: GPU shaders for what geometry can't express: raymarched SDFs and \
fractals, light and optics (beams, refraction, dispersion), volumes, fluids, particles, generative \
art. Write each pass as on Shadertoy: void mainImage(out vec4 fragColor, in vec2 fragCoord), with \
iResolution (vec3: width and height in pixels, then the display's pixel density), iTime, \
iTimeDelta, iFrame, iMouse (pixels from the bottom-left; xy where the pointer is, zw where it went \
down while it's held, negative otherwise), iCamera (vec4 yaw, pitch, zoom, 0; orbit = true lets the reader drag \
it), every inputs name as a float (bind them to state so sliders and toggles drive the shader), \
iNoise (a tiling 256x256 random RGBA texture), and iChannel0-iChannel3 with iChannelResolution: \
buffers run first, in order, each into a float texture that every pass reads as iChannel<i> (an \
earlier buffer's current frame; a buffer's own previous frame), for multi-pass work like blur, \
trails, accumulation, and simulation. The shader sits on the chat, never in a box: don't paint a \
background, vignette, or floor color. By default (blend = \"light\") the output is emitted light: \
brightness is opacity and black is clear, right for beams, glows, sparks, and light through glass. \
For solid shaded objects (fractals, SDF bodies) set blend = \"alpha\" and write alpha 0 where the ray \
misses. bloom = 0.6-1.5 adds a soft glow around bright light. \
The width varies with the reader (a phone is about 350px wide, often narrower than the shader is \
tall): scale the scene by min(iResolution.x, iResolution.y), not one axis, and keep the subject \
centered with room on both sides, so it fits and stays centered at any aspect. \
GLSL errors come back with the pass and line number.\n\
- Helpers: ui.scale(d0, d1, r0, r1) returns a mapping function; ui.ticks(lo, hi, n); ui.fmt(n, { unit, \
prefix, decimals }) groups digits; ui.map(list, function(v, i) end); ui.range(a, b, step); ui.clamp; \
ui.lerp; ui.series(i) is \"chart<i>\"; ui.color(token, alpha); ui.mix(a, b, t). print(...) output comes \
back to you.\n\
\n\
Colors are theme tokens, never hex or color names (except scene materials): text; muted (labels, axes, captions); faint \
(tertiary notes); accent (the one thing to notice); on-accent (text on an accent mark); success, \
warning, danger (status only); border (hairlines, gridlines); chart1, chart2, chart3, chart4, \
chart5, chart6 (series, in order; \
chart1 is the accent). They follow the reader's theme, light or dark, on frosted glass or an opaque \
surface, so never assume a background color.\n\
\n\
Style, enforced: layout nodes have no fill, border, shadow, padding, or font props. A visual is part \
of the reply, not a widget, so lay content directly on the chat with type, spacing, and alignment: no \
cards, tiles, boxed panels, banners, or canvas backgrounds. Stats are a row of plain ui.stat figures; \
a chart is a plain heading, then the chart; separate sections with gap or one ui.rule. Text renders \
in Zeron's interface font at the reader's text size: body for most text, sm and xs for labels and \
notes, lg and xl for headings, 2xl for a headline figure. The width is the reply column (about 720px, \
as narrow as 360px), so lay out fluidly with grow, wrap, and grid, and give charts, canvases, and \
scenes heights of 160-420px.";

/// Read `show_visual`'s asset files (paths relative to the agent's working
/// directory), base64-encoded by name for the engine.
async fn read_visual_assets(
    assets: &std::collections::BTreeMap<String, String>,
) -> anyhow::Result<serde_json::Map<String, Value>> {
    use base64::Engine as _;
    use zeron_proto::{
        VISUAL_MAX_ASSET_BYTES as MAX_ASSET_BYTES, VISUAL_MAX_ASSETS as MAX_ASSETS,
        VISUAL_MAX_TOTAL_ASSET_BYTES as MAX_TOTAL_ASSET_BYTES,
        valid_visual_asset_name as valid_name,
    };
    anyhow::ensure!(
        assets.len() <= MAX_ASSETS,
        "a visual takes at most {MAX_ASSETS} assets"
    );
    let mut out = serde_json::Map::new();
    let mut total = 0;
    let mut seen = std::collections::HashSet::new();
    for (name, path) in assets {
        anyhow::ensure!(
            valid_name(name),
            "asset name '{name}' must be a plain file name (letters, digits, '.', '-', '_')"
        );
        // Stored as files, and file systems often ignore case.
        anyhow::ensure!(
            seen.insert(name.to_ascii_lowercase()),
            "asset names must differ by more than case ('{name}')"
        );
        let metadata = tokio::fs::metadata(path)
            .await
            .with_context(|| format!("asset '{name}': can't read {path}"))?;
        anyhow::ensure!(metadata.is_file(), "asset '{name}': {path} is not a file");
        anyhow::ensure!(
            metadata.len() <= MAX_ASSET_BYTES as u64,
            "asset '{name}' is over {} MB",
            MAX_ASSET_BYTES / (1024 * 1024)
        );
        // Bounded however the file changes (or lies about its size).
        let mut bytes = Vec::new();
        {
            use tokio::io::AsyncReadExt as _;
            tokio::fs::File::open(path)
                .await
                .with_context(|| format!("asset '{name}': can't read {path}"))?
                .take(MAX_ASSET_BYTES as u64 + 1)
                .read_to_end(&mut bytes)
                .await
                .with_context(|| format!("asset '{name}': can't read {path}"))?;
        }
        anyhow::ensure!(
            bytes.len() <= MAX_ASSET_BYTES,
            "asset '{name}' is over {} MB",
            MAX_ASSET_BYTES / (1024 * 1024)
        );
        total += bytes.len();
        anyhow::ensure!(
            total <= MAX_TOTAL_ASSET_BYTES,
            "a visual's assets total at most {} MB",
            MAX_TOTAL_ASSET_BYTES / (1024 * 1024)
        );
        out.insert(
            name.clone(),
            Value::String(base64::engine::general_purpose::STANDARD.encode(bytes)),
        );
    }
    Ok(out)
}

// ---- argument shapes ---------------------------------------------------------

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ShowVisualArgs {
    script: String,
    title: String,
    #[serde(default)]
    assets: std::collections::BTreeMap<String, String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BatchArgs {
    requests: Vec<Value>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ChatArgs {
    chat: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MergePreviewArgs {
    chat: String,
    base: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ListModelsArgs {
    harness: String,
    device: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct ListChatsArgs {
    project: Option<String>,
    device: Option<String>,
    #[serde(default)]
    include_archived: bool,
    parent: Option<String>,
    limit: Option<usize>,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum ChatKind {
    Chat,
    Side,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct DeviceArgs {
    device: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RegisterProjectArgs {
    project: String,
    device: Option<String>,
    name: Option<String>,
    #[serde(default)]
    dry_run: bool,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct GithubPreflightArgs {
    project: Option<String>,
    cwd: Option<String>,
    branch: Option<String>,
    #[serde(default)]
    force: bool,
}

/// Messages already in the transcript before a send. Replies are matched by
/// id, not by time: `createdAt` is stamped on the host's clock, which can run
/// behind the caller's when the chat lives on another device.
#[derive(Clone, Default)]
struct TurnStart {
    message_ids: Vec<String>,
}

#[derive(Clone)]
struct PendingTurn {
    baseline: Option<Session>,
    start: TurnStart,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct CreateChatArgs {
    kind: Option<ChatKind>,
    project: Option<String>,
    device: Option<String>,
    parent: Option<String>,
    harness: Option<String>,
    model: Option<String>,
    reasoning: Option<String>,
    write: Option<bool>,
    sandbox: Option<String>,
    title: Option<String>,
    branch: Option<String>,
    cwd: Option<String>,
    prompt: Option<String>,
    #[serde(default)]
    wait: bool,
    timeout_secs: Option<u64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadChatArgs {
    chat: String,
    limit: Option<usize>,
    #[serde(default)]
    offset: usize,
    #[serde(default)]
    include_reasoning: bool,
    include_tools: Option<bool>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SendArgs {
    chat: String,
    text: String,
    mode: Option<String>,
    #[serde(default)]
    wait: bool,
    timeout_secs: Option<u64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BoardPostArgs {
    kind: String,
    text: String,
    #[serde(default)]
    session: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BoardReadArgs {
    since: Option<String>,
    limit: Option<u64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WaitArgs {
    chat: String,
    timeout_secs: Option<u64>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct WaitAnyArgs {
    #[serde(default)]
    chats: Vec<String>,
    timeout_secs: Option<u64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RememberArgs {
    text: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ForgetArgs {
    id: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ProjectUpdateArgs {
    goal: Option<String>,
    instructions: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ArchiveArgs {
    chat: String,
    archived: Option<bool>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RespondArgs {
    chat: String,
    request_id: Option<String>,
    #[serde(default)]
    answers: Vec<AnswerArg>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AnswerArg {
    question_id: String,
    labels: Vec<String>,
}

/// Arguments of a tool that takes none: anything but an empty object (or
/// null) is an error naming the stray field.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NoArgs {}

fn parse<T: serde::de::DeserializeOwned>(args: Value) -> Result<T, String> {
    serde_json::from_value(args).map_err(|e| format!("invalid arguments: {e}"))
}

fn no_args(args: Value) -> Result<(), String> {
    let args = if args.is_null() { json!({}) } else { args };
    parse::<NoArgs>(args).map(|NoArgs {}| ())
}

fn wait_duration(secs: Option<u64>) -> Duration {
    secs.map(Duration::from_secs)
        .unwrap_or(DEFAULT_WAIT)
        .min(MAX_WAIT)
}

#[cfg(test)]
fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or_default()
}

/// The thinking levels a parent may give a session it starts (Pi's own).
const THINKING_LEVELS: [&str; 7] = ["off", "minimal", "low", "medium", "high", "xhigh", "max"];

fn parse_enum<T: serde::de::DeserializeOwned>(what: &str, raw: &str) -> Result<T, String> {
    serde_json::from_value(Value::String(raw.trim().to_owned()))
        .map_err(|_| format!("unknown {what}: {raw:?}"))
}

// ---- summaries ---------------------------------------------------------------

/// Live posture of one chat as the tools report it.
fn status_of(session: Option<&Session>) -> (String, Option<i64>) {
    let Some(session) = session else {
        return ("idle".into(), None);
    };
    let age = chrono::Utc::now() - session.updated_at;
    let stale = age > SESSION_STALE;
    let queued = format!("queued for capacity (#{})", session.queue_position);
    let label = match session.status {
        SessionStatus::Working if stale => "working (stale — host may be offline)",
        // Still `Working` for every wait; this only tells the parent why it
        // has not started.
        SessionStatus::Working if session.queue_position > 0 => queued.as_str(),
        SessionStatus::Working if session.retry.is_some() => {
            return (retry_status(session), Some(age.num_seconds().max(0)));
        }
        SessionStatus::Working => "working",
        SessionStatus::AwaitingInput => "awaitingInput",
        SessionStatus::Errored => "errored",
        SessionStatus::Idle => "idle",
    };
    (label.into(), Some(age.num_seconds().max(0)))
}

/// `retrying (attempt 2/10, next try in 8s)`: the provider failed and the
/// harness is retrying on its own. Still working, not stalled.
fn retry_status(session: &Session) -> String {
    let Some(retry) = &session.retry else {
        return "working".into();
    };
    let mut label = match retry.max_attempts {
        Some(max) => format!("retrying (attempt {}/{max}", retry.attempt),
        None => format!("retrying (attempt {}", retry.attempt),
    };
    if let Some(at) = retry.next_attempt_at {
        let ms = (at - chrono::Utc::now()).num_milliseconds();
        if ms > 0 {
            label.push_str(&format!(", next try in {}s", (ms + 999) / 1000));
        }
    }
    label.push(')');
    label
}

/// The structured sibling of a `retrying` status; null when the session is
/// not retrying right now. `error` is provider text: data, not instructions.
fn retry_json(session: Option<&Session>) -> Value {
    session
        .filter(|s| {
            s.status == SessionStatus::Working && chrono::Utc::now() - s.updated_at <= SESSION_STALE
        })
        .and_then(|s| s.retry.as_ref())
        .map_or(Value::Null, |r| {
            json!({
                "attempt": r.attempt,
                "maxAttempts": r.max_attempts,
                "nextAttemptAt": r.next_attempt_at,
                "error": r.error,
            })
        })
}

/// Background jobs an idle chat is waiting on: their results start its next
/// turn. 0 for a working, stale or unknown session.
fn waiting_on_jobs(session: Option<&Session>) -> u32 {
    session
        .filter(|s| {
            s.status == SessionStatus::Idle && chrono::Utc::now() - s.updated_at <= SESSION_STALE
        })
        .map_or(0, |s| s.background_jobs)
}

fn summarize_chat(chat: &Chat, spaces: &[Space], sessions: &[Session]) -> Value {
    let space = chat
        .space_id
        .as_deref()
        .and_then(|id| spaces.iter().find(|s| s.id == id));
    let session = session_for(sessions, chat);
    let (status, status_age) = status_of(session.as_ref());
    json!({
        "id": chat.id,
        "title": chat.title,
        "project": space.map(|s| json!({
            "id": s.id,
            "name": s.display_name(),
            "path": s.path,
        })),
        "deviceId": chat.device_id,
        "cwd": chat.cwd.clone().or_else(|| space.map(|s| s.path.clone())),
        "branch": chat.branch,
        "harness": chat.config.as_ref().map(|c| c.harness),
        "model": chat.config.as_ref().and_then(|c| c.model.clone()),
        "reasoning": chat.config.as_ref().and_then(|c| c.reasoning),
        "archived": chat.archived,
        "parentChatId": chat.parent_chat_id,
        "status": status,
        "statusAgeSecs": status_age,
        "retry": retry_json(session.as_ref()),
        "backgroundJobs": session.as_ref().map_or(0, |s| s.background_jobs),
        "runningSubagents": session.as_ref().map_or(0, |s| s.running_subagents),
        "lastMessageAt": chat.last_message_at,
        "lastMessagePreview": chat.last_message_preview,
        "createdAt": chat.created_at,
    })
}

fn last_pending_input(messages: &[RenderedMessage]) -> Option<Value> {
    messages.iter().rev().find_map(|m| m.pending_input.clone())
}

/// The classified failure of the newest reply that has one.
fn failure_of(replies: &[RenderedMessage]) -> Option<Value> {
    replies.iter().rev().find_map(|m| m.failure.clone())
}

// ---- dispatch ----------------------------------------------------------------

impl Tools {
    pub fn new(zeron: Arc<Zeron>) -> Self {
        Self {
            zeron,
            pending_turns: Default::default(),
            reserved: Default::default(),
        }
    }

    fn reserved(&self) -> std::sync::MutexGuard<'_, HashMap<String, Slots>> {
        self.reserved
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Take a session slot below `root_id`, or say why the tree is full.
    /// Counts and takes the slot under one lock, so concurrent batch items
    /// cannot all pass the check.
    fn reserve_slot(&self, chats: &[Chat], root_id: &str) -> anyhow::Result<Reservation<'_>> {
        use zeron_proto::orchestrator::{MAX_LIVE_SESSIONS, live_members};
        let mut reserved = self.reserved();
        let slots = reserved.entry(root_id.to_owned()).or_default();
        slots
            .unfolded
            .retain(|id| !chats.iter().any(|chat| chat.id == *id));
        let live = live_members(chats, root_id).len() + slots.held + slots.unfolded.len();
        anyhow::ensure!(
            live < MAX_LIVE_SESSIONS,
            "This orchestrator already has {live} live sessions, the most one tree may hold ({MAX_LIVE_SESSIONS}). Wait for a session to finish, or archive one you are done with (archive_chat), then try again."
        );
        slots.held += 1;
        Ok(Reservation {
            tools: self,
            root: root_id.to_owned(),
            created: None,
        })
    }

    pub fn list(&self) -> Vec<ToolDef> {
        let in_chat = self.zeron.origin().chat_id.is_some();
        catalog()
            .into_iter()
            .filter(|tool| in_chat || !IN_CHAT_ONLY.contains(&tool.name))
            .collect()
    }

    pub fn has(&self, name: &str) -> bool {
        self.list().iter().any(|t| t.name == name)
    }

    /// `Ok` is the tool's structured result; `Err` is a message the model
    /// should read (surfaced as `isError`, never as a protocol error).
    pub async fn call(&self, name: &str, args: Value) -> Result<Value, String> {
        // Each tool's future is boxed so `call` stays small: inline, the
        // largest arm sized every caller's future and overflowed the stack
        // of a debug build.
        use futures::FutureExt;
        let result = match name {
            "whoami" => {
                no_args(args)?;
                self.whoami().boxed().await
            }
            "list_devices" => {
                no_args(args)?;
                self.list_devices().boxed().await
            }
            "list_projects" => self.list_projects(parse(args)?).boxed().await,
            "register_project" => self.register_project(parse(args)?).boxed().await,
            "github_preflight" => self.github_preflight(parse(args)?).boxed().await,
            "list_harnesses" => self.list_harnesses(parse(args)?).boxed().await,
            "list_models" => self.list_models(parse(args)?).boxed().await,
            "list_chats" => self.list_chats(parse(args)?).boxed().await,
            "get_chat" => self.get_chat(parse(args)?).boxed().await,
            "create_chat" => self.create_chat(parse(args)?).boxed().await,
            "create_chats" => self.batch(parse(args)?, true).boxed().await,
            "send_messages" => self.batch(parse(args)?, false).boxed().await,
            "read_chat" => self.read_chat(parse(args)?).boxed().await,
            "send_message" => self.send_message(parse(args)?).boxed().await,
            "wait_for_turn" => self.wait_for_turn(parse(args)?).boxed().await,
            "wait_for_turns" => self.wait_for_turns(parse(args)?).boxed().await,
            "interrupt_chat" => self.interrupt_chat(parse(args)?).boxed().await,
            "respond_to_input" => self.respond_to_input(parse(args)?).boxed().await,
            "archive_chat" => self.archive_chat(parse(args)?).boxed().await,
            SHOW_VISUAL => self.show_visual(parse(args)?).boxed().await,
            BOARD_POST => self.board_post(parse(args)?).boxed().await,
            BOARD_READ => self.board_read(parse(args)?).boxed().await,
            PROJECT_READ => {
                no_args(args)?;
                self.project_read().boxed().await
            }
            PROJECT_REMEMBER => {
                let args: RememberArgs = parse(args)?;
                self.project_memory(json!({ "text": args.text })).boxed().await
            }
            PROJECT_FORGET => {
                let args: ForgetArgs = parse(args)?;
                self.project_memory(json!({ "forget": args.id })).boxed().await
            }
            PROJECT_UPDATE => self.project_update(parse(args)?).boxed().await,
            MERGE_PREVIEW => self.merge_preview(parse(args)?).boxed().await,
            other => return Err(format!("unknown tool: {other}")),
        };
        result.map_err(|e| format!("{e:#}"))
    }

    /// Poll every request together, including its optional wait. A waiting
    /// first chat must not prevent subsequent chats from receiving their work.
    async fn batch(&self, args: BatchArgs, create: bool) -> anyhow::Result<Value> {
        anyhow::ensure!(
            (1..=MAX_BATCH).contains(&args.requests.len()),
            "requests must contain between 1 and {MAX_BATCH} items"
        );
        let results = futures::future::join_all(args.requests.into_iter().enumerate().map(
            |(index, args)| async move {
                let result = if create {
                    match serde_json::from_value(args) {
                        Ok(args) => self.create_chat(args).await,
                        Err(error) => Err(anyhow::anyhow!(
                            "requests[{index}]: invalid arguments: {error}"
                        )),
                    }
                } else {
                    match serde_json::from_value(args) {
                        Ok(args) => self.send_message(args).await,
                        Err(error) => Err(anyhow::anyhow!(
                            "requests[{index}]: invalid arguments: {error}"
                        )),
                    }
                };
                match result {
                    Ok(result) => json!({"index": index, "isError": false, "result": result}),
                    Err(error) => {
                        json!({"index": index, "isError": true, "error": error.to_string()})
                    }
                }
            },
        ))
        .await;
        Ok(json!({"results": results}))
    }

    async fn whoami(&self) -> anyhow::Result<Value> {
        let origin = self.zeron.origin().clone();
        let local_device = self.zeron.local_device_id().await?;
        let engine = self.zeron.engine_info().await.unwrap_or(Value::Null);
        let mut place = Value::Null;
        let chat = match origin.chat_id.as_deref() {
            Some(id) => match self.zeron.resolve_chat(id).await {
                Ok(chat) => {
                    let (chats, spaces, sessions) = tokio::try_join!(
                        self.zeron.chats(),
                        self.zeron.spaces(),
                        self.zeron.sessions()
                    )?;
                    place = describe_place(&chats, &chat.id);
                    summarize_chat(&chat, &spaces, &sessions)
                }
                Err(_) => json!({ "id": id }),
            },
            None => Value::Null,
        };
        Ok(json!({
            "chat": chat,
            "place": place,
            "originDeviceId": origin.device_id,
            "localDeviceId": local_device,
            "workspaceScope": engine.get("workspaceScope").cloned().unwrap_or(Value::Null),
            "note": if origin.chat_id.is_some() {
                "Messages you send are attributed to this chat; it cannot message itself."
            } else {
                "Not running inside a chat: messages are sent without attribution."
            },
        }))
    }

    async fn list_devices(&self) -> anyhow::Result<Value> {
        let (devices, local) =
            tokio::try_join!(self.zeron.devices(), self.zeron.local_device_id())?;
        Ok(json!({
            "devices": devices.iter().map(|d| json!({
                "id": d.id,
                "name": d.name,
                "platform": d.platform,
                "local": d.id == local,
                "lastSeenAt": d.last_seen_at,
                "version": d.version,
            })).collect::<Vec<_>>()
        }))
    }

    async fn list_projects(&self, args: DeviceArgs) -> anyhow::Result<Value> {
        let (mut spaces, devices) = tokio::try_join!(self.zeron.spaces(), self.zeron.devices())?;
        if let Some(device) = args.device.as_deref() {
            let device = self.zeron.resolve_device_id(Some(device)).await?;
            spaces.retain(|s| s.device_id == device);
        }
        let device_name = |id: &str| devices.iter().find(|d| d.id == id).map(|d| d.name.clone());
        Ok(json!({
            "projects": spaces.iter().map(|s| json!({
                "id": s.id,
                "name": s.display_name(),
                "path": s.path,
                "deviceId": s.device_id,
                "deviceName": device_name(&s.device_id),
                "git": s.git_detected,
            })).collect::<Vec<_>>()
        }))
    }

    /// Register an existing folder as a project on its device. The engine
    /// resolves a path or a short name there and registers one unambiguous
    /// folder; this waits for the new row so a `create_chat` right after finds
    /// the project.
    async fn register_project(&self, args: RegisterProjectArgs) -> anyhow::Result<Value> {
        let device = self.zeron.resolve_device_id(args.device.as_deref()).await?;
        let reply = self
            .zeron
            .call(
                zeron_rpc::methods::REGISTER_PROJECT,
                json!({
                    "query": args.project,
                    "name": args.name,
                    "dryRun": args.dry_run,
                    "targetDeviceId": device,
                }),
            )
            .await?;
        if args.dry_run {
            return Ok(reply);
        }
        let space: Space = serde_json::from_value(reply["space"].clone())
            .context("RegisterProject: unexpected reply")?;
        self.wait_for_space(&space.id).await;
        Ok(json!({
            "project": {
                "id": space.id,
                "name": space.display_name(),
                "path": space.path,
                "deviceId": space.device_id,
                "git": space.git_detected,
                "created": reply["created"],
            },
            "workspace": if space.git_detected {
                "each writing session gets its own worktree on a zeron/<name> branch"
            } else {
                "not a git repository: writing sessions would share this folder, so run one writer at a time"
            },
        }))
    }

    /// The registry row of a project just registered reaches the watch a
    /// moment after the engine saved it.
    async fn wait_for_space(&self, id: &str) {
        for _ in 0..50 {
            if self
                .zeron
                .spaces()
                .await
                .is_ok_and(|spaces| spaces.iter().any(|space| space.id == id))
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    /// The project (and so the checkout and device) a preflight is about:
    /// the named one, else the calling chat's own.
    async fn preflight_target(&self, project: Option<&str>) -> anyhow::Result<(Space, String)> {
        if project.is_some() {
            let (space, device) = self.zeron.resolve_target(project, None).await?;
            let space = space.context("project is required")?;
            return Ok((space, device));
        }
        let origin = self
            .zeron
            .origin()
            .chat_id
            .clone()
            .context("name a project: this tool was not started from a chat with one")?;
        let chat = self.zeron.resolve_chat(&origin).await?;
        let space = self
            .zeron
            .spaces()
            .await?
            .into_iter()
            .find(|space| chat.space_id.as_deref() == Some(space.id.as_str()))
            .context("your chat has no project; name one")?;
        let device = space.device_id.clone();
        Ok((space, device))
    }

    async fn github_preflight(&self, args: GithubPreflightArgs) -> anyhow::Result<Value> {
        let (space, device) = self.preflight_target(args.project.as_deref()).await?;
        let cwd = args
            .cwd
            .as_deref()
            .map(str::trim)
            .filter(|cwd| !cwd.is_empty())
            .map_or_else(|| space.path.clone(), str::to_owned);
        let mut report = self
            .zeron
            .call(
                zeron_rpc::methods::GITHUB_PREFLIGHT,
                json!({
                    "cwd": cwd,
                    "branch": args.branch,
                    "force": args.force,
                    "targetDeviceId": device,
                }),
            )
            .await?;
        report["project"] =
            json!({ "id": space.id, "name": space.display_name(), "path": space.path });
        Ok(report)
    }

    async fn list_harnesses(&self, args: DeviceArgs) -> anyhow::Result<Value> {
        let device = match args.device.as_deref() {
            Some(key) => Some(self.zeron.resolve_device_id(Some(key)).await?),
            None => None,
        };
        let harnesses = self.zeron.harnesses_on(device.as_deref()).await?;
        Ok(json!({
            "harnesses": harnesses.iter().map(|h| json!({
                "id": h.id,
                "name": h.name,
                "available": h.available(),
                "installed": h.installed,
                "steersMidTurn": h.steers_mid_turn(),
                "reasoningLevels": h.reasoning_levels,
            })).collect::<Vec<_>>()
        }))
    }

    async fn list_models(&self, args: ListModelsArgs) -> anyhow::Result<Value> {
        let harness: HarnessId =
            parse_enum("harness", &args.harness).map_err(anyhow::Error::msg)?;
        let device = match args.device.as_deref() {
            Some(key) => Some(self.zeron.resolve_device_id(Some(key)).await?),
            None => None,
        };
        let models = self.zeron.models_on(harness, device.as_deref()).await?;
        Ok(json!({
            "harness": harness,
            "models": models.iter().map(|m| json!({
                "id": m.id,
                "label": m.label,
                "description": m.description,
                "reasoningLevels": m.reasoning_levels,
            })).collect::<Vec<_>>()
        }))
    }

    async fn list_chats(&self, args: ListChatsArgs) -> anyhow::Result<Value> {
        let (mut chats, spaces, sessions) = tokio::try_join!(
            self.zeron.chats(),
            self.zeron.spaces(),
            self.zeron.sessions()
        )?;
        if let Some(project) = args.project.as_deref() {
            let (space, _) = self
                .zeron
                .resolve_target(Some(project), args.device.as_deref())
                .await?;
            let space = space.expect("project provided");
            chats.retain(|c| c.space_id.as_deref() == Some(space.id.as_str()));
        }
        if args.device.is_some() {
            let device = self.zeron.resolve_device_id(args.device.as_deref()).await?;
            chats.retain(|c| c.device_id == device);
        }
        if !args.include_archived {
            chats.retain(|c| !c.archived);
        }
        // A btw aside is the user's question beside a chat: no session to
        // follow, message or count.
        chats.retain(|c| !c.is_btw());
        if let Some(parent) = args.parent.as_deref() {
            let parent = self.zeron.resolve_chat(parent).await?;
            chats.retain(|c| c.parent_chat_id.as_deref() == Some(parent.id.as_str()));
        }
        chats.sort_by(|a, b| {
            let a_at = a.last_message_at.unwrap_or(a.created_at);
            let b_at = b.last_message_at.unwrap_or(b.created_at);
            b_at.cmp(&a_at)
        });
        let total = chats.len();
        let limit = args.limit.unwrap_or(50).clamp(1, 500);
        Ok(json!({
            "total": total,
            "chats": chats.iter().take(limit)
                .map(|c| summarize_chat(c, &spaces, &sessions))
                .collect::<Vec<_>>()
        }))
    }

    async fn get_chat(&self, args: ChatArgs) -> anyhow::Result<Value> {
        let chat = self.zeron.resolve_chat(&args.chat).await?;
        let (spaces, sessions, entries) = tokio::try_join!(
            self.zeron.spaces(),
            self.zeron.sessions(),
            self.zeron.transcript_on(&chat.id, Some(&chat.device_id))
        )?;
        let rendered = render_entries(&entries, RenderOptions::default());
        let mut summary = summarize_chat(&chat, &spaces, &sessions);
        summary["messageCount"] = json!(rendered.len());
        summary["pendingInput"] = last_pending_input(&rendered).unwrap_or(Value::Null);
        summary["lastMessage"] = rendered.last().map(|m| json!(m)).unwrap_or(Value::Null);
        Ok(summary)
    }

    async fn create_chat(&self, args: CreateChatArgs) -> anyhow::Result<Value> {
        ensure_prompt_fits("prompt", args.prompt.as_deref().unwrap_or_default())?;
        // An orchestrator's tree may grow sessions below sessions; elsewhere
        // only a top-level chat starts side chats.
        let chats = self.zeron.chats().await?;
        let tree = self.tree_gate(&chats).await?;
        if let Some(tree) = &tree {
            self.ensure_within_cap(tree).await?;
        }
        // The models of the chats launching this one, for the reserved check.
        let mut launcher_models: Vec<String> = Vec::new();
        if let Some(origin) = self.zeron.origin().chat_id.as_deref() {
            let chat = self.zeron.resolve_chat(origin).await?;
            anyhow::ensure!(
                chat.parent_chat_id.is_none()
                    || zeron_proto::orchestrator::depth(&chats, &chat.id).is_some(),
                "Side chats cannot create chats. Ask your parent chat to create another side chat."
            );
            ensure_room_below(&chats, &chat)?;
            launcher_models.extend(chat_model(&chat));
        }
        // Parent: the explicit `parent` argument, else the chat this server
        // speaks for. Resolved so a prefix/title works and a typo fails loud.
        let explicit_parent = args
            .parent
            .as_deref()
            .map(str::trim)
            .filter(|p| !p.is_empty());
        anyhow::ensure!(
            args.kind != Some(ChatKind::Chat) || explicit_parent.is_none(),
            "kind chat cannot have a parent"
        );
        let parent_chat_id = if args.kind == Some(ChatKind::Chat) {
            None
        } else {
            match explicit_parent {
                Some(key) => Some(self.zeron.resolve_chat(key).await?.id),
                None => self.zeron.origin().chat_id.clone(),
            }
            // A hidden voice orchestrator creates top-level sessions.
            .filter(|parent| !zeron_proto::voice::is_orchestrator_chat(parent))
        };
        anyhow::ensure!(
            args.kind != Some(ChatKind::Side) || parent_chat_id.is_some(),
            "kind side requires a parent or origin chat"
        );
        let kind = if parent_chat_id.is_some() {
            ChatKind::Side
        } else {
            ChatKind::Chat
        };

        // Sessions of an orchestrator's tree default to their parent's harness.
        let mut inherited_harness = None;
        // Held until this call returns, so a batch counts its own sessions.
        let mut slot = None;
        if let Some(parent) = parent_chat_id.as_deref() {
            let chat = self.zeron.resolve_chat(parent).await?;
            let orchestrated = zeron_proto::orchestrator::depth(&chats, &chat.id).is_some();
            anyhow::ensure!(
                chat.parent_chat_id.is_none() || orchestrated,
                "Cannot create a child of a side chat. Choose a top-level parent chat."
            );
            ensure_room_below(&chats, &chat)?;
            if let Some(root) = zeron_proto::orchestrator::root_of(&chats, &chat.id) {
                slot = Some(self.reserve_slot(&chats, &root.id)?);
            }
            if orchestrated {
                inherited_harness = chat.config.as_ref().map(|c| c.harness);
            }
            launcher_models.extend(chat_model(&chat));
        }
        ensure_model_not_reserved(args.model.as_deref(), &launcher_models)?;
        if let Some(tree) = &tree {
            // The new session inherits the restrictions of the chat above it.
            let holder = parent_chat_id
                .as_deref()
                .and_then(|id| chats.iter().find(|chat| chat.id == id))
                .unwrap_or(&tree.root);
            self.ensure_model_allowed(tree, holder, args.model.as_deref())
                .await?;
        }
        let (mut space, device_id) = match self
            .zeron
            .resolve_target(args.project.as_deref(), args.device.as_deref())
            .await
        {
            Ok(found) => found,
            // A folder that is not a project yet: a path or a short name
            // registers it, when exactly one folder matches.
            Err(error)
                if error.to_string().starts_with(crate::zeron::NO_PROJECT)
                    && args.project.is_some() =>
            {
                self.register_for_create(&args)
                    .await
                    .map_err(|register| register.context(format!("{error}")))?
            }
            Err(error) => return Err(error),
        };
        // A cwd that is a project's folder joins that project.
        if space.is_none()
            && let Some(cwd) = args.cwd.as_deref().map(str::trim).filter(|c| !c.is_empty())
        {
            space = self
                .zeron
                .spaces()
                .await?
                .into_iter()
                .find(|s| s.device_id == device_id && same_folder(&s.path, cwd));
        }
        let harnesses = self.zeron.harnesses_on(Some(&device_id)).await?;
        let inherited_harness =
            inherited_harness.filter(|id| harnesses.iter().any(|h| h.id == *id && h.available()));
        let harness = match args.harness.as_deref() {
            Some(raw) => {
                let id: HarnessId = parse_enum("harness", raw).map_err(anyhow::Error::msg)?;
                anyhow::ensure!(
                    harnesses.iter().any(|h| h.id == id && h.available()),
                    "harness {raw} is not available on device {device_id} (see list_harnesses with device)"
                );
                id
            }
            None => match inherited_harness {
                Some(id) => id,
                None => {
                    default_harness(&harnesses).with_context(|| format!("device {device_id}"))?
                }
            },
        };
        if let Some(model) = args.model.as_deref() {
            let models = self.zeron.models_on(harness, Some(&device_id)).await?;
            anyhow::ensure!(
                models.iter().any(|m| m.id == model),
                "model {model:?} is not offered by {harness:?} on device {device_id}; available: {}",
                models
                    .iter()
                    .map(|m| m.id.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
        // `off` is no effort level: Pi turns thinking off through its
        // `pi_thinking` option, as the composer's picker does.
        let thinking = args
            .reasoning
            .as_deref()
            .map(str::trim)
            .filter(|raw| !raw.is_empty());
        let thinking_off = thinking.is_some_and(|raw| raw.eq_ignore_ascii_case("off"));
        let reasoning: Option<ReasoningLevel> = match thinking.filter(|_| !thinking_off) {
            Some(raw) => Some(parse_enum("reasoning level", raw).map_err(|_| {
                anyhow::anyhow!(
                    "unknown reasoning level {raw:?}: use one of {}",
                    THINKING_LEVELS.join(", ")
                )
            })?),
            None => None,
        };
        let mut model_options = serde_json::Map::new();
        if thinking_off {
            model_options.insert("pi_thinking".into(), json!("off"));
        }
        let write = args.write.unwrap_or(true);
        let sandbox: SandboxLevel = match args.sandbox.as_deref() {
            Some(raw) => parse_enum("sandbox", raw).map_err(anyhow::Error::msg)?,
            None if write => SandboxLevel::WorkspaceWrite,
            None => SandboxLevel::ReadOnly,
        };
        anyhow::ensure!(
            write == (sandbox != SandboxLevel::ReadOnly),
            "write {write} conflicts with sandbox {}; leave sandbox unset",
            args.sandbox.as_deref().unwrap_or_default()
        );
        let mut preflight_note = None;
        if write && let (Some(tree), Some(space)) = (&tree, &space) {
            if space.git_detected {
                if tree.policy.require_preflight {
                    preflight_note = self.ensure_preflight(space, &device_id).await?;
                }
            } else {
                self.ensure_folder_free(tree, &chats, space).await?;
            }
        }
        let config = ChatConfig {
            harness,
            model: args.model.clone(),
            reasoning,
            model_options,
            sandbox,
        };

        let chat_id = uuid::Uuid::new_v4().to_string();
        let mut mutate = json!({
            "op": "createChat",
            "chatId": chat_id,
            "deviceId": device_id,
            "config": config,
        });
        if let Some(space) = &space {
            mutate["spaceId"] = json!(space.id);
        }
        if let Some(parent) = &parent_chat_id {
            mutate["parentChatId"] = json!(parent);
        }
        let mut cwd = args
            .cwd
            .as_deref()
            .map(str::trim)
            .filter(|c| !c.is_empty())
            .map(str::to_owned);
        let branch = args
            .branch
            .as_deref()
            .map(str::trim)
            .filter(|b| !b.is_empty());
        // A writing session in a git project gets its own worktree, made by
        // the host on its first run; the branch names where it starts.
        let worktree = isolation(sandbox, space.as_ref(), cwd.as_deref(), branch);
        if let Some(branch) = branch.filter(|_| worktree.is_none()) {
            mutate["branch"] = json!(branch);
        }
        // A projectless chat runs in its own scratch folder on a host that
        // creates them; naming it here keeps createChat and the first run
        // in agreement.
        if space.is_none()
            && cwd.is_none()
            && self.zeron.devices().await?.iter().any(|d| {
                d.id == device_id
                    && d.capabilities
                        .iter()
                        .any(|c| c == zeron_proto::capabilities::PROJECTLESS_SCRATCH_V1)
            })
        {
            cwd = Some(zeron_proto::projectless::scratch_cwd(
                &chat_id,
                chrono::Utc::now(),
            ));
        }
        if let Some(cwd) = &cwd {
            mutate["cwd"] = json!(cwd);
        }
        self.zeron.mutate(mutate).await?;
        if let Some(slot) = &mut slot {
            slot.created = Some(chat_id.clone());
        }
        if let Some(title) = args
            .title
            .as_deref()
            .map(str::trim)
            .filter(|t| !t.is_empty())
        {
            self.zeron
                .mutate(json!({ "op": "renameChat", "chatId": chat_id, "title": title }))
                .await?;
        }

        let workspace = if worktree.is_some() {
            "a new worktree, created on the first run"
        } else {
            "the project folder"
        };
        let mut result = json!({
            "chatId": chat_id,
            "kind": kind,
            "deviceId": device_id,
            "project": space.as_ref().map(|s| json!({ "id": s.id, "name": s.display_name(), "path": s.path })),
            "harness": harness,
            "model": args.model,
            "reasoning": if thinking_off { json!("off") } else { json!(reasoning) },
            "title": args.title,
            "parentChatId": parent_chat_id,
            "write": write,
            "workspace": workspace,
        });
        if let Some(note) = preflight_note {
            result["preflight"] = note;
        }
        if let Some(prompt) = args.prompt.filter(|p| !p.trim().is_empty()) {
            // The row may not have folded into WatchChats yet; build the
            // chat locally from what we just wrote rather than re-reading.
            let chat = Chat {
                id: chat_id.clone(),
                device_id: device_id.clone(),
                title: args.title.clone(),
                archived: false,
                cwd: cwd.clone(),
                branch: branch.filter(|_| worktree.is_none()).map(str::to_owned),
                checkout_id: None,
                source_context: None,
                config: Some(config),
                last_message_preview: None,
                last_message_at: None,
                created_at: chrono::Utc::now(),
                harness_session_id: None,
                harness_session_cwd: None,
                parent_chat_id: parent_chat_id.clone(),
                space_id: space.as_ref().map(|s| s.id.clone()),
                last_seen_at: None,
                room_gen: None,
            };
            let sent = self
                .deliver(
                    &chat,
                    space.as_ref(),
                    &harnesses,
                    None,
                    prompt,
                    "run",
                    worktree,
                )
                .await?;
            result["sent"] = sent;
            let pending = Arc::new(PendingTurn {
                baseline: None,
                // A new chat has no earlier messages.
                start: TurnStart::default(),
            });
            self.pending_turns
                .lock()
                .await
                .insert(chat.id.clone(), pending.clone());
            if args.wait {
                result["turn"] = self
                    .await_pending_turn(&chat, pending, wait_duration(args.timeout_secs))
                    .await?;
            }
        }
        Ok(result)
    }

    async fn read_chat(&self, args: ReadChatArgs) -> anyhow::Result<Value> {
        let chat = self.zeron.resolve_chat(&args.chat).await?;
        let (sessions, entries) = tokio::try_join!(
            self.zeron.sessions(),
            self.zeron.transcript_on(&chat.id, Some(&chat.device_id))
        )?;
        let session = session_for(&sessions, &chat);
        let (status, _) = status_of(session.as_ref());
        let rendered = render_entries(
            &entries,
            RenderOptions {
                include_reasoning: args.include_reasoning,
                include_tools: args.include_tools.unwrap_or(true),
            },
        );
        let total = rendered.len();
        let limit = args.limit.unwrap_or(40).clamp(1, 500);
        let end = total.saturating_sub(args.offset);
        let start = end.saturating_sub(limit);
        let window = &rendered[start..end];
        Ok(json!({
            "chatId": chat.id,
            "title": chat.title,
            "total": total,
            "returned": window.len(),
            "olderRemaining": start,
            "newerSkipped": total - end,
            "status": status,
            "retry": retry_json(session.as_ref()),
            "pendingInput": last_pending_input(&rendered),
            "messages": window,
        }))
    }

    async fn send_message(&self, args: SendArgs) -> anyhow::Result<Value> {
        ensure_prompt_fits("text", &args.text)?;
        let text = args.text.trim();
        if text.is_empty() {
            anyhow::bail!("text is empty");
        }
        let chat = self.zeron.resolve_chat(&args.chat).await?;
        if self.zeron.origin().chat_id.as_deref() == Some(chat.id.as_str()) {
            anyhow::bail!(
                "refusing to send a message to your own chat ({})",
                short(&chat.id)
            );
        }
        if self.zeron.origin().chat_id.is_some()
            && let Some(tree) = self.tree_gate(&self.zeron.chats().await?).await?
        {
            self.ensure_within_cap(&tree).await?;
        }
        let (spaces, sessions, harnesses) = tokio::try_join!(
            self.zeron.spaces(),
            self.zeron.sessions(),
            self.zeron.harnesses_on(Some(&chat.device_id))
        )?;
        let space = chat
            .space_id
            .as_deref()
            .and_then(|id| spaces.iter().find(|s| s.id == id));
        let baseline = session_for(&sessions, &chat);
        let mode = args.mode.as_deref().unwrap_or("auto");
        let message_ids = self
            .zeron
            .transcript_on(&chat.id, Some(&chat.device_id))
            .await?
            .into_iter()
            .map(|m| m.id)
            .collect();
        let body = self.attribute(&chat, text).await;
        let mut result = json!({
            "chatId": chat.id,
            "title": chat.title,
        });
        // A delegated writing session still in the project folder (created
        // without a prompt) gets its worktree on this first run.
        let worktree = chat
            .parent_chat_id
            .as_ref()
            .and_then(|_| chat.config.as_ref())
            .and_then(|config| {
                isolation(
                    config.sandbox,
                    space,
                    chat.cwd.as_deref(),
                    chat.branch.as_deref(),
                )
            });
        result["sent"] = self
            .deliver(
                &chat,
                space,
                &harnesses,
                baseline.as_ref(),
                body,
                mode,
                worktree,
            )
            .await?;
        let pending = Arc::new(PendingTurn {
            baseline,
            start: TurnStart { message_ids },
        });
        self.pending_turns
            .lock()
            .await
            .insert(chat.id.clone(), pending.clone());
        if args.wait {
            result["turn"] = self
                .await_pending_turn(&chat, pending, wait_duration(args.timeout_secs))
                .await?;
        }
        Ok(result)
    }

    async fn wait_for_turn(&self, args: WaitArgs) -> anyhow::Result<Value> {
        let chat = self.zeron.resolve_chat(&args.chat).await?;
        let pending = self.pending_turns.lock().await.get(&chat.id).cloned();
        let turn = match pending {
            Some(pending) => {
                self.await_pending_turn(&chat, pending, wait_duration(args.timeout_secs))
                    .await?
            }
            None => {
                self.await_turn(
                    &chat,
                    None,
                    false,
                    wait_duration(args.timeout_secs),
                    TurnStart::default(),
                )
                .await?
            }
        };
        Ok(json!({ "chatId": chat.id, "title": chat.title, "turn": turn }))
    }

    async fn archive_chat(&self, args: ArchiveArgs) -> anyhow::Result<Value> {
        let chat = self.zeron.resolve_chat(&args.chat).await?;
        let archived = args.archived.unwrap_or(true);
        self.zeron
            .mutate(json!({ "op": "setChatArchived", "chatId": chat.id, "archived": archived }))
            .await?;
        Ok(json!({ "chatId": chat.id, "title": chat.title, "archived": archived }))
    }

    /// Store the post, then hand it to every other session that sees it:
    /// a running one is steered at its next step, one waiting on a question
    /// gets it after that turn, and an idle one is not woken (it reads the
    /// board when it next starts), except an idle root for a blocker.
    async fn board_post(&self, args: BoardPostArgs) -> anyhow::Result<Value> {
        use zeron_proto::board::{self, BoardEntry, BoardKind};
        let chat_id = self.zeron.origin().chat_id.clone().context(
            "board_post only works from an agent Zeron launched in an orchestrator's tree",
        )?;
        let mut params = json!({ "chatId": chat_id, "kind": args.kind, "text": args.text });
        if let Some(session) = args.session.filter(|s| !s.trim().is_empty()) {
            params["boardChatId"] = json!(self.zeron.resolve_chat(&session).await?.id);
        }
        let posted = self
            .zeron
            .call(zeron_rpc::methods::POST_BOARD, params)
            .await?;
        let entry: BoardEntry = serde_json::from_value(posted["entry"].clone())?;
        let (chats, spaces, sessions) = tokio::try_join!(
            self.zeron.chats(),
            self.zeron.spaces(),
            self.zeron.sessions()
        )?;
        let (entry, notice) = (&entry, board::notice(&entry));
        let deliveries = chats
            .iter()
            .filter(|chat| {
                chat.id != entry.author_chat_id
                    && !chat.archived
                    && zeron_proto::orchestrator::root_of(&chats, &chat.id)
                        .is_some_and(|root| root.id == entry.root_id)
                    && board::visible_to(entry, chat)
            })
            .map(|chat| {
                let (notice, spaces, sessions) = (notice.clone(), &spaces, &sessions);
                async move {
                    let session = session_for(sessions, chat);
                    let mode = match session.as_ref().map(|s| s.status) {
                        Some(SessionStatus::Working) => "steer",
                        Some(SessionStatus::AwaitingInput) => "queue",
                        _ if entry.kind == BoardKind::Blocker
                            && zeron_proto::orchestrator::is_root_chat(&chat.id) =>
                        {
                            "run"
                        }
                        _ => {
                            return json!({
                                "chatId": chat.id, "title": chat.title,
                                "delivery": "none: not running; it reads the board when it next starts",
                            });
                        }
                    };
                    let space = chat
                        .space_id
                        .as_deref()
                        .and_then(|id| spaces.iter().find(|s| s.id == id));
                    match self
                        .deliver(chat, space, &[], session.as_ref(), notice, mode, None)
                        .await
                    {
                        Ok(sent) => json!({
                            "chatId": chat.id, "title": chat.title, "delivery": sent["delivery"],
                        }),
                        Err(error) => json!({
                            "chatId": chat.id, "title": chat.title, "error": format!("{error:#}"),
                        }),
                    }
                }
            });
        let recipients = futures::future::join_all(deliveries).await;
        Ok(json!({ "entry": entry, "recipients": recipients }))
    }

    async fn board_read(&self, args: BoardReadArgs) -> anyhow::Result<Value> {
        let chat_id = self.zeron.origin().chat_id.clone().context(
            "board_read only works from an agent Zeron launched in an orchestrator's tree",
        )?;
        let mut params = json!({ "chatId": chat_id });
        if let Some(since) = args.since.filter(|s| !s.trim().is_empty()) {
            params["since"] = json!(since.trim());
        }
        if let Some(limit) = args.limit {
            params["limit"] = json!(limit);
        }
        self.zeron
            .call(zeron_rpc::methods::LIST_BOARD, params)
            .await
    }

    /// The calling chat, which must be in an orchestrator's tree.
    fn tree_origin(&self, tool: &str) -> anyhow::Result<String> {
        self.zeron.origin().chat_id.clone().with_context(|| {
            format!("{tool} only works from an agent Zeron launched in an orchestrator's tree")
        })
    }

    /// The calling chat, which must be an orchestrator's root.
    fn root_origin(&self, tool: &str) -> anyhow::Result<String> {
        let chat_id = self.tree_origin(tool)?;
        anyhow::ensure!(
            zeron_proto::orchestrator::is_root_chat(&chat_id),
            "Only the orchestrator can use {tool}. Report what should change in your final reply."
        );
        Ok(chat_id)
    }

    /// Read-only: the engine runs git against the member's worktree and
    /// changes no tree or ref.
    async fn merge_preview(&self, args: MergePreviewArgs) -> anyhow::Result<Value> {
        let root = self.root_origin(MERGE_PREVIEW)?;
        let chat = self.zeron.resolve_chat(&args.chat).await?;
        self.zeron
            .call(
                zeron_rpc::methods::MERGE_PREVIEW,
                json!({
                    "chatId": root,
                    "member": chat.id,
                    "base": args.base,
                    "targetDeviceId": chat.device_id,
                }),
            )
            .await
    }

    async fn project_read(&self) -> anyhow::Result<Value> {
        let chat_id = self.tree_origin(PROJECT_READ)?;
        let mut reply = self
            .zeron
            .call(
                zeron_rpc::methods::GET_ORCHESTRATOR_PROJECT,
                json!({ "chatId": chat_id }),
            )
            .await?;
        // With a spending cap, how much of it is used.
        if reply["project"]["policy"]["spendCapCents"].is_u64()
            && let Ok(spend) = self
                .zeron
                .call(
                    zeron_rpc::methods::GET_TREE_SPEND,
                    json!({ "chatId": chat_id }),
                )
                .await
        {
            reply["spend"] = spend;
        }
        Ok(reply)
    }

    async fn project_memory(&self, mut params: Value) -> anyhow::Result<Value> {
        params["chatId"] = json!(self.root_origin(PROJECT_REMEMBER)?);
        self.zeron
            .call(zeron_rpc::methods::ORCHESTRATOR_MEMORY, params)
            .await
    }

    async fn project_update(&self, args: ProjectUpdateArgs) -> anyhow::Result<Value> {
        let chat_id = self.root_origin(PROJECT_UPDATE)?;
        anyhow::ensure!(
            args.goal.is_some() || args.instructions.is_some(),
            "pass goal, instructions or both"
        );
        self.zeron
            .call(
                zeron_rpc::methods::UPDATE_ORCHESTRATOR_PROJECT,
                json!({
                    "chatId": chat_id,
                    "update": { "goal": args.goal, "instructions": args.instructions },
                }),
            )
            .await
    }

    /// The gate every start of work passes. It refuses new work from an agent
    /// whose orchestrator is paused, and returns the tree's policy (`None`
    /// outside a tree).
    async fn tree_gate(&self, chats: &[Chat]) -> anyhow::Result<Option<Tree>> {
        let Some(origin) = self.zeron.origin().chat_id.as_deref() else {
            return Ok(None);
        };
        let Some(root) = zeron_proto::orchestrator::root_of(chats, origin) else {
            return Ok(None);
        };
        let root = root.clone();
        // An engine that cannot say leaves the gate open; pausing also
        // interrupts every running turn in the tree.
        let Ok(reply) = self
            .zeron
            .call(
                zeron_rpc::methods::GET_ORCHESTRATOR_PROJECT,
                json!({ "chatId": root.id }),
            )
            .await
        else {
            return Ok(Some(Tree {
                root,
                policy: Default::default(),
            }));
        };
        if reply["project"]["paused"] == true {
            match reply["project"]["pauseReason"]
                .as_str()
                .map(str::trim)
                .filter(|reason| !reason.is_empty())
            {
                Some(reason) => anyhow::bail!(
                    "This orchestrator is paused: {reason}. Start and send nothing; end your turn and say so. The user resumes it from Zeron."
                ),
                None => anyhow::bail!(
                    "The user paused this orchestrator. Start and send nothing; end your turn and say so. The user resumes it from Zeron."
                ),
            }
        }
        let policy = serde_json::from_value(reply["project"]["policy"].clone()).unwrap_or_default();
        Ok(Some(Tree { root, policy }))
    }

    /// Refuse new work once the tree has spent its cap. An engine that cannot
    /// say leaves the gate open.
    async fn ensure_within_cap(&self, tree: &Tree) -> anyhow::Result<()> {
        let Some(cap) = tree.policy.spend_cap_cents else {
            return Ok(());
        };
        let Ok(spend) = self
            .zeron
            .call(
                zeron_rpc::methods::GET_TREE_SPEND,
                json!({ "chatId": tree.root.id }),
            )
            .await
        else {
            return Ok(());
        };
        let spent = spend["spentUsd"].as_f64().unwrap_or_default();
        anyhow::ensure!(
            spent * 100.0 < cap as f64,
            "This orchestrator's spending cap of {} is reached ({:.2} spent). Start and send nothing; end your turn and say so. The user can raise the cap in the orchestrator's settings.",
            zeron_proto::orchestrator_project::dollars(cap),
            spent
        );
        Ok(())
    }

    /// Refuse a model the tree's policy does not allow, or that the chat the
    /// new session is below (or the orchestrator above it) disabled. The
    /// engine that cannot say leaves the disabled list open; Pi's guard
    /// holds it inside the session either way.
    async fn ensure_model_allowed(
        &self,
        tree: &Tree,
        holder: &Chat,
        model: Option<&str>,
    ) -> anyhow::Result<()> {
        let allowed = &tree.policy.allowed_models;
        let Some(model) = model.map(str::trim).filter(|model| !model.is_empty()) else {
            // Without a model the session runs on Pi's default, which may be
            // outside the allow-list; with only a deny-list that is Pi's
            // guard's to steer.
            anyhow::ensure!(
                allowed.is_empty(),
                "This orchestrator allows only these models: {}. Pass `model` with one of them (list_models shows what is offered).",
                allowed.join(", ")
            );
            return Ok(());
        };
        anyhow::ensure!(
            tree.policy.allows_model(model),
            "Model {model:?} is not allowed in this orchestrator; only these are: {}. Pass one of them as `model`.",
            allowed.join(", ")
        );
        let Ok(reply) = self
            .zeron
            .call(
                zeron_rpc::methods::GET_SESSION_MODEL_POLICY,
                json!({ "chatId": holder.id, "targetDeviceId": holder.device_id }),
            )
            .await
        else {
            return Ok(());
        };
        for list in ["disabled", "inherited"] {
            let hit = reply[list]
                .as_array()
                .into_iter()
                .flatten()
                .find_map(|entry| {
                    entry.as_str().filter(|entry| {
                        zeron_proto::orchestrator_project::model_matches(entry, model)
                    })
                });
            if let Some(entry) = hit {
                anyhow::bail!(
                    "Model {model:?} is disabled for this session tree ({entry}). Pass a different model (list_models shows what is offered)."
                );
            }
        }
        Ok(())
    }

    /// Before a writer starts in a GitHub project, the preflight must find no
    /// blockers. The engine's report is reused for a few minutes, and a check
    /// that cannot be made leaves the gate open. Warnings go back to the
    /// caller with the new session.
    async fn ensure_preflight(
        &self,
        space: &Space,
        device_id: &str,
    ) -> anyhow::Result<Option<Value>> {
        let report = tokio::time::timeout(
            PREFLIGHT_WAIT,
            self.zeron.call(
                zeron_rpc::methods::GITHUB_PREFLIGHT,
                json!({ "cwd": space.path, "targetDeviceId": device_id }),
            ),
        )
        .await;
        let Ok(Ok(report)) = report else {
            return Ok(Some(
                json!({ "verdict": "not checked: the GitHub preflight did not answer" }),
            ));
        };
        if report["supported"] == false {
            return Ok(None);
        }
        let verdict = report["verdict"].as_str().unwrap_or_default().to_owned();
        anyhow::ensure!(
            report["ok"] != false,
            "The GitHub preflight for {} found blockers, so no writing session starts there: {verdict}. Tell the user what to fix (ask_user), then run github_preflight with force to check again.",
            space.display_name()
        );
        Ok(Some(
            json!({ "verdict": verdict, "warnings": report["warnings"] }),
        ))
    }

    /// A folder that is not a git repository is shared by its writers: a
    /// second one is refused while another writer of the tree is working in
    /// it. Writers that finished and are idle do not count.
    async fn ensure_folder_free(
        &self,
        tree: &Tree,
        chats: &[Chat],
        space: &Space,
    ) -> anyhow::Result<()> {
        let sessions = self.zeron.sessions().await?;
        for member in zeron_proto::orchestrator::tree_members(chats, &tree.root.id) {
            let writes = member
                .config
                .as_ref()
                .is_some_and(|config| config.sandbox != SandboxLevel::ReadOnly);
            let here = member.space_id.as_deref() == Some(space.id.as_str())
                && member
                    .cwd
                    .as_deref()
                    .is_none_or(|cwd| same_folder(cwd, &space.path));
            let busy = session_for(&sessions, member).is_some_and(|session| {
                matches!(
                    session.status,
                    SessionStatus::Working | SessionStatus::AwaitingInput
                ) && chrono::Utc::now() - session.updated_at <= SESSION_STALE
            });
            if writes && here && busy {
                anyhow::bail!(
                    "{} is not a git repository, so its writing sessions share one folder, and {} is writing in it now. Wait for it (wait_for_turn), or make the folder a git repository first.",
                    space.display_name(),
                    member.title.as_deref().unwrap_or(&member.id)
                );
            }
        }
        Ok(())
    }

    /// `create_chat` named a folder that is not a project yet: register it,
    /// when the engine finds exactly one folder for the path or short name.
    async fn register_for_create(
        &self,
        args: &CreateChatArgs,
    ) -> anyhow::Result<(Option<Space>, String)> {
        let project = args.project.clone().unwrap_or_default();
        let registered = self
            .register_project(RegisterProjectArgs {
                project,
                device: args.device.clone(),
                name: None,
                dry_run: false,
            })
            .await?;
        let id = registered["project"]["id"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        self.zeron
            .resolve_target(Some(&id), args.device.as_deref())
            .await
    }

    /// The first of `chats` to stop working, and where the rest stand.
    async fn wait_for_turns(&self, args: WaitAnyArgs) -> anyhow::Result<Value> {
        anyhow::ensure!(
            args.chats.len() <= MAX_BATCH,
            "chats takes at most {MAX_BATCH} ids"
        );
        let mut targets: Vec<Chat> = Vec::new();
        if args.chats.is_empty() {
            let origin = self.zeron.origin().chat_id.clone().context(
                "pass chats: wait_for_turns watches your own sessions only from an agent Zeron launched",
            )?;
            let (chats, sessions) = tokio::try_join!(self.zeron.chats(), self.zeron.sessions())?;
            let pending = self.pending_turns.lock().await;
            targets = chats
                .into_iter()
                .filter(|chat| {
                    !chat.archived
                        && chat.parent_chat_id.as_deref() == Some(origin.as_str())
                        && (pending.contains_key(&chat.id)
                            || session_for(&sessions, chat)
                                .is_some_and(|s| s.status == SessionStatus::Working))
                })
                .collect();
            if targets.is_empty() {
                return Ok(json!({
                    "outcome": "none",
                    "note": "None of your sessions is working. zeron_list_chats shows where they stand.",
                }));
            }
        } else {
            for key in &args.chats {
                let chat = self.zeron.resolve_chat(key).await?;
                if !targets.iter().any(|seen| seen.id == chat.id) {
                    targets.push(chat);
                }
            }
        }
        let timeout = wait_duration(args.timeout_secs);
        let waits = targets.iter().map(|chat| {
            Box::pin(async move {
                let wait = WaitArgs {
                    chat: chat.id.clone(),
                    timeout_secs: Some(timeout.as_secs().max(1)),
                };
                (chat.id.clone(), self.wait_for_turn(wait).await)
            })
        });
        let ((chat_id, first), _, _) = futures::future::select_all(waits).await;
        let first = first?;
        let sessions = self.zeron.sessions().await?;
        let others: Vec<Value> = targets
            .iter()
            .filter(|chat| chat.id != chat_id)
            .map(|chat| {
                let session = session_for(&sessions, chat);
                let (status, _) = status_of(session.as_ref());
                json!({
                    "chatId": chat.id,
                    "title": chat.title,
                    "status": status,
                    "backgroundJobs": waiting_on_jobs(session.as_ref()),
                })
            })
            .collect();
        Ok(json!({
            "chatId": first["chatId"],
            "title": first["title"],
            "turn": first["turn"],
            "others": others,
        }))
    }

    async fn show_visual(&self, args: ShowVisualArgs) -> anyhow::Result<Value> {
        let chat_id = self
            .zeron
            .origin()
            .chat_id
            .clone()
            .context("show_visual only works from an agent Zeron launched in a chat")?;
        let assets = read_visual_assets(&args.assets).await?;
        let shown = self
            .zeron
            .call(
                zeron_rpc::methods::PUBLISH_VISUAL,
                json!({
                    "chatId": chat_id,
                    "title": args.title,
                    "script": args.script,
                    "assets": assets,
                }),
            )
            .await?;
        Ok(json!({
            "visual": shown,
            "message": "Shown to the reader above your reply. Don't mention or describe the visual; reply with only what it doesn't already say.",
        }))
    }

    async fn interrupt_chat(&self, args: ChatArgs) -> anyhow::Result<Value> {
        let chat = self.zeron.resolve_chat(&args.chat).await?;
        let command_id = self
            .zeron
            .queue_command(&chat.id, &SessionCommandPayload::Interrupt {})
            .await?;
        Ok(json!({ "chatId": chat.id, "commandId": command_id }))
    }

    async fn respond_to_input(&self, args: RespondArgs) -> anyhow::Result<Value> {
        let chat = self.zeron.resolve_chat(&args.chat).await?;
        let request_id = match args.request_id {
            Some(id) => id,
            None => {
                let entries = self
                    .zeron
                    .transcript_on(&chat.id, Some(&chat.device_id))
                    .await?;
                let rendered = render_entries(&entries, RenderOptions::default());
                last_pending_input(&rendered)
                    .and_then(|p| {
                        p.get("requestId")
                            .and_then(Value::as_str)
                            .map(str::to_owned)
                    })
                    .ok_or_else(|| {
                        anyhow::anyhow!("chat {} has no pending question", short(&chat.id))
                    })?
            }
        };
        if args.answers.is_empty() {
            anyhow::bail!("answers is empty");
        }
        let answers = args
            .answers
            .into_iter()
            .map(|a| UserInputAnswer {
                question_id: a.question_id,
                labels: a.labels,
            })
            .collect();
        let command_id = self
            .zeron
            .queue_command(
                &chat.id,
                &SessionCommandPayload::RespondInput {
                    request_id: request_id.clone(),
                    answers,
                },
            )
            .await?;
        Ok(json!({ "chatId": chat.id, "requestId": request_id, "commandId": command_id }))
    }

    // ---- shared pieces -------------------------------------------------------

    /// Prefix the sender's identity when this server speaks for a chat, so
    /// the receiving agent (and the human reading that transcript) can tell
    /// an agent-to-agent message from a typed one. Reply instructions always
    /// use the full id: display prefixes need not uniquely identify a chat.
    async fn attribute(&self, target: &Chat, text: &str) -> String {
        let Some(origin_id) = self.zeron.origin().chat_id.as_deref() else {
            return text.to_owned();
        };
        if origin_id == target.id {
            return text.to_owned();
        }
        let title = match self.zeron.resolve_chat(origin_id).await {
            Ok(chat) => chat.title,
            Err(_) => None,
        };
        let label = match title {
            Some(title) if !title.trim().is_empty() => {
                format!("{} ({})", title.trim(), short(origin_id))
            }
            _ => short(origin_id).to_owned(),
        };
        format!(
            "[Message from Zeron chat {label}. Reply to it with the Zeron `send_message` tool, chat {}.]\n\n{text}",
            origin_id
        )
    }

    /// Pick and perform the delivery the composer would. A new run carries
    /// `worktree`; the host reuses the chat's worktree once it has one.
    #[allow(clippy::too_many_arguments)]
    async fn deliver(
        &self,
        chat: &Chat,
        space: Option<&Space>,
        harnesses: &[HarnessInfo],
        session: Option<&Session>,
        text: String,
        mode: &str,
        worktree: Option<WorktreeSpec>,
    ) -> anyhow::Result<Value> {
        let harness = chat
            .config
            .as_ref()
            .map(|c| c.harness)
            .map_or_else(|| default_harness(harnesses), Ok)?;
        let (status, _) = status_of(session);
        // A quiet tool call can outlive the UI's stale-status window. Route
        // through steering and let the host decide whether a live run exists.
        let live = session.map(|s| s.status).unwrap_or(SessionStatus::Idle);
        let waiting = waiting_on_jobs(session);
        let chosen = match mode {
            // Nudging a chat that only waits on its own tests or CI to
            // "continue" starts a turn that can do nothing but wait again.
            "auto" if waiting > 0 => anyhow::bail!(
                "chat {} is idle but waiting on {waiting} background job(s) (tests, builds or a CI watch); their results wake it by themselves, so nothing was sent. Use mode 'queue' to add this to its next turn, or 'run' to start a turn now",
                short(&chat.id)
            ),
            "auto" => match live {
                SessionStatus::Idle | SessionStatus::Errored => "run",
                SessionStatus::Working => "steer",
                SessionStatus::AwaitingInput => anyhow::bail!(
                    "chat {} is waiting for an answer; use respond_to_input (or mode 'queue' to hold this message for after the turn)",
                    short(&chat.id)
                ),
            },
            "run" | "steer" | "queue" => mode,
            other => anyhow::bail!("unknown mode {other:?}; expected auto, run, steer, or queue"),
        };
        let id = match chosen {
            "run" => {
                let config = chat.config.clone();
                let cwd = chat
                    .cwd
                    .clone()
                    .or_else(|| space.map(|s| s.path.clone()))
                    .unwrap_or_else(|| "~".into());
                let request = RunRequest {
                    mcp: None,
                    prompt: text,
                    harness: Some(harness),
                    model: config.as_ref().and_then(|c| c.model.clone()),
                    reasoning: config.as_ref().and_then(|c| c.reasoning),
                    model_options: config
                        .as_ref()
                        .map(|c| c.model_options.clone())
                        .unwrap_or_default(),
                    cwd,
                    sandbox: config
                        .as_ref()
                        .map(|c| c.sandbox)
                        .unwrap_or(SandboxLevel::WorkspaceWrite),
                    auto_approve: false,
                    resume: None,
                    attachments: Vec::new(),
                    worktree,
                };
                self.zeron
                    .queue_command(
                        &chat.id,
                        &SessionCommandPayload::Run {
                            request,
                            message_id: uuid::Uuid::new_v4().to_string(),
                        },
                    )
                    .await?
            }
            "steer" => {
                self.zeron
                    .queue_command(
                        &chat.id,
                        &SessionCommandPayload::Steer {
                            prompt: text,
                            message_id: Some(uuid::Uuid::new_v4().to_string()),
                        },
                    )
                    .await?
            }
            _ => self.zeron.queue_message(&chat.id, &text).await?,
        };
        Ok(json!({
            "delivery": chosen,
            "id": id,
            "chatStatusAtSend": status,
        }))
    }

    async fn await_pending_turn(
        &self,
        chat: &Chat,
        pending: Arc<PendingTurn>,
        timeout: Duration,
    ) -> anyhow::Result<Value> {
        let turn = self
            .await_turn(
                chat,
                pending.baseline.as_ref(),
                true,
                timeout,
                pending.start.clone(),
            )
            .await?;
        if turn["timedOut"] == false {
            let mut turns = self.pending_turns.lock().await;
            // A concurrent send may already have installed a newer baseline.
            if turns
                .get(&chat.id)
                .is_some_and(|p| Arc::ptr_eq(p, &pending))
            {
                turns.remove(&chat.id);
            }
        }
        Ok(turn)
    }

    /// Wait, then report the outcome with the assistant messages that
    /// are not in `start`'s baseline.
    async fn await_turn(
        &self,
        chat: &Chat,
        baseline: Option<&Session>,
        expect_turn: bool,
        timeout: Duration,
        start: TurnStart,
    ) -> anyhow::Result<Value> {
        let deadline = Instant::now() + timeout;
        let (mut outcome, session) = self
            .zeron
            .wait_for_turn(chat, baseline, expect_turn, timeout)
            .await?;
        // Registry session updates can arrive before the separate transcript doc.
        // Keep the same deadline while waiting for a response to this send.
        let mut rendered = Vec::new();
        let mut replies = Vec::new();
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                if outcome == TurnOutcome::Completed && expect_turn {
                    outcome = TurnOutcome::TimedOut;
                }
                break;
            }
            let entries = match tokio::time::timeout(
                remaining,
                self.zeron.transcript_on(&chat.id, Some(&chat.device_id)),
            )
            .await
            {
                Ok(entries) => entries?,
                Err(_) => {
                    outcome = TurnOutcome::TimedOut;
                    break;
                }
            };
            rendered = render_entries(&entries, RenderOptions::default());
            replies = rendered
                .iter()
                .filter(|m| m.role == zeron_doc::MessageRole::Assistant)
                .filter(|m| !start.message_ids.contains(&m.id))
                .cloned()
                .collect();
            if !expect_turn
                || outcome != TurnOutcome::Completed
                || replies
                    .iter()
                    .any(|m| m.status != Some(zeron_doc::MessageStatus::Streaming))
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100).min(remaining)).await;
        }
        let (status, _) = status_of(session.as_ref());
        let mut result = json!({
            "outcome": outcome,
            "status": status,
            "timedOut": outcome == TurnOutcome::TimedOut,
            "pendingInput": last_pending_input(&rendered),
            "replies": replies,
        });
        if let Some(failure) = failure_of(&replies) {
            result["failure"] = failure;
        }
        if outcome == TurnOutcome::TimedOut {
            let retry = retry_json(session.as_ref());
            if !retry.is_null() {
                result["retry"] = retry;
                result["note"] =
                    json!("The provider is failing and Pi is retrying by itself. Not stalled.");
            }
        }
        Ok(result)
    }
}

/// claude-code when it is offered here, else the first available harness.
/// An orchestrator's tree stops growing [`MAX_DEPTH`] levels below its root.
///
/// [`MAX_DEPTH`]: zeron_proto::orchestrator::MAX_DEPTH
/// Refuse a prompt or message over [`MAX_PROMPT_CHARS`] before anything is
/// written.
fn ensure_prompt_fits(what: &str, text: &str) -> anyhow::Result<()> {
    let chars = text.chars().count();
    anyhow::ensure!(
        chars <= MAX_PROMPT_CHARS,
        "{what} is {chars} characters; the limit is {MAX_PROMPT_CHARS}. Put long material in a file in the worktree and send its path."
    );
    Ok(())
}

fn ensure_room_below(chats: &[Chat], parent: &Chat) -> anyhow::Result<()> {
    let max = zeron_proto::orchestrator::MAX_DEPTH;
    anyhow::ensure!(
        zeron_proto::orchestrator::depth(chats, &parent.id).is_none_or(|depth| depth < max),
        "This session is {max} levels below its orchestrator, the deepest a session may start more sessions from. Do the work here or report back."
    );
    Ok(())
}

/// The model a chat is configured with, if it names one.
fn chat_model(chat: &Chat) -> Option<String> {
    chat.config.as_ref()?.model.clone()
}

/// Refuse a session on a reserved model (Claude Fable, GPT Astra). Without a
/// `model` the session runs on Pi's own default, which Zeron cannot see, so
/// a launcher that itself runs on a reserved model must name another one.
fn ensure_model_not_reserved(
    model: Option<&str>,
    launcher_models: &[String],
) -> anyhow::Result<()> {
    use zeron_proto::reserved_models::{refusal, reserved_family};
    match model.map(str::trim).filter(|m| !m.is_empty()) {
        Some(model) => anyhow::ensure!(
            reserved_family(model).is_none(),
            "{}",
            refusal(model, "create_chat")
        ),
        None => {
            if let Some(reserved) = launcher_models
                .iter()
                .find(|m| reserved_family(m).is_some())
            {
                anyhow::bail!(
                    "create_chat needs a model: this chat runs on {reserved}, a Claude Fable or GPT Astra class model reserved for sessions the user starts, and a session created without `model` could start on it. Pass `model` with another model id from list_models, e.g. an Opus, Sonnet or GPT model."
                );
            }
        }
    }
    Ok(())
}

/// The same folder, spelled with either separator, a trailing separator,
/// or (Windows paths) another case.
/// `whoami`'s `place`: the calling chat's role, task, parent and
/// orchestrator, so a session (of any harness) knows where it is.
fn describe_place(chats: &[Chat], chat_id: &str) -> Value {
    use zeron_proto::orchestrator::{Place, Role};
    let place = Place::of(chats, chat_id);
    let title_of = |id: &str| {
        chats
            .iter()
            .find(|c| c.id == id)
            .and_then(|c| c.title.clone())
    };
    json!({
        "role": match place.role {
            Role::Chat if place.parent_id.is_some() => "side chat",
            Role::Chat => "chat",
            Role::Root => "orchestrator",
            Role::Member { .. } => "session in an orchestrator's tree",
        },
        "level": place.depth(),
        "task": &place.title,
        "parent": place.parent_id.as_ref().map(|id| json!({ "id": id, "title": &place.parent_title })),
        "orchestrator": place.root_id.as_ref().map(|id| json!({ "id": id, "title": title_of(id) })),
        "brief": place.brief(),
    })
}

/// The worktree a writing session in a git project's folder runs in: one of
/// its own, started from `base` (the project's HEAD by default). None for a
/// read-only session, a non-git project, or a session given another folder
/// (such as a worktree it already has).
fn isolation(
    sandbox: SandboxLevel,
    space: Option<&Space>,
    cwd: Option<&str>,
    base: Option<&str>,
) -> Option<WorktreeSpec> {
    let space = space.filter(|s| s.git_detected)?;
    if sandbox == SandboxLevel::ReadOnly || cwd.is_some_and(|cwd| !same_folder(cwd, &space.path)) {
        return None;
    }
    Some(WorktreeSpec {
        repo_path: space.path.clone(),
        base: base.unwrap_or("HEAD").to_owned(),
        space_id: Some(space.id.clone()),
    })
}

fn same_folder(a: &str, b: &str) -> bool {
    let norm = |p: &str| {
        let p = p.trim().replace('\\', "/");
        let p = p.trim_end_matches('/').to_owned();
        if p.as_bytes().get(1) == Some(&b':') {
            p.to_lowercase()
        } else {
            p
        }
    };
    norm(a) == norm(b)
}

fn default_harness(harnesses: &[HarnessInfo]) -> anyhow::Result<HarnessId> {
    harnesses
        .iter()
        .find(|h| h.id == HarnessId::ClaudeCode && h.available())
        .or_else(|| {
            harnesses
                .iter()
                .find(|h| h.available() && h.id != HarnessId::Mock)
        })
        .map(|h| h.id)
        .ok_or_else(|| {
            anyhow::anyhow!("no harness is available on this device (see list_harnesses)")
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::zeron::Origin;
    use async_trait::async_trait;
    use futures::StreamExt;
    use std::sync::Mutex;
    use zeron_rpc::{RpcError, RpcReply, RpcService, memory_client, methods};

    #[test]
    fn failure_of_reports_the_newest_classified_reply() {
        let reply = |id: &str, class: Option<zeron_proto::ErrorClass>| {
            let entry = zeron_doc::SessionMessageEntry {
                duration_ms: None,
                id: id.into(),
                role: zeron_doc::MessageRole::Assistant,
                parts: vec![zeron_doc::MessagePart::Error {
                    id: "e".into(),
                    message: "boom".into(),
                    class,
                }],
                created_at: 0,
                device_id: "d".into(),
                status: None,
                continuation_of: None,
            };
            render_entries(&[entry], RenderOptions::default()).remove(0)
        };
        assert_eq!(failure_of(&[]), None);
        assert_eq!(failure_of(&[reply("a", None)]), None);
        let replies = [
            reply("a", Some(zeron_proto::ErrorClass::SessionGone)),
            reply("b", Some(zeron_proto::ErrorClass::ProviderOverloaded)),
            reply("c", None),
        ];
        assert_eq!(failure_of(&replies).unwrap()["class"], "providerOverloaded");
    }

    /// Two devices with distinct catalogs and repeated project paths, plus
    /// two chats with a two-message transcript. Writes are recorded for assertions.
    #[derive(Default)]
    struct World {
        writes: Mutex<Vec<(String, Value)>>,
        dispatch_barrier: Option<tokio::sync::Barrier>,
        beta_parent: Option<String>,
        beta_remote: bool,
        extra_chats: Vec<Value>,
        sessions: Vec<Value>,
        catalog_error: Option<&'static str>,
        reads: Mutex<Vec<(String, Value)>>,
        paused: bool,
        /// The orchestrator project's `policy`, `pauseReason` and spend so far.
        policy: Value,
        pause_reason: &'static str,
        spent_usd: f64,
        /// What `GithubPreflight` answers; ready when unset.
        preflight: Option<Value>,
        /// Set once `RegisterProject` has added its project to the list.
        registered: std::sync::atomic::AtomicBool,
    }

    /// The project `RegisterProject` finds for "t3code".
    fn new_space() -> Value {
        json!({
            "id": "space-new", "deviceId": "dev-local", "path": "/home/me/codebase/t3code",
            "gitDetected": true, "createdAt": "2026-09-04T00:00:00Z"
        })
    }

    fn stream(item: Value) -> RpcReply {
        RpcReply::Stream(futures::stream::iter(vec![item]).boxed())
    }

    #[async_trait]
    impl RpcService for World {
        async fn handle(&self, method: &str, params: Value) -> Result<RpcReply, RpcError> {
            if matches!(method, methods::LIST_HARNESSES | methods::LIST_MODELS) {
                self.reads
                    .lock()
                    .unwrap()
                    .push((method.into(), params.clone()));
                if self.catalog_error == Some(method) {
                    return Err(RpcError::Failed("catalog unavailable".into()));
                }
            }
            let remote = params["targetDeviceId"] == "dev-remote";
            Ok(match method {
                methods::LOCAL_DEVICE => RpcReply::Value(json!({ "deviceId": "dev-local" })),
                methods::ENGINE_INFO => RpcReply::Value(json!({
                    "deviceId": "dev-local", "workspaceScope": "local"
                })),
                methods::WATCH_DEVICES => stream(json!([{
                    "id": "dev-local", "name": "Laptop", "platform": "linux",
                    "lastSeenAt": null
                }, {"id": "dev-remote", "name": "Worker", "platform": "linux", "lastSeenAt": null}])),
                methods::WATCH_SPACES => {
                    let mut spaces = json!([{
                        "id": "space-1", "deviceId": "dev-local", "path": "/repo/comet",
                        "gitDetected": true, "createdAt": "2026-09-01T00:00:00Z"
                    }, {
                        "id": "space-remote", "deviceId": "dev-remote", "path": "/repo/comet",
                        "gitDetected": true, "createdAt": "2026-09-01T00:00:00Z"
                    }, {
                        "id": "space-unique", "deviceId": "dev-remote", "path": "/repo/unique",
                        "gitDetected": true, "createdAt": "2026-09-01T00:00:00Z"
                    }]);
                    if self.registered.load(std::sync::atomic::Ordering::SeqCst) {
                        spaces.as_array_mut().unwrap().push(new_space());
                    }
                    stream(spaces)
                }
                methods::WATCH_CHATS => {
                    let mut chats = json!([
                    {
                        "id": "chat-alpha-1", "deviceId": "dev-local", "title": "Alpha",
                        "archived": false, "spaceId": "space-1",
                        "config": { "harness": "claude-code", "model": "opus", "reasoning": null, "sandbox": "workspace-write" },
                        "createdAt": "2026-09-01T00:00:00Z"
                    },
                    {
                        "id": "chat-beta-2", "deviceId": if self.beta_remote { "dev-remote" } else { "dev-local" }, "title": "Beta",
                        "parentChatId": self.beta_parent,
                        "archived": false, "spaceId": "space-1",
                        "createdAt": "2026-09-02T00:00:00Z"
                    }
                    ]);
                    chats
                        .as_array_mut()
                        .unwrap()
                        .extend(self.extra_chats.clone());
                    stream(chats)
                }
                methods::WATCH_SESSIONS => stream(json!(self.sessions)),
                methods::POST_BOARD => {
                    self.writes
                        .lock()
                        .unwrap()
                        .push((method.to_owned(), params.clone()));
                    let folder = if params["boardChatId"] == "m-elsewhere" {
                        "space:space-2"
                    } else {
                        "space:space-1"
                    };
                    RpcReply::Value(json!({ "entry": {
                        "id": "0000000000001-000000-dev-local", "rootId": "orchestrator-t",
                        "folder": folder, "authorChatId": params["chatId"],
                        "authorTitle": "Member A", "authorDepth": 1,
                        "kind": params["kind"], "text": params["text"], "at": 1,
                    }}))
                }
                methods::LIST_BOARD => {
                    self.reads
                        .lock()
                        .unwrap()
                        .push((method.to_owned(), params.clone()));
                    RpcReply::Value(json!({ "entries": [], "total": 0, "olderSkipped": 0 }))
                }
                methods::GET_ORCHESTRATOR_PROJECT => {
                    self.reads
                        .lock()
                        .unwrap()
                        .push((method.to_owned(), params.clone()));
                    let mut project = json!({
                        "rootId": "orchestrator-root", "goal": "Ship it",
                        "instructions": "", "paused": self.paused,
                        "pauseReason": self.pause_reason, "updatedAt": 1,
                    });
                    if !self.policy.is_null() {
                        project["policy"] = self.policy.clone();
                    }
                    RpcReply::Value(json!({ "project": project, "memory": [] }))
                }
                methods::GET_TREE_SPEND => {
                    self.reads
                        .lock()
                        .unwrap()
                        .push((method.to_owned(), params.clone()));
                    RpcReply::Value(json!({ "spentUsd": self.spent_usd, "requests": 3 }))
                }
                methods::GITHUB_PREFLIGHT => {
                    self.reads
                        .lock()
                        .unwrap()
                        .push((method.to_owned(), params.clone()));
                    RpcReply::Value(self.preflight.clone().unwrap_or_else(
                        || json!({ "ok": true, "verdict": "ready to push", "warnings": [] }),
                    ))
                }
                methods::REGISTER_PROJECT => {
                    self.writes
                        .lock()
                        .unwrap()
                        .push((method.to_owned(), params.clone()));
                    if params["dryRun"] == true {
                        RpcReply::Value(json!({ "matches": [new_space()] }))
                    } else {
                        self.registered
                            .store(true, std::sync::atomic::Ordering::SeqCst);
                        RpcReply::Value(json!({ "space": new_space(), "created": true }))
                    }
                }
                methods::MERGE_PREVIEW => {
                    self.reads
                        .lock()
                        .unwrap()
                        .push((method.to_owned(), params.clone()));
                    RpcReply::Value(json!({
                        "chatId": params["member"], "branch": "zeron/x", "base": "main",
                        "ahead": 1, "behind": 0, "conflictCheck": "clean", "conflicts": [],
                    }))
                }
                methods::ORCHESTRATOR_MEMORY | methods::UPDATE_ORCHESTRATOR_PROJECT => {
                    self.writes
                        .lock()
                        .unwrap()
                        .push((method.to_owned(), params.clone()));
                    RpcReply::Value(json!({ "ok": true }))
                }
                methods::LIST_HARNESSES if remote => RpcReply::Value(json!([
                    {"id":"codex", "name":"Remote Codex", "installed":true},
                    {"id":"cursor", "name":"Disabled Cursor", "installed":true, "enabled":false}
                ])),
                methods::LIST_HARNESSES => RpcReply::Value(json!([
                    { "id": "claude-code", "name": "Claude Code", "supportsSteering": true,
                      "steeringMode": "step-boundary", "reasoningLevels": [], "installed": true, "enabled": true },
                    { "id": "codex", "name": "Codex", "supportsSteering": true,
                      "steeringMode": "turn-boundary", "reasoningLevels": [], "installed": false, "enabled": true }
                ])),
                methods::LIST_MODELS if remote => RpcReply::Value(json!([
                    {"id":"remote-model", "label":"Remote model"}
                ])),
                methods::LIST_MODELS => RpcReply::Value(json!([
                    { "id": "opus", "label": "Opus" }, { "id": "sonnet", "label": "Sonnet" }
                ])),
                methods::WATCH_DOC_MESSAGES => stream(json!({ "reset": [
                    { "id": "u1", "role": "user", "createdAt": 1, "deviceId": "dev-local",
                      "parts": [{ "kind": "text", "id": "t", "text": "hi" }] },
                    { "id": "a1", "role": "assistant", "createdAt": 2, "deviceId": "dev-local",
                      "status": "complete",
                      "parts": [{ "kind": "text", "id": "t", "text": "hello back" }] }
                ]})),
                methods::PUBLISH_VISUAL => {
                    self.writes
                        .lock()
                        .unwrap()
                        .push((method.to_owned(), params.clone()));
                    RpcReply::Value(json!({ "id": "v:1", "title": params["title"] }))
                }
                methods::MUTATE | methods::QUEUE_COMMAND | methods::QUEUE_MESSAGE => {
                    self.writes
                        .lock()
                        .unwrap()
                        .push((method.to_owned(), params));
                    if method == methods::QUEUE_COMMAND
                        && let Some(barrier) = &self.dispatch_barrier
                    {
                        // No dispatch can finish until both chats have work.
                        // A sequential batch deadlocks here, before any waits.
                        barrier.wait().await;
                    }
                    RpcReply::Value(json!({ "commandId": "cmd-1", "id": "q-1" }))
                }
                other => return Err(RpcError::UnknownMethod(other.into())),
            })
        }
    }

    fn tools(world: Arc<World>, origin: Origin) -> Tools {
        let client = memory_client(world);
        Tools::new(Arc::new(Zeron::with_client(client, origin)))
    }

    #[tokio::test]
    async fn catalog_is_well_formed() {
        let defs = catalog();
        let mut names: Vec<&str> = defs.iter().map(|d| d.name).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), defs.len(), "tool names must be unique");
        for def in &defs {
            assert_eq!(def.input_schema["type"], "object", "{}", def.name);
            assert!(!def.description.is_empty());
        }
    }

    #[test]
    fn the_show_visual_example_passes_its_own_check() {
        let start = SHOW_VISUAL_DESCRIPTION.find("local s = ui.state").unwrap();
        let end = start
            + SHOW_VISUAL_DESCRIPTION[start..]
                .find("\n\nZeron runs the script")
                .unwrap();
        let report = zeron_visual::check(&SHOW_VISUAL_DESCRIPTION[start..end], 1).unwrap();
        assert_eq!(report.interactions, 2, "both tabs");
    }

    #[tokio::test]
    async fn show_visual_is_offered_only_inside_a_chat() {
        let world = Arc::new(World::default());
        let outside = tools(world.clone(), Origin::default());
        assert!(!outside.has("show_visual"));
        assert!(
            outside
                .call(
                    "show_visual",
                    json!({ "script": "return ui.text 'x'", "title": "x" })
                )
                .await
                .is_err()
        );

        let inside = tools(
            world.clone(),
            Origin {
                chat_id: Some("chat-alpha-1".into()),
                device_id: Some("dev-local".into()),
            },
        );
        assert!(inside.has("show_visual"));
        // The guidance names every color the runtime accepts.
        for name in zeron_visual::TOKEN_NAMES {
            assert!(SHOW_VISUAL_DESCRIPTION.contains(name), "{name}");
        }
        let shown = inside
            .call(
                "show_visual",
                json!({ "script": "return ui.text 'chart'", "title": "Chart" }),
            )
            .await
            .unwrap();
        assert_eq!(shown["visual"]["title"], "Chart");
        let writes = world.writes.lock().unwrap();
        let (method, params) = writes.last().unwrap();
        assert_eq!(method, methods::PUBLISH_VISUAL);
        assert_eq!(params["chatId"], "chat-alpha-1");
        assert_eq!(params["script"], "return ui.text 'chart'");
    }

    /// An orchestrator tree whose sessions are in every state, a second tree,
    /// and a session in another folder.
    fn board_world() -> World {
        let chat = |id: &str, parent: Option<&str>, space: &str, title: &str, archived: bool| {
            json!({
                "id": id, "deviceId": "dev-local", "title": title, "archived": archived,
                "spaceId": if space.is_empty() { Value::Null } else { json!(space) },
                "parentChatId": parent,
                "config": { "harness": "claude-code", "model": null, "reasoning": null, "sandbox": "workspace-write" },
                "createdAt": "2026-10-16T00:00:00Z"
            })
        };
        let session = |id: &str, status: &str| {
            json!({ "chatId": id, "deviceId": "dev-local", "status": status,
                    "startedAt": null, "updatedAt": "2026-10-16T00:00:00Z" })
        };
        let root = Some("orchestrator-t");
        World {
            extra_chats: vec![
                chat("orchestrator-t", None, "", "Orchestrator", false),
                chat("m-a", root, "space-1", "Member A", false),
                chat("m-work", root, "space-1", "Working", false),
                chat("m-ask", root, "space-1", "Asking", false),
                chat("m-idle", root, "space-1", "Idle", false),
                chat("m-gone", root, "space-1", "Archived", true),
                chat("m-elsewhere", root, "space-2", "Elsewhere", false),
                chat("orchestrator-x", None, "", "Orchestrator", false),
                chat(
                    "x-1",
                    Some("orchestrator-x"),
                    "space-1",
                    "Other tree",
                    false,
                ),
            ],
            sessions: vec![
                session("m-work", "working"),
                session("m-ask", "awaitingInput"),
                session("m-idle", "idle"),
                session("m-gone", "working"),
                session("m-elsewhere", "working"),
                session("x-1", "working"),
            ],
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn a_board_post_reaches_the_live_sessions_of_its_folder_only() {
        let world = Arc::new(board_world());
        let outside = tools(world.clone(), Origin::default());
        assert!(!outside.has("board_post") && !outside.has("board_read"));
        let member = tools(
            world.clone(),
            Origin {
                chat_id: Some("m-a".into()),
                device_id: Some("dev-local".into()),
            },
        );
        assert!(member.has("board_post") && member.has("board_read"));
        let posted = member
            .call(
                "board_post",
                json!({ "kind": "finding", "text": "the parser is in src/parse.rs" }),
            )
            .await
            .unwrap();
        assert_eq!(posted["entry"]["text"], "the parser is in src/parse.rs");
        let delivery = |id: &str| {
            posted["recipients"]
                .as_array()
                .unwrap()
                .iter()
                .find(|r| r["chatId"] == id)
                .map(|r| r["delivery"].clone())
        };
        assert_eq!(delivery("m-work"), Some(json!("steer")));
        assert_eq!(delivery("m-ask"), Some(json!("queue")));
        assert!(
            delivery("m-idle")
                .unwrap()
                .as_str()
                .unwrap()
                .starts_with("none")
        );
        assert!(
            delivery("orchestrator-t")
                .unwrap()
                .as_str()
                .unwrap()
                .starts_with("none"),
            "an idle root is not woken for a finding"
        );
        for absent in ["m-a", "m-gone", "m-elsewhere", "orchestrator-x", "x-1"] {
            assert_eq!(delivery(absent), None, "{absent}");
        }
        {
            let writes = world.writes.lock().unwrap();
            assert_eq!(writes[0].0, methods::POST_BOARD);
            assert_eq!(writes[0].1["chatId"], "m-a");
            assert_eq!(writes[0].1["kind"], "finding");
            let steer = writes
                .iter()
                .find(|(method, params)| {
                    method == methods::QUEUE_COMMAND && params["chatId"] == "m-work"
                })
                .expect("the working session is steered");
            let prompt = steer.1["command"]["prompt"].as_str().unwrap();
            assert!(
                prompt.starts_with("[Board finding from Zeron chat Member A (m-a), level 1."),
                "{prompt}"
            );
            assert!(prompt.ends_with("the parser is in src/parse.rs"));
            assert!(
                writes
                    .iter()
                    .any(|(method, params)| method == methods::QUEUE_MESSAGE
                        && params["chatId"] == "m-ask")
            );
        }
        // A blocker wakes an idle orchestrator.
        let blocked = member
            .call(
                "board_post",
                json!({ "kind": "blocker", "text": "CI is red on main" }),
            )
            .await
            .unwrap();
        let root = blocked["recipients"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["chatId"] == "orchestrator-t")
            .unwrap()
            .clone();
        assert_eq!(root["delivery"], "run");

        member
            .call(
                "board_read",
                json!({ "since": "0000000000001-000000-dev-local", "limit": 5 }),
            )
            .await
            .unwrap();
        let reads = world.reads.lock().unwrap();
        let (method, params) = reads.last().unwrap();
        assert_eq!(method, methods::LIST_BOARD);
        assert_eq!(params["chatId"], "m-a");
        assert_eq!(params["since"], "0000000000001-000000-dev-local");
        assert_eq!(params["limit"], 5);
    }

    #[tokio::test]
    async fn the_orchestrator_posts_to_one_folders_board() {
        let world = Arc::new(board_world());
        let root = tools(
            world.clone(),
            Origin {
                chat_id: Some("orchestrator-t".into()),
                device_id: Some("dev-local".into()),
            },
        );
        let posted = root
            .call(
                "board_post",
                json!({ "kind": "finding", "text": "the API moved to v2", "session": "m-elsewhere" }),
            )
            .await
            .unwrap();
        let recipients: Vec<_> = posted["recipients"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["chatId"].as_str().unwrap().to_owned())
            .collect();
        assert_eq!(recipients, ["m-elsewhere"], "never the other folder's sessions");
        let writes = world.writes.lock().unwrap();
        assert_eq!(writes[0].1["boardChatId"], "m-elsewhere");
    }

    #[tokio::test]
    async fn whoami_names_the_role_task_parent_and_orchestrator() {
        let world = Arc::new(board_world());
        let member = tools(
            world.clone(),
            Origin {
                chat_id: Some("m-a".into()),
                device_id: Some("dev-local".into()),
            },
        );
        let me = member.call("whoami", json!({})).await.unwrap();
        assert_eq!(me["place"]["role"], "session in an orchestrator's tree");
        assert_eq!(me["place"]["level"], 1);
        assert_eq!(me["place"]["task"], "Member A");
        assert_eq!(me["place"]["parent"]["id"], "orchestrator-t");
        assert_eq!(me["place"]["orchestrator"]["title"], "Orchestrator");
        assert!(
            me["place"]["brief"]
                .as_str()
                .unwrap()
                .contains("the orchestrator itself")
        );
        let root = tools(
            world,
            Origin {
                chat_id: Some("orchestrator-t".into()),
                device_id: None,
            },
        );
        let me = root.call("whoami", json!({})).await.unwrap();
        assert_eq!(me["place"]["role"], "orchestrator");
        assert_eq!(me["place"]["level"], 0);
        assert_eq!(me["place"]["orchestrator"]["id"], "orchestrator-t");
    }

    #[tokio::test]
    async fn list_and_read_chats() {
        let world = Arc::new(World::default());
        let tools = tools(world, Origin::default());
        let listed = tools.call("list_chats", json!({})).await.unwrap();
        assert_eq!(listed["total"], 2);
        // Newest activity first: Beta was created later.
        assert_eq!(listed["chats"][0]["title"], "Beta");
        assert_eq!(listed["chats"][1]["project"]["name"], "comet");
        assert_eq!(listed["chats"][1]["status"], "idle");

        let read = tools
            .call("read_chat", json!({ "chat": "alpha" }))
            .await
            .unwrap();
        assert_eq!(read["total"], 2);
        assert_eq!(read["messages"][1]["text"], "hello back");

        let read = tools
            .call("read_chat", json!({ "chat": "chat-alpha", "limit": 1 }))
            .await
            .unwrap();
        assert_eq!(read["returned"], 1);
        assert_eq!(read["olderRemaining"], 1);
        assert_eq!(read["messages"][0]["id"], "a1");
    }

    #[tokio::test]
    async fn send_attributes_and_refuses_self() {
        let world = Arc::new(World::default());
        let tools = tools(
            world.clone(),
            Origin {
                chat_id: Some("chat-beta-2".into()),
                device_id: Some("dev-local".into()),
            },
        );
        let err = tools
            .call("send_message", json!({ "chat": "beta", "text": "loop" }))
            .await
            .unwrap_err();
        assert!(err.contains("own chat"), "{err}");

        let sent = tools
            .call(
                "send_message",
                json!({ "chat": "alpha", "text": "please review" }),
            )
            .await
            .unwrap();
        assert_eq!(sent["sent"]["delivery"], "run");
        let writes = world.writes.lock().unwrap();
        let (method, params) = writes.last().expect("a queued command");
        assert_eq!(method, methods::QUEUE_COMMAND);
        assert_eq!(params["chatId"], "chat-alpha-1");
        assert_eq!(params["command"]["kind"], "run");
        let prompt = params["command"]["request"]["prompt"].as_str().unwrap();
        assert!(
            prompt.starts_with("[Message from Zeron chat Beta (chat-bet)"),
            "{prompt}"
        );
        assert!(prompt.ends_with("please review"));
        assert!(prompt.contains("tool, chat chat-beta-2.]"), "{prompt}");
        assert_eq!(params["command"]["request"]["cwd"], "/repo/comet");
        assert_eq!(params["command"]["request"]["harness"], "claude-code");
        assert_eq!(params["command"]["request"]["model"], "opus");
    }

    #[tokio::test]
    async fn replies_use_the_full_id_with_multiple_voice_orchestrators() {
        let first = "voice-orchestrator-11111111-1111-4111-8111-111111111111";
        let second = "voice-orchestrator-22222222-2222-4222-8222-222222222222";
        let world = Arc::new(World {
            extra_chats: [first, second]
                .into_iter()
                .map(|id| {
                    json!({
                        "id": id, "deviceId": "dev-local", "title": "Voice session",
                        "archived": false, "createdAt": "2026-10-01T00:00:00Z"
                    })
                })
                .collect(),
            ..Default::default()
        });
        let orchestrator = tools(
            world.clone(),
            Origin {
                chat_id: Some(second.into()),
                device_id: None,
            },
        );
        let error = orchestrator
            .zeron
            .resolve_chat("voice-or")
            .await
            .unwrap_err();
        assert!(error.to_string().contains("2 chats start with"), "{error}");
        orchestrator
            .call(
                "send_message",
                json!({
                    "chat": "alpha", "text": "please review"
                }),
            )
            .await
            .unwrap();
        let reply_id = {
            let writes = world.writes.lock().unwrap();
            let prompt = writes.last().unwrap().1["command"]["request"]["prompt"]
                .as_str()
                .unwrap();
            prompt
                .split_once("tool, chat ")
                .unwrap()
                .1
                .split_once(".]\n\n")
                .unwrap()
                .0
                .to_owned()
        };
        assert_eq!(reply_id, second);
        let worker = tools(
            world.clone(),
            Origin {
                chat_id: Some("chat-alpha-1".into()),
                device_id: None,
            },
        );
        worker
            .call(
                "send_message",
                json!({
                    "chat": reply_id, "text": "review complete"
                }),
            )
            .await
            .unwrap();
        let writes = world.writes.lock().unwrap();
        assert_eq!(writes.last().unwrap().1["chatId"], second);
    }

    #[test]
    fn status_of_reports_a_capacity_queue() {
        let mut session = Session {
            chat_id: "c".into(),
            device_id: "dev-local".into(),
            status: SessionStatus::Working,
            started_at: None,
            updated_at: chrono::Utc::now(),
            last_completed_turn: None,
            retry: None,
            running_subagents: 0,
            queue_position: 2,
            background_jobs: 0,
            subagents: Vec::new(),
        };
        assert_eq!(status_of(Some(&session)).0, "queued for capacity (#2)");
        session.queue_position = 0;
        assert_eq!(status_of(Some(&session)).0, "working");
        // A stale row is stale first, queued or not.
        session.queue_position = 2;
        session.updated_at = chrono::Utc::now() - chrono::Duration::minutes(10);
        assert!(status_of(Some(&session)).0.starts_with("working (stale"));
    }

    #[test]
    fn only_a_fresh_idle_session_waits_on_its_jobs() {
        let mut session = Session {
            chat_id: "c".into(),
            device_id: "dev-local".into(),
            status: SessionStatus::Idle,
            started_at: None,
            updated_at: chrono::Utc::now(),
            last_completed_turn: None,
            retry: None,
            running_subagents: 0,
            queue_position: 0,
            background_jobs: 2,
            subagents: Vec::new(),
        };
        assert_eq!(waiting_on_jobs(Some(&session)), 2);
        session.status = SessionStatus::Working;
        assert_eq!(waiting_on_jobs(Some(&session)), 0, "a working chat is steered");
        session.status = SessionStatus::Idle;
        session.updated_at = chrono::Utc::now() - chrono::Duration::minutes(10);
        assert_eq!(waiting_on_jobs(Some(&session)), 0, "a stale row proves nothing");
        assert_eq!(waiting_on_jobs(None), 0);
    }

    #[tokio::test]
    async fn auto_steers_busy_chats_even_at_turn_boundaries_or_after_long_quiet_tools() {
        let world = Arc::new(World::default());
        let tools = tools(world.clone(), Origin::default());
        let chats = tools.zeron.chats().await.unwrap();
        let session = Session {
            chat_id: chats[0].id.clone(),
            device_id: "dev-local".into(),
            status: SessionStatus::Working,
            started_at: None,
            updated_at: chrono::Utc::now() - chrono::Duration::minutes(10),
            last_completed_turn: None,
            retry: None,
            running_subagents: 0,
            queue_position: 0,
            background_jobs: 0,
            subagents: Vec::new(),
        };
        // No mid-turn capability is required for a live mailbox delivery.
        let sent = tools
            .deliver(
                &chats[0],
                None,
                &[],
                Some(&session),
                "follow up".into(),
                "auto",
                None,
            )
            .await
            .unwrap();
        assert_eq!(sent["delivery"], "steer");
        let writes = world.writes.lock().unwrap();
        assert_eq!(writes.len(), 1);
        assert_eq!(writes[0].0, methods::QUEUE_COMMAND);
        assert_eq!(writes[0].1["command"]["kind"], "steer");
    }

    #[tokio::test]
    async fn create_chat_writes_the_row_and_validates_model() {
        let world = Arc::new(World::default());
        let tools = tools(world.clone(), Origin::default());
        let err = tools
            .call(
                "create_chat",
                json!({ "project": "space-1", "model": "nope" }),
            )
            .await
            .unwrap_err();
        assert!(err.contains("not offered"), "{err}");

        let created = tools
            .call(
                "create_chat",
                json!({ "project": "space-1", "model": "sonnet", "title": "Review", "prompt": "go" }),
            )
            .await
            .unwrap();
        let chat_id = created["chatId"].as_str().unwrap().to_owned();
        let writes = world.writes.lock().unwrap();
        assert_eq!(writes.len(), 3, "createChat, renameChat, run");
        assert_eq!(writes[0].1["op"], "createChat");
        assert_eq!(writes[0].1["chatId"], chat_id);
        assert_eq!(writes[0].1["spaceId"], "space-1");
        assert!(
            writes[0].1.get("parentChatId").is_none(),
            "no origin, no parent"
        );
        assert_eq!(writes[0].1["config"]["harness"], "claude-code");
        assert_eq!(writes[0].1["config"]["model"], "sonnet");
        assert_eq!(writes[1].1["op"], "renameChat");
        assert_eq!(writes[2].0, methods::QUEUE_COMMAND);
        assert_eq!(writes[2].1["command"]["request"]["prompt"], "go");
    }

    #[tokio::test]
    async fn create_chat_refuses_a_reserved_model_and_an_unnamed_one_from_a_reserved_chat() {
        let reserved = json!({
            "id": "chat-reserved", "deviceId": "dev-local", "title": "Reserved",
            "archived": false, "spaceId": "space-1", "createdAt": "2026-09-03T00:00:00Z",
            "config": { "harness": "claude-code", "model": "anthropic/claude-fable-5", "reasoning": null, "sandbox": "workspace-write" },
        });
        let world = Arc::new(World {
            extra_chats: vec![reserved],
            ..Default::default()
        });
        // An explicit reserved model is refused before the catalog is asked,
        // from any chat.
        for origin in [None, Some("chat-alpha-1"), Some("chat-reserved")] {
            let tools = tools(
                world.clone(),
                Origin {
                    chat_id: origin.map(str::to_owned),
                    device_id: None,
                },
            );
            for model in ["anthropic/claude-fable-5", "openai/gpt-6-astra-mini"] {
                let err = tools
                    .call(
                        "create_chat",
                        json!({ "project": "space-1", "model": model }),
                    )
                    .await
                    .unwrap_err();
                assert!(err.contains("cannot start on"), "{err}");
                assert!(err.contains("reserved"), "{err}");
            }
        }
        // Without a model the child runs on Pi's default: fine from a chat on
        // an ordinary model, refused from one on a reserved model.
        let from_alpha = tools(
            world.clone(),
            Origin {
                chat_id: Some("chat-alpha-1".into()),
                device_id: None,
            },
        );
        from_alpha
            .call("create_chat", json!({ "project": "space-1" }))
            .await
            .unwrap();
        let from_reserved = tools(
            world.clone(),
            Origin {
                chat_id: Some("chat-reserved".into()),
                device_id: None,
            },
        );
        let err = from_reserved
            .call("create_chat", json!({ "project": "space-1" }))
            .await
            .unwrap_err();
        assert!(err.contains("Pass `model`"), "{err}");
        // A reserved explicit parent counts as the launcher too.
        let err = from_alpha
            .call(
                "create_chat",
                json!({ "project": "space-1", "parent": "chat-reserved" }),
            )
            .await
            .unwrap_err();
        assert!(err.contains("Pass `model`"), "{err}");
        // Naming another model works from the reserved chat.
        from_reserved
            .call(
                "create_chat",
                json!({ "project": "space-1", "model": "sonnet" }),
            )
            .await
            .unwrap();
        let created = world
            .writes
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, params)| params["op"] == "createChat")
            .count();
        assert_eq!(created, 2, "only the two allowed sessions were written");
    }

    #[tokio::test]
    async fn a_parent_picks_the_sessions_thinking_level_or_turns_it_off() {
        let world = Arc::new(World::default());
        let tools = tools(world.clone(), Origin::default());
        let err = tools
            .call(
                "create_chat",
                json!({ "project": "space-1", "reasoning": "extreme" }),
            )
            .await
            .unwrap_err();
        assert!(err.contains("off, minimal, low, medium, high, xhigh, max"), "{err}");
        let high = tools
            .call(
                "create_chat",
                json!({ "project": "space-1", "reasoning": "xhigh" }),
            )
            .await
            .unwrap();
        assert_eq!(high["reasoning"], "xhigh");
        let off = tools
            .call("create_chat", json!({ "project": "space-1", "reasoning": "off" }))
            .await
            .unwrap();
        assert_eq!(off["reasoning"], "off");
        let writes = world.writes.lock().unwrap();
        assert_eq!(writes[0].1["config"]["reasoning"], "xhigh");
        assert_eq!(writes[1].1["config"]["reasoning"], Value::Null);
        assert_eq!(writes[1].1["config"]["modelOptions"]["pi_thinking"], "off");
    }

    #[tokio::test]
    async fn a_writing_session_gets_its_own_worktree_and_a_reading_one_is_read_only() {
        let world = Arc::new(World::default());
        let tools = tools(world.clone(), Origin::default());
        let writer = tools
            .call(
                "create_chat",
                json!({ "project": "space-1", "write": true, "branch": "main", "prompt": "fix it" }),
            )
            .await
            .unwrap();
        assert_eq!(writer["write"], true);
        assert_eq!(
            writer["workspace"],
            "a new worktree, created on the first run"
        );
        let reader = tools
            .call(
                "create_chat",
                json!({ "project": "space-1", "write": false, "prompt": "find it" }),
            )
            .await
            .unwrap();
        assert_eq!(reader["workspace"], "the project folder");
        let err = tools
            .call(
                "create_chat",
                json!({ "project": "space-1", "write": false, "sandbox": "workspace-write" }),
            )
            .await
            .unwrap_err();
        assert!(err.contains("conflicts with sandbox"), "{err}");

        let writes = world.writes.lock().unwrap();
        let runs: Vec<&Value> = writes
            .iter()
            .filter(|(method, _)| method == methods::QUEUE_COMMAND)
            .map(|(_, params)| &params["command"]["request"])
            .collect();
        assert_eq!(runs.len(), 2);
        // The writer starts from the branch it named, in its own checkout.
        assert_eq!(runs[0]["worktree"]["repoPath"], "/repo/comet");
        assert_eq!(runs[0]["worktree"]["base"], "main");
        assert_eq!(runs[0]["worktree"]["spaceId"], "space-1");
        assert_eq!(runs[0]["sandbox"], "workspace-write");
        // The reader stays in the project folder and cannot edit.
        assert!(runs[1].get("worktree").is_none());
        assert_eq!(runs[1]["sandbox"], "read-only");
        let created: Vec<&Value> = writes
            .iter()
            .filter(|(_, params)| params["op"] == "createChat")
            .map(|(_, params)| params)
            .collect();
        assert!(
            created[0].get("branch").is_none(),
            "the host stamps zeron/<name>"
        );
        assert_eq!(created[1]["config"]["sandbox"], "read-only");
    }

    #[test]
    fn only_a_writing_session_in_a_git_project_folder_is_isolated() {
        let mut space = Space {
            id: "s".into(),
            device_id: "d".into(),
            path: "/repo".into(),
            name: None,
            git_detected: true,
            git_checked_at: None,
            checkout_id: None,
            repository_id: None,
            created_at: chrono::Utc::now(),
        };
        let spec = isolation(
            SandboxLevel::WorkspaceWrite,
            Some(&space),
            Some("/repo/"),
            None,
        )
        .unwrap();
        assert_eq!(spec.base, "HEAD");
        assert!(isolation(SandboxLevel::ReadOnly, Some(&space), None, None).is_none());
        // A session given another folder, such as its worktree, stays there.
        assert!(
            isolation(
                SandboxLevel::WorkspaceWrite,
                Some(&space),
                Some("/wt/repo-ab12"),
                None
            )
            .is_none()
        );
        assert!(isolation(SandboxLevel::WorkspaceWrite, None, None, None).is_none());
        space.git_detected = false;
        assert!(isolation(SandboxLevel::WorkspaceWrite, Some(&space), None, None).is_none());
    }

    #[tokio::test]
    async fn create_chat_records_the_origin_as_parent() {
        let world = Arc::new(World::default());
        let tools = tools(
            world.clone(),
            Origin {
                chat_id: Some("chat-beta-2".into()),
                device_id: None,
            },
        );
        let created = tools
            .call("create_chat", json!({ "project": "space-1" }))
            .await
            .unwrap();
        assert_eq!(created["parentChatId"], "chat-beta-2");
        assert_eq!(
            world.writes.lock().unwrap()[0].1["parentChatId"],
            "chat-beta-2"
        );

        // An explicit parent (by title) overrides the origin.
        let created = tools
            .call(
                "create_chat",
                json!({ "project": "space-1", "parent": "Alpha" }),
            )
            .await
            .unwrap();
        assert_eq!(created["parentChatId"], "chat-alpha-1");
        let err = tools
            .call(
                "create_chat",
                json!({ "project": "space-1", "parent": "nope" }),
            )
            .await
            .unwrap_err();
        assert!(err.contains("no chat matches"), "{err}");
    }

    #[tokio::test]
    async fn wait_on_an_idle_chat_returns_immediately() {
        let world = Arc::new(World::default());
        let tools = tools(world, Origin::default());
        let waited = tools
            .call(
                "wait_for_turn",
                json!({ "chat": "alpha", "timeout_secs": 5 }),
            )
            .await
            .unwrap();
        assert_eq!(waited["turn"]["outcome"], "completed");
        assert_eq!(waited["turn"]["replies"][0]["text"], "hello back");
    }

    #[tokio::test]
    async fn send_with_wait_keeps_waiting_until_a_turn_lands() {
        // The stub never grows a session row, so a send-then-wait must run
        // to its deadline rather than declaring the (unstarted) run done.
        let world = Arc::new(World::default());
        let tools = tools(world, Origin::default());
        let started = std::time::Instant::now();
        let sent = tools
            .call(
                "send_message",
                json!({ "chat": "alpha", "text": "hi", "wait": true, "timeout_secs": 1 }),
            )
            .await
            .unwrap();
        assert_eq!(sent["turn"]["outcome"], "timedOut");
        assert!(started.elapsed() >= Duration::from_millis(900));
    }

    #[tokio::test]
    async fn batches_dispatch_all_chats_before_waiting_and_keep_partial_results() {
        for (name, requests) in [
            (
                "create_chats",
                json!([
                    {"prompt": "first", "wait": true, "timeout_secs": 1},
                    {"harness": "not-a-harness"},
                    {"prompt": "second", "wait": true, "timeout_secs": 1},
                ]),
            ),
            (
                "send_messages",
                json!([
                    {"chat": "alpha", "text": "first", "wait": true, "timeout_secs": 1},
                    {"chat": "missing", "text": "invalid"},
                    {"chat": "beta", "text": "second", "wait": true, "timeout_secs": 1},
                ]),
            ),
        ] {
            let world = Arc::new(World {
                dispatch_barrier: Some(tokio::sync::Barrier::new(2)),
                ..Default::default()
            });
            let tools = tools(world.clone(), Origin::default());
            let reply = tokio::time::timeout(
                Duration::from_secs(3),
                crate::jsonrpc::handle_request(
                    &tools,
                    json!(42),
                    "tools/call",
                    json!({"name": name, "arguments": {"requests": requests}}),
                ),
            )
            .await
            .expect("both dispatches must proceed while other requests are waiting");
            assert_eq!(reply["result"]["isError"], false, "{reply}");
            let results = &reply["result"]["structuredContent"]["results"];
            for i in [0, 2] {
                assert_eq!(results[i]["index"], i);
                assert_eq!(results[i]["isError"], false, "{reply}");
                assert_eq!(results[i]["result"]["turn"]["outcome"], "timedOut");
            }
            assert_eq!(results[1]["index"], 1);
            assert_eq!(results[1]["isError"], true);
            let writes = world.writes.lock().unwrap();
            let dispatched: Vec<_> = writes
                .iter()
                .filter(|(method, _)| method == methods::QUEUE_COMMAND)
                .collect();
            assert_eq!(dispatched.len(), 2);
            assert_ne!(dispatched[0].1["chatId"], dispatched[1].1["chatId"]);
        }
    }

    #[tokio::test]
    async fn invalid_batch_sizes_have_no_side_effects() {
        let world = Arc::new(World::default());
        let tools = tools(world.clone(), Origin::default());
        for name in ["create_chats", "send_messages"] {
            for requests in [vec![], vec![json!({"prompt": "hello"}); MAX_BATCH + 1]] {
                assert!(
                    tools
                        .call(name, json!({"requests": requests}))
                        .await
                        .is_err()
                );
            }
        }
        assert!(world.writes.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn side_chats_cannot_create_chats_or_be_parents() {
        let world = Arc::new(World {
            beta_parent: Some("chat-alpha-1".into()),
            ..Default::default()
        });
        let side = tools(
            world.clone(),
            Origin {
                chat_id: Some("chat-beta-2".into()),
                device_id: None,
            },
        );
        for args in [json!({}), json!({"parent":"Alpha"}), json!({"kind":"chat"})] {
            assert!(
                side.call("create_chat", args)
                    .await
                    .unwrap_err()
                    .contains("Side chats cannot")
            );
        }
        let batch = side
            .call(
                "create_chats",
                json!({"requests":[{}, {"parent":"Alpha"}, {"kind":"chat"}]}),
            )
            .await
            .unwrap();
        assert!(
            batch["results"]
                .as_array()
                .unwrap()
                .iter()
                .all(|r| r["isError"] == true)
        );
        let root = tools(world.clone(), Origin::default());
        assert!(
            root.call("create_chat", json!({"parent":"Beta"}))
                .await
                .unwrap_err()
                .contains("child of a side chat")
        );
        assert!(world.writes.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn an_orchestrators_sessions_start_sessions_until_the_depth_limit() {
        let root = "orchestrator-root";
        let mut extra_chats = vec![json!({
            "id": root, "deviceId": "dev-local", "title": "Orchestrator",
            "archived": false, "createdAt": "2026-09-03T00:00:00Z",
            "config": { "harness": "claude-code", "model": null, "reasoning": null, "sandbox": "workspace-write" },
        })];
        let mut parent = root.to_owned();
        for level in 1..=zeron_proto::orchestrator::MAX_DEPTH {
            let id = format!("orchestrated-{level}");
            extra_chats.push(json!({
                "id": id, "deviceId": "dev-local", "title": format!("Level {level}"),
                "archived": false, "createdAt": "2026-09-03T00:00:00Z",
                "parentChatId": parent,
                "config": { "harness": "claude-code", "model": null, "reasoning": null, "sandbox": "workspace-write" },
            }));
            parent = id;
        }
        let world = Arc::new(World {
            extra_chats,
            ..Default::default()
        });
        let from = |chat: &str| {
            tools(
                world.clone(),
                Origin {
                    chat_id: Some(chat.into()),
                    device_id: None,
                },
            )
        };
        let created = from("orchestrated-1")
            .call(
                "create_chat",
                json!({"cwd": "/repo/comet/", "title": "Sub"}),
            )
            .await
            .unwrap();
        assert_eq!(created["parentChatId"], "orchestrated-1");
        assert_eq!(created["harness"], "claude-code", "the parent's harness");
        assert_eq!(created["project"]["id"], "space-1", "the cwd's project");
        let deepest = format!("orchestrated-{}", zeron_proto::orchestrator::MAX_DEPTH);
        let refused = from(&deepest)
            .call("create_chat", json!({}))
            .await
            .unwrap_err();
        assert!(
            refused.contains("levels below its orchestrator"),
            "{refused}"
        );
        assert!(same_folder("D:\\OpenTalk\\", "d:/opentalk"));
        assert!(!same_folder("/repo/Comet", "/repo/comet"));
    }

    fn orchestrator_world(paused: bool, sessions: Vec<Value>) -> Arc<World> {
        let config = json!({ "harness": "claude-code", "model": null, "reasoning": null, "sandbox": "workspace-write" });
        let chat = |id: &str, parent: Option<&str>| {
            json!({
                "id": id, "deviceId": "dev-local", "title": id, "archived": false,
                "spaceId": "space-1", "createdAt": "2026-09-03T00:00:00Z",
                "parentChatId": parent, "config": config,
            })
        };
        Arc::new(World {
            extra_chats: vec![
                chat("orchestrator-root", None),
                chat("member-a", Some("orchestrator-root")),
                chat("member-b", Some("orchestrator-root")),
            ],
            sessions,
            paused,
            ..Default::default()
        })
    }

    fn from(world: &Arc<World>, chat: &str) -> Tools {
        tools(
            world.clone(),
            Origin {
                chat_id: Some(chat.into()),
                device_id: None,
            },
        )
    }

    #[tokio::test]
    async fn the_whole_tree_reads_its_project_and_only_the_root_changes_it() {
        let world = orchestrator_world(false, Vec::new());
        let read = from(&world, "member-a")
            .call("project_read", json!({}))
            .await
            .unwrap();
        assert_eq!(read["project"]["goal"], "Ship it");
        assert_eq!(world.reads.lock().unwrap()[0].1["chatId"], "member-a");

        for (tool, args) in [
            ("project_remember", json!({ "text": "CI runs on main" })),
            ("project_forget", json!({ "id": "n-1" })),
            ("project_update", json!({ "goal": "Ship it" })),
        ] {
            let refused = from(&world, "member-a")
                .call(tool, args.clone())
                .await
                .unwrap_err();
            assert!(
                refused.contains("Only the orchestrator"),
                "{tool}: {refused}"
            );
            from(&world, "orchestrator-root")
                .call(tool, args)
                .await
                .unwrap();
        }
        let writes = world.writes.lock().unwrap();
        assert_eq!(writes.len(), 3);
        assert_eq!(writes[0].0, methods::ORCHESTRATOR_MEMORY);
        assert_eq!(
            writes[0].1,
            json!({ "chatId": "orchestrator-root", "text": "CI runs on main" })
        );
        assert_eq!(
            writes[1].1,
            json!({ "chatId": "orchestrator-root", "forget": "n-1" })
        );
        assert_eq!(writes[2].0, methods::UPDATE_ORCHESTRATOR_PROJECT);
        assert_eq!(
            writes[2].1["update"],
            json!({ "goal": "Ship it", "instructions": null })
        );

        let empty = from(&world, "orchestrator-root")
            .call("project_update", json!({}))
            .await
            .unwrap_err();
        assert!(empty.contains("pass goal"), "{empty}");
        assert!(
            tools(world.clone(), Origin::default())
                .call("project_read", json!({}))
                .await
                .unwrap_err()
                .contains("orchestrator's tree")
        );
    }

    #[tokio::test]
    async fn only_the_root_previews_a_members_merge() {
        let world = orchestrator_world(false, Vec::new());
        let calls = |world: &Arc<World>| {
            world
                .reads
                .lock()
                .unwrap()
                .iter()
                .filter(|(method, _)| method == methods::MERGE_PREVIEW)
                .map(|(_, params)| params.clone())
                .collect::<Vec<_>>()
        };
        assert!(
            !tools(world.clone(), Origin::default()).has("merge_preview"),
            "listed only inside a chat"
        );
        assert!(from(&world, "orchestrator-root").has("merge_preview"));

        let refused = from(&world, "member-a")
            .call("merge_preview", json!({ "chat": "member-b" }))
            .await
            .unwrap_err();
        assert!(refused.contains("Only the orchestrator"), "{refused}");
        assert!(calls(&world).is_empty());

        let root = from(&world, "orchestrator-root");
        let unknown = root
            .call("merge_preview", json!({ "chat": "nobody" }))
            .await
            .unwrap_err();
        assert!(unknown.contains("nobody"), "{unknown}");
        assert!(calls(&world).is_empty(), "no RPC for an unknown chat");

        for key in ["member-a", "MEMBER-A"] {
            let preview = root
                .call("merge_preview", json!({ "chat": key }))
                .await
                .unwrap();
            assert_eq!(preview["chatId"], "member-a");
        }
        root.call(
            "merge_preview",
            json!({ "chat": "member-b", "base": "dev" }),
        )
        .await
        .unwrap();
        let calls = calls(&world);
        assert_eq!(
            calls[0],
            json!({
                "chatId": "orchestrator-root", "member": "member-a",
                "base": null, "targetDeviceId": "dev-local",
            })
        );
        assert_eq!(calls[2]["member"], "member-b");
        assert_eq!(calls[2]["base"], "dev");
        assert!(world.writes.lock().unwrap().is_empty(), "read-only");
    }

    #[tokio::test]
    async fn a_paused_orchestrator_starts_and_sends_nothing() {
        let world = orchestrator_world(true, Vec::new());
        for (tool, args) in [
            (
                "create_chat",
                json!({ "project": "space-1", "title": "Sub" }),
            ),
            (
                "send_message",
                json!({ "chat": "member-b", "text": "go on" }),
            ),
        ] {
            for chat in ["orchestrator-root", "member-a"] {
                let refused = from(&world, chat)
                    .call(tool, args.clone())
                    .await
                    .unwrap_err();
                assert!(refused.contains("paused"), "{tool} from {chat}: {refused}");
            }
        }
        assert!(world.writes.lock().unwrap().is_empty());
        // A chat outside any tree is not held back.
        tools(world.clone(), Origin::default())
            .call(
                "create_chat",
                json!({ "project": "space-1", "title": "Solo" }),
            )
            .await
            .unwrap();
    }

    fn policy_world(policy: Value, spent_usd: f64) -> World {
        let world = Arc::try_unwrap(orchestrator_world(false, Vec::new()))
            .ok()
            .unwrap();
        World {
            policy,
            spent_usd,
            ..world
        }
    }

    /// Chats created so far.
    fn mutations(world: &Arc<World>) -> usize {
        world
            .writes
            .lock()
            .unwrap()
            .iter()
            .filter(|(method, params)| method == methods::MUTATE && params["op"] == "createChat")
            .count()
    }

    fn asked(world: &Arc<World>, method: &str) -> Vec<Value> {
        world
            .reads
            .lock()
            .unwrap()
            .iter()
            .filter(|(name, _)| name == method)
            .map(|(_, params)| params.clone())
            .collect()
    }

    #[tokio::test]
    async fn a_reached_spending_cap_starts_and_sends_nothing() {
        let args = [
            (
                "create_chat",
                json!({ "project": "space-1", "title": "Sub" }),
            ),
            (
                "send_message",
                json!({ "chat": "member-b", "text": "go on" }),
            ),
        ];
        let world = Arc::new(policy_world(json!({ "spendCapCents": 500 }), 5.0));
        for (tool, args) in &args {
            for chat in ["orchestrator-root", "member-a"] {
                let refused = from(&world, chat)
                    .call(tool, args.clone())
                    .await
                    .unwrap_err();
                assert!(
                    refused.contains("spending cap of $5.00 is reached"),
                    "{tool} from {chat}: {refused}"
                );
            }
        }
        assert_eq!(mutations(&world), 0);
        let spend = asked(&world, methods::GET_TREE_SPEND);
        assert!(
            spend
                .iter()
                .all(|params| params["chatId"] == "orchestrator-root"),
            "the spend of the whole tree is asked for, by its root: {spend:?}"
        );
        assert!(!spend.is_empty());
        // Under the cap, and without one, work starts.
        let under = Arc::new(policy_world(json!({ "spendCapCents": 500 }), 4.99));
        from(&under, "orchestrator-root")
            .call("create_chat", args[0].1.clone())
            .await
            .unwrap();
        assert_eq!(mutations(&under), 1);
        let uncapped = Arc::new(policy_world(json!({ "githubOnlyTests": true }), 99.0));
        from(&uncapped, "orchestrator-root")
            .call("create_chat", args[0].1.clone())
            .await
            .unwrap();
        assert!(
            asked(&uncapped, methods::GET_TREE_SPEND).is_empty(),
            "no cap, no spend lookup"
        );
    }

    #[tokio::test]
    async fn a_pause_the_cap_set_says_why() {
        let mut world = Arc::try_unwrap(orchestrator_world(true, Vec::new()))
            .ok()
            .unwrap();
        world.pause_reason = "the spending cap of $5.00 is reached ($5.12 spent)";
        let world = Arc::new(world);
        let refused = from(&world, "member-a")
            .call(
                "create_chat",
                json!({ "project": "space-1", "title": "Sub" }),
            )
            .await
            .unwrap_err();
        assert!(
            refused.contains("is paused: the spending cap of $5.00"),
            "{refused}"
        );
        assert!(refused.contains("resumes it"), "{refused}");
    }

    #[tokio::test]
    async fn create_chat_refuses_a_model_the_tree_does_not_allow() {
        let world = Arc::new(policy_world(json!({ "allowedModels": ["opus"] }), 0.0));
        let root = from(&world, "orchestrator-root");
        let refused = root
            .call(
                "create_chat",
                json!({ "project": "space-1", "title": "Sub", "model": "sonnet" }),
            )
            .await
            .unwrap_err();
        assert!(
            refused.contains("not allowed in this orchestrator"),
            "{refused}"
        );
        assert!(refused.contains("opus"), "{refused}");
        let unnamed = root
            .call(
                "create_chat",
                json!({ "project": "space-1", "title": "Sub" }),
            )
            .await
            .unwrap_err();
        assert!(
            unnamed.contains("allows only these models: opus"),
            "{unnamed}"
        );
        assert_eq!(mutations(&world), 0);
        root.call(
            "create_chat",
            json!({ "project": "space-1", "title": "Sub", "model": "opus" }),
        )
        .await
        .unwrap();
        assert_eq!(mutations(&world), 1);
        // Outside a tree nothing is held to a policy.
        tools(world.clone(), Origin::default())
            .call(
                "create_chat",
                json!({ "project": "space-1", "title": "Solo", "model": "sonnet" }),
            )
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn a_writer_waits_for_a_clean_github_preflight_when_the_tree_asks() {
        let mut blocked = policy_world(json!({ "requirePreflight": true }), 0.0);
        blocked.preflight = Some(json!({
            "ok": false, "verdict": "git cannot push to origin", "warnings": [],
        }));
        let world = Arc::new(blocked);
        let root = from(&world, "orchestrator-root");
        let refused = root
            .call(
                "create_chat",
                json!({ "project": "space-1", "title": "Sub" }),
            )
            .await
            .unwrap_err();
        assert!(refused.contains("found blockers"), "{refused}");
        assert!(refused.contains("git cannot push to origin"), "{refused}");
        assert_eq!(mutations(&world), 0);
        let call = asked(&world, methods::GITHUB_PREFLIGHT).remove(0);
        assert_eq!(call["cwd"], "/repo/comet");
        assert_eq!(call["targetDeviceId"], "dev-local");
        // A reader pushes nothing, so it starts regardless.
        root.call(
            "create_chat",
            json!({ "project": "space-1", "title": "Reader", "write": false }),
        )
        .await
        .unwrap();
        // A clean report starts the writer and travels back with it.
        let ready = Arc::new(policy_world(json!({ "requirePreflight": true }), 0.0));
        let created = from(&ready, "orchestrator-root")
            .call(
                "create_chat",
                json!({ "project": "space-1", "title": "Sub" }),
            )
            .await
            .unwrap();
        assert_eq!(created["preflight"]["verdict"], "ready to push");
        // Without the policy no preflight runs.
        let plain = Arc::new(policy_world(Value::Null, 0.0));
        from(&plain, "orchestrator-root")
            .call(
                "create_chat",
                json!({ "project": "space-1", "title": "Sub" }),
            )
            .await
            .unwrap();
        assert!(asked(&plain, methods::GITHUB_PREFLIGHT).is_empty());
    }

    #[tokio::test]
    async fn github_preflight_checks_the_named_project_on_its_device() {
        let world = Arc::new(World::default());
        let report = tools(world.clone(), Origin::default())
            .call(
                "github_preflight",
                json!({ "project": "space-unique", "branch": "zeron/x", "force": true }),
            )
            .await
            .unwrap();
        assert_eq!(report["ok"], true);
        assert_eq!(report["project"]["id"], "space-unique");
        let params = asked(&world, methods::GITHUB_PREFLIGHT).remove(0);
        assert_eq!(params["cwd"], "/repo/unique");
        assert_eq!(params["branch"], "zeron/x");
        assert_eq!(params["force"], true);
        assert_eq!(params["targetDeviceId"], "dev-remote");
        let unnamed = tools(world.clone(), Origin::default())
            .call("github_preflight", json!({}))
            .await
            .unwrap_err();
        assert!(unnamed.contains("name a project"), "{unnamed}");
        // Inside a chat, it defaults to that chat's own project.
        let own = from(&world, "chat-alpha-1")
            .call("github_preflight", json!({}))
            .await
            .unwrap();
        assert_eq!(own["project"]["id"], "space-1");
    }

    #[tokio::test]
    async fn register_project_registers_the_one_folder_the_engine_finds() {
        let world = Arc::new(World::default());
        let outside = tools(world.clone(), Origin::default());
        let found = outside
            .call(
                "register_project",
                json!({ "project": "t3code", "dry_run": true }),
            )
            .await
            .unwrap();
        assert_eq!(found["matches"][0]["path"], "/home/me/codebase/t3code");
        let registered = outside
            .call(
                "register_project",
                json!({ "project": "t3code", "name": "T3" }),
            )
            .await
            .unwrap();
        assert_eq!(registered["project"]["id"], "space-new");
        assert_eq!(registered["project"]["created"], true);
        assert!(
            registered["workspace"]
                .as_str()
                .unwrap()
                .contains("own worktree")
        );
        let writes = world.writes.lock().unwrap();
        assert_eq!(writes[0].1["dryRun"], true);
        assert_eq!(writes[1].1["query"], "t3code");
        assert_eq!(writes[1].1["name"], "T3");
        assert_eq!(writes[1].1["targetDeviceId"], "dev-local");
    }

    #[tokio::test]
    async fn create_chat_registers_a_folder_that_is_not_a_project_yet() {
        let world = Arc::new(World::default());
        tools(world.clone(), Origin::default())
            .call(
                "create_chat",
                json!({ "project": "t3code", "title": "Port it" }),
            )
            .await
            .unwrap();
        let writes = world.writes.lock().unwrap();
        assert!(
            writes
                .iter()
                .any(|(method, params)| method == methods::REGISTER_PROJECT
                    && params["query"] == "t3code")
        );
        assert!(writes.iter().any(|(method, _)| method == methods::MUTATE));
    }

    #[tokio::test]
    async fn project_read_shows_how_much_of_the_cap_is_used() {
        let capped = Arc::new(policy_world(json!({ "spendCapCents": 1000 }), 2.5));
        let read = from(&capped, "member-a")
            .call("project_read", json!({}))
            .await
            .unwrap();
        assert_eq!(read["spend"]["spentUsd"], 2.5);
        let uncapped = Arc::new(policy_world(Value::Null, 2.5));
        let read = from(&uncapped, "member-a")
            .call("project_read", json!({}))
            .await
            .unwrap();
        assert!(read.get("spend").is_none(), "{read}");
    }

    #[tokio::test]
    async fn wait_for_turns_with_nothing_working_says_so() {
        let world = orchestrator_world(false, Vec::new());
        let reply = from(&world, "orchestrator-root")
            .call("wait_for_turns", json!({}))
            .await
            .unwrap();
        assert_eq!(reply["outcome"], "none");
        let refused = tools(world.clone(), Origin::default())
            .call("wait_for_turns", json!({}))
            .await
            .unwrap_err();
        assert!(refused.contains("pass chats"), "{refused}");
    }

    #[tokio::test]
    async fn wait_for_turns_returns_the_first_session_to_stop() {
        let idle = |chat: &str, status: &str| {
            json!({
                "chatId": chat, "deviceId": "dev-local", "status": status,
                "updatedAt": "2026-09-03T00:00:00Z", "runningSubagents": 0,
            })
        };
        let world = orchestrator_world(
            false,
            vec![idle("member-a", "idle"), idle("member-b", "working")],
        );
        let reply = from(&world, "orchestrator-root")
            .call(
                "wait_for_turns",
                json!({ "chats": ["member-a", "member-b"], "timeout_secs": 1 }),
            )
            .await
            .unwrap();
        assert_eq!(reply["chatId"], "member-a", "{reply}");
        assert_eq!(reply["others"][0]["chatId"], "member-b");
        assert!(
            reply["others"][0]["status"]
                .as_str()
                .is_some_and(|status| status.starts_with("working")),
            "{reply}"
        );
    }

    #[tokio::test]
    async fn a_retrying_child_reads_as_retrying_not_as_stalled() {
        let row = |chat: &str, status: &str, updated: String| {
            json!({
                "chatId": chat, "deviceId": "dev-local", "status": status,
                "updatedAt": updated, "runningSubagents": 0,
                "retry": {
                    "attempt": 2, "maxAttempts": 10,
                    "nextAttemptAt": (chrono::Utc::now() + chrono::Duration::seconds(8)),
                    "error": "429 rate limited",
                },
            })
        };
        let fresh = chrono::Utc::now().to_rfc3339();
        let world = orchestrator_world(
            false,
            vec![
                row("member-a", "idle", fresh.clone()),
                row("member-b", "working", fresh.clone()),
            ],
        );
        let root = from(&world, "orchestrator-root");
        // The row's own status is the source of truth: idle never retries.
        let listed = root
            .call("list_chats", json!({ "parent": "orchestrator-root" }))
            .await
            .unwrap();
        let chats = listed["chats"].as_array().unwrap();
        let by = |id: &str| chats.iter().find(|c| c["id"] == id).unwrap();
        assert_eq!(by("member-a")["status"], "idle");
        assert!(by("member-a")["retry"].is_null());
        let status = by("member-b")["status"].as_str().unwrap();
        assert!(status.starts_with("retrying (attempt 2/10"), "{status}");
        assert_eq!(by("member-b")["retry"]["attempt"], 2);
        assert_eq!(by("member-b")["retry"]["error"], "429 rate limited");

        let read = root
            .call("read_chat", json!({ "chat": "member-b" }))
            .await
            .unwrap();
        assert!(
            read["status"]
                .as_str()
                .is_some_and(|s| s.starts_with("retrying")),
            "{read}"
        );

        // Still working for target selection: the default watch includes it,
        // and the wait keeps going until it times out.
        let waited = root
            .call("wait_for_turns", json!({ "timeout_secs": 1 }))
            .await
            .unwrap();
        assert_eq!(waited["turn"]["outcome"], "timedOut", "{waited}");
        // A timed-out single wait says the provider is failing, not stalled.
        let single = root
            .call(
                "wait_for_turn",
                json!({ "chat": "member-b", "timeout_secs": 1 }),
            )
            .await
            .unwrap();
        let turn = &single["turn"];
        assert_eq!(turn["outcome"], "timedOut", "{single}");
        assert!(turn["status"].as_str().unwrap().starts_with("retrying"));
        assert_eq!(turn["retry"]["attempt"], 2);
        assert!(turn["note"].as_str().unwrap().contains("Not stalled"));

        // Another child listed as `others` carries the same wording.
        let others = root
            .call(
                "wait_for_turns",
                json!({ "chats": ["member-a", "member-b"], "timeout_secs": 1 }),
            )
            .await
            .unwrap();
        assert_eq!(others["chatId"], "member-a", "{others}");
        assert!(
            others["others"][0]["status"]
                .as_str()
                .is_some_and(|s| s.starts_with("retrying")),
            "{others}"
        );

        // A retry on a row nobody refreshed is a dead host, not a retry.
        let stale = (chrono::Utc::now() - chrono::Duration::minutes(5)).to_rfc3339();
        let world = orchestrator_world(false, vec![row("member-b", "working", stale)]);
        let listed = from(&world, "orchestrator-root")
            .call("list_chats", json!({ "parent": "orchestrator-root" }))
            .await
            .unwrap();
        let chats = listed["chats"].as_array().unwrap();
        let b = chats.iter().find(|c| c["id"] == "member-b").unwrap();
        assert!(
            b["status"].as_str().unwrap().starts_with("working (stale"),
            "{b}"
        );
        assert!(b["retry"].is_null());
    }

    #[tokio::test]
    async fn kinds_preserve_defaults_and_reject_invalid_arguments_before_writing() {
        let world = Arc::new(World::default());
        let root = tools(world.clone(), Origin::default());
        for args in [
            json!({"kind":"side"}),
            json!({"kind":"unknown"}),
            json!({"kind":"chat", "parent":"Alpha", "title":"must not write"}),
        ] {
            assert!(root.call("create_chat", args).await.is_err());
        }
        assert!(world.writes.lock().unwrap().is_empty());
        let from_chat = tools(
            world.clone(),
            Origin {
                chat_id: Some("chat-alpha-1".into()),
                device_id: None,
            },
        );
        for (args, kind, parent) in [
            (json!({"kind":"chat"}), "chat", Value::Null),
            (json!({"kind":"chat", "parent":"  "}), "chat", Value::Null),
            (json!({"kind":"side"}), "side", json!("chat-alpha-1")),
            (json!({}), "side", json!("chat-alpha-1")),
            (json!({"parent":"Beta"}), "side", json!("chat-beta-2")),
        ] {
            let created = from_chat.call("create_chat", args).await.unwrap();
            assert_eq!(created["kind"], kind);
            assert_eq!(created["parentChatId"], parent);
            let writes = world.writes.lock().unwrap();
            let row = &writes.last().unwrap().1;
            if kind == "chat" {
                assert!(row.get("parentChatId").is_none());
            } else {
                assert_eq!(row["parentChatId"], parent);
            }
        }
        assert_eq!(
            root.call("create_chat", json!({})).await.unwrap()["kind"],
            "chat"
        );
        assert_eq!(
            root.call("create_chat", json!({"kind":"side", "parent":"Alpha"}))
                .await
                .unwrap()["kind"],
            "side"
        );
    }

    #[tokio::test]
    async fn project_and_device_resolution_scopes_names_and_exact_paths() {
        let world = Arc::new(World::default());
        let tools = tools(world.clone(), Origin::default());
        for args in [
            json!({"project":"comet"}),
            json!({"project":"/repo/comet"}),
            json!({"project":"space-1", "device":"Worker"}),
            json!({"project":"missing", "device":"Worker"}),
            json!({"device":"missing"}),
        ] {
            let error = tools.call("create_chat", args).await.unwrap_err();
            assert!(world.writes.lock().unwrap().is_empty(), "{error}");
        }
        let error = tools
            .call("create_chat", json!({"project":"/repo/comet"}))
            .await
            .unwrap_err();
        for candidate in ["space-1", "space-remote", "dev-local", "dev-remote"] {
            assert!(error.contains(candidate), "{error}");
        }
        for project in ["comet", "/repo/comet", "space-remote"] {
            let created = tools
                .call("create_chat", json!({"project":project,"device":"Worker"}))
                .await
                .unwrap();
            assert_eq!(created["deviceId"], "dev-remote");
            assert_eq!(created["project"]["id"], "space-remote");
            assert_eq!(created["harness"], "codex");
        }
        let created = tools
            .call("create_chat", json!({"project":"unique"}))
            .await
            .unwrap();
        assert_eq!(created["deviceId"], "dev-remote");
        let created = tools
            .call("create_chat", json!({"device":"dev-remote"}))
            .await
            .unwrap();
        assert!(created["project"].is_null());
        assert_eq!(created["deviceId"], "dev-remote");
        let created = tools.call("create_chat", json!({})).await.unwrap();
        assert_eq!(created["deviceId"], "dev-local");
    }

    #[tokio::test]
    async fn discovery_and_creation_use_the_selected_hosts_catalogs() {
        let world = Arc::new(World::default());
        let tools = tools(world.clone(), Origin::default());
        assert_eq!(
            tools.call("list_projects", json!({})).await.unwrap()["projects"]
                .as_array()
                .unwrap()
                .len(),
            3
        );
        let projects = tools
            .call("list_projects", json!({"device":"Worker"}))
            .await
            .unwrap();
        assert_eq!(projects["projects"].as_array().unwrap().len(), 2);
        assert!(
            projects["projects"]
                .as_array()
                .unwrap()
                .iter()
                .all(|p| p["deviceId"] == "dev-remote")
        );
        assert_eq!(
            tools.call("list_harnesses", json!({})).await.unwrap()["harnesses"][0]["id"],
            "claude-code"
        );
        assert_eq!(
            tools
                .call("list_harnesses", json!({"device":"Worker"}))
                .await
                .unwrap()["harnesses"][0]["id"],
            "codex"
        );
        assert_eq!(
            tools
                .call("list_models", json!({"harness":"codex", "device":"Worker"}))
                .await
                .unwrap()["models"][0]["id"],
            "remote-model"
        );
        assert_eq!(
            tools
                .call("list_models", json!({"harness":"claude-code"}))
                .await
                .unwrap()["models"][0]["id"],
            "opus"
        );
        for args in [
            json!({"device":"Worker", "harness":"claude-code"}),
            json!({"device":"Worker", "harness":"cursor"}),
            json!({"device":"Laptop", "harness":"codex"}),
            json!({"device":"Laptop", "harness":"mock"}),
            json!({"device":"Worker", "harness":"codex", "model":"opus"}),
        ] {
            assert!(tools.call("create_chat", args).await.is_err());
            assert!(world.writes.lock().unwrap().is_empty());
        }
        let created = tools
            .call(
                "create_chat",
                json!({"device":"Worker", "harness":"codex", "model":"remote-model"}),
            )
            .await
            .unwrap();
        assert_eq!(created["harness"], "codex");
        assert_eq!(created["model"], "remote-model");
        for method in [methods::LIST_HARNESSES, methods::LIST_MODELS] {
            let broken = Arc::new(World {
                catalog_error: Some(method),
                ..Default::default()
            });
            let tools = self::tools(broken.clone(), Origin::default());
            let error = tools
                .call(
                    "create_chat",
                    json!({"device":"Worker", "model":"remote-model"}),
                )
                .await
                .unwrap_err();
            assert!(
                error.contains("dev-remote") && error.contains("catalog unavailable"),
                "{error}"
            );
            assert!(broken.writes.lock().unwrap().is_empty());
        }
    }

    #[tokio::test]
    async fn remote_send_selects_the_remote_default_harness() {
        let world = Arc::new(World {
            beta_remote: true,
            ..Default::default()
        });
        let tools = tools(world.clone(), Origin::default());
        tools
            .call("send_message", json!({"chat":"Beta", "text":"go"}))
            .await
            .unwrap();
        assert_eq!(
            world.reads.lock().unwrap().last().unwrap().1["targetDeviceId"],
            "dev-remote"
        );
        assert_eq!(
            world.writes.lock().unwrap().last().unwrap().1["command"]["request"]["harness"],
            "codex"
        );
    }

    #[tokio::test]
    async fn mixed_batch_preserves_order_and_errors_per_request() {
        let world = Arc::new(World::default());
        let tools = tools(
            world.clone(),
            Origin {
                chat_id: Some("chat-alpha-1".into()),
                device_id: None,
            },
        );
        let result = tools
            .call(
                "create_chats",
                json!({"requests":[
                    {"kind":"chat", "device":"Worker", "project":"comet"},
                    {"kind":"chat", "parent":"Alpha"},
                    {"kind":"side", "project":"space-1"},
                    {"kind":"bogus"}
                ]}),
            )
            .await
            .unwrap();
        let results = &result["results"];
        assert_eq!(results[0]["result"]["kind"], "chat");
        assert_eq!(results[0]["result"]["deviceId"], "dev-remote");
        assert_eq!(results[1]["isError"], true);
        assert_eq!(results[2]["result"]["kind"], "side");
        assert_eq!(results[2]["result"]["parentChatId"], "chat-alpha-1");
        assert_eq!(results[3]["isError"], true);
        for i in 0..4 {
            assert_eq!(results[i]["index"], i);
        }
        assert_eq!(world.writes.lock().unwrap().len(), 2);
    }

    /// Sessions and transcripts are independently delivered. The command reply
    /// is deliberately slow, so the assistant starts before QueueCommand returns.
    struct DelayedTurn {
        world: World,
        sent: Mutex<Option<(i64, Instant, String)>>,
        reply_delay: Duration,
        never_reply: bool,
        /// Host clock offset: a remote engine stamps createdAt on its own clock.
        host_skew_millis: i64,
    }

    #[async_trait]
    impl RpcService for DelayedTurn {
        async fn handle(&self, method: &str, params: Value) -> Result<RpcReply, RpcError> {
            match method {
                methods::WATCH_CHATS => {
                    let reply = self.world.handle(method, params).await?;
                    let RpcReply::Stream(mut rows) = reply else {
                        unreachable!()
                    };
                    let mut rows = rows.next().await.unwrap();
                    if let Some((_, _, chat)) = self.sent.lock().unwrap().as_ref()
                        && chat != "chat-beta-2"
                    {
                        rows.as_array_mut().unwrap().push(json!({
                            "id": chat, "deviceId":"dev-remote", "archived":false,
                            "createdAt":chrono::Utc::now(), "config":{"harness":"codex", "sandbox":"workspace-write"}
                        }));
                    }
                    Ok(stream(rows))
                }
                methods::QUEUE_COMMAND => {
                    let reply = self.world.handle(method, params.clone()).await?;
                    *self.sent.lock().unwrap() = Some((
                        now_millis(),
                        Instant::now(),
                        params["chatId"].as_str().unwrap().into(),
                    ));
                    tokio::time::sleep(Duration::from_millis(150)).await;
                    Ok(reply)
                }
                methods::WATCH_SESSIONS => {
                    let sent = self.sent.lock().unwrap().clone();
                    let Some((_, _, chat)) = sent else {
                        return Ok(stream(json!([])));
                    };
                    Ok(RpcReply::Stream(
                        futures::stream::once(async { json!([]) })
                            .chain(futures::stream::once(async move {
                                tokio::time::sleep(Duration::from_millis(50)).await;
                                json!([{"chatId":chat, "deviceId":"dev-remote", "status":"idle",
                                "updatedAt":chrono::Utc::now(), "lastCompletedTurn":"new-turn"}])
                            }))
                            .boxed(),
                    ))
                }
                methods::WATCH_DOC_MESSAGES => {
                    let sent = self.sent.lock().unwrap().clone();
                    // Only the existing chat has history. Its previous reply
                    // shares the send's timestamp, so only its id tells them apart.
                    let mut messages = json!([]);
                    if params["chatId"] == "chat-beta-2" {
                        let created_at = sent.as_ref().map_or_else(now_millis, |(t, _, _)| *t);
                        messages.as_array_mut().unwrap().push(json!({
                            "id":"previous-reply", "role":"assistant", "createdAt":created_at,
                            "deviceId":"dev-remote", "status":"complete", "parts":[{"kind":"text", "id":"t", "text":"old reply"}]
                        }));
                    }
                    if let Some((timestamp, started, _)) = sent
                        && started.elapsed() >= self.reply_delay
                        && !self.never_reply
                    {
                        messages.as_array_mut().unwrap().push(json!({
                            "id":"new-reply", "role":"assistant", "createdAt":timestamp + self.host_skew_millis,
                            "deviceId":"dev-remote", "status":"complete", "parts":[{"kind":"text", "id":"t", "text":"new reply"}]
                        }));
                    }
                    Ok(stream(json!({"reset":messages})))
                }
                _ => self.world.handle(method, params).await,
            }
        }
    }

    #[tokio::test]
    async fn wait_allows_a_missing_session_and_a_late_remote_transcript() {
        for create in [true, false] {
            let service = Arc::new(DelayedTurn {
                world: World {
                    beta_remote: true,
                    ..Default::default()
                },
                sent: Mutex::new(None),
                reply_delay: Duration::from_millis(450),
                never_reply: false,
                host_skew_millis: 0,
            });
            let tools = Tools::new(Arc::new(Zeron::with_client(
                memory_client(service),
                Origin::default(),
            )));
            let (name, args) = if create {
                (
                    "create_chat",
                    json!({"kind":"chat", "device":"Worker", "prompt":"go", "wait":true, "timeout_secs":2}),
                )
            } else {
                (
                    "send_message",
                    json!({"chat":"Beta", "text":"go", "wait":true, "timeout_secs":2}),
                )
            };
            let result = tools.call(name, args).await.unwrap();
            assert_eq!(result["turn"]["outcome"], "completed", "{result}");
            assert_eq!(
                result["turn"]["replies"].as_array().unwrap().len(),
                1,
                "{result}"
            );
            assert_eq!(result["turn"]["replies"][0]["text"], "new reply");
        }
    }

    #[tokio::test]
    async fn standalone_wait_after_nonblocking_send_keeps_the_send_baseline() {
        for create in [true, false] {
            let service = Arc::new(DelayedTurn {
                world: World {
                    beta_remote: true,
                    ..Default::default()
                },
                sent: Mutex::new(None),
                reply_delay: Duration::from_millis(450),
                never_reply: false,
                host_skew_millis: 0,
            });
            let tools = Tools::new(Arc::new(Zeron::with_client(
                memory_client(service),
                Origin::default(),
            )));
            let sent = if create {
                tools
                    .call(
                        "create_chat",
                        json!({"kind":"chat", "device":"Worker", "prompt":"go", "wait":false}),
                    )
                    .await
                    .unwrap()
            } else {
                tools
                    .call(
                        "send_message",
                        json!({"chat":"Beta", "text":"go", "wait":false}),
                    )
                    .await
                    .unwrap()
            };
            let result = tools
                .call(
                    "wait_for_turn",
                    json!({"chat":sent["chatId"], "timeout_secs":2}),
                )
                .await
                .unwrap();
            assert_eq!(result["turn"]["outcome"], "completed", "{result}");
            assert_eq!(result["turn"]["replies"].as_array().unwrap().len(), 1);
            assert_eq!(result["turn"]["replies"][0]["text"], "new reply");
        }
    }

    /// A remote host stamps replies on its own clock. One running behind the
    /// caller must not hide the reply to this send.
    #[tokio::test]
    async fn wait_returns_the_reply_from_a_host_whose_clock_is_behind() {
        for create in [true, false] {
            let service = Arc::new(DelayedTurn {
                world: World {
                    beta_remote: true,
                    ..Default::default()
                },
                sent: Mutex::new(None),
                reply_delay: Duration::from_millis(100),
                never_reply: false,
                host_skew_millis: -30_000,
            });
            let tools = Tools::new(Arc::new(Zeron::with_client(
                memory_client(service),
                Origin::default(),
            )));
            let (name, args) = if create {
                (
                    "create_chat",
                    json!({"kind":"chat", "device":"Worker", "prompt":"go", "wait":true, "timeout_secs":2}),
                )
            } else {
                (
                    "send_message",
                    json!({"chat":"Beta", "text":"go", "wait":true, "timeout_secs":2}),
                )
            };
            let result = tools.call(name, args).await.unwrap();
            assert_eq!(result["turn"]["outcome"], "completed", "{result}");
            let replies = result["turn"]["replies"].as_array().unwrap();
            assert!(replies.iter().any(|r| r["text"] == "new reply"), "{result}");
        }
    }

    #[tokio::test]
    async fn completed_session_without_new_transcript_times_out_without_old_reply() {
        let service = Arc::new(DelayedTurn {
            world: World {
                beta_remote: true,
                ..Default::default()
            },
            sent: Mutex::new(None),
            reply_delay: Duration::ZERO,
            never_reply: true,
            host_skew_millis: 0,
        });
        let tools = Tools::new(Arc::new(Zeron::with_client(
            memory_client(service),
            Origin::default(),
        )));
        let result = tools
            .call(
                "send_message",
                json!({"chat":"Beta", "text":"go", "wait":true, "timeout_secs":1}),
            )
            .await
            .unwrap();
        assert_eq!(result["turn"]["outcome"], "timedOut");
        assert_eq!(result["turn"]["replies"], json!([]));
    }

    #[tokio::test]
    async fn remote_replies_do_not_require_synchronized_clocks() {
        for host_skew_millis in [-1_000, 1_000] {
            for wait in [true, false] {
                let service = Arc::new(DelayedTurn {
                    world: World {
                        beta_remote: true,
                        ..Default::default()
                    },
                    sent: Mutex::new(None),
                    reply_delay: Duration::from_millis(450),
                    never_reply: false,
                    host_skew_millis,
                });
                let tools = Tools::new(Arc::new(Zeron::with_client(
                    memory_client(service),
                    Origin::default(),
                )));
                let sent = tools
                    .call(
                        "send_message",
                        json!({
                            "chat":"Beta", "text":"go", "wait":wait, "timeout_secs":2
                        }),
                    )
                    .await
                    .unwrap();
                let result = if wait {
                    sent
                } else {
                    tools
                        .call(
                            "wait_for_turn",
                            json!({
                                "chat":sent["chatId"], "timeout_secs":2
                            }),
                        )
                        .await
                        .unwrap()
                };
                assert_eq!(
                    result["turn"]["outcome"], "completed",
                    "offset={host_skew_millis}, wait={wait}: {result}"
                );
                assert_eq!(
                    result["turn"]["replies"].as_array().unwrap().len(),
                    1,
                    "{result}"
                );
                assert_eq!(result["turn"]["replies"][0]["id"], "new-reply");
            }
        }
    }

    #[tokio::test]
    async fn initialize_and_list_over_jsonrpc() {
        let world = Arc::new(World::default());
        let tools = tools(world, Origin::default());
        let init = crate::jsonrpc::handle_request(
            &tools,
            json!(1),
            "initialize",
            json!({ "protocolVersion": "2025-03-26" }),
        )
        .await;
        assert_eq!(init["result"]["protocolVersion"], "2025-03-26");
        assert_eq!(init["result"]["serverInfo"]["name"], "zeron");
        let list =
            crate::jsonrpc::handle_request(&tools, json!(2), "tools/list", Value::Null).await;
        assert!(list["result"]["tools"].as_array().unwrap().len() >= 10);
        let bad = crate::jsonrpc::handle_request(
            &tools,
            json!(3),
            "tools/call",
            json!({ "name": "nope" }),
        )
        .await;
        assert_eq!(bad["error"]["code"], -32602);
        let whoami = crate::jsonrpc::handle_request(
            &tools,
            json!(4),
            "tools/call",
            json!({ "name": "whoami" }),
        )
        .await;
        assert_eq!(whoami["result"]["isError"], false);
        assert_eq!(
            whoami["result"]["structuredContent"]["localDeviceId"],
            "dev-local"
        );
    }

    // ---- strict, bounded control plane -------------------------------------

    fn no_writes(world: &World) {
        assert!(world.writes.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn unknown_arguments_are_rejected_naming_the_field() {
        let world = Arc::new(World::default());
        let tools = tools(world.clone(), Origin::default());
        for (tool, args, field) in [
            ("create_chat", json!({ "files": ["a"] }), "files"),
            (
                "send_message",
                json!({ "chat": "chat-alpha-1", "text": "x", "scope": "all" }),
                "scope",
            ),
            ("list_chats", json!({ "scope": 1 }), "scope"),
            (
                "read_chat",
                json!({ "chat": "chat-alpha-1", "tail": 3 }),
                "tail",
            ),
            (
                "wait_for_turn",
                json!({ "chat": "chat-alpha-1", "timeout": 3 }),
                "timeout",
            ),
            (
                "archive_chat",
                json!({ "chat": "chat-alpha-1", "hide": true }),
                "hide",
            ),
            (
                "board_post",
                json!({ "kind": "finding", "text": "x", "tag": "y" }),
                "tag",
            ),
            (
                "project_update",
                json!({ "goal": "x", "notes": "y" }),
                "notes",
            ),
            (
                "respond_to_input",
                json!({
                    "chat": "chat-alpha-1",
                    "answers": [{ "question_id": "q", "labels": [], "free_text": "z" }]
                }),
                "free_text",
            ),
            ("whoami", json!({ "x": 1 }), "x"),
            ("list_devices", json!({ "x": 1 }), "x"),
            ("project_read", json!({ "x": 1 }), "x"),
        ] {
            let err = tools.call(tool, args).await.unwrap_err();
            assert!(err.contains("invalid arguments"), "{tool}: {err}");
            assert!(err.contains("unknown field"), "{tool}: {err}");
            assert!(err.contains(&format!("`{field}`")), "{tool}: {err}");
        }
        no_writes(&world);
        // A null or empty argument object is still fine for zero-arg tools.
        assert!(tools.call("whoami", Value::Null).await.is_ok());
        assert!(tools.call("whoami", json!({})).await.is_ok());
    }

    #[tokio::test]
    async fn unknown_fields_in_batch_items_error_per_item() {
        let world = Arc::new(World::default());
        let tools = tools(world.clone(), Origin::default());
        let created = tools
            .call(
                "create_chats",
                json!({ "requests": [
                    { "project": "space-1" },
                    { "prompt": "x", "files": [] },
                ] }),
            )
            .await
            .unwrap();
        assert_eq!(created["results"][0]["isError"], false);
        assert_eq!(created["results"][1]["isError"], true);
        let error = created["results"][1]["error"].as_str().unwrap();
        assert!(error.contains("requests[1]"), "{error}");
        assert!(error.contains("unknown field `files`"), "{error}");

        let sent = tools
            .call(
                "send_messages",
                json!({ "requests": [
                    { "chat": "chat-alpha-1", "text": "hi", "urgent": true },
                ] }),
            )
            .await
            .unwrap();
        assert_eq!(sent["results"][0]["isError"], true);
        let error = sent["results"][0]["error"].as_str().unwrap();
        assert!(error.contains("unknown field `urgent`"), "{error}");

        let before = world.writes.lock().unwrap().len();
        let err = tools
            .call(
                "create_chats",
                json!({ "requests": [{ "project": "space-1" }], "extra": 1 }),
            )
            .await
            .unwrap_err();
        assert!(err.contains("unknown field `extra`"), "{err}");
        assert_eq!(world.writes.lock().unwrap().len(), before);
    }

    #[test]
    fn every_schema_forbids_extra_properties() {
        fn walk(path: &str, schema: &Value) {
            let Some(object) = schema.as_object() else {
                return;
            };
            if object.contains_key("properties") {
                assert_eq!(
                    object.get("additionalProperties"),
                    Some(&json!(false)),
                    "{path} must forbid extra properties"
                );
            }
            for (key, child) in object {
                if key == "properties" {
                    for (name, property) in child.as_object().unwrap() {
                        walk(&format!("{path}.{name}"), property);
                    }
                } else if key == "items" {
                    walk(&format!("{path}[]"), child);
                }
            }
        }
        let tools = catalog();
        for tool in &tools {
            assert_eq!(tool.input_schema["type"], "object", "{}", tool.name);
            walk(tool.name, &tool.input_schema);
        }
        let find = |name: &str| tools.iter().find(|t| t.name == name).unwrap();
        let item = &find("create_chats").input_schema["properties"]["requests"]["items"];
        assert_eq!(item["additionalProperties"], false);
        let answers = &find("respond_to_input").input_schema["properties"]["answers"]["items"];
        assert_eq!(answers["additionalProperties"], false);
        assert_eq!(
            find(SHOW_VISUAL).input_schema["properties"]["assets"]["additionalProperties"],
            json!({ "type": "string" })
        );
        assert_eq!(
            find("create_chat").input_schema["properties"]["prompt"]["maxLength"],
            MAX_PROMPT_CHARS
        );
        assert_eq!(
            find("send_message").input_schema["properties"]["text"]["maxLength"],
            MAX_PROMPT_CHARS
        );
    }

    #[tokio::test]
    async fn prompt_size_is_capped_before_anything_is_written() {
        let world = Arc::new(World::default());
        let tools = tools(world.clone(), Origin::default());
        let too_long = "x".repeat(MAX_PROMPT_CHARS + 1);
        let err = tools
            .call(
                "create_chat",
                json!({ "project": "space-1", "prompt": too_long }),
            )
            .await
            .unwrap_err();
        assert!(err.contains("the limit is 100000"), "{err}");
        let err = tools
            .call(
                "send_message",
                json!({ "chat": "chat-alpha-1", "text": too_long }),
            )
            .await
            .unwrap_err();
        assert!(err.contains("the limit is 100000"), "{err}");
        let batch = tools
            .call(
                "create_chats",
                json!({ "requests": [{ "project": "space-1", "prompt": too_long }] }),
            )
            .await
            .unwrap();
        assert_eq!(batch["results"][0]["isError"], true);
        assert!(
            batch["results"][0]["error"]
                .as_str()
                .unwrap()
                .contains("the limit is 100000")
        );
        no_writes(&world);

        // The characters count, not the bytes.
        let exact = "é".repeat(MAX_PROMPT_CHARS);
        tools
            .call(
                "create_chat",
                json!({ "project": "space-1", "prompt": exact }),
            )
            .await
            .unwrap();
    }

    fn capped_world(live: usize, archived: usize) -> Arc<World> {
        let config = json!({ "harness": "claude-code", "model": null, "reasoning": null, "sandbox": "workspace-write" });
        let chat = |id: &str, parent: &str, archived: bool| {
            json!({
                "id": id, "deviceId": "dev-local", "title": id, "archived": archived,
                "spaceId": "space-1", "createdAt": "2026-09-03T00:00:00Z",
                "parentChatId": parent, "config": config,
            })
        };
        let mut extra_chats = vec![json!({
            "id": "orchestrator-root", "deviceId": "dev-local", "title": "Orchestrator",
            "archived": false, "createdAt": "2026-09-03T00:00:00Z", "config": config,
        })];
        for n in 0..live + archived {
            // The last live one is nested, to count every level.
            let parent = if n + 1 == live && live > 1 {
                "live-0"
            } else {
                "orchestrator-root"
            };
            let id = if n < live {
                format!("live-{n}")
            } else {
                format!("done-{n}")
            };
            extra_chats.push(chat(&id, parent, n >= live));
        }
        Arc::new(World {
            extra_chats,
            ..Default::default()
        })
    }

    #[tokio::test]
    async fn a_tree_cannot_hold_more_than_the_live_session_cap() {
        use zeron_proto::orchestrator::MAX_LIVE_SESSIONS as MAX;
        let start = |world: &Arc<World>| {
            let tools = from(world, "orchestrator-root");
            async move {
                tools
                    .call("create_chat", json!({ "project": "space-1" }))
                    .await
            }
        };

        let full = capped_world(MAX, 0);
        let err = start(&full).await.unwrap_err();
        assert!(err.contains("live sessions"), "{err}");
        assert!(err.contains("archive"), "{err}");
        assert!(err.contains("Wait for a session to finish"), "{err}");
        no_writes(&full);
        // A session deep in the tree is held to the same cap.
        let err = from(&full, "live-0")
            .call("create_chat", json!({ "project": "space-1" }))
            .await
            .unwrap_err();
        assert!(err.contains("live sessions"), "{err}");

        // Archived sessions free their slot.
        assert!(start(&capped_world(MAX, 3)).await.is_err());
        assert!(start(&capped_world(MAX - 1, 3)).await.is_ok());

        // A chat outside any tree is not capped by a full tree.
        let standalone = tools(full.clone(), Origin::default())
            .call("create_chat", json!({ "project": "space-1" }))
            .await;
        assert!(standalone.is_ok(), "{standalone:?}");
        let created = from(&full, "orchestrator-root")
            .call(
                "create_chat",
                json!({ "kind": "chat", "project": "space-1" }),
            )
            .await;
        assert!(created.is_ok(), "{created:?}");
    }

    #[tokio::test]
    async fn a_batch_cannot_overshoot_the_live_session_cap() {
        use zeron_proto::orchestrator::MAX_LIVE_SESSIONS as MAX;
        let world = capped_world(MAX - 2, 0);
        let root = from(&world, "orchestrator-root");
        let batch = root
            .call(
                "create_chats",
                json!({ "requests": [
                    { "project": "space-1" }, { "project": "space-1" },
                    { "project": "space-1" }, { "project": "space-1" },
                ] }),
            )
            .await
            .unwrap();
        let results = batch["results"].as_array().unwrap();
        let ok = results.iter().filter(|r| r["isError"] == false).count();
        assert_eq!(ok, 2, "{batch}");
        for refused in results.iter().filter(|r| r["isError"] == true) {
            let error = refused["error"].as_str().unwrap();
            assert!(error.contains("live sessions"), "{error}");
        }
        // The snapshot has not folded the two new rows in, but they count.
        let err = root
            .call("create_chat", json!({ "project": "space-1" }))
            .await
            .unwrap_err();
        assert!(err.contains("live sessions"), "{err}");
    }
}
