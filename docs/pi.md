# Pi native RPC

Zeron is a front end for the Pi agent. It launches `pi --mode rpc` directly and
requires Pi 1.1.0 or newer; an older Pi, or one whose `--version` cannot be
read, is refused with a visible error. Protocol details and the acceptance
barrier are in [PROTOCOL.md](../crates/harness/src/pi/PROTOCOL.md).

## Executable

Zeron never searches PATH for Pi and never installs or updates it. The
executable comes from, in order:

1. Settings → General → Pi (per device, `piExecutable` in `harness-prefs.json`);
2. `PI_EXECUTABLE`.

With neither set, starting a chat shows "Pi is not configured" with where to set it. Install and
authenticate Pi through its own CLI. `PI_CODING_AGENT_DIR` continues to control
Pi settings, credentials and extensions. `PI_ACP_EXECUTABLE` and
`PI_ACP_PI_COMMAND` are no longer used.

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

Steering queues at a model step boundary and starts immediately when idle.
When no steering mode is configured, Zeron selects Pi's `all` mode, so messages
queued before the next model call enter that call together. A mode already set
in Pi's global or trusted project settings (including one chosen with
`/steering`) is never overwritten. Zeron confirms each original message only
when Pi consumes it. Extension commands and inputs handled without a model run
retain serialized delivery.

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
| `sheet` (ask-user, decisions, model picker, idle Pi dialogs) | replaces the composer; keyboard picks, Enter submits, Esc cancels |
| `pinned` (bash jobs, subagent fleet, workflows, todo) | collapsible trays above the todo panel, with row actions |
| `status`, `setStatus`, goal/footer channels, plan mode | chips in the composer footer |
| `screen` (subagent viewer, btw) | a right-pane tab, opened once, closed when the element goes away |
| `setWidget` | a tray of plain lines |
| `notify` (plain) | a toast while watched; transcript text otherwise |
| `setTitle` | renames the chat |
| `set_editor_text` | fills the composer |

A dialog Pi raises between turns has no turn to ask in; for a watching UI it
becomes a `pi:dialog-N` sheet (timeouts honoured), otherwise it is cancelled.

Extension messages with `display: true` (workflow results, subagent
deliveries, memory) appear in the transcript as `pi/<customType>` tool chips
with their details and text. Hidden messages, and ones arriving between turns,
stay out of the transcript (they are in Pi's history for the next turn).

Zeron's existing delegation uses a temporary MCP bridge loaded with
`--extension`. Image attachments are sent as native image blocks; image-only
tool results do not yet render inline.

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
