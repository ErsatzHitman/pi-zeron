#![cfg(feature = "native-fixture")]
use futures::StreamExt;
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};
use zeron_harness::{CancellationToken, Harness, PiHarness, RunControls, SteerMessage};
use zeron_proto::{AgentEvent, DoneStatus, RunRequest, SandboxLevel};
fn harness() -> PiHarness {
    PiHarness::new()
        .with_executable(env!("CARGO_BIN_EXE_harness-pi-fixture"))
        // Never consult the developer's own Pi settings.
        .with_agent_dir(std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join("pi-rpc-agent"))
        .with_graces(Duration::from_millis(100), Duration::from_millis(100))
}
fn request(cwd: &std::path::Path, prompt: &str) -> RunRequest {
    RunRequest {
        prompt: prompt.into(),
        harness: None,
        model: None,
        reasoning: None,
        model_options: Default::default(),
        cwd: cwd.display().to_string(),
        sandbox: SandboxLevel::WorkspaceWrite,
        auto_approve: true,
        resume: None,
        attachments: vec![],
        worktree: None,
        mcp: None,
    }
}
fn controls() -> (RunControls, mpsc::Sender<SteerMessage>, CancellationToken) {
    let (tx, rx) = mpsc::channel(8);
    let token = CancellationToken::new();
    (
        RunControls {
            realtime: None,
            execution_lease: None,
            steering: rx,
            interrupt: token.clone(),
            request_input: Box::new(|_| {
                let (tx, rx) = oneshot::channel();
                let _ = tx.send(vec![]);
                rx
            }),
            turn: Default::default(),
            extension_ui: Default::default(),
        },
        tx,
        token,
    )
}
async fn collect(prompt: &str) -> Vec<AgentEvent> {
    let dir = tempfile::tempdir().unwrap();
    let (c, tx, _) = controls();
    drop(tx);
    let stream = harness()
        .with_session_store(dir.path().join("index"))
        .run(request(dir.path(), prompt), c)
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), stream.map(Result::unwrap).collect())
        .await
        .unwrap()
}
#[tokio::test]
async fn terminal_contract_handles_normal_errors_retries_compaction_and_consumed_prompts() {
    for (prompt, status) in [
        ("hello", DoneStatus::Completed),
        ("error", DoneStatus::Errored),
        ("quota", DoneStatus::Errored),
        ("reject", DoneStatus::Errored),
        ("retry", DoneStatus::Completed),
        ("compact", DoneStatus::Completed),
        ("/noop", DoneStatus::Completed),
        ("handled", DoneStatus::Completed),
        ("crash", DoneStatus::Errored),
    ] {
        let events = collect(prompt).await;
        let dones: Vec<_> = events
            .iter()
            .filter_map(|e| {
                if let AgentEvent::Done { status, .. } = e {
                    Some(status)
                } else {
                    None
                }
            })
            .collect();
        assert_eq!(dones, vec![&status], "{prompt}: {events:?}");
        if prompt == "hello" {
            let text: String = events
                .iter()
                .filter_map(|e| {
                    if let AgentEvent::TextDelta { text } = e {
                        Some(text.as_str())
                    } else {
                        None
                    }
                })
                .collect();
            assert_eq!(text, "reply:hello");
        }
    }
}

#[cfg(unix)]
#[tokio::test]
async fn a_sigkilled_pi_reports_the_signal_and_an_out_of_memory_hint() {
    use zeron_proto::ErrorClass;
    let events = collect("sigkill").await;
    let dones: Vec<_> = events
        .iter()
        .filter_map(|event| match event {
            AgentEvent::Done {
                status,
                error,
                class,
                ..
            } => Some((*status, error.clone(), class.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(dones.len(), 1, "{events:?}");
    let (status, error, class) = &dones[0];
    assert_eq!(*status, DoneStatus::Errored);
    let error = error.as_deref().unwrap();
    assert!(
        error.contains("SIGKILL") && error.contains("signal 9") && error.contains("out of memory"),
        "{error}"
    );
    assert_eq!(
        *class,
        Some(ErrorClass::ProcessKilled {
            signal: 9,
            oom_suspected: true
        })
    );
}

/// A Pi that dies between turns has no Done to carry the failure: the stream
/// itself must say so, or the engine sees a clean end and never learns that
/// the background work it held is gone.
#[cfg(unix)]
#[tokio::test]
async fn a_pi_killed_between_turns_surfaces_an_error_event() {
    let dir = tempfile::tempdir().unwrap();
    // The steering sender stays open, so the runtime parks after its turn.
    let (c, _steering, _) = controls();
    let mut stream = harness()
        .with_session_store(dir.path().join("index"))
        .run(request(dir.path(), "idle-sigkill"), c)
        .await
        .unwrap();
    let mut finished = false;
    let failure = tokio::time::timeout(Duration::from_secs(10), async {
        while let Some(item) = stream.next().await {
            match item {
                Ok(AgentEvent::Done { .. }) => finished = true,
                Ok(_) => {}
                Err(error) => return Some(error.to_string()),
            }
        }
        None
    })
    .await
    .unwrap();
    assert!(finished, "the turn itself completed");
    let failure = failure.expect("the death is reported, not a silent end of the stream");
    assert!(
        failure.contains("SIGKILL") && failure.contains("signal 9"),
        "{failure}"
    );
}

#[tokio::test]
async fn failed_runs_carry_a_failure_class() {
    use zeron_proto::ErrorClass;
    let done_class = |events: &[AgentEvent]| -> Option<ErrorClass> {
        events.iter().find_map(|e| match e {
            AgentEvent::Done { class, .. } => Some(class.clone()),
            _ => None,
        })?
    };
    for (prompt, want) in [
        (
            "quota",
            Some(ErrorClass::QuotaExhausted { resets_at: None }),
        ),
        (
            "auth",
            Some(ErrorClass::AuthRequired {
                provider: Some("anthropic".into()),
            }),
        ),
        ("overload", Some(ErrorClass::ProviderOverloaded)),
        ("error", Some(ErrorClass::Other)),
        ("reject", Some(ErrorClass::Other)),
        (
            "reject-auth",
            Some(ErrorClass::AuthRequired { provider: None }),
        ),
        ("retry", None),
        ("hello", None),
    ] {
        let events = collect(prompt).await;
        assert_eq!(done_class(&events), want, "{prompt}: {events:?}");
    }
    let events = collect("limit").await;
    let Some(ErrorClass::RateLimited {
        resets_at: Some(at),
    }) = done_class(&events)
    else {
        panic!("limit: {events:?}");
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    assert!(
        (at - now - 30_000).abs() < 5_000,
        "reset in about 30s: {at} vs {now}"
    );
}

#[tokio::test]
async fn pi_retry_frames_become_retrying_events_and_the_run_still_completes() {
    let events = collect("retry").await;
    let retrying: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::Retrying {
                attempt,
                max_attempts,
                delay_ms,
                error,
                active,
            } => Some((*attempt, *max_attempts, *delay_ms, error.clone(), *active)),
            _ => None,
        })
        .collect();
    assert_eq!(
        retrying,
        vec![
            (
                1,
                Some(3),
                Some(150),
                Some("mock provider failure".to_string()),
                true
            ),
            (1, None, None, None, false),
        ],
        "{events:?}"
    );
    assert!(matches!(
        events.last(),
        Some(AgentEvent::Done {
            status: DoneStatus::Completed,
            ..
        })
    ));
}

#[tokio::test]
async fn nested_agent_environment_is_not_inherited() {
    if std::env::var_os("CLAUDECODE").is_none() {
        // Re-run in a private environment; never mutate this process's env.
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "nested_agent_environment_is_not_inherited",
                "--nocapture",
            ])
            .env("CLAUDECODE", "1")
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            output.status.success() && stdout.contains("1 passed"),
            "{output:?}"
        );
        return;
    }
    let text: String = collect("env")
        .await
        .iter()
        .filter_map(|e| match e {
            AgentEvent::TextDelta { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(text, "reply:env:false");
}

#[tokio::test]
async fn resumes_the_same_session_after_process_shutdown() {
    let dir = tempfile::tempdir().unwrap();
    let h = harness().with_session_store(dir.path().join("index"));
    let mut id = None;
    for text in ["first", "second"] {
        let (c, tx, _) = controls();
        drop(tx);
        let mut req = request(dir.path(), text);
        req.resume = id.clone();
        let events: Vec<_> = tokio::time::timeout(
            Duration::from_secs(5),
            h.run(req, c).await.unwrap().collect(),
        )
        .await
        .unwrap();
        for event in events {
            if let AgentEvent::Done {
                session_id, status, ..
            } = event.unwrap()
            {
                assert_eq!(status, DoneStatus::Completed);
                if id.is_some() {
                    assert_eq!(id, session_id);
                }
                id = session_id;
            }
        }
    }
    assert!(id.is_some());
}

#[tokio::test]
async fn unrestorable_session_starts_fresh_with_a_visible_notice() {
    // The engine resumes a chat's stored id on every dispatch, so a hard error
    // here would leave the chat unusable for good.
    let dir = tempfile::tempdir().unwrap();
    let (c, tx, _) = controls();
    drop(tx);
    let mut req = request(dir.path(), "hello");
    req.resume = Some("missing-session".into());
    let events: Vec<_> = tokio::time::timeout(
        Duration::from_secs(5),
        harness()
            .with_session_store(dir.path().join("index"))
            .run(req, c)
            .await
            .unwrap()
            .map(Result::unwrap)
            .collect(),
    )
    .await
    .unwrap();
    assert!(
        events.iter().any(|e| matches!(e, AgentEvent::Error { message }
            if message.contains("missing-session") && message.contains("without the previous context"))),
        "{events:?}"
    );
    let done: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::Done {
                status, session_id, ..
            } => Some((*status, session_id.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(
        done,
        vec![(DoneStatus::Completed, Some("pi-fixture-session".into()))]
    );
}

#[tokio::test]
async fn steers_confirm_on_consumption_and_interrupt_is_terminal_once() {
    let dir = tempfile::tempdir().unwrap();
    let (c, tx, token) = controls();
    let mut stream = harness()
        .with_session_store(dir.path().join("index"))
        .run(request(dir.path(), "slow"), c)
        .await
        .unwrap();
    tx.send(SteerMessage {
        prompt: "redirect".into(),
        message_id: Some("user-id".into()),
        attachments: Vec::new(),
        config: None,
    })
    .await
    .unwrap();
    let mut confirmed = 0;
    let mut done = vec![];
    tokio::time::timeout(Duration::from_secs(5), async {
        while let Some(event) = stream.next().await {
            match event.unwrap() {
                AgentEvent::Steered {
                    next_assistant_message_id,
                    ..
                } => {
                    assert_ne!(next_assistant_message_id.as_deref(), Some("user-id"));
                    confirmed += 1;
                    token.cancel();
                }
                AgentEvent::Done { status, .. } => done.push(status),
                _ => {}
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(confirmed, 1);
    assert_eq!(done, vec![DoneStatus::Interrupted]);
}
#[tokio::test]
async fn idle_mailbox_starts_another_turn_without_restarting_process() {
    let dir = tempfile::tempdir().unwrap();
    let (c, tx, _) = controls();
    let mut stream = harness()
        .with_session_store(dir.path().join("index"))
        .run(request(dir.path(), "first"), c)
        .await
        .unwrap();
    let mut sender = Some(tx);
    let mut done = 0;
    let mut confirmed = 0;
    tokio::time::timeout(Duration::from_secs(5), async {
        while let Some(event) = stream.next().await {
            match event.unwrap() {
                AgentEvent::Done { status, .. } => {
                    assert_eq!(status, DoneStatus::Completed);
                    done += 1;
                    if let Some(tx) = sender.take() {
                        tx.send(SteerMessage {
                            prompt: "second".into(),
                            message_id: None,
                            attachments: Vec::new(),
                            config: None,
                        })
                        .await
                        .unwrap();
                    }
                }
                AgentEvent::Steered { .. } => confirmed += 1,
                _ => {}
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(done, 2);
    assert_eq!(confirmed, 1);
}

#[tokio::test]
async fn each_delivered_user_message_is_named_for_a_rewind() {
    // The run's own prompt has no Zeron id here (the host knows it); each
    // warm prompt carries its id. Pi names both by their timestamp.
    let dir = tempfile::tempdir().unwrap();
    let (c, tx, _) = controls();
    let mut stream = harness()
        .with_session_store(dir.path().join("index"))
        .run(request(dir.path(), "first"), c)
        .await
        .unwrap();
    let mut sender = Some(tx);
    let mut delivered = vec![];
    tokio::time::timeout(Duration::from_secs(5), async {
        while let Some(event) = stream.next().await {
            match event.unwrap() {
                AgentEvent::Done { .. } => {
                    if let Some(tx) = sender.take() {
                        tx.send(SteerMessage {
                            prompt: "second".into(),
                            message_id: Some("zeron-2".into()),
                            attachments: Vec::new(),
                            config: None,
                        })
                        .await
                        .unwrap();
                    }
                }
                AgentEvent::UserMessageDelivered {
                    message_id,
                    harness_ref,
                } => delivered.push((message_id, harness_ref)),
                _ => {}
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(delivered.len(), 2, "{delivered:?}");
    assert_eq!(delivered[0].0, None);
    assert_eq!(delivered[1].0.as_deref(), Some("zeron-2"));
    for (_, harness_ref) in &delivered {
        assert!(harness_ref.starts_with("ts:"), "{harness_ref}");
    }
    assert_ne!(delivered[0].1, delivered[1].1);
}

#[tokio::test]
async fn a_refused_prompt_ends_its_turn_and_keeps_the_pi_process() {
    let dir = tempfile::tempdir().unwrap();
    let (c, tx, _) = controls();
    let mut stream = harness()
        .with_session_store(dir.path().join("index"))
        .run(request(dir.path(), "reject"), c)
        .await
        .unwrap();
    let mut sender = Some(tx);
    let mut statuses = Vec::new();
    tokio::time::timeout(Duration::from_secs(5), async {
        while let Some(event) = stream.next().await {
            if let AgentEvent::Done { status, .. } = event.unwrap() {
                statuses.push(status);
                // The same stream (and process) takes the next message.
                if let Some(tx) = sender.take() {
                    tx.send(SteerMessage {
                        prompt: "hello".into(),
                        message_id: None,
                        attachments: Vec::new(),
                        config: None,
                    })
                    .await
                    .unwrap();
                }
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(statuses, vec![DoneStatus::Errored, DoneStatus::Completed]);
}

/// Run `prompt` (the fixture starts a manual compaction with it), steer one
/// message in while it runs and collect the whole run.
async fn steer_into_a_compaction(prompt: &str) -> (Vec<AgentEvent>, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let (c, tx, _) = controls();
    let stream = harness()
        .with_session_store(dir.path().join("index"))
        .run(request(dir.path(), prompt), c)
        .await
        .unwrap();
    tx.send(SteerMessage {
        prompt: "after-compaction".into(),
        message_id: Some("steer-1".into()),
        attachments: Vec::new(),
        config: None,
    })
    .await
    .unwrap();
    drop(tx);
    let events = tokio::time::timeout(
        Duration::from_secs(15),
        stream.map(Result::unwrap).collect::<Vec<_>>(),
    )
    .await
    .unwrap();
    (events, dir)
}

fn assert_steer_taken_once_after_compaction(events: &[AgentEvent]) {
    let dones: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::Done { status, .. } => Some(*status),
            _ => None,
        })
        .collect();
    assert!(!dones.is_empty(), "{events:?}");
    assert!(
        dones.iter().all(|status| *status == DoneStatus::Completed),
        "a steer refused for a compaction must not fail the turn: {events:?}"
    );
    let steered = events
        .iter()
        .filter(|e| matches!(e, AgentEvent::Steered { .. }))
        .count();
    assert_eq!(steered, 1, "confirmed once, when consumed: {events:?}");
    let delivered: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::UserMessageDelivered { message_id, .. } => message_id.as_deref(),
            _ => None,
        })
        .collect();
    assert_eq!(delivered, ["steer-1"], "{events:?}");
    let text: String = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::TextDelta { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(
        text.matches("reply:after-compaction").count(),
        1,
        "the steer reached the model exactly once: {text:?}"
    );
}

#[tokio::test]
async fn a_steer_during_compaction_waits_and_is_confirmed_only_when_consumed() {
    // Pi's start frame never reached us, so the first send is refused.
    let (events, dir) = steer_into_a_compaction("silent-compaction").await;
    assert_steer_taken_once_after_compaction(&events);
    let refusals = std::fs::read_to_string(dir.path().join("compaction-refusals")).unwrap();
    assert_eq!(
        refusals.lines().collect::<Vec<_>>(),
        ["after-compaction"],
        "refused once, then held back until the compaction ended"
    );
}

#[tokio::test]
async fn a_steer_is_not_submitted_while_pi_reports_compacting() {
    let (events, dir) = steer_into_a_compaction("compacting-turn").await;
    assert_steer_taken_once_after_compaction(&events);
    assert!(
        !dir.path().join("compaction-refusals").exists(),
        "no prompt was sent into the compaction"
    );
}

#[tokio::test]
async fn steering_mode_is_selected_only_while_unconfigured() {
    for configured in [None, Some("one-at-a-time"), Some("all")] {
        let dir = tempfile::tempdir().unwrap();
        let agent = dir.path().join("agent");
        std::fs::create_dir_all(&agent).unwrap();
        if let Some(mode) = configured {
            std::fs::write(
                agent.join("settings.json"),
                serde_json::json!({ "steeringMode": mode }).to_string(),
            )
            .unwrap();
        }
        let (c, tx, _) = controls();
        drop(tx);
        let stream = harness()
            .with_agent_dir(&agent)
            .with_session_store(dir.path().join("index"))
            .run(request(dir.path(), "hello"), c)
            .await
            .unwrap();
        let events: Vec<_> =
            tokio::time::timeout(Duration::from_secs(5), stream.map(Result::unwrap).collect())
                .await
                .unwrap();
        assert!(
            events.iter().any(
                |e| matches!(e, AgentEvent::Done { status, .. } if *status == DoneStatus::Completed)
            ),
            "{events:?}"
        );
        // Pi persists set_steering_mode globally: an explicit choice must survive.
        assert_eq!(
            std::fs::read_to_string(dir.path().join("steering-mode")).ok(),
            configured.is_none().then(|| "all".to_string()),
            "{configured:?}"
        );
    }
}

#[tokio::test]
async fn discovers_model_specific_thinking_and_extension_commands() {
    let h = harness();
    let models = h.models().await.unwrap();
    assert_eq!(models[0].id, "mock/mock");
    assert_eq!(
        models[0].reasoning_levels,
        vec![
            zeron_proto::ReasoningLevel::Low,
            zeron_proto::ReasoningLevel::Medium,
            zeron_proto::ReasoningLevel::High
        ]
    );
    assert_eq!(models[0].options[0].id, "pi_thinking");
    let dir = tempfile::tempdir().unwrap();
    let commands = h.commands_for(dir.path()).await.unwrap();
    for expected in ["noop", "skill:probe", "compact", "session"] {
        assert!(commands.iter().any(|c| c.name == expected));
    }
    let skills = h.skills(dir.path()).await.unwrap().unwrap();
    assert!(skills.iter().any(|s| s.name == "probe"));
}
#[tokio::test]
async fn rpc_control_commands_finish_without_waiting_for_agent_settled() {
    for command in [
        "/compact",
        "/session",
        "/name Test",
        "/autocompact off",
        "/steering all",
    ] {
        let events = collect(command).await;
        assert_eq!(
            events
                .iter()
                .filter(|e| matches!(
                    e,
                    AgentEvent::Done {
                        status: DoneStatus::Completed,
                        ..
                    }
                ))
                .count(),
            1,
            "{command}: {events:?}"
        );
    }
}

#[tokio::test]
async fn extension_dialogs_roundtrip_without_autoaccepting_and_preserve_editor_text() {
    for (method, label, key, expected) in [
        (
            "select",
            Some("second"),
            "value",
            serde_json::json!("second"),
        ),
        ("select", None, "cancelled", serde_json::json!(true)),
        ("confirm", Some("No"), "confirmed", serde_json::json!(false)),
        (
            "input",
            Some("custom"),
            "value",
            serde_json::json!("custom"),
        ),
        (
            "editor",
            Some("  first\nsecond\n"),
            "value",
            serde_json::json!("  first\nsecond\n"),
        ),
        ("editor", Some(""), "value", serde_json::json!("")),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let (mut c, tx, _) = controls();
        drop(tx);
        c.request_input = Box::new(move |questions| {
            assert_eq!(questions.len(), 1);
            assert_eq!(questions[0].multiline, method == "editor");
            // Pi 1.1 sends `prefill` only with `editor`.
            assert_eq!(
                questions[0].prefill.as_deref(),
                (method == "editor").then_some("initial\nvalue")
            );
            let (tx, rx) = oneshot::channel();
            let _ = tx.send(
                label
                    .map(|l| {
                        vec![zeron_proto::UserInputAnswer {
                            question_id: questions[0].id.clone(),
                            labels: vec![l.into()],
                        }]
                    })
                    .unwrap_or_default(),
            );
            rx
        });
        let events: Vec<_> = tokio::time::timeout(
            Duration::from_secs(5),
            harness()
                .with_session_store(dir.path().join("index"))
                .run(request(dir.path(), &format!("/dialog {method}")), c)
                .await
                .unwrap()
                .map(Result::unwrap)
                .collect(),
        )
        .await
        .unwrap();
        // The fixture reports the reply as a notice: a row in the turn.
        let reply = events
            .iter()
            .find_map(|e| match e {
                AgentEvent::ToolCall {
                    call: zeron_proto::ToolCall::Mcp { tool, input, .. },
                    ..
                } if tool == "notice" => input
                    .as_ref()
                    .and_then(|input| input["message"].as_str())
                    .and_then(|text| serde_json::from_str::<serde_json::Value>(text.trim()).ok()),
                _ => None,
            })
            .expect("dialog reply notification");
        assert_eq!(reply[key], expected, "{events:?}");
        assert_eq!(
            events
                .iter()
                .filter(|e| matches!(
                    e,
                    AgentEvent::Done {
                        status: DoneStatus::Completed,
                        ..
                    }
                ))
                .count(),
            1
        );
    }
}

async fn wait_for_queued_steers(dir: &std::path::Path, expected: &[String]) {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let queued = std::fs::read(dir.join("pending-steers.json"))
                .ok()
                .and_then(|bytes| serde_json::from_slice::<Vec<String>>(&bytes).ok());
            if queued.as_deref() == Some(expected) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the entire burst must reach Pi before the blocked model step finishes");
}

#[tokio::test]
async fn steering_burst_reaches_one_model_step_and_confirms_each_message_on_consumption() {
    for cancel in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let (c, tx, token) = controls();
        let mut stream = harness()
            .with_session_store(dir.path().join("index"))
            .run(request(dir.path(), "burst-start"), c)
            .await
            .unwrap();
        let mut assistant = None;
        tokio::time::timeout(Duration::from_secs(5), async {
            while let Some(event) = stream.next().await {
                match event.unwrap() {
                    AgentEvent::SessionStarted {
                        assistant_message_id,
                        ..
                    } => assistant = Some(assistant_message_id),
                    AgentEvent::TextDelta { text } if text == "waiting" => return,
                    AgentEvent::Done { .. } => panic!("model step ended before release"),
                    _ => {}
                }
            }
            panic!("fixture did not start the blocked model step");
        })
        .await
        .unwrap();
        // Exceed the engine mailbox size and include duplicate text. Delivery
        // receipts must still be one per original message, in the original order.
        let prompts: Vec<_> = (0..40)
            .map(|i| {
                if i == 20 {
                    // A command name without '/' is still ordinary user text.
                    "noop".into()
                } else {
                    format!("redirect {}", i / 2)
                }
            })
            .collect();
        for (i, prompt) in prompts.iter().enumerate() {
            tx.send(SteerMessage {
                prompt: prompt.clone(),
                message_id: Some(format!("user-{i}")),
                attachments: Vec::new(),
                config: None,
            })
            .await
            .unwrap();
        }
        drop(tx);
        wait_for_queued_steers(dir.path(), &prompts).await;
        assert!(
            tokio::time::timeout(Duration::from_millis(30), stream.next())
                .await
                .is_err(),
            "queue acceptance must not confirm consumption or finish the turn"
        );
        if cancel {
            token.cancel();
        } else {
            std::fs::write(dir.path().join("release-burst"), "").unwrap();
        }
        let mut confirms = 0;
        let mut done = vec![];
        let mut text = String::new();
        tokio::time::timeout(Duration::from_secs(5), async {
            while let Some(event) = stream.next().await {
                match event.unwrap() {
                    AgentEvent::Steered {
                        assistant_message_id,
                        next_assistant_message_id,
                    } => {
                        assert_eq!(assistant_message_id, assistant);
                        assert!(next_assistant_message_id.is_some());
                        assistant = next_assistant_message_id;
                        confirms += 1;
                    }
                    AgentEvent::TextDelta { text: delta } => text.push_str(&delta),
                    AgentEvent::Done { status, .. } => done.push(status),
                    _ => {}
                }
            }
        })
        .await
        .unwrap();
        if cancel {
            assert_eq!(confirms, 0);
            assert_eq!(done, vec![DoneStatus::Interrupted]);
            assert!(!text.contains("redirect"), "{text}");
        } else {
            assert_eq!(confirms, prompts.len());
            assert_eq!(done, vec![DoneStatus::Completed]);
            assert_eq!(
                text,
                format!("reply:burst-startreply:{}", prompts.join("\n"))
            );
        }
    }
}

#[tokio::test]
async fn handled_input_between_steers_keeps_its_own_delivery_receipt() {
    for handled in ["handled", "/noop"] {
        let dir = tempfile::tempdir().unwrap();
        let (c, tx, _) = controls();
        let mut stream = harness()
            .with_session_store(dir.path().join("index"))
            .run(request(dir.path(), "burst-start"), c)
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            while let Some(event) = stream.next().await {
                if matches!(event.unwrap(), AgentEvent::TextDelta { text } if text == "waiting") {
                    return;
                }
            }
            panic!("fixture did not reach the blocked model step");
        })
        .await
        .unwrap();
        for prompt in ["redirect first", handled, "redirect last"] {
            tx.send(SteerMessage {
                prompt: prompt.into(),
                message_id: None,
                attachments: Vec::new(),
                config: None,
            })
            .await
            .unwrap();
        }
        drop(tx);
        wait_for_queued_steers(dir.path(), &["redirect first".into()]).await;
        std::fs::write(dir.path().join("release-burst"), "").unwrap();
        let mut receipts = 0;
        let mut text = String::new();
        let mut completions = 0;
        tokio::time::timeout(Duration::from_secs(5), async {
            while let Some(event) = stream.next().await {
                match event.unwrap() {
                    AgentEvent::Steered { .. } => receipts += 1,
                    AgentEvent::TextDelta { text: delta } => {
                        if delta.contains("redirect first") {
                            assert_eq!(
                                receipts, 1,
                                "handled input must not consume another message's receipt"
                            );
                        }
                        if delta.contains("redirect last") {
                            assert_eq!(receipts, 3);
                        }
                        text.push_str(&delta);
                    }
                    AgentEvent::Done { status, .. } => {
                        assert_eq!(status, DoneStatus::Completed);
                        completions += 1;
                    }
                    _ => {}
                }
            }
        })
        .await
        .unwrap();
        assert_eq!(receipts, 3);
        assert!(completions >= 1);
        assert_eq!(
            text,
            "reply:burst-startreply:redirect firstreply:redirect last"
        );
    }
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn crash_interrupt_and_consumer_drop_reap_descendants_even_with_inherited_pipes() {
    for scenario in ["inherited-pipe-crash", "interrupt", "drop"] {
        let dir = tempfile::tempdir().unwrap();
        let (c, tx, token) = controls();
        let prompt = if scenario == "inherited-pipe-crash" {
            scenario
        } else {
            "tree"
        };
        let mut stream = harness()
            .with_session_store(dir.path().join("index"))
            .run(request(dir.path(), prompt), c)
            .await
            .unwrap();
        let mut terminals = vec![];
        tokio::time::timeout(Duration::from_secs(5), async {
            while let Some(event) = stream.next().await {
                match event.unwrap() {
                    AgentEvent::TextDelta { text } if text == "tree ready" => {
                        if scenario == "drop" {
                            break;
                        }
                        token.cancel();
                    }
                    AgentEvent::Done { status, error, .. } => {
                        if scenario == "inherited-pipe-crash" {
                            assert!(error.unwrap().contains("inherited pipe diagnostic"));
                        }
                        terminals.push(status);
                    }
                    _ => {}
                }
            }
        })
        .await
        .unwrap();
        drop(stream);
        drop(tx);
        if scenario != "drop" {
            assert_eq!(
                terminals,
                vec![if scenario == "interrupt" {
                    DoneStatus::Interrupted
                } else {
                    DoneStatus::Errored
                }]
            );
        }
        for file in ["pi.pid", "tool.pid"] {
            let pid = std::fs::read_to_string(dir.path().join(file)).unwrap();
            tokio::time::timeout(Duration::from_secs(3), async {
                loop {
                    let stat =
                        std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap_or_default();
                    if stat.is_empty() || stat.split_whitespace().nth(2) == Some("Z") {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .expect("owned descendant must be dead");
        }
    }
}

#[tokio::test]
async fn late_extension_notifications_do_not_reopen_completed_turns() {
    let dir = tempfile::tempdir().unwrap();
    let (c, tx, _) = controls();
    let mut stream = harness()
        .with_session_store(dir.path().join("index"))
        .run(request(dir.path(), "late-notify"), c)
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while !matches!(
            stream.next().await.unwrap().unwrap(),
            AgentEvent::Done { .. }
        ) {}
    })
    .await
    .unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(150), stream.next())
            .await
            .is_err()
    );
    tx.send(SteerMessage {
        prompt: "next".into(),
        message_id: None,
        attachments: Vec::new(),
        config: None,
    })
    .await
    .unwrap();
    drop(tx);
    let events: Vec<_> =
        tokio::time::timeout(Duration::from_secs(5), stream.map(Result::unwrap).collect())
            .await
            .unwrap();
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(
                e,
                AgentEvent::Done {
                    status: DoneStatus::Completed,
                    ..
                }
            ))
            .count(),
        1
    );
}

/// A model or thinking switch reaches the live process before the message
/// it was sent with: no new process, no lost session.
#[tokio::test]
async fn a_model_switch_applies_to_the_live_process() {
    let dir = tempfile::tempdir().unwrap();
    let (c, tx, _) = controls();
    let mut opening = request(dir.path(), "which-model");
    opening.model = Some("mock/mock".into());
    let mut switched = opening.clone();
    switched.model = Some("mock/mock-2".into());
    switched.reasoning = Some(zeron_proto::ReasoningLevel::High);
    let driver = harness().with_session_store(dir.path().join("index"));
    assert!(driver.reconfigures_in_place(&opening, &switched));
    let mut stream = driver.run(opening, c).await.unwrap();
    let mut text = String::new();
    let mut dones = 0;
    let mut started = 0;
    while dones < 2 {
        let event = tokio::time::timeout(Duration::from_secs(5), stream.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        match event {
            AgentEvent::SessionStarted { .. } => started += 1,
            AgentEvent::TextDelta { text: delta } => text.push_str(&delta),
            AgentEvent::Done { .. } => {
                dones += 1;
                text.push('|');
                if dones == 1 {
                    tx.send(SteerMessage {
                        config: Some(Box::new(switched.clone())),
                        ..SteerMessage::text("which-model")
                    })
                    .await
                    .unwrap();
                }
            }
            _ => {}
        }
    }
    assert_eq!(started, 1);
    assert!(text.contains("reply:mock/medium|"), "{text}");
    assert!(text.contains("reply:mock-2/high|"), "{text}");
}

#[tokio::test]
async fn pi_older_than_1_1_is_refused_with_a_clear_error() {
    let dir = tempfile::tempdir().unwrap();
    #[cfg(windows)]
    let exe = {
        let path = dir.path().join("old-pi.cmd");
        std::fs::write(&path, "@echo 0.87.1\r\n").unwrap();
        path
    };
    #[cfg(unix)]
    let exe = {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.path().join("old-pi");
        std::fs::write(&path, "#!/bin/sh\necho 0.87.1\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    };
    let old = PiHarness::new().with_executable(&exe);
    let error = old.verified_executable().unwrap_err().to_string();
    assert!(
        error.contains("0.87.1") && error.contains("1.1.0"),
        "{error}"
    );
    let (c, _tx, _) = controls();
    let Err(error) = old.run(request(dir.path(), "hello"), c).await else {
        panic!("an old Pi must not start a session");
    };
    assert!(error.to_string().contains("too old"), "{error}");
    let (exe, version) = harness().verified_executable().unwrap();
    assert!(exe.exists());
    assert_eq!(version, semver::Version::new(1, 1, 0));
}

/// No setting and no `PI_EXECUTABLE`: Pi is "not configured", and the `pi`
/// on PATH (often an old global install) is never used instead.
#[test]
fn an_unconfigured_pi_never_falls_back_to_path() {
    if std::env::var_os("ZERON_PI_UNCONFIGURED_CHILD").is_none() {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "an_unconfigured_pi_never_falls_back_to_path"])
            .env("ZERON_PI_UNCONFIGURED_CHILD", "1")
            .env_remove("PI_EXECUTABLE")
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            output.status.success() && stdout.contains("1 passed"),
            "{output:?}"
        );
        return;
    }
    let setting: zeron_harness::pi::ExecutableSetting = std::sync::Arc::new(|| None);
    let pi = PiHarness::new().with_executable_setting(setting);
    let error = pi.resolve_executable().unwrap_err().to_string();
    assert!(error.contains("Settings"), "{error}");
    assert!(!pi.installed());
    let configured = std::path::PathBuf::from(env!("CARGO_BIN_EXE_harness-pi-fixture"));
    let setting: zeron_harness::pi::ExecutableSetting =
        std::sync::Arc::new(move || Some(configured.clone()));
    assert!(PiHarness::new().with_executable_setting(setting).installed());
}

#[tokio::test]
async fn edit_diffs_come_from_pi_edits_array_and_transport_commands_stay_hidden() {
    let events = collect("edit").await;
    let diff = events
        .iter()
        .find_map(|e| match e {
            AgentEvent::ToolResult { diff: Some(diff), .. } => Some(diff.clone()),
            _ => None,
        })
        .expect("edit diff");
    assert_eq!(diff.path, "a.txt");
    assert_eq!(diff.old_text.as_deref(), Some("one\ntwo"));
    assert_eq!(diff.new_text, "uno\ndos");
    let commands = events
        .iter()
        .find_map(|e| match e {
            AgentEvent::AvailableCommands { commands } => Some(commands.clone()),
            _ => None,
        })
        .expect("advertised commands");
    assert!(commands.iter().any(|c| c.name == "noop"));
    // An extension's command runs beside the chat; a skill is a prompt.
    assert!(commands.iter().any(|c| c.name == "noop" && c.side));
    assert!(commands.iter().all(|c| c.name != "skill:probe" || !c.side));
    assert!(commands.iter().all(|c| !c.name.starts_with("pi_ui_")));
}

fn guard(dir: &std::path::Path) -> zeron_harness::pi::DeleteGuardSetting {
    let guard = zeron_harness::pi::DeleteGuard {
        enabled: false,
        forced: false,
        state_file: Some(dir.join("harness-prefs.json")),
        dir: dir.join("pi-guard"),
        usage_alerts_file: Some(dir.join("usage-alerts.json")),
    };
    std::sync::Arc::new(move || guard.clone())
}

/// Every chat run loads the delete guard, MCP bridge or not, and tells it
/// the toggle, the live file and where it lives for the run's children.
#[tokio::test]
async fn chat_runs_load_the_delete_guard() {
    let dir = tempfile::tempdir().unwrap();
    let (c, tx, _) = controls();
    drop(tx);
    let req = request(dir.path(), "guard-env");
    let stream = harness()
        .with_session_store(dir.path().join("index"))
        .with_delete_guard(guard(dir.path()))
        .run(req, c)
        .await
        .unwrap();
    let events: Vec<_> = tokio::time::timeout(
        Duration::from_secs(5),
        stream.map(Result::unwrap).collect::<Vec<_>>(),
    )
    .await
    .unwrap();
    let text: String = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::TextDelta { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    let file = dir.path().join("harness-prefs.json");
    assert!(text.contains("on=0 "), "{text}");
    assert!(
        text.contains(&format!("file={} ", file.display())),
        "{text}"
    );
    assert!(text.contains("force=- "), "{text}");
    let usage = dir.path().join("usage-alerts.json");
    assert!(
        text.contains(&format!("usage={}", usage.display())),
        "the usage snapshot is named for the run and its children: {text}"
    );
    let installed = std::fs::read_dir(dir.path().join("pi-guard"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| {
            path.extension().is_some_and(|ext| ext == "mjs")
                && !path.to_string_lossy().contains("preload")
        })
        .expect("the guard is installed");
    assert!(
        text.contains(&format!("ext={} ", installed.display())),
        "{text}"
    );
    assert!(
        text.contains(&format!("--extension {}", installed.display())),
        "{text}"
    );
}

#[tokio::test]
async fn titles_run_in_a_bare_process_and_complete() {
    let dir = tempfile::tempdir().unwrap();
    let (c, _tx, _) = controls();
    let mut req = request(dir.path(), "args");
    req.model = Some("mock/mock".into());
    let events: Vec<_> = tokio::time::timeout(
        Duration::from_secs(5),
        harness()
            .with_delete_guard(guard(dir.path()))
            .run_title(req, c)
            .await
            .unwrap()
            .map(Result::unwrap)
            .collect::<Vec<_>>(),
    )
    .await
    .unwrap();
    let AgentEvent::TextDelta { text } = &events[0] else {
        panic!("{events:?}");
    };
    for flag in [
        "--no-session",
        "--no-tools",
        "--no-extensions",
        "--no-skills",
        "--no-context-files",
        "--system-prompt",
        "--model mock/mock",
    ] {
        assert!(text.contains(flag), "{flag} missing from {text}");
    }
    // A title runs no tools, so it loads no guard.
    assert!(!text.contains("zeron-delete-guard"), "{text}");
    assert!(!dir.path().join("pi-guard").exists());
    assert!(matches!(
        events.last(),
        Some(AgentEvent::Done { status: DoneStatus::Completed, .. })
    ));
}

#[tokio::test]
async fn recap_run_uses_recap_instructions_in_a_bare_process() {
    let dir = tempfile::tempdir().unwrap();
    let (c, _tx, _) = controls();
    let events: Vec<_> = tokio::time::timeout(
        Duration::from_secs(5),
        harness()
            .with_delete_guard(guard(dir.path()))
            .run_isolated(
                request(dir.path(), "args"),
                c,
                zeron_harness::RECAP_INSTRUCTIONS,
                zeron_harness::RECAP_USAGE_SURFACE,
            )
            .await
            .unwrap()
            .map(Result::unwrap)
            .collect::<Vec<_>>(),
    )
    .await
    .unwrap();
    let AgentEvent::TextDelta { text } = &events[0] else {
        panic!("{events:?}");
    };
    for flag in ["--no-session", "--no-tools", "--no-extensions"] {
        assert!(text.contains(flag), "{flag} missing from {text}");
    }
    assert!(
        text.contains("You write a recap"),
        "the recap instructions are the system prompt: {text}"
    );
    assert!(
        !text.contains("You generate session titles"),
        "the title instructions are not used: {text}"
    );
    assert!(matches!(
        events.last(),
        Some(AgentEvent::Done {
            status: DoneStatus::Completed,
            ..
        })
    ));
}

#[tokio::test]
async fn displayed_custom_messages_become_transcript_chips() {
    let events = collect("custom").await;
    let chips: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::ToolCall {
                id,
                call: zeron_proto::ToolCall::Mcp { server, tool, .. },
            } if server == "pi" => Some((id.clone(), tool.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(chips.len(), 1, "{events:?}");
    assert_eq!(chips[0].1, "workflow-result");
    assert!(events.iter().any(|e| matches!(e,
        AgentEvent::ToolResult { id, output: Some(output), .. } if *id == chips[0].0 && output == "All steps passed")));
}

struct UiPort {
    state: tokio::sync::watch::Receiver<zeron_proto::extension_ui::ExtensionUiState>,
    actions: mpsc::UnboundedSender<zeron_proto::extension_ui::ExtensionUiAction>,
    watched: tokio::sync::watch::Sender<bool>,
}
fn ui_port(watched: bool) -> (zeron_harness::ExtensionUiPort, UiPort) {
    let (state_tx, state) = tokio::sync::watch::channel(Default::default());
    let (actions, actions_rx) = mpsc::unbounded_channel();
    let (watched, watched_rx) = tokio::sync::watch::channel(watched);
    (
        zeron_harness::ExtensionUiPort {
            state: Some(state_tx),
            actions: Some(actions_rx),
            watched: Some(watched_rx),
        },
        UiPort {
            state,
            actions,
            watched,
        },
    )
}
async fn wait_ui(
    port: &mut UiPort,
    what: &str,
    ready: impl Fn(&zeron_proto::extension_ui::ExtensionUiState) -> bool,
) -> zeron_proto::extension_ui::ExtensionUiState {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            {
                let state = port.state.borrow_and_update();
                if ready(&state) {
                    return state.clone();
                }
            }
            port.state.changed().await.unwrap();
        }
    })
    .await
    .unwrap_or_else(|_| panic!("{what}: {:?}", port.state.borrow()))
}
fn keys(state: &zeron_proto::extension_ui::ExtensionUiState) -> Vec<String> {
    state
        .elements
        .iter()
        .map(|e| format!("{}:{}", e["ns"].as_str().unwrap(), e["id"].as_str().unwrap()))
        .collect()
}
/// The events the fake Pi logged. A line it is still writing is left for
/// the next read.
fn piui_events(dir: &std::path::Path) -> Vec<serde_json::Value> {
    std::fs::read_to_string(dir.join("piui-events.jsonl"))
        .unwrap_or_default()
        .split_inclusive('\n')
        .filter(|line| line.ends_with('\n'))
        .map(|line| serde_json::from_str(line.trim_end()).unwrap())
        .collect()
}

/// PIUI is on only while a UI that renders it watches the chat; elements keep
/// flowing between turns, chunks reassemble, actions reach the extensions
/// without opening a turn, and `bye` hands questions back to dialogs.
#[tokio::test]
async fn piui_follows_watchers_flows_while_idle_and_round_trips_actions() {
    let dir = tempfile::tempdir().unwrap();
    let (mut c, tx, _) = controls();
    let turn = zeron_harness::TurnControl::default();
    c.turn = turn.clone();
    let (port, mut ui) = ui_port(true);
    c.extension_ui = port;
    let mut stream = harness()
        .with_session_store(dir.path().join("index"))
        .run(request(dir.path(), "piui-idle"), c)
        .await
        .unwrap();
    let mut dones = 0;
    tokio::time::timeout(Duration::from_secs(5), async {
        while let Some(event) = stream.next().await {
            if matches!(event.unwrap(), AgentEvent::Done { .. }) {
                dones += 1;
                break;
            }
        }
    })
    .await
    .unwrap();
    let state = wait_ui(&mut ui, "hello snapshot + idle element", |s| {
        s.connected && s.elements.len() == 4
    })
    .await;
    assert_eq!(
        keys(&state),
        vec!["ask-user:question", "bash:jobs", "todo:list", "uniichat:working"]
    );
    assert_eq!(state.instance.as_deref(), Some("fixture-instance"));
    assert_eq!(
        state.elements[2]["sections"][0]["items"][0]["text"],
        "ñ".repeat(40)
    );
    assert!(turn.background_live(), "a running background shell holds the process");

    let action = |element: &str, action: &str, row: Option<&str>| {
        zeron_proto::extension_ui::ExtensionUiAction {
            element_id: element.into(),
            action_id: action.into(),
            value: Some(serde_json::json!({"fields":{"selection":"Blue"}})),
            row_id: row.map(str::to_owned),
        }
    };
    ui.actions.send(action("ask-user:question", "submit", None)).unwrap();
    wait_ui(&mut ui, "answered sheet removed", |s| s.elements.len() == 3).await;
    ui.actions.send(action("bash:jobs", "kill", Some("j1"))).unwrap();
    let state = wait_ui(&mut ui, "killed row", |s| {
        s.elements[0]["rows"][0]["state"] == "error"
    })
    .await;
    assert_eq!(state.elements[0]["title"], "Background bash · 0 running");
    // The fleet channel follows the patch in its own frame.
    tokio::time::timeout(Duration::from_secs(5), async {
        while turn.background_live() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("killing the last job frees the process");

    // The last watcher left: bye, and nothing stale lingers.
    ui.watched.send(false).unwrap();
    wait_ui(&mut ui, "disconnected", |s| !s.connected && s.elements.is_empty()).await;
    tokio::time::timeout(Duration::from_secs(5), async {
        while piui_events(dir.path()).last().map(|e| e["type"].clone()) != Some("bye".into()) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let events = piui_events(dir.path());
    assert_eq!(events[0], serde_json::json!({"type":"hello","protocol":1,"client":"zeron"}));
    assert_eq!(events[1]["elementId"], "ask-user:question");
    assert_eq!(events[1]["value"]["fields"]["selection"], "Blue");
    assert_eq!(events[2]["rowId"], "j1");
    assert_eq!(events.len(), 4);

    // No action ever opened a turn; closing the process clears the UI.
    drop(tx);
    let rest: Vec<_> = tokio::time::timeout(Duration::from_secs(5), stream.collect::<Vec<_>>())
        .await
        .unwrap();
    assert!(
        rest.iter()
            .all(|e| !matches!(e, Ok(AgentEvent::SessionStarted { .. }))),
        "{rest:?}"
    );
    assert_eq!(dones, 1);
    let state = wait_ui(&mut ui, "cleared on exit", |s| s.instance.is_none()).await;
    assert!(state.is_empty());
}

/// A side command (`/jev off`) runs beside the chat: it opens no turn, what
/// it reports is a toast, and one that says nothing still says it ran.
#[tokio::test]
async fn a_side_command_reports_in_toasts_and_never_opens_a_turn() {
    let dir = tempfile::tempdir().unwrap();
    let (mut c, tx, _) = controls();
    let (port, mut ui) = ui_port(true);
    c.extension_ui = port;
    let mut stream = harness()
        .with_session_store(dir.path().join("index"))
        .run(request(dir.path(), "first"), c)
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while let Some(event) = stream.next().await {
            if matches!(event.unwrap(), AgentEvent::Done { .. }) {
                break;
            }
        }
    })
    .await
    .unwrap();
    let side = |text: &str| zeron_proto::extension_ui::ExtensionUiAction {
        element_id: zeron_proto::SIDE_COMMAND_ELEMENT.into(),
        action_id: "run".into(),
        value: Some(serde_json::json!({ "text": text })),
        row_id: None,
    };
    let toasted = |message: &'static str| {
        move |s: &zeron_proto::extension_ui::ExtensionUiState| {
            s.notices.iter().any(|n| n.message == message)
        }
    };
    ui.actions.send(side("/noisy off")).unwrap();
    wait_ui(&mut ui, "its report as a toast", toasted("noisy: off")).await;
    ui.actions.send(side("/noop")).unwrap();
    wait_ui(&mut ui, "a quiet command says it ran", toasted("Ran /noop")).await;
    // A skill is a prompt: never run beside the chat.
    ui.actions.send(side("/skill:probe")).unwrap();
    wait_ui(
        &mut ui,
        "a skill refused",
        toasted("/skill:probe is not an extension command here"),
    )
    .await;

    drop(tx);
    let rest: Vec<_> = tokio::time::timeout(Duration::from_secs(5), stream.collect::<Vec<_>>())
        .await
        .unwrap();
    assert!(
        rest.iter().all(|e| !matches!(
            e,
            Ok(AgentEvent::SessionStarted { .. }
                | AgentEvent::ToolCall { .. }
                | AgentEvent::TextDelta { .. }
                | AgentEvent::Done { .. })
        )),
        "{rest:?}"
    );
}

/// A plain dialog Pi raises between turns has no transcript turn to ask in:
/// a watching UI gets it as a sheet, and its answer goes back to Pi.
#[tokio::test]
async fn an_idle_dialog_becomes_a_sheet_for_a_watching_ui() {
    let dir = tempfile::tempdir().unwrap();
    let (mut c, tx, _) = controls();
    let (port, mut ui) = ui_port(true);
    c.extension_ui = port;
    let mut stream = harness()
        .with_session_store(dir.path().join("index"))
        .run(request(dir.path(), "idle-dialog"), c)
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while let Some(event) = stream.next().await {
            if matches!(event.unwrap(), AgentEvent::Done { .. }) {
                break;
            }
        }
    })
    .await
    .unwrap();
    let state = wait_ui(&mut ui, "dialog sheet", |s| {
        keys(s).contains(&"pi:dialog-1".to_string())
    })
    .await;
    let sheet = state
        .typed_elements()
        .into_iter()
        .find(|e| e.key() == "pi:dialog-1")
        .unwrap();
    assert_eq!(sheet.title.as_deref(), Some("Keep the worktree?"));
    assert_eq!(sheet.form().unwrap().0[0].options.len(), 2);
    ui.actions
        .send(zeron_proto::extension_ui::ExtensionUiAction {
            element_id: "pi:dialog-1".into(),
            action_id: "submit".into(),
            value: Some(serde_json::json!({"fields":{"value":"Remove"}})),
            row_id: None,
        })
        .unwrap();
    wait_ui(&mut ui, "sheet closed", |s| {
        !keys(s).contains(&"pi:dialog-1".to_string())
    })
    .await;
    let path = dir.path().join("dialog-responses.jsonl");
    let response = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            // The fake Pi may still be writing the line: retry until it parses.
            if let Ok(text) = std::fs::read_to_string(&path)
                && let Some(line) = text.lines().next()
                && let Ok(response) = serde_json::from_str::<serde_json::Value>(line)
            {
                return response;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        response,
        serde_json::json!({"type":"extension_ui_response","id":"idle","value":"Remove"})
    );
    drop(tx);
}

/// Stop ends only the turn: Pi aborts and keeps running, and the next
/// message goes to the same process.
#[tokio::test]
async fn stopping_a_turn_keeps_the_pi_process() {
    let dir = tempfile::tempdir().unwrap();
    let (mut c, tx, _) = controls();
    let turn = zeron_harness::TurnControl::default();
    c.turn = turn.clone();
    let mut stream = harness()
        .with_session_store(dir.path().join("index"))
        .run(request(dir.path(), "slow"), c)
        .await
        .unwrap();
    let mut first = None;
    let mut text = String::new();
    tokio::time::timeout(Duration::from_secs(5), async {
        while let Some(event) = stream.next().await {
            match event.unwrap() {
                AgentEvent::SessionStarted { .. } if first.is_none() => {
                    first = Some(std::fs::read_to_string(dir.path().join("pi.pid")).unwrap());
                    turn.stop_turn();
                }
                AgentEvent::Done { status, .. } if text.is_empty() => {
                    assert_eq!(status, zeron_proto::DoneStatus::Interrupted);
                    text.push('|');
                    tx.send(SteerMessage {
                        prompt: "after".into(),
                        message_id: None,
                        attachments: Vec::new(),
                        config: None,
                    })
                    .await
                    .unwrap();
                }
                AgentEvent::TextDelta { text: delta } => text.push_str(&delta),
                AgentEvent::Done { status, .. } => {
                    assert_eq!(status, zeron_proto::DoneStatus::Completed);
                    break;
                }
                _ => {}
            }
        }
    })
    .await
    .unwrap();
    assert!(text.ends_with("reply:after"), "{text}");
    assert_eq!(
        std::fs::read_to_string(dir.path().join("pi.pid")).unwrap(),
        first.unwrap()
    );
}

/// Without a watching UI the extensions are never switched to PIUI, so a
/// plain client keeps getting dialogs and nothing is left waiting on a sheet.
#[tokio::test]
async fn an_unwatched_chat_never_turns_piui_on() {
    let dir = tempfile::tempdir().unwrap();
    let (mut c, tx, _) = controls();
    let (port, ui) = ui_port(false);
    c.extension_ui = port;
    drop(tx);
    let events: Vec<_> = tokio::time::timeout(
        Duration::from_secs(5),
        harness()
            .with_session_store(dir.path().join("index"))
            .run(request(dir.path(), "piui-idle"), c)
            .await
            .unwrap()
            .collect::<Vec<_>>(),
    )
    .await
    .unwrap();
    assert!(events.iter().any(|e| matches!(e, Ok(AgentEvent::Done { .. }))));
    assert!(piui_events(dir.path()).is_empty());
    assert!(ui.state.borrow().elements.is_empty());
}

/// Plain `setStatus`/`setTitle`/`set_editor_text` are live state for a
/// watching UI. A notice and a widget during a turn are the agent's work:
/// rows in the turn's transcript, not toasts or trays.
#[tokio::test]
async fn plain_extension_ui_requests_become_live_state() {
    let dir = tempfile::tempdir().unwrap();
    let (mut c, tx, _) = controls();
    let (port, mut ui) = ui_port(true);
    c.extension_ui = port;
    let mut stream = harness()
        .with_session_store(dir.path().join("index"))
        .run(request(dir.path(), "plain-ui"), c)
        .await
        .unwrap();
    let mut text = String::new();
    let mut rows = Vec::new();
    let mut settled = Vec::new();
    tokio::time::timeout(Duration::from_secs(5), async {
        while let Some(event) = stream.next().await {
            match event.unwrap() {
                AgentEvent::TextDelta { text: delta } => text.push_str(&delta),
                AgentEvent::ToolCall {
                    id,
                    call: zeron_proto::ToolCall::Mcp { server, tool, input },
                } if server == "pi" => rows.push((id, tool, input.unwrap_or_default())),
                AgentEvent::ToolResult { id, .. } => settled.push(id),
                AgentEvent::Done { .. } => break,
                _ => {}
            }
        }
    })
    .await
    .unwrap();
    assert!(!text.contains("Saved the plan"), "{text}");
    assert!(text.contains("reply:plain-ui"));
    let notice = rows.iter().find(|(_, tool, _)| tool == "notice").unwrap();
    assert_eq!(notice.2["message"], "Saved the plan");
    let widget = rows.iter().find(|(_, tool, _)| tool == "btw").unwrap();
    assert_eq!(widget.2["title"], "q: why?");
    assert_eq!(widget.2["lines"], serde_json::json!(["a: because"]));
    assert!(settled.contains(&notice.0) && settled.contains(&widget.0));
    let state = wait_ui(&mut ui, "plain UI", |s| s.editor_text.is_some()).await;
    assert!(state.notices.is_empty());
    assert_eq!(state.statuses[0].key, "!mode");
    assert_eq!(state.statuses[0].text, "Plan");
    assert_eq!(state.widgets[0].lines, vec!["q: why?", "a: because"]);
    assert_eq!(state.title.as_ref().unwrap().text, "Pi fixture");
    assert_eq!(state.editor_text.as_ref().unwrap().text, "rewritten prompt");
    drop(tx);
    let _: Vec<_> = tokio::time::timeout(Duration::from_secs(5), stream.collect::<Vec<_>>())
        .await
        .unwrap();
    // The process's elements go with it; its last notice and draft stay.
    let state = wait_ui(&mut ui, "process closed", |s| s.statuses.is_empty()).await;
    assert!(state.widgets.is_empty());
    assert_eq!(state.editor_text.as_ref().unwrap().text, "rewritten prompt");
}

/// A side chat's first run with its parent's session to fork.
fn fork_request(cwd: &std::path::Path, parent: &str) -> RunRequest {
    let mut req = request(cwd, "args");
    req.mcp = Some(zeron_proto::McpServer {
        name: "zeron".into(),
        command: "zeron".into(),
        args: vec!["mcp".into()],
        env: [(
            zeron_proto::orchestrator::ENV_FORK_SESSION.to_owned(),
            parent.to_owned(),
        )]
        .into(),
    });
    req
}

async fn run_events(harness: PiHarness, req: RunRequest) -> Vec<AgentEvent> {
    let (c, tx, _) = controls();
    drop(tx);
    tokio::time::timeout(
        Duration::from_secs(5),
        harness
            .run(req, c)
            .await
            .unwrap()
            .map(Result::unwrap)
            .collect(),
    )
    .await
    .unwrap()
}

#[tokio::test]
async fn a_side_chat_forks_its_parent_s_pi_session_file() {
    let dir = tempfile::tempdir().unwrap();
    let agent = dir.path().join("agent");
    let project = agent.join("sessions").join("project");
    std::fs::create_dir_all(&project).unwrap();
    let parent = project.join("parent.jsonl");
    let header = "{\"type\":\"session\",\"id\":\"parent-session\"}\n";
    std::fs::write(&parent, header).unwrap();
    let events = run_events(
        harness()
            .with_agent_dir(agent)
            .with_session_store(dir.path().join("index")),
        fork_request(dir.path(), "parent-session"),
    )
    .await;
    let text: String = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::TextDelta { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    let file = parent.canonicalize().unwrap();
    assert!(
        text.contains(&format!("--fork {}", file.display())),
        "{text}"
    );
    assert!(!text.contains("--session"), "{text}");
    // The parent's file is a source only: never written.
    assert_eq!(std::fs::read_to_string(&parent).unwrap(), header);
}

#[tokio::test]
async fn an_unforkable_parent_session_starts_the_side_chat_fresh_with_a_notice() {
    let dir = tempfile::tempdir().unwrap();
    let events = run_events(
        harness()
            .with_agent_dir(dir.path().join("agent"))
            .with_session_store(dir.path().join("index")),
        fork_request(dir.path(), "gone-session"),
    )
    .await;
    assert!(
        events.iter().any(|e| matches!(e, AgentEvent::Error { message }
            if message.contains("gone-session") && message.contains("without its context"))),
        "{events:?}"
    );
    assert!(matches!(
        events.last(),
        Some(AgentEvent::Done { status: DoneStatus::Completed, .. })
    ));
}

#[tokio::test]
async fn turn_trace_marks_each_pi_turn_once() {
    let dir = tempfile::tempdir().unwrap();
    let (mut c, tx, _) = controls();
    c.turn = zeron_harness::TurnControl::with_trace_enabled(true);
    let turn = c.turn.clone();
    let mut stream = harness()
        .with_session_store(dir.path().join("index"))
        .run(request(dir.path(), "first"), c)
        .await
        .unwrap();
    let mut sender = Some(tx);
    let mut traces = Vec::new();
    tokio::time::timeout(Duration::from_secs(5), async {
        while let Some(event) = stream.next().await {
            if let AgentEvent::Done { .. } = event.unwrap() {
                traces.push(turn.trace_finish());
                assert_eq!(turn.trace_finish(), None, "a turn is reported once");
                if let Some(tx) = sender.take() {
                    tx.send(SteerMessage {
                        prompt: "second".into(),
                        message_id: None,
                        attachments: Vec::new(),
                        config: None,
                    })
                    .await
                    .unwrap();
                }
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(traces.len(), 2);
    for t in traces {
        let t = t.expect("turn began");
        let (ack, barrier) = (t.ack_ms.expect("ack"), t.barrier_ms.expect("barrier"));
        assert!(ack <= barrier && barrier <= t.done_ms, "{t:?}");
    }
}
