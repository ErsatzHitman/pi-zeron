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
- `zeron_wait_for_turns {chats?, timeout_secs?}`: wait on several sessions at
  once and handle whichever finishes first, instead of waiting on them one
  by one. Without `chats` it watches the caller's working children.
  A child whose provider failed and that Pi is retrying reads `retrying`
  (attempt n, next try in s): it counts as working, so the wait continues
  and the root should not treat it as stalled.

The memory and update tools work only from the root; a member reports what
should change in its final reply.

## Controls

The chat menu of an orchestrator root opens **Orchestrator settings…**:
name, goal, instructions, the memory (each note can be forgotten), and:

- **Pause**: marks the project paused, then interrupts every running or
  waiting session of the tree. While paused, `create_chat(s)` and
  `send_message(s)` from any chat of the tree fail with a message telling the
  agent to stop and say so. **Resume** lifts it.
- **Archive all**: interrupts the tree, then archives the root and every
  session it started.

A new orchestrator opens this dialog so the user can set its goal first. The
root's row in the sidebar summarises its tree, most urgent first:
`Paused · 1 needs you · 2 working · 5 sessions`.

RPC: `GetOrchestratorProject`, `UpdateOrchestratorProject`,
`OrchestratorMemory`, `ControlOrchestrator {rootId, action: pause | resume |
archive | unarchive}`.
