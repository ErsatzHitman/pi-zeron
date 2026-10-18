//! Restart round-trip + harness resume continuity (the "chats forget everything
//! after an app restart" regression): the EMBED assembly (`EngineCore::assemble`)
//! is run twice over one data dir, asserting
//! - chats + transcripts survive a graceful shutdown → relaunch;
//! - the next run in an existing chat carries the chat's stored harness-native
//!   session id as `RunRequest.resume` (engine-owned, zeron sessions.ts:736);
//! - a kill -9 style crash recovers the session id from the run journal
//!   (zeron recoverDraft, sessions.ts:538-552) and stamps streaming entries
//!   `aborted`;
//! - resume is cwd-scoped (harness session stores are keyed by cwd);
//! - a startup crash retries once with the resume kept, and a helper that is
//!   down hard never tombstones the stored session id;
//! - a steer with no live run after a restart dispatches as a new turn that
//!   still resumes the prior conversation.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use futures::stream::BoxStream;

use zeron_doc::{
    MessagePart, MessageRole, MessageStatus, SessionCommandPayload, SessionDoc, SessionMessageEntry,
};
use zeron_engine::{EngineCore, HarnessRegistry, RunJournal};
use zeron_harness::{Harness, HarnessError, RunControls};
use zeron_proto::{
    AgentEvent, DoneStatus, HarnessId, Model, ReasoningLevel, RunRequest, SandboxLevel,
    SteeringMode,
};
use zeron_sync::DocsStore;

const CHAT: &str = "chat-restart";

type RequestLog = Arc<Mutex<Vec<RunRequest>>>;

/// The prompt a revived run starts from: the recovery notice quoting the
/// request that was interrupted, not the request itself.
fn is_recovery_of(prompt: &str, request: &str) -> bool {
    prompt.starts_with("[Message from Zeron (automatic recovery notice)")
        && prompt.contains(&format!("> {request}"))
}

fn run_request(prompt: &str, cwd: &str) -> RunRequest {
    RunRequest {
        mcp: None,
        prompt: prompt.into(),
        harness: None,
        model: None,
        reasoning: None,
        model_options: Default::default(),
        cwd: cwd.into(),
        sandbox: SandboxLevel::WorkspaceWrite,
        auto_approve: true,
        attachments: Vec::new(),
        worktree: None,
        resume: None,
    }
}

/// Records every `RunRequest` it receives (the resume-injection probe). A
/// successful run emits `SessionStarted{session_id}` … `Done{session_id}`;
/// while `fail_starts` is positive, a run dies the way a crashed agent child
/// does — an errored Done before any session starts — decrementing the
/// counter (so a transient spawn blip is one failure, a helper that is down
/// hard is `u32::MAX`).
struct RecordingHarness {
    requests: RequestLog,
    session_id: String,
    fail_starts: Arc<Mutex<u32>>,
}

#[async_trait]
impl Harness for RecordingHarness {
    fn id(&self) -> HarnessId {
        HarnessId::Mock
    }
    fn display_name(&self) -> &str {
        "Recording"
    }
    fn supports_steering(&self) -> bool {
        false
    }
    fn steering_mode(&self) -> SteeringMode {
        SteeringMode::TurnBoundary
    }
    fn reasoning_levels(&self) -> &[ReasoningLevel] {
        &[ReasoningLevel::Medium]
    }
    async fn models(&self) -> Result<Vec<Model>, HarnessError> {
        Ok(vec![])
    }
    async fn run(
        &self,
        request: RunRequest,
        _controls: RunControls,
    ) -> Result<BoxStream<'static, Result<AgentEvent, HarnessError>>, HarnessError> {
        self.requests
            .lock()
            .expect("request log")
            .push(request.clone());
        let fail = {
            let mut left = self.fail_starts.lock().expect("fail counter");
            if *left > 0 {
                *left -= 1;
                true
            } else {
                false
            }
        };
        let events: Vec<Result<AgentEvent, HarnessError>> = if fail {
            vec![Ok(AgentEvent::Done {
                status: DoneStatus::Errored,
                result: None,
                error: Some("Recording exited unexpectedly (exit code 1): boom".into()),
                session_id: None,
                class: None,
            })]
        } else {
            vec![
                Ok(AgentEvent::SessionStarted {
                    harness: HarnessId::Mock,
                    model: "mock-1".into(),
                    tools: vec![],
                    cwd: request.cwd.clone(),
                    session_id: self.session_id.clone(),
                    assistant_message_id: "a-1".into(),
                }),
                Ok(AgentEvent::TextDelta {
                    text: format!("ack: {}", request.prompt),
                }),
                Ok(AgentEvent::Done {
                    status: DoneStatus::Completed,
                    result: None,
                    error: None,
                    session_id: Some(self.session_id.clone()),
                    class: None,
                }),
            ]
        };
        Ok(futures::stream::iter(events).boxed())
    }
}

fn assemble(dir: &std::path::Path, harness: RecordingHarness) -> EngineCore {
    let registry = HarnessRegistry::new();
    registry.register(Arc::new(harness));
    EngineCore::assemble(dir, Arc::new(registry), HarnessId::Mock, None)
        .expect("engine core assembles")
}

fn queue_run(core: &EngineCore, prompt: &str, cwd: &str, message_id: &str) {
    core.doc_host
        .queue_command(
            CHAT,
            SessionCommandPayload::Run {
                request: run_request(prompt, cwd),
                message_id: message_id.into(),
            },
        )
        .expect("queue run command");
}

async fn wait_for<F>(predicate: F, what: &str)
where
    F: FnMut() -> bool,
{
    wait_for_within(predicate, what, Duration::from_secs(10)).await;
}

async fn wait_for_within<F>(mut predicate: F, what: &str, deadline: Duration)
where
    F: FnMut() -> bool,
{
    let deadline = tokio::time::Instant::now() + deadline;
    while !predicate() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for {what}"
        );
        tokio::time::sleep(Duration::from_millis(15)).await;
    }
}

/// Tolerant read for hot-polling predicates (mirrors e2e.rs `entries_now`).
fn entries_now(core: &EngineCore) -> Vec<SessionMessageEntry> {
    core.doc_host
        .open(CHAT)
        .ok()
        .and_then(|h| h.doc().read_entries().ok())
        .unwrap_or_default()
}

fn complete_assistant_count(core: &EngineCore) -> usize {
    entries_now(core)
        .iter()
        .filter(|e| e.role == MessageRole::Assistant && e.status == Some(MessageStatus::Complete))
        .count()
}

fn stored_harness_session(core: &EngineCore) -> Option<(String, Option<String>)> {
    let chat = core
        .workspace
        .chat(CHAT)
        .expect("read chat row")
        .expect("chat row exists");
    chat.harness_session_id
        .map(|id| (id, chat.harness_session_cwd))
}

/// Create + name the chat row up front so the auto-titler (which runs its own
/// harness request after a completed exchange on an UNTITLED chat) stays out
/// of the recorded request log.
fn pre_title(core: &EngineCore) {
    core.workspace
        .create_space("space-restart", &core.device_id, "/tmp", None, false)
        .expect("create space row");
    core.workspace
        .create_chat(CHAT, Some("space-restart"), None, None, None)
        .expect("create chat row");
    core.workspace
        .rename_chat(CHAT, "Pre-titled")
        .expect("rename chat");
}

/// One full turn in a fresh engine over `dir`, then graceful shutdown — the
/// "before restart" phase shared by the tests below.
async fn run_one_turn_and_shutdown(dir: &std::path::Path, requests: &RequestLog, session: &str) {
    let core = assemble(
        dir,
        RecordingHarness {
            requests: requests.clone(),
            session_id: session.into(),
            fail_starts: Default::default(),
        },
    );
    pre_title(&core);
    queue_run(
        &core,
        "remember the codeword PINEAPPLE",
        "/tmp",
        "msg-user-1",
    );
    wait_for(
        || complete_assistant_count(&core) == 1,
        "first turn to complete",
    )
    .await;
    core.shutdown().await;
    drop(core);
}

#[tokio::test]
async fn restart_roundtrip_restores_chats_transcript_and_resume() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("data");
    let requests: RequestLog = Arc::new(Mutex::new(Vec::new()));

    run_one_turn_and_shutdown(&dir, &requests, "hs-restart-1").await;
    assert_eq!(
        requests.lock().unwrap()[0].resume,
        None,
        "a chat's first run must start a fresh harness session"
    );

    // Relaunch over the same data dir (the embedded-engine restart path).
    let core = assemble(
        &dir,
        RecordingHarness {
            requests: requests.clone(),
            session_id: "hs-restart-2".into(),
            fail_starts: Default::default(),
        },
    );

    // Sidebar state survived: the chat row is back with its cwd, preview, and
    // the stored harness session (cwd-scoped).
    let chats = core.workspace.read_chats().expect("read chats");
    assert_eq!(chats.len(), 1, "chat row survives restart: {chats:#?}");
    assert_eq!(chats[0].id, CHAT);
    assert_eq!(chats[0].cwd.as_deref(), Some("/tmp"));
    assert!(chats[0].last_message_preview.is_some());
    assert_eq!(
        stored_harness_session(&core),
        Some(("hs-restart-1".into(), Some("/tmp".into())))
    );

    // Transcript survived: user + completed assistant entry, texts intact.
    let entries = entries_now(&core);
    assert_eq!(
        entries.len(),
        2,
        "transcript survives restart: {entries:#?}"
    );
    assert_eq!(entries[0].id, "msg-user-1");
    assert_eq!(entries[0].role, MessageRole::User);
    assert_eq!(entries[1].role, MessageRole::Assistant);
    assert_eq!(entries[1].status, Some(MessageStatus::Complete));
    assert!(matches!(
        &entries[1].parts[0],
        MessagePart::Text { text, .. } if text.contains("PINEAPPLE")
    ));

    // The next run resumes the SAME harness conversation: the engine injects
    // the stored session id even though the caller sent `resume: None`.
    queue_run(&core, "what was the codeword?", "/tmp", "msg-user-2");
    wait_for(
        || complete_assistant_count(&core) == 2,
        "second turn to complete",
    )
    .await;
    {
        let log = requests.lock().unwrap();
        assert_eq!(log.len(), 2);
        assert_eq!(
            log[1].resume.as_deref(),
            Some("hs-restart-1"),
            "post-restart dispatch must resume the stored harness session"
        );
    }
    // The fresh turn's session id replaces the stored one.
    assert_eq!(
        stored_harness_session(&core),
        Some(("hs-restart-2".into(), Some("/tmp".into())))
    );
    core.shutdown().await;
}

#[tokio::test]
async fn kill_crash_recovers_resume_from_journal_and_stamps_aborted() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("data");
    std::fs::create_dir_all(&dir).unwrap();
    // Pin the device id so the manufactured streaming entry counts as OURS.
    std::fs::write(dir.join("device-id"), "dev-crash").unwrap();

    // Manufacture the on-disk state a kill -9 mid-run leaves behind:
    // - a chat doc snapshot whose assistant entry is still `streaming`;
    // - a journal whose last event is NOT `Done` (run died mid-stream), holding
    //   the only copy of the harness session id (the debounced workspace-row
    //   write never landed).
    {
        let store = DocsStore::open(dir.join("orgs/dev-org/dev-user")).unwrap();
        let doc = SessionDoc::init(CHAT).unwrap();
        doc.push_message(&SessionMessageEntry {
            id: "msg-user-1".into(),
            role: MessageRole::User,
            parts: vec![MessagePart::Text {
                id: "t0".into(),
                text: "long task".into(),
            }],
            created_at: 1,
            device_id: "dev-crash".into(),
            status: Some(MessageStatus::Complete),
            continuation_of: None,
            duration_ms: None,
        })
        .unwrap();
        doc.push_message(&SessionMessageEntry {
            id: "msg-assistant-1".into(),
            role: MessageRole::Assistant,
            parts: vec![MessagePart::Text {
                id: "t0".into(),
                text: "partial…".into(),
            }],
            created_at: 2,
            device_id: "dev-crash".into(),
            status: Some(MessageStatus::Streaming),
            continuation_of: None,
            duration_ms: None,
        })
        .unwrap();
        store
            .save_snapshot(CHAT, &doc.export_snapshot().unwrap())
            .unwrap();

        let journal = RunJournal::open(dir.join("orgs/dev-org/dev-user/journals")).unwrap();
        journal
            .append(
                CHAT,
                &AgentEvent::SessionStarted {
                    harness: HarnessId::Mock,
                    model: "mock-1".into(),
                    tools: vec![],
                    cwd: "/tmp".into(),
                    session_id: "hs-crash".into(),
                    assistant_message_id: "msg-assistant-1".into(),
                },
            )
            .unwrap();
        journal
            .append(
                CHAT,
                &AgentEvent::TextDelta {
                    text: "partial…".into(),
                },
            )
            .unwrap();
    }

    let requests: RequestLog = Arc::new(Mutex::new(Vec::new()));
    let core = assemble(
        &dir,
        RecordingHarness {
            requests: requests.clone(),
            session_id: "hs-after-crash".into(),
            fail_starts: Default::default(),
        },
    );
    assert_eq!(core.device_id, "dev-crash");

    // Boot recovery stamped the abandoned streaming entry `aborted` …
    let entries = entries_now(&core);
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[1].status, Some(MessageStatus::Aborted));
    // … and closed the stale journal with a synthetic Done.
    let journal = RunJournal::open(dir.join("orgs/dev-org/dev-user/journals")).unwrap();
    assert!(matches!(
        journal.last_event(CHAT).unwrap(),
        Some((_, AgentEvent::Done { .. }))
    ));

    // The next run resumes the crashed conversation: the session id was
    // recovered from the journal (its only surviving home).
    pre_title(&core);
    queue_run(&core, "keep going", "/tmp", "msg-user-2");
    wait_for(
        || complete_assistant_count(&core) == 1,
        "post-crash turn to complete",
    )
    .await;
    assert_eq!(
        requests.lock().unwrap()[0].resume.as_deref(),
        Some("hs-crash"),
        "journal-recovered session id must ride the next dispatch"
    );
    core.shutdown().await;
}

/// Launch reaps Pi processes a killed engine left behind (marked with a dead
/// engine's identity) before recovery runs, and touches nothing else.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn launch_reaps_orphans_of_a_dead_engine_but_spares_live_and_unmarked() {
    use std::process::{Child, Command, Stdio};

    struct Reap(Child);
    impl Drop for Reap {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    fn sleeper(env: Option<(String, String)>) -> Reap {
        let mut command = Command::new("sleep");
        command
            .arg("60")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        if let Some((pid, start)) = &env {
            command
                .env("ZERON_ENGINE_PID", pid)
                .env("ZERON_ENGINE_START", start);
        }
        let child = Reap(command.spawn().unwrap());
        // The new environment is visible only once the child has exec'd.
        for _ in 0..300 {
            let environ =
                std::fs::read(format!("/proc/{}/environ", child.0.id())).unwrap_or_default();
            if env.is_none() || String::from_utf8_lossy(&environ).contains("ZERON_ENGINE_PID") {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        child
    }
    fn starttime(pid: u32) -> String {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
        stat[stat.rfind(')').unwrap() + 1..]
            .split_whitespace()
            .nth(19)
            .unwrap()
            .to_string()
    }

    let mut dead = Command::new("true").spawn().unwrap();
    let dead_pid = dead.id();
    dead.wait().unwrap();
    // A second, still running "engine" whose children must be left alone.
    let other_engine = sleeper(None);
    let other_pid = other_engine.0.id();

    let mut orphan = sleeper(Some((dead_pid.to_string(), "1".into())));
    let mut live = sleeper(Some((other_pid.to_string(), starttime(other_pid))));
    let mut unmarked = sleeper(None);

    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("data");
    std::fs::create_dir_all(&dir).unwrap();
    let core = assemble(
        &dir,
        RecordingHarness {
            requests: Arc::new(Mutex::new(Vec::new())),
            session_id: "hs".into(),
            fail_starts: Default::default(),
        },
    );
    assert!(
        orphan.0.try_wait().unwrap().is_some(),
        "orphan of a dead engine must be dead once assemble returns"
    );
    assert!(live.0.try_wait().unwrap().is_none());
    assert!(unmarked.0.try_wait().unwrap().is_none());
    core.shutdown().await;
}

/// The idle journal sweep rewrites a finished chat's journal; a crash in the NEXT
/// turn must still be revived from it (session id from the journal, stale journal
/// closed by boot recovery).
#[tokio::test]
async fn compacted_journal_still_revives_a_crashed_next_turn() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("data");
    let requests: RequestLog = Arc::new(Mutex::new(Vec::new()));
    let journals = dir.join("orgs/dev-org/dev-user/journals");

    let core = assemble(
        &dir,
        RecordingHarness {
            requests: requests.clone(),
            session_id: "hs-first".into(),
            fail_starts: Default::default(),
        },
    );
    pre_title(&core);
    queue_run(&core, "first turn", "/tmp", "msg-user-1");
    wait_for(
        || complete_assistant_count(&core) == 1,
        "first turn to complete",
    )
    .await;
    wait_for(|| !core.sessions.has_run(CHAT), "the run to end").await;

    // The sweep compacts the finished turn's deltas away.
    let compacted = core.sessions.compact_idle_journals(Duration::ZERO, 0).await;
    assert_eq!(compacted, 1);
    core.shutdown().await;
    drop(core);
    let journal = RunJournal::open(&journals).unwrap();
    let events = journal.replay(CHAT, 0).unwrap();
    assert!(
        !events
            .iter()
            .any(|(_, e)| matches!(e, AgentEvent::TextDelta { .. })),
        "{events:?}"
    );
    let last_seq = events.last().unwrap().0;

    // The next turn dies mid-stream, after the compaction.
    journal
        .append(
            CHAT,
            &AgentEvent::SessionStarted {
                harness: HarnessId::Mock,
                model: "mock-1".into(),
                tools: vec![],
                cwd: "/tmp".into(),
                session_id: "hs-crash-2".into(),
                assistant_message_id: "a-2".into(),
            },
        )
        .unwrap();
    journal
        .append(
            CHAT,
            &AgentEvent::TextDelta {
                text: "partial".into(),
            },
        )
        .unwrap();
    assert_eq!(journal.stale_sessions().unwrap(), vec![CHAT.to_string()]);
    drop(journal);

    let core = assemble(
        &dir,
        RecordingHarness {
            requests: requests.clone(),
            session_id: "hs-after".into(),
            fail_starts: Default::default(),
        },
    );
    let journal = RunJournal::open(&journals).unwrap();
    assert!(journal.stale_sessions().unwrap().is_empty());
    let (seq, event) = journal.last_event(CHAT).unwrap().unwrap();
    assert!(seq > last_seq + 2, "seqs stay monotonic across compaction");
    assert!(matches!(event, AgentEvent::Done { .. }));

    queue_run(&core, "keep going", "/tmp", "msg-user-2");
    wait_for(
        || complete_assistant_count(&core) == 2,
        "post-crash turn to complete",
    )
    .await;
    assert_eq!(
        requests.lock().unwrap()[1].resume.as_deref(),
        Some("hs-crash-2"),
        "journal-recovered session id must ride the next dispatch"
    );
    core.shutdown().await;
}

/// A harness whose stream stays OPEN after each turn's Done, serving follow-up
/// turns from the steering mailbox — the persistent-session shape (codex; and
/// claude's stream-json stdin). Counts `run()` calls to prove the engine
/// reuses one child across turns instead of respawning.
struct PersistentHarness {
    runs_started: Arc<Mutex<usize>>,
}

#[async_trait]
impl Harness for PersistentHarness {
    fn id(&self) -> HarnessId {
        HarnessId::Mock
    }
    fn display_name(&self) -> &str {
        "Persistent"
    }
    fn supports_steering(&self) -> bool {
        true
    }
    fn steering_mode(&self) -> SteeringMode {
        SteeringMode::StepBoundary
    }
    fn reasoning_levels(&self) -> &[ReasoningLevel] {
        &[ReasoningLevel::Medium]
    }
    async fn models(&self) -> Result<Vec<Model>, HarnessError> {
        Ok(vec![])
    }
    async fn run(
        &self,
        request: RunRequest,
        controls: RunControls,
    ) -> Result<BoxStream<'static, Result<AgentEvent, HarnessError>>, HarnessError> {
        *self.runs_started.lock().unwrap() += 1;
        let (tx, rx) = tokio::sync::mpsc::channel::<Result<AgentEvent, HarnessError>>(32);
        let mut steering = controls.steering;
        tokio::spawn(async move {
            let turn = |n: usize, prompt: &str| {
                vec![
                    AgentEvent::TextDelta {
                        text: format!("turn {n} ack: {prompt}"),
                    },
                    AgentEvent::Done {
                        status: DoneStatus::Completed,
                        result: None,
                        error: None,
                        session_id: Some("hs-persist".into()),
                        class: None,
                    },
                ]
            };
            let first = vec![AgentEvent::SessionStarted {
                harness: HarnessId::Mock,
                model: "mock-1".into(),
                tools: vec![],
                cwd: "/tmp".into(),
                session_id: "hs-persist".into(),
                assistant_message_id: "a-1".into(),
            }];
            for ev in first.into_iter().chain(turn(1, "first")) {
                if tx.send(Ok(ev)).await.is_err() {
                    return;
                }
            }
            // Parked: serve follow-up turns from the mailbox until the
            // engine hangs up (idle reap / interrupt / shutdown).
            let mut n = 1usize;
            while let Some(steer) = steering.recv().await {
                n += 1;
                let boundary = AgentEvent::Steered {
                    assistant_message_id: None,
                    next_assistant_message_id: steer.message_id.map(|id| format!("a-{id}")),
                };
                if tx.send(Ok(boundary)).await.is_err() {
                    return;
                }
                for ev in turn(n, &steer.prompt) {
                    if tx.send(Ok(ev)).await.is_err() {
                        return;
                    }
                }
            }
        });
        Ok(futures::stream::unfold(
            rx,
            |mut rx| async move { rx.recv().await.map(|ev| (ev, rx)) },
        )
        .boxed())
    }
}

#[tokio::test]
async fn persistent_session_serves_multiple_turns_on_one_child() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("data");
    std::fs::create_dir_all(&dir).unwrap();

    let runs_started = Arc::new(Mutex::new(0usize));
    let registry = HarnessRegistry::new();
    registry.register(Arc::new(PersistentHarness {
        runs_started: runs_started.clone(),
    }));
    let core = EngineCore::assemble(&dir, Arc::new(registry), HarnessId::Mock, None)
        .expect("engine core assembles");
    pre_title(&core);

    queue_run(&core, "first", "/tmp", "msg-user-1");
    wait_for(
        || complete_assistant_count(&core) == 1,
        "first turn to complete",
    )
    .await;

    // The session PARKS (zeron runsBySession): the second message routes into
    // the live child instead of spawning a new one.
    queue_run(&core, "second", "/tmp", "msg-user-2");
    wait_for(
        || complete_assistant_count(&core) == 2,
        "second turn to complete on the same child",
    )
    .await;

    assert_eq!(
        *runs_started.lock().unwrap(),
        1,
        "one harness child must serve both turns"
    );
    let entries = entries_now(&core);
    assert_eq!(
        entries
            .iter()
            .filter(|e| e.role == MessageRole::User)
            .count(),
        2
    );
    core.shutdown().await;
}

#[tokio::test]
async fn fresh_crash_auto_resumes_and_notes_the_interruption() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("data");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("device-id"), "dev-crash").unwrap();

    // Same manufactured kill -9 state as above, but FRESH: the streaming entry
    // crashed moments ago, inside the 12h revival window.
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    {
        let store = DocsStore::open(dir.join("orgs/dev-org/dev-user")).unwrap();
        let doc = SessionDoc::init(CHAT).unwrap();
        doc.push_message(&SessionMessageEntry {
            id: "msg-user-1".into(),
            role: MessageRole::User,
            parts: vec![MessagePart::Text {
                id: "t0".into(),
                text: "long task".into(),
            }],
            created_at: now - 60_000,
            device_id: "dev-crash".into(),
            status: Some(MessageStatus::Complete),
            continuation_of: None,
            duration_ms: None,
        })
        .unwrap();
        doc.push_message(&SessionMessageEntry {
            id: "msg-assistant-1".into(),
            role: MessageRole::Assistant,
            parts: vec![MessagePart::Text {
                id: "t0".into(),
                text: "partial…".into(),
            }],
            created_at: now - 30_000,
            device_id: "dev-crash".into(),
            status: Some(MessageStatus::Streaming),
            continuation_of: None,
            duration_ms: None,
        })
        .unwrap();
        store
            .save_snapshot(CHAT, &doc.export_snapshot().unwrap())
            .unwrap();

        let journal = RunJournal::open(dir.join("orgs/dev-org/dev-user/journals")).unwrap();
        journal
            .append(
                CHAT,
                &AgentEvent::SessionStarted {
                    harness: HarnessId::Mock,
                    model: "mock-1".into(),
                    tools: vec![],
                    cwd: "/tmp".into(),
                    session_id: "hs-crash".into(),
                    assistant_message_id: "msg-assistant-1".into(),
                },
            )
            .unwrap();
        journal
            .append(
                CHAT,
                &AgentEvent::TextDelta {
                    text: "partial…".into(),
                },
            )
            .unwrap();
    }

    let requests: RequestLog = Arc::new(Mutex::new(Vec::new()));
    let core = assemble(
        &dir,
        RecordingHarness {
            requests: requests.clone(),
            session_id: "hs-after-crash".into(),
            fail_starts: Default::default(),
        },
    );

    // The run is PICKED BACK UP without any user action (zeron: "not just
    // eulogized"): recovery re-dispatches the crashed prompt itself.
    wait_for(
        || complete_assistant_count(&core) == 1,
        "auto-resumed turn to complete",
    )
    .await;

    let entries = entries_now(&core);
    // The aborted entry SAYS why it ended — and that the run is resuming.
    let aborted = entries
        .iter()
        .find(|e| e.status == Some(MessageStatus::Aborted))
        .expect("crashed entry stays, stamped aborted");
    assert!(
        aborted.parts.iter().any(|p| matches!(
            p,
            MessagePart::Error { message, .. }
                if message.contains("engine restart") && message.contains("resuming")
        )),
        "aborted entry carries the visible interruption note"
    );
    // The interrupted request is continued, not replayed: the user's own
    // message stays the one they sent, and the recovery notice that follows
    // it is a new message of its own.
    let users: Vec<_> = entries
        .iter()
        .filter(|e| e.role == MessageRole::User)
        .collect();
    assert_eq!(users.len(), 2);
    assert_eq!(users[0].id, "msg-user-1");
    assert_ne!(users[1].id, "msg-user-1");
    // The revived run continues the journal-recovered harness conversation.
    // (An auto-title request may precede it — titling fires at dispatch.)
    let recorded = requests.lock().unwrap().clone();
    assert!(
        recorded.iter().all(|r| r.prompt != "long task"),
        "the interrupted request is never run again as if it were new"
    );
    let revived = recorded
        .iter()
        .find(|r| is_recovery_of(&r.prompt, "long task"))
        .expect("auto-resumed dispatch reached the harness");
    assert_eq!(
        revived.resume.as_deref(),
        Some("hs-crash"),
        "auto-resume must reattach the crashed harness session"
    );
    core.shutdown().await;
}

/// A steerable harness whose turn never ends: it logs the opening prompt,
/// then every prompt steered in behind it.
struct SteerRecorder {
    log: Arc<Mutex<Vec<String>>>,
}

#[async_trait]
impl Harness for SteerRecorder {
    fn id(&self) -> HarnessId {
        HarnessId::Mock
    }
    fn display_name(&self) -> &str {
        "SteerRecorder"
    }
    fn supports_steering(&self) -> bool {
        true
    }
    fn steering_mode(&self) -> SteeringMode {
        SteeringMode::StepBoundary
    }
    fn reasoning_levels(&self) -> &[ReasoningLevel] {
        &[ReasoningLevel::Medium]
    }
    async fn models(&self) -> Result<Vec<Model>, HarnessError> {
        Ok(vec![])
    }
    async fn run(
        &self,
        request: RunRequest,
        controls: RunControls,
    ) -> Result<BoxStream<'static, Result<AgentEvent, HarnessError>>, HarnessError> {
        self.log.lock().unwrap().push(request.prompt.clone());
        let log = self.log.clone();
        let started = AgentEvent::SessionStarted {
            harness: HarnessId::Mock,
            model: "mock-1".into(),
            tools: vec![],
            cwd: request.cwd.clone(),
            session_id: "hs-steer".into(),
            assistant_message_id: "a-steer".into(),
        };
        let steered = futures::stream::unfold(controls.steering, move |mut steering| {
            let log = log.clone();
            async move {
                let message = steering.recv().await?;
                log.lock().unwrap().push(message.prompt);
                let steered = AgentEvent::Steered {
                    assistant_message_id: None,
                    next_assistant_message_id: Some(uuid::Uuid::new_v4().to_string()),
                };
                Some((Ok::<_, HarnessError>(steered), steering))
            }
        });
        Ok(futures::stream::iter(vec![Ok(started)])
            .chain(steered)
            .boxed())
    }
}

fn user_entry(id: &str, text: &str, created_at: i64) -> SessionMessageEntry {
    SessionMessageEntry {
        id: id.into(),
        role: MessageRole::User,
        parts: vec![MessagePart::Text {
            id: "t0".into(),
            text: text.into(),
        }],
        created_at,
        device_id: "dev-crash".into(),
        status: Some(MessageStatus::Complete),
        continuation_of: None,
        duration_ms: None,
    }
}

/// A burst of steers died with the run: the model took up the first message
/// (Pi stamped it), the next two are in the transcript but were never
/// consumed. Recovery sends each of them again, in order, and the consumed
/// one never.
#[tokio::test]
async fn a_crash_redispatches_every_unconsumed_steered_message_in_order() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("data");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("device-id"), "dev-crash").unwrap();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    {
        let store = DocsStore::open(dir.join("orgs/dev-org/dev-user")).unwrap();
        let doc = SessionDoc::init(CHAT).unwrap();
        doc.push_message(&user_entry("msg-user-1", "first", now - 60_000))
            .unwrap();
        doc.push_message(&SessionMessageEntry {
            id: "msg-assistant-1".into(),
            role: MessageRole::Assistant,
            parts: vec![MessagePart::Text {
                id: "t0".into(),
                text: "partial…".into(),
            }],
            created_at: now - 50_000,
            device_id: "dev-crash".into(),
            status: Some(MessageStatus::Streaming),
            continuation_of: None,
            duration_ms: None,
        })
        .unwrap();
        doc.push_message(&user_entry("msg-user-2", "second", now - 40_000))
            .unwrap();
        doc.push_message(&user_entry("msg-user-3", "third", now - 30_000))
            .unwrap();
        doc.set_harness_ref("msg-user-1", "ts:1").unwrap();
        store
            .save_snapshot(CHAT, &doc.export_snapshot().unwrap())
            .unwrap();

        let journal = RunJournal::open(dir.join("orgs/dev-org/dev-user/journals")).unwrap();
        journal
            .append(
                CHAT,
                &AgentEvent::SessionStarted {
                    harness: HarnessId::Mock,
                    model: "mock-1".into(),
                    tools: vec![],
                    cwd: "/tmp".into(),
                    session_id: "hs-crash".into(),
                    assistant_message_id: "msg-assistant-1".into(),
                },
            )
            .unwrap();
    }

    let log = Arc::new(Mutex::new(Vec::new()));
    let registry = HarnessRegistry::new();
    registry.register(Arc::new(SteerRecorder { log: log.clone() }));
    let core = EngineCore::assemble(&dir, Arc::new(registry), HarnessId::Mock, None)
        .expect("engine core assembles");

    // An auto-title request may also reach the harness; only the user
    // messages matter here. The revived turn starts from the recovery notice
    // quoting the first unconsumed message, never that message as if new.
    let sent = |log: &Arc<Mutex<Vec<String>>>| -> Vec<String> {
        log.lock()
            .unwrap()
            .iter()
            .filter_map(|p| match p.as_str() {
                "first" | "second" | "third" => Some(p.clone()),
                _ => ["first", "second", "third"]
                    .into_iter()
                    .find(|request| is_recovery_of(p, request))
                    .map(|request| format!("recovery of {request}")),
            })
            .collect()
    };
    wait_for(
        || sent(&log).len() >= 2,
        "both unconsumed messages to reach the harness",
    )
    .await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        sent(&log),
        ["recovery of second", "third"],
        "the first restarts the turn, the second steers in behind it"
    );
    let ids: Vec<String> = entries_now(&core)
        .into_iter()
        .filter(|e| e.role == MessageRole::User)
        .map(|e| e.id)
        .collect();
    assert_eq!(
        ids[..3],
        ["msg-user-1", "msg-user-2", "msg-user-3"],
        "re-sending reuses the transcript entries"
    );
    assert_eq!(ids.len(), 4, "the recovery notice is the one new message");
    core.shutdown().await;
}

/// A kill moments into a turn: the journal shows the run, but the doc's
/// debounced save never stored a reply entry, so the user's message is the
/// transcript's last word. That is a crashed turn too — it is picked back
/// up, not left unanswered.
#[tokio::test]
async fn a_crash_before_the_reply_reached_disk_still_auto_resumes() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("data");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("device-id"), "dev-crash").unwrap();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    {
        let store = DocsStore::open(dir.join("orgs/dev-org/dev-user")).unwrap();
        let doc = SessionDoc::init(CHAT).unwrap();
        doc.push_message(&SessionMessageEntry {
            id: "msg-user-1".into(),
            role: MessageRole::User,
            parts: vec![MessagePart::Text {
                id: "t0".into(),
                text: "long task".into(),
            }],
            created_at: now - 5_000,
            device_id: "dev-crash".into(),
            status: Some(MessageStatus::Complete),
            continuation_of: None,
            duration_ms: None,
        })
        .unwrap();
        store
            .save_snapshot(CHAT, &doc.export_snapshot().unwrap())
            .unwrap();
        let journal = RunJournal::open(dir.join("orgs/dev-org/dev-user/journals")).unwrap();
        journal
            .append(
                CHAT,
                &AgentEvent::SessionStarted {
                    harness: HarnessId::Mock,
                    model: "mock-1".into(),
                    tools: vec![],
                    cwd: "/tmp".into(),
                    session_id: "hs-crash".into(),
                    assistant_message_id: "msg-assistant-1".into(),
                },
            )
            .unwrap();
    }

    let requests: RequestLog = Arc::new(Mutex::new(Vec::new()));
    let core = assemble(
        &dir,
        RecordingHarness {
            requests: requests.clone(),
            session_id: "hs-after-crash".into(),
            fail_starts: Default::default(),
        },
    );
    wait_for(
        || complete_assistant_count(&core) == 1,
        "the crashed turn to be picked back up",
    )
    .await;
    let recorded = requests.lock().unwrap().clone();
    let revived = recorded
        .iter()
        .find(|r| is_recovery_of(&r.prompt, "long task"))
        .expect("auto-resumed dispatch reached the harness");
    assert_eq!(revived.resume.as_deref(), Some("hs-crash"));
    core.shutdown().await;
}

/// The agent reports where it runs with symlinks resolved; the chat names
/// its folder through the symlink. A crash mid-turn leaves the resolved
/// spelling stored (only a turn's Done restores the request's), and the
/// recovered run must still resume that same conversation — not start fresh
/// with its history gone (live, Claude Code on macOS's `/var` → `/private/var`).
#[cfg(unix)]
#[tokio::test]
async fn a_crash_recovers_the_conversation_through_a_symlinked_folder() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("data");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("device-id"), "dev-crash").unwrap();
    let real = tmp.path().join("real-project");
    std::fs::create_dir_all(&real).unwrap();
    let link = tmp.path().join("linked-project");
    std::os::unix::fs::symlink(&real, &link).unwrap();
    let real = std::fs::canonicalize(&real).unwrap();
    let (real, link) = (real.to_str().unwrap(), link.to_str().unwrap());

    let requests: RequestLog = Arc::new(Mutex::new(Vec::new()));
    {
        let core = assemble(
            &dir,
            RecordingHarness {
                requests: requests.clone(),
                session_id: "unused".into(),
                fail_starts: Default::default(),
            },
        );
        core.workspace
            .create_space("space-link", &core.device_id, link, None, false)
            .unwrap();
        core.workspace
            .create_chat(CHAT, Some("space-link"), None, None, None)
            .unwrap();
        core.shutdown().await;
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    {
        let store = DocsStore::open(dir.join("orgs/dev-org/dev-user")).unwrap();
        let doc = SessionDoc::init(CHAT).unwrap();
        doc.push_message(&SessionMessageEntry {
            id: "msg-user-1".into(),
            role: MessageRole::User,
            parts: vec![MessagePart::Text {
                id: "t0".into(),
                text: "long task".into(),
            }],
            created_at: now - 5_000,
            device_id: "dev-crash".into(),
            status: Some(MessageStatus::Complete),
            continuation_of: None,
            duration_ms: None,
        })
        .unwrap();
        store
            .save_snapshot(CHAT, &doc.export_snapshot().unwrap())
            .unwrap();
        let journal = RunJournal::open(dir.join("orgs/dev-org/dev-user/journals")).unwrap();
        journal
            .append(
                CHAT,
                &AgentEvent::SessionStarted {
                    harness: HarnessId::Mock,
                    model: "mock-1".into(),
                    tools: vec![],
                    cwd: real.into(),
                    session_id: "hs-crash".into(),
                    assistant_message_id: "msg-assistant-1".into(),
                },
            )
            .unwrap();
    }

    let core = assemble(
        &dir,
        RecordingHarness {
            requests: requests.clone(),
            session_id: "hs-crash".into(),
            fail_starts: Default::default(),
        },
    );
    wait_for(
        || {
            requests
                .lock()
                .unwrap()
                .iter()
                .any(|r| is_recovery_of(&r.prompt, "long task"))
        },
        "the crashed turn to be picked back up",
    )
    .await;
    let revived = requests
        .lock()
        .unwrap()
        .iter()
        .find(|r| is_recovery_of(&r.prompt, "long task"))
        .cloned()
        .unwrap();
    assert_eq!(revived.cwd, link, "the chat's own folder");
    assert_eq!(
        revived.resume.as_deref(),
        Some("hs-crash"),
        "the same conversation, through the symlink"
    );
    core.shutdown().await;
}

#[tokio::test]
async fn resume_is_cwd_scoped() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("data");
    let requests: RequestLog = Arc::new(Mutex::new(Vec::new()));

    run_one_turn_and_shutdown(&dir, &requests, "hs-cwd-1").await;

    let core = assemble(
        &dir,
        RecordingHarness {
            requests: requests.clone(),
            session_id: "hs-cwd-2".into(),
            fail_starts: Default::default(),
        },
    );
    // Same chat, different launch directory: claude session stores are keyed
    // by cwd, so the stored id must NOT be injected.
    queue_run(
        &core,
        "now from another project",
        "/elsewhere",
        "msg-user-2",
    );
    wait_for(
        || complete_assistant_count(&core) == 2,
        "cross-cwd turn to complete",
    )
    .await;
    assert_eq!(
        requests.lock().unwrap()[1].resume,
        None,
        "a session created under /tmp must not resume from /elsewhere"
    );
    core.shutdown().await;
}

#[tokio::test]
async fn startup_crash_retries_once_with_resume_kept() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("data");
    let requests: RequestLog = Arc::new(Mutex::new(Vec::new()));

    run_one_turn_and_shutdown(&dir, &requests, "hs-live").await;

    // Relaunch with a harness whose child dies at startup ONCE (a transient
    // spawn blip). Since the ACP conversion a stale id falls back inside the
    // harness (`session/load` → `session/new`), so a startup death never
    // indicts the stored id: the retry must carry the SAME session id, not
    // start fresh — and never tombstone it.
    let core = assemble(
        &dir,
        RecordingHarness {
            requests: requests.clone(),
            session_id: "hs-next".into(),
            fail_starts: Arc::new(Mutex::new(1)),
        },
    );
    queue_run(&core, "second turn", "/tmp", "msg-user-2");
    wait_for(
        || complete_assistant_count(&core) == 2,
        "retried turn to complete",
    )
    .await;

    // The crashed attempt, then exactly one retry — resume kept both times.
    {
        let log = requests.lock().unwrap();
        assert_eq!(log.len(), 3, "one crashed attempt + one retry");
        assert_eq!(log[1].resume.as_deref(), Some("hs-live"));
        assert_eq!(
            log[2].resume.as_deref(),
            Some("hs-live"),
            "the retry must keep the stored conversation"
        );
        assert_eq!(log[2].prompt, "second turn");
    }
    // The retry reused the same user entry — no duplicates, no error turn.
    let entries = entries_now(&core);
    let users: Vec<_> = entries
        .iter()
        .filter(|e| e.role == MessageRole::User)
        .collect();
    assert_eq!(users.len(), 2, "retry must not duplicate the user entry");
    assert_eq!(entries.len(), 4, "user+assistant per turn: {entries:#?}");
    // The successful turn's session id replaces the stored one as usual.
    assert_eq!(
        stored_harness_session(&core),
        Some(("hs-next".into(), Some("/tmp".into())))
    );
    core.shutdown().await;
}

#[tokio::test]
async fn persistent_startup_crash_keeps_stored_session_id() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("data");
    let requests: RequestLog = Arc::new(Mutex::new(Vec::new()));

    run_one_turn_and_shutdown(&dir, &requests, "hs-live").await;

    // A helper that is down hard: every spawn dies at startup. The old guess
    // logic read this as "bad resume id" and tombstoned a perfectly good
    // session — permanently, surviving restarts (user incident 2026-08-13).
    let core = assemble(
        &dir,
        RecordingHarness {
            requests: requests.clone(),
            session_id: "hs-unused".into(),
            fail_starts: Arc::new(Mutex::new(u32::MAX)),
        },
    );
    queue_run(&core, "second turn", "/tmp", "msg-user-2");
    // Crashed attempt + its single retry — then it must STOP (no spawn loop).
    wait_for(
        || requests.lock().unwrap().len() == 3,
        "crashed attempt and retry to be recorded",
    )
    .await;
    tokio::time::sleep(Duration::from_millis(150)).await;
    {
        let log = requests.lock().unwrap();
        assert_eq!(log.len(), 3, "exactly one retry, no revival loop");
        assert_eq!(log[1].resume.as_deref(), Some("hs-live"));
        assert_eq!(log[2].resume.as_deref(), Some("hs-live"));
    }
    // THE fix: the stored id survives the startup failures — the next send
    // (say, after the restart that heals the helper) resumes the same
    // conversation.
    assert_eq!(
        stored_harness_session(&core),
        Some(("hs-live".into(), Some("/tmp".into())))
    );
    core.shutdown().await;
}

/// Real-CLI proof of the whole regression fix: tell claude a codeword, restart
/// the engine (fresh `EngineCore::assemble` over the same data dir), ask for
/// the codeword back — the reply can only contain it if the second run resumed
/// the first run's harness session. Ignored by default: needs an installed,
/// authenticated `claude` CLI and spends real tokens (haiku, two tiny turns).
/// Run with: `cargo test -p zeron-engine --test restart_resume -- --ignored`
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires installed+authenticated claude CLI; spends tokens"]
async fn real_claude_remembers_codeword_across_engine_restart() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("data");
    let cwd = tmp.path().join("project");
    std::fs::create_dir_all(&cwd).unwrap();
    let cwd = cwd.to_string_lossy().to_string();

    let real_request = |prompt: &str| RunRequest {
        mcp: None,
        prompt: prompt.into(),
        harness: None,
        model: Some("haiku".into()),
        reasoning: None,
        model_options: Default::default(),
        cwd: cwd.clone(),
        sandbox: SandboxLevel::WorkspaceWrite,
        auto_approve: false,
        attachments: Vec::new(),
        worktree: None,
        resume: None,
    };
    let assemble_real = || {
        EngineCore::assemble(
            &dir,
            Arc::new({
            let registry = zeron_engine::default_registry();
            zeron_engine::register_other_harnesses(&registry);
            registry
        }),
            HarnessId::ClaudeCode,
            None,
        )
        .expect("engine core assembles")
    };

    let core = assemble_real();
    pre_title(&core); // keep the auto-titler from spending a second model call
    core.doc_host
        .queue_command(
            CHAT,
            SessionCommandPayload::Run {
                request: real_request(
                    "Remember the codeword: PINEAPPLE. Reply with exactly: stored",
                ),
                message_id: "msg-user-1".into(),
            },
        )
        .expect("queue first real run");
    wait_for_within(
        || complete_assistant_count(&core) == 1,
        "first real claude turn",
        Duration::from_secs(120),
    )
    .await;
    assert!(
        stored_harness_session(&core).is_some(),
        "claude session id must be stored on the chat row"
    );
    core.shutdown().await;
    drop(core);

    // "App restart": a brand-new engine over the same data dir.
    let core = assemble_real();
    core.doc_host
        .queue_command(
            CHAT,
            SessionCommandPayload::Run {
                request: real_request(
                    "What was the codeword I told you earlier? Reply with just the codeword.",
                ),
                message_id: "msg-user-2".into(),
            },
        )
        .expect("queue second real run");
    wait_for_within(
        || complete_assistant_count(&core) == 2,
        "post-restart real claude turn",
        Duration::from_secs(120),
    )
    .await;

    let entries = entries_now(&core);
    let last_assistant_text: String = entries
        .iter()
        .rev()
        .find(|e| e.role == MessageRole::Assistant)
        .into_iter()
        .flat_map(|e| {
            e.parts.iter().filter_map(|p| match p {
                MessagePart::Text { text, .. } => Some(text.clone()),
                _ => None,
            })
        })
        .collect();
    assert!(
        last_assistant_text.to_uppercase().contains("PINEAPPLE"),
        "post-restart reply must recall the codeword (got: {last_assistant_text:?})"
    );
    core.shutdown().await;
}

#[tokio::test]
async fn steer_after_restart_dispatches_new_turn_with_resume() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("data");
    let requests: RequestLog = Arc::new(Mutex::new(Vec::new()));

    run_one_turn_and_shutdown(&dir, &requests, "hs-steer").await;

    // Relaunch: no live run, no in-process `last_request`. A steer must fall
    // back to a new turn built from the chat's workspace row, resuming the
    // prior harness conversation.
    let core = assemble(
        &dir,
        RecordingHarness {
            requests: requests.clone(),
            session_id: "hs-steer-2".into(),
            fail_starts: Default::default(),
        },
    );
    core.doc_host
        .queue_command(
            CHAT,
            SessionCommandPayload::Steer {
                prompt: "actually, also add tests".into(),
                message_id: Some("msg-user-2".into()),
            },
        )
        .expect("queue steer command");
    wait_for(
        || complete_assistant_count(&core) == 2,
        "steer-as-new-turn to complete",
    )
    .await;

    {
        let log = requests.lock().unwrap();
        assert_eq!(log.len(), 2);
        assert_eq!(log[1].prompt, "actually, also add tests");
        assert_eq!(log[1].cwd, "/tmp", "run config rebuilt from the chat row");
        assert_eq!(
            log[1].resume.as_deref(),
            Some("hs-steer"),
            "steer-turned-run must resume the stored harness session"
        );
    }
    core.shutdown().await;
}

const TREE_ROOT: &str = "orchestrator-recovery";
const TREE_MEMBER: &str = "member-recovery";

/// A root and one member below it, as an orchestrator leaves them.
fn create_tree(core: &EngineCore, paused: bool) {
    for (id, parent) in [(TREE_ROOT, None), (TREE_MEMBER, Some(TREE_ROOT))] {
        core.workspace
            .create_chat_with_parent(
                id,
                None,
                Some(&core.device_id),
                None,
                Some("/tmp".into()),
                parent.map(str::to_owned),
            )
            .expect("create chat row");
        core.workspace.rename_chat(id, id).expect("rename chat");
    }
    if paused {
        core.workspace
            .update_orchestrator_project(
                TREE_ROOT,
                zeron_proto::orchestrator_project::ProjectUpdate {
                    paused: Some(true),
                    ..Default::default()
                },
            )
            .expect("pause the tree");
    }
}

/// What a kill -9 a minute into a turn leaves behind for `chat`: the user's
/// message and a streaming reply in the doc, a journal that never reached Done.
fn manufacture_fresh_crash(dir: &std::path::Path, chat: &str) {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    let store = DocsStore::open(dir.join("orgs/dev-org/dev-user")).unwrap();
    let doc = SessionDoc::init(chat).unwrap();
    for (id, role, text, at, status) in [
        (
            "msg-user-1",
            MessageRole::User,
            "long task",
            now - 60_000,
            MessageStatus::Complete,
        ),
        (
            "msg-assistant-1",
            MessageRole::Assistant,
            "partial",
            now - 30_000,
            MessageStatus::Streaming,
        ),
    ] {
        doc.push_message(&SessionMessageEntry {
            id: id.into(),
            role,
            parts: vec![MessagePart::Text {
                id: "t0".into(),
                text: text.into(),
            }],
            created_at: at,
            device_id: "dev-crash".into(),
            status: Some(status),
            continuation_of: None,
            duration_ms: None,
        })
        .unwrap();
    }
    store
        .save_snapshot(chat, &doc.export_snapshot().unwrap())
        .unwrap();
    let journal = RunJournal::open(dir.join("orgs/dev-org/dev-user/journals")).unwrap();
    journal
        .append(
            chat,
            &AgentEvent::SessionStarted {
                harness: HarnessId::Mock,
                model: "mock-1".into(),
                tools: vec![],
                cwd: "/tmp".into(),
                session_id: "hs-crash".into(),
                assistant_message_id: "msg-assistant-1".into(),
            },
        )
        .unwrap();
    journal
        .append(
            chat,
            &AgentEvent::TextDelta {
                text: "partial".into(),
            },
        )
        .unwrap();
}

fn recovery_runs(requests: &RequestLog) -> usize {
    requests
        .lock()
        .unwrap()
        .iter()
        .filter(|r| {
            r.prompt
                .starts_with("[Message from Zeron (automatic recovery notice)")
        })
        .count()
}

fn member_entries(core: &EngineCore) -> Vec<SessionMessageEntry> {
    core.doc_host
        .open(TREE_MEMBER)
        .ok()
        .and_then(|h| h.doc().read_entries().ok())
        .unwrap_or_default()
}

/// A tree its owner paused or stopped stays down after a crash: the revival
/// would start work nobody wants running.
#[tokio::test]
async fn a_paused_tree_member_is_not_revived_on_boot() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("data");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("device-id"), "dev-crash").unwrap();
    let requests: RequestLog = Arc::new(Mutex::new(Vec::new()));
    let harness = || RecordingHarness {
        requests: requests.clone(),
        session_id: "hs-after-crash".into(),
        fail_starts: Default::default(),
    };
    {
        let core = assemble(&dir, harness());
        create_tree(&core, true);
        core.shutdown().await;
    }
    manufacture_fresh_crash(&dir, TREE_MEMBER);

    let core = assemble(&dir, harness());
    // Give a (wrongly) spawned revival time to reach the harness.
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert_eq!(
        recovery_runs(&requests),
        0,
        "{:?}",
        requests.lock().unwrap()
    );
    let entries = member_entries(&core);
    let aborted = entries
        .iter()
        .find(|e| e.status == Some(MessageStatus::Aborted))
        .expect("the crashed reply is stamped aborted");
    assert!(
        aborted.parts.iter().any(|p| matches!(
            p,
            MessagePart::Error { message, .. }
                if message.contains("not resumed") && message.contains("paused or stopped")
        )),
        "the note says why it stayed down: {aborted:?}"
    );
    assert_eq!(
        core.sessions.session_status(TREE_MEMBER).map(|s| s.status),
        Some(zeron_proto::SessionStatus::Idle),
        "held on purpose, not failed"
    );
    core.shutdown().await;
}

/// A member whose turn cannot be continued failed, as far as the orchestrator
/// above it is concerned: it must hear so rather than wait for silence.
#[tokio::test]
async fn a_member_that_cannot_be_resumed_tells_its_parent() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("data");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("device-id"), "dev-crash").unwrap();
    let requests: RequestLog = Arc::new(Mutex::new(Vec::new()));
    let harness = || RecordingHarness {
        requests: requests.clone(),
        session_id: "hs-after-crash".into(),
        fail_starts: Default::default(),
    };
    {
        let core = assemble(&dir, harness());
        create_tree(&core, false);
        core.shutdown().await;
    }
    manufacture_fresh_crash(&dir, TREE_MEMBER);
    // Recovery has already revived it three times in a row.
    let journal = RunJournal::open(dir.join("orgs/dev-org/dev-user/journals")).unwrap();
    for _ in 0..3 {
        journal.note_resume_attempt(TREE_MEMBER);
    }
    drop(journal);

    let core = assemble(&dir, harness());
    wait_for(
        || {
            requests.lock().unwrap().iter().any(|r| {
                r.prompt
                    .starts_with("[Message from Zeron (automatic child-outcome notice)")
            })
        },
        "the orchestrator to be told",
    )
    .await;
    let note = requests
        .lock()
        .unwrap()
        .iter()
        .find(|r| r.prompt.contains("automatic child-outcome notice"))
        .map(|r| r.prompt.clone())
        .unwrap();
    assert!(note.contains(TREE_MEMBER), "{note}");
    assert!(note.contains("failed (process lost)"), "{note}");
    assert!(note.contains("engine stopped unexpectedly"), "{note}");
    assert_eq!(recovery_runs(&requests), 0, "not resumed");
    assert_eq!(
        core.sessions.session_status(TREE_MEMBER).map(|s| s.status),
        Some(zeron_proto::SessionStatus::Errored)
    );
    core.shutdown().await;
}
