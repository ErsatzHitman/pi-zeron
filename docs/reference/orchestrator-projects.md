# Orchestrator projects

An orchestrator (the sidebar's Orchestrator tab) is a long-lived project, not
a single chat. Each root carries settings that every session of its tree
sees, a memory that outlasts conversations and compactions, and controls that
act on the whole tree at once. The ideas come from Claude Projects (project
instructions and memory shared by every chat in the project) and Cursor
(project rules applied to every agent, background agents you can stop and
review from one place).

## What a project holds

`zeron_proto::orchestrator_project`, stored in the registry so it syncs to the
user's other devices:

| Field          | Limit | Who changes it |
| -------------- | ----- | -------------- |
| `goal`         | one line, 300 characters | the user (settings dialog), or the root through `project_update` when the user asks |
| `instructions` | 6000 characters | same as the goal |
| `paused`       | — | the user only (Pause / Resume) |
| memory notes   | 40 notes of at most 500 characters; the oldest drop first | the root (`project_remember`, `project_forget`) and the user (forget) |

Registry kinds: `orchestratorProjects` (one row per root, id = root chat id)
and `orchestratorMemory` (one row per note, id an HLC so notes sort by time).
Deleting the root deletes both, along with every member chat.

## How sessions see it

When the engine starts a Pi run in a tree, it renders the project
(`OrchestratorProject::prompt`) as a `# Project` section with the goal, the
standing instructions and the newest memory notes that fit in 8 KiB. The Pi
harness appends it to the run's system prompt (`zeron-project.md`), after the
root's or member's own prompt. The section is fixed when Pi starts, so a
session that runs for a long time calls `zeron_project_read` for the current
values; the root prompt tells the orchestrator to do so at the start of every
turn.

## Tools

- `zeron_project_read`: the project and its memory, from any chat in the tree.
- `zeron_project_remember {text}`: save a durable fact (a decision, a
  convention, where something lives). An identical note is not saved twice.
- `zeron_project_forget {id}`: drop a note that is wrong or done.
- `zeron_project_update {goal?, instructions?}`: only when the user asks.
- `zeron_merge_preview {chat, base?}`: root only, read-only. Files, ahead/behind,
  uncommitted edits and predicted conflicts for merging a writer's branch
  (see [MCP](../mcp.md)).
- `zeron_wait_for_turns {chats?, timeout_secs?}`: wait on several sessions at
  once and handle whichever finishes first, instead of waiting on them one
  by one. Without `chats` it watches the caller's working children.
  A child whose provider failed and that Pi is retrying reads `retrying`
  (attempt n, next try in s): it counts as working, so the wait continues
  and the root should not treat it as stalled.

### Pushed child outcomes

When a session of the tree finishes a turn, fails or asks a question, its
parent is told automatically with a short note (child id, title, status, the
tail of its last reply, and its pending question). Outcomes arriving within
about 2 seconds share one note. An idle (or errored) parent is woken with a
new turn. A working parent gets the note through its message queue, like a
message the user typed: a steerable Pi turn takes it at its next step, any
other turn starts it when the turn ends. A parent waiting on an answer gets
the note queued behind the question. A root the user stopped is not restarted:
the note waits in its queue until the user sends something. The note can
repeat a child the parent already took from `zeron_wait_for_turns`, which the
note tells it to ignore.

Delivery is at least once. A turn counts as announced only after the note
reached the parent; a failed hand-off is retried (2, 4, 8, 16, then 32 seconds)
and, when the 20-per-hour budget is spent or a note is too long to name every
child, the remaining children go out in a later note instead of being
dropped. Nothing is sent while the project is paused or to an archived parent,
and a tree gets at most 20 pushes per rolling hour. The batch window and retry
state live in memory, so an engine restart forgets them.
`ZERON_PARENT_NOTIFY=0` turns it off.

The memory and update tools work only from the root; a member reports what
should change in its final reply.

## Controls

The chat menu of an orchestrator root opens **Orchestrator settings…**:
name, goal, instructions, the memory (each note can be forgotten), and:

- **Pause**: marks the project paused, then interrupts every running or
  waiting session of the tree. While paused, `create_chat(s)` and
  `send_message(s)` from any chat of the tree fail with a message telling the
  agent to stop and say so. **Resume** lifts it.
- **Stop all**: stops the whole tree for good. A Stop only ends a turn; a Pi
  runtime outlives it with its background shells and sub-agents, so this tears
  every session's Pi process down together with those, clears every queued
  message, and holds the tree (the pause flag) so nothing restarts or wakes by
  itself: not the orchestrator's tools, not an outcome note, not a revival
  after a crash. The toast says what stopped and what is still running.
  Sessions on other devices are stopped on their devices. **Resume** lifts the
  hold and restarts nothing; a stopped session picks its conversation up from
  its session file on its next message. The orchestrator can do the same for
  its own tree or one branch with `zeron_stop_tree`.
- **Archive all**: interrupts the tree, then archives the root and every
  session it started.

A new orchestrator opens this dialog so the user can set its goal first. The
root's row in the sidebar summarises its tree, most urgent first:
`Paused · 1 needs you · 2 working · 5 sessions`.

RPC: `GetOrchestratorProject`, `UpdateOrchestratorProject`,
`OrchestratorMemory`, `ControlOrchestrator {rootId, action: pause | resume |
archive | unarchive}`, `StopTree {chatId, scope: tree | subtree, hold, only?,
caller?}` (device-addressable; reports `stopped`, `alreadyIdle`,
`queuedCleared`, `held`, `remaining` and `remote`).

After an engine crash or reboot, a tree member whose turn was interrupted is
continued, never replayed: its next run starts from a recovery notice that
quotes the interrupted request and tells it to check the repository state and
not to repeat finished steps. A held tree stays down, and a member that is not
resumed fails and tells its parent.
