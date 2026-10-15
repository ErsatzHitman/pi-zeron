# Session panel

The session panel is the card at the right of a chat's transcript. It shows where the chat runs and what is working under it. Its layout follows T3 Code's thread details card, drawn with Zeron's own rows, icons and colours.

## Showing it

- **The window has room:** the panel is always docked beside the transcript, and the transcript and composer shift left as needed to clear it. It has no hide toggle.
- **The window is too narrow:** the panel steps aside, and a toggle appears in the titlebar. Pressing it floats the panel over the transcript. Pressing outside the panel, or choosing one of its rows, closes it again.

A setting saved by an older build that hid the panel is ignored.

## What it shows

| Part | Contents |
| --- | --- |
| Machine | The device the chat runs on, with a warning mark while it is offline |
| Project | The project's icon and name; hover for its path |
| Directory | The working directory, shortened to its last two folders; hover for the full path. A worktree says so |
| Session | The agent's mark and the chat's title. While the agent works it shows a spinner and a live timer; "Needs input" while it waits on the user; "Error" after a failure; otherwise the model. Its `+` button, "btw", starts a side chat |
| Branch | The conversation's branch |
| Changes | The working tree's `+added −removed` line totals ("None" when clean). Clicking it opens the chat's diff tab, or a new one if none is open |
| Goal | Only while pi-goal has an active goal: its status (Running, Waiting, Paused, Blocked, Budget reached), the objective and its progress line. It replaces the line that used to run beside the transcript and the goal chip in the composer footer |
| Todo | Only while the agent has a checklist with work left: one row per item (done, in progress with a spinner while the turn runs, pending), and the header counts `done/total`. A list the agent has finished shows only until its turn ends. Lists over six items fold to the current item and its neighbours, with "N earlier" and "N later" rows. Read-only; it mirrors the composer's checklist tray |
| Sub-agents | See below |
| Compaction | Pi chats only, with auto-compaction on: shown once the context is within 10% of the model's window of where Pi compacts (the window less its reserve from Pi's settings), and while a compaction runs, manual ones too. A bar fills toward that point with its percent, and the caption says where it compacts, or that it is compacting with a live timer, or why the last one failed. Once shown it stays until the context is 12% away. The header's pill reads Near limit, Compacting or Failed |
| Workflow | See below |
| Background | Only while one of Pi's background bash commands runs (the `bash-background` extension moves a command there after 20 seconds): one row per command with a live timer. A Stop button sits at its right end; the first click asks, and the second sends Kill |

The essentials, from Machine to Changes, always show. Goal, Todo, Sub-agents, Workflow, Background and Compaction appear only while active: nothing running means no sections. They have no fixed order. A section that becomes active grows in at the bottom, and one whose work ends folds away while the others close up; the card does not reorder what is already showing. Sections already active when a chat opens appear at once, ordered by when their work started. A section's header folds it, and a fold lasts for the app run.

## Sub-agents

Shown while a sub-agent or side chat runs, or when the chat has a parent (the Parent row is its way back). Renaming a side chat from its tab menu shows it too. The header counts what is running. An untitled side chat is called "btw" until its first turn names it. Rows, in order:

1. **Sub-sessions:**
   - the chat this one hangs off, marked "Parent", which opens it;
   - this chat's side chats (forks, and chats an agent started through the Zeron MCP server), newest first. A side chat's row shows its pull request badge and when it was last active.
   - Clicking a side chat opens it in the right pane. Double-clicking renames it in place, and right-clicking opens the chat menu.
2. **Running sub-agents:**
   - Pi's sub-agent fleet (the `subagents` extension). Each row has a live timer. Clicking it runs the roster's View. A Stop button sits at its right end; the first click asks, and the second sends Kill.
   - Subagents a harness spawned (the transcript's spawn chips). Clicking one opens its transcript.
3. **Previous agents (N):** finished and failed sub-agents. This group is closed until opened.

The sidebar nests the same side chats under their parent session ([sidebar side chats](sidebar-side-chats.md)).

## Workflow

Pi chats only, from the `workflows` extension's `workflow:state` channel. Shown while a workflow run is in flight or pausing, while a run that stopped early (paused, interrupted when Pi quit or crashed, stopped, or failed) can be resumed, and briefly after a run ends: 20 seconds, or 2 minutes for a failure. The header counts `done/total`, and its pill reads Running (with a spinner), Retrying, Paused, Done, Failed, Interrupted or Stopped.

- **The run:** its name and goal, a bar toward done with its percent, the current phase ("Verify · 2/3") with a live timer, and **Pause** (let running tasks finish, then pause) and **Stop**. Stop asks on the first click and acts on the second.
- **Tasks:** running, retrying and failed tasks lead, each with its state glyph. A running task shows a live timer; a retrying one, its attempt and when the next one starts; a failed one, its error. Hover a task for its full error and what it depends on.
- **Queued (N)** and **Finished (N)** fold, closed until opened, and list up to 12 tasks each, then "+N more". Finished includes results reused from an earlier run (cached), skipped and aborted tasks.
- **Durability:** the last checkpoint ("Checkpoint 2m ago · 7 done", "Resumed 10s ago"), how many results a resume reused or found no longer matching, and how many tasks the payload left out.
- **Recovery:** for a run that stopped early, why it stopped and **Resume** (picks up where it left off; finished work is reused) and **Discard** (stops offering it; asks first). With no run tracked, the newest interrupted run is offered the same way.

## btw

The Session row's `+` opens a side chat in a small pane: the card's width, beside the card on its left (in the card's place while the card is away), with its own prompt bar below. It eases in and folds away quickly; it never opens the right pane. While the window has room the transcript shifts further left to clear it, as far as it can without narrowing; past that the pane floats over the transcript.

- Its header shows the side chat's title (a spinner while it works) and three buttons: **Side chats** lists the conversation's side chats, newest activity first (click one to show it in the pane, right-click for the chat menu); **Open as tab** moves the chat to a right-pane tab; **Close**.
- Escape closes it while its agent is idle. A second `+` focuses the open pane.
- The pane opens at the card's height and keeps it while the card's sections come and go.
- A closed pane's side chat is dropped unless it has an unsent draft, as when its tab closes. An untitled one has no row until its first send.
- Floating the card over a narrow column closes the pane.

Fork chat is gone. The Files panel no longer has Subagents or Chats sections; this section replaces both.

## Implementation

`crates/ui/src/shell/session_info.rs` renders the panel and holds its state (`SessionPanel`):

- the active sections (`session_info/sections.rs`, pure): which are up, in activation order, and how far each has appeared. A section enters over `motion::COLLAPSE` and leaves over `motion::MENU_OUT`, keeping its seat and last data while it folds; reduced motion snaps;
- the folds;
- the armed Stop;
- the change totals, from its own `WatchCheckoutDiffs` subscription. Frames are decoded off the UI thread, and their patches are dropped.

`session_info/todos.rs` decides when the Todo section shows; the list, its folding and the status glyphs come from `todo_panel.rs`.

`shell/sidebar_children.rs` nests side chats under their parent in the sidebar ([sidebar side chats](sidebar-side-chats.md)).

`shell/btw_pane.rs` hosts the btw pane: one at a time, scoped to its conversation, backed by an ordinary side chat (`side_chats.rs`) attached to no right-pane tab.

`session_info/compact.rs` decides when Compaction shows and what it says, from the Pi harness's `zeron:compact` host channel (`CompactStatus`: on or off, the threshold, a running compaction) and the chat's context usage.

`session_info/workflow.rs` (pure) decides when Workflow shows and builds its rows from the `workflow:state` payload (`WorkflowState` in `zeron_proto::extension_ui`); `Shell::render_workflow` draws it, and its actions are sent with `WorkflowState::send`.

`session_info/agents.rs` builds the Sub-agents rows from `AppState`, and decides when Sub-agents and Background are active:

- the transcript's spawn chips;
- the chat list;
- the chat's live extension UI: the fleet roster element, plus `startedAt` from the `subagents:fleet` channel.
