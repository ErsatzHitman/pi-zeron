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
read-only list). Child Pi processes inherit it through the env and the
preload. A read-only run without the guard extension fails to start rather
than run writable. `crates/harness/tests/fixtures/pi-read-only-test.mjs` is
the corpus, run on macOS and Windows CI.

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
process; model switches do not persist global defaults. Commands, prompt
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
| `status`, `setStatus`, footer channel, plan mode (Plan only; Build is the resting state and shows nothing) | chips in the composer footer (a `setStatus` line repeating an element chip shows once). pi-freeflow's relay line reads `Relay · <relay> n/m` with a globe and shows nothing while requests go direct |
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
[MCP](mcp.md)). An
orchestrator's root chat (the sidebar's Orchestrator tab) runs with its own
profile instead of the coding one:

- `--system-prompt` with `crates/proto/src/orchestrator_root.md` in place of
  Pi's coding prompt: it hands each task to a Pi session in the right folder
  through `zeron_create_chat` and follows it with the other `zeron_*` tools;
- `--tools` limited to `orchestrator::ROOT_TOOLS` (the `zeron_*` tools,
  read-only `read`/`ls`/`find`/`grep`, `ask_user`, `todo`, `goal_*` and the
  memory reads): no bash, edits or extension delegation tools;
- `--no-mcp`, `--no-skills`, `--no-prompt-templates` and
  `--no-context-files`.

A session in an orchestrator's tree keeps Pi's coding prompt and every tool,
and also gets `crates/proto/src/orchestrator_member.md`: report in the final
reply, and the tree's depth limit. The engine marks each run with
`ZERON_ROLE` (`orchestrator` or `member`, unset for a plain chat) and, for a
member, `ZERON_TREE_DEPTH`, in both the Zeron MCP server's env and Pi's own,
so extensions can change their behaviour too. The user's pi-herdr-delegate
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
