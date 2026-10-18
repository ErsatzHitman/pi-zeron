//! Pushed child outcomes: a settled tree member tells its parent, batched,
//! unless the tree is paused or the parent is archived. A busy parent gets the
//! note through its message queue (steered in, or held), never dropped.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use futures::{StreamExt, stream::BoxStream};
use zeron_doc::SessionCommandPayload;
use zeron_engine::{EngineCore, HarnessRegistry};
use zeron_harness::{Harness, HarnessError, RunControls};
use zeron_proto::orchestrator_project::ProjectUpdate;
use zeron_proto::{
    AgentEvent, DoneStatus, HarnessId, Model, ReasoningLevel, RunRequest, SandboxLevel,
    SteeringMode, UserInputQuestion,
};

const ROOT: &str = "orchestrator-x";
const NOTICE: &str = "[Message from Zeron (automatic child-outcome notice)";

/// Records every prompt. "HANG" never finishes (a working chat; a steerable
/// one also records each steered prompt), "GATE" finishes once the test opens
/// the gate, "ASK" asks a question and waits (an awaiting-input chat);
/// anything else completes with `reply: <prompt>`.
struct Capture {
    prompts: Arc<Mutex<Vec<String>>>,
    /// Takes steered prompts mid-turn (a step-boundary harness).
    steerable: bool,
    gate: Arc<tokio::sync::Notify>,
}

#[async_trait]
impl Harness for Capture {
    fn id(&self) -> HarnessId {
        HarnessId::Mock
    }
    fn display_name(&self) -> &str {
        "Capture"
    }
    fn supports_steering(&self) -> bool {
        self.steerable
    }
    fn steering_mode(&self) -> SteeringMode {
        if self.steerable {
            SteeringMode::StepBoundary
        } else {
            SteeringMode::TurnBoundary
        }
    }
    fn reasoning_levels(&self) -> &[ReasoningLevel] {
        &[]
    }
    async fn models(&self) -> Result<Vec<Model>, HarnessError> {
        Ok(vec![])
    }
    async fn run(
        &self,
        request: RunRequest,
        controls: RunControls,
    ) -> Result<BoxStream<'static, Result<AgentEvent, HarnessError>>, HarnessError> {
        self.prompts.lock().unwrap().push(request.prompt.clone());
        let started = AgentEvent::SessionStarted {
            harness: HarnessId::Mock,
            model: "mock-1".into(),
            tools: vec![],
            cwd: request.cwd.clone(),
            session_id: format!("sess-{}", request.prompt.len()),
            assistant_message_id: "a-1".into(),
        };
        match request.prompt.as_str() {
            "HANG" if self.steerable => {
                let prompts = self.prompts.clone();
                let steered = futures::stream::unfold(controls.steering, move |mut steering| {
                    let prompts = prompts.clone();
                    async move {
                        let message = steering.recv().await?;
                        prompts.lock().unwrap().push(message.prompt);
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
            "GATE" => {
                let gate = self.gate.clone();
                let done = futures::stream::once(async move {
                    gate.notified().await;
                    Ok(AgentEvent::Done {
                        status: DoneStatus::Completed,
                        result: None,
                        error: None,
                        session_id: Some("sess".into()),
                        class: None,
                    })
                });
                Ok(futures::stream::iter(vec![Ok(started)]).chain(done).boxed())
            }
            "HANG" => Ok(futures::stream::iter(vec![Ok(started)])
                .chain(futures::stream::pending())
                .boxed()),
            "ASK" => {
                // The engine's input bridge owns the request id.
                let request_input = controls.request_input;
                tokio::spawn(async move {
                    let _ = request_input(vec![UserInputQuestion {
                        id: "q".into(),
                        header: "DB".into(),
                        question: "Which database?".into(),
                        options: vec!["pg".into(), "sqlite".into()],
                        multi_select: false,
                        prefill: None,
                        multiline: false,
                        secret: false,
                    }])
                    .await;
                });
                Ok(futures::stream::iter(vec![Ok(started)])
                    .chain(futures::stream::pending())
                    .boxed())
            }
            prompt => Ok(futures::stream::iter(vec![
                Ok(started),
                Ok(AgentEvent::TextDelta {
                    text: format!("reply: {prompt}"),
                }),
                Ok(AgentEvent::Done {
                    status: DoneStatus::Completed,
                    result: None,
                    error: None,
                    session_id: Some("sess".into()),
                    class: None,
                }),
            ])
            .boxed()),
        }
    }
}

struct Fixture {
    core: EngineCore,
    prompts: Arc<Mutex<Vec<String>>>,
    gate: Arc<tokio::sync::Notify>,
    _dir: tempfile::TempDir,
}

impl Fixture {
    /// A root that takes input at its turn boundary only.
    fn new() -> Self {
        Self::with_steering(false)
    }

    /// A root that takes steered messages mid-turn.
    fn steerable() -> Self {
        Self::with_steering(true)
    }

    fn with_steering(steerable: bool) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let prompts = Arc::new(Mutex::new(Vec::new()));
        let gate = Arc::new(tokio::sync::Notify::new());
        let registry = HarnessRegistry::new();
        registry.register(Arc::new(Capture {
            prompts: prompts.clone(),
            steerable,
            gate: gate.clone(),
        }));
        let core =
            EngineCore::assemble(dir.path(), Arc::new(registry), HarnessId::Mock, None).unwrap();
        let fixture = Self {
            core,
            prompts,
            gate,
            _dir: dir,
        };
        fixture.chat(ROOT, None, "Orchestrator");
        fixture
    }

    fn chat(&self, id: &str, parent: Option<&str>, title: &str) {
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
        self.core.workspace.rename_chat(id, title).unwrap();
    }

    async fn run(&self, chat: &str, prompt: &str) {
        self.core
            .sessions
            .dispatch(
                chat,
                HarnessId::Mock,
                RunRequest {
                    mcp: None,
                    prompt: prompt.into(),
                    harness: Some(HarnessId::Mock),
                    model: None,
                    reasoning: None,
                    model_options: Default::default(),
                    cwd: "/tmp".into(),
                    sandbox: SandboxLevel::WorkspaceWrite,
                    auto_approve: false,
                    resume: None,
                    attachments: vec![],
                    worktree: None,
                },
                Some(format!("m-{chat}-{prompt}")),
            )
            .await
            .unwrap();
    }

    fn notices(&self) -> Vec<String> {
        self.prompts
            .lock()
            .unwrap()
            .iter()
            .filter(|p| p.starts_with(NOTICE))
            .cloned()
            .collect()
    }

    async fn wait_for_notices(&self, count: usize) -> Vec<String> {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
        while self.notices().len() < count {
            assert!(
                tokio::time::Instant::now() < deadline,
                "no notice: {:?}",
                self.prompts.lock().unwrap()
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        self.notices()
    }

    fn root_queue(&self) -> Vec<zeron_doc::QueuedMessage> {
        self.core
            .doc_host
            .open(ROOT)
            .unwrap()
            .doc()
            .read_queue()
            .unwrap()
    }

    async fn wait_for_root_queue(&self, count: usize) -> Vec<zeron_doc::QueuedMessage> {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
        while self.root_queue().len() < count {
            assert!(
                tokio::time::Instant::now() < deadline,
                "no queued note: {:?}",
                self.prompts.lock().unwrap()
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        self.root_queue()
    }

    /// Texts of the user messages in the root's transcript, in order.
    fn root_user_texts(&self) -> Vec<String> {
        let entries = self
            .core
            .doc_host
            .open(ROOT)
            .unwrap()
            .doc()
            .read_entries()
            .unwrap();
        entries
            .iter()
            .filter(|e| e.role == zeron_doc::MessageRole::User)
            .map(|e| {
                e.parts
                    .iter()
                    .filter_map(|p| match p {
                        zeron_doc::MessagePart::Text { text, .. } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("")
            })
            .collect()
    }

    /// Past the 2 s batch window plus delivery.
    async fn settle(&self) {
        tokio::time::sleep(Duration::from_millis(3500)).await;
    }
}

#[tokio::test]
async fn two_children_finishing_together_wake_an_idle_root_once() {
    let f = Fixture::new();
    f.chat("child-a", Some(ROOT), "Fix parser");
    f.chat("child-b", Some(ROOT), "Write docs");
    f.run("child-a", "task a").await;
    f.run("child-b", "task b").await;
    let notices = f.wait_for_notices(1).await;
    f.settle().await;
    assert_eq!(f.notices().len(), 1, "one batched note, not two");
    let text = &notices[0];
    for want in [
        "child-a",
        "child-b",
        "Fix parser",
        "Write docs",
        "reply: task a",
        "reply: task b",
        "completed",
        "zeron_read_chat",
    ] {
        assert!(text.contains(want), "missing {want:?} in {text}");
    }
}

#[tokio::test]
async fn a_btw_aside_does_not_notify_its_parent() {
    let f = Fixture::new();
    f.chat("child-a", Some(ROOT), "Fix parser");
    f.chat("btw-aside", Some("child-a"), "Aside");
    f.chat("btw-root", Some(ROOT), "Root aside");
    f.run("btw-aside", "q").await;
    f.run("btw-root", "q").await;
    f.settle().await;
    assert!(f.notices().is_empty(), "{:?}", f.notices());
    // Nor does it start a turn on the member it sits beside.
    assert!(
        !f.prompts.lock().unwrap().iter().any(|p| p.contains("btw-aside")),
        "{:?}",
        f.prompts.lock().unwrap()
    );
}

#[tokio::test]
async fn a_paused_tree_is_not_woken() {
    let f = Fixture::new();
    f.chat("child-a", Some(ROOT), "Fix parser");
    f.core
        .workspace
        .update_orchestrator_project(
            ROOT,
            ProjectUpdate {
                paused: Some(true),
                ..Default::default()
            },
        )
        .unwrap();
    f.run("child-a", "task a").await;
    f.settle().await;
    assert!(f.notices().is_empty());
}

#[tokio::test]
async fn a_working_root_receives_the_outcome_as_one_steered_note() {
    let f = Fixture::steerable();
    f.chat("child-a", Some(ROOT), "Fix parser");
    f.run(ROOT, "HANG").await;
    f.run("child-a", "task a").await;
    let notices = f.wait_for_notices(1).await;
    f.settle().await;
    assert_eq!(f.notices().len(), 1, "one note, not a retry or a duplicate");
    assert!(notices[0].contains("child-a"), "{}", notices[0]);
    assert!(notices[0].contains("reply: task a"), "{}", notices[0]);
    assert!(f.root_queue().is_empty(), "{:?}", f.root_queue());
    let in_transcript = f
        .root_user_texts()
        .iter()
        .filter(|text| text.starts_with(NOTICE))
        .count();
    assert_eq!(in_transcript, 1, "{:?}", f.root_user_texts());
    let runs = f
        .prompts
        .lock()
        .unwrap()
        .iter()
        .filter(|p| p.as_str() == "HANG")
        .count();
    assert_eq!(
        runs, 1,
        "the note joined the running turn, not a second one"
    );
    assert!(
        f.core.sessions.turn_in_flight(ROOT),
        "the root keeps working"
    );
}

#[tokio::test]
async fn outcomes_arriving_apart_while_the_root_works_arrive_in_order_once() {
    let f = Fixture::steerable();
    f.chat("child-a", Some(ROOT), "Fix parser");
    f.chat("child-b", Some(ROOT), "Write docs");
    f.run(ROOT, "HANG").await;
    f.run("child-a", "task a").await;
    f.wait_for_notices(1).await;
    // Past the batch window: the second outcome is a note of its own.
    tokio::time::sleep(Duration::from_millis(2500)).await;
    f.run("child-b", "task b").await;
    f.wait_for_notices(2).await;
    f.settle().await;
    let notices = f.notices();
    assert_eq!(notices.len(), 2, "{notices:?}");
    assert!(notices[0].contains("child-a") && !notices[0].contains("child-b"));
    assert!(notices[1].contains("child-b") && !notices[1].contains("child-a"));
    let in_transcript = f
        .root_user_texts()
        .iter()
        .filter(|text| text.starts_with(NOTICE))
        .count();
    assert_eq!(in_transcript, 2, "{:?}", f.root_user_texts());
}

#[tokio::test]
async fn a_user_row_between_two_notes_keeps_arrival_order() {
    let f = Fixture::steerable();
    f.chat("child-a", Some(ROOT), "Fix parser");
    f.chat("child-b", Some(ROOT), "Write docs");
    f.run(ROOT, "HANG").await;
    f.run("child-a", "task a").await;
    f.wait_for_notices(1).await;
    f.core
        .doc_host
        .queue_message(ROOT, "user says hi", Vec::new())
        .unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    while !f
        .prompts
        .lock()
        .unwrap()
        .iter()
        .any(|p| p == "user says hi")
    {
        assert!(tokio::time::Instant::now() < deadline, "row never steered");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    f.run("child-b", "task b").await;
    f.wait_for_notices(2).await;
    f.settle().await;
    let seen = f.prompts.lock().unwrap().clone();
    assert_eq!(seen.len(), 4, "{seen:?}");
    assert_eq!(seen[0], "HANG");
    assert!(seen[1].starts_with(NOTICE) && seen[1].contains("child-a"));
    assert_eq!(seen[2], "user says hi");
    assert!(seen[3].starts_with(NOTICE) && seen[3].contains("child-b"));
}

#[tokio::test]
async fn a_turn_boundary_root_holds_the_note_until_the_turn_ends_then_runs_it_once() {
    let f = Fixture::new();
    f.chat("child-a", Some(ROOT), "Fix parser");
    f.run(ROOT, "GATE").await;
    f.run("child-a", "task a").await;
    let queue = f.wait_for_root_queue(1).await;
    assert!(queue[0].text.starts_with(NOTICE), "{queue:?}");
    assert!(queue[0].text.contains("child-a"));
    f.settle().await;
    assert!(f.notices().is_empty(), "the turn is still running");
    assert_eq!(f.root_queue().len(), 1, "{:?}", f.root_queue());
    f.gate.notify_one();
    let notices = f.wait_for_notices(1).await;
    f.settle().await;
    assert_eq!(f.notices().len(), 1, "exactly once: {:?}", f.notices());
    assert_eq!(notices[0], queue[0].text);
    assert!(f.root_queue().is_empty());
}

#[tokio::test]
async fn a_stopped_root_is_not_restarted_by_a_note() {
    let f = Fixture::new();
    f.chat("child-a", Some(ROOT), "Fix parser");
    f.run(ROOT, "HANG").await;
    f.core
        .doc_host
        .queue_command(ROOT, SessionCommandPayload::Interrupt {})
        .unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    while f.core.sessions.turn_in_flight(ROOT) {
        assert!(tokio::time::Instant::now() < deadline, "Stop never settled");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    f.run("child-a", "task a").await;
    let queue = f.wait_for_root_queue(1).await;
    f.settle().await;
    assert!(f.notices().is_empty(), "the note must not restart the root");
    assert!(!f.core.sessions.turn_in_flight(ROOT));
    assert_eq!(f.root_queue().len(), 1, "it waits for the user");
    // The user resuming releases it.
    f.core
        .doc_host
        .send_queued_now(ROOT, &queue[0].id)
        .await
        .unwrap();
    let notices = f.wait_for_notices(1).await;
    assert!(notices[0].contains("child-a"));
    assert!(f.root_queue().is_empty());
}

#[tokio::test]
async fn a_steerable_root_that_asks_keeps_its_question() {
    let f = Fixture::steerable();
    f.chat("child-a", Some(ROOT), "Fix parser");
    f.run(ROOT, "ASK").await;
    f.run("child-a", "task a").await;
    let queue = f.wait_for_root_queue(1).await;
    f.settle().await;
    assert!(queue[0].text.starts_with(NOTICE));
    assert!(f.notices().is_empty(), "a steer would abandon the question");
    assert!(f.core.sessions.turn_in_flight(ROOT));
    assert!(!f.core.sessions.steers_now(ROOT));
    assert_eq!(f.root_queue().len(), 1);
}

#[tokio::test]
async fn a_root_awaiting_input_gets_a_held_message_not_a_run() {
    let f = Fixture::new();
    f.chat("child-a", Some(ROOT), "Fix parser");
    f.run(ROOT, "ASK").await;
    f.run("child-a", "task a").await;
    f.settle().await;
    assert!(f.notices().is_empty(), "no turn was started");
    let queue = f
        .core
        .doc_host
        .open(ROOT)
        .unwrap()
        .doc()
        .read_queue()
        .unwrap();
    assert_eq!(queue.len(), 1, "{queue:?}");
    assert!(queue[0].hold_for_turn_end);
    assert!(queue[0].text.starts_with(NOTICE));
    assert!(queue[0].text.contains("child-a"));
}

#[tokio::test]
async fn an_awaiting_input_child_reports_its_question() {
    let f = Fixture::new();
    f.chat("child-a", Some(ROOT), "Pick storage");
    f.run("child-a", "ASK").await;
    let notices = f.wait_for_notices(1).await;
    assert!(notices[0].contains("awaiting input"), "{}", notices[0]);
    assert!(notices[0].contains("Which database?"), "{}", notices[0]);
    assert!(notices[0].contains("pg | sqlite"), "{}", notices[0]);
    assert!(notices[0].contains("zeron_respond_to_input"));
}

#[tokio::test]
async fn chats_outside_a_tree_are_not_pushed() {
    let f = Fixture::new();
    f.chat("plain", None, "Plain chat");
    f.chat("side", Some("plain"), "Side chat");
    f.run("side", "task").await;
    f.settle().await;
    assert!(f.notices().is_empty());
    // The root itself has no parent to tell.
    f.run(ROOT, "plan").await;
    f.settle().await;
    assert!(f.notices().is_empty());
}

#[tokio::test]
async fn an_archived_parent_is_not_woken() {
    let f = Fixture::new();
    f.chat("child-a", Some(ROOT), "Fix parser");
    f.core.workspace.set_chat_archived(ROOT, true).unwrap();
    f.run("child-a", "task a").await;
    f.settle().await;
    assert!(f.notices().is_empty());
    assert!(f.core.workspace.chat(ROOT).unwrap().unwrap().archived);
}

#[tokio::test]
async fn a_member_parent_is_notified_by_its_own_children() {
    let f = Fixture::new();
    f.chat("child-a", Some(ROOT), "Lead");
    f.chat("grand-1", Some("child-a"), "Helper");
    f.run("grand-1", "helper task").await;
    let notices = f.wait_for_notices(1).await;
    assert!(notices[0].contains("grand-1"));
    assert!(notices[0].contains("Helper"));
}
