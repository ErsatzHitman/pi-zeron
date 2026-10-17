//! Capacity control: the global running-turn cap (queued starts, visible in
//! the session row) and the idle TTL of Pi tree members' runtimes. The
//! knobs are process-env, so every test takes `ENV` and sets what it needs.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use futures::stream::BoxStream;
use tokio::sync::mpsc;

use zeron_doc::MessageRole;
use zeron_engine::{EngineCore, HarnessRegistry};
use zeron_harness::{Harness, HarnessError, RunControls, TurnControl};
use zeron_proto::{
    AgentEvent, DoneStatus, HarnessId, Model, ReasoningLevel, RunRequest, SandboxLevel,
    SessionStatus, SteeringMode,
};

static ENV: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn set_env(vars: &[(&str, &str)]) {
    for key in [
        "ZERON_MAX_RUNNING_TURNS",
        "ZERON_MEMBER_IDLE_MS",
        "ZERON_SESSION_IDLE_MS",
    ] {
        unsafe { std::env::remove_var(key) };
    }
    for (key, value) in vars {
        unsafe { std::env::set_var(key, value) };
    }
}

#[derive(Default)]
struct Ledger {
    spawns: AtomicUsize,
    kills: AtomicUsize,
    /// Prompt → the `resume` id the spawn carried.
    resumes: Mutex<Vec<(String, Option<String>)>>,
    /// Prompt → that runtime's turn control (the test flips background work).
    turns: Mutex<HashMap<String, TurnControl>>,
    /// Ends a turn whose prompt contains `wait`.
    release: tokio::sync::Notify,
}

/// One persistent agent process per `run()`. A prompt containing `wait` stays
/// in flight until the test releases it; any other prompt completes at once.
/// `pi` makes it a Pi-like runtime (id Pi, kept alive by the app).
struct ProcessHarness {
    ledger: Arc<Ledger>,
    pi: bool,
}

fn done(status: DoneStatus) -> AgentEvent {
    AgentEvent::Done {
        status,
        result: None,
        error: None,
        session_id: Some("provider-session".into()),
    }
}

#[async_trait]
impl Harness for ProcessHarness {
    fn id(&self) -> HarnessId {
        if self.pi {
            HarnessId::Pi
        } else {
            HarnessId::Mock
        }
    }
    fn display_name(&self) -> &str {
        "Process"
    }
    fn supports_steering(&self) -> bool {
        true
    }
    fn steering_mode(&self) -> SteeringMode {
        SteeringMode::StepBoundary
    }
    fn reasoning_levels(&self) -> &[ReasoningLevel] {
        &[ReasoningLevel::Medium, ReasoningLevel::High]
    }
    fn deterministic_turn_end(&self) -> bool {
        true
    }
    fn stops_turn_in_place(&self) -> bool {
        true
    }
    fn keeps_runtime_alive(&self) -> bool {
        self.pi
    }
    async fn models(&self) -> Result<Vec<Model>, HarnessError> {
        Ok(vec![])
    }
    async fn run(
        &self,
        request: RunRequest,
        mut controls: RunControls,
    ) -> Result<BoxStream<'static, Result<AgentEvent, HarnessError>>, HarnessError> {
        let ledger = self.ledger.clone();
        let id = self.id();
        ledger.spawns.fetch_add(1, Ordering::SeqCst);
        ledger
            .resumes
            .lock()
            .unwrap()
            .push((request.prompt.clone(), request.resume.clone()));
        ledger
            .turns
            .lock()
            .unwrap()
            .insert(request.prompt.clone(), controls.turn.clone());
        let (tx, rx) = mpsc::channel::<Result<AgentEvent, HarnessError>>(64);
        tokio::spawn(async move {
            let turn = controls.turn.clone();
            let send = |event: AgentEvent| {
                let tx = tx.clone();
                async move { tx.send(Ok(event)).await.is_ok() }
            };
            let _ = send(AgentEvent::SessionStarted {
                harness: id,
                model: "process".into(),
                tools: vec![],
                cwd: request.cwd.clone(),
                session_id: "provider-session".into(),
                assistant_message_id: "a0".into(),
            })
            .await;
            let _ = send(AgentEvent::TextDelta {
                text: format!("on it: {}", request.prompt),
            })
            .await;
            let mut waiting = request.prompt.contains("wait");
            if !waiting && !send(done(DoneStatus::Completed)).await {
                return;
            }
            loop {
                tokio::select! {
                    biased;
                    _ = ledger.release.notified(), if waiting => {
                        waiting = false;
                        if !send(done(DoneStatus::Completed)).await {
                            return;
                        }
                    }
                    _ = controls.interrupt.cancelled() => {
                        ledger.kills.fetch_add(1, Ordering::SeqCst);
                        let _ = send(done(DoneStatus::Interrupted)).await;
                        return;
                    }
                    _ = turn.stop_requested() => {
                        if !send(done(DoneStatus::Interrupted)).await {
                            return;
                        }
                    }
                    steer = controls.steering.recv() => {
                        let Some(steer) = steer else { return };
                        let boundary = AgentEvent::Steered {
                            assistant_message_id: None,
                            next_assistant_message_id: None,
                        };
                        if !send(boundary).await
                            || !send(AgentEvent::TextDelta { text: format!("on it: {}", steer.prompt) }).await
                        {
                            return;
                        }
                        waiting = steer.prompt.contains("wait");
                        if !waiting && !send(done(DoneStatus::Completed)).await {
                            return;
                        }
                    }
                }
            }
        });
        Ok(
            futures::stream::unfold(rx, |mut rx| async move { rx.recv().await.map(|e| (e, rx)) })
                .boxed(),
        )
    }
}

struct Rig {
    core: EngineCore,
    ledger: Arc<Ledger>,
    harness: HarnessId,
    _dir: tempfile::TempDir,
}

fn rig(pi: bool) -> Rig {
    let ledger = Arc::new(Ledger::default());
    let registry = HarnessRegistry::new();
    registry.register(Arc::new(ProcessHarness {
        ledger: ledger.clone(),
        pi,
    }));
    let harness = if pi { HarnessId::Pi } else { HarnessId::Mock };
    let dir = tempfile::tempdir().unwrap();
    let core = EngineCore::assemble(dir.path(), Arc::new(registry), harness, None)
        .expect("engine core assembles");
    Rig {
        core,
        ledger,
        harness,
        _dir: dir,
    }
}

fn request(harness: HarnessId, prompt: &str) -> RunRequest {
    RunRequest {
        mcp: None,
        prompt: prompt.into(),
        harness: Some(harness),
        model: Some("process-1".into()),
        reasoning: Some(ReasoningLevel::High),
        model_options: serde_json::Map::new(),
        cwd: "/tmp".into(),
        sandbox: SandboxLevel::WorkspaceWrite,
        auto_approve: false,
        attachments: Vec::new(),
        worktree: None,
        resume: None,
    }
}

impl Rig {
    async fn send(&self, chat: &str, prompt: &str) {
        self.core
            .sessions
            .dispatch(chat, self.harness, request(self.harness, prompt), None)
            .await
            .unwrap();
    }

    fn status(&self, chat: &str) -> Option<SessionStatus> {
        self.core.sessions.session_status(chat).map(|s| s.status)
    }

    fn queue_position(&self, chat: &str) -> u32 {
        self.core
            .sessions
            .session_status(chat)
            .map_or(0, |s| s.queue_position)
    }

    fn spawns(&self) -> usize {
        self.ledger.spawns.load(Ordering::SeqCst)
    }

    fn assistant_entries(&self, chat: &str) -> usize {
        self.core
            .doc_host
            .open(chat)
            .unwrap()
            .doc()
            .read_entries()
            .unwrap()
            .iter()
            .filter(|entry| entry.role == MessageRole::Assistant)
            .count()
    }

    fn create_chat(&self, id: &str, parent: Option<&str>) {
        self.core
            .workspace
            .create_chat_with_parent(
                id,
                None,
                Some(&self.core.device_id),
                None,
                Some("/tmp".into()),
                parent.map(str::to_owned),
            )
            .unwrap();
    }
}

async fn wait_for(mut predicate: impl FnMut() -> bool, what: &str) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while !predicate() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for {what}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test]
async fn running_turn_cap_queues_extra_starts_in_order_and_stop_withdraws_them() {
    let _env = ENV.lock().await;
    set_env(&[("ZERON_MAX_RUNNING_TURNS", "1")]);
    let rig = rig(false);

    rig.send("chat-a", "wait a").await;
    wait_for(|| rig.spawns() == 1, "chat-a to spawn").await;
    rig.send("chat-b", "wait b").await;
    rig.send("chat-c", "wait c").await;
    rig.send("chat-d", "wait d").await;

    // The cap holds the others back: no process, a visible place in line,
    // and still `Working` so every status reader keeps waiting on them.
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(rig.spawns(), 1);
    for (chat, place) in [("chat-b", 1), ("chat-c", 2), ("chat-d", 3)] {
        assert_eq!(rig.status(chat), Some(SessionStatus::Working), "{chat}");
        assert_eq!(rig.queue_position(chat), place, "{chat}");
        assert_eq!(rig.assistant_entries(chat), 0, "{chat} has not started");
    }
    assert_eq!(rig.queue_position("chat-a"), 0);

    // Stopping a queued chat withdraws it: it never spawns and the chats
    // behind it move up.
    assert!(rig.core.sessions.interrupt("chat-c").await.unwrap());
    wait_for(
        || rig.status("chat-c") == Some(SessionStatus::Idle),
        "chat-c to settle",
    )
    .await;
    assert_eq!(rig.queue_position("chat-c"), 0);
    assert_eq!(rig.queue_position("chat-b"), 1);
    assert_eq!(rig.queue_position("chat-d"), 2);
    assert_eq!(rig.spawns(), 1);
    assert_eq!(rig.assistant_entries("chat-c"), 0);

    // A message into chat-a's live runtime is a steer, not a new start.
    rig.send("chat-a", "wait a again").await;
    assert_eq!(rig.queue_position("chat-a"), 0);
    assert_eq!(rig.status("chat-a"), Some(SessionStatus::Working));
    assert_eq!(rig.spawns(), 1);

    // Finishing chat-a (its steered message is a turn of its own) frees the
    // slot for the head of the line only.
    rig.ledger.release.notify_one();
    tokio::time::sleep(Duration::from_millis(100)).await;
    rig.ledger.release.notify_one();
    wait_for(|| rig.spawns() == 2, "chat-b to start").await;
    wait_for(
        || rig.queue_position("chat-b") == 0,
        "chat-b to leave the queue",
    )
    .await;
    assert_eq!(rig.status("chat-b"), Some(SessionStatus::Working));
    assert_eq!(rig.queue_position("chat-d"), 1);
    assert_eq!(rig.status("chat-a"), Some(SessionStatus::Idle));

    rig.ledger.release.notify_one();
    wait_for(|| rig.spawns() == 3, "chat-d to start").await;
    wait_for(
        || rig.queue_position("chat-d") == 0,
        "chat-d to leave the queue",
    )
    .await;
    rig.ledger.release.notify_one();
    wait_for(
        || rig.status("chat-d") == Some(SessionStatus::Idle),
        "chat-d to finish",
    )
    .await;
    assert_eq!(rig.status("chat-b"), Some(SessionStatus::Idle));
    set_env(&[]);
}

#[tokio::test]
async fn idle_ttl_stops_tree_members_only_and_the_next_message_resumes() {
    let _env = ENV.lock().await;
    set_env(&[
        ("ZERON_MAX_RUNNING_TURNS", "0"),
        ("ZERON_MEMBER_IDLE_MS", "300"),
        ("ZERON_SESSION_IDLE_MS", "600000"),
    ]);
    let rig = rig(true);
    rig.create_chat("orchestrator-t", None);
    rig.create_chat("m1", Some("orchestrator-t"));
    rig.create_chat("m2", Some("orchestrator-t"));
    rig.create_chat("plain", None);

    for chat in ["orchestrator-t", "m1", "m2", "plain"] {
        rig.send(chat, &format!("work {chat}")).await;
    }
    wait_for(|| rig.spawns() == 4, "four runtimes").await;
    wait_for(
        || {
            ["orchestrator-t", "m1", "m2", "plain"]
                .iter()
                .all(|chat| rig.status(chat) == Some(SessionStatus::Idle))
        },
        "every turn to park",
    )
    .await;
    // m2 reports background work (a subagent, a shell): it must be spared.
    rig.ledger.turns.lock().unwrap()["work m2"].set_background(1);

    wait_for(
        || rig.ledger.kills.load(Ordering::SeqCst) >= 1,
        "m1 to be reaped",
    )
    .await;
    wait_for(|| !rig.core.sessions.has_run("m1"), "m1's run to end").await;
    // Well past the TTL: only the idle member without live work went.
    tokio::time::sleep(Duration::from_millis(900)).await;
    assert_eq!(rig.ledger.kills.load(Ordering::SeqCst), 1);
    assert_eq!(rig.status("m1"), Some(SessionStatus::Idle));
    assert!(rig.core.sessions.has_run("m2"), "background work spares m2");
    assert!(
        rig.core.sessions.has_run("orchestrator-t"),
        "roots stay warm"
    );
    assert!(rig.core.sessions.has_run("plain"), "plain chats stay warm");

    // The next message starts a fresh runtime that resumes the conversation.
    rig.send("m1", "work m1 again").await;
    wait_for(|| rig.spawns() == 5, "m1 to restart").await;
    let resumes = rig.ledger.resumes.lock().unwrap().clone();
    assert_eq!(
        resumes.last().unwrap(),
        &("work m1 again".to_string(), Some("provider-session".into()))
    );
    set_env(&[]);
}

#[tokio::test]
async fn idle_ttl_zero_disables_the_member_reaper() {
    let _env = ENV.lock().await;
    set_env(&[
        ("ZERON_MAX_RUNNING_TURNS", "0"),
        ("ZERON_MEMBER_IDLE_MS", "0"),
        ("ZERON_SESSION_IDLE_MS", "100"),
    ]);
    let rig = rig(true);
    rig.create_chat("orchestrator-z", None);
    rig.create_chat("m1", Some("orchestrator-z"));
    rig.send("m1", "work m1").await;
    wait_for(
        || rig.status("m1") == Some(SessionStatus::Idle) && rig.spawns() == 1,
        "m1 to park",
    )
    .await;
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert!(rig.core.sessions.has_run("m1"));
    assert_eq!(rig.ledger.kills.load(Ordering::SeqCst), 0);
    set_env(&[]);
}
