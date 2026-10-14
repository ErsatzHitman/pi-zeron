# Sidebar activity

Above the session lists the sidebar shows which chats need you and which are running: a **Needs input** group (amber triangle) and a **Working** group (spinner). A group shows only while it has members; nothing asking or running means no digest.

## What counts

- **Needs input:** the chat's status is awaiting input: a harness question or approval, an unwatched Pi dialog, `ask_user`, or a blocking PIUI sheet open on the selected chat.
- **Working:** a turn is running (a send in flight counts) or the chat's subagents still run.
- Not counted: archived chats, queued or undelivered sends (their row says Queued or Failed), voice and orchestrator root chats. Done and Failed stay on the row (green check, red dot); there is no Completed group.
- Side chats list under Needs input only. Their work shows through their parent's nested rows ([sidebar side chats](sidebar-side-chats.md)).
- Working follows the project filter. Needs input ignores it, so browsing another project never hides a blocked chat; each row names its project.
- Working is left out when its only member is the selected chat, whose own row already spins.

## The digest mirrors

- Every chat keeps its own row and place below. A status change never moves a row; the digest's rows are copies.
- Rows are ordered by when their turn started (oldest first), falling back to creation time.
- Each group lists five rows, then "Show N more". Its header folds it, and the fold lasts for the app run.
- Digest rows never join the jump slots, Ctrl+Tab cycling, pins or drag. While a session drag runs the digest holds still.

## Rows

- One line: status glyph, title, the "● N" subagent pill while working, and where the chat lives (its project, or "in {parent}" for a side chat).
- Click opens the chat as its own row would: a side chat opens beside its parent. Right-click opens the chat menu.

## Row status

The session rows use the same vocabulary: an amber triangle and "Needs input" (was a blue dot and "Input"); the session panel's Session row says "Needs input" too.

## Limits

- A PIUI sheet is seen only on the selected chat (only it has a live extension UI); elsewhere such a chat reads Working.
- Background-only work (background bash, goal loops) is not in the session row, so a chat doing only that reads idle.

## Implementation

`zeron_proto::view::activity` buckets a status; `AppState::activity_chats` lists the members; `shell/sidebar_attention.rs` draws the digest and records its height so the Pinned drag maps the pointer past it.

Tests (CI only): `cargo test -p zeron-proto activity`, `cargo test -p zeron-ui sidebar_attention activity_chats`.
