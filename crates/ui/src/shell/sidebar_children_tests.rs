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
        retry: None,
        running_subagents: count,
        queue_position: 0,
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
fn the_btw_pane_opens_at_the_bottom_left_with_its_bar_inside(cx: &mut TestAppContext) {
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
    settle(cx);
    let pane = cx.debug_bounds("session-btw").expect("the btw pane");
    let bar = cx.debug_bounds("session-btw-bar").expect("its prompt bar");
    // Its own fixed size, in the window's bottom-left corner.
    assert_eq!(f32::from(pane.size.width), btw_pane::BTW_WIDTH);
    assert!((f32::from(pane.size.height) - btw_pane::BTW_HEIGHT).abs() <= 1.0);
    assert_eq!(f32::from(pane.origin.x), 12.0);
    let viewport = cx.update(|window, _| f32::from(window.viewport_size().height));
    // Within 3px: Windows lays the window out a pixel or two lower than Linux.
    assert!(
        (f32::from(pane.bottom()) - (viewport - 12.0)).abs() <= 3.0,
        "the pane ends at {:?} in a {viewport}px window",
        pane.bottom()
    );
    // The prompt bar is inside the pane, at its bottom, on one line.
    assert!(bar.origin.x >= pane.origin.x && bar.right() <= pane.right());
    assert!(bar.origin.y > pane.origin.y && bar.bottom() <= pane.bottom() + px(0.5));
    assert!(
        f32::from(bar.size.height) < 64.0,
        "the prompt bar is {:?} tall",
        bar.size.height
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

/// Within a pixel: layout rounds the pane's edges to device pixels, a pixel
/// apart between platforms and frames.
fn near(a: gpui::Point<Pixels>, b: gpui::Point<Pixels>) -> bool {
    (f32::from(a.x) - f32::from(b.x)).abs() <= 1.0 && (f32::from(a.y) - f32::from(b.y)).abs() <= 1.0
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
fn the_btw_pane_keeps_its_size_and_scrolls_a_long_reply(cx: &mut TestAppContext) {
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
    let long = (1..=60)
        .map(|n| format!("Line {n} of a reply far taller than the pane."))
        .collect::<Vec<_>>()
        .join("\n\n");
    side.update(cx, |state, cx| {
        state.transcript_replayed = true;
        state.transcript = vec![
            entry("q", MessageRole::User, "What does this function do?"),
            entry("r", MessageRole::Assistant, &long),
        ];
        state.transcript_revision += 1;
        cx.notify();
    });
    settle(cx);
    // The same pane: the reply scrolls inside it, the bar stays at its bottom.
    let pane = cx.debug_bounds("session-btw").expect("the btw pane");
    let bar = cx.debug_bounds("session-btw-bar").expect("its prompt bar");
    assert_eq!(pane.size, empty.size);
    assert!(
        near(pane.origin, empty.origin),
        "{pane:?} moved from {empty:?}"
    );
    assert!(bar.bottom() <= pane.bottom() + px(0.5));
    let content = transcript.read_with(cx, |transcript, _| transcript.content_height());
    let room = f32::from(bar.origin.y - pane.origin.y) - 34.0 - 2.0;
    assert!(
        content > room,
        "the long reply scrolls: {content} in {room}"
    );

    // A prompt of several lines grows the bar, inside the pane, instead of
    // clipping all but its first.
    let one_line = bar.size.height;
    cx.update(|_, cx| {
        shell.update(cx, |shell, cx| {
            shell.send_to_btw(
                "First line.
Second line.
Third line.",
                cx,
            )
        })
    });
    settle(cx);
    let pane = cx.debug_bounds("session-btw").expect("the btw pane");
    let bar = cx.debug_bounds("session-btw-bar").expect("its prompt bar");
    assert!(
        bar.size.height > one_line + px(30.0),
        "the bar grew from {one_line:?} to {:?}",
        bar.size.height
    );
    assert!(bar.origin.y > pane.origin.y && bar.bottom() <= pane.bottom() + px(0.5));
}

#[gpui::test]
fn the_btw_pane_drags_by_its_header_and_stays_in_the_window(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let (shell, cx) = open_btw(cx, dir.path());
    settle(cx);
    let before = cx.debug_bounds("session-btw").expect("the btw pane");
    let start = cx
        .debug_bounds("session-btw-header")
        .expect("its header")
        .center();
    let drag = |cx: &mut VisualTestContext, to: gpui::Point<Pixels>| {
        cx.simulate_mouse_down(start, MouseButton::Left, gpui::Modifiers::default());
        cx.simulate_mouse_move(to, Some(MouseButton::Left), gpui::Modifiers::default());
        cx.simulate_mouse_up(to, MouseButton::Left, gpui::Modifiers::default());
        settle(cx);
    };
    drag(cx, start + gpui::point(px(600.0), px(-300.0)));
    let after = cx.debug_bounds("session-btw").expect("the btw pane");
    let moved = before.origin + gpui::point(px(600.0), px(-300.0));
    assert!(
        near(after.origin, moved),
        "dragged to {:?}, not {moved:?}",
        after.origin
    );
    assert_eq!(after.size, before.size);
    // Flung past the window's corner, it stops inside it.
    let start = cx
        .debug_bounds("session-btw-header")
        .expect("its header")
        .center();
    cx.simulate_mouse_down(start, MouseButton::Left, gpui::Modifiers::default());
    cx.simulate_mouse_move(
        gpui::point(px(5000.0), px(-500.0)),
        Some(MouseButton::Left),
        gpui::Modifiers::default(),
    );
    cx.simulate_mouse_up(
        gpui::point(px(5000.0), px(-500.0)),
        MouseButton::Left,
        gpui::Modifiers::default(),
    );
    settle(cx);
    let flung = cx.debug_bounds("session-btw").expect("the btw pane");
    assert!(
        (f32::from(flung.right()) - (1600.0 - 12.0)).abs() <= 1.0,
        "{flung:?}"
    );
    assert!((f32::from(flung.origin.y) - 12.0).abs() <= 1.0, "{flung:?}");
    // Closed and opened again, it comes back where it was left.
    cx.update(|_, cx| {
        shell.update(cx, |shell, cx| {
            shell.close_btw_pane(cx);
        })
    });
    shell.update(cx, |shell, _| {
        *shell.test_time.as_mut().unwrap() += Duration::from_secs(3);
    });
    settle(cx);
    assert!(cx.debug_bounds("session-btw").is_none());
    cx.update(|_, cx| shell.update(cx, |shell, cx| shell.open_btw_pane(cx)));
    shell.update(cx, |shell, _| {
        *shell.test_time.as_mut().unwrap() += Duration::from_secs(3);
    });
    settle(cx);
    let reopened = cx.debug_bounds("session-btw").expect("the btw pane");
    assert!(
        near(reopened.origin, flung.origin),
        "{reopened:?} vs {flung:?}"
    );
}

#[gpui::test]
fn send_to_btw_opens_the_pane_with_the_text_quoted(cx: &mut TestAppContext) {
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
        shell.test_time = Some(std::time::Instant::now());
        shell
    });
    cx.simulate_resize(gpui::size(px(1600.0), px(900.0)));
    cx.update(|window, cx| window.draw(cx).clear());
    let prompt = |shell: &Entity<Shell>, cx: &mut VisualTestContext| {
        shell.read_with(cx, |shell, cx| {
            let pane = shell.btw.as_ref()?;
            let tab = shell.side_chats.get(&pane.side_chat)?;
            Some(tab.composer.read(cx).input.read(cx).text().to_owned())
        })
    };
    cx.update(|_, cx| {
        shell.update(cx, |shell, cx| {
            shell.send_to_btw("Trailing commas are dropped.\nBy design?", cx)
        })
    });
    assert_eq!(
        prompt(&shell, cx).as_deref(),
        Some("> Trailing commas are dropped.\n> By design?\n\n"),
        "the pane opens with the selection quoted"
    );
    // A second quote joins the same side chat's prompt, after the first.
    cx.update(|_, cx| shell.update(cx, |shell, cx| shell.send_to_btw("Also this.", cx)));
    assert_eq!(
        prompt(&shell, cx).as_deref(),
        Some("> Trailing commas are dropped.\n> By design?\n\n> Also this.\n\n")
    );
}
