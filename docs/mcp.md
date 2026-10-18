# Zeron MCP server

`zeron mcp` serves the Model Context Protocol on stdin/stdout and proxies every
tool into the running engine's localhost IPC (`ws://127.0.0.1:$ZERON_IPC_PORT`,
default 27654) — the same `zeron_rpc` surface the headed app and `zeron sync`
dial. It is a subcommand of the one `zeron` binary: no Node runtime, no extra
install, a few MB resident.

Crate: `crates/mcp` (`zeron-mcp`). The protocol layer is hand-rolled
(`initialize`, `ping`, `tools/list`, `tools/call`; newline-delimited JSON-RPC
2.0) — the repo already owns JSON-RPC framing for the Codex and ACP drivers and
the stdio tool-server subset is tiny, so no SDK dependency was taken.

## Identity

The engine can inject this server into a harness's MCP config with the
originating chat in the environment:

| Variable          | Meaning                                                       |
| ----------------- | ------------------------------------------------------------- |
| `ZERON_IPC_PORT`  | Engine to proxy (default 27654).                              |
| `ZERON_CHAT_ID`   | The chat whose agent spawned this server.                     |
| `ZERON_DEVICE_ID` | That chat's host device.                                      |
| `ZERON_ROLE`      | `orchestrator` for a root, `member` inside a tree, else unset. |
| `ZERON_TREE_DEPTH` | A member's level below its root.                             |
| `ZERON_CHAT_TITLE` | The chat's title (a tree chat's task label), when it has one. |
| `ZERON_PARENT_CHAT_ID`, `ZERON_PARENT_TITLE` | The chat that started it.   |
| `ZERON_ROOT_CHAT_ID` | Its orchestrator root (its own id, for a root).             |

These come from `zeron_proto::orchestrator::Place`, computed by the host from
the chat and its ancestors on every run. Pi gets them in its process env too
(sub-agents inherit them), with `ZERON_READ_ONLY=1` for a read-only run; the Pi
harness turns them into the session's capability map (see [Pi](pi.md)).
`whoami` returns them as `place` (role, level, task, parent, orchestrator,
brief) for any harness. Titles are one line of at most 120 characters.

When `ZERON_CHAT_ID` is set, every `send_message` is prefixed with a
`[Message from Zeron chat <title> (<id8>) …]` line so the receiving agent and the
human reading that transcript can tell an agent-to-agent message from a typed
one, and the server refuses to message its own chat. The transcript renders
this routing header as “Message from **chat name**”, keeping the full routing
instructions in the stored prompt for agents.

### Parent links

`create_chat` and each request in `create_chats` accept `kind`:

| Input | Placement |
| --- | --- |
| `kind: "chat"` | Standalone session, visible in the left **Sessions** sidebar; no `parentChatId` is written. |
| `kind: "side"` | Child of the explicit `parent` or origin chat. Requires one of those. |
| Kind omitted, origin or parent supplied | Child, preserving the previous default. |
| Kind omitted, no origin or parent | Standalone, preserving terminal usage. |

`kind: "chat"` with a nonempty `parent`, `kind: "side"` without an origin/parent,
and unknown kinds fail before any writes. Empty `parent` uses the default.
The creation result includes the effective `kind`, `chatId`, `deviceId`,
`project` and `parentChatId`. Kind is derived from the parent; it is not persisted.

Children record `Chat.parent_chat_id` (proto) ⇄ `parentChatId` on the registry
row (`Mutate createChat { parentChatId? }`). Chats with a parent cannot create
chats through MCP, including standalone and batch creation or an explicit parent
override. A side chat cannot be selected as a parent; only one level is supported.

The exception is an orchestrator's tree (`zeron_proto::orchestrator`): a root
chat whose id starts with `orchestrator-`, and every chat below it. There a
session may start sessions, and be a parent, until it is `MAX_DEPTH` (4)
levels below the root; deeper, `create_chat` refuses with "...levels below its
orchestrator...". Their children default to the parent's harness, and a `cwd`
that is a project's folder (any separator or trailing slash, any case on a
Windows drive) joins that project. The sidebar's Orchestrator tab lists these
trees; the root never shows in Sessions.

### Write access

`create_chat` (and each `create_chats` request) takes `write`, default `true`:

| `write` | Sandbox | Folder |
| --- | --- | --- |
| `true` | `workspace-write` | In a git project's folder: a new worktree on a `zeron/<name>` branch, made by the host on the first run and reused after; `branch` names its start (default the project's `HEAD`). Elsewhere, or with a `cwd` outside the project folder: that folder. |
| `false` | `read-only` | The project folder. For Pi, the guard blocks every edit (see [Pi](pi.md#read-only-sessions)). |

A `sandbox` that disagrees with `write` fails before any writes. The result
carries `write` and `workspace`. A delegated chat created without a prompt gets
its worktree on its first `send_message`.

For children, `parent` (id, prefix, or title) overrides the origin (`ZERON_CHAT_ID`).
`list_chats { parent }` returns children, and chat summaries carry `parentChatId`,
`runningSubagents` and `backgroundJobs` (Pi's running background shells, from
the `bash-bg:fleet` channel). btw side chats are never listed. A child's
outcome note is held while it is idle on background jobs: the note comes when
they settle, and a btw aside never notifies its parent.
Rows from older engines read as parentless; a dangling parent id is tolerated.

The sidebar's Sessions list holds top-level chats only (`AppState::visible_chats`);
children nest under their listed parent instead
([sidebar side chats](reference/sidebar-side-chats.md)). Jump slots, cycling and
the Archived section stay top-level, and Orchestrator members show only in the
Orchestrator tab. Children remain addressable by id, deep link, and every MCP
tool; `list_chats { parent }` is how an orchestrator finds them.

### Injection

The host engine stamps this server onto every run it drives
(`RunRequest.mcp`, additive): the same `zeron` binary with `args: ["mcp"]`
and the three variables above, pointed at the port the engine itself serves
(never a port it lost the bind race for). Each driver spells it in its own
dialect and leaves the user's configured servers alone:

| Harness | Where |
| ------- | ----- |
| Claude  | `--mcp-config <inline json>` (no `--strict-mcp-config`)             |
| ACP (Devin, Grok, Hermes, Antigravity) | `session/new` and `session/load` → `mcpServers: [{name, command, args, env}]` |
| Pi | Per-run `--extension` bridges stdio MCP into Pi tools in the native RPC process |
| OpenCode | Child-only `OPENCODE_CONFIG_CONTENT`: `mcp.zeron` on 1.x, `mcp.servers.zeron` on 2.x |
| Codex   | `thread/start` config overrides `mcp_servers.zeron.{command,args,env}` |
| Cursor  | SDK `Agent.create` / `Agent.resume` → inline `mcpServers.zeron`, plus `local.settingSources: ["user", "team", "mdm", "plugins"]` so `~/.cursor/mcp.json` and plugin servers load (not `project`: the SDK skips MCP approvals, so repo-defined servers would run unprompted) |

OpenCode preserves inherited inline configuration and other servers. Its config
shape follows the installed binary's major version. Cursor uses the SDK's
[inline MCP configuration](https://cursor.com/docs/sdk/typescript); OpenCode's
[1.x config layer](https://opencode.ai/docs/config/) and
[2.x MCP format](https://opencode.ai/v2/docs/mcp-servers) differ.
Pi's temporary extension lives only for the native RPC process lifetime;
user settings, extensions, and session arguments remain intact. The bridge
registers `zeron_<tool>` tools, propagates cancellation and errors, and closes
the MCP child when the Pi session shuts down.

Title runs never carry it. A run with no served port (embedded engine that
lost the bind) gets no Zeron tools rather than a dead server.

### Forks and the session panel

`ForkSideChat { chatId, sourceChatId, parentChatId? }` copies a chat's
history through its latest completed response into a new chat and appends
a `fork` part (a system entry: `sourceChatId`, `sourceTitle`) as the seam;
the transcript draws it as "This chat was forked from <title>". `parentChatId`
defaults to the source; a side chat's own fork button passes its parent so
the copy lists as a sibling. The session panel's **Sub-agents** section
lists a chat's parent, its children (forks and `kind: "side"` spawns), its
spawn chips and Pi's sub-agent fleet, and opens each in the right pane.

A side chat created on a parent with `createChat` + `parentChatId` (the btw
pane, or `create_chat` outside an orchestrator tree) starts as a fork too, so
it never has to read the parent to catch up. On its first run the host saves
the fork in the side chat's doc (`forkParent`: parent id, title, cwd, the
parent's last completed reply, its provider session). A Pi side chat of a Pi
parent on the same device gets `ZERON_FORK_SESSION` on the run's MCP env, and
the Pi harness starts with `--fork <parent session file>`: a new session file
with the parent's whole context, with the parent's file left as it is. If Pi
can't resolve that file, the run starts fresh with a visible notice. Any other
harness, or a later fresh session, gets the parent's transcript up to the
saved reply (newest first, within about 60k characters, each tool output cut
to 2k) at the front of its first prompt. The parent's later turns never reach
it. Members of an orchestrator tree still get only their task.

## Tools

Chats are referenced by full id, a unique id prefix, or an exact title.
Projects by id, path, display name, or unique path suffix. Devices by id or
name (default: the local engine's device).

| Tool               | Engine calls                                              |
| ------------------ | --------------------------------------------------------- |
| `whoami`           | `LocalDevice`, `EngineInfo`, origin chat summary          |
| `list_devices`     | `WatchDevices` snapshot                                   |
| `list_projects`    | `WatchSpaces` snapshot                                    |
| `list_harnesses`   | `ListHarnesses {targetDeviceId?}`                          |
| `list_models`      | `ListModels {harness, targetDeviceId?}`                                    |
| `list_chats`       | `WatchChats` + `WatchSessions` snapshots (status merged)  |
| `get_chat`         | above + `WatchDocMessages` opening frame (pending input)  |
| `create_chat`      | `Mutate createChat` (+ `renameChat`; optional first send) |
| `create_chats`     | Concurrent `create_chat` requests with per-request results |
| `send_messages`    | Concurrent `send_message` requests with per-request results |
| `read_chat`        | `WatchDocMessages` opening `reset` frame, rendered        |
| `send_message`     | `QueueCommand` Run / Steer, or `QueueMessage`             |
| `wait_for_turn`    | `WatchSessions` until the chat settles                    |
| `interrupt_chat`   | `QueueCommand` Interrupt                                  |
| `respond_to_input` | `QueueCommand` RespondInput                               |
| `archive_chat`     | `Mutate setChatArchived`                                  |
| `show_visual`      | `PublishVisual {chatId, title, script, assets}` (origin chat only; assets base64 by name, read by the MCP server from the agent's cwd) |
| `board_post`       | `PostBoard {chatId, kind, text}`, then a `send_message`-style delivery to each other session that sees the post (origin chat only) |
| `board_read`       | `ListBoard {chatId, since?, limit?}` (origin chat only)    |
| `wait_for_turns`   | `wait_for_turn` on up to 32 chats at once (default: the caller's working children); returns the first to settle and where the rest stand |
| `project_read`     | `GetOrchestratorProject {chatId}`: the tree's goal, instructions, paused flag and memory (any chat of the tree) |
| `project_remember` / `project_forget` | `OrchestratorMemory {chatId, text}` / `{chatId, forget}` (root only) |
| `project_update`   | `UpdateOrchestratorProject {chatId, update: {goal?, instructions?}}` (root only) |
| `merge_preview`    | `MergePreview {chatId, member, base?, targetDeviceId}`: read-only merge preview of a session's worktree branch (root only) |

**Pushed child outcomes.** `wait_for_turns` is the pull path and is unchanged.
Besides it, the engine pushes: when a tree member settles (done, failed,
needs input) it queues a batched `[Message from Zeron (automatic child-outcome
notice)...]` note for the parent, delivered as a `Run` on an idle parent or a
held queued message on one awaiting input, never to a working or paused one
(see [orchestrator projects](reference/orchestrator-projects.md)).

`create_chat`, `create_chats`, `send_message` and `send_messages` refuse from a
chat in a paused orchestrator's tree; see
[orchestrator projects](reference/orchestrator-projects.md).

### Message board

The sessions of one orchestrator tree share a board of findings, discoveries
and blockers (`zeron_proto::board`). A post's board is the author's folder
(`folder_of`: its project, so writers in their own worktrees still share one;
else its working folder), and a session sees only that board: sessions in
different directories never read or receive each other's posts. The root
reads every folder's posts and posts to one folder at a time (`board_post`'s
`session`, `PostBoard`'s `boardChatId`: a session of its tree, whose folder
gets the post); a session cannot post to another folder. Root posts from
before boards were per folder (empty `folder`) are the root's alone. A
different tree never sees them: the engine derives the tree from the calling
chat (`root_of`), never from the caller.

The session panel's Board section shows a folder's posts only while a session
of the tree in that folder is still at work (a turn, a question, a place in
the start queue, sub-agents or background jobs). Once every session there has
finished, that folder's board leaves the panel; the posts stay in the
registry and `board_read` still returns them.

`PostBoard` stores the post as a registry row (kind `boardPosts`, id an HLC,
so posts sort in order and sync to other devices; at most 200 per tree, the
oldest dropped; text 1 to 2000 characters; deleted with the root).
`board_post` then hands it on as `[Board <kind> from Zeron chat <title> (<id8>),
level N. ...]` followed by the text: a working session is steered at its next
step, one awaiting input gets it queued after that turn, an idle one is not
woken (it calls `board_read` when it next starts, as its prompt says), and an
idle root is woken only for a blocker. The result lists each recipient's
delivery. `ListBoard` returns the posts the caller sees, oldest first: the
newest `limit` (default 30, at most 100) after `since`, with `total` and
`olderSkipped`. The transcript shows a post as "Message from <title>" and
"Board <kind>: ...".

Watch streams are the engine's only read surface (there is no one-shot "get
transcript" RPC); a snapshot is "subscribe, take the first item, drop" — drop
cancels server-side, exactly what the sidebar does on attach.

`send_message` mode `auto` starts idle chats and steers working chats through
their live mailbox without interrupting tools or child processes. Providers
consume it at their next supported input boundary. Only explicit `queue` mode
creates a held queue row. `awaitingInput` refuses and points at
`respond_to_input`. An idle chat still running its own background jobs (tests,
builds, a CI watch; `backgroundJobs` above 0 in `list_chats`) refuses `auto`
too: its job results wake it, and a "continue" only repeats work. `queue` or
`run` still reach it when the caller means it. Quiet, long-running turns still receive steering; the host
falls back to starting a turn if the live runtime has already exited.

Claude uses `priority: "next"` and confirms consumption through replayed user
messages. Cursor uses the SDK's native `Run.steer`, waits for active tools to
finish, and retains input until its delivery acknowledgment. New injections do
not wait for earlier delivery acknowledgments; that wait serializes responses
even when the SDK reports a single active turn. These are mid-turn
paths; they do not terminate the harness process. Adapters that only accept input
between turns are labeled **Send next** in the composer.

The opt-in `steering_live` engine test checks a rapid burst, foreground and
background process survival, retained context, exactly-once effects, and ordered
normal queue delivery. For Claude, Cursor and Codex it additionally requires the
burst to finish within the original turn. Set `ZERON_TEST_BURST=6` to reproduce a
six-message burst; select an inexpensive model with `ZERON_TEST_MODEL` and the
harness with `ZERON_TEST_HARNESS`. Codex is checked for child survival during
the active tool: its runtime cleans up background jobs on normal tool completion
even without steering. Other providers also check a job that outlives that tool.

For conversational redirection, run `cargo run -p zeron-harness --example
cursor_steering_probe -- gemini-3-flash`. It sends six bare digits during streamed
prose and requires only the latest requested answer. Counting turn completions
alone cannot distinguish real steering from serial responses inside one run.

`wait_for_turn` after a send is edge-triggered on the `Session` row captured
before the send: it returns on a new `last_completed_turn`, an
`awaitingInput`/`errored` stamp newer than the baseline, or a working→idle
edge. A brand-new chat has no session row until the host picks the run up, so
the wait keeps waiting in that case rather than reporting the unstarted run as
done (this was the one bug the first live run found).

Status strings are `working`, `retrying (attempt n/max, next try in Ns)`,
`awaitingInput`, `errored` and `idle` (`working (stale ...)` for a row nobody
refreshed in 45 s). `retrying` is a `Working` session row that carries a
`retry` (Pi `auto_retry_start`, cleared on `auto_retry_end`, on settle and
when the run exits): the provider failed and Pi retries on its own. The wait
keeps going through it, `list_chats`/`read_chat` carry a structured `retry`
object (`attempt`, `maxAttempts`, `nextAttemptAt`, `error`; null otherwise),
and a timed-out `wait_for_turn` adds `retry` and a note so the caller does not
treat it as stalled. The `error` is provider text: data, not instructions.

### Merge preview

`merge_preview {chat, base?}` (root only) answers "what would merging this
writer's branch into the lead do?" without changing anything. The engine
resolves the lead in the session's project folder (its checked-out branch, or
`base`) and the member's live branch in its worktree, turns both into shas,
and reports:

- `ahead` / `behind`: commits only the member / only the lead has;
  `alreadyMerged` (nothing ahead) and `fastForward` (nothing behind).
- `files`: what `base...branch` changes (`A`, `M`, `D`, `R` with `oldPath`),
  capped at 200 with `filesTotal` / `filesTruncated`.
- `uncommitted`: staged / unstaged / untracked counts and the first paths in
  the member's worktree. These are NOT part of the merge.
- `conflictCheck`: `clean`, `conflicts` (paths in `conflicts`, capped at
  100), `unavailable` or `failed` (see `note`), from
  `git merge-tree --write-tree`, which needs git 2.38 or newer. On older git
  the check is `unavailable` and the rest of the preview is still accurate.
  `mergeTree` is the tree the merge would produce.

`merge-tree --write-tree` writes loose tree and blob objects to the object
database (harmless, collected by `git gc`); it never touches an index, a
working tree, HEAD or a ref. `status` runs with `--no-optional-locks`. A
member without a worktree of its own (read-only, or not started) is refused,
and a member on another device is previewed there. Applying a merge is out of
scope: the root asks the user, then briefs a write session.

### Visual replies

`show_visual` is listed only when `ZERON_CHAT_ID` is set: it shows an
interactive visual (chart, table, dashboard, diagram, simulation, 3D view)
inline in the calling chat, above the agent's final reply. A visual is a Luau
script, not a web page. The script builds its data and state once and returns a
view function; the view returns a tree of `ui.*` nodes (rows, columns, grids,
text, stats, tables, charts, tabs, toggles, sliders, buttons, links, 2D
canvases, 3D scenes, and GPU shaders). Controls call the script's handlers, and Zeron renders
the view again.

`crates/visual` (`zeron-visual`) is the runtime. Scripts run in a Luau sandbox
with only the `string`, `table`, `math`, `utf8` and `bit32` libraries (no I/O,
network, or `require`), a 64 MB memory cap, and a time budget per call (250 ms
for setup, renders and handlers, 60 ms per canvas or scene frame), so a runaway
script fails with an error instead of freezing the app. `math.random` is
reseeded from the visual's id before every call, so random data is identical on
every render and device. The style rules are the API rather than advice: colors
are theme tokens (`text`, `muted`, `faint`, `accent`, `on-accent`, `success`,
`warning`, `danger`, `border`, `chart1`…`chart6`), text sizes are steps on the
transcript's type scale, the font is always the interface font, and layout nodes
have no fill, border, shadow, padding, or font props; using one is an error that
names the rule. 3D scenes render on the GPU (`zeron-visual-gpu`: physically
based materials, shadows, glass with dispersion, glTF models, textures,
environment maps, sprites and bloom), with the runtime's software rasterizer
(`raster.rs`) as the fallback where there's no GPU: Zeron frames an orbit
camera on the geometry and draws it into one transparent image. A visual's
files (`assets`: at most 16, 8 MB each, 10 MB together) are stored beside its
script as `{id}-asset-{name}` with an `{id}-assets.json` list, and travel to
other devices like attachments. `ui.shader` takes Shadertoy-style GLSL
(`mainImage`, `iResolution`, `iTime`, `iMouse`, plus `iCamera` for an orbit
camera, named float inputs bound to the script's state, `iNoise`, and up to four
Shadertoy-style buffer passes read as `iChannel0`-`3`, with feedback from their
own previous frame); `zeron_visual::shader` wraps each pass in a GLSL 4.50
program and validates it with naga, so a compile error reaches the agent with
its pass and line. A shader composites onto the chat by `blend` (`alpha`,
`light` for emitted light where black is clear, or `opaque`) with optional
bloom, so it never sits in a black box.

The engine runs every script before showing it (`zeron_visual::check`: setup,
the first render, every canvas and scene draw, then each tab, toggle, slider
end, and button once) and returns the first error with its line, so the agent
fixes it before the reader sees anything. A passing script is stored in uploads
(`{id}-visual.luau`) and a `visual` doc part is folded into the chat's live turn
through the run's engine event channel; with no live turn on this device the
call fails and nothing is stored. The doc carries only the path and title, so
clients read the script with `ReadAttachmentChunk` like any attachment; old
clients see an empty part.

The desktop (`crates/ui/src/visuals.rs`) renders the tree with the transcript's
own GPUI elements: markdown's table treatment, the settings tab, switch, and
button idioms, canvases as GPUI paths and shaped text in theme colors, and
scenes and shaders as images. Shaders run offscreen on the GPU through wgpu
(`crates/visual-gpu`, one shared device, pipelines cached by source) and are
read back like a scene's image. Nothing foreign is hosted, so a visual scrolls, clips, blurs under
the glass composer, and takes input like any other row, identically on Linux,
macOS, and Windows, and it follows theme, appearance, and text-size changes
without rerunning the script.

## Selecting a host and project

`list_projects {device?}` filters by device id or exact name; without arguments
it lists all projects. `list_harnesses {device?}` and
`list_models {harness, device?}` query the selected host with `targetDeviceId`;
without `device` they retain the local engine catalogs.

For creation, project alone determines the host. Device alone creates a session
without a project on that device. With both, the project is resolved **within**
the selected device, and a project id belonging to another device is rejected.
Neither argument means a projectless session on the local engine. Repeated names
or paths are errors with candidate ids and devices; use ids from discovery.

Before writing, creation checks the chosen engine's harness catalog, chooses
its available default (Claude Code when available, otherwise the first available
non-mock harness), and validates any explicit model against that host's catalog.
An explicit harness must be offered, installed and enabled. Catalog failures,
including model lookup failures, are returned with the device id; there is no
fallback to the local catalog. A successful remote query checks reachability;
`lastSeenAt` alone does not establish that an engine is online.

Discover and launch a standalone session:

```text
list_devices {}
list_projects { device: "<device-id>" }
list_harnesses { device: "<device-id>" }
list_models { device: "<device-id>", harness: "codex" }
create_chat {
  kind: "chat",
  device: "<device-id>",
  project: "<project-id>",
  harness: "codex",
  model: "<model-id from that host's catalog>",
  title: "Implement feature",
  prompt: "Implement the feature...",
  wait: false
}
```

Without a project (the host uses its home directory unless `cwd` is given):

```json
{"kind":"chat", "device":"<device-id>", "harness":"codex", "prompt":"Reply with pong", "wait":true}
```

A mixed `create_chats` batch, with an origin chat or explicit top-level parent:

```json
{
  "requests": [
    {"kind":"chat", "device":"<device-id>", "project":"<project-id>", "title":"Feature", "prompt":"Implement the feature"},
    {"kind":"side", "parent":"<parent-chat-id>", "project":"<project-id>", "prompt":"Review the API"}
  ]
}
```

The first prompt and later sends use durable chat commands drained by the host.
Transcript reads target the chat's host; session status comes from the shared
registry. Sends remember the session and existing message ids on the MCP connection before
sending, so a subsequent `wait_for_turn` after `wait:false` waits for that send,
even before a session row appears. A separate MCP server has no send baseline
and reports the chat's current posture. Creation records the send time before dispatching its first
prompt. If completion arrives before the transcript, the wait allows the new
assistant response to arrive within the **same timeout**. A previous response
is never substituted for a missing response to a new send. An unfinished
transcript or missing response at the deadline returns `timedOut`.

## Parallel side chats

Use `create_chats` with a `requests` array to launch independent workers in
one tool call, including when the harness executes its tool calls sequentially:

```json
{
  "requests": [
    {"project": "/repo", "title": "Review tests", "prompt": "Review test coverage"},
    {"project": "/repo", "title": "Review API", "prompt": "Review API compatibility"}
  ]
}
```

Use `send_messages` with the same envelope for existing chats; each item uses
`send_message` arguments. Both accept 1–32 requests, run them concurrently,
and return `results` in input order with `index`, `isError`, and either `result`
or `error`. A failed request does not cancel or roll back successful requests.
Each request defaults to `wait: false`; explicit waits also run concurrently.
Only batch independent work, not ordered messages to the same chat.

When using individual tools, launch **all** chats/messages with `wait: false`
first, then collect replies with `wait_for_turn`. Waiting on each individual
launch before issuing the next serializes work at the caller, even though
both the MCP transport and engine support concurrent chat runs.

## Reserved models

Claude Fable and GPT Astra class models, any version, are reserved for
sessions the user starts, so `create_chat` (and each `create_chats` request)
refuses them: an explicit `model` that matches (`anthropic/claude-fable-5`,
`gpt-6-astra-mini`) fails with a message naming the model. Without `model` the
new session runs on Pi's own default, which Zeron cannot see, so a call whose
chat, or whose `parent`, is configured with a reserved model fails too and
asks for a `model` from `list_models`. The check runs before the host's
catalog is read. The rule lives in `zeron_proto::reserved_models`; the Pi
launch and the advisor settings enforce it as well (see `docs/pi.md`).

## Smoke recipe

```sh
BIN=target/debug/zeron
{ echo '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}'
  echo '{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"list_chats","arguments":{"limit":5}}}'
  sleep 5; } | ZERON_IPC_PORT=27655 $BIN mcp
```

`create_chat` with `"prompt": "Reply with exactly the word pong", "wait": true`
against a live daemon returns the assistant's `pong` in a few seconds; archive
the chat afterwards with `archive_chat`. Unit tests (`cargo test -p zeron-mcp`)
drive the whole tool set against an in-memory stub `RpcService`.

An isolated stdio instance is available with
`cargo run -p zeron-engine --example mcp_standalone_smoke`. It provides a temporary
project, an origin coordinator and a scripted `codex` adapter (`smoke-1`) returning
`pong`. Discover its ids with the list tools, create `kind: "chat"` with a prompt
and `wait: true`, then check `read_chat`, `send_message` and a mixed batch. The
profile is removed on exit. This checks MCP dispatch and engine execution without
changing your running app's sessions or requiring provider credentials.

The `mcp_standalone_session_executes_on_the_selected_device` test in
`device_routing` additionally runs two engines with a shared registry and device
relay, verifies execution on the selected host (and absence of execution on the
caller), and retrieves responses through MCP.
