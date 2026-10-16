//! Nested sub-agent rows and the btw pane, through actual GPUI layout.
use super::*;
use gpui::{AppContext, TestAppContext, VisualTestContext};

fn init(cx: &mut TestAppContext, dir: &std::path::Path) {
    cx.update(|cx| {
        gpui_base::init(cx);
        cx.set_global(Theme::default());
        crate::app_menus::init(cx);
        settings::init(UiSettings::default(), dir, cx);
        crate::history::init(
            Default::default(),
            Default::default(),
            Default::default(),
            Default::default(),
            cx,
        );
    });
}

fn boot(dir: &std::path::Path) -> EngineBootConfig {
    EngineBootConfig {
        data_dir: dir.into(),
        ipc_port: 0,
        edge_url: "http://127.0.0.1:1".into(),
        edge_token: None,
        org_id: None,
        workos_client_id: None,
        default_harness: zeron_proto::HarnessId::Mock,
    }
}

fn chat(id: &str, minutes_ago: i64) -> zeron_proto::Chat {
    serde_json::from_value(serde_json::json!({
        "id": id, "title": id, "deviceId": "local", "archived": false,
        "createdAt": Utc::now() - chrono::Duration::minutes(minutes_ago),
    }))
    .unwrap()
}

/// A working session row carrying `subagents` by name (or, empty, only
/// their count, as an older engine publishes it).
fn working(
    chat_id: &str,
    count: u32,
    subagents: Vec<zeron_proto::RunningSubagent>,
) -> zeron_proto::Session {
    zeron_proto::Session {
        last_completed_turn: None,
        chat_id: chat_id.into(),
        device_id: "local".into(),
        status: zeron_proto::SessionStatus::Working,
        started_at: Some(Utc::now()),
        updated_at: Utc::now(),
        running_subagents: count,
        subagents,
    }
}

fn subagent(doc_id: &str, title: &str) -> zeron_proto::RunningSubagent {
    zeron_proto::RunningSubagent {
        doc_id: doc_id.into(),
        title: title.into(),
        started_at: Utc::now() - chrono::Duration::seconds(30),
        fleet_row: None,
    }
}

/// A Pi fleet job, as the engine mirrors an unwatched roster.
fn fleet_job(row: &str, title: &str) -> zeron_proto::RunningSubagent {
    zeron_proto::RunningSubagent {
        doc_id: String::new(),
        title: title.into(),
        started_at: Utc::now() - chrono::Duration::seconds(30),
        fleet_row: Some(row.into()),
    }
}

struct SidebarHost(Entity<Shell>);

impl Render for SidebarHost {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.0.update(cx, |shell, cx| {
            div()
                .w(px(280.0))
                .h(px(800.0))
                .child(shell.render_chat_sidebar(&Theme::default(), cx))
        })
    }
}

fn sidebar(
    cx: &mut TestAppContext,
    sessions: Vec<zeron_proto::Session>,
) -> (tempfile::TempDir, Entity<Shell>, &mut VisualTestContext) {
    let dir = tempfile::tempdir().unwrap();
    init(cx, dir.path());
    let config = boot(dir.path());
    let (host, cx) = cx.add_window_view(|_, cx| {
        SidebarHost(cx.new(|cx| {
            let state = cx.new(|_| AppState::new());
            let mut shell = Shell::new(state, config, cx);
            shell.settings.sidebar_organization = SidebarOrganization::InOneList;
            shell.reduced_motion = true;
            shell.state.update(cx, |state, _| {
                state.workspace_scope = Some(WorkspaceScope::Local);
                state.local_device_id = Some("local".into());
                state.no_project = true;
                state.chats = vec![chat("open", 10), chat("busy", 5)];
                state.selected_chat = Some("open".into());
                state.sessions = sessions;
            });
            shell
        }))
    });
    let shell = host.read_with(cx, |host, _| host.0.clone());
    cx.update(|window, cx| {
        window.activate_window();
        window.draw(cx).clear();
    });
    (dir, shell, cx)
}

#[gpui::test]
fn a_session_that_is_not_open_lists_its_subagents_under_it(cx: &mut TestAppContext) {
    let (_dir, _shell, cx) = sidebar(
        cx,
        vec![working(
            "busy",
            2,
            vec![
                subagent("busy--sub--t1", "audit the auth flow"),
                subagent("busy--sub--t2", "scan the repo"),
            ],
        )],
    );
    let parent = cx.debug_bounds("chat-busy").expect("the session row");
    let first = cx
        .debug_bounds("sidebar-agent-subagent-busy--sub--t1")
        .expect("the first subagent row");
    let second = cx
        .debug_bounds("sidebar-agent-subagent-busy--sub--t2")
        .expect("the second subagent row");
    // Nested: indented on the tree rail, below the session, in start order.
    assert!(first.origin.x > parent.origin.x);
    assert!(first.origin.y >= parent.origin.y + parent.size.height);
    assert!(second.origin.y > first.origin.y);
    assert_eq!(
        f32::from(first.size.height),
        sidebar_children::CHILD_ROW_HEIGHT
    );
    // Named rows replace the summary row.
    assert!(cx.debug_bounds("sidebar-agents-busy").is_none());
    // The open session has none.
    assert!(cx.debug_bounds("sidebar-agents-open").is_none());
}

#[gpui::test]
fn a_session_that_is_not_open_lists_its_pi_fleet_and_a_click_opens_it(cx: &mut TestAppContext) {
    let (_dir, shell, cx) = sidebar(
        cx,
        vec![working(
            "busy",
            2,
            vec![
                fleet_job("j1", "reviewer"),
                subagent("busy--sub--t1", "audit the auth flow"),
            ],
        )],
    );
    let parent = cx.debug_bounds("chat-busy").expect("the session row");
    let fleet = cx
        .debug_bounds("sidebar-agent-fleet-j1")
        .expect("the fleet job's row");
    let harness = cx
        .debug_bounds("sidebar-agent-subagent-busy--sub--t1")
        .expect("the harness subagent's row");
    assert!(fleet.origin.x > parent.origin.x);
    assert!(fleet.origin.y >= parent.origin.y + parent.size.height);
    assert!(harness.origin.y > fleet.origin.y);
    assert!(cx.debug_bounds("sidebar-agents-busy").is_none());
    cx.simulate_event(MouseDownEvent {
        button: MouseButton::Left,
        position: fleet.center(),
        click_count: 1,
        ..Default::default()
    });
    cx.simulate_event(MouseUpEvent {
        button: MouseButton::Left,
        position: fleet.center(),
        click_count: 1,
        ..Default::default()
    });
    let selected = shell.read_with(cx, |shell, cx| shell.state.read(cx).selected_chat.clone());
    assert_eq!(selected.as_deref(), Some("busy"));
}

#[gpui::test]
fn an_engine_that_only_counts_gets_one_summary_row(cx: &mut TestAppContext) {
    let (_dir, _shell, cx) = sidebar(cx, vec![working("busy", 3, Vec::new())]);
    let parent = cx.debug_bounds("chat-busy").expect("the session row");
    let summary = cx
        .debug_bounds("sidebar-agents-busy")
        .expect("the summary row");
    assert!(summary.origin.x > parent.origin.x);
    assert!(summary.origin.y >= parent.origin.y + parent.size.height);
}

#[gpui::test]
fn clicking_a_nested_subagent_opens_its_session(cx: &mut TestAppContext) {
    let (_dir, shell, cx) = sidebar(
        cx,
        vec![working(
            "busy",
            1,
            vec![subagent("busy--sub--t1", "audit the auth flow")],
        )],
    );
    let row = cx
        .debug_bounds("sidebar-agent-subagent-busy--sub--t1")
        .expect("the subagent row");
    for event in [true, false] {
        if event {
            cx.simulate_event(MouseDownEvent {
                button: MouseButton::Left,
                position: row.center(),
                click_count: 1,
                ..Default::default()
            });
        } else {
            cx.simulate_event(MouseUpEvent {
                button: MouseButton::Left,
                position: row.center(),
                click_count: 1,
                ..Default::default()
            });
        }
    }
    let selected = shell.read_with(cx, |shell, cx| shell.state.read(cx).selected_chat.clone());
    assert_eq!(selected.as_deref(), Some("busy"));
}

#[gpui::test]
fn the_btw_pane_opens_at_the_cards_width_with_its_bar_inside(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    init(cx, dir.path());
    let config = boot(dir.path());
    let (shell, cx) = cx.add_window_view(|_, cx| {
        let state = cx.new(|_| {
            let mut state = AppState::new();
            state.no_project = true;
            state.selected_chat = Some("a".into());
            state.chats = vec![chat("a", 1)];
            state
        });
        let mut shell = Shell::new(state, config, cx);
        shell.debug_gate = Some(GatePhase::Ready);
        shell.splash = SplashPhase::Gone;
        shell.active_chat = "a".into();
        shell.reduced_motion = false;
        shell.test_time = Some(std::time::Instant::now());
        shell
    });
    cx.simulate_resize(gpui::size(px(1600.0), px(900.0)));
    cx.update(|window, cx| window.draw(cx).clear());
    shell.update(cx, |shell, _| {
        shell.test_time = Some(std::time::Instant::now())
    });
    cx.update(|_, cx| shell.update(cx, |shell, cx| shell.open_btw_pane(cx)));
    let reveal = |shell: &Entity<Shell>, cx: &mut VisualTestContext| {
        cx.update(|_, cx| shell.update(cx, |shell, cx| shell.btw_frame(cx).map(|(_, r)| r)))
    };
    // It eases in: nothing on its first frame, partway halfway through,
    // then whole.
    assert_eq!(reveal(&shell, cx), None);
    shell.update(cx, |shell, _| {
        *shell.test_time.as_mut().unwrap() +=
            Duration::from_millis(100).mul_f32(motion::speed_scale());
    });
    let early = reveal(&shell, cx).expect("the pane is showing");
    assert!(
        early > 0.0 && early < 1.0,
        "the pane opens animated, got {early}"
    );
    shell.update(cx, |shell, _| {
        *shell.test_time.as_mut().unwrap() += Duration::from_secs(3);
    });
    assert_eq!(reveal(&shell, cx), Some(1.0));
    cx.update(|window, cx| {
        window.refresh();
        window.draw(cx).clear();
    });
    let pane = cx.debug_bounds("session-btw").expect("the btw pane");
    let bar = cx.debug_bounds("session-btw-bar").expect("its prompt bar");
    assert_eq!(f32::from(pane.size.width), session_info::SESSION_CARD_WIDTH);
    // The prompt bar is inside the pane, at its bottom.
    assert!(bar.origin.x >= pane.origin.x && bar.right() <= pane.right());
    assert!(bar.origin.y > pane.origin.y && bar.bottom() <= pane.bottom() + px(0.5));
    // An empty side chat is its header and prompt bar, not a blank box.
    assert!(
        f32::from(pane.size.height) < 160.0,
        "an empty pane is {:?} tall",
        pane.size.height
    );
    // A small pane, not a sidebar: the right pane stays shut.
    assert!(
        cx.debug_bounds("right-panel")
            .is_none_or(|b| f32::from(b.size.width) < 1.0)
    );
}

/// A shell with chat "a" open and its btw pane fully revealed.
fn open_btw<'a>(
    cx: &'a mut TestAppContext,
    dir: &std::path::Path,
) -> (Entity<Shell>, &'a mut VisualTestContext) {
    init(cx, dir);
    let config = boot(dir);
    let (shell, cx) = cx.add_window_view(|_, cx| {
        let state = cx.new(|_| {
            let mut state = AppState::new();
            state.no_project = true;
            state.selected_chat = Some("a".into());
            state.chats = vec![chat("a", 1)];
            state
        });
        let mut shell = Shell::new(state, config, cx);
        shell.debug_gate = Some(GatePhase::Ready);
        shell.splash = SplashPhase::Gone;
        shell.active_chat = "a".into();
        shell.test_time = Some(std::time::Instant::now());
        shell
    });
    cx.simulate_resize(gpui::size(px(1600.0), px(900.0)));
    cx.update(|window, cx| window.draw(cx).clear());
    cx.update(|_, cx| shell.update(cx, |shell, cx| shell.open_btw_pane(cx)));
    shell.update(cx, |shell, _| {
        *shell.test_time.as_mut().unwrap() += Duration::from_secs(3);
    });
    (shell, cx)
}

/// Draw until the pane stops asking for frames (it follows its thread's
/// measured height a frame behind).
fn settle(cx: &mut VisualTestContext) {
    for _ in 0..8 {
        cx.update(|window, cx| {
            window.refresh();
            window.draw(cx).clear();
            window.simulate_next_frame(cx);
        });
    }
}

#[gpui::test]
fn the_btw_pane_grows_to_show_a_whole_reply(cx: &mut TestAppContext) {
    use zeron_doc::{MessagePart, MessageRole, MessageStatus, SessionMessageEntry};
    let dir = tempfile::tempdir().unwrap();
    let (shell, cx) = open_btw(cx, dir.path());
    settle(cx);
    let empty = cx.debug_bounds("session-btw").expect("the btw pane");
    // No model picker in the pane (the main composer keeps its own): the
    // side chat runs its parent's model.
    assert!(
        cx.debug_bounds("composer-model-picker")
            .is_none_or(|picker| !empty.intersects(&picker))
    );

    let (side, transcript) = shell.read_with(cx, |shell, _| {
        let id = shell.btw.as_ref().unwrap().side_chat;
        let tab = &shell.side_chats[&id];
        (tab.state.clone(), tab.transcript.clone())
    });
    let entry = |id: &str, role, text: &str| SessionMessageEntry {
        id: id.into(),
        role,
        parts: vec![MessagePart::Text {
            id: format!("{id}-text"),
            text: text.into(),
        }],
        created_at: 0,
        device_id: "dev".into(),
        status: Some(MessageStatus::Complete),
        continuation_of: None,
        duration_ms: None,
    };
    let reply = (1..=4)
        .map(|n| format!("Point {n}: the side chat's reply wraps across the narrow pane."))
        .collect::<Vec<_>>()
        .join("\n\n");
    side.update(cx, |state, cx| {
        state.transcript_replayed = true;
        state.transcript = vec![
            entry("q", MessageRole::User, "What does this function do?"),
            entry("r", MessageRole::Assistant, &reply),
        ];
        state.transcript_revision += 1;
        cx.notify();
    });
    settle(cx);

    let pane = cx.debug_bounds("session-btw").expect("the btw pane");
    let bar = cx.debug_bounds("session-btw-bar").expect("its prompt bar");
    let content = transcript.read_with(cx, |transcript, _| transcript.content_height());
    assert!(content > 150.0, "the reply was laid out: {content}");
    assert!(pane.size.height > empty.size.height + px(100.0));
    // The whole thread fits between the header and the bar: nothing to scroll.
    let room = f32::from(bar.origin.y - pane.origin.y) - 34.0 - 2.0;
    assert!(content <= room + 1.0, "content {content} in {room}");
    assert!(transcript.read_with(cx, |transcript, _| transcript.distance_from_bottom()) < 1.0);
    // Still the bar at the bottom, inside.
    assert!(bar.bottom() <= pane.bottom() + px(0.5));

    // A thread taller than the window stops the pane at the window's room:
    // the bar stays on screen and the thread scrolls.
    let long = (1..=60)
        .map(|n| format!("Line {n} of a reply far taller than the window."))
        .collect::<Vec<_>>()
        .join("\n\n");
    side.update(cx, |state, cx| {
        state.transcript = vec![
            entry("q", MessageRole::User, "What does this function do?"),
            entry("r", MessageRole::Assistant, &long),
        ];
        state.transcript_revision += 1;
        cx.notify();
    });
    settle(cx);
    let pane = cx.debug_bounds("session-btw").expect("the btw pane");
    let bar = cx.debug_bounds("session-btw-bar").expect("its prompt bar");
    assert!(
        f32::from(pane.bottom()) <= 900.0,
        "pane ends at {:?}",
        pane.bottom()
    );
    assert!(bar.bottom() <= pane.bottom() + px(0.5));
    let content = transcript.read_with(cx, |transcript, _| transcript.content_height());
    let room = f32::from(bar.origin.y - pane.origin.y) - 34.0 - 2.0;
    assert!(
        content > room,
        "the long reply scrolls: {content} in {room}"
    );
}
