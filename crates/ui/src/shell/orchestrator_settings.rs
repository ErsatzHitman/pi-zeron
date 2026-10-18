//! An orchestrator's settings dialog: its name, the goal and standing
//! instructions every session of its tree gets, the memory it keeps between
//! turns, the rules every session of the tree is held to (where tests run,
//! heavy-command and writer limits, allowed models, a spending cap), and the
//! controls that pause or archive the whole tree
//! (`zeron_proto::orchestrator_project`).

use super::*;
use std::collections::{HashMap, HashSet};
use zeron_proto::orchestrator;
use zeron_proto::orchestrator_project::{MemoryNote, OrchestratorProject, TreePolicy, dollars};

pub(super) struct OrchestratorSettingsDialog {
    root_id: String,
    name: Entity<ComposerInput>,
    goal: Entity<ComposerInput>,
    instructions: Entity<ComposerInput>,
    heavy: Entity<ComposerInput>,
    writers: Entity<ComposerInput>,
    models: Entity<ComposerInput>,
    cap: Entity<ComposerInput>,
    /// The policy as loaded: what the form leaves out (the warning
    /// percentage) is saved unchanged.
    policy: TreePolicy,
    original_name: String,
    memory: Vec<MemoryNote>,
    loaded: bool,
    saving: bool,
    error: Option<String>,
    focus_pending: bool,
    _load: Option<Task<()>>,
    _events: Vec<Subscription>,
}

/// The policy the dialog's form describes, or what to fix in it.
fn policy_from_form(
    dialog: &OrchestratorSettingsDialog,
    cx: &mut Context<Shell>,
) -> Result<TreePolicy, String> {
    let limit = |input: &Entity<ComposerInput>, what: &str, cx: &mut Context<Shell>| {
        let text = input.read(cx).text().trim().to_owned();
        if text.is_empty() {
            return Ok(None);
        }
        text.parse::<u8>()
            .map(Some)
            .map_err(|_| format!("{what}: enter a whole number, or leave it empty for no limit"))
    };
    let heavy_command_limit = limit(&dialog.heavy, "Heavy commands", cx)?;
    let writer_limit = limit(&dialog.writers, "Writing sessions", cx)?;
    let cap_text = dialog
        .cap
        .read(cx)
        .text()
        .trim()
        .trim_start_matches('$')
        .to_owned();
    let spend_cap_cents = if cap_text.is_empty() {
        None
    } else {
        let dollars: f64 = cap_text
            .parse()
            .map_err(|_| "Spending cap: enter dollars such as 25 or 12.50".to_owned())?;
        if !dollars.is_finite() || dollars <= 0.0 {
            return Err("Spending cap: enter more than 0 dollars".into());
        }
        Some((dollars * 100.0).round() as u64)
    };
    let allowed_models = dialog
        .models
        .read(cx)
        .text()
        .split([',', '\n'])
        .map(|model| model.trim().to_owned())
        .filter(|model| !model.is_empty())
        .collect();
    Ok(TreePolicy {
        heavy_command_limit,
        writer_limit,
        allowed_models,
        spend_cap_cents,
        ..dialog.policy.clone()
    })
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
        let rule = |placeholder: &'static str, cx: &mut Context<Self>| {
            cx.new(|cx| ComposerInput::new(placeholder, cx).with_single_line())
        };
        let heavy = rule("No limit", cx);
        let writers = rule("No limit", cx);
        let models = rule("Any model, or ids separated by commas", cx);
        let cap = rule("No cap, or dollars such as 25", cx);
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
                        let policy = &reply.project.policy;
                        let text = |value: Option<String>| value.unwrap_or_default();
                        dialog.heavy.update(cx, |input, cx| {
                            input.set_text(
                                text(policy.heavy_command_limit.map(|n| n.to_string())),
                                cx,
                            )
                        });
                        dialog.writers.update(cx, |input, cx| {
                            input.set_text(text(policy.writer_limit.map(|n| n.to_string())), cx)
                        });
                        dialog.models.update(cx, |input, cx| {
                            input.set_text(policy.allowed_models.join(", "), cx)
                        });
                        dialog.cap.update(cx, |input, cx| {
                            input.set_text(
                                text(policy.spend_cap_cents.map(|c| dollars(c).replace('$', ""))),
                                cx,
                            )
                        });
                        dialog.policy = policy.clone();
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
            heavy,
            writers,
            models,
            cap,
            policy: TreePolicy::default(),
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
        let policy = match policy_from_form(dialog, cx) {
            Ok(policy) => policy,
            Err(error) => {
                dialog.error = Some(error);
                cx.notify();
                return;
            }
        };
        let update = serde_json::json!({
            "chatId": root_id,
            "update": {
                "goal": dialog.goal.read(cx).text(),
                "instructions": dialog.instructions.read(cx).text(),
                "policy": policy,
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

    /// Flip one of the rules that is a plain switch.
    fn toggle_orchestrator_rule(&mut self, rule: &'static str, cx: &mut Context<Self>) {
        if let Some(dialog) = self.orchestrator_settings.as_mut() {
            match rule {
                "github-only-tests" => {
                    dialog.policy.github_only_tests = !dialog.policy.github_only_tests
                }
                _ => dialog.policy.require_preflight = !dialog.policy.require_preflight,
            }
            cx.notify();
        }
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

    /// Stop `root_id`'s whole tree for good: every session's process with its
    /// background jobs and sub-agents, every queued message, and hold the tree
    /// so nothing restarts. Sessions on other devices are stopped there.
    pub(super) fn stop_orchestrator(&mut self, root_id: String, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            self.show_notice(crate::toast::ToastKind::Error, "Engine not connected", cx);
            cx.notify();
            return;
        };
        self.show_notice(
            crate::toast::ToastKind::Info,
            "Stopping every session of the orchestrator…",
            cx,
        );
        cx.notify();
        cx.spawn(async move |this, cx| {
            let params = serde_json::json!({ "chatId": root_id, "scope": "tree", "hold": true });
            let reply = engine
                .client()
                .call(methods::STOP_TREE, params.clone())
                .await;
            let reply = match reply {
                Ok(mut report) => {
                    for (device, chats) in remote_stop_targets(&report) {
                        let mut follow_up = params.clone();
                        follow_up["only"] = serde_json::json!(chats);
                        follow_up["targetDeviceId"] = serde_json::json!(device);
                        match engine.client().call(methods::STOP_TREE, follow_up).await {
                            Ok(more) => fold_stop_report(&mut report, more),
                            Err(err) => {
                                for chat in &chats {
                                    fold_stop_report(
                                        &mut report,
                                        serde_json::json!({ "remaining": [{
                                            "chatId": chat,
                                            "reason": format!("device {device} unreachable: {err}"),
                                        }] }),
                                    );
                                }
                            }
                        }
                    }
                    report["remote"] = serde_json::json!([]);
                    Ok(report)
                }
                Err(err) => Err(err),
            };
            this.update(cx, |shell, cx| {
                match reply {
                    Ok(report) => {
                        if report["held"] == true
                            && let Some(project) =
                                shell.orchestrator_projects.known.get_mut(&root_id)
                        {
                            project.paused = true;
                        }
                        let kind = if report["remaining"]
                            .as_array()
                            .is_some_and(|remaining| !remaining.is_empty())
                        {
                            crate::toast::ToastKind::Error
                        } else {
                            crate::toast::ToastKind::Info
                        };
                        shell.show_notice(kind, stop_notice(&report), cx);
                    }
                    Err(err) => shell.show_notice(
                        crate::toast::ToastKind::Error,
                        format!("Could not stop the orchestrator: {err}"),
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
        let (heavy, writers, models, cap) = (
            dialog.heavy.clone(),
            dialog.writers.clone(),
            dialog.models.clone(),
            dialog.cap.clone(),
        );
        let (github_only, preflight) = (
            dialog.policy.github_only_tests,
            dialog.policy.require_preflight,
        );
        let switch = |title: &'static str,
                      hint: &'static str,
                      on: bool,
                      rule: &'static str,
                      cx: &mut Context<Self>| {
            div()
                .id(SharedString::from(format!("orchestrator-rule-{rule}")))
                .mt(px(8.0))
                .flex()
                .flex_row()
                .items_center()
                .gap(px(10.0))
                .cursor_pointer()
                .role(gpui::Role::Button)
                .on_click(
                    cx.listener(move |this, _, _, cx| this.toggle_orchestrator_rule(rule, cx)),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .flex_col()
                        .child(
                            div()
                                .text_size(crate::typography::ui_rems(12.5))
                                .text_color(theme.text)
                                .child(SharedString::from(title)),
                        )
                        .child(
                            div()
                                .text_size(crate::typography::ui_rems(11.5))
                                .text_color(theme.text_faint)
                                .child(SharedString::from(hint)),
                        ),
                )
                .child(crate::settings::widgets::toggle_switch(
                    &theme,
                    on,
                    format!("orchestrator-rule-{rule}"),
                ))
        };
        let github_switch = switch(
            "Build and test on GitHub only",
            "Compilers and test runners are refused in its sessions; they push and read CI",
            github_only,
            "github-only-tests",
            cx,
        );
        let preflight_switch = switch(
            "Check GitHub before a writer starts",
            "Push access, token scopes, CI triggers and runners must pass first",
            preflight,
            "require-preflight",
            cx,
        );
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
        let stop_root = root_id.clone();
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
            .child(label("Rules for every session"))
            .child(github_switch)
            .child(preflight_switch)
            .child(label("Heavy commands at once (1-8)"))
            .child(popover::dialog_field(heavy.into_any_element()))
            .child(label("Writing sessions at once (1-16)"))
            .child(popover::dialog_field(writers.into_any_element()))
            .child(label("Allowed models"))
            .child(popover::dialog_field(models.into_any_element()))
            .child(label("Spending cap in dollars, all sessions together"))
            .child(popover::dialog_field(cap.into_any_element()))
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
                        popover::btn_danger(&theme, "Stop all")
                            .id("orchestrator-stop-all")
                            .tooltip(crate::settings::widgets::text_tooltip(
                                "Stop every session, background job and queued message, and hold the tree",
                            ))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.stop_orchestrator(stop_root.clone(), cx)
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

/// What a stop report says, for the toast.
fn stop_notice(report: &serde_json::Value) -> String {
    let stopped = report["stopped"].as_array().map_or(0, Vec::len);
    let cleared = report["queuedCleared"].as_u64().unwrap_or(0);
    let remaining = report["remaining"].as_array().map_or(0, Vec::len);
    let mut text = match stopped {
        0 => "Nothing was running".to_owned(),
        1 => "Stopped 1 session".to_owned(),
        n => format!("Stopped {n} sessions"),
    };
    if cleared > 0 {
        text.push_str(&format!(
            ", cleared {cleared} queued message{}",
            if cleared == 1 { "" } else { "s" }
        ));
    }
    if remaining > 0 {
        text.push_str(&format!(". Still running: {remaining}"));
    }
    if report["held"] == true {
        text.push_str(". The orchestrator is held until you resume it");
    }
    text
}

/// The sessions a stop reported as hosted elsewhere, by device.
fn remote_stop_targets(report: &serde_json::Value) -> Vec<(String, Vec<String>)> {
    let mut by_device: Vec<(String, Vec<String>)> = Vec::new();
    for remote in report["remote"].as_array().into_iter().flatten() {
        let (Some(chat), Some(device)) = (remote["chatId"].as_str(), remote["deviceId"].as_str())
        else {
            continue;
        };
        match by_device.iter_mut().find(|(known, _)| known == device) {
            Some((_, chats)) => chats.push(chat.to_owned()),
            None => by_device.push((device.to_owned(), vec![chat.to_owned()])),
        }
    }
    by_device
}

/// Add the report of a follow-up stop on another device to `report`.
fn fold_stop_report(report: &mut serde_json::Value, more: serde_json::Value) {
    for key in ["stopped", "alreadyIdle", "remaining"] {
        let Some(items) = more[key].as_array() else {
            continue;
        };
        if let Some(list) = report[key].as_array_mut() {
            list.extend(items.iter().cloned());
        } else {
            report[key] = serde_json::Value::Array(items.clone());
        }
    }
    let cleared =
        report["queuedCleared"].as_u64().unwrap_or(0) + more["queuedCleared"].as_u64().unwrap_or(0);
    report["queuedCleared"] = serde_json::json!(cleared);
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_stop_toast_says_what_stopped_and_what_did_not() {
        let report = json!({
            "stopped": [{"chatId": "a"}, {"chatId": "b"}], "queuedCleared": 3,
            "remaining": [], "held": true,
        });
        assert_eq!(
            stop_notice(&report),
            "Stopped 2 sessions, cleared 3 queued messages. The orchestrator is held until you resume it"
        );
        let stuck = json!({
            "stopped": [{"chatId": "a"}], "queuedCleared": 1,
            "remaining": [{"chatId": "b", "reason": "its runtime is still shutting down"}],
            "held": false,
        });
        assert_eq!(
            stop_notice(&stuck),
            "Stopped 1 session, cleared 1 queued message. Still running: 1"
        );
        assert_eq!(
            stop_notice(&json!({"stopped": [], "remaining": []})),
            "Nothing was running"
        );
    }

    #[test]
    fn remote_sessions_are_grouped_by_device_and_their_reports_folded_in() {
        let mut report = json!({
            "stopped": [{"chatId": "a"}], "queuedCleared": 2, "remaining": [],
            "remote": [
                {"chatId": "x", "deviceId": "d1"},
                {"chatId": "y", "deviceId": "d2"},
                {"chatId": "z", "deviceId": "d1"},
            ],
        });
        assert_eq!(
            remote_stop_targets(&report),
            vec![
                ("d1".to_owned(), vec!["x".to_owned(), "z".to_owned()]),
                ("d2".to_owned(), vec!["y".to_owned()]),
            ]
        );
        fold_stop_report(
            &mut report,
            json!({"stopped": [{"chatId": "x"}], "queuedCleared": 1}),
        );
        assert_eq!(report["stopped"].as_array().unwrap().len(), 2);
        assert_eq!(report["queuedCleared"], 3);
        fold_stop_report(
            &mut report,
            json!({"remaining": [{"chatId": "y", "reason": "unreachable"}]}),
        );
        assert_eq!(report["remaining"].as_array().unwrap().len(), 1);
    }
}
