Pi RPC compatibility contract

The native driver targets Pi >= 1.1.0 and refuses anything older (or a Pi whose
`--version` cannot be read) with a visible error. There is no compatibility
path for older releases. Requests and events are JSONL, never JSON-RPC.
Responses and lifecycle events must retain stdout order.

Executable: an explicit path, else the device setting (Settings → General →
Pi, persisted in `harness-prefs.json` as `piExecutable`), else
`PI_EXECUTABLE`. Zeron never searches PATH for Pi and never updates Pi.

Delete guard: every chat run gets `--extension <data dir>/pi-guard/zeron-delete-guard-<hash>.mjs`
(`guard.mjs`), which blocks deleting tool calls through `tool_call`. It reads
`blockDeletes` from `harness-prefs.json` on each call (absent means on); see
the Delete guard section of `docs/pi.md` for its env.

`prompt` success acknowledges preflight with `data.disposition`
(`started` | `queued` | `handled`). Normal runs finish at `agent_settled`
(which carries `aborted`), not `agent_end`; a final error message determines
the terminal status after retries (`auto_retry_start`/`auto_retry_end` frame
them; the runner also emits `AgentEvent::Retrying`, which the engine
publishes as the `retry` field of the session row while its status stays
Working). Extension commands and input handlers acknowledge `handled` without a
run; their handler is awaited before the ACK, so dialogs or a run it starts
precede it on stdout.

ACK barrier (verified against Pi 1.1.0 and main, `agent-session.ts`): after
prompt acceptance request `get_state`. AgentSession's `isStreaming` is
`_isAgentRunActive`, covering retries/post-run continuations. `_startRun` marks
the run active before preparing it and before the `started` preflight callback,
so `isStreaming` is already true when the ACK is written and before another
stdin command can execute. A false isStreaming AND false isCompacting at this
ordered barrier closes a handled prompt (or a run already settled). This is
NOT polling for idle or a quiet timeout. Process all preceding events before
the response, and scope barriers to the submitted execution. `disposition`
alone is not enough: the state barrier also catches extension-started work,
which can accompany a `handled` disposition.

Pi reads stdin lines concurrently (`void handleInputLine`), so correlation is
by id, never by order. Zeron never pipelines dependent commands: each barrier
is sent only after the ACK it follows.

Independent work started later by an extension is a new run (`agent_start`).
Discovery/startup timeouts diagnose startup only; no active model run is timed
out for lack of output. Interruption clears queues before aborting; Pi answers
`abort` only after the run has settled.

Steering uses `prompt` with `streamingBehavior: "steer"`. `set_steering_mode`
persists globally and is not validated by Pi, so Zeron only ever sends `all`
or `one-at-a-time`, and selects `all` only while neither the global nor a
trusted project's settings configure a mode. Pi loads `<cwd>/.pi/settings.json`
only for a trusted project (`<agent dir>/trust.json`, nearest explicit
decision); RPC mode never prompts, so an undecided project is untrusted.
Preflight/state barriers remain serialized, but consumption does not gate the
next submission after `queue_update` confirms an appended steering entry.
Keep one FIFO delivery record per original input, including duplicate text.
`message_start` for each consumed user input emits its own `Steered`; neither
the prompt ACK nor `queue_update` is a consumption receipt. Extension commands
wait for earlier deliveries, and unqueued/handled inputs block pipelining until
consumed or settled. An idle barrier must not confirm queued inputs that never
produced their consumption events.

Every command Pi advertises (extension, prompt template, skill) shadows Zeron's
built-in mapping of the same name (`/session`, `/compact`, ...). Commands named
`pi_ui_*` are GUI transport (PIUI) and are hidden from the palette.

Dialogs (`select`, `confirm`, `input`, `editor`) are answered with
`extension_ui_response`; one still open when a turn ends or is interrupted is
answered `cancelled: true` (Pi's `editor` has no timeout and would otherwise
stall `abort`).

Frames: Pi writes `JSON + "\n"` with no size limit. Zeron splits on LF only and
skips (with a warning) any single frame over 32 MiB rather than failing the
session; such frames are `agent_end`/`message_end` replays whose content has
already streamed.

Tools: `edit` takes `{path, edits: [{oldText, newText}]}`; the transcript diff
joins the edits in order. `bash` and `powershell` are both shell execution.

Sources inspected: the Pi checkout at v1.1.0 plus main (`a1df620`):
`packages/coding-agent/src/modes/rpc/rpc-mode.ts`, `rpc-types.ts`,
`src/core/agent-session.ts`, `src/core/trust-manager.ts`,
`src/core/tools/edit.ts`, and `docs/rpc*.md`.

Session identity is refreshed from every accepted state barrier, including
extension-driven new/switch/fork operations (Pi emits no session-switch event).
Since 0.99 Pi creates the session file at the first user message. Only a
host-owned empty-session record, checked with get_entries (model/thinking
changes only) and a second idle get_state, permits recreation using
--session-id in the same cwd. Any submitted input revokes that proof before the
write to stdin. Missing history/custom entries never authorize recreating the
same UUID; an unrestorable session starts a new one with a visible "without the
previous context" notice, like the other harnesses.

Command completions (`catalog.rs`): `get_commands` carries no argument
completions. When it lists `pi_ui_completions` (the user extension
`00-zeron-completions.ts`), the driver sends the prompt `/pi_ui_completions`
after `get_commands` at bootstrap, in discovery, and before the list is fetched
again after a turn's slash command (the turn's barrier waits for it). The
extension answers with one notify `ZERON-COMPLETIONS {v:1, commands:{name:
[{value, label, description?, children?}]}}` (two levels of
`getArgumentCompletions`), which never reaches the transcript; it rides each
`SlashCommand.arguments`. Without the extension commands have no arguments.

Catalog refresh (`catalog.rs`, `catalog-refresh.mjs`): only the model
discovery process loads the extension. When `get_commands` lists
`pi_ui_refresh_models`, the driver sends the prompt `/pi_ui_refresh_models`
(`/pi_ui_refresh_models force` on Zeron's first discovery or a forced one)
before `get_available_models`. The handler awaits
`modelRegistry.refresh({allowNetwork, force})` (20 s cap; no network under
`PI_OFFLINE`) and Pi acknowledges `handled` once it returns. A failed or slow
refresh leaves the stored catalog.

PIUI (extension UI, `piui.rs`; spec: the user's `~/.pi/agent/extensions/PIUI.md`)

Transport: extensions publish `notify` messages whose text is `PIUI ` + JSON
envelope `{v:1, agentSeq, instance, ...op}`; large envelopes arrive as `chunk`
ops (`id`, `i`, `n`, `data`) and incomplete sets are dropped after 60 s (a
snapshot repairs state). The client answers by sending the prompt
`/pi_ui_event <base64url JSON>` (an extension command: it acknowledges
`handled` and never starts a run). Client events are
`{type:"hello", protocol:1, client:"zeron"}`, `{type:"bye"}` and
`{type:"action", elementId, actionId, value?, rowId?}`; a form's submit value
is `{fields: {id: value}}`. A new `instance` (the Pi runtime restarted) clears
all state; an envelope whose `agentSeq` is not newer than the last is dropped.

Gating: the driver says `hello` only while its chat is watched (at least one
`WatchExtensionUi` stream on the host; the engine's `ExtensionUiHub` releases a
chat 3 s after its last watcher) and `bye` when it stops being watched.
Before `hello` and after `bye`, extensions use plain
dialogs/`setWidget`/`setStatus`, so an unwatched chat never waits on PIUI. On
`bye` Pi's live elements are dropped, but an open dialog sheet stays: the
agent is waiting on that question, so leaving the chat must not answer it.
The next watcher finds the sheet in the state, and only the dialog's own
timeout or the turn's abort closes it.

Ops: `set` (whole element, keyed `ns:id`), `patch` (merge into an element),
`remove`, `reset` (everything), `channel` (keyed payload, e.g.
`minimal-status:footer`, `pi-goal:status`, `subagents:fleet`, `bash-bg:fleet`,
`workflow:progress`). A removed element with `durable: true` is kept (last 16)
in the live state only.

Plain RPC UI requests are folded into the same live state: `notify` (a notice;
also transcript text during a turn when no UI watches), `setStatus`,
`setWidget`, `setTitle`, `set_editor_text`. A blocking dialog with no turn to
answer it in becomes a `pi:dialog-N` sheet element for a watching UI (its
timeout honoured); with no watcher it is cancelled.

Custom messages: `message_end` with `message.role == "custom"` and
`display: true` maps to a `pi`/`<customType>` tool call with `details` as input
and the text content as output. `display: false` messages are dropped, and so
is any custom message arriving while no turn is active.

Stop: `stops_turn_in_place` — interrupt sends `clear_queue` then `abort`,
cancels open dialogs and keeps the process, its extensions and background work
alive for the next message.
