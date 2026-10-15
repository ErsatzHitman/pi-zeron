# Pi dynamic workflows: research and design

Status: research complete; this is the design the implementation follows.
Scope: the Pi `workflows` extension (`~/.pi/agent/extensions/workflows`) and its
native surface in Pi-Zeron's session panel.

## TL;DR

- Pi already has a capable workflow runtime: an LLM-written JS script runs in
  `node:vm` behind an Acorn gate and fans out `pi --mode rpc --no-session`
  children, with a write-through journal per run. What it lacks is
  **recovery**, not orchestration.
- Resume is **purely positional**. The first non-done agent becomes the
  frontier, and every completed record after it is thrown away. One failed
  sibling in a `parallel()` re-bills every sibling that finished after it.
  A changed prompt marks the run `divergent` and blocks resume forever.
  `dag()` is documented as not resume-safe.
- Nothing notices a crashed or quit run. `session_start` does no recovery,
  `session_shutdown` writes `aborted` (indistinguishable from a user Stop), and
  a hard kill leaves `running` plus a stale lock.
- The live channel (`workflow:progress`) carries counts only, and Zeron has no
  typed view of it.
- The plan: content-keyed reuse on top of the positional cache, a soft
  divergence downgrade, `interrupted` runs with a startup reaper, typed
  retry/cached/invalidated events, a versioned `workflow:state` channel, and a
  native **Workflow** section in the session panel. No new RPC, capability or
  Pi core patch.

## Method

Nine agents ran as one research workflow: six non-overlapping researchers
(Claude Code's workflows, the Pi workflows extension, Pi core extension APIs,
Zeron's ingestion and panel, durable-execution frameworks, the Pi agent
ecosystem), two verifiers that re-read every load-bearing claim against source,
and a completeness critic that produced the failure matrix. All nine ran on
Sonnet 5.5 (`claude-sonnet-5-5`) at high effort (workflow run `wf_29a7b5d1-bc9`).
Claims below are marked **verified** (re-read in source by a second agent or
by me) or **assumption**. File references are to the Pi config checkout unless prefixed
with a Zeron crate path.

## Verified behavior

### Claude Code dynamic workflows (reference design)

| Area | Behavior |
| --- | --- |
| Authoring | A plain JS script with `agent()`, `parallel()`, `pipeline()`, `phase()`, `log()`; `Date.now()`/`Math.random()` throw, enforced by a static lint. |
| Cache key | Hash chain `v2:sha256(prevKey \0 prompt \0 opts subset)`. Order-coupled: one observed replay kept 12 of 33 results because `pipeline()` call order followed completion order. |
| Taint | The first cache miss taints everything after it; agents already in flight do not taint. |
| Failures | A failed or null agent is journaled `failed` and re-runs on resume. |
| Journal | Append-only JSONL, tolerant of a torn final line. Resume is same-session and explicit; there is no auto-resume. |
| Retry | Stalls retry up to 5 times. Concurrency is `min(16, max(2, cpus - 2))`. |

Lesson: copy the authoring model and the explicit resume; do **not** copy the
hash-chain key.

### Pi workflows extension (what we are changing)

| Area | Behavior | Evidence |
| --- | --- | --- |
| Tools | `workflow` (permanent; goal, script or saved name), `workflow_script` (only after `/workflow plan`), `workflow_control`. | `index.ts` |
| Execution | `node:vm` behind an Acorn gate; one RPC child per attempt. | `sandbox.ts`, `executor/` |
| Journal | `~/.pi/workflows/projects/<slug>-<fnv(cwd)>/<runId>.json`; tmp + fsync + rename with a `.bak`; write-ahead records. | `journal.ts` |
| Resume | `resumeFrontier` = first non-done ordinal; `reopen` keeps only done records below it. | `journal.ts` `resumeFrontier`, `reopen` |
| Divergence | A positional promptHash/policyHash mismatch throws `WorkflowDivergenceError`; the run finishes `divergent: true` and every later resume is refused. | `run.ts` existing.divergent check |
| dag | Node ids validated unique, but cached replays complete instantly, so admission order (and ordinals) differ on resume. Header says not resume-safe. | `dag.ts` header |
| Startup | `session_start` only installs the bridge. | `index.ts` |
| Shutdown | `session_shutdown` aborts the run and awaits it: status `aborted`. | `index.ts` |
| RunStatus | `running \| completed \| paused \| error \| aborted`. | `types.ts` |
| Lock | pid file created `wx`; acquired **before** the executor `await` and before the `try/finally` that releases it, so a construction throw leaks it until process exit. | `run.ts` `acquireRunLock` |
| Retries | `attemptsFor = min(plan, agentRetries + 1)` with default `agentRetries` 1, so the README retry table overstates. | `retry.ts`, `run.ts` |
| Timeouts | Wall clock only; `maxTurns` and `context` are accepted and hashed but never enforced. | `dsl.ts` |
| Events | `phase`, `log`, `progress`, `agent{ordinal,label,model,status}`, `telemetry`, `fanout`. Retry, escalation, skip and cache hits appear only as log strings. | `dsl.ts` |
| Live channel | `workflow:progress` = `{active,name,phase,status,running,done,total,startedAt,dryRun}`, republished every 1 s. | `progress.ts` |
| GUI panel | `workflows:run` is a **screen** placement: no transcript rows; the `workflows:status` chip opens it. | `ui/bridge-panel.ts` |
| Isolation | Worktree isolation is ordinal-coupled: `cwdFor(ordinal)`, cached patch restore by ordinal, `merge()` checks captured ordinals. | `isolation.ts`, `run.ts` |
| Outcome | No run ends `error` when every agent failed. Results reach the model through `lib/background-outbox` (`workflow-result:<runId>`). | `run.ts`, `index.ts` |

### Pi core extension APIs

- RPC mode is long-lived per chat; there is no `session_switch` event, so the
  extension must rebind state in `session_start`.
- `pi.events` is an in-process EventEmitter.
- The PIUI hub keeps the last payload per channel and replays it on `hello`.
  `00-piui` clears channels on a non-startup `session_start`, so an extension
  must republish its channels from its own `session_start`.
- Any channel name works through `lib/bridge.ts` `publish(channel, payload)`.
  Payloads over 32 KiB per notify are chunked.
- Actions need no rendered element: `00-piui` turns a `pi_ui_event` into
  `pi.events` `piui:<namespace>` with `{elementId, actionId, value, rowId}`.

### Zeron

- Pi is long-lived per chat. Composer Stop ends only the turn, so a workflow
  keeps running. Quit or a launch-config change terminates the runtime and
  writes `Done{interrupted}`; Windows kills through a job object with no flush.
- Ingestion is already generic: Pi stdout -> `piui.rs` `ExtensionUi::handle` ->
  watch -> `WatchExtensionUi` (whole-state frames, 50 ms) ->
  `AppState.extension_ui`, for the selected chat.
- `workflow:progress` is read only by `background_tasks()` in `piui.rs`, which
  keeps the idle reaper off while a run is active.
- Sending an action (`send_action`) needs no element on screen.
- The session panel (`crates/ui/src/shell/session_info/`) has sections
  Goal, Todos, Agents, Background and Compact; declaration order breaks ties;
  `crates/proto/src/extension_ui.rs` `CompactStatus` is the typed-channel
  pattern to copy.

### Durable-execution frameworks (what to borrow)

| System | Idea taken |
| --- | --- |
| Inngest | Step id plus occurrence counter as the memo key. |
| LangGraph | Pending writes: successful siblings survive a failed superstep. |
| AWS Step Functions | Redrive re-runs only failed branches. |
| Prefect, Bazel | Content hashes decide reuse, not position. |
| DBOS | Dead-letter after N recoveries. |
| Temporal | Separate timeouts and heartbeats (start-to-close vs idle). |
| pi-subagent-workflow | Lineage + prompt hash + environment fingerprint; invalidate only the causal tail. |

## Contradictions resolved

- **Workflow row controls in the transcript.** One researcher described pinned
  inline controls for workflows. Wrong: `workflows:run` is a screen element,
  and Stop/Pause exist only in that screen. The new section adds them.
- **"Pi runs once per turn"** in `docs/pi.md` is stale since Pi became
  long-lived per chat; fixed alongside this work.
- **Retry counts.** The README table lists per-class plans, but the effective
  attempts are capped by `agentRetries + 1` (default 2).
- **Claude Code keeps results across reorders.** It does not; its key is a
  chain, and an unchanged script can lose most of its cache.

## Assumptions and unverified items

- The installed Pi binary may lag the source checkout (`entry_appended`,
  `get_entries` were not confirmed in the binary Zeron launches).
- Whether `pi --mode rpc` children exit on stdin EOF on Windows, and whether a
  hard-killed parent orphans them, is unknown.
- Hub replay of an arbitrary new channel name after `/new` is inferred from
  the hub code, not observed.
- A realistic 60-row `workflow:state` payload is estimated at 6-10 KiB, well
  under the chunking threshold; not measured.

## Failure matrix

| # | Failure | Today | Designed behavior |
| --- | --- | --- | --- |
| 1 | Agent child dies, Pi alive | Classified `AGENT_SPAWN_FAILED`, no same-tier retry | Typed `retry` event with attempt/maxAttempts; task shows *retrying* |
| 2 | Pi exits cleanly (EOF, `/new`, `/reload`) | Run ends `aborted` | Run ends `interrupted` (reason `shutdown`); resume offered next session |
| 3 | Hard kill / job-object kill | Journal stays `running`, stale lock | `session_start` reaper marks it `interrupted`, clears the lock, publishes a resume card |
| 4 | Zeron restart | Live UI is gone | Extension republishes `workflow:state` (incl. interrupted runs) on the next session start |
| 5 | Remote tunnel drop | Watch drops; state resets | Payload carries `runId` and `updatedAt`; the first frame after reconnect replaces state |
| 6 | Edited prompts on resume | `WorkflowDivergenceError`, journal burned | Mismatch becomes a cache miss with an `invalidated` event (strict only when worktree patches are involved) |
| 7 | `dag()` resume | Unsound, ordinal swaps | Content-keyed reuse makes order irrelevant for matching prompts; node ids flow into the UI as dependencies |
| 8 | Orphaned children | Keep running | Out of scope for v1; documented |
| 9 | Timeout | Wall clock only | Unchanged in v1; idle timeout is a follow-up |
| 10 | Rate limit | Retried within `agentRetries` | Retry is now visible with its delay and code |
| 11 | Partial fan-out failure | Finished siblings after the failure discarded | Finished siblings are kept in a reuse pool and served by content key |
| 12 | Torn journal | Primary + `.bak` | Unchanged (already torn-write safe) |
| 13 | Two runs | Second launch refused | Unchanged; the channel shows the one active run |
| 14 | Double Resume click | Lock refuses own pid | Resume action is idempotent while a run is active |
| 15 | Lock leak on setup throw | Lock held until exit | `try/finally` starts right after acquisition |
| 16 | Every agent failed | Run reports `completed` | Run ends `error` |
| 17 | Frozen last frame after Pi dies | Looks live | `updatedAt` lets the UI mark it stale; Zeron drops channels with the process |
| 18 | Untrusted strings | Raw | Extension truncates labels/errors; Zeron strips control characters |

## Recommended architecture

### Pi runtime

1. **Lock leak fix.** Open the `try` immediately after `acquireRunLock`.
2. **`interrupted` status and reaper.** Add `interrupted` to `RunStatus`.
   `session_shutdown` finishes an active run as `interrupted` unless the user
   stopped it. `session_start` scans the project's journal directory for
   `running`/`paused` journals whose lock pid is dead (or that have no lock)
   and rewrites them as `interrupted`, then publishes them as resumable.
3. **Content-keyed reuse ("orphan pool").** Positional serving of records below
   the frontier is unchanged. On reopen, done, non-worktree records **after**
   the frontier move into a reuse pool keyed by `promptHash|policyHash`
   instead of being discarded. A pooled record is eligible only if every
   non-done record before it either cannot write (worktree-isolated) or had not
   finished when the pooled record started, so a sibling that ran concurrently
   with, or before, the failure is reusable, while work that ran after a
   failed writer is not. When `agent()` misses positionally it takes a matching
   pooled record (same ordinal first, else lowest), journals it under its new
   ordinal with `cached: true` and `reusedFrom`, and removes it from the pool.
4. **Soft divergence.** When the run has no worktree-isolated records, a
   positional hash mismatch no longer throws: the call is a cache miss, an
   `invalidated` event is emitted, the remaining positional cache moves into
   the content pool, and the call runs live. With worktree records the strict
   throw stays, because patches are ordinal-coupled.
5. **dag node identity.** `runDag` reports node state transitions, and an
   `AsyncLocalStorage` node context stamps each agent record with its
   `nodeId` and the node's dependencies, which the UI renders.
6. **All-failed is an error.** A run where every agent failed finishes `error`.
7. **Typed events.** `retry {attempt, maxAttempts, delayMs, errorCode}`,
   `cached`, `invalidated`, `node`, plus the existing events.

### Live channel `workflow:state` (v1)

Whole-state and idempotent; unknown fields are ignored on both sides;
`workflow:progress` keeps its current shape so `background_tasks()` and the TUI
footer keep working.

```jsonc
{
  "v": 1,
  "updatedAt": 1760000000000,          // ms since epoch, monotonic per process
  "run": {
    "id": "wf-…", "name": "…", "goal": "…",  // goal <= 300 chars
    "status": "running",                 // running|paused|completed|error|aborted|interrupted
    "statusReason": "…",
    "startedAt": 0, "endedAt": 0,
    "dryRun": false, "resumed": false, "phase": "Review"
  },
  "progress": { "total": 0, "done": 0, "running": 0, "queued": 0, "failed": 0,
                "cached": 0, "retrying": 0, "skipped": 0 },
  "phases": [{ "title": "…", "state": "active", "done": 0, "total": 0 }],
  "tasks": [{                            // capped at 60; tasksTruncated counts the rest
    "id": "7", "label": "…", "phase": "…",
    "state": "running",                  // queued|running|retrying|done|cached|failed|skipped|aborted
    "attempt": 1, "maxAttempts": 2,
    "dependsOn": ["parse"], "node": "review",
    "cached": false, "startedAt": 0, "endedAt": 0,
    "error": { "code": "RATE_LIMITED", "message": "…", "retriable": true },
    "model": "…", "tokens": 0
  }],
  "tasksTruncated": 0,
  "checkpoints": [{ "at": 0, "kind": "agent", "doneCount": 0, "label": "…" }],
  "recovery": {
    "resumable": true, "reason": "…",
    "reused": 0, "invalidated": 0,
    "interrupted": [{ "runId": "…", "name": "…", "endedAt": 0, "done": 0, "total": 0 }],
    "actions": [{ "id": "resume", "label": "Resume", "runId": "…" }]
  }
}
```

Actions come back as `pi.events` `piui:workflows` with `elementId`
`workflows:state`, `actionId` one of `stop | pause | resume | discard`, and
`rowId` = run id. Resume goes through the same approval gate as
`/workflow resume`.

### Zeron

- **Proto:** `WORKFLOW_STATE_CHANNEL` and `WorkflowState::of(&ExtensionUiState)`
  beside `CompactStatus`; every field `#[serde(default)]`, camelCase, unknown
  task states parse to `Other`.
- **Harness:** `background_tasks()` also counts an active `workflow:state`.
- **UI:** `SectionKey::Workflow` after Agents, a pure
  `session_info/workflow.rs` view model with unit tests, and a section with a
  done/total count and status pill, name and goal, progress bar, phase strip,
  task rows (running, retrying and failed first; Queued and Completed folds;
  cached tags; attempt counts; error and dependency tooltips), a checkpoint
  line, and a recovery strip (Resume, Discard) plus hover Stop/Pause. The
  section appears only while a run is live, retrying, paused, failed, or
  interrupted and resumable.

## Non-goals

- An event-sourced replay engine or deterministic replay for arbitrary scripts.
- Mid-turn agent checkpoints (children stay `--no-session`; a dead agent
  restarts from scratch).
- A new Zeron RPC, capability bit or Pi core patch.
- Several concurrent runs per session.
- Auto-resume without consent: an interrupted write-capable run is offered,
  never restarted.
- Copying Claude Code's chain key.

## Test plan

- **Pi (`agent/extensions/workflows/test`):** reuse pool serves a completed
  sibling after a failed one; a write-after-failure record is not reused;
  divergence downgrade returns a cache miss without isolation and still throws
  with it; reaper marks a dead-lock `running` journal `interrupted`; lock is
  released when setup throws; all-failed run ends `error`; state publisher caps
  tasks and orders running/failed first.
- **Zeron proto:** round-trip and tolerance tests for `WorkflowState`.
- **Zeron harness:** `background_tasks()` counts an active `workflow:state`.
- **Zeron UI:** view-model grouping, relevance (hidden when idle or after a
  clean completion), clock-skew clamping, unknown states, sanitising.
- All of it runs on GitHub CI, never on a developer laptop.
