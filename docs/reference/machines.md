# Machines

The titlebar's machine switcher drives another computer's Zeron from this window. The other machine runs the engine (and Pi) as its daemon; this window shows that machine's sidebar, chats, projects and terminals, with that machine's paths.

## The switcher

- A 24px titlebar control just before the sidebar view options ("…"), riding the sidebar's trailing edge with them. A monitor means This PC; a server glyph in the accent means another machine. Its tooltip names the machine in view.
- It shows while the sidebar has room for it beside the options past the left controls (always on Windows and Linux; on macOS from a sidebar about 26px wider than the narrowest).
- Its menu: a "Machines" heading, This PC, then each machine (its name, and its `user@host` when the name differs), the one in view checked; then "Add machine…", which opens Settings → Devices.
- The window reconnects to the machine it showed last when Zeron starts. If that machine can't be reached, the gate says why (ssh's own last line, or that no engine answers) with Retry and "Use This PC".

## Settings → Devices → Machines

The machines the switcher offers, in the order they were added. Each row has its name, its SSH destination and Remove. Add machine asks for the destination (`user@host` or an `~/.ssh/config` alias, never starting with `-` or holding spaces) and an optional name; the host is the name otherwise. The list lives in this PC's `ui-settings.json` (`machines`, `activeMachine`), never in the repository.

## How a switch works

1. The engine in view detaches like on an account change: every view and subscription of its workspace goes (chats, sessions, projects, the transcript, the queue, extension UI, subagent feeds). Leaving This PC, its engine keeps running ("parked"): its agents carry on, its IPC port keeps serving other windows, and switching back re-attaches it at once. On quit, both engines shut down.
2. For another machine, Zeron starts `ssh -N -L <free port>:127.0.0.1:27654 -- <destination>` with `BatchMode=yes` (a key or agent must answer; no prompts), `StrictHostKeyChecking=accept-new`, `ExitOnForwardFailure=yes`, `ConnectTimeout=10` and a 30s keepalive. On Windows it runs without a console, in a job object that ends it with Zeron. Zeron then attaches to the forwarded port, probing every half second for up to 20 seconds; it never starts an engine of its own for a machine.
3. A switch made while another is still opening wins; the earlier tunnel closes unused.

Pins and sidebar sections are kept per machine (`machine:<destination>`). Accounts belong to This PC's engine: signing in or out asks to switch to This PC first, and nothing ever stops another machine's daemon.

## The other machine

Install Zeron there, put `PI_EXECUTABLE` and the model keys in `~/.zeron/env`, and run `zeron daemon install` (see `docs/pi.md`, "Pi on another machine"). Keep its Zeron at this one's version: `zeron update` there.
