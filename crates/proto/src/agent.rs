//! Agent-side wire types: harness identity, run requests, streaming events, tool calls.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum HarnessId {
    ClaudeCode,
    Codex,
    Cursor,
    /// Cognition's Devin agent, driven over ACP (`devin acp`).
    Devin,
    /// xAI's Grok Build agent, driven over ACP (`grok agent stdio`).
    Grok,
    /// Nous Research's Hermes Agent, driven over ACP (`hermes acp`).
    Hermes,
    /// The Pi coding agent (pi.dev), driven over its native JSONL RPC protocol.
    Pi,
    /// SST's opencode agent, driven natively over its own HTTP/SSE server
    /// protocol (`opencode serve` — the same wire the opencode desktop app
    /// speaks).
    Opencode,
    /// google's antigravity agent over acp (`agy_acp_server`, installed from
    /// its pinned release archive).
    Antigravity,
    /// Test harness; never shown in production pickers.
    Mock,
}

/// Durable user preference for one agent's independently-installed CLI.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum HarnessUpdatePolicy {
    /// Check and surface an update, but never mutate the installation without
    /// an explicit action.
    #[default]
    Notify,
    /// Apply a discovered update after the harness execution gate becomes idle.
    AutoWhenIdle,
    /// Do not probe or update this harness.
    Off,
}

/// Best-effort classification of the installation that owns an agent CLI.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum HarnessInstallSource {
    Npm,
    Homebrew,
    Cargo,
    Vendor,
    ManagedByZeron,
    #[default]
    Unknown,
}

/// The externally visible harness-update state machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum HarnessUpdatePhase {
    Dormant,
    Checking,
    Current,
    Available,
    WaitingForIdle,
    Preparing,
    Downloading,
    Installing,
    Verifying,
    Updated,
    ManualActionRequired,
    Failed,
}

/// Real provider progress only. An absent value means the UI must render an
/// indeterminate activity indicator rather than inventing a percentage.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HarnessUpdateProgress {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completed_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HarnessUpdateFailure {
    pub message: String,
    #[serde(default)]
    pub retryable: bool,
}

/// One row in the device-local harness-update stream.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HarnessUpdateStatus {
    pub harness: HarnessId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub installed_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latest_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel: Option<String>,
    #[serde(default)]
    pub source: HarnessInstallSource,
    #[serde(default)]
    pub policy: HarnessUpdatePolicy,
    pub phase: HarnessUpdatePhase,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub progress: Option<HarnessUpdateProgress>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checked_at: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<HarnessUpdateFailure>,
    /// Whether this device can safely apply the update without guessing which
    /// package manager owns the installation. Older engines omit this field,
    /// so clients must default to the conservative read-only behavior.
    #[serde(default)]
    pub can_apply: bool,
    /// Provider command shown when mutation cannot be safely automated.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub manual_command: Option<String>,
}

impl HarnessUpdateStatus {
    /// Home notices describe discovered releases or an update in progress.
    /// Manual checks remain available in Settings without implying that a
    /// newer release exists.
    pub fn show_update_notice(&self) -> bool {
        matches!(
            self.phase,
            HarnessUpdatePhase::Available
                | HarnessUpdatePhase::WaitingForIdle
                | HarnessUpdatePhase::Preparing
                | HarnessUpdatePhase::Downloading
                | HarnessUpdatePhase::Installing
                | HarnessUpdatePhase::Verifying
                | HarnessUpdatePhase::Updated
                | HarnessUpdatePhase::Failed
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ReasoningLevel {
    Minimal,
    Low,
    Medium,
    High,
    XHigh,
    Max,
    Ultra,
    /// xhigh + harness-specific setting.
    Ultracode,
    /// Prompt-prefix driven (Claude).
    Ultrathink,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SandboxLevel {
    ReadOnly,
    WorkspaceWrite,
    DangerFullAccess,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SteeringMode {
    /// Steer delivered at the next step boundary within the live turn.
    StepBoundary,
    /// Steer delivered only between turns.
    TurnBoundary,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Model {
    pub id: String,
    pub label: String,
    /// Short tagline rendered under the name in the model picker (11px muted),
    /// mirroring the Electron app's `ModelInfo.description`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default)]
    pub reasoning_levels: Vec<ReasoningLevel>,
    #[serde(default)]
    pub options: Vec<ModelOption>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelOption {
    pub id: String,
    pub label: String,
    pub choices: Vec<ModelOptionChoice>,
    pub default_choice: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelOptionChoice {
    pub id: String,
    pub label: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunRequest {
    pub prompt: String,
    /// The harness picked at send time. Rides the command plane so
    /// claim-on-first-command (chat row still in flight on the registry
    /// channel) dispatches — and records — the picked harness instead of the
    /// engine default. Additive + serde-defaulted for wire compat.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harness: Option<HarnessId>,
    pub model: Option<String>,
    pub reasoning: Option<ReasoningLevel>,
    /// Harness-specific option selections (option id -> choice id), JSON round-tripped.
    #[serde(default)]
    pub model_options: serde_json::Map<String, serde_json::Value>,
    pub cwd: String,
    pub sandbox: SandboxLevel,
    #[serde(default)]
    pub auto_approve: bool,
    /// Harness-native session id to resume, if any.
    pub resume: Option<String>,
    /// Absolute paths of image attachments already staged on the run device
    /// (composer uploads: UploadChunk/UploadCommit → durable path). The same
    /// paths also ride the prompt text as `Attached images (local files …)`
    /// refs (zeron's `withAttachments` transport — that's what persists in the
    /// doc); this field additionally lets a harness inline the bytes as image
    /// content blocks. Additive + serde-defaulted for wire compat.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<String>,
    /// Host-side isolated-worktree creation (see [`WorktreeSpec`]): when set,
    /// the HOST materializes the worktree at command-drain time and runs there
    /// instead of `cwd`. Additive + serde-defaulted for wire compat — an old
    /// host ignores it and runs in `cwd` (the repo's main checkout).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree: Option<WorktreeSpec>,
    /// Zeron's own MCP server, injected by the HOST engine as it starts the
    /// run: the `zeron mcp` subcommand of this same binary, pointed at the
    /// engine's loopback IPC and stamped with the originating chat so the
    /// agent can spawn, read, and message side chats. Additive +
    /// serde-defaulted — an old host leaves it unset and the agent simply has
    /// no Zeron tools; title runs never carry it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mcp: Option<McpServer>,
}

/// A stdio MCP server the harness should add to the agent's session, on top
/// of whatever the user configured. Each driver spells it in its own dialect
/// (Claude `--mcp-config`, ACP `session/new` `mcpServers`, Codex
/// `mcp_servers.*` config overrides).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpServer {
    pub name: String,
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: std::collections::BTreeMap<String, String>,
}

/// Isolated-worktree directive riding [`RunRequest`]. The worktree is created
/// by the HOST while draining the queued Run — not by the sender over a
/// blocking CreateWorktree RPC — so the send path stays durable: a lost relay
/// frame can't wedge the composer on "Sending…" while the session runs anyway
/// (2026-08-18 user report).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorktreeSpec {
    /// The repo whose worktree to create (the space's folder on the host).
    pub repo_path: String,
    /// Base ref the fresh `zeron/<name>` branch is created off.
    pub base: String,
    /// Owning project used to resolve host-local setup Actions. Optional for
    /// wire compatibility with clients that only request worktree creation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub space_id: Option<String>,
}

/// The session-scoped singleton id for the live plan/todo chip. ACP plan
/// updates carry no wire id; adapters emit every update under this one id so
/// the fold refreshes the same chip in place. Consumers that de-duplicate
/// tool ids across segment boundaries (the engine's stale-echo filter) must
/// EXEMPT this id — it legitimately reappears in every segment for the whole
/// life of a run.
pub const LIVE_PLAN_TOOL_ID: &str = "acp-plan";

/// The generated-image id of the `index`th image an agent chose to show the
/// user (Pi's `share_images` tool). The engine copies these from the chat's
/// working directory, and the transcript shows a run of them as one strip of
/// thumbnails.
pub fn shared_image_id(tool_id: &str, index: usize) -> String {
    format!("{tool_id}:share{index}")
}

/// The tool id a [`shared_image_id`] came from, or `None` for any other id.
pub fn shared_image_tool(id: &str) -> Option<&str> {
    let (tool, tail) = id.rsplit_once(':')?;
    let index = tail.strip_prefix("share")?;
    (!index.is_empty() && index.bytes().all(|b| b.is_ascii_digit())).then_some(tool)
}

/// A decoded tool invocation, reduced to the fields each kind renders.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum ToolCall {
    Exec {
        command: String,
    },
    ReadFile {
        path: String,
    },
    WriteFile {
        path: String,
        /// Full content; STRIPPED by the render-parts policy before entering the doc.
        #[serde(skip_serializing_if = "Option::is_none")]
        content: Option<String>,
    },
    EditFile {
        path: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        old_string: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        new_string: Option<String>,
    },
    ApplyPatch {
        #[serde(skip_serializing_if = "Option::is_none")]
        path: Option<String>,
    },
    Search {
        pattern: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        path: Option<String>,
    },
    Glob {
        pattern: String,
    },
    WebFetch {
        url: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        prompt: Option<String>,
    },
    WebSearch {
        query: String,
    },
    Todo {
        #[serde(default)]
        items: Vec<TodoItem>,
    },
    Mcp {
        server: String,
        tool: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        input: Option<serde_json::Value>,
    },
    Unknown {
        name: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        input: Option<serde_json::Value>,
    },
}

impl ToolCall {
    /// A subagent SPAWN call — the `Agent[: <description>]` naming convention
    /// every driver decodes its spawn tool into (claude/codex `Task`, cursor
    /// `task`, grok `spawn_subagent`, opencode `task`). This is the single
    /// genus gate for subagent binding: tagged subagent traffic may only ever
    /// stamp a ref/status onto a spawn call, so a driver keying bug can never
    /// turn an ordinary Run/Read chip into a spawn chip (2026-08-20: claude's
    /// background-shell `task_notification` did exactly that — the chip
    /// linked to a never-created doc and opened an empty panel).
    pub fn is_subagent_spawn(&self) -> bool {
        let name = match self {
            ToolCall::Unknown { name, .. } => name,
            ToolCall::Mcp { tool, .. } => tool,
            _ => return false,
        };
        name == "Agent" || name.starts_with("Agent: ")
    }

    /// The model a subagent SPAWN was given, when the spawn named one.
    ///
    /// Read off the spawn's own input rather than the session's picked model:
    /// a spawn may override it per child (claude's `Agent` takes `model`, grok
    /// `spawn_subagent` a `model_id`), and two chips spawned in one turn can
    /// legitimately name different models. `None` means the spawn didn't say —
    /// the child inherits the parent's model, which the chip already implies,
    /// so nothing is rendered rather than guessing a name.
    ///
    /// Only ever answers for [`is_subagent_spawn`](Self::is_subagent_spawn)
    /// calls: an ordinary tool with a stray `model` argument is not a spawn.
    pub fn subagent_model(&self) -> Option<&str> {
        if !self.is_subagent_spawn() {
            return None;
        }
        let input = match self {
            ToolCall::Unknown { input, .. } | ToolCall::Mcp { input, .. } => input.as_ref()?,
            _ => return None,
        };
        SUBAGENT_MODEL_KEYS
            .iter()
            .find_map(|key| input.get(key).and_then(serde_json::Value::as_str))
            .map(str::trim)
            .filter(|model| !model.is_empty())
    }

    /// A subagent's title: the BARE task description ("verify the marker
    /// pipeline"). A fixed-width tab spent on "Agent: " never shows the task,
    /// so the genus is stripped and the call input's description/prompt
    /// fields back up a bare name (older docs); "Subagent" only as the last
    /// resort.
    pub fn subagent_title(&self) -> String {
        let (name, input) = match self {
            ToolCall::Unknown { name, input } => (name.as_str(), input.as_ref()),
            ToolCall::Mcp { tool, input, .. } => (tool.as_str(), input.as_ref()),
            _ => return "Subagent".into(),
        };
        let candidates = [
            Some(name),
            input.and_then(|i| i.get("description")?.as_str()),
            input.and_then(|i| i.get("prompt")?.as_str()),
        ];
        for text in candidates.into_iter().flatten() {
            if let Some(title) = title_line(strip_spawn_prefix(text), SUBAGENT_TITLE_MAX) {
                return title;
            }
        }
        "Subagent".into()
    }
}

/// Max chars a subagent title keeps. The strip chip is fixed-width and
/// truncates visually, but the derived title also rides drag ghosts, the
/// sidebar and the session row — cap it at the source.
pub const SUBAGENT_TITLE_MAX: usize = 40;

/// First line of `text`, trimmed, capped at `max` chars with an ellipsis.
fn title_line(text: &str, max: usize) -> Option<String> {
    let line = text.lines().find(|l| !l.trim().is_empty())?.trim();
    let mut out: String = line.chars().take(max).collect();
    if line.chars().count() > max {
        out.push('…');
    }
    Some(out)
}

/// Drop a leading "Agent"/"Task" genus (with its `:` and spacing) from a
/// spawn-title candidate. Only a real word boundary strips — "Taskmaster"
/// keeps its name. A bare "Agent"/"Task" strips to "" (no context at all).
fn strip_spawn_prefix(text: &str) -> &str {
    let t = text.trim();
    for prefix in ["agent", "task"] {
        if t.len() >= prefix.len()
            && t.is_char_boundary(prefix.len())
            && t[..prefix.len()].eq_ignore_ascii_case(prefix)
        {
            let rest = &t[prefix.len()..];
            if rest.is_empty() {
                return "";
            }
            if rest.starts_with(':') || rest.starts_with(char::is_whitespace) {
                return rest.trim_start_matches(':').trim();
            }
        }
    }
    t
}

/// Spawn-input keys that carry a child model, in precedence order. Drivers
/// disagree on the spelling, so the lookup is by key set, not by harness —
/// a new adapter naming it any of these needs no code change here.
pub const SUBAGENT_MODEL_KEYS: [&str; 4] = ["model", "modelId", "model_id", "subagent_model"];

/// The spawn-input keys [`sanitize_tool_call`](crate::) must preserve so the
/// chip can name the child's model. Deliberately tiny: everything else on a
/// spawn's input (the whole prompt, most of all) stays host-local.
pub const SUBAGENT_INPUT_KEEP: [&str; 5] = [
    "model",
    "modelId",
    "model_id",
    "subagent_model",
    "subagent_type",
];

/// Bounds on an agent's inline visual ([`AgentEvent::Visual`]): a Luau
/// script Zeron runs in a sandbox and renders natively.
pub const VISUAL_MAX_BYTES: usize = 256 * 1024;
pub const VISUAL_MAX_TITLE_CHARS: usize = 200;
/// Files a visual may ship with (models, textures, environment maps).
pub const VISUAL_MAX_ASSETS: usize = 16;
pub const VISUAL_MAX_ASSET_BYTES: usize = 8 * 1024 * 1024;
pub const VISUAL_MAX_TOTAL_ASSET_BYTES: usize = 10 * 1024 * 1024;

// The MCP server sends a visual to the engine in one WebSocket message, its
// assets base64-encoded; that message must fit the engine's 16 MiB frame
// (tungstenite's default), with room for the script escaped as JSON.
const _: () = assert!(
    VISUAL_MAX_TOTAL_ASSET_BYTES.div_ceil(3) * 4 + VISUAL_MAX_BYTES * 6 + 64 * 1024
        < 16 * 1024 * 1024
);

/// Whether `name` can name a visual's asset: a plain file name.
pub fn valid_visual_asset_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && !name.starts_with('.')
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
}

/// Where one checklist item stands. Claude's TodoWrite, OpenCode and ACP plans
/// distinguish the item being worked on from those still waiting; Codex and
/// Cursor only report done / not done.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum TodoStatus {
    #[default]
    Pending,
    InProgress,
    Completed,
}

impl TodoStatus {
    /// Decode a harness status string. Anything unrecognised (including
    /// `cancelled`) is `Pending`: not finished, not being worked on.
    pub fn parse(raw: &str) -> Self {
        match raw {
            "completed" | "complete" | "done" => Self::Completed,
            "in_progress" | "inProgress" | "in-progress" | "active" => Self::InProgress,
            _ => Self::Pending,
        }
    }
}

/// One checklist entry.
///
/// `done` is the original wire field and stays authoritative for completion,
/// so docs written before `status` existed (and readers that ignore it) keep
/// working. `status` is additive and written only when it adds information —
/// i.e. for an in-progress item; a missing or unreadable value derives from
/// `done`. Build items with [`TodoItem::new`] to keep the two consistent.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TodoItem {
    pub text: String,
    pub done: bool,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "lenient_todo_status"
    )]
    pub status: Option<TodoStatus>,
}

impl TodoItem {
    pub fn new(text: impl Into<String>, status: TodoStatus) -> Self {
        Self {
            text: text.into(),
            done: status == TodoStatus::Completed,
            status: (status == TodoStatus::InProgress).then_some(status),
        }
    }

    /// Effective status: `done` wins, then the explicit status, else pending.
    pub fn status(&self) -> TodoStatus {
        if self.done {
            TodoStatus::Completed
        } else {
            self.status.unwrap_or_default()
        }
    }
}

/// A status this build does not know (a future `blocked`, say) must not fail
/// the whole tool call — the part would vanish from the transcript. Treat it
/// as absent so the item falls back to `done`.
fn lenient_todo_status<'de, D>(deserializer: D) -> Result<Option<TodoStatus>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Option::<serde_json::Value>::deserialize(deserializer)?;
    Ok(value.and_then(|v| serde_json::from_value(v).ok()))
}

/// A slash command advertised by the agent (ACP `availableCommands`): typed as
/// `/name` at the start of the composer, sent to the agent as prompt text.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SlashCommand {
    pub name: String,
    #[serde(default)]
    pub description: String,
    /// Placeholder hint for the command's argument, when it takes one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_hint: Option<String>,
    /// Sub-commands and other argument values the command offers, in order
    /// (Pi's `getArgumentCompletions`: `/goal draft`, `/mcp reconnect <server>`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub arguments: Vec<ArgumentCompletion>,
    /// Runs beside the conversation (an extension's own control, such as
    /// Pi's `/jev off`): sent straight to the live agent on
    /// [`SIDE_COMMAND_ELEMENT`], never written to the chat, and what it
    /// reports shows as toasts.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub side: bool,
}

/// One value a slash command accepts as its argument. `value` is the text
/// that replaces the typed argument (a trailing space hands over to the next
/// word); `children` are the values that may follow it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ArgumentCompletion {
    pub value: String,
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<ArgumentCompletion>,
}

/// A file modification carried inline on a tool result (ACP
/// `ToolCallContent::Diff`). `old_text: None` means a new file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolDiff {
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub old_text: Option<String>,
    pub new_text: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UserInputQuestion {
    pub id: String,
    pub header: String,
    pub question: String,
    pub options: Vec<String>,
    #[serde(default)]
    pub multi_select: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prefill: Option<String>,
    #[serde(default)]
    pub multiline: bool,
    /// The free-text answer is a credential (an API key, a password): shown
    /// as dots and never copied.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub secret: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UserInputAnswer {
    pub question_id: String,
    pub labels: Vec<String>,
}

/// The label a secret question's answer carries in the chat's command
/// history: the secret itself went to the agent directly, as an
/// [`crate::extension_ui::ExtensionUiAction`] on
/// `SECRET_ANSWER_ELEMENT` + the question's id (`value: {"value": …}`).
pub const SECRET_ANSWER_SENT: &str = "\u{2063}secret-sent";
/// Element id prefix of a secret question's direct answer.
pub const SECRET_ANSWER_ELEMENT: &str = "pi:secret:";
/// Element id of a [`SlashCommand::side`] command sent to the live agent
/// (`value: {"text": "/jev off"}`): it never enters the chat's history.
pub const SIDE_COMMAND_ELEMENT: &str = "pi:side-command";
/// The Pi command that rewinds code and/or conversation to a user message
/// (Pi's `zeron-rewind` extension), sent as a side command:
/// `/pi_ui_rewind <harness ref> <both|conversation|code> <nonce>`.
pub const REWIND_COMMAND: &str = "pi_ui_rewind";
/// Host channel carrying the rewind command's last answer
/// (`{"v":1,"ok":..,"nonce":..,"conversation":..,"restored":n,...}`).
pub const REWIND_CHANNEL: &str = "zeron:rewind";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum DoneStatus {
    Completed,
    Interrupted,
    Errored,
}

/// Why a run failed, when the failure text is recognisable. Informational:
/// it picks the hint the UI shows and what orchestrators are told, and never
/// triggers a resume or failover. `Other` is the conservative default.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum ErrorClass {
    /// `resets_at` is unix ms when the provider said when it clears.
    #[serde(rename_all = "camelCase")]
    RateLimited {
        resets_at: Option<i64>,
    },
    #[serde(rename_all = "camelCase")]
    QuotaExhausted {
        resets_at: Option<i64>,
    },
    #[serde(rename_all = "camelCase")]
    AuthRequired {
        provider: Option<String>,
    },
    ProviderOverloaded,
    SessionGone,
    /// The agent process was ended by a signal it did not choose. A SIGKILL
    /// (`signal` 9) is `oom_suspected` unless the kernel's OOM-kill counter
    /// proves the OOM killer did not fire.
    #[serde(rename_all = "camelCase")]
    ProcessKilled {
        signal: i32,
        oom_suspected: bool,
    },
    Other,
}

impl ErrorClass {
    /// Stable camelCase name of the class (the serde `kind`).
    pub fn slug(&self) -> &'static str {
        match self {
            ErrorClass::RateLimited { .. } => "rateLimited",
            ErrorClass::QuotaExhausted { .. } => "quotaExhausted",
            ErrorClass::AuthRequired { .. } => "authRequired",
            ErrorClass::ProviderOverloaded => "providerOverloaded",
            ErrorClass::SessionGone => "sessionGone",
            ErrorClass::ProcessKilled { .. } => "processKilled",
            ErrorClass::Other => "other",
        }
    }

    /// The `resets_at` (unix ms) of a class that carries one.
    pub fn resets_at(&self) -> Option<i64> {
        match self {
            ErrorClass::RateLimited { resets_at } | ErrorClass::QuotaExhausted { resets_at } => {
                *resets_at
            }
            _ => None,
        }
    }

    /// The provider an `AuthRequired` failure names.
    pub fn provider(&self) -> Option<&str> {
        match self {
            ErrorClass::AuthRequired { provider } => provider.as_deref(),
            _ => None,
        }
    }

    /// One line telling the user what to do next; `None` for `Other`.
    pub fn hint(&self) -> Option<String> {
        Some(match self {
            ErrorClass::AuthRequired { provider } => match provider.as_deref() {
                Some(p) => format!("Log in to {p} again (Settings > Accounts)"),
                None => "Log in again (Settings > Accounts)".to_string(),
            },
            ErrorClass::RateLimited { resets_at } => match resets_at.and_then(local_time_label) {
                Some(at) => format!("Rate limited; try again after {at}"),
                None => "Rate limited; try again shortly".to_string(),
            },
            ErrorClass::QuotaExhausted { resets_at } => {
                let base = "Usage limit reached; wait for it to reset or switch account or model";
                match resets_at.and_then(local_time_label) {
                    Some(at) => format!("{base} (resets {at})"),
                    None => base.to_string(),
                }
            }
            ErrorClass::ProviderOverloaded => {
                "The provider is overloaded; try again in a moment".to_string()
            }
            ErrorClass::SessionGone => {
                "Pi no longer has this session; start a new chat".to_string()
            }
            ErrorClass::ProcessKilled {
                oom_suspected: true,
                ..
            } => "Pi was killed by the system (SIGKILL), most likely out of memory. \
                  Free memory or raise the limit, then ask it to continue."
                .to_string(),
            ErrorClass::ProcessKilled { signal: 9, .. } => {
                "Pi was killed (SIGKILL) by another process, not the kernel's OOM killer; \
                 ask it to continue"
                    .to_string()
            }
            ErrorClass::ProcessKilled { signal, .. } => {
                format!("Pi was ended by signal {signal}; ask it to continue")
            }
            ErrorClass::Other => return None,
        })
    }
}

/// `3:04 PM` in the host's local zone, or `Mon 3:04 PM` when more than a day
/// out; `None` for an unrepresentable timestamp.
fn local_time_label(ms: i64) -> Option<String> {
    use chrono::{Local, TimeZone};
    let at = Local.timestamp_millis_opt(ms).single()?;
    let far = at.signed_duration_since(Local::now()) > chrono::Duration::hours(24);
    Some(
        at.format(if far { "%a %-I:%M %p" } else { "%-I:%M %p" })
            .to_string(),
    )
}

/// The normalized streaming event every harness emits.
///
/// Mirrors zeron's `AgentEvent` tagged enum.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum AgentEvent {
    #[serde(rename_all = "camelCase")]
    SessionStarted {
        harness: HarnessId,
        model: String,
        #[serde(default)]
        tools: Vec<String>,
        cwd: String,
        /// Harness-native session id (used for resume).
        session_id: String,
        assistant_message_id: String,
    },
    TextDelta {
        text: String,
    },
    /// A generated raster asset. The engine materializes this path before publication.
    #[serde(rename_all = "camelCase")]
    GeneratedImage {
        id: String,
        path: String,
        name: String,
        mime_type: String,
    },
    /// An agent-authored visual shown inline in the reply: a Luau script
    /// that builds native UI (`zeron-visual`). The engine emits it when the
    /// agent calls Zeron's `show_visual` MCP tool; `path` is the stored
    /// script on the host device.
    Visual {
        id: String,
        path: String,
        title: String,
    },
    ReasoningDelta {
        text: String,
    },
    /// Backend-internal steering boundary marker.
    #[serde(rename_all = "camelCase")]
    AssistantMessageCompleted {
        assistant_message_id: String,
    },
    ToolCall {
        id: String,
        call: ToolCall,
    },
    #[serde(rename_all = "camelCase")]
    ToolResult {
        id: String,
        is_error: bool,
        /// Tool output text, capped by the emitting harness (ACP tool-call
        /// content; claude/codex adapters never populate it). The doc-side
        /// fold applies its own byte cap before anything persists.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        output: Option<String>,
        /// Inline file diff for edit-shaped tools (ACP `Diff` content).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        diff: Option<ToolDiff>,
    },
    /// Latest context occupancy, independent of cumulative billing usage.
    /// Missing fields preserve the previous measurement; zero tokens is valid.
    #[serde(rename_all = "camelCase")]
    ContextUsage {
        tokens: Option<u64>,
        window: Option<u64>,
    },
    /// The harness is retrying after a provider failure (Pi `auto_retry_start`
    /// / `auto_retry_end`). The engine publishes it on the session row; never
    /// persisted to docs and never a turn boundary.
    #[serde(rename_all = "camelCase")]
    Retrying {
        attempt: u32,
        #[serde(default)]
        max_attempts: Option<u32>,
        #[serde(default)]
        delay_ms: Option<u64>,
        #[serde(default)]
        error: Option<String>,
        /// True on start, false once the retry sequence ended.
        active: bool,
    },
    /// Kept as a harness passthrough (rate-limit probes); never persisted to docs.
    #[serde(rename_all = "camelCase")]
    Usage {
        input_tokens: u64,
        output_tokens: u64,
    },
    /// The agent advertised (or changed) its slash-command set — ACP
    /// `available_commands_update`. The engine caches the latest list per
    /// harness for the composer's `/` popup; never persisted to docs.
    #[serde(rename_all = "camelCase")]
    AvailableCommands {
        commands: Vec<SlashCommand>,
    },
    Error {
        message: String,
    },
    #[serde(rename_all = "camelCase")]
    InputRequested {
        request_id: String,
        questions: Vec<UserInputQuestion>,
    },
    #[serde(rename_all = "camelCase")]
    InputResolved {
        request_id: String,
    },
    /// A confirmed new assignment. When tagged as Subagent, this reopens the
    /// same child transcript even if the provider does not echo the user text.
    #[serde(rename_all = "camelCase")]
    Steered {
        assistant_message_id: Option<String>,
        next_assistant_message_id: Option<String>,
    },
    /// A user message reached the harness's own session, which names it
    /// `harness_ref` (Pi: `ts:<message timestamp>`), so it can be rewound
    /// there. `message_id` is the Zeron message; `None` is the prompt the
    /// run started with.
    #[serde(rename_all = "camelCase")]
    UserMessageDelivered {
        message_id: Option<String>,
        harness_ref: String,
    },
    #[serde(rename_all = "camelCase")]
    Done {
        status: DoneStatus,
        result: Option<String>,
        error: Option<String>,
        session_id: Option<String>,
        /// What kind of failure `error` is, when the harness could tell.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        class: Option<ErrorClass>,
    },
    /// A USER-role message injected into a running session — today only seen
    /// wrapped in [`AgentEvent::Subagent`]: the PARENT agent steering its
    /// subagent mid-run (claude: a tagged user frame's text blocks). The
    /// engine writes it to the subagent doc as its own user entry, closing
    /// the streaming assistant segment above it — the subagent transcript
    /// then reads like any steered chat. Never emitted untagged (the parent
    /// chat's user messages come from doc commands, not the wire).
    #[serde(rename_all = "camelCase")]
    UserMessage {
        text: String,
    },
    /// An event belonging to a SUBAGENT's nested transcript, attributed to
    /// the spawning tool call (`parent_tool_use_id` = the parent-feed
    /// `ToolCall::id` that launched it). Never folded into the parent chat
    /// doc — the engine routes these to the subagent's own doc; the parent
    /// keeps only the spawn chip. Additive: old consumers that don't match
    /// this variant drop the nested traffic, which is the pre-subagent-viz
    /// behavior.
    #[serde(rename_all = "camelCase")]
    Subagent {
        parent_tool_use_id: String,
        event: Box<AgentEvent>,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_class_round_trips_and_done_stays_compatible() {
        let class = ErrorClass::RateLimited {
            resets_at: Some(1_700_000_000_000),
        };
        let json = serde_json::to_value(&class).unwrap();
        assert_eq!(json["kind"], "rateLimited");
        assert_eq!(json["resetsAt"], 1_700_000_000_000i64);
        assert_eq!(serde_json::from_value::<ErrorClass>(json).unwrap(), class);

        // A journal written before the field existed.
        let old =
            r#"{"type":"done","status":"errored","result":null,"error":"x","sessionId":null}"#;
        let ev: AgentEvent = serde_json::from_str(old).unwrap();
        assert!(matches!(ev, AgentEvent::Done { class: None, .. }));

        let plain = AgentEvent::Done {
            status: DoneStatus::Errored,
            result: None,
            error: Some("x".into()),
            session_id: None,
            class: None,
        };
        assert!(!serde_json::to_string(&plain).unwrap().contains("class"));
        let classed = AgentEvent::Done {
            status: DoneStatus::Errored,
            result: None,
            error: Some("x".into()),
            session_id: None,
            class: Some(ErrorClass::SessionGone),
        };
        let json = serde_json::to_string(&classed).unwrap();
        assert_eq!(serde_json::from_str::<AgentEvent>(&json).unwrap(), classed);
    }

    #[test]
    fn error_class_hints() {
        let auth = ErrorClass::AuthRequired {
            provider: Some("openai-codex".into()),
        };
        assert!(
            auth.hint()
                .unwrap()
                .starts_with("Log in to openai-codex again")
        );
        assert_eq!(auth.slug(), "authRequired");
        assert!(
            ErrorClass::AuthRequired { provider: None }
                .hint()
                .unwrap()
                .starts_with("Log in again")
        );
        assert_eq!(
            ErrorClass::RateLimited { resets_at: None }.hint().unwrap(),
            "Rate limited; try again shortly"
        );
        let soon = chrono::Utc::now().timestamp_millis() + 60_000;
        assert!(
            ErrorClass::RateLimited {
                resets_at: Some(soon)
            }
            .hint()
            .unwrap()
            .starts_with("Rate limited; try again after ")
        );
        assert!(
            ErrorClass::QuotaExhausted { resets_at: None }
                .hint()
                .unwrap()
                .starts_with("Usage limit reached")
        );
        assert!(ErrorClass::ProviderOverloaded.hint().is_some());
        assert!(ErrorClass::SessionGone.hint().is_some());
        assert_eq!(ErrorClass::Other.hint(), None);
    }

    #[test]
    fn a_killed_process_class_round_trips_and_says_what_to_do() {
        let killed = ErrorClass::ProcessKilled {
            signal: 9,
            oom_suspected: true,
        };
        assert_eq!(killed.slug(), "processKilled");
        let json = serde_json::to_value(&killed).unwrap();
        assert_eq!(
            json,
            serde_json::json!({"kind": "processKilled", "signal": 9, "oomSuspected": true})
        );
        assert_eq!(serde_json::from_value::<ErrorClass>(json).unwrap(), killed);
        assert!(killed.hint().unwrap().contains("out of memory"));
        let crash = ErrorClass::ProcessKilled {
            signal: 11,
            oom_suspected: false,
        };
        let hint = crash.hint().unwrap();
        assert!(
            hint.contains("signal 11") && !hint.contains("memory"),
            "{hint}"
        );
        let outside = ErrorClass::ProcessKilled {
            signal: 9,
            oom_suspected: false,
        }
        .hint()
        .unwrap();
        assert!(
            outside.contains("another process") && !outside.contains("most likely out of memory"),
            "{outside}"
        );
        assert!(!killed.hint().unwrap().contains("  "), "no stray indentation");
    }

    #[test]
    fn agent_event_round_trips() {
        let ev = AgentEvent::ToolCall {
            id: "t1".into(),
            call: ToolCall::Exec {
                command: "cargo test".into(),
            },
        };
        let json = serde_json::to_string(&ev).unwrap();
        assert_eq!(serde_json::from_str::<AgentEvent>(&json).unwrap(), ev);
    }

    /// Drivers spell the key differently; the chip must not care which one
    /// spawned the child. Non-spawns never answer, whatever they carry.
    #[test]
    fn subagent_model_reads_every_spelling_and_only_off_a_spawn() {
        let spawn = |input: serde_json::Value| ToolCall::Unknown {
            name: "Agent: scan".into(),
            input: Some(input),
        };
        for key in SUBAGENT_MODEL_KEYS {
            let call = spawn(serde_json::json!({ key: "haiku" }));
            assert_eq!(call.subagent_model(), Some("haiku"), "key {key}");
        }
        // An MCP-shaped spawn (cursor routes its `task` through MCP) too.
        assert_eq!(
            ToolCall::Mcp {
                server: "s".into(),
                tool: "Agent: scan".into(),
                input: Some(serde_json::json!({ "model": "sonnet" })),
            }
            .subagent_model(),
            Some("sonnet")
        );
        // Not a spawn: the name gate wins over the key.
        assert_eq!(
            ToolCall::Unknown {
                name: "Bash".into(),
                input: Some(serde_json::json!({ "model": "haiku" })),
            }
            .subagent_model(),
            None
        );
        // A spawn that named nothing usable inherits — nothing to render.
        assert_eq!(
            spawn(serde_json::json!({ "model": " " })).subagent_model(),
            None
        );
        assert_eq!(
            spawn(serde_json::json!({ "prompt": "x" })).subagent_model(),
            None
        );
        assert_eq!(
            ToolCall::Unknown {
                name: "Agent".into(),
                input: None
            }
            .subagent_model(),
            None
        );
        // Non-string values are not names.
        assert_eq!(
            spawn(serde_json::json!({ "model": 5 })).subagent_model(),
            None
        );
    }

    #[test]
    fn run_request_attachments_default_and_round_trip() {
        // Old-wire JSON without the field parses (additive compat)…
        let old = r#"{"prompt":"p","model":null,"reasoning":null,"cwd":".","sandbox":"workspace-write","resume":null}"#;
        let req: RunRequest = serde_json::from_str(old).unwrap();
        assert!(req.attachments.is_empty());
        // …and an empty list serializes away (old readers never see it).
        let json = serde_json::to_value(&req).unwrap();
        assert!(json.get("attachments").is_none());
        // Populated lists round-trip.
        let req = RunRequest {
            attachments: vec!["/tmp/a.png".into()],
            ..req
        };
        let round: RunRequest =
            serde_json::from_value(serde_json::to_value(&req).unwrap()).unwrap();
        assert_eq!(round.attachments, vec!["/tmp/a.png".to_string()]);
    }

    #[test]
    fn run_request_worktree_default_and_round_trip() {
        // Old-wire JSON without the field parses (additive compat)…
        let old = r#"{"prompt":"p","model":null,"reasoning":null,"cwd":".","sandbox":"workspace-write","resume":null}"#;
        let req: RunRequest = serde_json::from_str(old).unwrap();
        assert!(req.worktree.is_none());
        // …and `None` serializes away (old readers never see it).
        let json = serde_json::to_value(&req).unwrap();
        assert!(json.get("worktree").is_none());
        // A populated spec round-trips camelCased.
        let req = RunRequest {
            worktree: Some(WorktreeSpec {
                repo_path: "/repos/comet".into(),
                base: "main".into(),
                space_id: Some("space-1".into()),
            }),
            ..req
        };
        let json = serde_json::to_value(&req).unwrap();
        assert_eq!(json["worktree"]["repoPath"], "/repos/comet");
        assert_eq!(json["worktree"]["spaceId"], "space-1");
        let round: RunRequest = serde_json::from_value(json).unwrap();
        assert_eq!(round.worktree, req.worktree);
    }

    #[test]
    fn harness_id_uses_kebab_case() {
        assert_eq!(
            serde_json::to_string(&HarnessId::ClaudeCode).unwrap(),
            "\"claude-code\""
        );
    }

    #[test]
    fn harness_update_status_uses_stable_wire_names() {
        let status = HarnessUpdateStatus {
            harness: HarnessId::ClaudeCode,
            installed_version: Some("1.0.0".into()),
            latest_version: Some("1.1.0".into()),
            channel: Some("stable".into()),
            source: HarnessInstallSource::Npm,
            policy: HarnessUpdatePolicy::AutoWhenIdle,
            phase: HarnessUpdatePhase::WaitingForIdle,
            progress: None,
            checked_at: Some(42),
            error: None,
            can_apply: true,
            manual_command: Some("claude update".into()),
        };
        let json = serde_json::to_value(&status).unwrap();
        assert_eq!(json["harness"], "claude-code");
        assert_eq!(json["installedVersion"], "1.0.0");
        assert_eq!(json["policy"], "auto-when-idle");
        assert_eq!(json["phase"], "waiting-for-idle");
        assert_eq!(json["canApply"], true);
        assert_eq!(
            serde_json::from_value::<HarnessUpdateStatus>(json.clone()).unwrap(),
            status
        );
        let mut legacy = json;
        legacy.as_object_mut().unwrap().remove("canApply");
        assert!(
            !serde_json::from_value::<HarnessUpdateStatus>(legacy)
                .unwrap()
                .can_apply
        );
    }

    #[test]
    fn manual_checks_do_not_claim_an_available_update() {
        let mut status: HarnessUpdateStatus = serde_json::from_value(serde_json::json!({
            "harness": "cursor",
            "phase": "manual-action-required",
            "canApply": true,
        }))
        .unwrap();
        assert!(!status.show_update_notice());
        status.phase = HarnessUpdatePhase::Available;
        status.latest_version = Some("2.0.0".into());
        status.can_apply = false;
        assert!(
            status.show_update_notice(),
            "known manual releases still need attention"
        );
        status.phase = HarnessUpdatePhase::Installing;
        assert!(status.show_update_notice());
    }
}

/// Host-owned context snapshot, replicated with the chat document.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContextUsage {
    pub tokens: Option<u64>,
    pub window: Option<u64>,
}

impl ContextUsage {
    pub fn fraction(self) -> Option<f64> {
        Some(self.tokens? as f64 / self.window.filter(|n| *n > 0)? as f64)
    }
}

#[cfg(test)]
mod generated_image_tests {
    use super::*;

    #[test]
    fn generated_image_event_round_trip() {
        assert_eq!(shared_image_tool(&shared_image_id("t:1", 3)), Some("t:1"));
        assert_eq!(shared_image_tool("t:image"), None);
        assert_eq!(shared_image_tool("t:page"), None);
        assert_eq!(shared_image_tool("t:share"), None);
        assert_eq!(shared_image_tool("t:sharex"), None);
        let event = AgentEvent::GeneratedImage {
            id: "item:image".into(),
            path: "/uploads/generated.png".into(),
            name: "generated.png".into(),
            mime_type: "image/png".into(),
        };
        let value = serde_json::to_value(&event).unwrap();
        assert_eq!(value["type"], "generatedImage");
        assert_eq!(value["mimeType"], "image/png");
        assert_eq!(serde_json::from_value::<AgentEvent>(value).unwrap(), event);
    }
}

#[cfg(test)]
mod todo_item_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn legacy_items_without_status_derive_from_done() {
        let done: TodoItem = serde_json::from_value(json!({ "text": "a", "done": true })).unwrap();
        let open: TodoItem = serde_json::from_value(json!({ "text": "b", "done": false })).unwrap();
        assert_eq!(done.status(), TodoStatus::Completed);
        assert_eq!(open.status(), TodoStatus::Pending);
        assert_eq!(open.status, None);
    }

    #[test]
    fn only_in_progress_adds_a_field_so_other_docs_are_unchanged() {
        let pending = serde_json::to_value(TodoItem::new("a", TodoStatus::Pending)).unwrap();
        let done = serde_json::to_value(TodoItem::new("b", TodoStatus::Completed)).unwrap();
        let active = serde_json::to_value(TodoItem::new("c", TodoStatus::InProgress)).unwrap();
        assert_eq!(pending, json!({ "text": "a", "done": false }));
        assert_eq!(done, json!({ "text": "b", "done": true }));
        assert_eq!(
            active,
            json!({ "text": "c", "done": false, "status": "inProgress" })
        );
    }

    #[test]
    fn unknown_status_falls_back_to_done_instead_of_failing_the_call() {
        let call: ToolCall = serde_json::from_value(json!({
            "kind": "todo",
            "items": [
                { "text": "a", "done": false, "status": "blocked" },
                { "text": "b", "done": true, "status": 7 },
            ]
        }))
        .unwrap();
        let ToolCall::Todo { items } = call else {
            panic!("expected a todo call");
        };
        assert_eq!(items[0].status(), TodoStatus::Pending);
        assert_eq!(items[1].status(), TodoStatus::Completed);
    }

    #[test]
    fn done_outranks_a_stale_status() {
        let item = TodoItem {
            text: "a".into(),
            done: true,
            status: Some(TodoStatus::InProgress),
        };
        assert_eq!(item.status(), TodoStatus::Completed);
    }

    #[test]
    fn harness_status_strings_decode() {
        assert_eq!(TodoStatus::parse("completed"), TodoStatus::Completed);
        assert_eq!(TodoStatus::parse("in_progress"), TodoStatus::InProgress);
        assert_eq!(TodoStatus::parse("inProgress"), TodoStatus::InProgress);
        assert_eq!(TodoStatus::parse("pending"), TodoStatus::Pending);
        assert_eq!(TodoStatus::parse("cancelled"), TodoStatus::Pending);
        assert_eq!(TodoStatus::parse(""), TodoStatus::Pending);
    }
}
