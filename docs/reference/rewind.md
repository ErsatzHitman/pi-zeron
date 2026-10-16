# Rewind

Rewind takes a chat back to before one of your messages, like Claude Code's checkpoints. You can restore the code, the conversation, or both.

## Where it appears

Hover a message you sent. The row under it shows the time, the copy action and a rewind icon. The icon opens a menu:

| Item | What it does |
| --- | --- |
| Restore code and conversation | Puts back the files the agent edited from that message on, and removes that message and everything after it. |
| Restore conversation | Removes that message and everything after it. Files stay as they are. |
| Restore code | Puts back the files. The conversation stays as it is. |

When the conversation is restored, the message goes back into the input box (after anything you had already typed there), so you can edit it and send it again.

Rewind is offered only:

- In a normal main session running Pi. It is not offered in side chats, in chats an agent or orchestrator started, in orchestrators, in subagent tabs, or with other harnesses.
- While the agent is idle. During a turn the icon is hidden.
- On messages Pi received after rewind was added. Older messages report that they can't be rewound.

## What is restored

Code: before each `edit` or `write` tool call runs, Pi's `zeron-rewind` extension saves the file as it was, under the message that started the turn. Restoring code to a message undoes every later edit made this way:

- Files the agent created are deleted.
- Files are overwritten even if you changed them by hand afterwards.

Changes made through `bash`, by other programs or by hand are not saved, so they are not undone. Snapshots live outside the repository, in `~/.pi/agent/rewind/<session id>/`. Only the newest 100 messages keep snapshots.

Conversation: Pi moves its session back to just before the message, the way `/tree` does. The later messages stay in Pi's session file on a side branch. Zeron deletes them from the transcript.

## Requirements

- The `zeron-rewind` extension in Pi's extensions folder (`~/.pi/agent/extensions/zeron-rewind/`).
- Pi running for the chat. If it is not running, send a message first, then rewind.
- The chat's host device runs the rewind. Another device asks it over the relay.

In Pi's own terminal, `/rewind` does the same: pick a message, then what to restore.

## How it works

1. When Pi receives a user message, Zeron records which Pi entry it became: the message's `timestamp`, stored as `ts:<ms>` in the chat doc's `harnessRefs` map.
2. `RewindChat { chatId, messageId, mode }` checks the chat, the device, that no turn runs and that the message has a ref. It then runs `/pi_ui_rewind <ref> <mode> <nonce>` in Pi as a side command.
3. The extension restores the files, moves the session, and answers with one `ZERON-REWIND {…}` notice. The harness passes it to the engine on the `zeron:rewind` host channel.
4. If the conversation moved, the engine deletes the message and everything after it from the chat doc.

Pi goes first, so a failure there leaves the transcript untouched.
