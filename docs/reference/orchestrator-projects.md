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
| `paused`       | — | the user only (Pause / Resume), or Zeron when the spending cap is reached |
| `pauseReason`  | one line | Zeron: why it paused the tree; empty for the user's own pause |
| `policy`       | see below | the user only (settings dialog) |
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

## Tree policy

The rules the user sets for every session of the tree. All are off by
default; the settings dialog's switches and fields edit them, and an agent
cannot (an agent that could would also be able to lift the limits it is
bound by). They are stored with the project, shown to every session in a
`## Tree policy` section of its prompt, and enforced in code:

| Rule | Enforced by |
| ---- | ----------- |
| Build and test on GitHub only (`githubOnlyTests`) | Pi's guard refuses compilers, bundlers and test runners (cargo, npm/pnpm/yarn build and test, tsc, pytest, go test, make and the like, also behind `rtk`, `timeout`, `env`, `bash -c`) in any shell tool of the tree and points the agent at `gh pr checks`. `cargo fmt`, git, gh and searches are not heavy. |
| Heavy commands at once (`heavyCommandLimit`, 1 to 8) | The same classifier. A heavy command takes a slot, a file under `tree-policy/<root>.slots` shared by every Pi of the tree on the device; it waits for one (up to 90 s), then is refused. Slots of dead processes and stale ones are reaped. |
| Writing sessions at once (`writerLimit`, 1 to 16) | The engine's admission queue: a writer over the limit queues like one over the global cap, and waits only for its own tree. |
| Allowed models (`allowedModels`) | `create_chat` refuses a model outside the list (and a start with no model); Pi's guard refuses it in `model` fields of tool calls, moves a session off one, and stops a request on one. A bare id names the model on any provider. |
| Spending cap (`spendCapCents`, a warning at `spendWarnPct`) | See below. |
| Check GitHub first (`requirePreflight`) | `create_chat` runs `github_preflight` before a writer starts in a git project and refuses on blockers. |

**Spending cap.** The spend of a tree is what the Pi sessions of its chats
spent on this device, counted from Pi's own session files (per session, so
sub-agents and extensions that name the session count too) from the moment the
cap was last changed: raising or lowering it starts a fresh budget. A watcher
checks every 30 s. At `spendWarnPct` (default 80) the orchestrator is told to
wrap up. At the cap the engine pauses the tree with a reason, which nothing in
flight survives longer than its next model request: Pi's guard stops the
request, and `create_chat`, `send_message` and their batch forms refuse. The
pause is soft: running turns end where they are rather than being interrupted
mid-tool. **Resume** is refused while the tree is still over the cap; raise the
cap first. Sessions on other devices are not counted (the numbers say so when
the tree spans devices). `GetTreeSpend {chatId}` returns the figures;
`zeron_project_read` includes them when a cap is set.

The engine keeps the policy a Pi must obey in a file per orchestrator,
`tree-policy/<root>.json` under the data folder, named in `ZERON_TREE_POLICY`
for every Pi of the tree; the guard re-reads it when it changes, so an edit
applies to running sessions. It refreshes within 30 s of a change made on
another device. Like the rest of the guard it stops mistakes, it is not a
sandbox: a script, a make target or an interpreter that runs a build is not
seen.

## Tools

- `zeron_project_read`: the project, its policy and memory, from any chat in the tree.
- `zeron_register_project {project, device?, name?, dry_run?}` and
  `zeron_github_preflight {project?, cwd?, branch?, force?}`: see
  [MCP](../mcp.md).
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
new turn; a parent waiting on an answer gets the note held until its turn
ends; a working parent gets nothing (it sees the outcome through
`zeron_wait_for_turns` / `zeron_list_chats`, which keep working). Nothing is
sent while the project is paused or to an archived parent, and a tree gets at
most 20 pushes per rolling hour. `ZERON_PARENT_NOTIFY=0` turns it off.

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

RPC: `GetOrchestratorProject`, `UpdateOrchestratorProject {chatId, update:
{goal?, instructions?, policy?}}`, `OrchestratorMemory`, `ControlOrchestrator
{rootId, action: pause | resume | archive | unarchive}`, `GetTreeSpend
{chatId}`, `RegisterProject`, `GithubPreflight`.
