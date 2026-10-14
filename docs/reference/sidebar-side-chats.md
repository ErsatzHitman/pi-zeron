# Sidebar side chats

The sidebar's Sessions list nests each session's side chats under it, on the Orchestrator tab's tree rail.

## What nests

- Direct, unarchived children of a listed session: btw and forked side chats, and chats an agent started through the Zeron MCP server.
- The selected session's sub-agents at work: Pi fleet sub-agents and harness subagents, after its side chats, longest-running first. Only the selected session's roster and transcript are watched, so other rows keep the "● N" pill that counts theirs. A settled sub-agent leaves the list (the session panel keeps it under Previous agents).
- Not nested: background bash, grandchildren, children of archived parents, and Orchestrator members (their roots are not listed).
- Children list in the order they started (id breaks ties), so positions never jump. The session panel's list stays newest-first.
- The project filter, sort, pins and custom sections apply to the parent only; its children travel with it.

## Folding

- A parent with side chats or working sub-agents wears a chip after its title: a chevron, the count of both and, while folded, a dot in the most urgent one's status colour.
- Folded by default. A parent opens by itself, and stays open, once a child is working, asking for input or failed, or is selected or open beside it, or a sub-agent works. Clicking the chip folds or unfolds it, and that choice wins for the app run.
- Past eight children an open parent lists every child needing attention, the selected one and the one beside, then the most recently active, in start order, with a "Show N more" row. Folding resets it.

## Child rows

- Compact rows (29px), indented one level, with status glyph, harness icon, title and time; no project label, branch, pull request badge, jump chip, drag or pin. An untitled one reads "Side chat" until its first turn names it.
- A child is lit while it is selected or open beside its parent.
- Click: its parent becomes the open session, from any page, and the child opens beside it in the right pane, its composer focused.
- Cmd-click (macOS) or Ctrl-click (Windows, Linux): opens the child as the session itself.
- A sub-agent row has a running glyph (a warning triangle while it asks), a bot icon, its name and a live elapsed time; its roster detail is the tooltip. Click: a Pi sub-agent runs its roster View, a harness subagent opens its transcript tab, as from the session panel. A fleet row without View does nothing.
- Double-click renames it in place. Right-click opens the side-chat menu (Open as session, Rename, Delete). Hovering the corner archives it.

## Keyboard and drag

- Jump slots and Ctrl+Tab cycling count top-level sessions only. A selected side chat cycles from its parent.
- A dragged parent travels alone: its children hide for the drag. A drop splits on the parent row, not on its children.

## Implementation

`crates/ui/src/shell/sidebar_children.rs`: the per-frame child index, the fold rules, the capped list, the chip, the child rows and the sub-agent rows (from `session_info::sidebar_agents`, the session panel's running Sub-agents rows). `spaces.rs::render_active_rows` folds each family's height into its parent's slot (and the Pinned drag heights); `render_chat_row` takes a `SidebarRowRole`.

Tests (CI only): `cargo test -p zeron-ui sidebar_children`.
