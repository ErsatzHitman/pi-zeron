# Pi native RPC

Zeron is a front end for the Pi agent: Pi is the only harness it registers
(the other adapters remain in the tree for tests, and chats created with them
continue on Pi). It launches `pi --mode rpc` directly and
requires Pi 1.1.0 or newer; an older Pi, or one whose `--version` cannot be
read, is refused with a visible error. Protocol details and the acceptance
barrier are in [PROTOCOL.md](../crates/harness/src/pi/PROTOCOL.md).

## Executable

Zeron never searches PATH for Pi and never installs or updates it. The
executable comes from, in order:

1. Settings → General → Pi (per device, `piExecutable` in `harness-prefs.json`);
2. `PI_EXECUTABLE`.

A `.cmd`/`.bat` launcher (Pi run from source through Node) works; its
`--version` gets 20 seconds, even after a quicker probe elsewhere timed out.

With neither set, starting a chat shows "Pi is not configured" with where to set it. Install and
authenticate Pi through its own CLI. `PI_CODING_AGENT_DIR` continues to control
Pi settings, credentials and extensions. `PI_ACP_EXECUTABLE` and
`PI_ACP_PI_COMMAND` are no longer used.

## Delete guard

Settings → General → Pi → Block deletes (on by default, `blockDeletes` in
`harness-prefs.json`) blocks tool calls that delete: `rm`, `del`,
`Remove-Item`, `find -delete`, `git clean`/`rm`/`branch -D`/ref-deleting
pushes, `rsync --delete`, package uninstalls, docker/kubectl/terraform
deletes, SQL drops, HTTP `DELETE`, embedded `os.remove`/`fs.rmSync`/`unlink`
in `python -c`/`node -e` and heredocs, and MCP or extension tools named or
marked as deleting (`application_delete`, `trash_message`,
`destructiveHint`). Wrappers (`sudo`, `xargs`, `ssh`, `docker exec`,
`rtk`), `sh -c`, `cmd /c`, `powershell -Command`/`-EncodedCommand`, `$(…)`,
backticks and pipes into a shell are looked through. An empty `write` over a
non-empty file counts as a delete. The blocked call returns a reason that
tells the agent not to retry another way and to tell the user what it wanted
removed. The user's own `!command` is never blocked.

It is the Pi extension `crates/harness/src/pi/guard.mjs`, installed
content-addressed in `<data dir>/pi-guard/` and passed as `--extension` to
every chat, sub-chat, orchestrator root and member run (not to model probes
or title runs, which run no tools). It loads with or without the MCP bridge
and while the toggle is off, so switching it on reaches running chats: the
extension re-reads `harness-prefs.json` on each tool call (missing, corrupt
or unreadable means on). Env for the run and its children:

| Variable | Meaning |
|---|---|
| `ZERON_DELETE_GUARD_EXT` | The installed extension, for child launchers to pass on with `-e` |
| `ZERON_DELETE_GUARD_PRELOAD` | The child-side preload (`guard-preload.mjs`), installed beside it |
| `ZERON_DELETE_GUARD_PI` | Set by the extension: the Pi script the preload hands the guard to |
| `ZERON_DELETE_GUARD` | The toggle at spawn (`1`/`0`), used when there is no prefs file |
| `ZERON_DELETE_GUARD_FILE` | The live `harness-prefs.json` |
| `ZERON_DELETE_GUARD_FORCE` | `1` holds the guard on whatever the file says |

`ZERON_BLOCK_DELETES=1` on the engine (headless engines: `~/.zeron/env`)
sets the force flag and greys the switch out. Every Pi a guarded Pi starts
is guarded too, with no change to the user's Pi config: the extension adds
`--import=<guard-preload.mjs>` to its own node flags, which Pi's child
launchers (fleet subagents, loop workers, the advisor: `piInvocation`)
pass to each child, and the preload adds `--extension <guard>` to that
child's arguments when it runs the same Pi script; grandchildren inherit
the flag. The installed preload has the guard's paths (extension, prefs
file, force flag) written in, so a child that gets the node flags but not
the env, such as a Pi typed into a herdr pane or opened in a new terminal
tab by herdr-delegate, is guarded too: there the preload matches Pi's
`cli` entry script and restores the env for the child's own children. A
missing guard file stops the child. A compiled Pi binary takes
no node flags, so there the extension wraps `child_process` instead: each
spawn of the same executable (Pi's launchers start a compiled Pi that way)
gets `--extension <guard>` and inherits the env. A nested `pi --no-extensions`
and non-interactive agent CLIs (`claude -p`, `codex exec`) run from bash are
blocked, since they would run unguarded.

This is a guard against mistakes and model-driven deletes, not a sandbox:
code written to a file and run later, an unrecognised tool name, or a path
assembled at run time can still delete. Other harnesses are not covered.
The corpus in `crates/harness/tests/fixtures/pi-delete-guard-test.mjs` is
its acceptance test, and `pi-delete-guard-child-test.mjs` checks a child
Pi started the way Pi's launchers start one gets the guard; both run on
Linux, macOS and Windows CI.

## Read-only sessions

A run with sandbox `read-only` (MCP `create_chat` with `write: false`) gets
`ZERON_READ_ONLY=1`, and the same guard extension then refuses, whatever the
delete-guard switch says: `write`, `edit`, tools marked destructive or named
for a change (`delete`, `create`, `push`, ...), and bash that writes (redirects, `tee`, `sed -i`, package
installs, git commands other than read-only ones, and any command not on its
read-only list). Zeron's board and messaging tools (`zeron_board_post`,
`zeron_board_read`, `zeron_send_message(s)`) stay usable; starting sessions
does not. Research goes to sub-agents in a read-only session too, so
`subagent_start` passes while every task only reads (`writeAccess` absent or
`false`); a task with any other `writeAccess` is refused (`ro-subagent-writer`),
and the sub-agents inherit the flag. `workflow` and `workflow_script` are
refused (`ro-workflow`): workflow agents run in the Pi process with tools this
guard does not see. `web_search` and `web_fetch` stay usable too: they come from the
web-tools extension and call DeepSeek's Search and Fetch APIs with the DeepSeek key from
Settings → API keys. Child Pi processes inherit it through the env and the
preload. A read-only run without the guard extension fails to start rather
than run writable. `crates/harness/tests/fixtures/pi-read-only-test.mjs` is
the corpus, run on macOS and Windows CI.

## Sub-agents

A Pi that a Zeron run's Pi starts (a sub-agent of the user's subagent or
delegate extensions) loads the same guard extension. The guard counts Pi
levels below the Zeron session in `ZERON_SUBAGENT_DEPTH`, keyed by the Pi's pid in
`ZERON_PI_PID` so a reload keeps its level (Zeron clears both on
every run it starts) and appends a note to a sub-agent's system prompt: whose
sub-agent it is (the session's title and id, and its orchestrator and level
from the `ZERON_*` place env it inherits), that its task is the prompt it was
given and its result goes to the agent that started it. The read-only corpus
checks it.

## Updates

Zeron for Pi updates itself from this fork's GitHub releases
(`github.com/ErsatzHitman/pi-zeron/releases/latest/download`), never from
upstream Zeron's feed. Each download is checked against the release's
`manifest.json` sha256 and installs on "Restart to update" or at the next
quit. The Windows zip and installer, the macOS app and a Linux `install.sh`
install update in place; other copies (the bare Windows exe, source builds)
get a link to the release page.
`ZERON_RELEASES_URL` overrides the feed. Pi itself is never updated.

## Sessions

Sessions retain their native UUID and JSONL history. Zeron records the UUID to
absolute file mapping in `PI_CODING_AGENT_DIR/zeron-sessions` (normally
`~/.pi/agent/zeron-sessions`). For older chats it also reads
`~/.pi/pi-acp/session-map.json`, then searches native session directories,
including configured `sessionDir` locations. It validates the session header
before reopening the exact file. A session that cannot be found starts a new
conversation with a visible notice. A session that Pi has not yet written can
retain its UUID through `--session-id` only after ordered native queries prove
it has no conversation or extension entries. The proof is revoked before
another input is sent. Unsaved custom extension state cannot be recreated: Pi
only materializes it after its first assistant response, so if that file never
existed the chat continues in a new session with the same notice. Session
changes made by extensions refresh the UUID and file mapping before turn
completion.

## Models, commands, steering

Models and supported thinking levels come from native RPC discovery. The
Thinking option can disable reasoning. Unsupported saved effort levels are
clamped to a supported lower level. Discovery uses a temporary no-session
process; model switches do not persist global defaults.

That process loads Zeron's `catalog-refresh.mjs` and, before reading the
list, refreshes every provider's catalog (the pi.dev catalog for built-ins,
an extension provider's own discovery). Pi's RPC mode would otherwise answer
from the stored catalog while its own refresh runs in the background, so a
model a provider added reached the picker a start late. The first discovery
after Zeron opens forces the refresh past Pi's 4-hour freshness window, as
does a picker open. The picker shows the saved catalog at once, then swaps in
the refreshed one (`ListModels` with `wait`). Commands, prompt
templates and skills are discovered in the workspace; a command Pi advertises
shadows Zeron's built-in mapping of the same name. Pi controls
project-extension trust.

The `/` menu never starts a Pi process. It lists what the chat's own Pi
reported when it started, kept in `zeron-sessions/commands.json` so it is
there after a restart. A workspace no chat has run in yet shows the latest
list from the same Pi until its first run. Only a Pi with no list anywhere is
asked once by a no-session process.

An extension's own command (`/jev off`, `/mcp`) is a side
command: Zeron sends it straight to the chat's running Pi on the
`pi:side-command` UI action, outside any turn, even while the agent works.
It never enters the chat or its synced history; what it reports shows as
toasts (a long report as a report screen), and one that says nothing toasts
"Ran /name". With no Pi running for the chat yet, it goes as an ordinary
message, which starts one. Prompt templates and skills are always messages.
After a side command the list is fetched again.

`/reload` (keybindings, extensions, skills, prompts, themes, context files) is
Pi's terminal command; RPC mode has none. Zeron's guard extension registers it
in the session's own Pi (not its sub-agents) and runs `ctx.reload()`, so it
works like any extension command. The `/` menu lists it before Pi has.

Installed Pi packages (extensions from `pi install`) are checked for updates
with Pi's own check, at most twice a day per engine: Zeron sets
`ZERON_PACKAGE_CHECK=1` on a run, and the guard extension in that session's
Pi answers once. Any updates show as a toast in that chat, worded like the
app's update toast ("3 Pi package updates available — a, b, c · run `pi update
--extensions`"). Zeron never updates the packages itself.

A message sent while Pi works always steers: the chat's host hands every
queued text row to the running turn at once, in order (there is no setting
for it). Rows with images, and rows sent while Pi waits on a question, stay
in the queue for the turn's end; such a row offers Send now (interrupt).

Steering queues at a model step boundary and starts immediately when idle.
When no steering mode is configured, Zeron selects Pi's `all` mode, so messages
queued before the next model call enter that call together. A mode already set
in Pi's global or trusted project settings (including one chosen with
`/steering`) is never overwritten. Zeron confirms each original message only
when Pi consumes it. Extension commands and inputs handled without a model run
retain serialized delivery.

Automatic chat titles use a throwaway Pi process with no session, tools,
extensions, skills, prompt templates or context files; the title instructions
are its whole system prompt. A failure falls back to the prompt's first words.

Stop ends the turn in place: Zeron clears Pi's queues and sends `abort`, and
the Pi process keeps running with its extensions, subagents and background
shells. The next message goes to the same process. Closing the chat or quitting
terminates the owned process tree after a grace period.

## Extension UI

Pi extensions draw their UI through PIUI (the user's `~/.pi/agent/extensions/PIUI.md`):
JSON envelopes in `notify` messages prefixed `PIUI `, answered with the
`/pi_ui_event <base64url>` command. The Pi driver on the chat's host keeps the
live state; it never enters the synced chat document.

- **hello / bye.** A chat is *watched* while at least one desktop holds a
  `WatchExtensionUi` stream for it. The first watcher makes the driver send
  `hello`; the last one leaving sends `bye` after a 3 s grace, so switching
  chats or reconnecting does not bounce extensions between modes. Unwatched
  (iOS, older desktops, Zeron closed), extensions fall back to plain dialogs,
  which become Zeron questions, so nothing waits on a UI that is not there.
- **Forwarding.** Desktops reach a chat on another device through the usual
  `targetDeviceId` forwarding. Hosts advertise the `extension-ui-v1`
  capability; a desktop never watches a host without it.
- **Actions** (`ExtensionUiAction`) are sent back as PIUI `action` events.

| PIUI | Zeron |
| --- | --- |
| `sheet` (ask-user, decisions, model picker, idle Pi dialogs) | the prompt panel in the composer's place; keyboard picks, Enter submits, Esc cancels |
| `screen` (btw, subagent viewer, `/usage`, `/workspace`, `/workflow` reports, `/clinepass`, `/llama-models`, `/mcp-servers`) | the prompt panel too, drawn like the question panel: header, the body scrolling under it (btw's thread in the chat's own bubbles), and its form as the panel's input row. The X (or Esc) closes it; Esc stops a streaming btw reply first. More than three actions wrap into a toolbar |
| `pinned`, `inline` (advisor, Jev and decision cards, bash jobs, subagent fleet, workflow reports, todo), `setWidget` | the agent's own work: a `pi` tool row in the turn's transcript, like a command or a read (see below). While the element is up, its live controls (Kill a job, Stop or Pause a workflow, Cancel the advisor, Open the web curator) sit at the row's right end; one that asks first takes a second click |
| `status`, `setStatus`, footer channel, plan mode (Plan only; Build is the resting state and shows nothing) | the composer footer shows only the footer channel's speed chip, plan mode and Zeron's own notices; every other status, including workflow and other extension action chips, shows in Session Details (a `setStatus` line repeating an element chip shows once). pi-freeflow's relay line reads `Relay · <relay> n/m` with a globe and shows nothing while requests go direct. The footer channel's chip reads `42 tok/s · 87% cache · $0.042`: the session's prompt-cache hit rate (`session.cacheHitRate`) sits beside the speed, with cached and new input tokens in its tooltip |
| `zeron:compact` (Zeron's own channel, not an extension's: the harness publishes whether auto-compaction is on, where Pi compacts and a running compaction) | the session panel's Compaction section, near the limit and while compacting |
| goal channel (`pi-goal:status`) | the session panel's Goal section: status, objective and progress line. The goal's own status line is not repeated as a chip |
| subagent fleet (`subagents:fleet`) | also listed in the session panel's Sub-agents section, with a live timer per running job; a row runs View, and its Stop sends Kill after a second click. The engine also mirrors the running jobs onto the chat's session row (from the roster, or from its `piui:subagents:fleet` text mirror while no UI watches the chat), so the sidebar nests them under a session that is not open |
| background bash (`bash:jobs`, `bash-bg:fleet`) | also listed in the session panel's Background section, with a live timer per running command; its Stop sends Kill after a second click |
| workflow run state (`workflow:state`, from the `workflows` extension) | the session panel's Workflow section: the run's progress, phase, tasks by state, retries, errors, checkpoints and reused results, with Pause and Stop, and Resume or Discard for a run that stopped early. Actions go back on the `workflows:state` element with the run id as the row. A live run counts once as background work, whether the extension sends this channel, the `workflow:progress` footer line, or both |
| `notify` (plain) | during a turn, a `pi · notice` row in the transcript; between turns, a toast. A long info notice between turns (a command's report, four lines or more) opens as a report screen instead; a warning or error never does, so Pi's startup "MCP servers need attention" is a toast. Terminal escapes (colours, OSC-8 links) are stripped from notices, statuses, widgets, titles and dialog text |
| `setTitle` | renames the chat |
| `set_editor_text` | fills the composer |

**The prompt queue.** Everything that waits on the user (the agent's question,
sheets, screens) takes the composer's place one at a time. With more than one
open they queue as tabs above the panel ("2 of 3"); Ctrl+Tab and Ctrl+Shift+Tab
walk them, and a dot marks a question waiting behind another tab. A new prompt
comes to the front only over the message box; one that arrives while another
is shown waits as a tab. While only screens are open (btw, a report), their
tabs stay above the composer: clicking the shown screen's tab steps back to the
message box, and clicking it again reopens the screen. There is no separate
Message tab. Opening a chat whose screen is still up starts on the message box.

**The agent's work as rows.** The driver records what the extensions draw
while the agent works (non-prompt elements and widgets) as `pi` tool calls in
the turn: one row per element, labelled by what it is (Advisor, Jev,
Decisions, Agent, Run, Workflow, Todo, Web, Memory, Note), its lines as the
row's body. A row repaints at most once a second while its element streams
and settles (failed when the element reports an error) when the element goes
away or the turn ends. An element left up unchanged across turns (a card
until dismissed) gets no new row; one that changes does. Only the title and
up to 60 lines enter the chat document.

A tool call that Pi reports as failed can be a hold: the decisions extension
holds a launch of writing sub-agents once per intent, for one consultation, and
says so in a result starting `[decisions] HELD` (or the older
`[decisions] Pre-launch consultation`, or `[jev] ... HELD for one consult`).
The model still gets it as an error and reads the reason, but Zeron shows the
call as done with that text, not as failed. The marker is one constant in
`crates/harness/src/pi/normalize.rs`; keep it in step with the extension.
Read-only research launches are never held.

A dialog Pi raises between turns has no turn to ask in; for a watching UI it
becomes a `pi:dialog-N` sheet (timeouts honoured), otherwise it is cancelled.

Pi's elements go when its process does. What an extension sends as the turn
settles (a last notice, a title, the composer draft) outlives the process
instead, and a new process numbers its requests after them, so a toast sent at
the very end still shows once.

An `input` dialog asking for a credential (its title, message or placeholder
names an API key, password, passphrase, secret, access/auth token or bearer)
is masked: the sheet's field (`PiUiField.secret`) and, during a turn, the
question's free-text answer (`UserInputQuestion.secret`) show dots and refuse
copy and cut. PIUI forms can set `secret` on a text field themselves.

Extension messages with `display: true` (workflow results, subagent
deliveries, memory) appear in the transcript as `pi/<customType>` tool chips
with their details and text. Hidden messages, and ones arriving between turns,
stay out of the transcript (they are in Pi's history for the next turn).

Zeron's existing delegation uses a temporary MCP bridge loaded with
`--extension`. Every Pi chat also gets `--append-system-prompt` with
`crates/harness/src/pi/visuals.md`, which describes the charts, diagrams and
tables Zeron draws ([chat visuals](reference/chat-visuals.md)) and the
interactive Luau visuals Pi shows with `zeron_show_visual` (see
[MCP](mcp.md)). Every Pi run except a btw side chat also gets
`crates/harness/src/pi/host.md`: before heavy work (full test suites,
release builds, workflows, many sub-agents) the agent checks the machine's
memory, load, cgroup memory cap and GPU with shell commands, and on a busy
machine prefers CI, a narrower run or a short wait, and says what it deferred.

What a session is and what it can do is the harness's to say, not Zeron's:
Zeron publishes where a run sits as env (`ZERON_PI_MCP` with the chat id,
`ZERON_ROLE`, `ZERON_TREE_DEPTH`, `ZERON_CHAT_TITLE`, `ZERON_PARENT_CHAT_ID`,
`ZERON_PARENT_TITLE`, `ZERON_ROOT_CHAT_ID`, `ZERON_READ_ONLY`), and the Pi
harness renders it in the session's capability map, gated on the tools the
session really has: main chat, side chat, orchestrator root or member, its
parent and tree, read-only, and how Zeron behaves around it (steering, Stop,
dialogs, voice, delegate and sub-agents). Pi's own sub-agents, workflow agents
and advisor are told by Pi whose Zeron chat they work for. To tell the session
from the Pis it starts (which inherit the env), the harness claims the session
by pid in `PI_ZERON_SESSION_PID`, which Zeron clears on every run along with the
guard's `ZERON_PI_PID` and `ZERON_SUBAGENT_DEPTH`. An
orchestrator's root chat (the sidebar's Orchestrator tab) runs with its own
profile instead of the coding one:

- `--system-prompt` with `crates/proto/src/orchestrator_root.md` in place of
  Pi's coding prompt: it hands each task to a Pi session in the right folder
  through `zeron_create_chat` and follows it with the other `zeron_*` tools.
  Research is never the root's own: exploring code, docs or the web and
  investigating go to `write: false` sessions, started together when
  independent;
- `--tools` limited to `orchestrator::ROOT_TOOLS` (the `zeron_*` tools,
  read-only `read`/`ls`/`find`/`grep`, `web_search`/`web_fetch`, `ask_user`,
  `todo`, `goal_*` and the memory reads): no bash, edits or extension
  delegation tools;
- `--no-mcp`, `--no-skills`, `--no-prompt-templates` and
  `--no-context-files`.

A session in an orchestrator's tree keeps Pi's coding prompt and every tool,
and also gets `crates/proto/src/orchestrator_member.md`: report in the final
reply, the tree's depth limit and the board, and the research rule: a member
never explores the codebase, reads docs or the web or investigates in its own
context but sends it to read-only sub-agents (several at once when independent),
in a read-only session too, down to the last tree level. One named file or one
grep is a lookup, and reading the files it is about to change is its task. A
plain or side chat has no Zeron prompt of its own; it gets the rule from the Pi
capability map, gated on the sub-agent tools. The engine marks each run with
`ZERON_ROLE` (`orchestrator` or `member`, unset for a plain chat) and, for a
member, `ZERON_TREE_DEPTH`, in both the Zeron MCP server's env and Pi's own,
so extensions can change their behaviour too. A run in a tree whose orchestrator has a goal,
instructions or memory also gets them as one more `--append-system-prompt`
file (`zeron-project.md`; see
[orchestrator projects](reference/orchestrator-projects.md)). The engine hands
it over in the MCP config's env as `ZERON_PROJECT_PROMPT`, and the harness
takes it out again before the config reaches the Zeron MCP server. The user's pi-herdr-delegate
extension sees `ZERON_PI_MCP` and starts its children the same way, so they
join the tree. Image attachments are sent as native image blocks; image-only
tool results do not yet render inline.

## Screenshots

The `screenshot` extension (in the Pi config, `agent/extensions/screenshot/`)
gives the agent two tools:

- `screenshot` captures the primary monitor, another monitor (`monitor`, 0
  for all), one window by title (`window`) or a rectangle (`region`). The PNG
  goes to `<cwd>/.pi/screenshots/`, a folder that ignores itself in git. The
  agent gets it back as an image block, resized to fit the model. The user
  does not see it.
- `share_images` takes the paths the agent wants the user to see. The
  normalizer turns its result's `details.images` into one generated image per
  file, with the id `{tool id}:share{n}` (`zeron_proto::shared_image_id`).
  The engine copies each into the uploads folder, but only from inside the
  run's cwd; anything else fails the call with "Shared image unavailable".
  The extension already copies images from elsewhere into `.pi/screenshots/`
  first. In the transcript, consecutive shared images form one strip of
  thumbnails the size of a user's attachments
  (`RowKind::SharedImages`). Each opens the image viewer, and right-click
  offers the image menu.

Capture uses what the OS has:

| OS | Programs |
|---|---|
| Windows | PowerShell with System.Drawing, DPI-aware. A window is drawn with `PrintWindow`, so it can be covered. |
| macOS | `screencapture`. A window resolves to its bounds through System Events. This needs the Screen Recording and Accessibility permissions. |
| Wayland | `grim`, then `gnome-screenshot`, then `spectacle`. Capturing one window is not supported. |
| X11 | `maim`, `import` or `scrot`; `gnome-screenshot` for a whole monitor. A window is found with `xdotool`. |

A machine with no display says so instead of failing silently.

## Pi on another machine

The engine runs Pi, so Pi lives wherever the engine does. To keep Zeron's
window on one computer and Pi on another (a Linux box with the repos and
keys):

1. **On the Pi machine**, install Zeron from the release tarball
   (`install.sh`, no root), then write `~/.zeron/env` (mode 600; the systemd
   unit reads it) with `PI_EXECUTABLE=/path/to/pi` and the provider keys Pi
   needs (`OPENROUTER_API_KEY=…`). Run `zeron daemon install` (and
   `loginctl enable-linger $USER` so it starts at boot). `zeron status` should
   show the engine listening on `127.0.0.1:27654`; it stays loopback-only.
2. **On the desktop**, add the machine in Settings → Devices → Machines
   (`user@host`, the SSH key must already log in) and pick it from the
   titlebar's machine switcher (see `docs/reference/machines.md`). The
   window's sidebar, chats and projects become that machine's; This PC is
   one click back. Alternatively, `dist/windows/Zeron Remote.cmd user@host`
   (next to `zeron.exe`) forwards local port 27755 to the engine over SSH and
   opens a separate window with `ZERON_IPC_PORT=27755`, its own data dir
   (`%LOCALAPPDATA%\ZeronRemote`) and `ZERON_ATTACH_ONLY=1`: that window only
   views the remote engine, and says so rather than starting a local one when
   the tunnel is down. The normal Zeron keeps running beside it.

Projects, chats and terminals are the remote machine's; paths are its paths.
SSH (a key, over the LAN or Tailscale) is the only way in.

## Turn trace and scheduling lag

Two diagnostics for "the turn felt slow" reports. This is the first, small step:
no UI paint span and no benchmark harness yet.

**Turn trace** (opt-in). Set `ZERON_TURN_TRACE=1` (`true`, `on` and `yes` also
work) and the run driver logs one `info` line per turn on target
`zeron::turn_trace`, so also allow it in the log filter:

```sh
ZERON_TURN_TRACE=1 RUST_LOG=zeron::turn_trace=info zeron
```

```text
turn trace chat=c1 run=r7 harness=Pi status="Completed" prompt_sent_ms=0 ack_ms=Some(4) barrier_ms=Some(6) first_event_ms=Some(412) first_commit_ms=Some(470) done_ms=3180
```

All fields are milliseconds after `prompt_sent`, the moment the prompt went to
Pi. `ack` is Pi's response accepting the prompt; `barrier` is the `get_state`
response after it (the point where Pi's earlier lifecycle events are known to
have arrived); `first_event` is the first agent output the engine saw (not
session or context bookkeeping, not sub-agent traffic); `first_commit` is the
first successful write of the turn into the chat doc; `done` is the turn end. A
phase the turn never reached prints `None`.

A parked Pi session reuses one run for many turns, and each prompt (including a
queued message that starts an idle runtime) restarts the clock. A steer into a
turn already running does not. A turn the agent starts on its own has no
`prompt_sent`, so it logs nothing. Other harnesses count from the start of the
run and only report `first_event`, `first_commit` and `done`. A turn the quiesce
watchdog parks logs with `status="quiesced"`. With the flag off the cost is one
cached flag read per call.

**Scheduling lag** (always on). The engine runs a 50 ms tick on the tokio
runtime and warns on target `zeron::lag` when a tick runs 100 ms or more late,
which means something blocked a worker or starved the runtime:

```text
WARN zeron::lag: tokio scheduling lag late_ms=240 max_late_ms=240 suppressed=0
```

At most one warning is logged per 10 s; later late ticks in that window are
counted into the next line as `suppressed`, with the worst lag as `max_late_ms`.
`ZERON_LAG_WARN_MS` changes the threshold in milliseconds; `0` turns the monitor
off. The monitor wakes about 20 times a second and allocates nothing.

## Validation

```sh
cargo test -p zeron-harness --features native-fixture
cargo test -p zeron-engine --lib --test pi_resume --test acp_lifecycle --test message_queue --test e2e
cargo test -p zeron-harness --test pi_live -- --ignored --nocapture
cargo test -p zeron-ui --lib extension_ui
cargo build -p zeron-mobile --features bindgen
```

The fake Pi (`crates/harness/tests/fixtures/pi_rpc.rs`) speaks PIUI: prompts
such as `piui-idle`, `piui-btw`, `idle-dialog` and `custom` publish sheets,
trays, screens and custom messages offline. The ignored live test requires an
installed Pi and uses isolated settings and a local provider; it makes no model
API requests.

## Reserved models

Claude Fable and GPT Astra class models, any version, current or future, are
for sessions the user starts. No sub-agent, delegated session, workflow
agent, advisor or reviewer child, or session an orchestrator or another agent
launches may start on one; the user can still pick them for their own
sessions and for btw side chats. A family matches as a whole word of the
model's spec or name, case insensitive, with a version or suffix allowed after
it (`anthropic/claude-fable-5`, `gpt-6-astra-mini`), not `astral` or `fables`.
A start on one is refused with a message that says to pass another model; the
model is never swapped silently, except in the advisor's roster and failover
list, which skips reserved entries and fails only if none are left.

Zeron enforces it in:

- `zeron_proto::reserved_models` (`crates/proto/src/reserved_models.rs`): the
  rule and the refusal text, shared by the places below. Pi keeps the same
  rule in `agent/extensions/lib/reserved-models.ts`.
- `create_chat` in the MCP server (`crates/mcp/src/tools.rs`): refuses a
  reserved `model`, and refuses a call without one when the calling or parent
  chat is configured with a reserved model, because the new session would
  take Pi's own default, which Zeron cannot see.
- Settings → General → Advisor (`crates/engine/src/advisor_settings.rs`):
  `SetAdvisorSettings` refuses a reserved model or fallback. A `loop.json`
  that already names one is still read as it is, and the settings show the
  row as blocked.
- The Pi launch (`launch_refusal` in `crates/harness/src/pi/mcp.rs`, called
  from `run`): a chat in an orchestrator's tree (not a btw chat) fails to start when
  its configured model is reserved. A fork or side chat the user opens has a
  parent but no tree and is not checked. Only a model Zeron set is checked;
  Pi's `settings.json` is not read.

## You should know

Settings → General → Pi → You should know (off by default) adds a second
pair of eyes to long turns, modeled on Claude Code's "You should know".
Every 6 tool steps of a turn, Zeron asks the live Pi for the conversation
(`get_messages`) and hands it to a throwaway reviewer Pi (no session, tools,
extensions or context files; the chat's model; usage surface `zeron-ysk`)
whose whole system prompt is `crates/harness/src/pi/ysk.md`. When something
clears the bar, one note docks above the composer:

- **Heads up**: a decision the agent made without telling you.
- **You should know**: how something works that matters for the task.

Learn more opens the explanation (then Understood, or Chat in main session,
which puts the note in the composer); Knew this already silences that topic
for the chat; Dismiss closes it. A note left unanswered goes away on the
second prompt, and ignored notes back reviews off (1, 2, 4, up to 16 skipped
reviews). The agent never waits for a review, and only one runs at a time.

## Advisor

Settings → General → Advisor picks the model Pi's advisor consults when an
agent is stuck after three failed tries, and up to two fallbacks tried in
order when it is unavailable. Each row uses the composer's model picker held
to Pi's models; Pi lists them as `provider/id`, the spec the roster takes,
so a pick is saved as is. The engine writes them to `roster.advisor.model`
and `roster.advisor.failover` in `<pi agent dir>/loop.json`
(`GetAdvisorSettings` / `SetAdvisorSettings`), keeping the advisor's
`thinking` and every other key, and the advisor reads the file on every
consultation. Reset on the model row removes both keys, so Pi's default
roster (Claude Opus 5, with its OpenCode fallback) applies again. A
`loop.json` that is not valid JSON is never overwritten: saving fails until
it is fixed or removed. A reserved model (see Reserved models) is refused as
the advisor or a fallback.

## Default subagent

Settings → General → Default subagent picks the agent every child an agent
starts runs on when the call names none of its own: `subagent_start` tasks,
dynamic-workflow agents (an `agent()` with neither `model` nor `tier`),
delegated sessions, and Pi sessions an orchestrator or agent creates with
`zeron_create_chat` without a `model` (when the device offers that model;
`reasoning` falls back to the thinking row the same way). It applies in every session: a plain chat, an
orchestrator's root, its members and their sub-sessions. The model row uses
the composer's model picker held to Pi; the thinking row offers Pi's levels or
the model's own default. The engine writes `defaultModel` and
`defaultThinkingLevel` in `<pi agent dir>/subagents.json`
(`GetSubagentSettings` / `SetSubagentSettings`), keeping every other key, and
the children read the file at each launch, so running chats pick a change up
at their next child. A model or level the call names still wins, and so does a
subagent profile's own model. Reset removes the model, and children run on
their session's model again. A reserved model is refused.

Once the model is set, two fallback rows appear (`fallbackModels` in the same
file, each a model and an optional level; without one it runs at the default's
level). A child on the default starts on the first of the default and its
fallbacks that can run: a reserved model, or one on a subscription Settings →
Usage marks almost used up, is skipped. A `subagent_start` child that fails
before it runs a single tool (an unknown model, a provider error, no key) is
started again on the next fallback, and its result says so (`- Fallback:`). A
workflow agent on the default whose retries all fail runs on each fallback in
turn before any tier escalation. A delegated session starts on the first one
that can run, and `zeron_create_chat` on the first one the device offers. A
fallback that repeats the default or an earlier one is dropped; Reset on a
fallback moves the next one up, and Reset on the model clears both.

## Decision gate

Settings → General → Decision gate picks the decision model Pi's decisions
gate asks before a plan with a writer launches. Jev (`typesafe/jev-1.13`) runs
on OpenRouter's decisions endpoint with the OpenRouter key; SubQ Snap
(`subq-snap-preview`) runs on SubQ's Decisions API
(`https://api.subq.ai/v1/systemone`) with the active SubQ key from Settings →
API keys, or `SUBQ_API_KEY`. Both take the same System One request and return
the same answers, so the gate decides the same way with either. The engine
writes `{ "model": "jev" | "snap" }` to `<pi agent dir>/decisions-model.json`
(`GetDecisionSettings` / `SetDecisionSettings`); the gate reads it at every
consult, so a change applies to the next one. The card warns when Snap is
chosen and the store holds no SubQ key: the gate then lets launches through
without a consult. `/decisions model [jev|snap]` sets the same file, and
`DECISIONS_MODEL` overrides it.
