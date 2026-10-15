//! The titlebar's machine switcher: This PC, or another computer's Zeron
//! engine over SSH (see [`crate::machines`]). Switching swaps the engine in
//! place, as an account change does: the sidebar, chats and projects become
//! that machine's. This PC's engine keeps running while another machine is
//! in view, so its agents carry on and switching back is instant.

use super::*;

/// The switcher's slot beside the sidebar view options: its button and the
/// 2px rhythm between them.
pub(super) const MACHINE_SLOT_WIDTH: f32 = 24.0 + TITLEBAR_CONTROL_GAP;

/// The switcher's tooltip and accessible name.
pub(super) fn machine_trigger_label(machine: Option<&str>) -> String {
    format!("Machine: {}", machine.unwrap_or("This PC"))
}

impl Shell {
    /// The machine in view, as its label; `None` is This PC.
    pub(super) fn active_machine_label(&self) -> Option<String> {
        let target = self.machine.as_deref()?;
        Some(
            self.settings
                .machines
                .iter()
                .find(|machine| machine.ssh == target)
                .map_or_else(|| crate::machines::default_name(target), |m| m.label()),
        )
    }

    /// Show `target`'s workspace (`None`: This PC's). Its engine replaces the
    /// one in view; a machine's opens through a fresh SSH tunnel.
    pub(super) fn switch_machine(&mut self, target: Option<String>, cx: &mut Context<Self>) {
        self.close_machine_menu(cx);
        if self.runtime_change_task.is_some() {
            self.show_notice(
                crate::toast::ToastKind::Info,
                "Wait for the account change to finish".into(),
                cx,
            );
            return;
        }
        let state = self.state.clone();
        let ready = matches!(state.read(cx).connection, ConnectionStatus::Ready);
        if target == self.machine && ready {
            return;
        }
        let leaving_this_pc = self.machine.is_none();
        // Detach before the tunnel closes, so nothing reads a dead socket.
        drop(state.update(cx, |s, cx| s.detach_for_machine_switch(leaving_this_pc, cx)));
        self.machine_tunnel = None;
        self.machine_task = None;
        self.machine = target.clone();
        self.space_boot_applied = false;
        if self.settings.active_machine != target {
            self.settings.active_machine = target.clone();
            self.schedule_save(cx);
        }
        match target {
            None => match state.update(cx, |s, _| s.take_parked_local()) {
                Some(handle) => AppState::adopt_engine(state, handle, cx),
                None => AppState::bootstrap(state, self.boot.clone(), cx),
            },
            Some(ssh) => {
                let open = Tokio::spawn(cx, crate::machines::open(ssh.clone()));
                self.machine_task = Some(cx.spawn(async move |this, cx| {
                    let outcome = match open.await {
                        Ok(outcome) => outcome,
                        Err(error) => Err(error.to_string()),
                    };
                    this.update(cx, |shell, cx| {
                        // Switched again meanwhile: this tunnel closes unused.
                        if shell.machine.as_deref() != Some(ssh.as_str()) {
                            return;
                        }
                        shell.machine_task = None;
                        match outcome {
                            Ok((tunnel, handle)) => {
                                shell.machine_tunnel = Some(tunnel);
                                AppState::adopt_engine(shell.state.clone(), handle, cx);
                            }
                            Err(message) => shell.state.update(cx, |s, cx| {
                                tracing::warn!(%message, "machine switch failed");
                                s.connection = ConnectionStatus::Failed(message);
                                cx.notify();
                            }),
                        }
                    })
                    .ok();
                }));
            }
        }
        cx.notify();
    }

    /// At launch, once This PC's engine is up: back to the machine this
    /// window drove last, if it is still listed.
    pub(super) fn restore_machine(&mut self, cx: &mut Context<Self>) {
        if self.machine_restored
            || !matches!(self.state.read(cx).connection, ConnectionStatus::Ready)
        {
            return;
        }
        self.machine_restored = true;
        let Some(target) = self.settings.active_machine.clone() else {
            return;
        };
        if self
            .settings
            .machines
            .iter()
            .any(|machine| machine.ssh == target)
        {
            self.switch_machine(Some(target), cx);
        } else {
            self.settings.active_machine = None;
            self.schedule_save(cx);
        }
    }

    pub(super) fn close_machine_menu(&mut self, cx: &mut Context<Self>) {
        if self.machine_menu.begin_close() {
            popover::reap_popup(cx, |shell: &mut Self| &mut shell.machine_menu);
            cx.notify();
        }
    }

    /// The switcher: a titlebar control beside the sidebar view options.
    /// This PC shows a monitor; another machine a server, in the accent.
    pub(super) fn render_machine_trigger(
        &mut self,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let open = self.machine_menu.is_open();
        let label = self.active_machine_label();
        let remote = label.is_some();
        let tooltip = machine_trigger_label(label.as_deref());
        let trigger = div()
            .id("titlebar-machine")
            .debug_selector(|| "titlebar-machine".into())
            .role(gpui::Role::Button)
            .aria_label(tooltip.clone())
            .aria_expanded(open)
            .size(px(24.0))
            .flex_none()
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(6.0))
            .cursor_pointer()
            .bg(if open {
                theme.glass_hover()
            } else {
                theme.glass_hover().opacity(0.0)
            })
            .hover(|el| el.bg(theme.glass_hover()))
            // Over the titlebar's drag strip: a click never starts a drag.
            .occlude()
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(|this, _, window, _| {
                    window.prevent_default();
                    this.machine_menu.note_trigger_press();
                }),
            )
            .on_click(cx.listener(|this, _, _, cx| {
                cx.stop_propagation();
                if this.machine_menu.take_press_was_open() {
                    this.close_machine_menu(cx);
                } else {
                    this.machine_menu.open(());
                    cx.notify();
                }
            }))
            .tooltip(settings::widgets::text_tooltip(tooltip))
            .child(
                icon(if remote {
                    icons::REMOTE_SERVER
                } else {
                    icons::MONITOR
                })
                .size(px(16.0))
                .text_color(if remote {
                    theme.accent
                } else {
                    theme.text_muted
                }),
            );
        if self.machine_menu.get().is_none() {
            return trigger.into_any_element();
        }
        let closing = self.machine_menu.closing_since();
        let menu = self.render_machine_menu(theme, cx);
        trigger
            .relative()
            .child(popover::anchored_menu_below(
                "titlebar-machine-menu",
                menu,
                closing,
            ))
            .into_any_element()
    }

    /// This PC, then each machine, the one in view checked; then Add.
    fn render_machine_menu(&mut self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let active = self.machine.clone();
        let check = |on: bool| {
            div().w(px(14.0)).flex_none().when(on, |el| {
                el.child(
                    icon(icons::CHECK)
                        .size(px(14.0))
                        .text_color(theme.text_muted),
                )
            })
        };
        let row = |id: String, glyph: &'static str, title: String, detail: Option<String>| {
            popover::menu_row(theme, false, id.clone())
                .id(SharedString::from(id.clone()))
                .debug_selector(move || id.clone())
                .child(
                    icon(glyph)
                        .size(px(16.0))
                        .flex_none()
                        .text_color(theme.text_muted),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .flex_col()
                        .child(div().truncate().child(SharedString::from(title)))
                        .children(detail.map(|detail| {
                            div()
                                .truncate()
                                .text_size(crate::typography::ui_rems(11.0))
                                .text_color(theme.text_muted)
                                .child(SharedString::from(detail))
                        })),
                )
        };
        let mut menu = popover::popover_card(theme)
            .w(px(240.0))
            .on_mouse_down_out(cx.listener(|this, _, _, cx| this.close_machine_menu(cx)))
            .flex()
            .flex_col()
            .gap(px(2.0))
            .child(
                div()
                    .px(px(8.0))
                    .pt(px(6.0))
                    .pb(px(4.0))
                    .text_size(crate::typography::ui_rems(11.0))
                    .text_color(theme.text_muted)
                    .child(SharedString::from("Machines")),
            )
            .child(
                row(
                    "machine-this-pc".into(),
                    icons::MONITOR,
                    "This PC".into(),
                    None,
                )
                .child(check(active.is_none()))
                .on_click(cx.listener(|this, _, _, cx| this.switch_machine(None, cx))),
            );
        for (ix, machine) in self.settings.machines.iter().enumerate() {
            let target = machine.ssh.clone();
            let on = active.as_deref() == Some(target.as_str());
            let detail = (machine.label() != target).then(|| target.clone());
            menu =
                menu.child(
                    row(
                        format!("machine-{ix}"),
                        icons::REMOTE_SERVER,
                        machine.label(),
                        detail,
                    )
                    .child(check(on))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.switch_machine(Some(target.clone()), cx)
                    })),
                );
        }
        menu.child(popover::picker_divider())
            .child(
                row(
                    "machine-add".into(),
                    icons::PLUS,
                    "Add machine…".into(),
                    None,
                )
                .on_click(cx.listener(|this, _, _, cx| {
                    this.close_machine_menu(cx);
                    this.open_settings(SettingsSection::Devices, cx);
                })),
            )
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{AppContext as _, TestAppContext, VisualTestContext};

    fn window(
        cx: &mut TestAppContext,
        machines: Vec<settings::Machine>,
    ) -> (tempfile::TempDir, Entity<Shell>, &mut VisualTestContext) {
        let dir = tempfile::tempdir().unwrap();
        let ui = settings::UiSettings {
            machines,
            ..Default::default()
        };
        cx.update(|cx| {
            gpui_base::init(cx);
            cx.set_global(Theme::default());
            crate::app_menus::init(cx);
            crate::history::init(
                Default::default(),
                Default::default(),
                Default::default(),
                Default::default(),
                cx,
            );
            settings::init(ui, dir.path(), cx);
        });
        let data_dir = dir.path().to_path_buf();
        let (shell, cx) = cx.add_window_view(|_, cx| {
            let state = cx.new(|_| {
                let mut state = AppState::new();
                state.connection = ConnectionStatus::Ready;
                state
            });
            Shell::new(
                state,
                EngineBootConfig {
                    data_dir,
                    ipc_port: 0,
                    edge_url: "http://127.0.0.1:1".into(),
                    edge_token: None,
                    org_id: None,
                    workos_client_id: None,
                    default_harness: zeron_proto::HarnessId::Mock,
                },
                cx,
            )
        });
        shell.update(cx, |shell, cx| {
            shell.debug_gate = Some(GatePhase::Ready);
            cx.notify();
        });
        cx.simulate_resize(gpui::size(px(1400.0), px(800.0)));
        cx.run_until_parked();
        (dir, shell, cx)
    }

    #[gpui::test]
    fn the_switcher_rides_the_titlebar_and_lists_every_machine(cx: &mut TestAppContext) {
        let (_dir, shell, cx) = window(
            cx,
            vec![settings::Machine {
                ssh: "akshat@lab-box".into(),
                name: "Lab".into(),
            }],
        );
        let toggle = cx
            .debug_bounds("toggle-sidebar")
            .expect("the sidebar toggle");
        let machine = cx
            .debug_bounds("titlebar-machine")
            .expect("the machine switcher");
        // In the titlebar row, past the controls, at their size.
        assert!(machine.origin.x > toggle.origin.x + toggle.size.width);
        assert!((f32::from(machine.center().y) - f32::from(toggle.center().y)).abs() < 1.0);
        assert_eq!(machine.size, toggle.size);
        shell.update(cx, |shell, cx| {
            shell.machine_menu.open(());
            cx.notify();
        });
        cx.run_until_parked();
        let this_pc = cx.debug_bounds("machine-this-pc").expect("This PC");
        let lab = cx.debug_bounds("machine-0").expect("the listed machine");
        let add = cx.debug_bounds("machine-add").expect("Add machine");
        assert!(this_pc.origin.y < lab.origin.y && lab.origin.y < add.origin.y);
    }

    #[gpui::test]
    fn an_unreachable_machine_offers_this_pc(cx: &mut TestAppContext) {
        let (_dir, shell, cx) = window(cx, Vec::new());
        assert!(cx.debug_bounds("use-this-pc").is_none());
        shell.update(cx, |shell, cx| {
            shell.machine = Some("akshat@lab-box".into());
            shell.debug_gate = Some(GatePhase::Failed("Couldn't reach lab-box".into()));
            cx.notify();
        });
        cx.run_until_parked();
        assert!(cx.debug_bounds("use-this-pc").is_some());
        // Each machine keeps its own pins.
        let key = shell.read_with(cx, |shell, cx| shell.active_sidebar_pin_profile_key(cx));
        assert_eq!(key.as_deref(), Some("machine:akshat@lab-box"));
    }

    #[test]
    fn the_switcher_names_the_machine_in_view() {
        assert_eq!(machine_trigger_label(None), "Machine: This PC");
        assert_eq!(machine_trigger_label(Some("lab-box")), "Machine: lab-box");
    }
}
