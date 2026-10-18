//! Pushed child outcomes: a settled tree member tells its parent, batched,
//! unless the parent is busy, the tree is paused, or the parent is archived.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use futures::{StreamExt, stream::BoxStream};
use zeron_engine::{EngineCore, HarnessRegistry};
use zeron_harness::{Harness, HarnessError, RunControls};
use zeron_proto::orchestrator_project::ProjectUpdate;
use zeron_proto::{
    AgentEvent, DoneStatus, HarnessId, Model, ReasoningLevel, RunRequest, SandboxLevel,
    SteeringMode, UserInputQuestion,
};

const ROOT: &str = "orchestrator-x";
const NOTICE: &str = "[Message from Zeron (automatic child-outcome notice)";

/// Records every prompt. "HANG" never finishes (a working chat), "ASK" asks a
/// question and waits (an awaiting-input chat); anything else completes with
/// `reply: <prompt>`.
struct Capture(Arc<Mutex<Vec<String>>>);

#[async_trait]
impl Harness for Capture {
    fn id(&self) -> HarnessId {
        HarnessId::Mock
    }
    fn display_name(&self) -> &str {
        "Capture"
    }
    fn supports_steering(&self) -> bool {
        false
    }
    fn steering_mode(&self) -> SteeringMode {
        SteeringMode::TurnBoundary
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
        self.0.lock().unwrap().push(request.prompt.clone());
        let started = AgentEvent::SessionStarted {
            harness: HarnessId::Mock,
            model: "mock-1".into(),
            tools: vec![],
            cwd: request.cwd.clone(),
            session_id: format!("sess-{}", request.prompt.len()),
            assistant_message_id: "a-1".into(),
        };
        match request.prompt.as_str() {
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
    _dir: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let prompts = Arc::new(Mutex::new(Vec::new()));
        let registry = HarnessRegistry::new();
        registry.register(Arc::new(Capture(prompts.clone())));
        let core =
            EngineCore::assemble(dir.path(), Arc::new(registry), HarnessId::Mock, None).unwrap();
        let fixture = Self {
            core,
            prompts,
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
async fn a_working_root_is_not_pushed_to() {
    let f = Fixture::new();
    f.chat("child-a", Some(ROOT), "Fix parser");
    f.run(ROOT, "HANG").await;
    f.run("child-a", "task a").await;
    f.settle().await;
    assert!(f.notices().is_empty());
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
