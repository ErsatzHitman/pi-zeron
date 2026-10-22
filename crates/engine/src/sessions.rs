//! SessionsEngine — per-chat agent runs: dispatch, steering, interrupts, input bridging,
//! journal + broadcast fan-out, and 120ms coalesced doc streaming.
//!
//! Pragmatic port of zeron's `sessions.ts` (spec: feature-inventory §3.2):
//! - every `AgentEvent` is (a) appended to the on-disk run journal, (b) broadcast to
//!   in-process subscribers, (c) folded via `fold_event_into_parts` and diffed into the
//!   chat's `SessionDoc` through `SegmentWriter` on a coalesced `STREAM_COMMIT_MS` timer;
//! - the user message entry is pushed to the doc immediately on dispatch (id = the
//!   command's client-minted message id, so optimistic echoes never flicker);
//! - a `Steered` event splits the assistant entry at the exact boundary;
//! - recovery (interrupt or a stale journal at boot) stamps the streaming entry `aborted`.
//!
//! Scope notes: sessions are keyed by chat id (one live run per chat). Zeron's pulse
//! loop is ported as the 15s liveness heartbeat in `drive_run`; its stall watchdog is
//! deliberately NOT ported (rejected in review — agents may legitimately wait on
//! something for far longer than any timeout, and a live child IS the working signal).
//! Every dying path must instead carry its own visible error (child crash with stderr,
//! spawn failure, stream error, engine-restart recovery).

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};

use chrono::Utc;
use futures::StreamExt;
use tokio::sync::{broadcast, mpsc, oneshot, watch};

use zeron_doc::{
    DocError, MessagePart, MessageRole, MessageStatus, STREAM_COMMIT_MS, SegmentWriter, SessionDoc,
    SessionMessageEntry, fold_event_into_parts, sanitize_tool_call,
};
use zeron_harness::{CancellationToken, Harness, RunControls, SteerMessage, TurnControl};
use zeron_proto::{
    AgentEvent, DoneStatus, HarnessId, RunRequest, Session, SessionStatus, UserInputAnswer,
    UserInputQuestion,
};

use crate::admission::{Admission, Admit};
use crate::doc_host::{ChatDocHandle, DocHost};
use crate::registry::{HarnessDescriptor, HarnessRegistry};
use crate::run_journal::RunJournal;
use crate::{EngineError, new_id, now_ms};

/// One journaled event: the durable seq plus the event, as broadcast to subscribers.
#[derive(Debug, Clone)]
pub struct JournaledEvent {
    pub seq: u64,
    pub event: AgentEvent,
}

/// Outcome of a steer attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SteerOutcome {
    /// Delivered into the live run's steering mailbox.
    Accepted,
    /// A live run owns the turn, but an update prevents accepting another prompt.
    DeferredByUpdate,
    /// No live steerable run — the caller should dispatch the prompt as a new turn.
    NotSteerable,
}

type PendingInputs = Arc<Mutex<HashMap<String, oneshot::Sender<Vec<UserInputAnswer>>>>>;

/// A harness-native session id plus the cwd it was created under. Harness
/// session stores are cwd-scoped (claude keys conversations by project
/// directory — zeron sessions.ts:563 "harness session stores are keyed by
/// cwd"), so resume is only injected for runs launched from the same cwd.
#[derive(Debug, Clone)]
struct HarnessSessionRef {
    session_id: String,
    cwd: String,
}

/// The configuration a live harness runtime launched with. The steering
/// mailbox carries only the prompt (and its images), so a request whose
/// launch settings differ would silently run with the old process's — it
/// must replace the runtime and resume its harness-native session instead.
/// The driver decides what "differ" means ([`Harness::same_runtime`]): a
/// replacement kills the runtime's background work, so a setting the
/// launch never reads, or the same value spelled differently by another
/// client, must never force one.
#[derive(Clone)]
struct RuntimeConfig {
    harness_id: HarnessId,
    request: RunRequest,
    harness: Arc<dyn Harness>,
}

impl RuntimeConfig {
    fn new(harness_id: HarnessId, request: &RunRequest, harness: Arc<dyn Harness>) -> Self {
        Self {
            harness_id,
            request: request.clone(),
            harness,
        }
    }

    fn can_route(&self, harness_id: HarnessId, request: &RunRequest) -> bool {
        self.harness_id == harness_id && self.harness.same_runtime(&self.request, request)
    }

    /// How `request` reaches this runtime: `Some(false)` as is, `Some(true)`
    /// with its configuration adopted in place first
    /// ([`Harness::reconfigures_in_place`]), `None` only by replacing it.
    fn route(&self, harness_id: HarnessId, request: &RunRequest) -> Option<bool> {
        if self.can_route(harness_id, request) {
            Some(false)
        } else if self.harness_id == harness_id
            && self.harness.reconfigures_in_place(&self.request, request)
        {
            Some(true)
        } else {
            None
        }
    }
}

/// A run's live voice calls. They hold its idle reaper off, and each hang-up
/// wakes the reaper to restart its window from there.
#[derive(Default)]
pub(crate) struct VoiceActivity {
    calls: std::sync::atomic::AtomicUsize,
    ended: tokio::sync::Notify,
}
impl VoiceActivity {
    pub(crate) fn live(&self) -> bool {
        self.calls.load(std::sync::atomic::Ordering::Acquire) > 0
    }
    /// A live call until the returned guard drops.
    pub(crate) fn call(self: &Arc<Self>) -> VoiceCall {
        self.calls.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        VoiceCall(self.clone())
    }
}
pub(crate) struct VoiceCall(Arc<VoiceActivity>);
impl Drop for VoiceCall {
    fn drop(&mut self) {
        self.0
            .calls
            .fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
        // A stored permit: the reaper hears it even while busy elsewhere.
        self.0.ended.notify_one();
    }
}

struct RunHandle {
    voice: Option<zeron_harness::codex::realtime::RealtimeHandle>,
    voice_active: Arc<VoiceActivity>,
    run_id: String,
    steerable: bool,
    runtime_config: RuntimeConfig,
    steer_tx: mpsc::Sender<SteerMessage>,
    /// Harness-level cancellation (protocol interrupt + child teardown).
    interrupt_token: CancellationToken,
    /// Turn-level stop + the runtime's background-work report. Used for
    /// stopping a turn when the driver [`Harness::stops_turn_in_place`].
    turn: TurnControl,
    stops_turn_in_place: bool,
    /// [`Harness::keeps_runtime_alive`]: the user's Stop ends only the turn.
    keeps_runtime_alive: bool,
    /// The harness stream is open. Before that there is no runtime to keep:
    /// a stop tears the pending start down instead.
    started: Arc<std::sync::atomic::AtomicBool>,
    /// Engine-level cancel: arms the run task's grace deadline so a harness that
    /// ignores its token can never strand the run.
    cancel: watch::Sender<bool>,
    engine_tx: mpsc::UnboundedSender<AgentEvent>,
    pending_inputs: PendingInputs,
    /// Steers accepted into the mailbox but not yet confirmed by a `Steered`
    /// event — the at-least-once ledger. A run can die with accepted steers
    /// still in its mailbox (idle reaper vs. a routed send; a mid-turn error
    /// discarding queued boundary steers): the run task drains this at exit
    /// and re-dispatches each entry as a fresh turn, so an accepted message
    /// can never silently evaporate from a transcript that shows it as sent.
    routed_steers: Arc<Mutex<std::collections::VecDeque<RoutedSteer>>>,
    /// This runtime's provider session holds the fork's copied history (a
    /// side chat's bootstrap went out on its run or on one of its steers).
    fork_history_sent: Arc<std::sync::atomic::AtomicBool>,
    /// The user stopped this runtime: it is being torn down and takes no
    /// more prompts. Flipped under the ledger lock, so every accepted steer
    /// is unambiguously before the stop (cancelled with it) or after it
    /// (re-dispatched to a fresh runtime — never lost).
    retiring: Arc<std::sync::atomic::AtomicBool>,
}

/// One accepted-but-unconfirmed steer: enough to re-dispatch it verbatim.
#[derive(Debug, Clone)]
struct RoutedSteer {
    prompt: String,
    message_id: String,
    /// The delivered text carried the fork's copied history: its delivery
    /// is recorded only when the runtime confirms consuming it (`Steered`).
    /// An orphan re-dispatches the bare prompt, still owing the history.
    fork_history: bool,
    /// Accepted after the user's Stop began tearing the runtime down: the
    /// Stop did not cancel it, so the message still runs.
    after_stop: bool,
    /// The run configuration it was sent with, images included: an orphan
    /// re-dispatches exactly this, not whatever the chat's latest request
    /// became. `None` when the chat had no recorded request.
    request: Option<Box<RunRequest>>,
}

/// The chats DELETED from this workspace while the process ran (chat ids are
/// never reused). A run can settle a little after [`SessionsEngine::forget_chat`]
/// — `terminate` waits a bounded 5s — and its status write must not resurrect
/// the row the delete just dropped, so deletes are recorded and honoured.
///
/// Only a settlement within seconds of the delete needs defeating, so the
/// oldest marks are dropped once the ledger is full instead of it growing for
/// the life of the process (voice calls delete a chat per rotation).
#[derive(Default)]
struct DeletedChats {
    ids: std::collections::HashSet<String>,
    order: std::collections::VecDeque<String>,
}

impl DeletedChats {
    /// How many recent deletions the ledger remembers.
    const MEMORY: usize = 4096;

    fn mark(&mut self, chat_id: &str) {
        if self.ids.insert(chat_id.to_string()) {
            self.order.push_back(chat_id.to_string());
            while self.order.len() > Self::MEMORY {
                if let Some(old) = self.order.pop_front() {
                    self.ids.remove(&old);
                }
            }
        }
    }

    fn contains(&self, chat_id: &str) -> bool {
        self.ids.contains(chat_id)
    }
}

struct Inner {
    voice: crate::voice::VoiceManager,
    device_id: String,
    /// The engine is shutting down: dying runs re-dispatch nothing.
    shutting_down: std::sync::atomic::AtomicBool,
    /// Per chat: messages a Stop cancelled while they were still on their
    /// way (see [`SessionsEngine::cancel_sent_messages`]).
    cancelled_sends: Mutex<HashMap<String, std::collections::HashSet<String>>>,
    /// Chats that were DELETED (their ids are never reused). A run that settles
    /// after [`SessionsEngine::forget_chat`] — `terminate` only waits so long —
    /// must not resurrect the session row it just lost, so the delete is
    /// recorded here and every status/dispatch write honours it.
    deleted_chats: Mutex<DeletedChats>,
    /// Loopback IPC port this engine serves, once known (0 = not serving):
    /// what the injected `zeron mcp` server dials back into.
    ipc_port: std::sync::atomic::AtomicU16,
    journal: Arc<RunJournal>,
    registry: Arc<HarnessRegistry>,
    /// Set-once (first wins), cleared on runtime retirement: sessions and
    /// doc-host reference each other through Arcs, so this back-edge must be
    /// severable for a replaced engine graph to drop.
    doc_host: Mutex<Option<DocHost>>,
    /// chat_id → live run.
    runs: Mutex<HashMap<String, RunHandle>>,
    /// chat_id → broadcast hub (retained across runs so subscribers survive turns).
    hubs: Mutex<HashMap<String, broadcast::Sender<JournaledEvent>>>,
    /// chat_id → session row. A `BTreeMap`: the published list is in chat-id
    /// order, so no status change has to clone AND sort the whole map (there is
    /// one status change per heartbeat of every working chat — audit #115).
    statuses: Mutex<BTreeMap<String, Session>>,
    sessions_tx: watch::Sender<Vec<Session>>,
    /// Slots of the global running-turn cap and the queue of starts waiting
    /// for one. Lock order: `statuses` before `admission`.
    admission: Mutex<Admission>,
    /// Last dispatched request per chat — the steer→new-turn fallback re-derives its
    /// run config from this (chat config rows land with the workspace doc in M4).
    last_requests: Mutex<HashMap<String, RunRequest>>,
    /// Harness-native session ids per chat (resume continuity across turns) —
    /// the live-process cache over the durable copy on the workspace chat row
    /// (zeron kept the same pair on `chats.harness_session_id`). An empty
    /// session id is the "do not resume" tombstone after a rejected resume.
    harness_sessions: Mutex<HashMap<String, HarnessSessionRef>>,
    /// Auto-titler for untitled chats (wired at engine assembly; absent in bare tests).
    titles: OnceLock<crate::titles::TitleGenerator>,
    generated_images: OnceLock<(crate::uploads::Uploads, std::path::PathBuf)>,
    /// [`crate::Repos::scratch_root`]; bare tests fall back to
    /// `~/.zeron/scratch`.
    scratch_root: OnceLock<std::path::PathBuf>,
    /// Fired with `(chat_id, cwd)` when a user prompt starts a turn (fresh
    /// dispatch or accepted steer) — the diff sync snapshots the checkout tree
    /// for the Changes pane's "Latest turn" scope. Absent in bare tests.
    turn_listener: OnceLock<TurnListener>,
    /// Live extension UI (Pi's PIUI) per chat; never persisted.
    extension_ui: crate::extension_ui::ExtensionUiHub,
    /// Pushes a settled tree member's outcome to its parent.
    parent_notifier: parent_notify::ParentNotifier,
    /// This engine's own `Arc`, for background tasks started from `&self`.
    this: std::sync::Weak<Inner>,
}

#[path = "parent_notify.rs"]
mod parent_notify;

/// Turn-start hook: called with `(chat_id, cwd)`.
pub type TurnListener = Arc<dyn Fn(&str, &str) + Send + Sync>;

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Journal sweep defaults: pass every 15 min; a journal must be quiet for 10 min (the doc
/// snapshot debounce and `Done` finalization settle) and at least 1 MiB to be worth a rewrite.
const JOURNAL_SWEEP_INTERVAL_MS: u64 = 15 * 60 * 1000;
const JOURNAL_SETTLE_MS: u64 = 10 * 60 * 1000;
const JOURNAL_MIN_COMPACT_BYTES: u64 = 1024 * 1024;

/// A chat with a run entry (live or parked) is never compacted. `chat` may be the
/// sanitized journal file stem, so compare run keys through the same sanitizer.
fn journal_chat_is_live(inner: &Inner, chat: &str) -> bool {
    lock(&inner.runs)
        .keys()
        .any(|id| id == chat || crate::run_journal::sanitize_id(id) == chat)
}

async fn sweep_journals_once(
    inner: &Inner,
    settle: std::time::Duration,
    min_bytes: u64,
    is_live: impl Fn(&str) -> bool,
) -> usize {
    let mut compacted = 0;
    for chat in inner.journal.journaled_chats() {
        if inner
            .shutting_down
            .load(std::sync::atomic::Ordering::Acquire)
        {
            break;
        }
        if is_live(&chat) {
            continue;
        }
        if inner.journal.idle_for(&chat).is_none_or(|age| age < settle) {
            continue;
        }
        let journal = inner.journal.clone();
        let name = chat.clone();
        match tokio::task::spawn_blocking(move || journal.compact(&name, min_bytes)).await {
            Ok(Ok(Some(stats))) => {
                compacted += 1;
                tracing::info!(
                    chat = %chat,
                    before = stats.before_bytes,
                    after = stats.after_bytes,
                    dropped = stats.dropped,
                    "run journal compacted"
                );
            }
            Ok(Ok(None)) => {}
            Ok(Err(err)) => tracing::warn!(chat = %chat, error = %err, "journal compaction failed"),
            Err(err) => tracing::warn!(chat = %chat, error = %err, "journal compaction panicked"),
        }
        // Low priority: yield between chats so the sweep never hogs a worker.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    compacted
}

#[derive(Clone)]
pub struct SessionsEngine {
    inner: Arc<Inner>,
}

impl SessionsEngine {
    pub fn new(
        device_id: String,
        journal: Arc<RunJournal>,
        registry: Arc<HarnessRegistry>,
    ) -> Self {
        let (sessions_tx, _) = watch::channel(Vec::new());
        Self {
            inner: Arc::new_cyclic(|this| Inner {
                voice: crate::voice::VoiceManager::default(),
                device_id,
                shutting_down: std::sync::atomic::AtomicBool::new(false),
                cancelled_sends: Mutex::new(HashMap::new()),
                deleted_chats: Mutex::new(DeletedChats::default()),
                ipc_port: std::sync::atomic::AtomicU16::new(0),
                journal,
                registry,
                doc_host: Mutex::new(None),
                runs: Mutex::new(HashMap::new()),
                hubs: Mutex::new(HashMap::new()),
                statuses: Mutex::new(BTreeMap::new()),
                sessions_tx,
                admission: Mutex::new(Admission::default()),
                last_requests: Mutex::new(HashMap::new()),
                harness_sessions: Mutex::new(HashMap::new()),
                titles: OnceLock::new(),
                generated_images: OnceLock::new(),
                scratch_root: OnceLock::new(),
                turn_listener: OnceLock::new(),
                extension_ui: Default::default(),
                parent_notifier: parent_notify::ParentNotifier::new(),
                this: this.clone(),
            }),
        }
    }

    /// Mirror `chat_id`'s Pi fleet onto its session row, so every client's
    /// sidebar lists its jobs, the chat watched or not. One follower per
    /// chat for the engine's life; it heartbeats the row while jobs run (a
    /// fleet outlives the turn that launched it).
    fn follow_fleet(&self, chat_id: &str) {
        const BEAT: std::time::Duration = std::time::Duration::from_secs(10);
        let Some(mut state) = self.inner.extension_ui.follow(chat_id) else {
            return;
        };
        let inner = Arc::downgrade(&self.inner);
        let chat_id = chat_id.to_owned();
        tokio::spawn(async move {
            let mut seen = HashMap::new();
            loop {
                let (jobs, background) = {
                    let ui = state.borrow_and_update();
                    (ui.running_fleet(), ui.running_background_jobs())
                };
                let list = fleet_list(jobs, &mut seen, Utc::now());
                let busy = !list.is_empty() || background > 0;
                {
                    let Some(inner) = inner.upgrade() else {
                        return;
                    };
                    inner.set_running_fleet(&chat_id, list);
                    inner.set_background_jobs(&chat_id, background);
                    if busy {
                        inner.touch_session(&chat_id);
                    }
                }
                tokio::select! {
                    changed = state.changed() => {
                        if changed.is_err() {
                            return;
                        }
                    }
                    () = tokio::time::sleep(BEAT), if busy => {}
                }
            }
        });
    }

    /// Start the low-priority sweep that compacts idle chats' run journals (see
    /// [`RunJournal::compact`]). Weak-held: it ends with the engine. Knobs, read once:
    /// `ZERON_JOURNAL_SWEEP_MS`, `ZERON_JOURNAL_SETTLE_MS`, `ZERON_JOURNAL_MIN_BYTES`.
    pub fn spawn_journal_sweep(&self) {
        const FIRST_DELAY: std::time::Duration = std::time::Duration::from_secs(3 * 60);
        let env_ms = |key: &str, default: u64| {
            std::env::var(key)
                .ok()
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or(default)
        };
        let interval = std::time::Duration::from_millis(env_ms(
            "ZERON_JOURNAL_SWEEP_MS",
            JOURNAL_SWEEP_INTERVAL_MS,
        ));
        let settle =
            std::time::Duration::from_millis(env_ms("ZERON_JOURNAL_SETTLE_MS", JOURNAL_SETTLE_MS));
        let min_bytes = env_ms("ZERON_JOURNAL_MIN_BYTES", JOURNAL_MIN_COMPACT_BYTES);
        let first = FIRST_DELAY.min(interval);
        // Assembly can run from a bare sync context; there is nothing to sweep then.
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let inner = Arc::downgrade(&self.inner);
        runtime.spawn(async move {
            tokio::time::sleep(first).await;
            loop {
                {
                    let Some(inner) = inner.upgrade() else {
                        return;
                    };
                    if inner
                        .shutting_down
                        .load(std::sync::atomic::Ordering::Acquire)
                    {
                        return;
                    }
                    let compacted = sweep_journals_once(&inner, settle, min_bytes, |chat| {
                        journal_chat_is_live(&inner, chat)
                    })
                    .await;
                    tracing::info!(
                        journal_bytes = inner.journal.total_bytes(),
                        chats_compacted = compacted,
                        "journal sweep finished"
                    );
                }
                tokio::time::sleep(interval).await;
            }
        });
    }

    /// One sweep pass: compact every idle chat's journal. Returns how many were
    /// compacted. Public so tests (and tools) can run a pass without the timer.
    pub async fn compact_idle_journals(
        &self,
        settle: std::time::Duration,
        min_bytes: u64,
    ) -> usize {
        sweep_journals_once(&self.inner, settle, min_bytes, |chat| {
            journal_chat_is_live(&self.inner, chat)
        })
        .await
    }

    /// Combined size of all run journals on disk.
    pub fn journal_bytes(&self) -> u64 {
        self.inner.journal.total_bytes()
    }

    /// Watch a chat's live extension UI; see [`crate::extension_ui`].
    pub(crate) fn watch_extension_ui(
        &self,
        chat_id: &str,
    ) -> (
        watch::Receiver<zeron_proto::extension_ui::ExtensionUiState>,
        crate::extension_ui::WatcherGuard,
    ) {
        self.inner.extension_ui.watch(chat_id)
    }

    /// A one-shot copy of a chat's live extension UI, channels included;
    /// unlike [`Self::watch_extension_ui`] it does not mark the chat watched.
    pub(crate) fn extension_ui_state(
        &self,
        chat_id: &str,
    ) -> zeron_proto::extension_ui::ExtensionUiState {
        self.inner.extension_ui.state(chat_id)
    }

    pub(crate) fn run_journal(&self) -> Arc<RunJournal> {
        self.inner.journal.clone()
    }

    /// Send an extension UI action to the chat's live harness process.
    pub(crate) fn extension_ui_action(
        &self,
        chat_id: &str,
        action: zeron_proto::extension_ui::ExtensionUiAction,
    ) -> Result<(), String> {
        self.inner.extension_ui.act(chat_id, action)
    }

    pub(crate) fn voice_manager(&self) -> crate::voice::VoiceManager {
        self.inner.voice.clone()
    }

    /// Record the loopback IPC port this engine serves. Runs started after
    /// this carry Zeron's MCP server (see [`Inner::zeron_mcp`]); until then —
    /// or with 0 — agents get no Zeron tools rather than a dead server.
    pub fn set_ipc_port(&self, port: u16) {
        self.inner
            .ipc_port
            .store(port, std::sync::atomic::Ordering::Relaxed);
    }

    /// Wire the doc host (called once at engine assembly; the two services are mutually
    /// referential by design — sessions stream into docs, docs execute commands here).
    pub fn set_doc_host(&self, host: DocHost) {
        // First set wins (the OnceLock contract this slot replaced).
        let mut slot = lock(&self.inner.doc_host);
        if slot.is_none() {
            *slot = Some(host);
        }
    }

    /// Sever the doc-host back-edge (runtime retirement; the doc host's
    /// `shutdown_workers` clears its own sessions edge). Every access site
    /// already treats a missing doc host as "not wired".
    pub fn clear_doc_host(&self) {
        lock(&self.inner.doc_host).take();
    }

    /// Bind generated-image intake to the same profile store used by attachment RPCs.
    /// Where projectless chats' scratch folders resolve (set once at boot).
    pub(crate) fn set_scratch_root(&self, root: std::path::PathBuf) {
        let _ = self.inner.scratch_root.set(root);
    }

    pub fn set_generated_images(
        &self,
        uploads: crate::uploads::Uploads,
        allowed_root: std::path::PathBuf,
    ) {
        let _ = self.inner.generated_images.set((uploads, allowed_root));
    }

    /// Wire the chat auto-titler (called once at engine assembly). After each
    /// completed exchange the run task fires it for still-untitled chats.
    pub fn set_titles(&self, titles: crate::titles::TitleGenerator) {
        let _ = self.inner.titles.set(titles);
    }

    /// Wire the turn-start listener (called once at engine assembly).
    pub fn set_turn_listener(&self, listener: TurnListener) {
        let _ = self.inner.turn_listener.set(listener);
    }

    fn note_turn_start(&self, chat_id: &str, cwd: &str) {
        if let Some(listener) = self.inner.turn_listener.get() {
            listener(chat_id, cwd);
        }
    }

    fn doc_handle(&self, chat_id: &str) -> Result<Arc<ChatDocHandle>, EngineError> {
        let host = self
            .inner
            .doc_host()
            .ok_or_else(|| EngineError::Other("doc host not wired into sessions engine".into()))?;
        host.open(chat_id)
    }

    /// Status watch: the full session list, re-sent on every transition.
    pub fn watch_sessions(&self) -> watch::Receiver<Vec<Session>> {
        self.inner.sessions_tx.subscribe()
    }

    pub fn session_status(&self, chat_id: &str) -> Option<Session> {
        lock(&self.inner.statuses).get(chat_id).cloned()
    }

    /// Whether this harness takes a prompt *during* a turn, rather than only at
    /// the boundary between turns. The queue drain's central question: a
    /// mid-turn agent gets the message now, everything else holds it until the
    /// turn ends. Catalog lookup only — never spawns.
    pub fn steers_mid_turn(&self, harness: HarnessId) -> bool {
        self.inner
            .registry
            .descriptors()
            .iter()
            .find(|d| d.id == harness)
            .is_some_and(HarnessDescriptor::steers_mid_turn)
    }

    /// Whether this chat has a turn in flight — streaming, or parked on a
    /// question it is still owed an answer to (see [`is_active`]). A persistent
    /// session parked BETWEEN turns is not: it holds a warm child with nothing
    /// outstanding, and takes the next prompt straight through its mailbox.
    pub fn turn_in_flight(&self, chat_id: &str) -> bool {
        lock(&self.inner.statuses)
            .get(chat_id)
            .is_some_and(is_active)
    }

    /// A queued text message can steer the running turn now: it is working
    /// (not parked on a question, which a steer would abandon), on a runtime
    /// that takes input mid-turn, with no agent update gating it.
    pub fn steers_now(&self, chat_id: &str) -> bool {
        let working = lock(&self.inner.statuses)
            .get(chat_id)
            .is_some_and(|session| session.status == SessionStatus::Working);
        let harness = lock(&self.inner.runs)
            .get(chat_id)
            .filter(|h| h.steerable)
            .map(|h| h.runtime_config.harness_id);
        working
            && harness.is_some_and(|harness| self.steers_mid_turn(harness))
            && !self.live_run_update_pending(chat_id)
    }

    /// Whether the chat's live runtime reports background work (subagents,
    /// background shells) still running — parked or not.
    pub fn holds_background_work(&self, chat_id: &str) -> bool {
        lock(&self.inner.runs)
            .get(chat_id)
            .is_some_and(|h| h.turn.background_live())
    }

    /// Any run currently working or blocked on input — the auto-updater's
    /// "don't restart from under a session" gate.
    pub fn any_active(&self) -> bool {
        lock(&self.inner.statuses).values().any(is_active)
            || lock(&self.inner.runs)
                .values()
                .any(|r| r.voice_active.live())
    }

    /// A text prompt for `chat_id` would land in the mailbox of a live
    /// turn-boundary agent mid-turn. The agent reads it only after the turn,
    /// but a mailbox delivery writes the user message now — above the reply
    /// still streaming for the message before it. Such prompts belong in the
    /// visible queue. `request` = a Run that may differ from the live config
    /// (one the runtime adopts in place still lands in its mailbox; any other
    /// config restarts the runtime instead, which is no hold).
    pub fn defers_to_turn_end(
        &self,
        chat_id: &str,
        request: Option<(HarnessId, &RunRequest)>,
    ) -> bool {
        if !self.turn_in_flight(chat_id) {
            return false;
        }
        let live = lock(&self.inner.runs).get(chat_id).map(|h| {
            (
                h.runtime_config.harness_id,
                h.steerable
                    && request.is_none_or(|(id, r)| h.runtime_config.route(id, r).is_some()),
            )
        });
        live.is_some_and(|(harness, routable)| routable && !self.steers_mid_turn(harness))
    }

    /// Whether an accepted agent update gates the chat's live run, so a new
    /// prompt waits in the queue rather than joining that run's transcript.
    pub fn live_run_update_pending(&self, chat_id: &str) -> bool {
        let harness = lock(&self.inner.runs)
            .get(chat_id)
            .map(|h| h.runtime_config.harness_id);
        harness.is_some_and(|harness| self.inner.registry.update_pending(harness))
    }

    /// The chat's live run accepts steering into its mailbox.
    pub fn live_run_steerable(&self, chat_id: &str) -> bool {
        lock(&self.inner.runs)
            .get(chat_id)
            .is_some_and(|h| h.steerable && !h.retiring.load(std::sync::atomic::Ordering::Acquire))
    }

    /// The last request dispatched for a chat (steer→new-turn fallback).
    pub fn last_request(&self, chat_id: &str) -> Option<RunRequest> {
        lock(&self.inner.last_requests).get(chat_id).cloned()
    }

    /// Subscribe to a chat's live event stream: returns the journal replay after
    /// `after_seq` plus a live receiver. Subscribe-then-replay ordering means overlap
    /// (dedupe by seq) rather than gaps.
    pub fn subscribe(
        &self,
        chat_id: &str,
        after_seq: u64,
    ) -> Result<(Vec<JournaledEvent>, broadcast::Receiver<JournaledEvent>), EngineError> {
        let rx = {
            let mut hubs = lock(&self.inner.hubs);
            hubs.entry(chat_id.to_string())
                .or_insert_with(|| broadcast::channel(1024).0)
                .subscribe()
        };
        let replay = self
            .inner
            .journal
            .replay(chat_id, after_seq)?
            .into_iter()
            .map(|(seq, event)| JournaledEvent { seq, event })
            .collect();
        Ok((replay, rx))
    }

    /// Local experimental bootstrap. No Submit, empty user entry or title request.
    pub async fn start_idle(
        &self,
        chat_id: &str,
        request: RunRequest,
    ) -> Result<String, EngineError> {
        // A fresh explicit request may renew an identity-invalidated idle
        // process. Never interrupt a native delegated turn to recover voice.
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(8);
        loop {
            let stale = lock(&self.inner.runs).get(chat_id).map(|run| {
                (
                    run.voice.as_ref().is_some_and(|v| v.invalidated()),
                    run.voice_active.live(),
                )
            });
            match stale {
                Some((true, false)) => {
                    if self.turn_in_flight(chat_id) {
                        return Err(EngineError::Other(
                            "identity changed during delegated work".into(),
                        ));
                    }
                    self.interrupt(chat_id).await?;
                    break;
                }
                Some((true, _)) => {
                    if tokio::time::Instant::now() >= deadline {
                        return Err(EngineError::Other("voice cleanup in progress".into()));
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                }
                _ => break,
            }
        }
        self.dispatch_inner(chat_id, HarnessId::Codex, request, None, false, true)
            .await
    }

    pub(crate) fn take_voice_bridge(
        &self,
        chat_id: &str,
    ) -> Option<(
        zeron_harness::codex::realtime::RealtimeHandle,
        tokio::sync::broadcast::Receiver<zeron_proto::voice::VoiceEvent>,
        Arc<VoiceActivity>,
    )> {
        let mut runs = lock(&self.inner.runs);
        let run = runs.get_mut(chat_id)?;
        Some((
            run.voice.clone()?,
            run.voice.as_ref()?.events.subscribe(),
            run.voice_active.clone(),
        ))
    }

    /// Start (or route) a run for `chat_id`.
    ///
    /// - The user message entry is written to the doc immediately (id = `message_id`).
    /// - A live steerable run receives the prompt as its next turn via the mailbox
    ///   (zeron's persistent-session routing); otherwise any live run is interrupted
    ///   first — never two runtimes driving one chat.
    pub async fn dispatch(
        &self,
        chat_id: &str,
        harness_id: HarnessId,
        request: RunRequest,
        message_id: Option<String>,
    ) -> Result<String, EngineError> {
        self.dispatch_with(chat_id, harness_id, request, message_id, false)
            .await
    }

    /// [`Self::dispatch`] with the startup-crash retry marker: the retry
    /// re-dispatches with `startup_retry = true`, which makes that attempt
    /// final (its own startup death surfaces instead of retrying again).
    /// Boxed future: `drive_run` re-enters this for that retry, and the
    /// erasure breaks the opaque-type cycle the recursion would otherwise form.
    fn dispatch_with<'a>(
        &'a self,
        chat_id: &'a str,
        harness_id: HarnessId,
        request: RunRequest,
        message_id: Option<String>,
        startup_retry: bool,
    ) -> futures::future::BoxFuture<'a, Result<String, EngineError>> {
        Box::pin(self.dispatch_inner(
            chat_id,
            harness_id,
            request,
            message_id,
            startup_retry,
            false,
        ))
    }

    async fn dispatch_inner(
        &self,
        chat_id: &str,
        harness_id: HarnessId,
        mut request: RunRequest,
        mut message_id: Option<String>,
        startup_retry: bool,
        idle: bool,
    ) -> Result<String, EngineError> {
        // Never start a turn for a chat that was deleted (a drain or a peer
        // send can still land after the delete): it would recreate the row and
        // the doc `forget_chat`/`purge_chat` just dropped (audit #115).
        if self.inner.is_deleted(chat_id) {
            return Err(EngineError::Other("chat was deleted".into()));
        }
        let harness_id = self.inner.registry.runnable(harness_id);
        if request.harness.is_some() {
            request.harness = Some(harness_id);
        }
        // Project-less chats store a home-relative cwd (the creating device
        // can't know the host's home); expand it here, on the host, where the
        // run spawns, creating a new chat's scratch folder.
        let scratch_root = self
            .inner
            .scratch_root
            .get()
            .cloned()
            .unwrap_or_else(|| crate::repos::home_dir().join(".zeron").join("scratch"));
        request.cwd = crate::repos::resolve_chat_cwd(&request.cwd, &scratch_root)
            .map_err(EngineError::Other)?;
        // Native-only catalog entries have no portable file fallback. Reject
        // cross-harness delivery before recording or routing the user turn.
        zeron_proto::invocation::validate_harness_invocations(&request.prompt, harness_id)
            .map_err(EngineError::Other)?;
        let routed = lock(&self.inner.runs).get(chat_id).map(|h| {
            (
                h.run_id.clone(),
                h.steerable && !h.retiring.load(std::sync::atomic::Ordering::Acquire),
                h.runtime_config.route(harness_id, &request),
                h.steer_tx.clone(),
                h.routed_steers.clone(),
                h.fork_history_sent.clone(),
                h.retiring.clone(),
            )
        });
        if let Some((run_id, steerable, route, steer_tx, ledger, history_sent, retiring)) = routed {
            if idle {
                // No prompt rides an idle start, so nothing could carry an
                // in-place reconfiguration: only the runtime as is will do.
                return if route == Some(false) && harness_id == HarnessId::Codex {
                    Ok(run_id)
                } else {
                    Err(EngineError::Other("incompatible live runtime".into()))
                };
            }
            let user_id = message_id.clone().unwrap_or_else(new_id);
            let mut bootstrap = None;
            let same_runtime = route.is_some();
            let reconfigure = route == Some(true);
            let accepted = if steerable && same_runtime {
                bootstrap =
                    self.warm_fork_history(chat_id, harness_id, &request.prompt, &history_sent);
                let delivered = bootstrap.as_deref().unwrap_or(&request.prompt);
                // Warm dispatch uses the same mailbox as explicit steering.
                // Register acceptance before a fast boundary can retire it.
                let message = SteerMessage {
                    // OpenCode must see the canonical selection before it
                    // decodes the provider command: a project-scoped command
                    // can disappear between composer discovery and delivery.
                    prompt: if harness_id == HarnessId::Opencode {
                        delivered.to_owned()
                    } else {
                        zeron_proto::invocation::harness_prompt(delivered, harness_id)
                    },
                    message_id: Some(user_id.clone()),
                    // Images ride the mailbox too: replacing the runtime just
                    // to inline them killed its background work.
                    attachments: request.attachments.clone(),
                    // A changed model/effort/option the live process adopts
                    // in place: replacing it killed its background work.
                    config: reconfigure.then(|| Box::new(request.clone())),
                };
                if let Ok(permit) = steer_tx.reserve().await {
                    // Commit the reserved slot atomically with the update
                    // marker. An accepted update releases the slot instead, and
                    // the prompt takes the fresh-run path behind the update.
                    self.inner
                        .registry
                        .while_update_clear(harness_id, || {
                            let mut pending = lock(&ledger);
                            pending.push_back(RoutedSteer {
                                prompt: request.prompt.clone(),
                                message_id: user_id.clone(),
                                fork_history: bootstrap.is_some(),
                                after_stop: retiring.load(std::sync::atomic::Ordering::Acquire),
                                request: Some(Box::new(request.clone())),
                            });
                            permit.send(message);
                        })
                        .is_some()
                } else {
                    false
                }
            } else {
                false
            };
            if accepted {
                if reconfigure {
                    // The runtime runs this configuration from now on: later
                    // sends compare against it, and a re-dispatch of an
                    // orphaned steer starts from it.
                    if let Some(h) = lock(&self.inner.runs)
                        .get_mut(chat_id)
                        .filter(|h| h.run_id == run_id)
                    {
                        h.runtime_config.request = request.clone();
                    }
                    lock(&self.inner.last_requests).insert(chat_id.to_string(), request.clone());
                }
                let handle = self.doc_handle(chat_id)?;
                handle.write_user_message(&user_id, &request.prompt, now_ms())?;
                self.note_turn_start(chat_id, &request.cwd);
                if self.is_live(chat_id, &run_id) {
                    if bootstrap.is_some() {
                        history_sent.store(true, std::sync::atomic::Ordering::Release);
                    }
                    // Working BEFORE the lastMessageAt bump: both ride the
                    // workspace doc from this one peer, so causal order makes it
                    // impossible for an observer to hold [new message, old status]
                    // — that gap read as unseen-with-no-live-run = a phantom
                    // "completed" flash on every remote send (2026-07-31).
                    self.set_status(chat_id, SessionStatus::Working, false);
                    self.inner.note_message(chat_id, &request.prompt);
                    return Ok(run_id);
                }
                // The run died around the send. If its exit drain already
                // claimed the entry, that re-dispatch owns the message —
                // otherwise reclaim it and fall through to a fresh run.
                let reclaimed = {
                    let mut ledger = lock(&ledger);
                    let before = ledger.len();
                    ledger.retain(|s| s.message_id != user_id);
                    ledger.len() != before
                };
                if !reclaimed {
                    self.inner.note_message(chat_id, &request.prompt);
                    return Ok(run_id);
                }
                // Keep the already-written doc entry's id for the fresh run
                // below (write_user_message dedupes by id).
                message_id = Some(user_id);
            }
            if !same_runtime {
                tracing::info!(
                    chat = %chat_id,
                    "restarting live harness to apply changed run configuration"
                );
            }
            // Mailbox closed (runtime mid-teardown / non-steering harness) or
            // the routed run died with the message reclaimed, or the launch
            // configuration changed: replace the runtime.
            self.terminate(chat_id).await?;
        }

        // A Stop sent after this message cancelled it while it was on its
        // way: never start the stopped work again in a fresh runtime.
        if let Some(id) = &message_id
            && lock(&self.inner.cancelled_sends)
                .get_mut(chat_id)
                .is_some_and(|ids| ids.remove(id))
        {
            tracing::info!(chat = %chat_id, "message cancelled by a later Stop: not dispatched");
            return Ok(String::new());
        }
        let harness = self.inner.registry.resolve(harness_id)?;
        let handle = self.doc_handle(chat_id)?;
        let user_id = message_id.unwrap_or_else(new_id);
        if !idle {
            handle.write_user_message(&user_id, &request.prompt, now_ms())?;
        }

        // Engine-owned resume (zeron sessions.ts:736 — every dispatch read the
        // chat's stored harness session): callers always send `resume: None`;
        // the engine threads the chat's prior harness session back in so a new
        // process (app restart) continues the same harness conversation. The
        // startup-crash retry injects too — a stale id is the harness's
        // problem now (`session/load` falls back to `session/new` internally),
        // and starting the retry fresh silently dropped a good conversation.
        let mut resume_injected = false;
        if request.resume.is_none() {
            request.resume = self.inner.resume_for(chat_id, &request.cwd);
            resume_injected = request.resume.is_some();
        }
        lock(&self.inner.last_requests).insert(chat_id.to_string(), request.clone());

        let run_id = new_id();
        let (steer_tx, steer_rx) = mpsc::channel::<SteerMessage>(32);
        let (cancel_tx, cancel_rx) = watch::channel(false);
        let (engine_tx, engine_rx) = mpsc::unbounded_channel::<AgentEvent>();
        let pending_inputs: PendingInputs = Arc::new(Mutex::new(HashMap::new()));

        // Input bridge: the harness asks questions; we mint the request id, park the
        // resolver for `respond_input`, and surface the event through the run pipeline.
        let request_input = {
            let pending = pending_inputs.clone();
            let engine_tx = engine_tx.clone();
            Box::new(move |questions: Vec<UserInputQuestion>| {
                let (mut tx, rx) = oneshot::channel();
                let (answer_tx, answer_rx) = oneshot::channel();
                let request_id = new_id();
                lock(&pending).insert(request_id.clone(), answer_tx);
                let _ = engine_tx.send(AgentEvent::InputRequested {
                    request_id: request_id.clone(),
                    questions,
                });
                let pending = pending.clone();
                let engine_tx = engine_tx.clone();
                tokio::spawn(async move {
                    tokio::select! {
                        // A dropped resolver stays an error for the harness, as before.
                        answer = answer_rx => if let Ok(answer) = answer { let _ = tx.send(answer); },
                        _ = tx.closed() => {
                            lock(&pending).remove(&request_id);
                            let _ = engine_tx.send(AgentEvent::InputResolved { request_id });
                        }
                    }
                });
                rx
            })
        };
        let interrupt_token = CancellationToken::new();
        let turn = TurnControl::default();
        let started = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let fork_history_sent = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (voice_handle, realtime, _voice_events) = zeron_harness::codex::realtime::channel();
        let voice_active = Arc::new(VoiceActivity::default());
        let controls = RunControls {
            realtime: (harness_id == HarnessId::Codex).then_some(realtime),
            execution_lease: None,
            request_input,
            steering: steer_rx,
            interrupt: interrupt_token.clone(),
            turn: turn.clone(),
            extension_ui: self.inner.extension_ui.port(chat_id),
        };
        self.follow_fleet(chat_id);

        lock(&self.inner.runs).insert(
            chat_id.to_string(),
            RunHandle {
                voice: (harness_id == HarnessId::Codex).then_some(voice_handle),
                voice_active: voice_active.clone(),
                run_id: run_id.clone(),
                steerable: harness.supports_steering(),
                runtime_config: RuntimeConfig::new(harness_id, &request, harness.clone()),
                steer_tx,
                interrupt_token,
                stops_turn_in_place: harness.supports_steering() && harness.stops_turn_in_place(),
                keeps_runtime_alive: harness.supports_steering()
                    && harness.stops_turn_in_place()
                    && harness.keeps_runtime_alive(),
                turn: turn.clone(),
                started: started.clone(),
                cancel: cancel_tx,
                engine_tx,
                pending_inputs,
                routed_steers: Arc::new(Mutex::new(std::collections::VecDeque::new())),
                fork_history_sent: fork_history_sent.clone(),
                retiring: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            },
        );
        // Admission: a fresh turn takes a slot or waits for one. Voice
        // start_idle holds no turn, and routed steers never reach here.
        let admission = if idle {
            None
        } else {
            let depth = self.inner.member_depth(chat_id);
            let tree = self.inner.writer_limit(chat_id);
            let cap = self.inner.max_running_turns();
            let mut gate = lock(&self.inner.admission);
            match gate.admit_in(chat_id, depth, cap, tree) {
                Admit::Now => None,
                Admit::Queued(grant) => {
                    tracing::info!(
                        chat = %chat_id,
                        position = gate.position(chat_id),
                        running = gate.running(),
                        cap,
                        "running-turn cap reached: start queued"
                    );
                    Some(grant)
                }
            }
        };
        self.set_status(
            chat_id,
            if idle {
                SessionStatus::Idle
            } else {
                SessionStatus::Working
            },
            true,
        );
        if admission.is_some() {
            // A queued member can move ahead of waiters already listed.
            self.inner.publish_queue_positions();
        }
        // AFTER Working (same causal-order guarantee as the steer path): the
        // lastMessageAt bump must never be observable ahead of the live run.
        if !idle {
            self.inner.note_message(chat_id, &request.prompt);
        }

        // Name the chat NOW, off the first prompt — not after the first
        // exchange completes ("called New session for a long time for no
        // reason"; the titler only needs the prompt and skips titled chats;
        // the Done-time call below stays as the retry for a failed
        // generation).
        if !idle && let Some(titles) = self.inner.titles.get() {
            titles.maybe_generate(chat_id, harness_id, &request.prompt, &request.cwd);
        }

        crate::lag_monitor::ensure_started();
        tokio::spawn(drive_run(
            self.inner.clone(),
            chat_id.to_string(),
            run_id.clone(),
            harness,
            request,
            handle.writer(),
            controls,
            engine_rx,
            cancel_rx,
            RunResumeState {
                idle,
                voice_active,
                user_message_id: user_id,
                resume_injected,
                startup_retry,
                fork_history_sent,
                started,
                admission,
            },
            turn,
        ));
        Ok(run_id)
    }

    /// A warm send's prompt with the fork's copied history in front, when the
    /// live provider session never received it (the fork's first turns were
    /// native commands) and no earlier send in this runtime carries it
    /// (`sent`). `None` = send the prompt as is.
    fn warm_fork_history(
        &self,
        chat_id: &str,
        harness_id: HarnessId,
        prompt: &str,
        sent: &std::sync::atomic::AtomicBool,
    ) -> Option<String> {
        if sent.load(std::sync::atomic::Ordering::Acquire) {
            return None;
        }
        let session = lock(&self.inner.harness_sessions)
            .get(chat_id)
            .map(|known| known.session_id.clone())
            .filter(|id| !id.is_empty());
        let handle = self.doc_handle(chat_id).ok()?;
        self.inner.fork_history_prompt(
            chat_id,
            handle.doc(),
            harness_id,
            prompt,
            None,
            ProviderSession::Continued(session.as_deref()),
            None,
        )
    }

    /// Push a steer prompt into the live run's mailbox. `NotSteerable` when no live
    /// steerable run exists — the caller (command executor) dispatches a new turn.
    pub async fn steer(
        &self,
        chat_id: &str,
        prompt: &str,
        message_id: Option<String>,
    ) -> Result<SteerOutcome, EngineError> {
        self.steer_at(chat_id, prompt, message_id, now_ms()).await
    }

    pub(crate) async fn steer_at(
        &self,
        chat_id: &str,
        prompt: &str,
        message_id: Option<String>,
        issued_at: i64,
    ) -> Result<SteerOutcome, EngineError> {
        let target = lock(&self.inner.runs)
            .get(chat_id)
            .filter(|h| h.steerable && !h.retiring.load(std::sync::atomic::Ordering::Acquire))
            .map(|h| {
                (
                    h.run_id.clone(),
                    h.runtime_config.harness_id,
                    h.steer_tx.clone(),
                    h.routed_steers.clone(),
                    h.fork_history_sent.clone(),
                    h.retiring.clone(),
                )
            });
        let Some((run_id, harness_id, steer_tx, ledger, history_sent, retiring)) = target else {
            return Ok(SteerOutcome::NotSteerable);
        };
        zeron_proto::invocation::validate_harness_invocations(prompt, harness_id)
            .map_err(EngineError::Other)?;
        let user_id = message_id.unwrap_or_else(new_id);
        let bootstrap = self.warm_fork_history(chat_id, harness_id, prompt, &history_sent);
        let delivered = bootstrap.as_deref().unwrap_or(prompt);
        // A steer runs on the configuration live now; record it with the
        // message so a re-dispatch keeps it.
        let sent_with = self.last_request(chat_id).map(|mut request| {
            request.prompt = prompt.to_string();
            request.attachments = Vec::new();
            Box::new(request)
        });
        let message = SteerMessage {
            prompt: if harness_id == HarnessId::Opencode {
                delivered.to_owned()
            } else {
                zeron_proto::invocation::harness_prompt(delivered, harness_id)
            },
            message_id: Some(user_id.clone()),
            attachments: Vec::new(),
            config: None,
        };
        // Saturation is backpressure, not a dead runtime. Waiting for room
        // preserves the live process and every accepted message in a burst.
        let Ok(permit) = steer_tx.reserve().await else {
            return Ok(SteerOutcome::NotSteerable);
        };
        let accepted = self.inner.registry.while_update_clear(harness_id, || {
            // Serialize mailbox acceptance with confirmation and Done-time
            // inspection: a fast consumer must never outrun its ledger entry.
            // The update marker is checked in the same critical section, so an
            // accepted update releases the reserved slot instead.
            let mut pending = lock(&ledger);
            pending.push_back(RoutedSteer {
                prompt: prompt.to_string(),
                message_id: user_id.clone(),
                fork_history: bootstrap.is_some(),
                after_stop: retiring.load(std::sync::atomic::Ordering::Acquire),
                request: sent_with,
            });
            permit.send(message);
        });
        if accepted.is_none() {
            return Ok(SteerOutcome::DeferredByUpdate);
        }
        // The mailbox owns the message from here on. A failed transcript write
        // must not surface as an error: the caller would take the steer for
        // undelivered and send it again (a duplicate for the model). The
        // callers write the entry before this too, so it is usually there.
        let written = self.doc_handle(chat_id).and_then(|handle| {
            handle
                .write_user_message(&user_id, prompt, issued_at.min(now_ms()))
                .map_err(EngineError::from)
        });
        if let Err(err) = written {
            tracing::warn!(chat = %chat_id, message = %user_id, error = %err,
                "steered message accepted but its transcript write failed");
        }
        // A routed steer is a turn too. Fired here (not only on the confirmed
        // path) — a reclaim falls back to dispatch, which just re-snapshots.
        if let Some(request) = self.last_request(chat_id) {
            self.note_turn_start(chat_id, &request.cwd);
        }
        if self.is_live(chat_id, &run_id) {
            if bootstrap.is_some() {
                history_sent.store(true, std::sync::atomic::Ordering::Release);
            }
            self.set_status(chat_id, SessionStatus::Working, false);
            self.inner.note_message(chat_id, prompt);
            return Ok(SteerOutcome::Accepted);
        }
        // The run died around the send. Exit drain claimed the entry → its
        // re-dispatch owns the message; still ours → reclaim and report
        // NotSteerable so the executor falls back to a fresh dispatch
        // (same message id — the doc entry dedupes).
        let reclaimed = {
            let mut ledger = lock(&ledger);
            let before = ledger.len();
            ledger.retain(|s| s.message_id != user_id);
            ledger.len() != before
        };
        if reclaimed {
            return Ok(SteerOutcome::NotSteerable);
        }
        self.inner.note_message(chat_id, prompt);
        Ok(SteerOutcome::Accepted)
    }

    /// A Stop acting now cancels these messages, sent before it but still
    /// on their way: one that reaches dispatch after the runtime went down
    /// is dropped rather than run in a fresh runtime.
    pub fn cancel_sent_messages(&self, chat_id: &str, message_ids: Vec<String>) {
        if message_ids.is_empty() {
            return;
        }
        lock(&self.inner.cancelled_sends)
            .entry(chat_id.to_string())
            .or_default()
            .extend(message_ids);
    }

    /// The user's Stop: a hard boundary. The in-flight turn ends `aborted`
    /// and the runtime goes with it — background subagents, background
    /// shells, everything the agent runs. The next message resumes the
    /// conversation in a fresh process. (Sending, steering and switching
    /// models never come here: they keep the runtime and its work.)
    ///
    /// A runtime that lives as long as the app ([`Harness::keeps_runtime_alive`],
    /// Pi) is the exception: Stop ends its turn in place, and its process,
    /// extensions and their background work stay up.
    pub async fn interrupt(&self, chat_id: &str) -> Result<bool, EngineError> {
        let keeps_runtime = lock(&self.inner.runs)
            .get(chat_id)
            .is_some_and(|h| h.keeps_runtime_alive);
        if keeps_runtime {
            return self.stop_turn(chat_id).await;
        }
        self.terminate(chat_id).await
    }

    /// End the in-flight turn so another prompt can take its place ("send
    /// now"), keeping the runtime and its background work. Its streaming
    /// entry is stamped `aborted`; this waits (bounded) for that settlement
    /// so callers observe a consistent doc. Returns whether there was a turn
    /// (or runtime) to stop.
    ///
    /// A driver that [`Harness::stops_turn_in_place`] ends only the turn: the
    /// runtime parks with its background work — subagents, background shells
    /// — still running, and the next prompt routes into it. Drivers without
    /// that, runs whose harness has not started yet, and stops that do not
    /// settle fall back to [`Self::terminate`].
    pub async fn stop_turn(&self, chat_id: &str) -> Result<bool, EngineError> {
        const STOP_SETTLE: std::time::Duration = std::time::Duration::from_secs(15);
        let target = lock(&self.inner.runs)
            .get(chat_id)
            .filter(|h| {
                h.stops_turn_in_place && h.started.load(std::sync::atomic::Ordering::Acquire)
            })
            .map(|h| (h.run_id.clone(), h.turn.clone(), h.pending_inputs.clone()));
        let Some((run_id, turn, pending)) = target else {
            return self.terminate(chat_id).await;
        };
        if !self.turn_in_flight(chat_id) {
            return Ok(false); // parked between turns: nothing to stop
        }
        // Unpark questions first: the stopped turn can never be answered.
        let parked: Vec<_> = lock(&pending).drain().map(|(_, tx)| tx).collect();
        for tx in parked {
            let _ = tx.send(Vec::new());
        }
        turn.stop_turn();
        let deadline = tokio::time::Instant::now() + STOP_SETTLE;
        while tokio::time::Instant::now() < deadline {
            if !self.is_live(chat_id, &run_id) || !self.turn_in_flight(chat_id) {
                return Ok(true);
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        tracing::warn!(chat = %chat_id, "turn stop did not settle; tearing the runtime down");
        self.terminate(chat_id).await
    }

    /// Tear the chat's live runtime down (protocol interrupt, then the
    /// child). The run settles with a synthetic `Done{interrupted}` and its
    /// streaming entry stamped `aborted`; this waits (bounded) for that
    /// settlement. Background work the runtime held dies with it.
    pub async fn terminate(&self, chat_id: &str) -> Result<bool, EngineError> {
        let target = lock(&self.inner.runs).get(chat_id).map(|h| {
            (
                h.run_id.clone(),
                h.interrupt_token.clone(),
                h.cancel.clone(),
                h.pending_inputs.clone(),
                h.routed_steers.clone(),
                h.retiring.clone(),
            )
        });
        let Some((run_id, token, cancel, pending, ledger, retiring)) = target else {
            return Ok(false);
        };
        // From here the runtime takes no prompt: a send now starts a fresh
        // runtime, and one that raced into this mailbox is re-dispatched.
        {
            let _order = lock(&ledger);
            retiring.store(true, std::sync::atomic::Ordering::Release);
        }
        // Publish cancellation BEFORE waking the harness. Otherwise a fast
        // EOF after token.cancel() can beat this watch notification and be
        // classified as an error instead of an interrupted turn.
        let _ = cancel.send(true);
        // Unpark questions before harness teardown, which can await them.
        let parked: Vec<_> = lock(&pending).drain().map(|(_, tx)| tx).collect();
        for tx in parked {
            let _ = tx.send(Vec::new());
        }
        // Harness-level interrupt (protocol + child teardown) …
        token.cancel();
        // Bounded settle wait (the run task appends Done + stamps `aborted`).
        for _ in 0..500 {
            if !self.is_live(chat_id, &run_id) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        Ok(true)
    }

    /// Drop every per-chat thing a DELETED chat left behind: its session row
    /// (a leftover row reappears in every published status list), its journal
    /// hub, its resume cache, its last request, its cancelled-send ledger and
    /// its live extension UI. Nothing else ever prunes those maps, so a chat
    /// kept them for the life of the process (audit #115). Call after
    /// [`Self::terminate`], whose settlement can still write a row — the
    /// delete is marked FIRST so that settlement cannot resurrect it.
    pub fn forget_chat(&self, chat_id: &str) {
        lock(&self.inner.deleted_chats).mark(chat_id);
        let dropped = {
            // `statuses` before `admission` (lock order); publish inside like
            // every other status change.
            let mut statuses = lock(&self.inner.statuses);
            let dropped = statuses.remove(chat_id).is_some();
            if dropped {
                let list: Vec<Session> = statuses.values().cloned().collect();
                self.inner.sessions_tx.send_replace(list);
            }
            dropped
        };
        lock(&self.inner.hubs).remove(chat_id);
        lock(&self.inner.last_requests).remove(chat_id);
        lock(&self.inner.harness_sessions).remove(chat_id);
        lock(&self.inner.cancelled_sends).remove(chat_id);
        self.inner.extension_ui.forget(chat_id);
        self.inner.forget_admission(chat_id);
        if dropped {
            tracing::info!(chat = %chat_id, "session state dropped with the chat");
        }
    }

    /// The one teardown every chat-DELETION path runs, so no delete route can
    /// forget half of it (audit #115): end the chat's runtime (interrupt, then
    /// the bounded wait for it to settle), drop its per-chat engine state and
    /// its doc. The workspace row belongs to the caller (each site deletes it
    /// under its own policy and error handling), as do the recap cache and the
    /// voice lease, which those sites own.
    pub async fn discard_chat(&self, chat_id: &str) {
        if let Err(err) = self.terminate(chat_id).await {
            tracing::debug!(chat = %chat_id, error = %err, "deleted chat teardown skipped");
        }
        self.forget_chat(chat_id);
        if let Some(host) = self.inner.doc_host() {
            host.purge_chat(chat_id);
        }
    }

    /// Fold an engine-made event (a `show_visual` page) into the chat's live
    /// run, after whatever the harness streamed so far. Returns `false` when
    /// the chat has no run on this device.
    /// Whether the chat has a run on this device (one `inject_event` can reach).
    pub fn has_run(&self, chat_id: &str) -> bool {
        lock(&self.inner.runs).contains_key(chat_id)
    }

    pub fn inject_event(&self, chat_id: &str, event: AgentEvent) -> bool {
        lock(&self.inner.runs)
            .get(chat_id)
            .is_some_and(|h| h.engine_tx.send(event).is_ok())
    }

    /// Resolve a pending `request_input` question set. Returns `false` when no such
    /// request is pending (unknown id, or the run already settled).
    pub fn respond_input(
        &self,
        chat_id: &str,
        request_id: &str,
        answers: Vec<UserInputAnswer>,
    ) -> Result<bool, EngineError> {
        let target = lock(&self.inner.runs)
            .get(chat_id)
            .map(|h| (h.pending_inputs.clone(), h.engine_tx.clone()));
        let Some((pending, engine_tx)) = target else {
            return Ok(false);
        };
        let Some(resolver) = lock(&pending).remove(request_id) else {
            return Ok(false);
        };
        let _ = resolver.send(answers);
        let _ = engine_tx.send(AgentEvent::InputResolved {
            request_id: request_id.to_string(),
        });
        Ok(true)
    }

    /// Boot recovery: for every journal whose last event is not `Done` (a run died
    /// mid-stream), stamp this device's abandoned `streaming` doc entries `aborted`
    /// with a VISIBLE "Run interrupted by engine restart" error part, close the
    /// journal with a synthetic `Done{interrupted}` — and then PICK THE RUN BACK
    /// UP: a fresh crashed turn with revival budget left is re-dispatched against
    /// the remembered harness session (zeron: "not just eulogized";
    /// `MAX_AUTO_RESUME` = 3 consecutive revivals, fresh = crashed < 12h ago).
    ///
    /// The turn is continued, never replayed: the revived run is told the
    /// previous turn was interrupted and to check what it already did
    /// ([`recovery_prompt`]), because a task that committed or pushed before
    /// the crash must not be run again as if it were new. A tree an
    /// orchestrator paused or stopped stays down, and a tree member that is
    /// not resumed tells its parent instead of going quiet.
    pub fn recover_stale(&self) -> Result<usize, EngineError> {
        const MAX_AUTO_RESUME: u32 = 3;
        const RESUME_FRESH_MS: i64 = 12 * 60 * 60 * 1000;

        let stale = self.inner.journal.stale_sessions()?;
        let mut recovered = 0usize;
        for chat_id in stale {
            if lock(&self.inner.runs).contains_key(&chat_id) {
                continue; // a live run owns this journal
            }
            // One unreadable doc must not cost every other crashed session
            // its recovery: log it and move on.
            let handle = match self.doc_handle(&chat_id) {
                Ok(handle) => handle,
                Err(err) => {
                    tracing::warn!(chat = %chat_id, error = %err,
                        "recovery skipped: the chat doc could not be opened");
                    continue;
                }
            };
            // Harness continuity first: the crashed run's session id may only
            // exist in the journal (the debounced workspace-row write may
            // never have landed) — remember it so the revived run resumes the
            // same harness conversation (zeron recoverDraft, sessions.ts:538).
            if let Some((session_id, cwd)) = self.inner.journal_harness_session(&chat_id) {
                self.inner
                    .remember_harness_session(&chat_id, &session_id, &cwd);
            }
            // The turn to continue: the last user message, quoted in the
            // recovery notice the revived run starts from.
            let prompt = handle.doc().read_entries().ok().and_then(|entries| {
                entries
                    .iter()
                    .rev()
                    .find(|e| e.role == MessageRole::User)
                    .and_then(|e| {
                        e.parts.iter().find_map(|p| match p {
                            MessagePart::Text { text, .. } => Some((e.id.clone(), text.clone())),
                            _ => None,
                        })
                    })
            });
            // A burst of steers can die with the run: Pi stamps each user
            // message as it takes it up, so the ones after the last stamp
            // are in the transcript but never reached the model. A harness
            // that steers mid-turn gets all of them back, in order (below).
            let unconsumed = handle.doc().unconsumed_user_entries().unwrap_or_default();
            let attempts = self.inner.journal.resume_attempts(&chat_id);
            // Fresh: the crashed reply's streaming entry is recent — or no
            // reply entry reached disk at all (the doc saves on a debounce,
            // and a kill moments into the turn leaves the user's message as
            // the transcript's last word), and that message is recent.
            let fresh = handle
                .doc()
                .read_entries()
                .ok()
                .and_then(|entries| {
                    entries
                        .iter()
                        .rev()
                        .find(|e| e.status == Some(MessageStatus::Streaming))
                        .or_else(|| entries.last().filter(|e| e.role == MessageRole::User))
                        .map(|e| now_ms() - e.created_at < RESUME_FRESH_MS)
                })
                .unwrap_or(false);
            // A paused or stopped tree stays down: nothing in it starts by itself.
            let held = self.inner.tree_held(&chat_id);
            let will_resume = fresh && prompt.is_some() && attempts < MAX_AUTO_RESUME && !held;
            let not_resumed = if held {
                "its orchestrator tree is paused or stopped"
            } else if attempts >= MAX_AUTO_RESUME {
                "recovery already revived it three times in a row"
            } else {
                "there is no recent turn to continue"
            };

            let note = if will_resume {
                "Run interrupted by engine restart — resuming".to_string()
            } else {
                format!("Run interrupted by engine restart — not resumed: {not_resumed}")
            };
            let done = AgentEvent::Done {
                status: DoneStatus::Interrupted,
                result: None,
                error: Some(note.clone()),
                session_id: None,
                class: None,
            };
            self.inner.publish(&chat_id, &done);
            let stamped = match handle.mark_abandoned_streams(&note) {
                Ok(stamped) => stamped.len(),
                Err(err) => {
                    // The journal is closed above, so this session is over; it
                    // just could not be stamped. Leave it Idle and let the
                    // remaining stale sessions recover.
                    tracing::warn!(chat = %chat_id, error = %err,
                        "recovery skipped: abandoned streams could not be stamped");
                    self.set_status(&chat_id, SessionStatus::Idle, false);
                    continue;
                }
            };
            // A tree member whose recent turn will not be continued failed as
            // far as its parent is concerned: Errored, and the parent is told.
            let member_lost = fresh && !will_resume && !held && self.inner.in_tree(&chat_id);
            self.set_status(
                &chat_id,
                if member_lost {
                    SessionStatus::Errored
                } else {
                    SessionStatus::Idle
                },
                false,
            );
            if member_lost {
                self.inner.parent_notifier_lost(
                    &chat_id,
                    &format!(
                        "The engine stopped unexpectedly (a crash, an out-of-memory kill or a reboot) during this session's turn, and the turn was not resumed because {not_resumed}. Its work so far is in the worktree. Check git status before continuing."
                    ),
                );
            }
            if !will_resume
                && prompt.is_some()
                && let Some(dir) = self.inner.registry.data_dir()
            {
                crate::model_notices::record(
                    &dir,
                    &chat_id,
                    now_ms(),
                    &format!(
                        "The engine stopped unexpectedly (a crash, an out-of-memory kill or a reboot) during your turn, and the turn was not resumed because {not_resumed}."
                    ),
                );
            }
            tracing::info!(chat = %chat_id, stamped, will_resume, attempts, "recovered stale session journal");
            recovered += 1;

            if !will_resume {
                continue;
            }
            let attempt = self.inner.journal.note_resume_attempt(&chat_id);
            let (user_id, prompt_text) = prompt.expect("gated by will_resume");
            let sessions = self.clone();
            tokio::spawn(async move {
                let Some(host) = sessions.inner.doc_host() else {
                    return;
                };
                let request = sessions
                    .last_request(&chat_id)
                    .or_else(|| host.request_from_chat_row(&chat_id, &prompt_text))
                    // Last resort: the journal's own cwd (zeron's draft config)
                    // — a crash can predate the debounced workspace-row write.
                    .or_else(|| {
                        let (_, cwd) = sessions.inner.journal_harness_session(&chat_id)?;
                        Some(RunRequest {
                            mcp: None,
                            prompt: String::new(),
                            harness: None,
                            model: None,
                            reasoning: None,
                            model_options: Default::default(),
                            cwd,
                            sandbox: zeron_proto::SandboxLevel::WorkspaceWrite,
                            auto_approve: false,
                            attachments: Vec::new(),
                            resume: None,
                            worktree: None,
                        })
                    });
                let Some(mut request) = request else {
                    tracing::warn!(chat = %chat_id, "auto-resume skipped: no run config");
                    return;
                };
                request.resume = None; // dispatch re-injects the remembered session
                request.attachments = Vec::new();
                let harness_id = host.harness_for_request(&chat_id, &request);
                // The first unconsumed message restarts the turn, the rest
                // steer it in behind. Without a mid-turn mailbox (or any
                // stamp) it stays the last user message.
                let mut followups = Vec::new();
                let (_, prompt_text) =
                    if sessions.steers_mid_turn(harness_id) && !unconsumed.is_empty() {
                        followups = unconsumed;
                        followups.remove(0)
                    } else {
                        (user_id, prompt_text)
                    };
                // A NEW message: the notice is part of the transcript, and the
                // original request stays the one entry the user sent.
                request.prompt = recovery_prompt(&prompt_text);
                match host
                    .dispatch_with_source_context(
                        &sessions,
                        &chat_id,
                        harness_id,
                        request,
                        Some(new_id()),
                    )
                    .await
                {
                    Ok(_) => {
                        tracing::info!(chat = %chat_id, attempt, "auto-resumed crashed run")
                    }
                    Err(err) => {
                        tracing::warn!(chat = %chat_id, error = %err, "auto-resume dispatch failed");
                        return;
                    }
                }
                // Same ids as the transcript entries, so a steer that lands
                // twice writes nothing twice.
                for (id, text) in followups {
                    match sessions.steer(&chat_id, &text, Some(id.clone())).await {
                        Ok(SteerOutcome::Accepted) => {}
                        Ok(outcome) => tracing::warn!(chat = %chat_id, message = %id, ?outcome,
                            "auto-resume could not re-send an unconsumed message"),
                        Err(err) => tracing::warn!(chat = %chat_id, message = %id, error = %err,
                            "auto-resume could not re-send an unconsumed message"),
                    }
                }
            });
        }
        Ok(recovered)
    }

    /// Graceful shutdown: interrupt every live run so streaming entries settle.
    pub async fn shutdown(&self) {
        self.inner
            .shutting_down
            .store(true, std::sync::atomic::Ordering::Release);
        self.inner.voice.retire();
        let chats: Vec<String> = lock(&self.inner.runs).keys().cloned().collect();
        for chat_id in chats {
            if let Err(err) = self.terminate(&chat_id).await {
                tracing::warn!(chat = %chat_id, error = %err, "shutdown interrupt failed");
            }
        }
    }

    fn is_live(&self, chat_id: &str, run_id: &str) -> bool {
        lock(&self.inner.runs)
            .get(chat_id)
            .is_some_and(|h| h.run_id == run_id)
    }

    fn set_status(&self, chat_id: &str, status: SessionStatus, fresh_start: bool) {
        self.inner.set_status(chat_id, status, fresh_start);
    }
}

impl Inner {
    /// Journal + broadcast one event (the two unconditional legs of the pipeline).
    /// Sanitize before ANY journal/publish path, including nested subagent events.
    /// `cwd` is the run's working directory: images the agent shared from
    /// there (`share_images`) are copied in like generated ones.
    async fn prepare_generated_image(
        &self,
        chat_id: &str,
        cwd: &str,
        event: AgentEvent,
        seen: &mut std::collections::HashSet<String>,
    ) -> Vec<AgentEvent> {
        let mut parents = Vec::new();
        let mut leaf = event;
        while let AgentEvent::Subagent {
            parent_tool_use_id,
            event,
        } = leaf
        {
            parents.push(parent_tool_use_id);
            leaf = *event;
        }
        let events = if let AgentEvent::GeneratedImage { id, path, name, .. } = leaf {
            let key = std::iter::once(chat_id)
                .chain(parents.iter().map(String::as_str))
                .chain(std::iter::once(id.as_str()))
                .collect::<Vec<_>>()
                .join("\0");
            if seen.contains(&key) {
                return vec![];
            }
            let shared = zeron_proto::shared_image_tool(&id).is_some();
            let result = if let Some((uploads, allowed_root)) = self.generated_images.get() {
                let uploads = uploads.clone();
                // A shared image must come from the chat's own folder.
                let root = if shared {
                    std::path::PathBuf::from(cwd)
                } else {
                    allowed_root.clone()
                };
                let stable_key = key.clone();
                tokio::task::spawn_blocking(move || {
                    let path = std::path::Path::new(&path);
                    if shared {
                        // Shown under its own file name, never a path.
                        let name = std::path::Path::new(&name)
                            .file_name()
                            .map(|n| n.to_string_lossy().chars().take(120).collect::<String>())
                            .filter(|n| !n.is_empty())
                            .unwrap_or_else(|| "Image".into());
                        uploads
                            .import_generated_image(path, &root, &stable_key)
                            .map(|image| crate::uploads::ImportedImage { name, ..image })
                    } else {
                        uploads.import_generated_image(path, &root, &stable_key)
                    }
                })
                .await
                .unwrap_or_else(|err| Err(EngineError::Other(err.to_string())))
            } else {
                Err(EngineError::Other(
                    "Generated image intake is not configured".into(),
                ))
            };
            match result {
                Ok(image) => {
                    seen.insert(key);
                    vec![AgentEvent::GeneratedImage {
                        id,
                        path: image.path,
                        name: image.name,
                        mime_type: image.mime_type,
                    }]
                }
                Err(err) => {
                    tracing::warn!(chat = %chat_id, error = %err, "generated image import failed");
                    vec![
                        AgentEvent::ToolResult {
                            id: id
                                .strip_suffix(":image")
                                .or_else(|| zeron_proto::shared_image_tool(&id))
                                .unwrap_or(&id)
                                .into(),
                            is_error: true,
                            output: None,
                            diff: None,
                        },
                        AgentEvent::Error {
                            message: if shared {
                                "Shared image unavailable"
                            } else {
                                "Generated image unavailable"
                            }
                            .into(),
                        },
                    ]
                }
            }
        } else {
            vec![leaf]
        };
        events
            .into_iter()
            .map(|mut event| {
                for parent in parents.iter().rev() {
                    event = AgentEvent::Subagent {
                        parent_tool_use_id: parent.clone(),
                        event: Box::new(event),
                    };
                }
                event
            })
            .collect()
    }

    fn publish(&self, chat_id: &str, event: &AgentEvent) -> u64 {
        let seq = match self.journal.append(chat_id, event) {
            Ok(seq) => seq,
            Err(err) => {
                tracing::error!(chat = %chat_id, error = %err, "journal append failed");
                0
            }
        };
        if let Some(hub) = lock(&self.hubs).get(chat_id) {
            let _ = hub.send(JournaledEvent {
                seq,
                event: event.clone(),
            });
        }
        seq
    }

    /// Bump the session's freshness on stream activity WITHOUT a status
    /// transition. Long silent-LOOKING stretches (thinking heartbeats, a big
    /// tool input being generated) still carry events — the UI's 45s
    /// staleness gate must not flip "Working" off mid-run. Throttled: a
    /// workspace-doc mirror per delta would be far too chatty.
    fn touch_session(&self, chat_id: &str) {
        const TOUCH_THROTTLE_MS: i64 = 10_000;
        let now = Utc::now();
        let session = {
            let mut statuses = lock(&self.statuses);
            let Some(entry) = statuses.get_mut(chat_id) else {
                return;
            };
            let age = now
                .signed_duration_since(entry.updated_at)
                .num_milliseconds();
            if age < TOUCH_THROTTLE_MS {
                return;
            }
            entry.updated_at = now;
            let session = entry.clone();
            let list: Vec<Session> = statuses.values().cloned().collect();
            self.sessions_tx.send_replace(list);
            session
        };
        if let Some(ws) = self.workspace() {
            ws.record_session(&session);
        }
    }

    fn set_status(&self, chat_id: &str, status: SessionStatus, fresh_start: bool) {
        self.set_status_with_completion(chat_id, status, fresh_start, None);
    }

    /// Publish which harness subagents of `chat_id` are streaming.
    fn set_running_subagents(&self, chat_id: &str, list: Vec<zeron_proto::RunningSubagent>) {
        self.publish_subagents(chat_id, false, list);
    }

    /// Publish which Pi fleet jobs of `chat_id` are at work.
    fn set_running_fleet(&self, chat_id: &str, list: Vec<zeron_proto::RunningSubagent>) {
        self.publish_subagents(chat_id, true, list);
    }

    /// Publish how many background jobs `chat_id`'s runtime is running, so
    /// a parent sees an idle chat that is still waiting on tests or CI. An
    /// unchanged count is a no-op, mirrored like any status change.
    fn set_background_jobs(&self, chat_id: &str, jobs: u32) {
        let session = {
            let mut statuses = lock(&self.statuses);
            let Some(entry) = statuses.get_mut(chat_id) else {
                return;
            };
            if entry.background_jobs == jobs {
                return;
            }
            entry.background_jobs = jobs;
            entry.updated_at = Utc::now();
            let session = entry.clone();
            let list: Vec<Session> = statuses.values().cloned().collect();
            self.sessions_tx.send_replace(list);
            session
        };
        if let Some(ws) = self.workspace() {
            ws.record_session(&session);
        }
    }

    /// Replace one source's part (the fleet's, or the harness's) of the
    /// subagents `chat_id`'s session row carries. A change is mirrored like
    /// any status transition (it doubles as a heartbeat); an unchanged list
    /// is a no-op, and a chat without a status row has nothing to badge yet.
    fn publish_subagents(
        &self,
        chat_id: &str,
        fleet: bool,
        part: Vec<zeron_proto::RunningSubagent>,
    ) {
        let session = {
            let mut statuses = lock(&self.statuses);
            let Some(entry) = statuses.get_mut(chat_id) else {
                return;
            };
            let list = merge_subagents(&entry.subagents, fleet, part);
            let running = u32::try_from(list.len()).unwrap_or(u32::MAX);
            if entry.running_subagents == running && entry.subagents == list {
                return;
            }
            entry.running_subagents = running;
            entry.subagents = list;
            entry.updated_at = Utc::now();
            let session = entry.clone();
            let list: Vec<Session> = statuses.values().cloned().collect();
            self.sessions_tx.send_replace(list);
            session
        };
        if let Some(ws) = self.workspace() {
            ws.record_session(&session);
        }
    }

    /// Publish (or clear) the harness's automatic retry on `chat_id`'s
    /// session row. The status stays Working; a retry only rides a Working
    /// row, so a late frame never marks a settled chat as retrying. Mirrored
    /// like any status change; an unchanged value is a no-op.
    fn set_retry(&self, chat_id: &str, retry: Option<zeron_proto::RetryState>) {
        let session = {
            let mut statuses = lock(&self.statuses);
            let Some(entry) = statuses.get_mut(chat_id) else {
                return;
            };
            let retry = retry.filter(|_| entry.status == SessionStatus::Working);
            if entry.retry == retry {
                return;
            }
            entry.retry = retry;
            entry.updated_at = Utc::now();
            let session = entry.clone();
            let list: Vec<Session> = statuses.values().cloned().collect();
            self.sessions_tx.send_replace(list);
            session
        };
        if let Some(ws) = self.workspace() {
            ws.record_session(&session);
        }
    }

    /// What a runtime that just died took with it, in words ("2 background
    /// jobs and 1 sub-agent"): the work its session row still showed, or the
    /// run's own sub-agent sinks. `None` when it held nothing (an idle Pi
    /// nobody waited on dying is no loss).
    fn lost_work(
        &self,
        chat_id: &str,
        live_subagents: usize,
        background_live: bool,
    ) -> Option<String> {
        let (jobs, agents) = lock(&self.statuses)
            .get(chat_id)
            .map_or((0, 0), |s| (s.background_jobs, s.running_subagents));
        let agents = agents.max(u32::try_from(live_subagents).unwrap_or(u32::MAX));
        // The runtime says it had work but the row had not counted it yet.
        let jobs = if jobs == 0 && agents == 0 && background_live {
            1
        } else {
            jobs
        };
        describe_lost_work(jobs, agents)
    }

    /// Whether `chat_id` was deleted from the workspace in this process; its
    /// late-settling run must write no status row and start no new turn.
    fn is_deleted(&self, chat_id: &str) -> bool {
        lock(&self.deleted_chats).contains(chat_id)
    }

    fn has_pending_steers(&self, chat_id: &str, run_id: &str) -> bool {
        lock(&self.runs)
            .get(chat_id)
            .filter(|h| h.run_id == run_id)
            .is_some_and(|h| !lock(&h.routed_steers).is_empty())
    }

    /// Publish the completion marker atomically with the settled status. Keep
    /// it on subsequent Working/heartbeat rows for coalesced and remote watches.
    fn set_status_with_completion(
        &self,
        chat_id: &str,
        status: SessionStatus,
        fresh_start: bool,
        completed_turn: Option<String>,
    ) {
        // A chat deleted while its run was still settling: `forget_chat`
        // dropped the row and the doc, so this late write must not put any of
        // it back (audit #115).
        if self.is_deleted(chat_id) {
            return;
        }
        let now = Utc::now();
        let settled_turn = completed_turn.clone();
        let prev;
        // The running-turn slot follows the row: Working holds one (a queued
        // chat keeps waiting instead), a settled or question-parked turn
        // frees it. Done before the statuses lock (lock order).
        let queue_changed = self.account_slot(chat_id, status);
        let session = {
            let mut statuses = lock(&self.statuses);
            prev = statuses.get(chat_id).map(|s| s.status);
            let entry = statuses
                .entry(chat_id.to_string())
                .or_insert_with(|| Session {
                    last_completed_turn: None,
                    chat_id: chat_id.to_string(),
                    device_id: self.device_id.clone(),
                    status,
                    started_at: None,
                    updated_at: now,
                    retry: None,
                    running_subagents: 0,
                    queue_position: 0,
                    background_jobs: 0,
                    subagents: Vec::new(),
                });
            // `started_at` is the elapsed-timer base and must only ever mean
            // "this turn". Entering Working from a settled state always
            // restamps (a steer into a parked session is a NEW turn — reusing
            // the old base showed the previous turn's 30min on send), and a
            // settled session drops its base entirely so no later reader can
            // resurrect a stale elapsed.
            let was_active = matches!(
                entry.status,
                SessionStatus::Working | SessionStatus::AwaitingInput
            );
            if let Some(turn) = completed_turn {
                entry.last_completed_turn = Some(turn);
            }
            entry.status = status;
            entry.updated_at = now;
            entry.queue_position = if status == SessionStatus::Working {
                lock(&self.admission).position(chat_id)
            } else {
                0
            };
            if status != SessionStatus::Working || fresh_start {
                entry.retry = None;
            }
            match status {
                SessionStatus::Working if fresh_start || !was_active => {
                    entry.started_at = Some(now);
                }
                SessionStatus::Working | SessionStatus::AwaitingInput => {}
                SessionStatus::Idle | SessionStatus::Errored => {
                    entry.started_at = None;
                }
            }
            let session = entry.clone();
            let list: Vec<Session> = statuses.values().cloned().collect();
            // send_replace: keep the current value fresh even with no receivers,
            // so late WatchSessions subscribers see the last transition.
            self.sessions_tx.send_replace(list);
            session
        };
        // Mirror the transition into the workspace doc's session-status row so
        // remote devices' sidebars show this run (staleness-checked client-side).
        if let Some(ws) = self.workspace() {
            ws.record_session(&session);
        }
        self.parent_notifier_note(chat_id, prev, status, settled_turn.as_deref());
        // A turn ENDING is not a doc change the chat's queue can see, so tell
        // the doc host to re-drain its held messages. Notified HERE, on the
        // producer, rather than watched off the published list: that list
        // arrives over a coalescing watch, and a turn that started and ended
        // while another chat's drain was in flight would never be observed
        // (audit #116).
        if !matches!(
            status,
            SessionStatus::Working | SessionStatus::AwaitingInput
        ) && let Some(host) = self.doc_host()
        {
            host.notify_turn_end(chat_id);
        }
        // A freed slot moved the queue: the waiters' rows show it.
        if queue_changed {
            self.publish_queue_positions();
        }
    }

    // ── admission ───────────────────────────────────────────────────────────

    /// The global cap on concurrently running turns: `ZERON_MAX_RUNNING_TURNS`,
    /// else `maxRunningTurns` in `harness-prefs.json`, else a default scaled
    /// to the machine. 0 = unlimited. Read on every admit/release, so it is
    /// live.
    fn max_running_turns(&self) -> usize {
        std::env::var("ZERON_MAX_RUNNING_TURNS")
            .ok()
            .and_then(|v| v.trim().parse::<usize>().ok())
            .or(self.registry.capacity_prefs().max_running_turns)
            .unwrap_or_else(default_max_running_turns)
    }

    /// `chat_id`'s depth below its orchestrator root when it is a tree member.
    fn member_depth(&self, chat_id: &str) -> Option<usize> {
        match zeron_proto::orchestrator::Place::of(&self.lineage(chat_id), chat_id).role {
            zeron_proto::orchestrator::Role::Member { depth } => Some(depth),
            _ => None,
        }
    }

    /// The limit on concurrent writers of the orchestrator tree `chat_id`
    /// writes in, when its user set one. Only a member that may write counts:
    /// the root stays Working while it waits on its children, and a reader
    /// changes nothing.
    fn writer_limit(&self, chat_id: &str) -> Option<crate::admission::TreeLimit> {
        self.member_depth(chat_id)?;
        let workspace = self.workspace()?;
        let chat = workspace.chat(chat_id).ok().flatten()?;
        let writes = chat
            .config
            .as_ref()
            .is_some_and(|config| config.sandbox != zeron_proto::SandboxLevel::ReadOnly);
        if !writes {
            return None;
        }
        let root = zeron_proto::orchestrator::Place::of(&self.lineage(chat_id), chat_id).root_id?;
        let limit = workspace.orchestrator_project(&root).policy.writer_limit?;
        Some(crate::admission::TreeLimit {
            root,
            limit: usize::from(limit),
        })
    }

    /// Keep the admission slots in step with a status transition; whether the
    /// queue changed (so its rows need republishing).
    fn account_slot(&self, chat_id: &str, status: SessionStatus) -> bool {
        match status {
            SessionStatus::Working => {
                if !lock(&self.admission).tracks(chat_id) {
                    let depth = self.member_depth(chat_id);
                    let tree = self.writer_limit(chat_id).map(|tree| tree.root);
                    lock(&self.admission).mark_running_in(chat_id, depth, tree);
                }
                false
            }
            SessionStatus::AwaitingInput | SessionStatus::Idle | SessionStatus::Errored => {
                let cap = self.max_running_turns();
                lock(&self.admission).release(chat_id, cap)
            }
        }
    }

    /// Drop every slot and wait of `chat_id` (its run is gone).
    fn forget_admission(&self, chat_id: &str) {
        let cap = self.max_running_turns();
        if lock(&self.admission).forget(chat_id, cap) {
            self.publish_queue_positions();
        }
    }

    /// Withdraw a queued start (it was stopped before it began).
    fn cancel_admission(&self, chat_id: &str) {
        if lock(&self.admission).cancel(chat_id) {
            self.publish_queue_positions();
        }
    }

    /// Bring every session row's `queue_position` in line with the queue.
    fn publish_queue_positions(&self) {
        let changed: Vec<Session> = {
            let mut statuses = lock(&self.statuses);
            let positions = lock(&self.admission).positions();
            let now = Utc::now();
            let mut changed = Vec::new();
            for entry in statuses.values_mut() {
                let position = positions.get(&entry.chat_id).copied().unwrap_or(0);
                if entry.queue_position != position {
                    entry.queue_position = position;
                    entry.updated_at = now;
                    changed.push(entry.clone());
                }
            }
            if changed.is_empty() {
                return;
            }
            let list: Vec<Session> = statuses.values().cloned().collect();
            self.sessions_tx.send_replace(list);
            changed
        };
        if let Some(ws) = self.workspace() {
            for session in &changed {
                ws.record_session(session);
            }
        }
    }

    /// A granted start begins its turn now: the timer excludes the queue wait.
    fn restart_turn_clock(&self, chat_id: &str) {
        let session = {
            let mut statuses = lock(&self.statuses);
            let Some(entry) = statuses
                .get_mut(chat_id)
                .filter(|e| e.status == SessionStatus::Working)
            else {
                return;
            };
            let now = Utc::now();
            entry.started_at = Some(now);
            entry.updated_at = now;
            entry.queue_position = 0;
            let session = entry.clone();
            let list: Vec<Session> = statuses.values().cloned().collect();
            self.sessions_tx.send_replace(list);
            session
        };
        if let Some(ws) = self.workspace() {
            ws.record_session(&session);
        }
    }

    /// The doc host, once wired. `None` before assembly or after retirement.
    fn doc_host(&self) -> Option<DocHost> {
        lock(&self.doc_host).clone()
    }

    /// A message the user already sent follows the turn that is ending
    /// (see [`crate::doc_host::ChatDocHandle::continues_with_sent_message`]).
    fn continues_with_sent_message(&self, chat_id: &str) -> bool {
        self.doc_host()
            .and_then(|host| host.open(chat_id).ok())
            .is_some_and(|handle| handle.continues_with_sent_message())
    }

    /// Zeron's own MCP server for a run of `chat_id`: this binary's `zeron
    /// mcp` subcommand, dialing the engine's IPC port and stamped with the
    /// originating chat + device so the agent's side chats link back here.
    /// None when the engine serves no port or its executable is unknown.
    fn zeron_mcp(&self, chat_id: &str) -> Option<zeron_proto::McpServer> {
        let port = self.ipc_port.load(std::sync::atomic::Ordering::Relaxed);
        if port == 0 {
            return None;
        }
        let command = std::env::current_exe().ok()?.to_str()?.to_owned();
        let mut env: std::collections::BTreeMap<String, String> = [
            ("ZERON_IPC_PORT".to_owned(), port.to_string()),
            ("ZERON_CHAT_ID".to_owned(), chat_id.to_owned()),
            ("ZERON_DEVICE_ID".to_owned(), self.device_id.clone()),
        ]
        .into_iter()
        .collect();
        // Who the run is and who it answers to: role, task label, parent
        // and orchestrator (see `Place`).
        let place = zeron_proto::orchestrator::Place::of(&self.lineage(chat_id), chat_id);
        env.extend(
            place
                .env()
                .into_iter()
                .map(|(key, value)| (key.to_owned(), value)),
        );
        Some(zeron_proto::McpServer {
            name: "zeron".into(),
            command,
            args: vec!["mcp".into()],
            env,
        })
    }

    /// `chat_id` and its ancestors, nearest first: enough to place a run in
    /// an orchestrator tree without reading every chat.
    fn lineage(&self, chat_id: &str) -> Vec<zeron_proto::Chat> {
        let Some(workspace) = self.workspace() else {
            return Vec::new();
        };
        let mut lineage: Vec<zeron_proto::Chat> = Vec::new();
        let mut next = Some(chat_id.to_owned());
        while let Some(id) = next.take() {
            if lineage.len() > zeron_proto::orchestrator::MAX_DEPTH + 1
                || lineage.iter().any(|chat| chat.id == id)
            {
                break;
            }
            let Ok(Some(chat)) = workspace.chat(&id) else {
                break;
            };
            next = chat.parent_chat_id.clone();
            lineage.push(chat);
        }
        lineage
    }

    /// Whether `chat_id` belongs to an orchestrator tree its owner paused or
    /// stopped: nothing in it may start by itself, a revival after a crash
    /// included.
    fn tree_held(&self, chat_id: &str) -> bool {
        let Some(workspace) = self.workspace() else {
            return false;
        };
        zeron_proto::orchestrator::Place::of(&self.lineage(chat_id), chat_id)
            .root_id
            .is_some_and(|root| workspace.orchestrator_project(&root).paused)
    }

    /// Whether `chat_id` is a member below an orchestrator (someone to tell
    /// when it fails), as opposed to a root or an ordinary chat.
    fn in_tree(&self, chat_id: &str) -> bool {
        matches!(
            zeron_proto::orchestrator::Place::of(&self.lineage(chat_id), chat_id).role,
            zeron_proto::orchestrator::Role::Member { .. }
        )
    }

    fn workspace(&self) -> Option<crate::workspace_host::WorkspaceHost> {
        self.doc_host().and_then(|host| host.workspace().cloned())
    }

    /// The project section of a run in an orchestrator's tree: its goal,
    /// standing instructions and memory, as the run's prompt carries them.
    fn project_prompt(&self, chat_id: &str) -> Option<String> {
        let root = zeron_proto::orchestrator::Place::of(&self.lineage(chat_id), chat_id).root_id?;
        let workspace = self.workspace()?;
        workspace
            .orchestrator_project(&root)
            .prompt(&workspace.orchestrator_memory(&root))
    }

    /// The tree policy file of a run in an orchestrator's tree, brought up to
    /// date with its project, for Pi's guard to enforce.
    fn tree_policy_file(&self, chat_id: &str, data_dir: &std::path::Path) -> Option<String> {
        let root = zeron_proto::orchestrator::Place::of(&self.lineage(chat_id), chat_id).root_id?;
        let project = self.workspace()?.orchestrator_project(&root);
        let file = crate::tree_policy::sync(data_dir, &project).ok()?;
        Some(file.to_string_lossy().into_owned())
    }

    /// Sidebar freshness: push a message-persist preview into the chat's workspace row.
    fn note_message(&self, chat_id: &str, text: &str) {
        if text.is_empty() {
            return;
        }
        if let Some(ws) = self.workspace() {
            ws.note_message(chat_id, text);
        }
    }

    /// Record the chat's harness-native session id (and its cwd): live-process
    /// cache plus the durable workspace chat row — the row is what survives an
    /// engine restart (zeron sessions.ts:1039).
    fn remember_harness_session(&self, chat_id: &str, session_id: &str, cwd: &str) {
        if session_id.is_empty() {
            return;
        }
        lock(&self.harness_sessions).insert(
            chat_id.to_string(),
            HarnessSessionRef {
                session_id: session_id.to_string(),
                cwd: cwd.to_string(),
            },
        );
        if let Some(ws) = self.workspace() {
            ws.set_chat_harness_session(chat_id, session_id, cwd);
        }
    }

    // NB: there is deliberately no `forget_harness_session` anymore. The old
    // tombstone fired on "run died before SessionStarted", which — since the
    // ACP conversion made stale ids a harness-internal fallback — only ever
    // meant a child STARTUP failure, and permanently severed good
    // conversations (user incident 2026-08-13). A truly stale id simply
    // yields a fresh session whose SessionStarted overwrites the row.

    /// The session id to resume for a run in `chat_id` launching from `cwd`
    /// (zeron sessions.ts:736, looked up on every dispatch):
    /// live-process cache → workspace chat row → journal scan (the crash path
    /// where the debounced row write never landed — SessionStarted/Done events
    /// are journaled per event, flushed immediately). Cwd-gated throughout:
    /// harness session stores are keyed by cwd, so a session created elsewhere
    /// never rides `--resume`. An empty stored id is the explicit tombstone —
    /// no resume, no falling through to staler sources.
    fn resume_for(&self, chat_id: &str, cwd: &str) -> Option<String> {
        // The same directory, however it is spelled: an agent reports where
        // it runs with symlinks resolved (`/private/var/…` for `/var/…` on
        // macOS), and until a turn's Done restores the request's spelling a
        // mid-turn crash leaves that one stored — a string compare then
        // started the resumed run in a fresh session, its history gone.
        let cwd_ok = |session_cwd: &str| {
            session_cwd.is_empty()
                || session_cwd == cwd
                || std::fs::canonicalize(session_cwd)
                    .is_ok_and(|a| std::fs::canonicalize(cwd).is_ok_and(|b| a == b))
        };
        if let Some(known) = lock(&self.harness_sessions).get(chat_id).cloned() {
            return (!known.session_id.is_empty() && cwd_ok(&known.cwd))
                .then_some(known.session_id);
        }
        if let Some(ws) = self.workspace()
            && let Some((session_id, session_cwd)) = ws.chat_harness_session(chat_id)
        {
            return (!session_id.is_empty() && cwd_ok(session_cwd.as_deref().unwrap_or("")))
                .then_some(session_id);
        }
        let (session_id, session_cwd) = self.journal_harness_session(chat_id)?;
        // Cache the journal hit (memory + row) so later dispatches skip the scan.
        self.remember_harness_session(chat_id, &session_id, &session_cwd);
        cwd_ok(&session_cwd).then_some(session_id)
    }

    /// The last harness session id named anywhere in the chat's journal, with
    /// the cwd of the `SessionStarted` that governs it. `Done.session_id`
    /// inherits the cwd of the most recent `SessionStarted` (same run).
    ///
    /// Walks the journal newest-first and stops at the answer, so boot recovery
    /// doesn't load whole multi-MB journals: the newest non-empty id wins, and a
    /// `Done` id takes the cwd of the nearest earlier `SessionStarted` (any id).
    fn journal_harness_session(&self, chat_id: &str) -> Option<(String, String)> {
        let warn = |err: crate::run_journal::JournalError| {
            tracing::warn!(chat = %chat_id, error = %err, "journal scan for harness session failed");
        };
        let events = match self.journal.events_rev(chat_id) {
            Ok(events) => events,
            Err(err) => {
                warn(err);
                return None;
            }
        };
        // A `Done` id seen first, still waiting for its run's `SessionStarted` cwd.
        let mut done_id: Option<String> = None;
        for item in events {
            let (seq, event) = match item {
                Ok(item) => item,
                Err(err) => {
                    warn(err);
                    return None;
                }
            };
            if seq == 0 {
                continue; // `replay(_, 0)` (the old full scan) never yielded seq 0
            }
            match event {
                AgentEvent::SessionStarted {
                    session_id, cwd, ..
                } => {
                    if let Some(id) = done_id {
                        return Some((id, cwd));
                    }
                    if !session_id.is_empty() {
                        return Some((session_id, cwd));
                    }
                }
                AgentEvent::Done {
                    session_id: Some(session_id),
                    ..
                } if done_id.is_none() && !session_id.is_empty() => {
                    done_id = Some(session_id);
                }
                _ => {}
            }
        }
        done_id.map(|id| (id, String::new()))
    }

    fn remove_run(&self, chat_id: &str, run_id: &str) {
        let mut runs = lock(&self.runs);
        if runs.get(chat_id).is_some_and(|h| h.run_id == run_id) {
            runs.remove(chat_id);
            drop(runs);
            // Belt and braces: a removed run can never leak a slot or a wait.
            self.forget_admission(chat_id);
        }
    }
}

/// Fallback for the running-turn cap: twice the cores, between 8 and 24. Runs
/// are mostly waiting on a model, so cores bound them loosely; the cap exists
/// to keep a runaway orchestrator from spawning dozens of Pi processes.
fn default_max_running_turns() -> usize {
    std::thread::available_parallelism()
        .map_or(8, |n| n.get() * 2)
        .clamp(8, 24)
}

/// Park a queued start until the admission queue grants it a slot. `true` =
/// granted (the turn clock restarts); `false` = stopped or withdrawn while
/// waiting. The row is a Working row, so it heartbeats like a live run.
async fn wait_for_admission(
    inner: &Inner,
    chat_id: &str,
    mut grant: oneshot::Receiver<()>,
    interrupt: &CancellationToken,
) -> bool {
    let queued_at = tokio::time::Instant::now();
    let mut beat = tokio::time::interval(std::time::Duration::from_secs(15));
    beat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            biased;
            _ = interrupt.cancelled() => {
                inner.cancel_admission(chat_id);
                tracing::info!(chat = %chat_id, "queued start stopped before it began");
                return false;
            }
            granted = &mut grant => {
                if granted.is_err() {
                    return false;
                }
                tracing::info!(
                    chat = %chat_id,
                    waited_ms = queued_at.elapsed().as_millis() as u64,
                    "capacity available: queued start admitted"
                );
                inner.restart_turn_clock(chat_id);
                return true;
            }
            _ = beat.tick() => inner.touch_session(chat_id),
        }
    }
}

// ── run task ────────────────────────────────────────────────────────────────

/// A side chat's fork of its parent, fixed at its first run and kept in its
/// doc (`forkParent`): the parent's conversation up to then is its starting
/// context, and nothing the parent does later reaches it.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct ForkParent {
    chat_id: String,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    cwd: Option<String>,
    /// The parent's last completed reply at the fork; `None` = it had none.
    #[serde(default)]
    upto: Option<String>,
    #[serde(default)]
    harness: Option<HarnessId>,
    /// The parent's provider session then, when it lives on this device:
    /// a harness that can fork sessions (Pi `--fork`) starts from it.
    #[serde(default)]
    session: Option<String>,
}

/// The parent's transcript a side chat carries in its first prompt, most
/// recent first within this many characters.
const FORK_HISTORY_BUDGET: usize = 60_000;
/// One tool output in a carried transcript, at most.
const FORK_TOOL_OUTPUT: usize = 2_000;

/// One transcript entry as carried history: its text and tool calls.
fn history_item(entry: &SessionMessageEntry) -> Option<serde_json::Value> {
    let text = entry
        .parts
        .iter()
        .filter_map(|part| match part {
            MessagePart::Text { text, .. } => Some(text.clone()),
            MessagePart::Tool { call, output, .. } => {
                let mut output = output.clone().unwrap_or_default();
                if output.len() > FORK_TOOL_OUTPUT {
                    let mut end = FORK_TOOL_OUTPUT;
                    while !output.is_char_boundary(end) {
                        end -= 1;
                    }
                    output.truncate(end);
                    output.push_str(" [...]");
                }
                Some(format!(
                    "Tool: {}\n{}",
                    serde_json::to_string(call).unwrap_or_default(),
                    output
                ))
            }
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    (!text.is_empty()).then(|| serde_json::json!({ "role": entry.role, "text": text }))
}

/// Which provider session a side chat's prompt goes to.
#[derive(Clone, Copy)]
enum ProviderSession<'a> {
    /// A new provider session: it holds none of the transcript.
    Fresh,
    /// An existing one (id when known): it holds the fork's own turns.
    Continued(Option<&'a str>),
}

/// A run's provider session now holds the fork history its own prompt
/// carried (steers record theirs when confirmed, not here).
fn note_fork_history_session(doc: &SessionDoc, carried: bool, session_id: &str) {
    if carried && !session_id.is_empty() {
        let _ = doc.set_fork_history_session(session_id);
    }
}

impl Inner {
    /// `prompt` with the conversation a side chat's provider session lacks in
    /// front of it, or `None` when it lacks nothing. A fresh session gets the
    /// whole transcript (`current` excluded); a continued one only the fork's
    /// copied history before the seam, until that went out to that session.
    /// Native commands are never wrapped: providers route a leading `/name`.
    /// A fresh session of a side chat that `fork`s its parent also gets the
    /// parent's conversation up to the fork, ahead of its own.
    #[allow(clippy::too_many_arguments)]
    fn fork_history_prompt(
        &self,
        chat_id: &str,
        doc: &SessionDoc,
        harness_id: HarnessId,
        prompt: &str,
        current: Option<&str>,
        provider: ProviderSession<'_>,
        fork: Option<&ForkParent>,
    ) -> Option<String> {
        if native_command(prompt, harness_id) || !self.is_side_chat(chat_id) {
            return None;
        }
        let entries: Vec<_> = doc
            .read_entries()
            .unwrap_or_default()
            .into_iter()
            .filter(|entry| Some(entry.id.as_str()) != current)
            .collect();
        let end = match provider {
            ProviderSession::Fresh => entries.len(),
            ProviderSession::Continued(session) => {
                let delivered = doc.fork_history_session();
                if session.is_some() && delivered.as_deref() == session {
                    return None;
                }
                entries.iter().position(|entry| {
                    entry
                        .parts
                        .iter()
                        .any(|part| matches!(part, zeron_doc::MessagePart::Fork { .. }))
                })?
            }
        };
        let mut history = match (provider, fork) {
            (ProviderSession::Fresh, Some(fork)) => self.parent_history(fork),
            _ => Vec::new(),
        };
        history.extend(entries[..end].iter().filter_map(history_item));
        (!history.is_empty()).then(|| {
            format!(
                "Continue this side conversation using the following prior conversation as context.\n<conversation>\n{}\n</conversation>\n\n{}",
                serde_json::to_string(&history).unwrap_or_default(),
                prompt
            )
        })
    }

    /// The parent's conversation up to the fork, as carried history: a
    /// line naming the parent, then its most recent messages that fit.
    fn parent_history(&self, fork: &ForkParent) -> Vec<serde_json::Value> {
        let parent = match &fork.title {
            Some(title) => format!("\"{title}\" (chat {})", fork.chat_id),
            None => format!("chat {}", fork.chat_id),
        };
        let mut header = format!(
            "The conversation of the parent chat {parent} up to when this side chat was opened"
        );
        if let Some(cwd) = &fork.cwd {
            header.push_str(&format!(", in {cwd}"));
        }
        let entries = fork
            .upto
            .as_deref()
            .and_then(|upto| {
                let entries = self
                    .doc_host()?
                    .open(&fork.chat_id)
                    .ok()?
                    .doc()
                    .read_entries()
                    .ok()?;
                let end = entries.iter().position(|entry| entry.id == upto)?;
                Some(entries[..=end].to_vec())
            })
            .unwrap_or_default();
        let mut items: Vec<_> = entries.iter().filter_map(history_item).collect();
        if items.is_empty() {
            return items;
        }
        let mut used = 0;
        let mut keep = items.len();
        for (at, item) in items.iter().enumerate().rev() {
            let len = item["text"].as_str().map_or(0, str::len);
            if used + len > FORK_HISTORY_BUDGET {
                break;
            }
            used += len;
            keep = at;
        }
        if keep > 0 {
            header.push_str(&format!(
                "; its {keep} earliest messages are left out for length"
            ));
        }
        header.push('.');
        items.drain(..keep);
        let mut history = vec![serde_json::json!({ "role": "system", "text": header })];
        history.extend(items);
        history
    }

    /// A side chat: a chat with a parent, outside an orchestrator's tree —
    /// the same rule [`Inner::fork_parent`] records a fork by. A member of a
    /// tree has a parent too, but it starts from its task and its own
    /// transcript, never as a forked side conversation.
    fn is_side_chat(&self, chat_id: &str) -> bool {
        let place = zeron_proto::orchestrator::Place::of(&self.lineage(chat_id), chat_id);
        place.role == zeron_proto::orchestrator::Role::Chat && place.parent_id.is_some()
    }

    /// The fork a side chat starts from: recorded in its doc at its first
    /// run, read back after. `None` for every chat that is not a side chat
    /// outside an orchestrator's tree (members get their task, not their
    /// parent's conversation), and for one forked by copying (its doc holds
    /// the history already). `true` when this call recorded it: only that
    /// first run may fork the parent's live session, which has moved on
    /// since.
    fn fork_parent(&self, chat_id: &str, doc: &SessionDoc) -> Option<(ForkParent, bool)> {
        if let Some(record) = doc.fork_parent() {
            return serde_json::from_str(&record).ok().map(|fork| (fork, false));
        }
        if !self.is_side_chat(chat_id) {
            return None;
        }
        let lineage = self.lineage(chat_id);
        let place = zeron_proto::orchestrator::Place::of(&lineage, chat_id);
        let parent = lineage
            .iter()
            .find(|chat| Some(&chat.id) == place.parent_id.as_ref())?;
        let entries = doc.read_entries().unwrap_or_default();
        if entries.iter().any(|entry| {
            entry
                .parts
                .iter()
                .any(|part| matches!(part, MessagePart::Fork { .. }))
        }) {
            return None;
        }
        let upto = self
            .doc_host()
            .and_then(|host| host.open(&parent.id).ok())
            .and_then(|handle| handle.doc().read_entries().ok())
            .and_then(|entries| {
                entries
                    .iter()
                    .rev()
                    .find(|entry| {
                        entry.role == MessageRole::Assistant
                            && entry.status == Some(MessageStatus::Complete)
                    })
                    .map(|entry| entry.id.clone())
            });
        let session = (parent.device_id == self.device_id)
            .then(|| {
                lock(&self.harness_sessions)
                    .get(&parent.id)
                    .map(|known| known.session_id.clone())
                    .or_else(|| parent.harness_session_id.clone())
            })
            .flatten()
            .filter(|id| !id.is_empty());
        let fork = ForkParent {
            chat_id: parent.id.clone(),
            title: parent.title.clone(),
            cwd: parent.cwd.clone(),
            upto,
            harness: parent.config.as_ref().map(|config| config.harness),
            session,
        };
        let _ = doc.set_fork_parent(&serde_json::to_string(&fork).ok()?);
        Some((fork, true))
    }
}

/// Whether the provider routes this prompt as a native command: its delivered
/// text leads with `/name` (Codex `command_request`, OpenCode commands, Claude
/// slash commands). Anything put in front of it would make it model input.
fn native_command(prompt: &str, harness: HarnessId) -> bool {
    let delivered = if harness == HarnessId::Codex {
        zeron_proto::invocation::invocation_prompt(prompt)
    } else {
        zeron_proto::invocation::harness_prompt(prompt, harness)
    };
    zeron_proto::invocation::leading_command(&delivered).is_some()
}

/// A turn is in flight: streaming, or parked on a question it is still owed an
/// answer to. Notably NOT a persistent session that has parked between turns —
/// that holds a warm child with nothing outstanding.
fn is_active(session: &Session) -> bool {
    matches!(
        session.status,
        SessionStatus::Working | SessionStatus::AwaitingInput
    )
}

// ── subagent docs ───────────────────────────────────────────────────────────

/// The per-subagent doc id: `{chatId}--sub--{suffix}`. Constrained by the
/// edge's `ID_RE` (`^[A-Za-z0-9_-]{1,128}$` — the same id names the ChatRoom
/// `chat2/{id}/ws` and the frozen blob `blob/{chatId}/{id}`): a clean, short
/// tool-use id rides verbatim; anything unclean or over budget hashes.
pub(crate) fn subagent_doc_id(chat_id: &str, tool_use_id: &str) -> String {
    let clean = tool_use_id
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    let budget = 128usize.saturating_sub(chat_id.len() + "--sub--".len());
    if clean && !tool_use_id.is_empty() && tool_use_id.len() <= budget {
        return format!("{chat_id}--sub--{tool_use_id}");
    }
    use sha2::Digest as _;
    let digest = sha2::Sha256::digest(tool_use_id.as_bytes());
    let mut hex = String::with_capacity(16);
    for b in &digest[..8] {
        use std::fmt::Write as _;
        let _ = write!(hex, "{b:02x}");
    }
    format!("{chat_id}--sub--{hex}")
}

/// Test-only instrumentation: how often `drive_run`'s coalesced commit branch
/// fires. The sink-dirty regression test proves the branch ticks once per
/// commit window instead of spinning on a deadline left in the past.
/// Thread-local so parallel tests' runs can't bleed into the count: the
/// test's current-thread runtime runs its spawned `drive_run` on its own
/// thread.
#[cfg(test)]
thread_local! {
    static FLUSH_TICKS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// A live subagent transcript sink: its own doc (opened by id — the room
/// `chat2/{docId}/ws` dials automatically, so viewers sync it like a chat),
/// one streaming assistant entry folded from the tagged events. The held
/// doc Arc pins the doc warm for the LRU while the subagent runs.
struct SubagentSink {
    doc_id: String,
    /// Its spawn chip's title, for the session row.
    title: String,
    doc: crate::doc_host::DocWriter,
    entry_id: String,
    started_at: i64,
    entry_index: Option<usize>,
    written: Vec<MessagePart>,
    folded: Vec<MessagePart>,
    dirty: bool,
}

/// `current` with one source's part replaced (the Pi fleet's jobs, or the
/// harness's subagents), oldest first.
fn merge_subagents(
    current: &[zeron_proto::RunningSubagent],
    fleet: bool,
    part: Vec<zeron_proto::RunningSubagent>,
) -> Vec<zeron_proto::RunningSubagent> {
    let mut list: Vec<zeron_proto::RunningSubagent> = current
        .iter()
        .filter(|sub| sub.fleet_row.is_some() != fleet)
        .cloned()
        .chain(part)
        .collect();
    list.sort_by(|a, b| {
        a.started_at
            .cmp(&b.started_at)
            .then_with(|| a.doc_id.cmp(&b.doc_id))
            .then_with(|| a.fleet_row.cmp(&b.fleet_row))
    });
    list
}

/// The fleet's jobs as the session row carries them, timed from the
/// roster's channel, else from when `seen` first met them (by title: a
/// job's row id changes as its chat goes between watched and not).
fn fleet_list(
    jobs: Vec<zeron_proto::extension_ui::FleetJob>,
    seen: &mut HashMap<String, chrono::DateTime<Utc>>,
    now: chrono::DateTime<Utc>,
) -> Vec<zeron_proto::RunningSubagent> {
    seen.retain(|title, _| jobs.iter().any(|job| job.label == *title));
    jobs.into_iter()
        .map(|job| {
            let first = *seen.entry(job.label.clone()).or_insert(now);
            zeron_proto::RunningSubagent {
                doc_id: String::new(),
                started_at: job
                    .started_at_ms
                    .and_then(chrono::DateTime::<Utc>::from_timestamp_millis)
                    .unwrap_or(first),
                title: job.label,
                fleet_row: Some(job.row),
            }
        })
        .collect()
}

/// The streaming subagents, oldest first, as the session row carries them.
fn running_list(
    subagents: &std::collections::HashMap<String, SubagentSink>,
) -> Vec<zeron_proto::RunningSubagent> {
    let mut list: Vec<zeron_proto::RunningSubagent> = subagents
        .values()
        .map(|sink| zeron_proto::RunningSubagent {
            doc_id: sink.doc_id.clone(),
            title: sink.title.clone(),
            started_at: chrono::DateTime::<Utc>::from_timestamp_millis(sink.started_at)
                .unwrap_or_else(Utc::now),
            fleet_row: None,
        })
        .collect();
    list.sort_by(|a, b| {
        a.started_at
            .cmp(&b.started_at)
            .then_with(|| a.doc_id.cmp(&b.doc_id))
    });
    list
}

impl SubagentSink {
    fn flush(&mut self, device_id: &str) {
        if !self.dirty {
            return;
        }
        // An empty fold still counts as flushed: leaving dirty set would hold
        // the run loop's coalesced-commit guard open on a deadline already in
        // the past, spinning it until the subagent emits real content.
        self.dirty = false;
        if self.folded.is_empty() {
            return;
        }
        let rendered = render_parts(&self.folded);
        let result = match self.entry_index {
            Some(ix) => {
                let mut w = SegmentWriter::resume(&self.doc, ix, std::mem::take(&mut self.written));
                let r = w.sync(&rendered);
                let (ix, written) = w.into_state();
                self.entry_index = Some(ix);
                self.written = written;
                r
            }
            None => {
                match SegmentWriter::begin(&self.doc, &self.entry_id, device_id, self.started_at) {
                    Ok(mut w) => {
                        let r = w.sync(&rendered);
                        let (ix, written) = w.into_state();
                        self.entry_index = Some(ix);
                        self.written = written;
                        r
                    }
                    Err(e) => Err(e),
                }
            }
        };
        if let Err(err) = result {
            // Fail soft: a broken subagent doc degrades to chip-only, never
            // errors the chat.
            tracing::warn!(doc = %self.doc_id, error = %err, "subagent sink flush failed");
        }
    }

    /// A parent→subagent steer ([`AgentEvent::UserMessage`], tagged): close
    /// the open assistant segment (it reads Complete above the steer), write
    /// the steer as its own USER entry, and reset so the next tagged delta
    /// opens a fresh assistant entry below it — the subagent transcript then
    /// reads like any steered chat.
    fn push_user(&mut self, device_id: &str, text: &str) {
        let rendered = render_parts(&self.folded);
        let closed = match self.entry_index.take() {
            Some(ix) => SegmentWriter::resume(&self.doc, ix, std::mem::take(&mut self.written))
                .finish(&rendered, MessageStatus::Complete),
            None if !self.folded.is_empty() => {
                match SegmentWriter::begin(&self.doc, &self.entry_id, device_id, self.started_at) {
                    Ok(w) => w.finish(&rendered, MessageStatus::Complete),
                    Err(e) => Err(e),
                }
            }
            None => Ok(()),
        };
        if let Err(err) = closed {
            tracing::warn!(doc = %self.doc_id, error = %err, "subagent segment close failed");
        }
        let entry = SessionMessageEntry {
            id: new_id(),
            role: MessageRole::User,
            parts: vec![MessagePart::Text {
                id: "t0".into(),
                text: text.to_owned(),
            }],
            created_at: now_ms(),
            device_id: device_id.to_owned(),
            status: Some(MessageStatus::Complete),
            continuation_of: None,
            duration_ms: None,
        };
        if let Err(err) = self.doc.push_message(&entry) {
            tracing::warn!(doc = %self.doc_id, error = %err, "subagent steer write failed");
        }
        self.entry_id = new_id();
        self.started_at = now_ms();
        self.written = Vec::new();
        self.folded = Vec::new();
        self.dirty = false;
    }

    /// Final flush + entry finalize; returns the transcript JSON for the
    /// frozen blob (entries as the client renders them).
    fn finish(mut self, device_id: &str, status: MessageStatus) -> Option<String> {
        let rendered = render_parts(&self.folded);
        let finished = match self.entry_index {
            Some(ix) => SegmentWriter::resume(&self.doc, ix, std::mem::take(&mut self.written))
                .finish(&rendered, status),
            None if !self.folded.is_empty() => {
                match SegmentWriter::begin(&self.doc, &self.entry_id, device_id, self.started_at) {
                    Ok(w) => w.finish(&rendered, status),
                    Err(e) => Err(e),
                }
            }
            None => Ok(()),
        };
        if let Err(err) = finished {
            tracing::warn!(doc = %self.doc_id, error = %err, "subagent sink finish failed");
        }
        let entries = zeron_doc::join_continuation_entries(self.doc.read_entries().ok()?);
        serde_json::to_string(&entries).ok()
    }
}

/// The parent-chip refresh a tagged event implies, for the in-place (parked)
/// path — LIFECYCLE ONLY, mirroring the fold (live tails were rejected:
/// per-delta chip rewrites grew the parent doc for the whole run).
fn subagent_chip_update(event: &AgentEvent) -> Option<&'static str> {
    match event {
        AgentEvent::Done { status, .. } => Some(match status {
            DoneStatus::Errored => "failed",
            _ => "done",
        }),
        _ => Some("running"),
    }
}

/// Apply the render-parts privacy policy: strip heavy/sensitive tool inputs before doc
/// entry. Full inputs live only in the local run journal.
fn render_parts(parts: &[MessagePart]) -> Vec<MessagePart> {
    parts
        .iter()
        .map(|part| match part {
            MessagePart::Tool {
                id,
                call,
                is_error,
                resolved,
                output,
                diff,
                output_ref,
                output_bytes,
                diff_ref,
                diff_stats,
                subagent_ref,
                subagent_status,
                subagent_tail,
            } => MessagePart::Tool {
                id: id.clone(),
                call: sanitize_tool_call(call),
                is_error: *is_error,
                resolved: *resolved,
                // Output summaries, diff stats, and sidecar refs are
                // deliberately kept: unlike raw tool inputs they are the
                // transcript's record of what happened, and the strip already
                // bounded them (docs/chat2-sync.md A1).
                output: output.clone(),
                diff: diff.clone(),
                output_ref: output_ref.clone(),
                output_bytes: *output_bytes,
                diff_ref: diff_ref.clone(),
                diff_stats: diff_stats.clone(),
                subagent_ref: subagent_ref.clone(),
                subagent_status: *subagent_status,
                subagent_tail: subagent_tail.clone(),
            },
            other => other.clone(),
        })
        .collect()
}

/// The persisted assistant text of a folded segment (workspace preview source).
fn folded_text(parts: &[MessagePart]) -> String {
    parts
        .iter()
        .filter_map(|part| match part {
            MessagePart::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Longest part of the interrupted request quoted back in a recovery notice.
const RECOVERY_QUOTE_MAX_CHARS: usize = 4000;

/// What a revived run is told instead of the interrupted request itself. The
/// turn it continues may have committed, pushed or deleted before it died,
/// so running the request again is not the same as continuing it.
fn recovery_prompt(request: &str) -> String {
    let request = request.trim();
    let quoted: String = request.chars().take(RECOVERY_QUOTE_MAX_CHARS).collect();
    let cut = if quoted.len() < request.len() {
        "\n> [cut]"
    } else {
        ""
    };
    let quoted = quoted
        .lines()
        .map(|line| format!("> {line}"))
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "[Message from Zeron (automatic recovery notice). Not the user. The previous turn \
         was interrupted when the engine stopped unexpectedly (a crash, an out-of-memory \
         kill or a reboot). Review the transcript and run git status before acting: do not \
         repeat steps that already completed, and continue only what is unfinished.]\n\n\
         The request that was interrupted:\n{quoted}{cut}"
    )
}

/// Tool calls of the current segment that never got a result, one short
/// line each, for a crash notice.
fn unfinished_tools(parts: &[MessagePart]) -> Vec<String> {
    const MAX_CHARS: usize = 200;
    parts
        .iter()
        .filter_map(|part| match part {
            MessagePart::Tool {
                call,
                resolved: false,
                ..
            } => Some(match call {
                zeron_proto::ToolCall::Exec { command } => {
                    let command: String = command.chars().take(MAX_CHARS).collect();
                    format!("`{command}`")
                }
                other => {
                    let value = serde_json::to_value(other).unwrap_or_default();
                    let kind = value["kind"].as_str().unwrap_or("tool");
                    match value["path"].as_str() {
                        Some(path) => format!("{kind} {path}"),
                        None => kind.to_owned(),
                    }
                }
            }),
            _ => None,
        })
        .collect()
}

/// "2 background jobs and 1 sub-agent" for the work a dead runtime held;
/// `None` for none.
fn describe_lost_work(jobs: u32, agents: u32) -> Option<String> {
    let plural = |n: u32, one: &str, many: &str| format!("{n} {}", if n == 1 { one } else { many });
    match (jobs, agents) {
        (0, 0) => None,
        (jobs, 0) => Some(plural(jobs, "background job", "background jobs")),
        (0, agents) => Some(plural(agents, "sub-agent", "sub-agents")),
        (jobs, agents) => Some(format!(
            "{} and {}",
            plural(jobs, "background job", "background jobs"),
            plural(agents, "sub-agent", "sub-agents")
        )),
    }
}

fn sync_segment<'a>(
    doc: &'a SessionDoc,
    writer: &mut Option<SegmentWriter<'a>>,
    entry_id: &str,
    device_id: &str,
    started_at: i64,
    folded: &[MessagePart],
) -> Result<(), DocError> {
    if folded.is_empty() {
        return Ok(());
    }
    let rendered = render_parts(folded);
    if writer.is_none() {
        *writer = Some(SegmentWriter::begin(doc, entry_id, device_id, started_at)?);
    }
    if let Some(w) = writer.as_mut() {
        w.sync(&rendered)?;
    }
    Ok(())
}

fn finish_segment<'a>(
    doc: &'a SessionDoc,
    writer: Option<SegmentWriter<'a>>,
    entry_id: &str,
    device_id: &str,
    started_at: i64,
    folded: &[MessagePart],
    status: MessageStatus,
) -> Result<(), DocError> {
    let rendered = render_parts(folded);
    match writer {
        Some(w) => w.finish(&rendered, status),
        None if !folded.is_empty() => {
            SegmentWriter::begin(doc, entry_id, device_id, started_at)?.finish(&rendered, status)
        }
        None => Ok(()),
    }
}

/// Resume bookkeeping for one run task: which user entry the run answers (so
/// the startup-crash retry re-dispatches idempotently against the same doc
/// entry), whether `dispatch` injected the resume id itself (only
/// engine-injected resumes retry — a caller-specified resume fails loudly),
/// and whether this run already IS the retry (one attempt only).
struct RunResumeState {
    idle: bool,
    voice_active: Arc<VoiceActivity>,
    user_message_id: String,
    resume_injected: bool,
    startup_retry: bool,
    fork_history_sent: Arc<std::sync::atomic::AtomicBool>,
    /// Set once the harness stream is open (see [`RunHandle::started`]).
    started: Arc<std::sync::atomic::AtomicBool>,
    /// The start is queued behind the running-turn cap: fires when a slot is
    /// granted. `None` = the run already holds its slot.
    admission: Option<oneshot::Receiver<()>>,
}

/// Default idle window of a Pi tree member's runtime. Deliberately long: a
/// parked Pi carries extensions, subagents and background shells, and the
/// reaper already holds off while any of them report live work.
const DEFAULT_MEMBER_IDLE: std::time::Duration = std::time::Duration::from_secs(20 * 60);

/// Default idle window of any parked Pi runtime that finished its work.
/// Short on purpose: an idle Pi holds a process and its extensions, while
/// the next message restarts it with the session resumed.
const DEFAULT_FINISHED_IDLE: std::time::Duration = std::time::Duration::from_secs(10);

/// How long a parked Pi runtime with nothing left running may sit idle
/// before it is stopped: `ZERON_FINISHED_IDLE_MS` (0 disables) >
/// `finishedIdleSecs` in `harness-prefs.json` (0 disables) >
/// [`DEFAULT_FINISHED_IDLE`]. `None` = never.
fn finished_idle_ttl(registry: &HarnessRegistry) -> Option<std::time::Duration> {
    let window = match std::env::var("ZERON_FINISHED_IDLE_MS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
    {
        Some(ms) => std::time::Duration::from_millis(ms),
        None => registry
            .capacity_prefs()
            .finished_idle_secs
            .map_or(DEFAULT_FINISHED_IDLE, std::time::Duration::from_secs),
    };
    (!window.is_zero()).then_some(window)
}

/// How long a parked Pi tree member's runtime may sit idle before it is
/// stopped: `ZERON_MEMBER_IDLE_MS` (0 disables) > `memberIdleSecs` in
/// `harness-prefs.json` (0 disables) > [`DEFAULT_MEMBER_IDLE`]. `None` = never.
fn member_idle_ttl(registry: &HarnessRegistry) -> Option<std::time::Duration> {
    let window = match std::env::var("ZERON_MEMBER_IDLE_MS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
    {
        Some(ms) => std::time::Duration::from_millis(ms),
        None => registry
            .capacity_prefs()
            .member_idle_secs
            .map_or(DEFAULT_MEMBER_IDLE, std::time::Duration::from_secs),
    };
    (!window.is_zero()).then_some(window)
}

fn cursor_unstarted_history(
    doc: &SessionDoc,
    current_id: &str,
    prompt: &str,
    has_session: bool,
) -> Result<String, DocError> {
    // Convert each message before JSON encoding. Rewriting canonical chips in
    // the encoded envelope can introduce unescaped quotes or newlines and can
    // cause Cursor's current message to be converted twice.
    let prompt = zeron_proto::invocation::harness_prompt(prompt, HarnessId::Cursor);
    let entries = doc.read_entries()?;
    let preceding: Vec<_> = entries
        .iter()
        .take_while(|entry| entry.id != current_id)
        .collect();
    // With an existing session, only bridge the tail whose assistant never
    // produced content. A process can die before writing its SDK-side receipt,
    // even though an older session ID still exists.
    let after = if has_session {
        preceding
            .iter()
            .rposition(|entry| {
                entry.role == MessageRole::Assistant
                    && entry.parts.iter().any(|part| match part {
                        MessagePart::Text { text, .. } | MessagePart::Reasoning { text, .. } => {
                            !text.is_empty()
                        }
                        MessagePart::Tool { .. } | MessagePart::Input { .. } => true,
                        _ => false,
                    })
            })
            .map_or(0, |i| i + 1)
    } else {
        0
    };
    let previous: Vec<String> = preceding
        .iter()
        .skip(after)
        .filter(|entry| entry.role == MessageRole::User)
        .map(|entry| {
            entry
                .parts
                .iter()
                .filter_map(|part| match part {
                    MessagePart::Text { text, .. } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n")
        })
        .filter(|text| !text.is_empty())
        .map(|text| zeron_proto::invocation::harness_prompt(&text, HarnessId::Cursor))
        .collect();
    if previous.is_empty() {
        return Ok(prompt);
    }
    Ok(format!(
        "The preceding user messages may not have reached a Cursor checkpoint before startup stopped. Retain this JSON as conversation history; do not rerun prior tools or side effects. Respond to the current message.\n{}",
        serde_json::json!({"previousUserMessages": previous, "currentUserMessage": prompt})
    ))
}

#[allow(clippy::too_many_arguments)]
/// One `zeron::turn_trace` line for a finished turn (opt-in, see
/// `zeron_harness::turn_trace`). Absent phases are omitted.
fn log_turn_trace(
    chat_id: &str,
    run_id: &str,
    harness_id: HarnessId,
    status: &str,
    t: zeron_harness::turn_trace::TurnTimings,
) {
    tracing::info!(
        target: "zeron::turn_trace",
        chat = %chat_id,
        run = %run_id,
        harness = ?harness_id,
        status,
        prompt_sent_ms = 0u64,
        ack_ms = ?t.ack_ms,
        barrier_ms = ?t.barrier_ms,
        first_event_ms = ?t.first_event_ms,
        first_commit_ms = ?t.first_commit_ms,
        done_ms = t.done_ms,
        "turn trace"
    );
}

async fn drive_run(
    inner: Arc<Inner>,
    chat_id: String,
    run_id: String,
    harness: Arc<dyn Harness>,
    mut request: RunRequest,
    doc: crate::doc_host::DocWriter,
    mut controls: RunControls,
    mut engine_rx: mpsc::UnboundedReceiver<AgentEvent>,
    mut cancel_rx: watch::Receiver<bool>,
    mut resume_state: RunResumeState,
    turn: TurnControl,
) {
    let device_id = inner.device_id.clone();
    // Captured for post-run auto-titling (the request moves into the harness).
    let harness_id = harness.id();
    let user_prompt = request.prompt.clone();
    let run_cwd = request.cwd.clone();
    if request.resume.is_none() {
        let _ = doc.clear_context_usage();
    }
    // The host stamps its own MCP server onto every run it drives, so the
    // agent can spawn and talk to side chats through the engine it runs in.
    if request.mcp.is_none() {
        request.mcp = inner.zeron_mcp(&chat_id);
    }
    // A side chat starts as a fork of its parent. Pi forks the parent's own
    // session (its whole context, tools included); anything else carries
    // the parent's transcript up to the fork in the first prompt.
    let fork = if request.resume.is_none() && !resume_state.idle {
        inner.fork_parent(&chat_id, &doc)
    } else {
        None
    };
    let mut native_fork = false;
    if let Some((fork, true)) = &fork
        && harness_id == HarnessId::Pi
        && fork.harness == Some(HarnessId::Pi)
        && let Some(session) = fork.session.clone()
        && let Some(mcp) = request.mcp.as_mut()
    {
        mcp.env.insert(
            zeron_proto::orchestrator::ENV_FORK_SESSION.to_owned(),
            session,
        );
        native_fork = true;
    }
    // A Pi run in an orchestrator's tree starts with its project's goal,
    // instructions and memory (the Pi harness appends them to its prompt).
    if harness_id == HarnessId::Pi
        && let Some(prompt) = inner.project_prompt(&chat_id)
        && let Some(mcp) = request.mcp.as_mut()
    {
        mcp.env.insert(
            zeron_proto::orchestrator::ENV_PROJECT_PROMPT.to_owned(),
            prompt,
        );
    }
    // Every Pi of this session is kept off the models it, or any chat above
    // it (its orchestrator, a side chat's parent), disabled.
    if harness_id == HarnessId::Pi
        && let Some(dir) = inner.registry.data_dir()
        && let Some(mcp) = request.mcp.as_mut()
    {
        let ancestors: Vec<String> = inner
            .lineage(&chat_id)
            .into_iter()
            .map(|chat| chat.id)
            .filter(|id| *id != chat_id)
            .collect();
        if let Some(files) = crate::model_policy::env_value(&dir, &chat_id, &ancestors) {
            mcp.env.insert(
                zeron_proto::orchestrator::ENV_MODEL_POLICY.to_owned(),
                files,
            );
        }
        // Every Pi of an orchestrator's tree is held to the tree's policy.
        if let Some(file) = inner.tree_policy_file(&chat_id, &dir) {
            mcp.env
                .insert(zeron_proto::orchestrator::ENV_TREE_POLICY.to_owned(), file);
        }
    }
    // Notices the model is owed (its last runtime died, a turn was not
    // resumed) lead this prompt. Before the retry copy, so a startup-crash
    // retry carries them too; cleared once the agent answers. A command must
    // stay first in its prompt, so it leaves them for the next message.
    let mut notice_ids: Vec<String> = Vec::new();
    if !resume_state.idle
        && !native_command(&request.prompt, harness_id)
        && let Some(dir) = inner.registry.data_dir()
    {
        let notices = crate::model_notices::pending(&dir, &chat_id);
        if !notices.is_empty() {
            request.prompt = crate::model_notices::wrap(&notices, &request.prompt);
            notice_ids = notices.into_iter().map(|notice| notice.id).collect();
        }
    }
    // Kept whole for the startup-crash retry (same user entry; dispatch
    // re-injects the stored resume id). Option so the retry branch (inside
    // the event loop) can take ownership.
    let mut retry_request = Some(RunRequest {
        resume: None,
        ..request.clone()
    });
    // A side chat owns a fresh provider session. Bootstrap it from the frozen
    // conversation, never resume (and mutate) the parent's provider session.
    let mut carried_history = false;
    let provider = match request.resume.as_deref() {
        None => ProviderSession::Fresh,
        resumed => ProviderSession::Continued(resumed),
    };
    if !resume_state.idle
        && let Some(prompt) = inner.fork_history_prompt(
            &chat_id,
            &doc,
            harness_id,
            &request.prompt,
            Some(&resume_state.user_message_id),
            provider,
            fork.as_ref().map(|(fork, _)| fork).filter(|_| !native_fork),
        )
    {
        request.prompt = prompt;
        carried_history = true;
        resume_state
            .fork_history_sent
            .store(true, std::sync::atomic::Ordering::Release);
    }
    // Startup can stop before the SDK saves user text, with no new session
    // ID or receipt. Bridge that unacknowledged tail from our transcript;
    // a fresh session needs all prior user text, not just the latest tail.
    let prepared = if harness_id == HarnessId::Cursor {
        cursor_unstarted_history(
            &doc,
            &resume_state.user_message_id,
            &request.prompt,
            request.resume.is_some(),
        )
        .map(|prompt| request.prompt = prompt)
        .map_err(|e| zeron_harness::HarnessError::Protocol(e.to_string()))
    } else {
        Ok(())
    };
    // Waiting here keeps dispatch and the shared queue-flush watcher responsive.
    // The pending-update marker still orders new subprocesses after installation.
    // Share the lease with the adapter so child cleanup outlives this event loop.
    let mut _execution_lease = None;
    let admitted = match resume_state.admission.take() {
        Some(grant) => wait_for_admission(&inner, &chat_id, grant, &controls.interrupt).await,
        None => true,
    };
    let started = match prepared {
        Ok(()) => {
            // A stop while queued takes the synthetic Interrupted branch below.
            let lease = if admitted {
                tokio::select! {
                    biased;
                    _ = controls.interrupt.cancelled() => None,
                    lease = inner.registry.execution_lease(harness_id) => Some(Arc::new(lease)),
                }
            } else {
                None
            };
            if let Some(lease) = lease {
                _execution_lease = Some(lease.clone());
                controls.execution_lease = Some(lease);
                if let Some(listener) = inner.turn_listener.get() {
                    listener(&chat_id, &request.cwd);
                }
                let mut wire_request = request;
                if !matches!(harness_id, HarnessId::Cursor | HarnessId::Opencode) {
                    wire_request.prompt =
                        zeron_proto::invocation::harness_prompt(&wire_request.prompt, harness_id);
                }
                if resume_state.idle {
                    harness.start_idle(wire_request, controls).await
                } else {
                    // Pi marks its own prompt_sent; others start at run start.
                    if harness_id != HarnessId::Pi {
                        turn.trace_begin();
                    }
                    harness.run(wire_request, controls).await
                }
            } else {
                Ok(futures::stream::once(async {
                    Ok(AgentEvent::Done {
                        status: DoneStatus::Interrupted,
                        result: None,
                        error: None,
                        session_id: None,
                        class: None,
                    })
                })
                .boxed())
            }
        }
        Err(error) => Err(error),
    };
    let mut stream = match started {
        Ok(stream) => {
            resume_state
                .started
                .store(true, std::sync::atomic::Ordering::Release);
            stream
        }
        Err(err) => {
            let message = err.to_string();
            tracing::warn!(chat = %chat_id, harness = ?harness_id, error = %message, "run failed to start");
            // The journal alone is live-only: without an entry the transcript
            // shows "Run failed" with no reason (an OpenCode server that never
            // booted looked exactly like that).
            let parts = [MessagePart::Error {
                id: "e0".into(),
                message: message.clone(),
                class: None,
            }];
            if let Err(err) = finish_segment(
                &doc,
                None,
                &new_id(),
                &device_id,
                now_ms(),
                &parts,
                MessageStatus::Complete,
            ) {
                tracing::warn!(chat = %chat_id, error = %err, "start failure entry failed");
            }
            inner.publish(
                &chat_id,
                &AgentEvent::Error {
                    message: message.clone(),
                },
            );
            inner.publish(
                &chat_id,
                &AgentEvent::Done {
                    status: DoneStatus::Errored,
                    result: None,
                    error: Some(message),
                    session_id: None,
                    class: None,
                },
            );
            inner.remove_run(&chat_id, &run_id);
            inner.set_status(&chat_id, SessionStatus::Errored, false);
            return;
        }
    };

    let doc_ref: &SessionDoc = &doc;
    let mut folded: Vec<MessagePart> = Vec::new();
    // Every tool id this run has folded, across segment resets. Adapters
    // re-emit shape-bearing `tool_call_update`s (title/rawInput refreshes,
    // long-running completions) as full ToolCall events; once the fold has
    // reset at a steer/park boundary those ids are gone from `folded`, and
    // folding the echo would mint an orphan chip mid-text in the NEXT
    // segment — the mid-word transcript splits.
    let mut seen_tools: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut seen_images = std::collections::HashSet::new();
    for entry in doc_ref.read_entries().unwrap_or_default() {
        for part in entry.parts {
            if let MessagePart::Image { id, .. } = part {
                seen_images.insert(format!("{chat_id}\0{id}"));
                if let Some(tool_id) = id.strip_suffix(":image") {
                    seen_tools.insert(tool_id.to_owned());
                }
            }
        }
    }
    let mut prepared_events = std::collections::VecDeque::new();
    let mut entry_id = new_id();
    let mut segment_started = now_ms();
    let mut writer: Option<SegmentWriter<'_>> = None;
    let mut dirty = false;
    let mut flush_at = tokio::time::Instant::now();
    // Set when the engine interrupts the run: the harness gets this long to end its own
    // stream (its token was cancelled); past it, a terminal Done is synthesized.
    let mut interrupt_deadline: Option<tokio::time::Instant> = None;
    let mut interrupted = false;
    let mut saw_session_started = false;
    // Liveness heartbeat: this loop RUNNING is proof the harness stream is
    // open, so freshness must not depend on events arriving. Silent stretches
    // are normal and UNBOUNDED — a long tool call, redacted thinking, an
    // agent waiting on an external process, a question parked for an hour —
    // and each starved the UI's 45s staleness gate in turn (working strip /
    // AwaitingInput dot vanishing mid-run, both user-reported). No stall
    // timeout here by design (a first port was rejected — agents may
    // legitimately be quiet for >10min): a live child means Working, dying
    // paths each carry their own error, and engine death stops these ticks
    // so the gate still catches real crashes. touch_session throttles at 10s.
    let mut live_heartbeat = tokio::time::interval(std::time::Duration::from_secs(15));
    live_heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // PERSISTENT SESSION (zeron runsBySession): a completed turn on a
    // steerable harness parks here instead of ending the run — the child and
    // its steering mailbox stay warm, and the next user message (dispatch
    // routes into a live run) starts the next turn with zero respawn/resume
    // latency. `Some(when)` = idle since then; the 30-min reaper below ends
    // a session nobody comes back to (zeron SESSION_IDLE_MS). A voice
    // orchestrator holds Codex and its MCP servers (~200 MB) for calls only,
    // so it goes after 5 minutes. A call holds the reaper off, and the window
    // restarts at hang-up. `ZERON_SESSION_IDLE_MS` overrides both (tests).
    let session_idle = std::env::var("ZERON_SESSION_IDLE_MS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .map(std::time::Duration::from_millis)
        .unwrap_or(std::time::Duration::from_secs(
            if zeron_proto::voice::is_orchestrator_chat(&chat_id) {
                5 * 60
            } else {
                30 * 60
            },
        ));
    // A parked Pi runtime that finished its work (the reaper below holds off
    // while it reports background jobs, sub-agents or a workflow) is stopped
    // after the finished TTL: each one holds a Pi process, and the next
    // message restarts it through the normal resume path, history intact. A
    // tree MEMBER's window is also capped by the member TTL. Read once, here:
    // the chat's place cannot change under a live run.
    let member_ttl = if harness.keeps_runtime_alive() && harness_id == HarnessId::Pi {
        let member = matches!(
            zeron_proto::orchestrator::Place::of(&inner.lineage(&chat_id), &chat_id).role,
            zeron_proto::orchestrator::Role::Member { .. }
        );
        let member = if member {
            member_idle_ttl(&inner.registry)
        } else {
            None
        };
        match (finished_idle_ttl(&inner.registry), member) {
            (Some(finished), Some(member)) => Some(finished.min(member)),
            (finished, member) => finished.or(member),
        }
    } else {
        None
    };
    // A lost subagent signal is judged on the chat's own window, not the
    // short finished TTL.
    let subagent_silence = session_idle * 8;
    let session_idle = member_ttl.unwrap_or(session_idle);
    // A live subagent stretches the window, but never unbounded: every
    // driver's subagent terminal is a best-effort vendor signal (claude's
    // untagged task_notification, grok's subagent_finished, codex thread
    // closure), and one lost signal would otherwise pin the child — and this
    // loop's registry heartbeat — until the chat is archived. A subagent
    // silent this long (4h by default) is presumed lost; the reap stamps its
    // chip failed. (`subagent_silence` is set above.)
    let mut idle_since: Option<tokio::time::Instant> =
        resume_state.idle.then(tokio::time::Instant::now);
    // A parked runtime whose agent reports background work (see
    // `TurnControl::background_live`) is not idle: reaping it killed that
    // work. The reaper re-checks a full idle window later instead.
    let mut reap_deferred_until: Option<tokio::time::Instant> = None;
    // The reaper last deferred for background work. Its end is not idleness
    // either: the agent reports the drained set a moment before the turn the
    // completion wakes, and reaping in that gap lost the wake (issue #831).
    let mut held_for_background = false;
    // The parked turn FAILED (a provider error on a runtime that outlives it).
    // Stopping that runtime later is a clean resource decision — the turn
    // itself did not recover — so every post-park end below keeps reporting
    // Errored instead of stamping the row Idle (issue #118: the 10s finished
    // TTL rewrote the failure the user was about to read).
    let mut parked_errored = false;
    let steerable = harness.supports_steering();
    let stops_turn_in_place = steerable && harness.stops_turn_in_place();
    // The harness stream itself failed or ended: its Done is synthetic and
    // nothing is left to park.
    let mut stream_failed = false;
    // TURN-QUIESCE WATCHDOG (2026-08-12 stuck-Working incident): a harness
    // that loses a turn's Done — the adapter never settles `session/prompt`
    // even though the agent finished — strands Working forever: the live
    // heartbeat above keeps the row fresh, and there is no per-turn timeout
    // by design. This is NOT that stall timeout: it never ends the run or
    // errors anything. When the stream has been silent past the window AND
    // the fold shows completed output with nothing in flight (no unresolved
    // tool, no open question), the turn parks exactly like a Done would —
    // segment finalized Complete, status Idle, child and mailbox warm. A
    // false trip (the agent was quietly waiting on something invisible)
    // costs a status dip: the parked-resume path below re-arms Working the
    // moment output flows again, and nothing is lost. `ZERON_TURN_QUIESCE_MS`
    // overrides the window; 0 disables.
    // RETIRED for native drivers: a harness whose every turn shape ends with
    // a deterministic wire Done (claude/codex/cursor native) needs no
    // quiesce backstop — arming one only risks false parks on long silent
    // work. The env knob can configure a diagnostic window, but a prompt
    // with an authoritative completion signal must still await that signal.
    // ACP retains the watchdog only for unowned self-continued activity.
    let deterministic_turn_end = harness.deterministic_turn_end();
    // A runtime that lives as long as the app (Pi) is never reaped idle,
    // unless it is a tree member with a TTL (`member_ttl`).
    let keeps_runtime_alive = harness.keeps_runtime_alive();
    let authoritative_prompt_end = harness.authoritative_prompt_end();
    let quiesce_after: Option<std::time::Duration> = match std::env::var("ZERON_TURN_QUIESCE_MS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
    {
        Some(0) => None,
        Some(ms) => Some(std::time::Duration::from_millis(ms)),
        None if deterministic_turn_end => None,
        None => Some(std::time::Duration::from_secs(120)),
    };
    let mut last_stream_activity = tokio::time::Instant::now();
    // SELF-CONTINUED turns get a much SHORTER quiesce window. A turn the
    // agent starts on its own (background-task wake) can never receive a
    // harness Done: the adapter has no `session/prompt` outstanding to
    // settle — verified in claude-agent-acp's autonomous-result lane, which
    // consumes the SDK's turn-end without emitting anything; codex shows
    // the same shape. The watchdog is that turn shape's ONLY settle path,
    // so the default 120s window read as 2min of stuck-Working after every
    // background notification (user report 2026-08-13). The in-flight
    // fold gate below still protects running tools; reasoning heartbeats
    // push the window during real thinking. `ZERON_SELF_TURN_QUIESCE_MS`
    // overrides; 0 falls back to the normal window. An explicit
    // `ZERON_TURN_QUIESCE_MS=0` still disables the watchdog entirely.
    let self_quiesce_after: Option<std::time::Duration> =
        match std::env::var("ZERON_SELF_TURN_QUIESCE_MS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
        {
            Some(0) => None,
            Some(ms) => Some(std::time::Duration::from_millis(ms)),
            None => Some(std::time::Duration::from_secs(20)),
        };
    let mut self_continued_turn = false;
    // Spawn chips that have SETTLED (tagged Done seen). Content events for a
    // settled chip with no live sink are dropped — a straggler frame after
    // the freeze must not mint a new doc entry or wedge the transcript back
    // into Streaming. Only a steer (UserMessage) legitimately REOPENS a
    // settled subagent: it announces more work is coming.
    let mut settled_subagents: std::collections::HashSet<String> = std::collections::HashSet::new();
    // Live subagent sinks, parent tool-use id → transcript doc state.
    let mut subagents: std::collections::HashMap<String, SubagentSink> =
        std::collections::HashMap::new();
    // Last tagged subagent event. A background subagent outlives the turn that
    // spawned it, and its traffic never un-parks the chat, so the idle reaper
    // measures from here as well as from the park.
    let mut last_subagent_activity: Option<tokio::time::Instant> = None;

    let mut final_completed_turn = None;
    // Set when the runtime died while parked holding background work: the
    // cause, for the parent's note once the final status is published.
    let mut lost_runtime: Option<String> = None;
    let final_status = loop {
        let event: AgentEvent = if let Some(event) = prepared_events.pop_front() {
            event
        } else {
            let raw_event = tokio::select! {
                biased;
                changed = cancel_rx.changed(), if !interrupted => {
                    let _ = changed;
                    interrupted = true;
                    interrupt_deadline = Some(
                        tokio::time::Instant::now() + std::time::Duration::from_secs(3),
                    );
                    continue;
                }
                _ = tokio::time::sleep_until(
                    interrupt_deadline.unwrap_or_else(tokio::time::Instant::now)
                ), if interrupt_deadline.is_some() => AgentEvent::Done {
                    status: DoneStatus::Interrupted,
                    result: None,
                    error: None,
                    session_id: None,
                    class: None,
                },
                _ = live_heartbeat.tick() => {
                    inner.touch_session(&chat_id);
                    continue;
                }
                // A hang-up restarts the idle window, however long the call.
                _ = resume_state.voice_active.ended.notified() => {
                    if idle_since.is_some() {
                        idle_since = Some(tokio::time::Instant::now());
                    }
                    continue;
                }
                // An accepted update must not wait behind a warm between-turn
                // child for the full idle-reaper window. The completed turn is
                // already durable, so retire the parked process cleanly and let
                // the queued exclusive lease proceed.
                _ = tokio::time::sleep_until(tokio::time::Instant::now()),
                    if idle_since.is_some()
                        && !resume_state.voice_active.live()
                        && inner.registry.update_pending(harness_id)
                        && !turn.background_live()
                        && (subagents.is_empty()
                            || last_subagent_activity.is_none_or(|at| at.elapsed() >= subagent_silence)) =>
                {
                    if let Some(token) = lock(&inner.runs)
                        .get(&chat_id)
                        .filter(|h| h.run_id == run_id)
                        .map(|h| h.interrupt_token.clone())
                    {
                        token.cancel();
                    }
                    break if parked_errored {
                        SessionStatus::Errored
                    } else {
                        SessionStatus::Idle
                    };
                }
                // Idle reaper (zeron SESSION_IDLE_MS): a parked persistent session
                // nobody returned to within its window releases its child. The turn
                // was finalized at Done, so this end is clean — no aborted stamp.
                // A live background subagent is somebody still using the child:
                // the window counts from its last activity, and stretches to
                // `subagent_silence` while its sink is open.
                _ = tokio::time::sleep_until(
                    {
                        let window = if subagents.is_empty() { session_idle } else { subagent_silence };
                        let base = idle_since
                            .map(|at| at.max(last_subagent_activity.unwrap_or(at)) + window);
                        match (base, reap_deferred_until) {
                            (Some(base), Some(deferred)) => base.max(deferred),
                            (base, deferred) => base.or(deferred).unwrap_or_else(tokio::time::Instant::now),
                        }
                    }
                ), if idle_since.is_some() && (!keeps_runtime_alive || member_ttl.is_some()) && !resume_state.voice_active.live() => {
                    // A call that began after this wait was armed holds it off.
                    if resume_state.voice_active.live() {
                        continue;
                    }
                    // The driver's own report of background work (Claude's
                    // background agents and shells) is authoritative — it
                    // lists every task the process still runs — so it holds
                    // the runtime for as long as that work lives.
                    if turn.background_live() {
                        if !held_for_background {
                            tracing::info!(chat = %chat_id, "idle reaper deferred: the agent reports background work");
                        }
                        reap_deferred_until = Some(tokio::time::Instant::now() + session_idle);
                        held_for_background = true;
                        continue;
                    }
                    if std::mem::take(&mut held_for_background) {
                        // The work just ended: a full idle window from here.
                        reap_deferred_until = Some(tokio::time::Instant::now() + session_idle);
                        continue;
                    }
                    if member_ttl.is_some() {
                        tracing::info!(
                            chat = %chat_id,
                            live_subagents = subagents.len(),
                            "stopping a finished Pi runtime (restarts on its next message)"
                        );
                    } else {
                        tracing::info!(
                            chat = %chat_id,
                            live_subagents = subagents.len(),
                            "reaping idle persistent session"
                        );
                    }
                    if let Some(token) = lock(&inner.runs)
                        .get(&chat_id)
                        .filter(|h| h.run_id == run_id)
                        .map(|h| h.interrupt_token.clone())
                    {
                        token.cancel();
                    }
                    // Reaping a runtime after a FAILED turn ends the process,
                    // not the failure: the row must keep saying Errored until
                    // the user's next message (issue #118).
                    break if parked_errored {
                        SessionStatus::Errored
                    } else {
                        SessionStatus::Idle
                    };
                }
                Some(event) = engine_rx.recv() => event,
                next = stream.next() => match next {
                    Some(Ok(event)) => {
                        // The agent answered the prompt that carried the
                        // notices: they reached it. A startup failure (an
                        // errored Done first) leaves them for the retry or
                        // the next message.
                        if !notice_ids.is_empty()
                            && !matches!(
                                event,
                                AgentEvent::Done {
                                    status: DoneStatus::Errored,
                                    ..
                                }
                            )
                        {
                            if let Some(dir) = inner.registry.data_dir() {
                                crate::model_notices::mark_delivered(&dir, &chat_id, &notice_ids);
                            }
                            notice_ids.clear();
                        }
                        event
                    }
                    // A stream error while PARKED is a post-turn child death —
                    // the turn was already finalized, so the run ends clean
                    // instead of stamping a completed session Errored.
                    Some(Err(err)) if idle_since.is_some() => {
                        let Some(held) = inner.lost_work(&chat_id, subagents.len(), turn.background_live()) else {
                            tracing::warn!(chat = %chat_id, error = %err, "parked session child died; ending clean");
                            break if parked_errored {
                                SessionStatus::Errored
                            } else {
                                SessionStatus::Idle
                            };
                        };
                        // The runtime held jobs or sub-agents nobody has
                        // heard the end of: their results died with it, and
                        // the transcript must say so.
                        let cause = match &err {
                            zeron_harness::HarnessError::Protocol(text) => text.clone(),
                            other => other.to_string(),
                        };
                        let message = format!(
                            "{cause}. It was still running {held}; their results were not saved. Check git status and the worktree before continuing."
                        );
                        tracing::warn!(chat = %chat_id, error = %message, "parked session child died holding work");
                        if let Some(dir) = inner.registry.data_dir() {
                            crate::model_notices::record(&dir, &chat_id, now_ms(), &message);
                        }
                        let parts = [MessagePart::Error {
                            id: "e0".into(),
                            message: message.clone(),
                            class: None,
                        }];
                        if let Err(err) = finish_segment(
                            doc_ref,
                            None,
                            &new_id(),
                            &device_id,
                            now_ms(),
                            &parts,
                            MessageStatus::Complete,
                        ) {
                            tracing::warn!(chat = %chat_id, error = %err, "lost-runtime entry failed");
                        }
                        lost_runtime = Some(message);
                        break SessionStatus::Errored;
                    }
                    Some(Err(err)) => {
                        stream_failed = true;
                        AgentEvent::Done {
                            status: DoneStatus::Errored,
                            result: None,
                            error: Some(err.to_string()),
                            session_id: None,
                            class: None,
                        }
                    }
                    None if interrupted => AgentEvent::Done {
                        status: DoneStatus::Interrupted,
                        result: None,
                        error: None,
                        session_id: None,
                        class: None,
                    },
                    // Stream end while PARKED idle: a per-turn adapter closing
                    // after its final Done — a clean end, not a crash (the turn
                    // was already finalized). Persistent adapters keep the
                    // stream open and never hit this.
                    None if idle_since.is_some() => break if parked_errored {
                        SessionStatus::Errored
                    } else {
                        SessionStatus::Idle
                    },
                    None => {
                        stream_failed = true;
                        AgentEvent::Done {
                            status: DoneStatus::Errored,
                            result: None,
                            error: Some("harness stream ended without Done".into()),
                            session_id: None,
                            class: None,
                        }
                    }
                },
                _ = tokio::time::sleep_until(flush_at), if dirty || subagents.values().any(|s| s.dirty) => {
                    #[cfg(test)]
                    FLUSH_TICKS.with(|t| t.set(t.get() + 1));
                    // Coalesced STREAM_COMMIT_MS tick: one doc commit per window
                    // (parent + any dirty subagent docs).
                    if dirty {
                        if let Err(err) = sync_segment(
                            doc_ref, &mut writer, &entry_id, &device_id, segment_started, &folded,
                        ) {
                            tracing::warn!(chat = %chat_id, error = %err, "segment sync failed");
                        } else {
                            turn.trace_mark(zeron_harness::turn_trace::Phase::FirstCommit);
                        }
                        dirty = false;
                    }
                    for sink in subagents.values_mut() {
                        sink.flush(&device_id);
                    }
                    continue;
                }
                // Turn-quiesce watchdog (see the knob above). Armed only when the
                // fold says nothing is in flight: an unresolved tool part is a
                // command still running (legitimately silent for minutes — the
                // rejected stall-timeout case), an unresolved input part is a
                // question awaiting the user. The live-plan chip is exempt from
                // the tool check: it is a singleton that never resolves. An EMPTY
                // fold still arms — a Steered boundary that no output ever
                // follows is one of the wedge shapes — it just parks without
                // writing a segment (an empty finalize would leave a stub entry).
                _ = tokio::time::sleep_until({
                    let mut window = quiesce_after.unwrap_or_default();
                    if self_continued_turn && let Some(short) = self_quiesce_after {
                        window = window.min(short);
                    }
                    last_stream_activity + window
                }), if quiesce_after.is_some()
                    && (self_continued_turn || !authoritative_prompt_end)
                    && idle_since.is_none()
                    && !interrupted
                    && steerable
                    && !folded.iter().any(|p| match p {
                        MessagePart::Tool { id, resolved: false, .. } => {
                            id != zeron_proto::LIVE_PLAN_TOOL_ID
                        }
                        MessagePart::Input { resolved: false, .. } => true,
                        _ => false,
                    }) =>
                {
                    tracing::warn!(
                        chat = %chat_id,
                        quiet_ms = quiesce_after.unwrap_or_default().as_millis() as u64,
                        "turn quiesced: stream silent after completed output with no \
                         turn-end; parking (suspected missing harness Done)"
                    );
                    // Some adapters close a completed response through this
                    // engine watchdog instead of a native Done. Preserve that
                    // completion notice, but never notify for an empty boundary
                    // or while an accepted steer still awaits delivery.
                    let completed_turn = ((!folded.is_empty() || writer.is_some())
                        && !inner.has_pending_steers(&chat_id, &run_id)
                        && !inner.continues_with_sent_message(&chat_id))
                        .then(|| entry_id.clone());
                    if !folded.is_empty() || writer.is_some() {
                        if let Err(err) = finish_segment(
                            doc_ref,
                            writer.take(),
                            &entry_id,
                            &device_id,
                            segment_started,
                            &folded,
                            MessageStatus::Complete,
                        ) {
                            tracing::warn!(chat = %chat_id, error = %err, "quiesce segment finish failed");
                        } else {
                            turn.trace_mark(zeron_harness::turn_trace::Phase::FirstCommit);
                        }
                        inner.note_message(&chat_id, &folded_text(&folded));
                    }
                    if let Some(t) = turn.trace_finish() {
                        log_turn_trace(&chat_id, &run_id, harness_id, "quiesced", t);
                    }
                    folded.clear();
                    dirty = false;
                    entry_id = new_id();
                    segment_started = now_ms();
                    idle_since = Some(tokio::time::Instant::now());
                    reap_deferred_until = None;
                    self_continued_turn = false;
                    parked_errored = false;
                    inner.set_status_with_completion(
                        &chat_id, SessionStatus::Idle, false, completed_turn,
                    );
                    continue;
                }
            };

            prepared_events.extend(
                inner
                    .prepare_generated_image(&chat_id, &run_cwd, raw_event, &mut seen_images)
                    .await,
            );
            let Some(event) = prepared_events.pop_front() else {
                continue;
            };
            event
        };

        // ── subagent routing ───────────────────────────────────────────
        // Tagged events NEVER fold into the parent transcript: they stream
        // into the subagent's own doc, and the parent keeps only the spawn
        // chip (ref + status + one-line tail) — refreshed through the live
        // fold while its segment still streams, else stamped in place (the
        // eager-done norm: the chip's entry finished before the background
        // subagent spoke). Handled BEFORE the parked gate so a parked
        // session's background traffic still reaches the subagent doc
        // without un-parking the chat.
        if let AgentEvent::Subagent {
            parent_tool_use_id,
            event: sub_event,
        } = &event
        {
            inner.publish(&chat_id, &event);
            last_subagent_activity = Some(tokio::time::Instant::now());
            // Background subagents can outlive the turn and be the only
            // traffic for minutes: keep the row (and its count) inside the
            // UI's staleness window while any is live.
            if !subagents.is_empty() {
                inner.touch_session(&chat_id);
            }
            let is_steer = matches!(
                sub_event.as_ref(),
                AgentEvent::UserMessage { .. } | AgentEvent::Steered { .. }
            );
            if is_steer {
                settled_subagents.remove(parent_tool_use_id);
            } else if settled_subagents.contains(parent_tool_use_id)
                && !subagents.contains_key(parent_tool_use_id)
            {
                // Straggler after the freeze: chip-only silence (the old
                // pre-viz behavior), never a reopened doc.
                continue;
            }
            let sub_id = subagent_doc_id(&chat_id, parent_tool_use_id);
            let chip_streaming = folded
                .iter()
                .any(|p| matches!(p, MessagePart::Tool { id, .. } if id == parent_tool_use_id));
            let sink_known = subagents.contains_key(parent_tool_use_id);
            // A Done with NO sink (a subagent that never streamed — codex
            // turn ends can beat registration) is chip-only: minting a doc
            // just to freeze it empty helps no one, and stamping the ref
            // would link the chip to that never-created doc (an empty tab
            // on click).
            let done_only = !sink_known && matches!(sub_event.as_ref(), AgentEvent::Done { .. });
            if chip_streaming {
                if !sink_known && !done_only {
                    for p in folded.iter_mut() {
                        if let MessagePart::Tool {
                            id,
                            call,
                            subagent_ref,
                            ..
                        } = p
                            && id == parent_tool_use_id
                            // Genus gate: a subagent ref may only ever bind
                            // to a SPAWN call — mis-keyed tagged traffic
                            // must not turn an ordinary chip into a spawn
                            // link (empty tab on click).
                            && call.is_subagent_spawn()
                        {
                            *subagent_ref = Some(sub_id.clone());
                        }
                    }
                }
                zeron_doc::fold_event_into_parts(&mut folded, &event);
                if !dirty {
                    dirty = true;
                    flush_at = tokio::time::Instant::now()
                        + std::time::Duration::from_millis(STREAM_COMMIT_MS);
                }
            }
            // Open the sink lazily; an open failure degrades to chip-only.
            if !sink_known && !done_only {
                let opened = inner.doc_host().and_then(|host| match host.open(&sub_id) {
                    Ok(handle) => Some(handle.writer()),
                    Err(err) => {
                        tracing::warn!(doc = %sub_id, error = %err, "subagent doc open failed (chip-only)");
                        None
                    }
                });
                if let Some(sub_doc) = opened {
                    let title = folded
                        .iter()
                        .find_map(|part| match part {
                            MessagePart::Tool { id, call, .. } if id == parent_tool_use_id => {
                                Some(call.subagent_title())
                            }
                            _ => None,
                        })
                        .unwrap_or_else(|| "Subagent".into());
                    subagents.insert(
                        parent_tool_use_id.clone(),
                        SubagentSink {
                            doc_id: sub_id.clone(),
                            title,
                            doc: sub_doc,
                            entry_id: new_id(),
                            started_at: now_ms(),
                            entry_index: None,
                            written: Vec::new(),
                            folded: Vec::new(),
                            dirty: false,
                        },
                    );
                    inner.set_running_subagents(&chat_id, running_list(&subagents));
                    if !chip_streaming {
                        let _ = doc_ref.update_subagent_chip(
                            parent_tool_use_id,
                            Some(&sub_id),
                            Some("running"),
                            None,
                        );
                    }
                }
            }
            let done = matches!(sub_event.as_ref(), AgentEvent::Done { .. });
            if done {
                settled_subagents.insert(parent_tool_use_id.clone());
                if !chip_streaming {
                    // A resumed run can finish an older chip without ever
                    // opening a sink. The chip's lifecycle is independent of
                    // whether this run received transcript content.
                    let _ = doc_ref.update_subagent_chip(
                        parent_tool_use_id,
                        None,
                        subagent_chip_update(sub_event),
                        None,
                    );
                }
            }
            if let Some(sink) = subagents.get_mut(parent_tool_use_id) {
                if let AgentEvent::UserMessage { text } = sub_event.as_ref() {
                    // A steer splits ENTRIES, not parts — handled at the
                    // sink level (the fold ignores UserMessage).
                    sink.push_user(&device_id, text);
                    continue;
                }
                let was_clean = !sink.dirty;
                zeron_doc::fold_event_into_parts(&mut sink.folded, sub_event);
                sink.dirty = true;
                // A sink waking on its own must arm the same commit window
                // the parent's dirty flag does — `flush_at` is otherwise only
                // rescheduled on the parent's flip, so a past deadline would
                // fire the commit branch on every event instead of once per
                // STREAM_COMMIT_MS window.
                if was_clean && !dirty && flush_at <= tokio::time::Instant::now() {
                    flush_at = tokio::time::Instant::now()
                        + std::time::Duration::from_millis(STREAM_COMMIT_MS);
                }
                if done {
                    let status = match sub_event.as_ref() {
                        AgentEvent::Done {
                            status: DoneStatus::Errored,
                            ..
                        } => MessageStatus::Complete,
                        AgentEvent::Done {
                            status: DoneStatus::Interrupted,
                            ..
                        } => MessageStatus::Aborted,
                        _ => MessageStatus::Complete,
                    };
                    let sink = subagents.remove(parent_tool_use_id).expect("checked");
                    inner.set_running_subagents(&chat_id, running_list(&subagents));
                    let doc_id = sink.doc_id.clone();
                    // FREEZE: the finished transcript uploads as a static R2
                    // blob (`blob/{chatId}/{subDocId}`) so viewers of
                    // finished subagents never wake the doc's room; dropping
                    // the sink unpins the doc for the LRU and the room
                    // idles. The live doc remains the fallback.
                    if let Some(json) = sink.finish(&device_id, status)
                        && let Some(host) = inner.doc_host()
                    {
                        host.upload_tool_sidecar(
                            &chat_id,
                            zeron_doc::SidecarPayload {
                                part_id: doc_id,
                                output: Some(json),
                                diff: None,
                            },
                        );
                    }
                }
            }
            continue;
        }

        // Any stream activity proves the run is alive — keep the session's
        // freshness inside the UI's 45s staleness window (throttled), and
        // push the quiesce watchdog's window out.
        inner.touch_session(&chat_id);
        last_stream_activity = tokio::time::Instant::now();
        if !matches!(
            event,
            AgentEvent::SessionStarted { .. } | AgentEvent::ContextUsage { .. }
        ) {
            turn.trace_mark(zeron_harness::turn_trace::Phase::FirstEvent);
        }
        // The engine's input bridge is the sole authority on input requests:
        // it mints the id and parks the resolver BEFORE emitting the event,
        // so a legitimate id is always pending here. A harness emitting its
        // own copy (a different id no resolver knows) would fold an
        // unanswerable twin chip into the doc — and answering the twin would
        // never resume the run. Dropped BEFORE the parked gate below: letting
        // it un-park the session and then dropping it would disarm the idle
        // reaper with no turn running (a leaked child no reap ever ends).
        if let AgentEvent::InputRequested { request_id, .. } = &event {
            let pending = lock(&inner.runs)
                .get(&chat_id)
                .map(|h| h.pending_inputs.clone());
            let known = pending.is_some_and(|p| lock(&p).contains_key(request_id));
            if !known {
                tracing::warn!(
                    chat = %chat_id,
                    request = %request_id,
                    "dropping harness-emitted InputRequested (unknown id; \
                     the engine input bridge owns this lifecycle)"
                );
                continue;
            }
        }
        // Where the harness keeps each user message, for a rewind. Never a
        // turn boundary.
        if let AgentEvent::UserMessageDelivered {
            message_id,
            harness_ref,
        } = &event
        {
            let message_id = message_id
                .as_deref()
                .unwrap_or(&resume_state.user_message_id);
            if let Err(err) = doc_ref.set_harness_ref(message_id, harness_ref) {
                tracing::warn!(%chat_id, error = %err, "harness ref write failed");
            }
            continue;
        }
        // The harness retrying a failed request rides the session row (the
        // status stays Working). Never a doc part or a turn boundary.
        if let AgentEvent::Retrying {
            attempt,
            max_attempts,
            delay_ms,
            error,
            active,
        } = &event
        {
            inner.set_retry(
                &chat_id,
                active.then(|| zeron_proto::RetryState {
                    attempt: *attempt,
                    max_attempts: *max_attempts,
                    next_attempt_at: delay_ms.and_then(|ms| {
                        i64::try_from(ms)
                            .ok()
                            .map(|ms| Utc::now() + chrono::Duration::milliseconds(ms))
                    }),
                    error: error.clone(),
                }),
            );
            continue;
        }
        // Capacity/occupancy can settle after Done; updating it must not reopen a turn.
        if let AgentEvent::ContextUsage { tokens, window } = &event {
            if let Err(err) = doc_ref.update_context_usage(*tokens, *window) {
                tracing::warn!(%chat_id, error = %err, "context usage write failed");
            }
            continue;
        }
        // PARKED: a steer boundary, a terminal Done, or SELF-CONTINUED OUTPUT
        // re-opens the session; everything else stays gated. The ACP child
        // keeps forwarding `session/update` frames after a turn completes,
        // and they split two ways:
        //
        // - Post-turn NOISE — late tool_call_updates for commands folded in a
        //   prior segment, command refreshes, reasoning heartbeats. Treating
        //   those as "the next turn" re-armed Working with no Done ever
        //   coming (the eternally-running session bug) and folded orphan
        //   parts into a phantom segment. Still dropped.
        // - SELF-CONTINUED WORK — Claude Code re-invokes itself when a
        //   background task finishes (turns no prompt started) and streams
        //   real output for them. Dropping those LOST transcript content
        //   (2026-08-12: "Build finished successfully…" streamed by the
        //   agent, absent from the doc). Fresh text or a genuinely new tool
        //   call resumes the session: new segment, Working, and the turn
        //   settles again via Done — or via the quiesce watchdog, which is
        //   what makes this resume safe where the naive version was not.
        //
        // The RESUME_GATE separates the two by arrival time: a finished
        // turn's tail flush lands within milliseconds of its Done, while a
        // self-continued turn starts a whole new agent round trip (seconds
        // at minimum, minutes in the incident). Inside the gate everything
        // non-boundary stays inert, exactly as before.
        let turn_was_active = idle_since.is_none();
        const RESUME_GATE: std::time::Duration = std::time::Duration::from_secs(1);
        if idle_since.is_some() {
            let self_continued = idle_since
                .is_some_and(|parked_at| parked_at.elapsed() >= RESUME_GATE)
                && (matches!(
                    &event,
                    AgentEvent::TextDelta { text } if !text.is_empty()
                ) || matches!(
                    &event,
                    AgentEvent::ToolCall { id, .. }
                        if id == zeron_proto::LIVE_PLAN_TOOL_ID || !seen_tools.contains(id)
                ));
            if self_continued {
                tracing::info!(
                    chat = %chat_id,
                    "parked session resumed by self-continued agent output"
                );
                idle_since = None;
                self_continued_turn = true;
                // The park cleared the fold; rotate to a fresh entry and
                // fall through — this event is the new segment's first part.
                entry_id = new_id();
                segment_started = now_ms();
                inner.set_status(&chat_id, SessionStatus::Working, true);
            } else {
                match &event {
                    AgentEvent::SessionStarted { .. }
                        if resume_state.idle && !saw_session_started => {}
                    AgentEvent::Steered { .. } => {
                        idle_since = None;
                        inner.set_status(&chat_id, SessionStatus::Working, true);
                    }
                    AgentEvent::Done { .. } => {
                        idle_since = None;
                    }
                    // A question with NO turn behind it (post-turn permission
                    // noise): answer it empty right here. Un-parking into
                    // AwaitingInput would disarm the idle reaper with no Done
                    // ever coming — stranded status, leaked warm child.
                    AgentEvent::InputRequested { request_id, .. } => {
                        let resolver = lock(&inner.runs)
                            .get(&chat_id)
                            .and_then(|h| lock(&h.pending_inputs).remove(request_id));
                        if let Some(tx) = resolver {
                            let _ = tx.send(Vec::new());
                        }
                        tracing::debug!(chat = %chat_id, "parked session: post-turn input request auto-declined");
                        continue;
                    }
                    // A stale answer settling after its turn already closed:
                    // nothing is running — stay parked.
                    AgentEvent::InputResolved { .. } => continue,
                    _ => continue,
                }
            }
        }
        // Empty reasoning deltas are PURE heartbeats: redacted thinking and
        // tool-input-generation windows stream them with no text. They fold
        // to nothing, so journaling/publishing them is only noise (hundreds
        // per long turn observed) — the touch above already did their job.
        if matches!(&event, AgentEvent::ReasoningDelta { text } if text.is_empty()) {
            continue;
        }

        // Stale tool echoes: a ToolCall/ToolResult naming an id folded in a
        // PRIOR segment (the fold reset at a steer or park since) belongs to
        // a chip that already rendered in its own entry. Folding the echo
        // would splice a phantom chip into the middle of the current
        // segment's streaming text (the mid-word transcript splits). Dropped;
        // same-segment refreshes still land in place.
        let in_segment = |folded: &[MessagePart], id: &str| {
            folded
                .iter()
                .any(|p| matches!(p, MessagePart::Tool { id: pid, .. } if pid == id))
        };
        match &event {
            // The live plan chip is a deliberate singleton (every update
            // reuses `LIVE_PLAN_TOOL_ID` so the fold refreshes in place):
            // treating its reappearance after a park/steer reset as a stale
            // echo dropped the todo list for the rest of the run — from the
            // first boundary on, plans never rendered again.
            AgentEvent::ToolCall { id, .. } if id == zeron_proto::LIVE_PLAN_TOOL_ID => {}
            AgentEvent::ToolResult { id, .. } if id == zeron_proto::LIVE_PLAN_TOOL_ID => {}
            AgentEvent::ToolCall { id, .. } => {
                if !in_segment(&folded, id) && seen_tools.contains(id) {
                    continue;
                }
                seen_tools.insert(id.clone());
            }
            AgentEvent::ToolResult { id, .. }
                if !in_segment(&folded, id) && seen_tools.contains(id) =>
            {
                continue;
            }
            _ => {}
        }

        // Startup-crash retry: a run that dies before ever starting (errored
        // Done, no SessionStarted, nothing streamed) means the AGENT CHILD
        // failed to come up — not that the injected resume id was bad. Since
        // the ACP conversion (2026-08-08) a stale id is handled inside the
        // harness (`session/load` falls back to `session/new`), so the old
        // guess here — tombstone the id, retry fresh — fired only on child
        // startup failures and permanently severed GOOD conversations (user
        // incident 2026-08-13). The id stays; retry ONCE against the same
        // user entry, resume and all, in case the crash was transient. A
        // helper that is down hard fails the retry too and surfaces its
        // crash text (the harness now appends exit status + stderr).
        if resume_state.resume_injected
            && !resume_state.idle
            && !resume_state.startup_retry
            && !saw_session_started
            && folded.is_empty()
            && !interrupted
            && matches!(
                &event,
                AgentEvent::Done {
                    status: DoneStatus::Errored,
                    ..
                }
            )
            && let Some(retry) = retry_request.take()
        {
            tracing::warn!(
                chat = %chat_id,
                "run died before session start; retrying once (resume kept)"
            );
            inner.remove_run(&chat_id, &run_id);
            let engine = SessionsEngine {
                inner: inner.clone(),
            };
            let chat = chat_id.clone();
            let message_id = resume_state.user_message_id.clone();
            tokio::spawn(async move {
                // The user entry write inside dispatch is idempotent by
                // message id; `startup_retry` makes this attempt final.
                if let Err(err) = engine
                    .dispatch_with(&chat, harness_id, retry, Some(message_id), true)
                    .await
                {
                    tracing::error!(chat = %chat, error = %err, "startup-crash retry dispatch failed");
                    // No run is coming: leaving the row Working would spin
                    // the session forever with nothing behind it.
                    engine
                        .inner
                        .set_status(&chat, SessionStatus::Errored, false);
                }
            });
            return;
        }

        // A steer boundary splits the assistant entry exactly where the fold resets.
        if let AgentEvent::Steered {
            next_assistant_message_id,
            ..
        } = &event
        {
            inner.publish(&chat_id, &event);
            // A steer boundary means a real prompt owns the turn again — its
            // Done will come; the short self-continued window stands down.
            self_continued_turn = false;
            if let Err(err) = finish_segment(
                doc_ref,
                writer.take(),
                &entry_id,
                &device_id,
                segment_started,
                &folded,
                MessageStatus::Complete,
            ) {
                tracing::warn!(chat = %chat_id, error = %err, "segment finish failed");
            } else {
                turn.trace_mark(zeron_harness::turn_trace::Phase::FirstCommit);
            }
            inner.note_message(&chat_id, &folded_text(&folded));
            folded.clear();
            dirty = false;
            entry_id = next_assistant_message_id.clone().unwrap_or_else(new_id);
            segment_started = now_ms();
            // The elapsed timer is per user message, not per child process: a
            // steer boundary restarts it (matches the parked-resume path and
            // the composer's optimistic overlay, which already reads 0:00).
            inner.set_status(&chat_id, SessionStatus::Working, true);
            // The boundary confirms delivery of the oldest accepted steer —
            // retire its at-least-once ledger entry.
            let confirmed = lock(&inner.runs)
                .get(&chat_id)
                .filter(|h| h.run_id == run_id)
                .and_then(|h| lock(&h.routed_steers).pop_front());
            // Consumed: a steer carrying the fork history delivered it to
            // this runtime's provider session.
            if confirmed.is_some_and(|steer| steer.fork_history)
                && let Some(session) = lock(&inner.harness_sessions)
                    .get(&chat_id)
                    .map(|known| known.session_id.clone())
                    .filter(|id| !id.is_empty())
            {
                let _ = doc.set_fork_history_session(&session);
            }
            continue;
        }

        match &event {
            AgentEvent::SessionStarted {
                session_id, cwd, ..
            } => {
                saw_session_started = true;
                // The event's own cwd (where the harness actually created the
                // session) scopes the stored id, not the request's.
                inner.remember_harness_session(&chat_id, session_id, cwd);
                note_fork_history_session(&doc, carried_history, session_id);
            }
            AgentEvent::Done {
                session_id: Some(session_id),
                ..
            } => {
                inner.remember_harness_session(&chat_id, session_id, &run_cwd);
                note_fork_history_session(&doc, carried_history, session_id);
            }
            AgentEvent::InputRequested { .. } => {
                // Known-id guaranteed: the unknown-id twin was dropped above,
                // before the parked gate.
                inner.set_status(&chat_id, SessionStatus::AwaitingInput, false);
            }
            AgentEvent::InputResolved { .. } => {
                inner.set_status(&chat_id, SessionStatus::Working, false);
            }
            _ => {}
        }

        inner.publish(&chat_id, &event);

        // Defensive rule from zeron: a mid-run SessionStarted re-emission (Claude SDK
        // background re-invocations) must not wipe the segment being written.
        let skip_fold = matches!(&event, AgentEvent::SessionStarted { .. }) && !folded.is_empty();
        if !skip_fold {
            fold_event_into_parts(&mut folded, &event);
            // R2 sidecar PARKED (2026-08-10, product call): the fold's
            // summary/stats ARE the doc's whole record — no refs stamped, no
            // uploads. Full outputs survive only in the host's local run
            // journal. To reintroduce: `zeron_doc::sidecar_payload(&event)`
            // → `apply_sidecar_refs` → `doc_host.upload_tool_sidecar`, all
            // still in place and tested.
        }

        // The agent died mid-turn: it cannot tell itself, so the next prompt
        // carries the cause and what was still running.
        if let AgentEvent::Done {
            status: DoneStatus::Errored,
            error,
            class,
            ..
        } = &event
            && (stream_failed || matches!(class, Some(zeron_proto::ErrorClass::ProcessKilled { .. })))
            && let Some(dir) = inner.registry.data_dir()
        {
            let cause = error.as_deref().unwrap_or("the agent process ended without a reply");
            let unfinished = unfinished_tools(&folded);
            let text = if unfinished.is_empty() {
                format!("Your turn ended abruptly: {cause}.")
            } else {
                format!(
                    "Your turn ended abruptly: {cause}. Tool calls still running then: {}.",
                    unfinished.join("; ")
                )
            };
            crate::model_notices::record(&dir, &chat_id, now_ms(), &text);
        }

        if let AgentEvent::Done { status, .. } = &event {
            // A question still pending at turn end can never be legitimately
            // answered (its turn is over): drain the resolvers NOW, or a late
            // `respond_input` finds one, emits InputResolved, and un-parks
            // the session into Working with no turn behind it — stranded
            // Working, timer forever, reaper disarmed. Empty answers unblock
            // the harness-side bridge like an interrupt does.
            let pending = lock(&inner.runs)
                .get(&chat_id)
                .filter(|h| h.run_id == run_id)
                .map(|h| h.pending_inputs.clone());
            if let Some(pending) = pending {
                for (_, tx) in lock(&pending).drain() {
                    let _ = tx.send(Vec::new());
                }
            }
            let message_status = match status {
                DoneStatus::Interrupted => MessageStatus::Aborted,
                DoneStatus::Completed | DoneStatus::Errored => MessageStatus::Complete,
            };
            // No dangling chips: a run that ends for ANY reason (completed,
            // errored, interrupted) terminally resolves its input parts — an
            // unresolved question must not outlive the run that asked it
            // (its resolver died with the run; an answer could never land).
            for part in folded.iter_mut() {
                if let MessagePart::Input { resolved, .. } = part {
                    *resolved = true;
                }
            }
            // A Done landing on a PARKED session with nothing streamed (the
            // idle reaper's or an interrupt's own teardown) has no entry to
            // finalize — writing one would leave an empty aborted stub.
            let nothing_streamed = writer.is_none() && folded.is_empty();
            if !nothing_streamed {
                if let Err(err) = finish_segment(
                    doc_ref,
                    writer.take(),
                    &entry_id,
                    &device_id,
                    segment_started,
                    &folded,
                    message_status,
                ) {
                    tracing::warn!(chat = %chat_id, error = %err, "final segment finish failed");
                } else {
                    turn.trace_mark(zeron_harness::turn_trace::Phase::FirstCommit);
                }
                inner.note_message(&chat_id, &folded_text(&folded));
            }
            if let Some(t) = turn.trace_finish() {
                log_turn_trace(&chat_id, &run_id, harness_id, &format!("{status:?}"), t);
            }
            if *status == DoneStatus::Completed {
                // A cleanly completed turn resets the auto-resume revival
                // budget: only consecutive crash-revive-crash cycles spend it.
                inner.journal.clear_resume_attempts(&chat_id);
            }
            // Exchange completed on an untitled chat → name it (fire-and-forget;
            // interrupted/errored turns never trigger naming).
            if *status == DoneStatus::Completed
                && let Some(titles) = inner.titles.get()
            {
                titles.maybe_generate(&chat_id, harness_id, &user_prompt, &run_cwd);
            }
            // A turn stopped in place: the runtime lives on, parked like a
            // completed turn. Steers it had accepted but not yet delivered
            // were cancelled with the turn (never delivered, never
            // confirmed) — drop them, or they would hold the parked session
            // Working forever.
            let stopped_in_place =
                *status == DoneStatus::Interrupted && stops_turn_in_place && !interrupted;
            if stopped_in_place
                && let Some(ledger) = lock(&inner.runs)
                    .get(&chat_id)
                    .filter(|h| h.run_id == run_id)
                    .map(|h| h.routed_steers.clone())
            {
                lock(&ledger).clear();
            }
            // An accepted steer awaiting its boundary owns the continuation:
            // the previous Done is an internal handoff, not a completion ping.
            // So does a message the user already sent that is still on its
            // way (see `continues_with_sent_message`); rows the user queued
            // themselves still notify.
            let pending_steer = inner.has_pending_steers(&chat_id, &run_id);
            let completed_turn = (*status == DoneStatus::Completed
                && !interrupted
                && turn_was_active
                && !pending_steer
                && !inner.continues_with_sent_message(&chat_id))
            .then(|| entry_id.clone());
            // PERSISTENT SESSION: a cleanly completed turn on a steerable
            // harness PARKS instead of ending — child + mailbox stay warm for
            // the next routed dispatch; per-turn state resets for it.
            // A runtime that lives as long as the app (Pi) also parks after a
            // failed turn: the provider failed, not the process.
            let failed_in_place =
                *status == DoneStatus::Errored && keeps_runtime_alive && !stream_failed;
            if (*status == DoneStatus::Completed || stopped_in_place || failed_in_place)
                && steerable
                && !interrupted
                && !inner.registry.update_pending(harness_id)
            {
                folded.clear();
                dirty = false;
                entry_id = new_id();
                segment_started = now_ms();
                // Resume-retry is strictly a first-turn concern.
                saw_session_started = true;
                idle_since = Some(tokio::time::Instant::now());
                reap_deferred_until = None;
                self_continued_turn = false;
                parked_errored = failed_in_place;
                // Accepted steers still own this runtime. Publishing Idle
                // here lets the ordinary queue overtake that continuation.
                inner.set_status_with_completion(
                    &chat_id,
                    if pending_steer {
                        SessionStatus::Working
                    } else if failed_in_place {
                        SessionStatus::Errored
                    } else {
                        SessionStatus::Idle
                    },
                    false,
                    completed_turn,
                );
                continue;
            }
            final_completed_turn = completed_turn;
            break match status {
                DoneStatus::Errored => SessionStatus::Errored,
                _ => SessionStatus::Idle,
            };
        }

        if !folded.is_empty() && !dirty {
            dirty = true;
            flush_at =
                tokio::time::Instant::now() + std::time::Duration::from_millis(STREAM_COMMIT_MS);
        }
    };

    // Any subagent still streaming when the run ends freezes as-is: the
    // parent process is gone, so nothing more can arrive on this stream.
    for (parent_id, sink) in subagents.drain() {
        let doc_id = sink.doc_id.clone();
        let _ = doc_ref.update_subagent_chip(&parent_id, None, Some("failed"), None);
        if let Some(json) = sink.finish(&device_id, MessageStatus::Aborted)
            && let Some(host) = inner.doc_host()
        {
            host.upload_tool_sidecar(
                &chat_id,
                zeron_doc::SidecarPayload {
                    part_id: doc_id,
                    output: Some(json),
                    diff: None,
                },
            );
        }
    }
    inner.set_running_subagents(&chat_id, Vec::new());
    // The runtime is gone, and its jobs with it.
    inner.set_background_jobs(&chat_id, 0);
    inner.set_retry(&chat_id, None);

    // Claim any accepted-but-unconfirmed steers BEFORE the handle goes away:
    // a routed send that raced this exit either finds its entry gone (we own
    // it — re-dispatched below) or reclaims it and starts a fresh run itself.
    let orphans: Vec<RoutedSteer> = lock(&inner.runs)
        .get(&chat_id)
        .filter(|h| h.run_id == run_id)
        .map(|h| std::mem::take(&mut *lock(&h.routed_steers)).into())
        .unwrap_or_default();
    inner.remove_run(&chat_id, &run_id);
    inner.set_status_with_completion(&chat_id, final_status, false, final_completed_turn);
    if let Some(cause) = &lost_runtime {
        inner.parent_notifier_lost(&chat_id, cause);
    }
    // A Stop cancels the messages it found waiting; one accepted after it
    // began is still the user's next message and must run.
    let orphans: Vec<RoutedSteer> = if inner
        .shutting_down
        .load(std::sync::atomic::Ordering::Acquire)
    {
        Vec::new()
    } else if interrupted {
        orphans.into_iter().filter(|s| s.after_stop).collect()
    } else {
        orphans
    };
    if !orphans.is_empty() {
        // The dying run accepted these into its mailbox but never confirmed a
        // Steered boundary (idle-reaper race, a mid-turn error discarding
        // queued boundary steers, a parked child death). Their user entries
        // are already in the transcript — a message that shows as sent must
        // never silently not run. Re-dispatch each as a fresh turn
        // (write_user_message dedupes by id; resume is engine-injected).
        let engine = SessionsEngine {
            inner: inner.clone(),
        };
        let chat = chat_id.clone();
        tokio::spawn(async move {
            for steer in orphans {
                let Some(mut request) = steer
                    .request
                    .map(|request| *request)
                    .or_else(|| engine.last_request(&chat))
                else {
                    tracing::warn!(chat = %chat, "orphaned steer lost: no run config to re-dispatch");
                    break;
                };
                request.prompt = steer.prompt.clone();
                request.resume = None;
                tracing::info!(chat = %chat, "re-dispatching steer orphaned by a dying run");
                let Some(host) = engine.inner.doc_host() else {
                    tracing::warn!(chat = %chat, "orphaned steer lost: doc host unavailable");
                    break;
                };
                if let Err(err) = host
                    .dispatch_with_source_context(
                        &engine,
                        &chat,
                        harness_id,
                        request,
                        Some(steer.message_id.clone()),
                    )
                    .await
                {
                    tracing::warn!(chat = %chat, error = %err, "orphaned steer re-dispatch failed");
                    engine
                        .inner
                        .set_status(&chat, SessionStatus::Errored, false);
                    break;
                }
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lost_work_is_worded_by_what_the_runtime_held() {
        assert_eq!(describe_lost_work(0, 0), None);
        assert_eq!(
            describe_lost_work(1, 0).as_deref(),
            Some("1 background job")
        );
        assert_eq!(describe_lost_work(0, 2).as_deref(), Some("2 sub-agents"));
        assert_eq!(
            describe_lost_work(2, 1).as_deref(),
            Some("2 background jobs and 1 sub-agent")
        );
    }

    #[test]
    fn a_recovery_prompt_continues_the_turn_instead_of_replaying_it() {
        let prompt = recovery_prompt("  deploy the thing\nthen tag it  ");
        assert!(
            prompt.starts_with("[Message from Zeron (automatic recovery notice). Not the user.")
        );
        assert!(prompt.contains("git status"));
        assert!(prompt.contains("do not repeat steps that already completed"));
        // The request is quoted, line by line, so it cannot pass for the notice.
        assert!(
            prompt.ends_with("> deploy the thing\n> then tag it"),
            "{prompt}"
        );
        let long = recovery_prompt(&"~".repeat(RECOVERY_QUOTE_MAX_CHARS + 50));
        assert!(long.ends_with("\n> [cut]"));
        assert_eq!(long.matches('~').count(), RECOVERY_QUOTE_MAX_CHARS);
    }

    fn sub(title: &str, at: i64, fleet_row: Option<&str>) -> zeron_proto::RunningSubagent {
        zeron_proto::RunningSubagent {
            doc_id: if fleet_row.is_some() {
                String::new()
            } else {
                format!("chat--sub--{title}")
            },
            title: title.into(),
            started_at: chrono::DateTime::<Utc>::from_timestamp_millis(at).unwrap(),
            fleet_row: fleet_row.map(Into::into),
        }
    }

    #[test]
    fn the_fleet_and_the_harness_each_replace_only_their_own_subagents() {
        let harness = vec![sub("audit", 2_000, None)];
        let list = merge_subagents(&[], false, harness.clone());
        let list = merge_subagents(&list, true, vec![sub("scan", 1_000, Some("j1"))]);
        assert_eq!(
            list,
            vec![sub("scan", 1_000, Some("j1")), sub("audit", 2_000, None)]
        );
        // The run ends: its subagents go, the fleet stays.
        let list = merge_subagents(&list, false, Vec::new());
        assert_eq!(list, vec![sub("scan", 1_000, Some("j1"))]);
        // The fleet settles: nothing is left.
        assert!(merge_subagents(&list, true, Vec::new()).is_empty());
    }

    #[test]
    fn fleet_jobs_without_a_start_keep_the_one_first_seen() {
        use zeron_proto::extension_ui::FleetJob;
        let job = |row: &str, label: &str, at: Option<i64>| FleetJob {
            row: row.into(),
            label: label.into(),
            started_at_ms: at,
        };
        let t0 = chrono::DateTime::<Utc>::from_timestamp_millis(10_000).unwrap();
        let t1 = t0 + chrono::Duration::seconds(30);
        let mut seen = HashMap::new();
        let list = fleet_list(
            vec![job("scan", "scan", None), job("j2", "lint", Some(5_000))],
            &mut seen,
            t0,
        );
        assert_eq!(
            list,
            vec![
                sub("scan", 10_000, Some("scan")),
                sub("lint", 5_000, Some("j2"))
            ]
        );
        // Watched now: the row id changes, the start does not.
        let list = fleet_list(vec![job("j1", "scan", None)], &mut seen, t1);
        assert_eq!(list, vec![sub("scan", 10_000, Some("j1"))]);
        // Gone, then back: a new job, timed afresh.
        fleet_list(Vec::new(), &mut seen, t1);
        let list = fleet_list(vec![job("j1", "scan", None)], &mut seen, t1);
        assert_eq!(list[0].started_at, t1);
    }

    #[test]
    fn cursor_recovery_converts_rich_messages_before_json_encoding() {
        let doc = zeron_doc::SessionDoc::init("cursor-rich-recovery").unwrap();
        let skill = zeron_proto::invocation::Invocation::Skill {
            name: "review \"quoted\"".into(),
            path: "/repo/quoted \"path\"/SKILL.md".into(),
            command: None,
        }
        .link();
        let previous = format!("Previous {skill}\nSecond line with \\ and \"quotes\"");
        doc.push_message(&zeron_doc::SessionMessageEntry {
            id: "u1".into(),
            role: zeron_doc::MessageRole::User,
            parts: vec![zeron_doc::MessagePart::Text {
                id: "u1-text".into(),
                text: previous.clone(),
            }],
            created_at: 0,
            device_id: "test".into(),
            status: None,
            continuation_of: None,
            duration_ms: None,
        })
        .unwrap();
        let current = format!("Current {skill}\nKeep **Markdown**");
        let delivered = super::cursor_unstarted_history(&doc, "u2", &current, false).unwrap();
        let (_, json) = delivered.split_once('\n').unwrap();
        let parsed: serde_json::Value = serde_json::from_str(json).unwrap();
        assert_eq!(
            parsed["currentUserMessage"],
            zeron_proto::invocation::harness_prompt(&current, zeron_proto::HarnessId::Cursor)
        );
        assert_eq!(
            parsed["previousUserMessages"][0],
            zeron_proto::invocation::harness_prompt(&previous, zeron_proto::HarnessId::Cursor)
        );
    }

    #[test]
    fn cursor_without_a_session_id_retains_only_preceding_user_messages() {
        let doc = zeron_doc::SessionDoc::init("cursor-unstarted").unwrap();
        for (id, role, text) in [
            (
                "u1",
                zeron_doc::MessageRole::User,
                "first interrupted request",
            ),
            ("a1", zeron_doc::MessageRole::Assistant, "partial output"),
            ("u2", zeron_doc::MessageRole::User, "current request"),
            ("u3", zeron_doc::MessageRole::User, "future pending request"),
        ] {
            doc.push_message(&zeron_doc::SessionMessageEntry {
                id: id.into(),
                role,
                parts: vec![zeron_doc::MessagePart::Text {
                    id: format!("{id}-text"),
                    text: text.into(),
                }],
                created_at: 0,
                device_id: "test".into(),
                status: None,
                continuation_of: None,
                duration_ms: None,
            })
            .unwrap();
        }
        assert_eq!(
            super::cursor_unstarted_history(&doc, "u1", "first interrupted request", false)
                .unwrap(),
            "first interrupted request"
        );
        let prompt = super::cursor_unstarted_history(&doc, "u2", "current request", false).unwrap();
        assert!(prompt.contains("first interrupted request"));
        assert!(prompt.contains("current request"));
        assert!(!prompt.contains("partial output"));
        assert!(!prompt.contains("future pending request"));
        assert!(prompt.contains("do not rerun prior tools or side effects"));
        assert_eq!(
            super::cursor_unstarted_history(&doc, "u2", "current request", true).unwrap(),
            "current request",
            "content from the preceding turn is already in the native session"
        );
        assert!(
            super::cursor_unstarted_history(&doc, "u3", "future pending request", true)
                .unwrap()
                .contains("current request"),
            "an unacknowledged prompt after an older checkpoint must survive"
        );
        assert_eq!(doc.read_entries().unwrap().len(), 4);
    }

    use zeron_proto::{HarnessId, RunRequest, SandboxLevel};

    #[tokio::test]
    async fn journal_sweep_skips_live_and_unsettled_chats() {
        use super::*;
        let dir = tempfile::tempdir().unwrap();
        let journal = Arc::new(RunJournal::open(dir.path().join("journals")).unwrap());
        let sessions = SessionsEngine::new(
            "host".into(),
            journal.clone(),
            Arc::new(HarnessRegistry::new()),
        );
        let finished_turn = |chat: &str| {
            for event in [
                AgentEvent::TextDelta { text: "a".into() },
                AgentEvent::TextDelta { text: "b".into() },
                AgentEvent::Done {
                    status: DoneStatus::Completed,
                    result: None,
                    error: None,
                    session_id: None,
                    class: None,
                },
            ] {
                journal.append(chat, &event).unwrap();
            }
        };
        for chat in ["idle", "live", "fresh"] {
            finished_turn(chat);
        }
        let inner = &sessions.inner;
        let zero = std::time::Duration::ZERO;
        let live = |chat: &str| chat == "live";

        // A chat written moments ago has not settled.
        let hour = std::time::Duration::from_secs(3600);
        assert_eq!(sweep_journals_once(inner, hour, 0, live).await, 0);
        assert_eq!(journal.size_bytes("idle"), journal.size_bytes("live"));

        let live_len = journal.size_bytes("live");
        assert_eq!(sweep_journals_once(inner, zero, 0, live).await, 2);
        assert_eq!(journal.size_bytes("live"), live_len, "live run untouched");
        assert!(journal.size_bytes("idle") < live_len);
        assert_eq!(journal.replay("idle", 0).unwrap().len(), 1);
        // Idempotent, and below-minimum journals are left alone.
        assert_eq!(sweep_journals_once(inner, zero, 0, live).await, 0);
        assert_eq!(
            sweep_journals_once(inner, zero, u64::MAX, |_| false).await,
            0
        );
        assert!(sessions.journal_bytes() > 0);
        // No run entry exists, so the engine-level pass also compacts "live".
        assert_eq!(sessions.compact_idle_journals(zero, 0).await, 1);
    }

    #[tokio::test]
    async fn a_shared_image_is_copied_in_from_the_chat_folder_only() {
        use super::*;
        let dir = tempfile::tempdir().unwrap();
        let sessions = SessionsEngine::new(
            "host".into(),
            Arc::new(RunJournal::open(dir.path().join("journals")).unwrap()),
            Arc::new(HarnessRegistry::new()),
        );
        let uploads = crate::uploads::Uploads::from_root(&dir.path().join("uploads"));
        sessions.set_generated_images(uploads, dir.path().join("codex-images"));
        let cwd = dir.path().join("project");
        let outside = dir.path().join("elsewhere");
        std::fs::create_dir_all(cwd.join(".pi/screenshots")).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        let png = b"\x89PNG\r\n\x1a\nfixture";
        let shot = cwd.join(".pi/screenshots/login.png");
        std::fs::write(&shot, png).unwrap();
        std::fs::write(outside.join("private.png"), png).unwrap();
        let share = |path: &std::path::Path, index| AgentEvent::GeneratedImage {
            id: zeron_proto::shared_image_id("s", index),
            path: path.to_string_lossy().into_owned(),
            name: "/w/login.png".into(),
            mime_type: "image/png".into(),
        };
        let cwd_text = cwd.to_string_lossy().into_owned();
        let mut seen = std::collections::HashSet::new();
        let events = sessions
            .inner
            .prepare_generated_image("chat", &cwd_text, share(&shot, 0), &mut seen)
            .await;
        let [
            AgentEvent::GeneratedImage {
                id,
                path,
                name,
                mime_type,
            },
        ] = events.as_slice()
        else {
            panic!("{events:?}")
        };
        assert_eq!(id, "s:share0");
        assert_eq!(name, "login.png", "the name is a file name, not a path");
        assert_eq!(mime_type, "image/png");
        let uploads_root = dir.path().join("uploads").canonicalize().unwrap();
        assert!(
            std::path::Path::new(path).starts_with(&uploads_root),
            "{path}"
        );
        assert_eq!(std::fs::read(path).unwrap(), png);

        // Outside the chat's folder: the share call fails, nothing is copied.
        let events = sessions
            .inner
            .prepare_generated_image(
                "chat",
                &cwd_text,
                share(&outside.join("private.png"), 1),
                &mut seen,
            )
            .await;
        assert!(
            matches!(&events[0], AgentEvent::ToolResult { id, is_error: true, .. } if id == "s"),
            "{events:?}"
        );
        assert!(
            matches!(&events[1], AgentEvent::Error { message } if message == "Shared image unavailable")
        );
    }

    #[tokio::test]
    async fn generated_image_failure_is_sanitized_even_inside_subagents() {
        use super::*;
        let dir = tempfile::tempdir().unwrap();
        let sessions = SessionsEngine::new(
            "host".into(),
            Arc::new(RunJournal::open(dir.path().join("journals")).unwrap()),
            Arc::new(HarnessRegistry::new()),
        );
        let source = "/outside/private/source.png";
        let event = AgentEvent::Subagent {
            parent_tool_use_id: "child".into(),
            event: Box::new(AgentEvent::GeneratedImage {
                id: "i:image".into(),
                path: source.into(),
                name: "secret".into(),
                mime_type: "bad".into(),
            }),
        };
        let events = sessions
            .inner
            .prepare_generated_image("chat", "", event, &mut std::collections::HashSet::new())
            .await;
        assert_eq!(events.len(), 2);
        let mut parts = vec![];
        fold_event_into_parts(
            &mut parts,
            &AgentEvent::ToolCall {
                id: "i".into(),
                call: zeron_proto::ToolCall::Unknown {
                    name: "Generate image".into(),
                    input: None,
                },
            },
        );
        for event in &events {
            sessions.inner.publish("chat", event);
            let AgentEvent::Subagent {
                parent_tool_use_id,
                event,
            } = event
            else {
                panic!("lost owner")
            };
            assert_eq!(parent_tool_use_id, "child");
            fold_event_into_parts(&mut parts, event);
        }
        assert!(matches!(
            parts[0],
            MessagePart::Tool {
                is_error: true,
                resolved: true,
                ..
            }
        ));
        assert!(matches!(parts[1], MessagePart::Error { .. }));
        let journal =
            serde_json::to_string(&sessions.inner.journal.replay("chat", 0).unwrap()).unwrap();
        assert!(!journal.contains(source));
        assert!(!journal.contains("secret"));
    }

    /// The backwards journal scan must answer exactly what the old full forward
    /// replay did: newest non-empty id, cwd from the governing `SessionStarted`.
    #[tokio::test]
    async fn journal_harness_session_matches_forward_scan() {
        fn started(id: &str, cwd: &str) -> AgentEvent {
            AgentEvent::SessionStarted {
                harness: zeron_proto::HarnessId::ClaudeCode,
                model: "m".into(),
                tools: Vec::new(),
                cwd: cwd.into(),
                session_id: id.into(),
                assistant_message_id: "a".into(),
            }
        }
        fn done(id: Option<&str>) -> AgentEvent {
            AgentEvent::Done {
                status: DoneStatus::Completed,
                result: None,
                error: None,
                session_id: id.map(Into::into),
                class: None,
            }
        }
        fn text() -> AgentEvent {
            AgentEvent::TextDelta { text: "t".into() }
        }
        /// The pre-change implementation, verbatim, over a forward replay.
        fn forward(events: Vec<(u64, AgentEvent)>) -> Option<(String, String)> {
            let mut current_cwd = String::new();
            let mut found: Option<(String, String)> = None;
            for (_, event) in events {
                match event {
                    AgentEvent::SessionStarted {
                        session_id, cwd, ..
                    } => {
                        current_cwd = cwd;
                        if !session_id.is_empty() {
                            found = Some((session_id, current_cwd.clone()));
                        }
                    }
                    AgentEvent::Done {
                        session_id: Some(session_id),
                        ..
                    } if !session_id.is_empty() => {
                        found = Some((session_id, current_cwd.clone()));
                    }
                    _ => {}
                }
            }
            found
        }

        /// (chat id, journaled events, expected `(session id, cwd)`).
        type Case = (
            &'static str,
            Vec<AgentEvent>,
            Option<(&'static str, &'static str)>,
        );
        let cases: Vec<Case> = vec![
            ("empty", vec![], None),
            ("no-ids", vec![text(), done(None), done(Some(""))], None),
            (
                "started-only",
                vec![started("s1", "/a"), text()],
                Some(("s1", "/a")),
            ),
            (
                "done-inherits-run-cwd",
                vec![started("s1", "/a"), text(), done(Some("s2"))],
                Some(("s2", "/a")),
            ),
            (
                "done-before-any-started",
                vec![text(), done(Some("s1"))],
                Some(("s1", "")),
            ),
            (
                "empty-started-sets-cwd-for-later-done",
                vec![started("s1", "/a"), started("", "/b"), done(Some("s2"))],
                Some(("s2", "/b")),
            ),
            (
                "later-empty-started-does-not-override",
                vec![
                    started("s1", "/a"),
                    done(Some("s2")),
                    started("", "/b"),
                    text(),
                ],
                Some(("s2", "/a")),
            ),
            (
                "newest-run-wins",
                vec![
                    started("s1", "/a"),
                    done(Some("s1")),
                    started("s3", "/c"),
                    text(),
                ],
                Some(("s3", "/c")),
            ),
            (
                "empty-done-ids-ignored",
                vec![started("s1", "/a"), done(Some("")), done(None)],
                Some(("s1", "/a")),
            ),
            (
                "only-empty-started-then-done-from-older-run",
                vec![
                    started("s1", "/a"),
                    done(Some("s9")),
                    started("", "/b"),
                    done(None),
                ],
                Some(("s9", "/a")),
            ),
        ];

        let dir = tempfile::tempdir().unwrap();
        let sessions = SessionsEngine::new(
            "host".into(),
            Arc::new(RunJournal::open(dir.path().join("journals")).unwrap()),
            Arc::new(HarnessRegistry::new()),
        );
        for (chat, events, want) in cases {
            for event in &events {
                sessions.inner.journal.append(chat, event).unwrap();
            }
            let want = want.map(|(id, cwd)| (id.to_string(), cwd.to_string()));
            let replayed = sessions.inner.journal.replay(chat, 0).unwrap();
            assert_eq!(forward(replayed), want, "{chat}: reference");
            assert_eq!(sessions.inner.journal_harness_session(chat), want, "{chat}");
        }
        // Torn tail: the last complete SessionStarted still governs.
        let path = dir.path().join("journals").join("started-only.jsonl");
        let mut f = std::fs::OpenOptions::new().append(true).open(path).unwrap();
        std::io::Write::write_all(&mut f, b"{\"seq\":3,\"event\":{\"type\":\"done\"").unwrap();
        assert_eq!(
            sessions.inner.journal_harness_session("started-only"),
            Some(("s1".into(), "/a".into()))
        );
    }

    fn request() -> RunRequest {
        RunRequest {
            mcp: None,
            prompt: "first".into(),
            harness: None,
            model: Some("grok-4.6".into()),
            reasoning: Some(zeron_proto::ReasoningLevel::High),
            model_options: serde_json::Map::new(),
            cwd: "/tmp".into(),
            sandbox: SandboxLevel::WorkspaceWrite,
            auto_approve: true,
            resume: None,
            attachments: Vec::new(),
            worktree: None,
        }
    }

    struct IdleStartupFixture {
        idle_calls: Arc<std::sync::atomic::AtomicUsize>,
        inference_calls: Arc<std::sync::atomic::AtomicUsize>,
        fail: bool,
    }
    #[async_trait::async_trait]
    impl zeron_harness::Harness for IdleStartupFixture {
        fn id(&self) -> HarnessId {
            HarnessId::Codex
        }
        fn display_name(&self) -> &str {
            "Offline idle fixture"
        }
        fn supports_steering(&self) -> bool {
            false
        }
        fn steering_mode(&self) -> zeron_proto::SteeringMode {
            zeron_proto::SteeringMode::TurnBoundary
        }
        fn reasoning_levels(&self) -> &[zeron_proto::ReasoningLevel] {
            &[]
        }
        async fn models(&self) -> Result<Vec<zeron_proto::Model>, zeron_harness::HarnessError> {
            Ok(vec![])
        }
        async fn start_idle(
            &self,
            request: RunRequest,
            _controls: zeron_harness::RunControls,
        ) -> Result<
            futures::stream::BoxStream<'static, Result<AgentEvent, zeron_harness::HarnessError>>,
            zeron_harness::HarnessError,
        > {
            use futures::StreamExt;
            self.idle_calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            assert!(request.prompt.is_empty());
            assert_eq!(request.resume.as_deref(), Some("old-native-thread"));
            let event = if self.fail {
                AgentEvent::Done {
                    status: zeron_proto::DoneStatus::Errored,
                    result: None,
                    error: Some("offline startup failed".into()),
                    session_id: None,
                    class: None,
                }
            } else {
                AgentEvent::SessionStarted {
                    harness: HarnessId::Codex,
                    model: "fixture".into(),
                    tools: vec![],
                    cwd: request.cwd,
                    session_id: "new-native-thread".into(),
                    assistant_message_id: "fixture-empty".into(),
                }
            };
            Ok(futures::stream::iter([Ok(event)])
                .chain(futures::stream::pending())
                .boxed())
        }
        async fn run(
            &self,
            _request: RunRequest,
            _controls: zeron_harness::RunControls,
        ) -> Result<
            futures::stream::BoxStream<'static, Result<AgentEvent, zeron_harness::HarnessError>>,
            zeron_harness::HarnessError,
        > {
            self.inference_calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Err(zeron_harness::HarnessError::Protocol(
                "fixture forbids inference".into(),
            ))
        }
    }

    async fn idle_startup_case(fail: bool) {
        use super::*;
        let temp = tempfile::tempdir().unwrap();
        let idle_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let inference_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let registry = crate::HarnessRegistry::new();
        registry.register(Arc::new(IdleStartupFixture {
            idle_calls: idle_calls.clone(),
            inference_calls: inference_calls.clone(),
            fail,
        }));
        let core = crate::EngineCore::assemble_with_profile(
            crate::EngineProfile::local(temp.path()).unwrap(),
            Arc::new(registry),
            HarnessId::Codex,
            None,
        )
        .unwrap();
        core.workspace
            .create_chat("idle", None, Some(&core.device_id), None, None)
            .unwrap();
        let mut request = request();
        request.prompt.clear();
        core.sessions
            .inner
            .remember_harness_session("idle", "old-native-thread", &request.cwd);
        core.sessions.start_idle("idle", request).await.unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                let completed = if fail {
                    core.sessions
                        .session_status("idle")
                        .is_some_and(|s| s.status == SessionStatus::Errored)
                } else {
                    core.sessions.inner.resume_for("idle", "/tmp").as_deref()
                        == Some("new-native-thread")
                };
                if completed {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(idle_calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(inference_calls.load(std::sync::atomic::Ordering::SeqCst), 0);
        let entries = core
            .doc_host
            .open("idle")
            .unwrap()
            .doc()
            .read_entries()
            .unwrap();
        assert!(
            !entries
                .iter()
                .any(|entry| entry.role == zeron_doc::MessageRole::User)
        );
        if !fail {
            assert!(entries.is_empty());
            assert_eq!(
                core.sessions.session_status("idle").unwrap().status,
                SessionStatus::Idle
            );
            assert_eq!(
                core.workspace.chat_harness_session("idle").unwrap().0,
                "new-native-thread"
            );
            let events = core.sessions.inner.journal.replay("idle", 0).unwrap();
            assert!(events.iter().any(|(_, event)| matches!(event, AgentEvent::SessionStarted { session_id, .. } if session_id == "new-native-thread")));
        }
        core.sessions.shutdown().await;
    }
    #[tokio::test]
    async fn voice_idle_startup_failure_never_retries_as_inference() {
        idle_startup_case(true).await;
    }
    #[tokio::test]
    async fn voice_idle_startup_remembers_thread_without_creating_messages() {
        idle_startup_case(false).await;
    }

    #[test]
    fn live_routing_requires_the_same_runtime_configuration() {
        let initial = request();
        let config = RuntimeConfig::new(
            HarnessId::Grok,
            &initial,
            Arc::new(zeron_harness::AcpHarness::grok()),
        );

        let mut follow_up = initial.clone();
        follow_up.prompt = "second".into();
        follow_up.resume = Some("session-1".into());
        assert!(config.can_route(HarnessId::Grok, &follow_up));
        assert!(!config.can_route(HarnessId::Devin, &follow_up));

        // Images ride the mailbox: they never force a replacement runtime.
        follow_up.attachments.push("/tmp/image.png".into());
        assert!(config.can_route(HarnessId::Grok, &follow_up));

        // Another client's spelling of the same option value is the same runtime.
        let mut spelled = initial.clone();
        spelled
            .model_options
            .insert("fastMode".into(), serde_json::json!(true));
        let config = RuntimeConfig::new(
            HarnessId::Grok,
            &spelled,
            Arc::new(zeron_harness::AcpHarness::grok()),
        );
        follow_up
            .model_options
            .insert("fastMode".into(), serde_json::json!("true"));
        assert!(config.can_route(HarnessId::Grok, &follow_up));

        follow_up.model = Some("grok-4.5".into());
        assert!(!config.can_route(HarnessId::Grok, &follow_up));
        follow_up.model = initial.model.clone();

        follow_up.reasoning = Some(zeron_proto::ReasoningLevel::Medium);
        assert!(!config.can_route(HarnessId::Grok, &follow_up));
    }

    #[test]
    fn claude_routing_compares_what_the_cli_launches_with() {
        let claude = || -> Arc<dyn Harness> { Arc::new(zeron_harness::ClaudeHarness::new()) };
        let mut initial = request();
        initial.model = Some("claude-opus-5-5".into());
        initial
            .model_options
            .insert("fastMode".into(), serde_json::json!(false));
        let config = RuntimeConfig::new(HarnessId::ClaudeCode, &initial, claude());

        // The CLI never reads the sandbox level; an option that launches no
        // flag in either spelling is the same runtime.
        let mut follow_up = initial.clone();
        follow_up.sandbox = SandboxLevel::DangerFullAccess;
        follow_up.model_options.clear();
        follow_up.worktree = None;
        assert!(config.can_route(HarnessId::ClaudeCode, &follow_up));

        follow_up
            .model_options
            .insert("fastMode".into(), serde_json::json!("true"));
        assert!(!config.can_route(HarnessId::ClaudeCode, &follow_up));
        follow_up.model_options.clear();

        follow_up.cwd = "/elsewhere".into();
        assert!(!config.can_route(HarnessId::ClaudeCode, &follow_up));
    }

    #[test]
    fn clean_tool_ids_ride_verbatim_and_fit_id_re() {
        let id = subagent_doc_id("chat-abc123", "toolu_01EjFLnNhCiMR2PBNKcVvkT");
        assert_eq!(id, "chat-abc123--sub--toolu_01EjFLnNhCiMR2PBNKcVvkT");
        assert!(id.len() <= 128);
        assert!(
            id.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        );
    }

    #[test]
    fn unclean_or_oversized_ids_hash_deterministically() {
        let dirty = subagent_doc_id("chat", "call/with:odd chars");
        assert!(
            dirty
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-'),
            "{dirty}"
        );
        assert_eq!(dirty, subagent_doc_id("chat", "call/with:odd chars"));
        let long_chat = "c".repeat(100);
        let long = subagent_doc_id(&long_chat, &"t".repeat(64));
        assert!(long.len() <= 128, "{}", long.len());
        // Distinct inputs stay distinct.
        assert_ne!(
            subagent_doc_id("chat", "a:b"),
            subagent_doc_id("chat", "a:c")
        );
    }

    #[tokio::test]
    async fn subagent_sink_flush_clears_dirty_when_nothing_folded() {
        use super::*;
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(zeron_sync::DocsStore::open(dir.path()).unwrap());
        let host = DocHost::new(
            store,
            crate::doc_host::DocHostConfig {
                device_id: "dev-test".into(),
                default_harness: HarnessId::Mock,
                edge: None,
            },
        );
        let handle = host.open("chat-sub-flush--sub--t1").unwrap();
        let mut sink = SubagentSink {
            doc_id: "chat-sub-flush--sub--t1".into(),
            title: "Subagent".into(),
            doc: handle.writer(),
            entry_id: "entry-1".into(),
            started_at: 0,
            entry_index: None,
            written: Vec::new(),
            folded: Vec::new(),
            dirty: true,
        };
        // Nothing to write is still a completed flush — left set, the flag
        // holds the run loop's commit guard open on a past deadline.
        sink.flush("dev-test");
        assert!(!sink.dirty);
    }

    /// Feed-by-hand harness: the test pushes wire events through a channel
    /// and decides when the stream ends.
    struct FeedHarness {
        feed: Mutex<Option<mpsc::UnboundedReceiver<AgentEvent>>>,
    }

    #[async_trait::async_trait]
    impl Harness for FeedHarness {
        fn id(&self) -> HarnessId {
            HarnessId::Mock
        }
        fn display_name(&self) -> &str {
            "Feed"
        }
        fn supports_steering(&self) -> bool {
            true
        }
        fn steering_mode(&self) -> zeron_proto::SteeringMode {
            zeron_proto::SteeringMode::StepBoundary
        }
        fn reasoning_levels(&self) -> &[zeron_proto::ReasoningLevel] {
            &[zeron_proto::ReasoningLevel::Medium]
        }
        async fn models(&self) -> Result<Vec<zeron_proto::Model>, zeron_harness::HarnessError> {
            Ok(vec![])
        }
        async fn run(
            &self,
            _request: RunRequest,
            _controls: RunControls,
        ) -> Result<
            futures::stream::BoxStream<'static, Result<AgentEvent, zeron_harness::HarnessError>>,
            zeron_harness::HarnessError,
        > {
            let mut feed = self
                .feed
                .lock()
                .unwrap()
                .take()
                .expect("FeedHarness serves one run per test");
            Ok(futures::stream::poll_fn(move |cx| feed.poll_recv(cx).map(|e| e.map(Ok))).boxed())
        }
    }

    // A tagged subagent event that folds to NO parts used to leave its sink
    // dirty forever: the commit branch's guard stayed true on a deadline in
    // the past and the run loop burned a core until real content arrived.
    #[tokio::test]
    async fn empty_subagent_events_commit_once_per_window_not_per_tick() {
        use super::*;
        FLUSH_TICKS.with(|t| t.set(0));
        let (feed, rx) = mpsc::unbounded_channel();
        let registry = HarnessRegistry::new();
        registry.register(Arc::new(FeedHarness {
            feed: Mutex::new(Some(rx)),
        }));
        let dir = tempfile::tempdir().unwrap();
        let core =
            crate::EngineCore::assemble(dir.path(), Arc::new(registry), HarnessId::Mock, None)
                .unwrap();
        let chat = "chat-sink-spin";
        core.sessions
            .dispatch(chat, HarnessId::Mock, request(), None)
            .await
            .unwrap();
        feed.send(AgentEvent::SessionStarted {
            harness: HarnessId::Mock,
            model: "mock-1".into(),
            tools: vec![],
            cwd: "/tmp".into(),
            session_id: "hs-1".into(),
            assistant_message_id: "a1".into(),
        })
        .unwrap();
        feed.send(AgentEvent::Subagent {
            parent_tool_use_id: "tool-1".into(),
            event: Box::new(AgentEvent::ReasoningDelta {
                text: String::new(),
            }),
        })
        .unwrap();
        // Well past several commit windows with the stream still open: the
        // loop must idle between commits, not spin on a past deadline.
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        let ticks = FLUSH_TICKS.with(|t| t.get());
        assert!(
            (1..=8).contains(&ticks),
            "commit branch fired {ticks} times in 1s for one empty subagent event"
        );
        assert_eq!(
            core.sessions.session_status(chat).map(|s| s.status),
            Some(SessionStatus::Working),
            "stream still open, turn still running"
        );
        feed.send(AgentEvent::Done {
            status: DoneStatus::Completed,
            result: None,
            error: None,
            session_id: None,
            class: None,
        })
        .unwrap();
        core.sessions.shutdown().await;
    }
}
