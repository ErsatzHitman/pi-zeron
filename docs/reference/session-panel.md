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
| Session | The agent's mark and the chat's title. While the agent works it shows a spinner and a live timer; "Needs you" while it waits on the user; "Error" after a failure; otherwise the model |
| Branch | The conversation's branch |
| Changes | The working tree's `+added −removed` line totals ("None" when clean). Clicking it opens the chat's diff tab, or a new one if none is open |
| Goal | Only while pi-goal has an active goal: its status (Running, Waiting, Paused, Blocked, Budget reached), the objective and its progress line. It replaces the line that used to run beside the transcript and the goal chip in the composer footer |
| Todo | Only while the agent has a checklist with work left: one row per item (done, in progress with a spinner while the turn runs, pending), and the header counts `done/total`. A list the agent has finished shows only until its turn ends. Lists over six items fold to the current item and its neighbours, with "N earlier" and "N later" rows. Read-only; it mirrors the composer's checklist tray |
| Sub-agents | See below |
| Background | Only while Pi has bash commands in the background (the `bash-background` extension moves a command there after 20 seconds): one row per command with a live timer. Its Stop appears on hover; the first click asks, and the second sends Kill |

The essentials, from Machine to Changes, always show. Goal, Todo, Sub-agents and Background are sections whose headers fold them. A fold lasts for the app run.

## Sub-agents

The header counts what is running and has a `+` button, "btw", that starts a new side chat. An untitled side chat is called "btw" until its first turn names it. Rows, in order:

1. **Sub-sessions:**
   - the chat this one hangs off, marked "Parent", which opens it;
   - this chat's side chats (forks, and chats an agent started through the Zeron MCP server), newest first. A side chat's row shows its pull request badge and when it was last active.
   - Clicking a side chat opens it in the right pane. Double-clicking renames it in place, and right-clicking opens the chat menu.
2. **Running sub-agents:**
   - Pi's sub-agent fleet (the `subagents` extension). Each row has a live timer. Clicking it runs the roster's View. Its Stop appears on hover; the first click asks, and the second sends Kill.
   - Subagents a harness spawned (the transcript's spawn chips). Clicking one opens its transcript.
3. **Previous agents (N):** finished and failed sub-agents. This group is closed until opened.

Fork chat is gone. The Files panel no longer has Subagents or Chats sections; this section replaces both.

## Implementation

`crates/ui/src/shell/session_info.rs` renders the panel and holds its state (`SessionPanel`):

- the folds;
- the armed Stop;
- the change totals, from its own `WatchCheckoutDiffs` subscription. Frames are decoded off the UI thread, and their patches are dropped.

`session_info/todos.rs` decides when the Todo section shows; the list, its folding and the status glyphs come from `todo_panel.rs`.

`session_info/agents.rs` builds the Sub-agents rows from `AppState`:

- the transcript's spawn chips;
- the chat list;
- the chat's live extension UI: the fleet roster element, plus `startedAt` from the `subagents:fleet` channel.
