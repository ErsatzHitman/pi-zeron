//! The chat context menu: a session in an orchestrator's tree has a parent
//! too, but keeps the menu of a session of its own. Without booting an engine.
use super::*;
use gpui::{AppContext, TestAppContext, VisualTestContext};

struct MenuHost(Entity<Shell>);

impl Render for MenuHost {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.0.update(cx, |shell, cx| {
            let overlays = shell.render_overlays(window.viewport_size(), window, cx);
            div()
                .size_full()
                .child(shell.render_chat_sidebar(&Theme::default(), cx))
                .children(overlays)
        })
    }
}

fn setup(cx: &mut TestAppContext) -> (Entity<Shell>, &mut VisualTestContext) {
    let dir = tempfile::tempdir().unwrap();
    cx.update(|cx| {
        gpui_base::init(cx);
        cx.set_global(Theme::default());
        crate::app_menus::init(cx);
        settings::init(UiSettings::default(), dir.path(), cx);
        crate::history::init(
            Default::default(),
            Default::default(),
            Default::default(),
            Default::default(),
            cx,
        );
        apply_keymap(
            cx,
            &KeymapConfig::default(),
            ComposerSendBehavior::default(),
        );
    });
    let (host, cx) = cx.add_window_view(|_, cx| {
        MenuHost(cx.new(|cx| {
            let state = cx.new(|_| AppState::new());
            let mut shell = Shell::new(
                state,
                EngineBootConfig {
                    data_dir: dir.path().into(),
                    ipc_port: 0,
                    edge_url: "http://127.0.0.1:1".into(),
                    edge_token: None,
                    org_id: None,
                    workos_client_id: None,
                    default_harness: zeron_proto::HarnessId::Mock,
                },
                cx,
            );
            shell.settings.sidebar_organization = SidebarOrganization::InOneList;
            shell.reduced_motion = true;
            shell.state.update(cx, |state, _| {
                state.workspace_scope = Some(WorkspaceScope::Local);
                state.local_device_id = Some("local".into());
                state.no_project = true;
                state.chats = ["orchestrator-1", "worker", "older", "aside"]
                    .into_iter()
                    .enumerate()
                    .map(|(ix, id)| {
                        let parent = match id {
                            "worker" => Some("orchestrator-1"),
                            "aside" => Some("older"),
                            _ => None,
                        };
                        serde_json::from_value(serde_json::json!({
                            "id": id, "title": id, "deviceId": "local", "archived": false,
                            "parentChatId": parent,
                            "createdAt": Utc::now() - chrono::Duration::minutes(10 - ix as i64),
                        }))
                        .unwrap()
                    })
                    .collect();
            });
            shell
        }))
    });
    let shell = host.read_with(cx, |host, _| host.0.clone());
    cx.update(|window, cx| {
        window.activate_window();
        window.draw(cx).clear();
    });
    (shell, cx)
}

fn redraw(cx: &mut VisualTestContext) {
    cx.update(|window, cx| {
        window.refresh();
        window.draw(cx).clear();
    });
}

fn open_menu(shell: &Entity<Shell>, cx: &mut VisualTestContext, chat_id: &str) {
    shell.update(cx, |shell, cx| {
        shell.chat_menu.open(ChatMenuState {
            tab: None,
            chat_id: chat_id.into(),
            position: gpui::point(px(20.0), px(20.0)),
            page: ChatMenuPage::Root,
        });
        cx.notify();
    });
    redraw(cx);
}

#[gpui::test]
fn a_tree_sessions_menu_keeps_pin_archive_and_copy(cx: &mut TestAppContext) {
    let (shell, cx) = setup(cx);

    // A session an orchestrator started is a session of its own: no "Open as
    // session" row, and the rows only top-level sessions get.
    open_menu(&shell, cx, "worker");
    assert!(cx.debug_bounds("chat-menu-open").is_none());
    for row in ["chat-menu-pin", "chat-menu-archive"] {
        assert!(cx.debug_bounds(row).is_some(), "{row}");
    }

    // A side chat keeps its own menu.
    open_menu(&shell, cx, "aside");
    assert!(cx.debug_bounds("chat-menu-open").is_some());
    assert!(cx.debug_bounds("chat-menu-pin").is_none());
    assert!(cx.debug_bounds("chat-menu-archive").is_none());
}
