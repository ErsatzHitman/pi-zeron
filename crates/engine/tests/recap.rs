//! Recaps: the isolated side-model run, its cache, the failure cooldown and the
//! `RecapChat` RPC.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use futures::{StreamExt, stream::BoxStream};
use zeron_doc::{MessagePart, MessageRole, MessageStatus, SessionMessageEntry};
use zeron_engine::recap::{RecapAction, RecapStatus};
use zeron_engine::registry::TitleSettings;
use zeron_engine::{EngineCore, HarnessRegistry};
use zeron_harness::{Harness, HarnessError, RunControls};
use zeron_proto::{
    AgentEvent, DoneStatus, HarnessId, Model, ReasoningLevel, RunRequest, SteeringMode,
};
use zeron_rpc::methods;

struct Call {
    request: RunRequest,
    instructions: &'static str,
    usage_surface: &'static str,
}

/// A Pi stand-in: answers isolated runs from a script, records each call.
struct Scripted {
    id: HarnessId,
    calls: Arc<Mutex<Vec<Call>>>,
    fail: Arc<Mutex<bool>>,
    delay: std::time::Duration,
}

#[async_trait]
impl Harness for Scripted {
    fn id(&self) -> HarnessId {
        self.id
    }
    fn display_name(&self) -> &str {
        "Scripted"
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
        _: RunRequest,
        _: RunControls,
    ) -> Result<BoxStream<'static, Result<AgentEvent, HarnessError>>, HarnessError> {
        panic!("a recap must never use the coding entry point")
    }
    async fn run_isolated(
        &self,
        request: RunRequest,
        _: RunControls,
        instructions: &'static str,
        usage_surface: &'static str,
    ) -> Result<BoxStream<'static, Result<AgentEvent, HarnessError>>, HarnessError> {
        assert!(std::path::Path::new(&request.cwd).is_dir());
        self.calls.lock().unwrap().push(Call {
            request,
            instructions,
            usage_surface,
        });
        tokio::time::sleep(self.delay).await;
        if *self.fail.lock().unwrap() {
            return Err(HarnessError::Protocol("model unavailable".into()));
        }
        Ok(futures::stream::iter(vec![
            Ok(AgentEvent::TextDelta {
                text: "Recap: Fixing the login flow\nTests pass\nNext: ship it".into(),
            }),
            Ok(AgentEvent::Done {
                status: DoneStatus::Completed,
                result: None,
                error: None,
                session_id: None,
                class: None,
            }),
        ])
        .boxed())
    }
}

fn entry(id: &str, role: MessageRole, text: &str, status: MessageStatus) -> SessionMessageEntry {
    SessionMessageEntry {
        id: id.into(),
        role,
        parts: vec![MessagePart::Text {
            id: format!("{id}-text"),
            text: text.into(),
        }],
        created_at: 1,
        device_id: "device".into(),
        status: Some(status),
        continuation_of: None,
        duration_ms: None,
    }
}

struct Fixture {
    core: EngineCore,
    calls: Arc<Mutex<Vec<Call>>>,
    fail: Arc<Mutex<bool>>,
    _dir: tempfile::TempDir,
}

fn fixture(delay_ms: u64) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let calls = Arc::new(Mutex::new(Vec::new()));
    let fail = Arc::new(Mutex::new(false));
    let registry = Arc::new(HarnessRegistry::new());
    registry.register(Arc::new(Scripted {
        id: HarnessId::Pi,
        calls: calls.clone(),
        fail: fail.clone(),
        delay: std::time::Duration::from_millis(delay_ms),
    }));
    let core = EngineCore::assemble(dir.path(), registry.clone(), HarnessId::Mock, None).unwrap();
    registry
        .set_title_settings(TitleSettings {
            harness: Some(HarnessId::Pi),
            model: Some("chosen-side-model".into()),
        })
        .unwrap();
    Fixture {
        core,
        calls,
        fail,
        _dir: dir,
    }
}

fn chat_with(core: &EngineCore, chat_id: &str, entries: &[SessionMessageEntry]) {
    core.workspace
        .create_chat(
            chat_id,
            None,
            Some(&core.device_id),
            None,
            Some("/tmp".into()),
        )
        .unwrap();
    let handle = core.doc_host.open(chat_id).unwrap();
    for entry in entries {
        handle.doc().push_message(entry).unwrap();
    }
}

fn finished_turn() -> Vec<SessionMessageEntry> {
    vec![
        entry(
            "u1",
            MessageRole::User,
            "Fix the login flow",
            MessageStatus::Complete,
        ),
        entry(
            "a1",
            MessageRole::Assistant,
            "Fixed it; the tests pass.",
            MessageStatus::Complete,
        ),
    ]
}

#[tokio::test]
async fn recap_generator_runs_isolated_and_caches() {
    let fx = fixture(0);
    chat_with(&fx.core, "main", &finished_turn());
    let generate = RecapAction::Generate { force: false };

    let first = fx.core.recaps.handle("main", generate).await.unwrap();
    assert_eq!(first.status, RecapStatus::Ready);
    let recap = first.recap.unwrap();
    assert_eq!(
        recap.text,
        "Fixing the login flow\nTests pass\nNext: ship it"
    );
    assert_eq!(recap.key.last_entry_id, "a1");
    {
        let calls = fx.calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        let call = &calls[0];
        assert_eq!(call.instructions, zeron_harness::RECAP_INSTRUCTIONS);
        assert_eq!(call.usage_surface, "zeron-recap");
        assert_eq!(call.request.model.as_deref(), Some("chosen-side-model"));
        assert_eq!(call.request.sandbox, zeron_proto::SandboxLevel::ReadOnly);
        assert!(call.request.resume.is_none());
        assert!(
            !std::path::Path::new(&call.request.cwd).exists(),
            "the scratch directory is cleaned up"
        );
        assert!(call.request.prompt.contains("Fix the login flow"));
    }

    // The transcript has not moved: the stored recap answers.
    let again = fx.core.recaps.handle("main", generate).await.unwrap();
    assert_eq!(again.status, RecapStatus::Cached);
    assert_eq!(again.recap.as_ref(), Some(&recap));
    assert_eq!(fx.calls.lock().unwrap().len(), 1);

    // `get` reads the store without a model call.
    let got = fx
        .core
        .recaps
        .handle("main", RecapAction::Get)
        .await
        .unwrap();
    assert_eq!(got.recap.as_ref(), Some(&recap));

    // force regenerates.
    let forced = fx
        .core
        .recaps
        .handle("main", RecapAction::Generate { force: true })
        .await
        .unwrap();
    assert_eq!(forced.status, RecapStatus::Ready);
    assert_eq!(fx.calls.lock().unwrap().len(), 2);

    // New settled work invalidates it.
    let handle = fx.core.doc_host.open("main").unwrap();
    handle
        .doc()
        .push_message(&entry(
            "u2",
            MessageRole::User,
            "And the logout flow?",
            MessageStatus::Complete,
        ))
        .unwrap();
    handle
        .doc()
        .push_message(&entry(
            "a2",
            MessageRole::Assistant,
            "Done too.",
            MessageStatus::Complete,
        ))
        .unwrap();
    let next = fx.core.recaps.handle("main", generate).await.unwrap();
    assert_eq!(next.status, RecapStatus::Ready);
    assert_eq!(next.recap.unwrap().key.last_entry_id, "a2");
    assert_eq!(fx.calls.lock().unwrap().len(), 3);
    fx.core.shutdown().await;
}

#[tokio::test]
async fn concurrent_recaps_for_one_chat_share_one_run() {
    let fx = fixture(150);
    chat_with(&fx.core, "main", &finished_turn());
    let generate = RecapAction::Generate { force: false };
    let (a, b) = tokio::join!(
        fx.core.recaps.handle("main", generate),
        fx.core.recaps.handle("main", generate)
    );
    let (a, b) = (a.unwrap(), b.unwrap());
    assert_eq!(a.recap, b.recap);
    assert!(a.recap.is_some());
    assert_eq!(fx.calls.lock().unwrap().len(), 1);
    fx.core.shutdown().await;
}

#[tokio::test]
async fn recap_refuses_live_or_empty_chats_and_side_chats() {
    let fx = fixture(0);
    let generate = RecapAction::Generate { force: false };
    chat_with(&fx.core, "empty", &[]);
    chat_with(
        &fx.core,
        "single",
        &[entry(
            "u1",
            MessageRole::User,
            "hi",
            MessageStatus::Complete,
        )],
    );
    chat_with(
        &fx.core,
        "user-only",
        &[
            entry("u1", MessageRole::User, "hi", MessageStatus::Complete),
            entry("u2", MessageRole::User, "hello?", MessageStatus::Complete),
        ],
    );
    chat_with(&fx.core, "btw-side", &finished_turn());
    for chat in ["empty", "single", "user-only", "btw-side"] {
        let outcome = fx.core.recaps.handle(chat, generate).await.unwrap();
        assert_eq!(outcome.status, RecapStatus::NothingToRecap, "{chat}");
        assert!(outcome.recap.is_none());
    }
    assert!(fx.calls.lock().unwrap().is_empty());
    fx.core.shutdown().await;
}

#[tokio::test]
async fn recap_failure_is_not_stored_and_cools_down() {
    let fx = fixture(0);
    chat_with(&fx.core, "main", &finished_turn());
    let generate = RecapAction::Generate { force: false };
    *fx.fail.lock().unwrap() = true;
    assert!(fx.core.recaps.handle("main", generate).await.is_err());
    let attempts = fx.calls.lock().unwrap().len();
    assert!(attempts >= 1);
    let stored = fx
        .core
        .recaps
        .handle("main", RecapAction::Get)
        .await
        .unwrap();
    assert_eq!(
        stored.status,
        RecapStatus::None,
        "no fallback text is stored"
    );

    // Inside the cooldown an automatic request does not call the model.
    let cooled = fx.core.recaps.handle("main", generate).await.unwrap();
    assert_eq!(cooled.status, RecapStatus::Busy);
    assert_eq!(fx.calls.lock().unwrap().len(), attempts);

    // An explicit request bypasses it.
    *fx.fail.lock().unwrap() = false;
    let forced = fx
        .core
        .recaps
        .handle("main", RecapAction::Generate { force: true })
        .await
        .unwrap();
    assert_eq!(forced.status, RecapStatus::Ready);
    fx.core.shutdown().await;
}

#[tokio::test]
async fn recap_falls_back_when_the_title_harness_cannot_serve_recaps() {
    let dir = tempfile::tempdir().unwrap();
    let pi_calls = Arc::new(Mutex::new(Vec::new()));
    let claude_calls = Arc::new(Mutex::new(Vec::new()));
    let registry = Arc::new(HarnessRegistry::new());
    for (id, calls) in [
        (HarnessId::Pi, pi_calls.clone()),
        (HarnessId::ClaudeCode, claude_calls.clone()),
    ] {
        registry.register(Arc::new(Scripted {
            id,
            calls,
            fail: Arc::new(Mutex::new(false)),
            delay: std::time::Duration::ZERO,
        }));
    }
    let core = EngineCore::assemble(dir.path(), registry.clone(), HarnessId::Mock, None).unwrap();
    // The Thread-naming harness only has a title path: recaps go to the chat's
    // own harness instead, and the title model does not follow them there.
    registry
        .set_title_settings(TitleSettings {
            harness: Some(HarnessId::ClaudeCode),
            model: Some("title-only-model".into()),
        })
        .unwrap();
    chat_with(&core, "main", &finished_turn());
    let outcome = core
        .recaps
        .handle("main", RecapAction::Generate { force: false })
        .await
        .unwrap();
    assert_eq!(outcome.status, RecapStatus::Ready);
    assert!(claude_calls.lock().unwrap().is_empty());
    let calls = pi_calls.lock().unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].request.harness, Some(HarnessId::Pi));
    assert_ne!(calls[0].request.model.as_deref(), Some("title-only-model"));
    drop(calls);
    core.shutdown().await;
}

#[tokio::test]
async fn recap_chat_rpc_round_trip_and_delete_forgets() {
    let fx = fixture(0);
    chat_with(&fx.core, "main", &finished_turn());
    let client = zeron_rpc::memory_client(fx.core.rpc_service());
    let target = fx.core.device_id.clone();

    let empty: serde_json::Value = client
        .call_as(
            methods::RECAP_CHAT,
            serde_json::json!({"chatId": "main", "action": "get", "targetDeviceId": target}),
        )
        .await
        .unwrap();
    assert_eq!(empty["status"], "none");

    let made: serde_json::Value = client
        .call_as(
            methods::RECAP_CHAT,
            serde_json::json!({"chatId": "main", "action": "generate", "targetDeviceId": target}),
        )
        .await
        .unwrap();
    assert_eq!(made["status"], "ready");
    assert_eq!(made["recap"]["key"]["lastEntryId"], "a1");

    let got: serde_json::Value = client
        .call_as(
            methods::RECAP_CHAT,
            serde_json::json!({"chatId": "main", "action": "get", "targetDeviceId": target}),
        )
        .await
        .unwrap();
    assert_eq!(got["recap"], made["recap"]);

    client
        .call(
            methods::MUTATE,
            serde_json::json!({"op": "deleteChat", "chatId": "main"}),
        )
        .await
        .unwrap();
    let gone = fx
        .core
        .recaps
        .handle("main", RecapAction::Get)
        .await
        .unwrap();
    assert_eq!(gone.status, RecapStatus::None);
    fx.core.shutdown().await;
}
