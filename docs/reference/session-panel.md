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
| Context | Any chat whose agent reports a context window. See below |
| Goal | Only while pi-goal has an active goal: its status (Running, Waiting, Paused, Blocked, Budget reached), the objective and its progress line. It replaces the line that used to run beside the transcript and the goal chip in the composer footer |
| Todo | Only while the agent has a checklist with work left: one row per item (done, in progress with a spinner while the turn runs, pending), and the header counts `done/total`. A list the agent has finished shows only until its turn ends. Lists over six items fold to the current item and its neighbours, with "N earlier" and "N later" rows. Read-only; it mirrors the composer's checklist tray |
| Sub-agents | See below |
| Compaction | Pi chats only, with auto-compaction on: shown once the context is within 10% of the model's window of where Pi compacts (the window less its reserve from Pi's settings), and while a compaction runs, manual ones too. A bar fills toward that point with its percent, and the caption says where it compacts, or that it is compacting with a live timer, or why the last one failed. Once shown it stays until the context is 12% away. The header's pill reads Near limit, Compacting or Failed |
| Workflow | See below |
| Background | Only while one of Pi's background bash commands runs (the `bash-background` extension moves a command there after 20 seconds): one row per command with a live timer. A Stop button sits at its right end; the first click asks, and the second sends Kill |

The essentials, from Machine to Changes, always show, and so does Context under them whenever the chat has a context window. Goal, Todo, Sub-agents, Workflow, Background and Compaction appear only while active: nothing running means none of those sections. They have no fixed order. A section that becomes active grows in at the bottom, and one whose work ends folds away while the others close up; the card does not reorder what is already showing. Sections already active when a chat opens appear at once, ordered by when their work started. A section's header folds it, and a fold lasts for the app run.

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

## Context

Under Changes, for any chat whose agent reports a context window. Folded (the default) it is one line: **Context**, its percent and a ring (the composer footer has no context ring). Click it to open the breakdown, Claude Code's `/context`:

- **Used of the window:** "7% used" and "71.3k / 1M", then a bar split by what fills it, in each part's colour, with the autocompact buffer at its end.
- **One row per part,** with its tokens and share of the window: System tools, MCP tools, System prompt, Messages, Skills, MCP server instructions, Since last update (tokens the provider has not counted yet, estimated), Autocompact buffer (kept free because Pi compacts before the window fills) and Free space. Hover a row for what it counts.
- **Deferred tools:** MCP tools and system tools the model is not sent until it looks them up, listed dimmer with no share, since they take no room.
- **MCP tools (N):** opens to each MCP server with how many of its tools the model is sent and their size.
- **When it last changed:** "Last updated just now", and while the agent works, that it refreshes when the turn ends.

The total and the window are the chat's own (the footer ring's numbers), so the two always agree; right after a compaction, Pi's estimate of what is left stands in. The split comes from Pi's `context-window` extension (`context:window` channel) and updates live as Pi publishes it, at each turn, reply, compaction, model change and MCP change, with no reload. The parts are estimated (characters ÷ 4); Messages takes whatever the measured total leaves, so the rows add up to the headline. Without the extension (or another harness) the section shows Used, the autocompact buffer when Zeron knows it, and Free space. A chat whose Pi is not running keeps its last split for the app run, with its age.

## Workflow

Pi chats only, from the `workflows` extension's `workflow:state` channel. Shown while a workflow run is in flight or pausing, while a run that stopped early (paused, interrupted when Pi quit or crashed, stopped, or failed) can be resumed, and briefly after a run ends: a run that finished (done, stopped, or interrupted with nothing to resume) clears within 5 seconds; a failure stays 2 minutes so its reason can be read. The header counts `done/total`; a running run shows only a spinner, and any other state a pill: Retrying, Paused, Done, Failed, Interrupted or Stopped.

- **The run:** its name and goal, a bar toward done with its percent, the current phase ("Verify · 2/3") with a live timer, and Pause (let running tasks finish, then pause) and Stop as glyph buttons, named in their tooltips. Stop asks on the first click ("Stop?") and acts on the second.
- **Tasks:** running, retrying and failed tasks lead, each with its state glyph. A running task shows a live timer; a retrying one, its attempt and when the next one starts; a failed one, its error. Hover a task for its full error and what it depends on.
- **Queued (N)** and **Finished (N)** fold, closed until opened, and list up to 12 tasks each, then "+N more". Finished includes results reused from an earlier run (cached), skipped and aborted tasks.
- **Notes:** how many results a resume reused or found no longer matching, and how many tasks the payload left out.
- **Recovery:** for a run that stopped early, why it stopped and **Resume** (picks up where it left off; finished work is reused) and **Discard** (stops offering it; asks first). With no run tracked, the newest interrupted run is offered the same way.
- **Waiting on the host:** while the workflows extension's resource gate holds new agents back (the machine is short of memory, swap, load or disk), a warning line under the run's name says why: which reading is short, both the measured and the threshold value, how many agents are queued behind it, and how long the wait has run. It is host state, not a failure — the run resumes on its own once the machine recovers, and Stop is always available.

A run from a workflows extension that groups by phase (`workflow:state` v2) is drawn the way Claude Code draws a workflow instead of the bar and the Queued and Finished folds:

- **Totals:** how long it has run (frozen once it ends) with the Pause and Stop glyphs, and the number of agents and their tokens ("4 agents · 170.8k tokens", retries included).
- **Phases:** every phase in the order the script started them, any number and any names; a repeated title (a second Verify round) is its own phase, and phases declared but not reached yet follow, dimmed. Each shows its title, `done/total` and a strip of dots: done, running, retrying, failed, and still to come (past 24 agents a dot stands for several).
- **Agents:** a phase opens to a table of its agents: Agent, Model, Tokens and Time (a live timer while running; "reused" for a result reused from an earlier run). Agents the phase is waiting to start show as dim rows with no model yet. Every agent the payload carries is listed — a long phase is read by scrolling the panel itself, not a nested list — and a phase whose rows Pi truncated ends with "+N more". The phase in progress is open, and the panel scrolls to it when the run moves on; click any phase to open or close it, and that choice lasts for the run. Time and Tokens always show in full: their columns take at least the width the widest value needs and scale with the UI font size, while the Agent name and the Model truncate with an ellipsis.
- A grouped run that failed stays up for 10 minutes so its table can be read; one that finished clears within 5 seconds like any other.

## btw

The Session row's `+` opens a side chat in a small floating pane (360 × 460, smaller only in a smaller window) at the window's bottom left, with its prompt bar inside it at the bottom. It floats over everything (sidebar, transcript, panes), under menus and dialogs. Drag its header to move it anywhere in the window; it stays inside the window's edges, and the next side chat opens where the last one was left. It eases in and folds away quickly; it never opens the right pane or moves the transcript.

- Its header shows the side chat's title (a spinner while it works) and two buttons: **Side chats** lists the conversation's side chats, newest activity first (click one to show it in the pane, right-click for the chat menu); **Close**. It has no expand button: the pane stays small.
- The prompt bar has no model picker: the side chat runs its parent's model, following it until its first send.
- Escape closes it while its agent is idle. A second `+` focuses the open pane.
- The pane keeps its size: the thread scrolls inside it, with faded edges once it does. A send reserves no blank space for the reply, unlike the main transcript.
- The prompt bar grows to more lines as the prompt does (no attachment button or microphone).
- The side chat sees its parent's conversation so far (a fork of its Pi session) and has only `read`, `grep`, `find`, `ls`, `web_search` and `web_fetch` (DeepSeek's Search and Fetch APIs, with the DeepSeek key from [Settings → API keys](api-keys.md)) and no MCP servers: no commands, edits, agents or sessions, and nothing it does reaches the main conversation.
- Close (the X) deletes a sent side chat: its Pi session stops and it leaves the sidebar, the task manager and the Sub-agents section. Escape only hides the pane; that side chat stays in the Side chats list. An unsent one is dropped unless it has a draft, as when its tab closes. An untitled one has no row until its first send.
- A btw side chat is never shown as a sub-agent, in this panel or nested under a session in the sidebar, and an orchestrator never sees it as a member.
- **Send to /btw**: selecting text in the conversation shows a small bar just above the selection, like Claude Code's "Send to side chat". Its button opens the pane (or brings the open one forward) with the selection quoted in the prompt, after anything already typed there, and the caret below it, ready for the question. Only the main conversation offers it, not a side chat's own transcript or a subagent tab.

Fork chat is gone. The Files panel no longer has Subagents or Chats sections; this section replaces both.

## Implementation

`crates/ui/src/shell/session_info.rs` renders the panel and holds its state (`SessionPanel`):

- the active sections (`session_info/sections.rs`, pure): which are up, in activation order, and how far each has appeared. A section enters over `motion::COLLAPSE` and leaves over `motion::MENU_OUT`, keeping its seat and last data while it folds; reduced motion snaps;
- the folds;
- the armed Stop;
- the change totals, from its own `WatchCheckoutDiffs` subscription. Frames are decoded off the UI thread, and their patches are dropped.

`session_info/todos.rs` decides when the Todo section shows; the list, its folding and the status glyphs come from `todo_panel.rs`.

`shell/sidebar_children.rs` nests side chats under their parent in the sidebar ([sidebar side chats](sidebar-side-chats.md)).

`shell/btw_pane.rs` hosts the btw pane: one at a time, scoped to its conversation, backed by an ordinary side chat (`side_chats.rs`) attached to no right-pane tab. Its transcript runs in fit mode (`Transcript::set_fit_content`: no reply runway, `content_height` for the pane to size to), and its composer hides the model picker (`Composer::set_model_picker_hidden`); `AppState::inherit_side_chat_config` keeps an unsent side chat on its parent's model. The selection bar is `Transcript::render_quote_bar`, on only for the shell's main transcript (`set_quote_actions`): it places itself from `markdown::render::selection_bounds` and emits `TranscriptEvent::SendToBtw`, which `Shell::send_to_btw` hands to `Composer::quote_into_input`.

`session_info/compact.rs` decides when Compaction shows and what it says, from the Pi harness's `zeron:compact` host channel (`CompactStatus`: on or off, the threshold, a running compaction) and the chat's context usage.

`session_info/context.rs` (pure) builds the Context rows from the chat's `ContextUsage`, the `zeron:compact` status and the `context:window` payload (`ContextBreakdown` in `zeron_proto::extension_ui`); `Shell::render_context` draws it. `session_info/workflow.rs` (pure) decides when Workflow shows and builds its rows from the `workflow:state` payload (`WorkflowState` in `zeron_proto::extension_ui`), the host resource gate (`run.gate`, v3) included; `Shell::render_workflow` draws it, and its actions are sent with `WorkflowState::send`.

`session_info/agents.rs` builds the Sub-agents rows from `AppState`, and decides when Sub-agents and Background are active:

- the transcript's spawn chips;
- the chat list;
- the chat's live extension UI: the fleet roster element, plus `startedAt` from the `subagents:fleet` channel.
