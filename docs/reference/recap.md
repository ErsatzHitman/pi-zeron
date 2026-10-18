# Recap

A recap is a few lines at the end of a conversation telling you where it stands after you were away: the goal, what got done, where things are now, and the next step or what is waiting on you. It looks like a dim `※ Recap` block under the last message and goes away when anything newer arrives.

## When it appears

- **Automatically**, when you come back to a conversation after at least three minutes away and the agent finished work you have not seen. "Back" means the window gets focus again, or you reopen the conversation from another one. Set `ZERON_RECAP_AWAY_SECS` to change the three minutes.
- **On demand**, with the `/recap` command in the composer. It writes a fresh recap even when one is current.

It is never written for a run in progress (a conversation waiting on your answer still gets one), for short conversations of fewer than three messages, or for side chats (`btw`) and subagent tabs. `/recap` typed in a side chat's composer recaps the main conversation, like `/rename` and `/stop`.

The close button on the block (it appears on hover or keyboard focus) dismisses it for that point in the conversation. The next piece of new work brings a new recap.

## How it is written

The host of the conversation writes it, with the model chosen under Settings > General > Thread naming (the cheapest model of the chosen agent when none is set). The conversation goes in as quoted JSON data to an isolated one-shot Pi process in an empty scratch folder: read-only, no tools, extensions, skills or project files. Claude and Codex have only a title path, so when Thread naming names one of them the recap goes to the conversation's own agent if that is Pi, else to any enabled Pi.

The last recap of each conversation (up to 200 conversations) is kept in `recaps.json` next to the other host-local stores. It is derived data: it is not in the synced registry or the conversation document, and a failed run stores nothing. The cache key is the end of the joined transcript (last entry id, entry count and the last entry's part count), so the same transcript is never summarised twice. Concurrent requests for one conversation share one run, at most two run at once, and a failed run pauses automatic requests for that conversation for ten minutes.

Dismissal is local to the device. The recap text is rendered as plain text; control characters, escape sequences and link syntax are stripped.

## Settings and compatibility

- Settings > General > Thread naming has a "Recap when you return" switch for the automatic recaps (per device, on by default). `/recap` works either way.
- A host advertises the `recap-v1` capability and serves the `RecapChat` method (`{chatId, action: "get" | "generate", force}`). A device never asks a host without it.
- Usage is attributed to `zeron-recap` on the Usage page.
