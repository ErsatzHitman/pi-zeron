//! An orchestrator's settings dialog: its name, the goal and standing
//! instructions every session of its tree gets, the memory it keeps between
//! turns, and the controls that pause or archive the whole tree
//! (`zeron_proto::orchestrator_project`).

use super::*;
use std::collections::{HashMap, HashSet};
use zeron_proto::orchestrator;
use zeron_proto::orchestrator_project::{MemoryNote, OrchestratorProject};

pub(super) struct OrchestratorSettingsDialog {
    root_id: String,
    name: Entity<ComposerInput>,
    goal: Entity<ComposerInput>,
    instructions: Entity<ComposerInput>,
    original_name: String,
    memory: Vec<MemoryNote>,
    loaded: bool,
    saving: bool,
    error: Option<String>,
    focus_pending: bool,
    _load: Option<Task<()>>,
    _events: Vec<Subscription>,
}

/// What the sidebar knows of each orchestrator's project, filled as the
/// Orchestrator tab shows a root and whenever this window changes one.
#[derive(Default)]
pub(super) struct OrchestratorProjects {
    pub known: HashMap<String, OrchestratorProject>,
    loading: HashSet<String>,
    _tasks: Vec<Task<()>>,
}

impl OrchestratorProjects {
    pub(super) fn paused(&self, root_id: &str) -> bool {
        self.known
            .get(root_id)
            .is_some_and(|project| project.paused)
    }
}

#[derive(serde::Deserialize)]
struct ProjectReply {
    project: OrchestratorProject,
    #[serde(default)]
    memory: Vec<MemoryNote>,
}

impl Shell {
    /// Load the projects of `roots` this window has not read yet.
    pub(super) fn ensure_orchestrator_projects(
        &mut self,
        roots: &[String],
        cx: &mut Context<Self>,
    ) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let missing: Vec<String> = roots
            .iter()
            .filter(|root| {
                !self.orchestrator_projects.known.contains_key(*root)
                    && !self.orchestrator_projects.loading.contains(*root)
            })
            .cloned()
            .collect();
        if missing.is_empty() {
            return;
        }
        self.orchestrator_projects
            .loading
            .extend(missing.iter().cloned());
        // Finished loads leave their tasks behind; drop them with each batch.
        self.orchestrator_projects._tasks.clear();
        self.orchestrator_projects
            ._tasks
            .push(cx.spawn(async move |this, cx| {
                for root in missing {
                    let reply = engine
                        .client()
                        .call_as::<ProjectReply>(
                            methods::GET_ORCHESTRATOR_PROJECT,
                            serde_json::json!({ "chatId": root }),
                        )
                        .await;
                    this.update(cx, |shell, cx| {
                        shell.orchestrator_projects.loading.remove(&root);
                        // An engine without projects reads as an empty one.
                        let project = reply.map(|reply| reply.project).unwrap_or_else(|_| {
                            OrchestratorProject {
                                root_id: root.clone(),
                                ..Default::default()
                            }
                        });
                        shell.orchestrator_projects.known.insert(root, project);
                        cx.notify();
                    })
                    .ok();
                }
            }));
    }

    pub(super) fn open_orchestrator_settings(&mut self, root_id: String, cx: &mut Context<Self>) {
        self.close_chat_menu(cx);
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            self.show_notice(crate::toast::ToastKind::Error, "Engine not connected", cx);
            cx.notify();
            return;
        };
        let original_name = self
            .state
            .read(cx)
            .chats
            .iter()
            .find(|chat| chat.id == root_id)
            .and_then(|chat| chat.title.clone())
            .unwrap_or_else(|| orchestrator::ROOT_CHAT_TITLE.into());
        let name = cx.new(|cx| ComposerInput::new("Name", cx).with_single_line());
        name.update(cx, |input, cx| input.set_text(original_name.clone(), cx));
        let goal = cx.new(|cx| {
            ComposerInput::new("What this orchestrator is for, in one line", cx).with_single_line()
        });
        let instructions = cx.new(|cx| {
            ComposerInput::new(
                "Standing instructions for every session: conventions, constraints, how to verify",
                cx,
            )
        });
        let events = [&name, &goal]
            .into_iter()
            .map(|input| {
                cx.subscribe(input, |this: &mut Shell, _, event, cx| {
                    if matches!(event, ComposerInputEvent::Submitted) {
                        this.submit_orchestrator_settings(cx);
                    }
                })
            })
            .collect();
        let load_root = root_id.clone();
        let load = cx.spawn(async move |this, cx| {
            let reply = engine
                .client()
                .call_as::<ProjectReply>(
                    methods::GET_ORCHESTRATOR_PROJECT,
                    serde_json::json!({ "chatId": load_root }),
                )
                .await;
            this.update(cx, |shell, cx| {
                let Some(dialog) = shell
                    .orchestrator_settings
                    .as_mut()
                    .filter(|dialog| dialog.root_id == load_root)
                else {
                    return;
                };
                match reply {
                    Ok(reply) => {
                        dialog.goal.update(cx, |input, cx| {
                            input.set_text(reply.project.goal.clone(), cx)
                        });
                        dialog.instructions.update(cx, |input, cx| {
                            input.set_text(reply.project.instructions.clone(), cx)
                        });
                        dialog.memory = reply.memory;
                        dialog.loaded = true;
                        shell
                            .orchestrator_projects
                            .known
                            .insert(load_root, reply.project);
                    }
                    Err(err) => dialog.error = Some(format!("Could not load the settings: {err}")),
                }
                cx.notify();
            })
            .ok();
        });
        self.orchestrator_settings = Some(OrchestratorSettingsDialog {
            root_id,
            name,
            goal,
            instructions,
            original_name,
            memory: Vec::new(),
            loaded: false,
            saving: false,
            error: None,
            focus_pending: true,
            _load: Some(load),
            _events: events,
        });
        cx.notify();
    }

    pub(super) fn submit_orchestrator_settings(&mut self, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let Some(dialog) = self.orchestrator_settings.as_mut() else {
            return;
        };
        if !dialog.loaded || dialog.saving {
            return;
        }
        let root_id = dialog.root_id.clone();
        let name = dialog.name.read(cx).text().trim().to_owned();
        let rename = (!name.is_empty() && name != dialog.original_name)
            .then(|| serde_json::json!({ "op": "renameChat", "chatId": root_id, "title": name }));
        let update = serde_json::json!({
            "chatId": root_id,
            "update": {
                "goal": dialog.goal.read(cx).text(),
                "instructions": dialog.instructions.read(cx).text(),
            },
        });
        dialog.saving = true;
        dialog.error = None;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let client = engine.client();
            let saved = async {
                if let Some(rename) = rename {
                    client.call(methods::MUTATE, rename).await?;
                }
                client
                    .call_as::<ProjectReply>(methods::UPDATE_ORCHESTRATOR_PROJECT, update)
                    .await
            }
            .await;
            this.update(cx, |shell, cx| {
                match saved {
                    Ok(reply) => {
                        shell
                            .orchestrator_projects
                            .known
                            .insert(root_id, reply.project);
                        shell.orchestrator_settings = None;
                    }
                    Err(err) => {
                        if let Some(dialog) = shell.orchestrator_settings.as_mut() {
                            dialog.saving = false;
                            dialog.error = Some(err.to_string());
                        }
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn forget_orchestrator_note(&mut self, note_id: String, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let Some(dialog) = self.orchestrator_settings.as_mut() else {
            return;
        };
        dialog.memory.retain(|note| note.id != note_id);
        let params = serde_json::json!({ "chatId": dialog.root_id, "forget": note_id });
        cx.notify();
        cx.spawn(async move |this, cx| {
            if let Err(err) = engine
                .client()
                .call(methods::ORCHESTRATOR_MEMORY, params)
                .await
            {
                this.update(cx, |shell, cx| {
                    shell.show_notice(crate::toast::ToastKind::Error, format!("{err}"), cx);
                    cx.notify();
                })
                .ok();
            }
        })
        .detach();
    }

    /// Pause, resume or archive `root_id`'s whole tree.
    pub(super) fn control_orchestrator(
        &mut self,
        root_id: String,
        action: &'static str,
        cx: &mut Context<Self>,
    ) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            self.show_notice(crate::toast::ToastKind::Error, "Engine not connected", cx);
            cx.notify();
            return;
        };
        if action == "archive" {
            self.orchestrator_settings = None;
        }
        cx.notify();
        cx.spawn(async move |this, cx| {
            let reply = engine
                .client()
                .call(
                    methods::CONTROL_ORCHESTRATOR,
                    serde_json::json!({ "rootId": root_id, "action": action }),
                )
                .await;
            this.update(cx, |shell, cx| {
                match reply {
                    Ok(reply) => {
                        if let Ok(project) =
                            serde_json::from_value::<OrchestratorProject>(reply["project"].clone())
                        {
                            shell.orchestrator_projects.known.insert(root_id, project);
                        }
                        let notice = match action {
                            "pause" => Some(match reply["stopped"].as_u64().unwrap_or(0) {
                                0 => "Orchestrator paused".to_owned(),
                                1 => "Orchestrator paused; stopped 1 running session".to_owned(),
                                n => format!("Orchestrator paused; stopped {n} running sessions"),
                            }),
                            "resume" => Some("Orchestrator resumed".to_owned()),
                            "archive" => Some(format!(
                                "Archived the orchestrator and its {} sessions",
                                reply["archived"].as_u64().unwrap_or(1).saturating_sub(1)
                            )),
                            _ => None,
                        };
                        if let Some(notice) = notice {
                            shell.show_notice(crate::toast::ToastKind::Info, notice, cx);
                        }
                    }
                    Err(err) => shell.show_notice(
                        crate::toast::ToastKind::Error,
                        format!("Could not {action} the orchestrator: {err}"),
                        cx,
                    ),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    pub(super) fn render_orchestrator_settings_overlay(
        &mut self,
        viewport: gpui::Size<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let theme = Theme::of(cx).for_popup();
        let dialog = self.orchestrator_settings.as_mut()?;
        if std::mem::take(&mut dialog.focus_pending) {
            window.focus(&dialog.goal.focus_handle(cx), cx);
        }
        let root_id = dialog.root_id.clone();
        let paused = self.orchestrator_projects.paused(&root_id);
        let (name, goal, instructions) = (
            dialog.name.clone(),
            dialog.goal.clone(),
            dialog.instructions.clone(),
        );
        let (loaded, saving, error) = (dialog.loaded, dialog.saving, dialog.error.clone());
        let notes: Vec<MemoryNote> = dialog.memory.clone();
        let label = |text: &str| {
            div()
                .mt(px(14.0))
                .mb(px(6.0))
                .text_size(crate::typography::ui_rems(12.0))
                .text_color(theme.text_muted)
                .child(SharedString::from(text.to_owned()))
        };
        let memory = if notes.is_empty() {
            div()
                .text_size(crate::typography::ui_rems(12.0))
                .text_color(theme.text_faint)
                .child(SharedString::from(if loaded {
                    "Nothing yet. The orchestrator saves what later turns need to know here."
                } else {
                    "Loading…"
                }))
                .into_any_element()
        } else {
            div()
                .id("orchestrator-memory")
                .max_h(px(140.0))
                .overflow_y_scroll()
                .flex()
                .flex_col()
                .gap(px(4.0))
                .children(notes.into_iter().map(|note| {
                    let forget_id = note.id.clone();
                    div()
                        .flex()
                        .flex_row()
                        .items_start()
                        .gap(px(6.0))
                        .text_size(crate::typography::ui_rems(12.0))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .child(SharedString::from(note.text)),
                        )
                        .child(
                            div()
                                .id(SharedString::from(format!("forget-{}", note.id)))
                                .flex_none()
                                .cursor_pointer()
                                .role(gpui::Role::Button)
                                .tooltip(crate::settings::widgets::text_tooltip("Forget"))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.forget_orchestrator_note(forget_id.clone(), cx)
                                }))
                                .child(
                                    icon(icons::CLOSE)
                                        .size(px(14.0))
                                        .text_color(theme.text_faint),
                                ),
                        )
                }))
                .into_any_element()
        };
        let pause_root = root_id.clone();
        let archive_root = root_id.clone();
        let card = popover::dialog_card(&theme)
            .w(px(460.0))
            .on_key_down(cx.listener(|this, ev: &gpui::KeyDownEvent, _, cx| {
                if ev.keystroke.key == "escape" {
                    this.orchestrator_settings = None;
                    cx.notify();
                    cx.stop_propagation();
                }
            }))
            .child(popover::dialog_title(&theme, "Orchestrator settings"))
            .child(popover::dialog_body(
                &theme,
                "Every session this orchestrator starts gets its goal and instructions.",
            ))
            .child(label("Name"))
            .child(popover::dialog_field(name.into_any_element()))
            .child(label("Goal"))
            .child(popover::dialog_field(goal.into_any_element()))
            .child(label("Instructions"))
            .child(popover::dialog_field(
                div()
                    .id("orchestrator-instructions")
                    .min_h(px(72.0))
                    .max_h(px(180.0))
                    .overflow_y_scroll()
                    .child(instructions)
                    .into_any_element(),
            ))
            .child(label("Memory"))
            .child(memory)
            .when_some(error, |el, error| {
                el.child(
                    div()
                        .mt(px(12.0))
                        .text_size(crate::typography::ui_rems(12.0))
                        .text_color(theme.danger)
                        .child(SharedString::from(error)),
                )
            })
            .child(
                div()
                    .mt(px(18.0))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(8.0))
                    .child(
                        popover::btn_ghost(
                            &theme,
                            if paused { "Resume" } else { "Pause" },
                            "orchestrator-pause",
                        )
                        .id("orchestrator-pause")
                        .tooltip(crate::settings::widgets::text_tooltip(if paused {
                            "Let its sessions start and message work again"
                        } else {
                            "Stop every running session and hold new work"
                        }))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.control_orchestrator(
                                pause_root.clone(),
                                if paused { "resume" } else { "pause" },
                                cx,
                            )
                        })),
                    )
                    .child(
                        popover::btn_danger(&theme, "Archive all")
                            .id("orchestrator-archive")
                            .tooltip(crate::settings::widgets::text_tooltip(
                                "Stop and archive the orchestrator and every session it started",
                            ))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.control_orchestrator(archive_root.clone(), "archive", cx)
                            })),
                    )
                    .child(div().flex_1())
                    .child(
                        popover::btn_ghost(&theme, "Cancel", "orchestrator-settings-cancel")
                            .id("orchestrator-settings-cancel")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.orchestrator_settings = None;
                                cx.notify();
                            })),
                    )
                    .child(
                        popover::btn_primary(&theme, if saving { "Saving…" } else { "Save" })
                            .id("orchestrator-settings-save")
                            .when(!loaded || saving, |el| el.opacity(0.5))
                            .on_click(
                                cx.listener(|this, _, _, cx| this.submit_orchestrator_settings(cx)),
                            ),
                    ),
            )
            .into_any_element();
        Some(popover::modal(
            "orchestrator-settings-dialog",
            viewport,
            card,
        ))
    }
}
