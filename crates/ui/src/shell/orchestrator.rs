//! The sidebar's Orchestrator tab. A segmented control above the lists
//! switches between the sessions the user opens and the trees orchestrators
//! run (`zeron_proto::orchestrator`): each root, then the sessions it started
//! grouped by folder, with their own sessions nested under them. The
//! titlebar names where the open session sits in its tree, every level a
//! click away.

use super::*;
use crate::settings::SidebarTab;
use std::collections::HashSet;
use zeron_proto::{Chat, ChatIndicator, Space, orchestrator};

/// Indent of each nesting level; its rail runs down the middle of it.
pub(super) const TREE_INDENT: f32 = 14.0;

/// One folder's sessions under a root: its top sessions in start order, each
/// followed by the sessions it started, `level` deep.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct OrchestratorGroup {
    pub key: String,
    pub path: String,
    pub rows: Vec<(String, usize)>,
}

#[derive(Debug, Clone, PartialEq)]
pub(super) struct OrchestratorTree {
    pub root: String,
    pub groups: Vec<OrchestratorGroup>,
}

/// The caption under a root's title: where its sessions stand, what needs
/// the user first.
pub(super) fn tree_caption(statuses: &[ChatIndicator], paused: bool) -> String {
    let count = |want: ChatIndicator| statuses.iter().filter(|s| **s == want).count();
    let mut parts: Vec<String> = Vec::new();
    if paused {
        parts.push("Paused".into());
    }
    let waiting = count(ChatIndicator::AwaitingInput);
    if waiting > 0 {
        parts.push(format!(
            "{waiting} need{} you",
            if waiting == 1 { "s" } else { "" }
        ));
    }
    let failed = count(ChatIndicator::Errored);
    if failed > 0 {
        parts.push(format!("{failed} failed"));
    }
    let working = count(ChatIndicator::Working);
    if working > 0 {
        parts.push(format!("{working} working"));
    }
    match statuses.len() {
        0 if parts.is_empty() => parts.push(orchestrator::ROOT_CHAT_TITLE.into()),
        0 => {}
        1 => parts.push("1 session".into()),
        n => parts.push(format!("{n} sessions")),
    }
    parts.join(" · ")
}

/// A chat's folder: its cwd, else its project's.
fn chat_folder(chat: &Chat, spaces: &[Space]) -> String {
    chat.cwd
        .clone()
        .filter(|cwd| !cwd.trim().is_empty())
        .or_else(|| {
            let id = chat.space_id.as_deref()?;
            spaces
                .iter()
                .find(|space| space.id == id)
                .map(|space| space.path.clone())
        })
        .unwrap_or_default()
}

fn folder_key(path: &str) -> String {
    crate::pickers::folder_path_key(path.trim().trim_end_matches(['/', '\\']))
}

/// A folder's name in the tree: its project's, else its last component.
fn folder_label(path: &str, spaces: &[Space]) -> String {
    let key = folder_key(path);
    if key.is_empty() {
        return "No folder".into();
    }
    if let Some(space) = spaces.iter().find(|space| folder_key(&space.path) == key) {
        return space.display_name().to_string();
    }
    path.trim()
        .trim_end_matches(['/', '\\'])
        .rsplit(['/', '\\'])
        .next()
        .filter(|name| !name.is_empty())
        .unwrap_or(path)
        .to_string()
}

/// Every live orchestrator, newest first, with its sessions grouped by
/// folder. Folders with the latest activity lead, like the session list.
pub(super) fn orchestrator_trees(chats: &[Chat], spaces: &[Space]) -> Vec<OrchestratorTree> {
    let children = |parent: &str| -> Vec<&Chat> {
        let mut kids: Vec<&Chat> = chats
            .iter()
            .filter(|c| !c.archived && !c.is_btw() && c.parent_chat_id.as_deref() == Some(parent))
            .collect();
        kids.sort_by(|a, b| {
            a.created_at
                .cmp(&b.created_at)
                .then_with(|| a.id.cmp(&b.id))
        });
        kids
    };
    let activity = |id: &str| {
        chats
            .iter()
            .find(|c| c.id == id)
            .map(|c| c.last_message_at.unwrap_or(c.created_at))
    };
    let mut roots: Vec<&Chat> = chats
        .iter()
        .filter(|c| !c.archived && orchestrator::is_root_chat(&c.id))
        .collect();
    roots.sort_by(|a, b| b.created_at.cmp(&a.created_at));
    roots
        .into_iter()
        .map(|root| {
            let mut groups: Vec<OrchestratorGroup> = Vec::new();
            let mut seen: HashSet<&str> = HashSet::from([root.id.as_str()]);
            for child in children(&root.id) {
                let path = chat_folder(child, spaces);
                let key = folder_key(&path);
                let at = match groups.iter().position(|group| group.key == key) {
                    Some(at) => at,
                    None => {
                        groups.push(OrchestratorGroup {
                            key,
                            path,
                            rows: Vec::new(),
                        });
                        groups.len() - 1
                    }
                };
                let mut stack = vec![(child, 0)];
                while let Some((chat, level)) = stack.pop() {
                    if !seen.insert(chat.id.as_str()) {
                        continue;
                    }
                    groups[at].rows.push((chat.id.clone(), level));
                    stack.extend(
                        children(&chat.id)
                            .into_iter()
                            .rev()
                            .map(|kid| (kid, level + 1)),
                    );
                }
            }
            groups.sort_by_cached_key(|group| {
                std::cmp::Reverse(group.rows.iter().filter_map(|(id, _)| activity(id)).max())
            });
            OrchestratorTree {
                root: root.id.clone(),
                groups,
            }
        })
        .collect()
}

impl Shell {
    fn is_orchestrated(&self, chat_id: &str, cx: &App) -> bool {
        orchestrator::depth(&self.state.read(cx).chats, chat_id).is_some()
    }

    /// Show the tab `chat_id` lists in, so an orchestrated session opened
    /// from anywhere (the palette, a notification) is found in its tree.
    pub(super) fn follow_sidebar_tab(&mut self, chat_id: &str, cx: &mut Context<Self>) {
        let tab = if self.is_orchestrated(chat_id, cx) {
            SidebarTab::Orchestrator
        } else {
            SidebarTab::Normal
        };
        if self.settings.sidebar_tab != tab {
            self.settings.sidebar_tab = tab;
            self.schedule_save(cx);
        }
    }

    /// The segmented control above the sidebar's lists.
    pub(super) fn render_sidebar_tabs(
        &mut self,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let reduced = self.reduced_motion;
        let tray = div()
            .id("sidebar-tabs")
            .role(gpui::Role::TabList)
            .aria_label("Sidebar lists")
            .flex_1()
            .min_w_0()
            .h(px(32.0))
            .p(px(2.0))
            .rounded(px(8.0))
            .bg(crate::theme::ink(0.035))
            .flex()
            .items_center()
            .gap(px(2.0))
            .children(
                [
                    (SidebarTab::Normal, "Normal", "sidebar-tab-normal"),
                    (
                        SidebarTab::Orchestrator,
                        "Orchestrator",
                        "sidebar-tab-orchestrator",
                    ),
                ]
                .into_iter()
                .map(|(tab, label, id)| {
                    let chosen = self.settings.sidebar_tab == tab;
                    let selected = motion::state_t(id, chosen, motion::HOVER_FADE, reduced);
                    let hover_key = format!("{id}-hover");
                    let hover = motion::hover_t(&hover_key);
                    let fill = move || {
                        crate::theme::wash(0.12 * selected + 0.06 * hover * (1.0 - selected))
                    };
                    crate::surface_chrome::tab_frame(id, chosen, theme)
                        .role(gpui::Role::Tab)
                        .flex_1()
                        .min_w_0()
                        .justify_center()
                        .h(px(28.0))
                        .bg(fill())
                        .when(chosen, |el| {
                            el.shadow(crate::theme::card_selected_shadows())
                        })
                        .text_color(motion::mix(
                            theme.text_muted,
                            theme.text,
                            selected.max(hover),
                        ))
                        .on_hover(motion::hover_listener(hover_key))
                        .hover(move |style| style.bg(fill()))
                        .debug_selector(move || id.into())
                        .text_size(crate::typography::ui_rems(12.0))
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .px(px(12.0))
                        .child(div().truncate().child(label))
                        .on_click(cx.listener(move |this, _, _, cx| this.set_sidebar_tab(tab, cx)))
                }),
            );
        div()
            .flex_none()
            .flex()
            .px(px(Theme::SPACE_SM))
            .pt(px(8.0))
            .child(tray)
            .into_any_element()
    }

    /// Switch the sidebar's list. Each tab comes back to the session it
    /// showed last; the Orchestrator tab starts an orchestrator when there
    /// is none yet.
    pub(super) fn set_sidebar_tab(&mut self, tab: SidebarTab, cx: &mut Context<Self>) {
        if self.settings.sidebar_tab == tab {
            return;
        }
        let selected = self.state.read(cx).selected_chat.clone();
        let orchestrated = selected
            .as_deref()
            .is_some_and(|id| self.is_orchestrated(id, cx));
        if orchestrated {
            self.orchestrator_return = selected.clone();
        } else {
            self.normal_return = selected.clone();
        }
        self.cancel_sidebar_session_transfer(cx);
        self.cancel_pinned_session_drag(cx);
        self.settings.sidebar_tab = tab;
        self.schedule_save(cx);
        match tab {
            SidebarTab::Orchestrator if !orchestrated => {
                let target = {
                    let state = self.state.read(cx);
                    self.orchestrator_return
                        .clone()
                        .filter(|id| {
                            state.chats.iter().any(|c| c.id == *id && !c.archived)
                                && orchestrator::depth(&state.chats, id).is_some()
                        })
                        .or_else(|| {
                            orchestrator_trees(&state.chats, &state.spaces)
                                .first()
                                .map(|tree| tree.root.clone())
                        })
                };
                match target {
                    Some(id) => self.open_chat(id, cx),
                    None => self.start_orchestrator(cx),
                }
            }
            SidebarTab::Normal if orchestrated => {
                let target = self.normal_return.clone().filter(|id| {
                    self.state
                        .read(cx)
                        .visible_chats()
                        .any(|chat| chat.id == *id)
                });
                match target {
                    Some(id) => self.open_chat(id, cx),
                    None => self.open_new_session(None, cx),
                }
            }
            _ => {}
        }
        cx.notify();
    }

    /// A new orchestrator: a Pi chat on this device that runs its tasks in
    /// Pi sessions of their own (the harness gives it the instructions).
    pub(super) fn start_orchestrator(&mut self, cx: &mut Context<Self>) {
        let (engine, device) = {
            let state = self.state.read(cx);
            (state.engine().cloned(), state.local_device_id.clone())
        };
        let (Some(engine), Some(device)) = (engine, device) else {
            self.show_notice(crate::toast::ToastKind::Error, "Engine not connected", cx);
            cx.notify();
            return;
        };
        let chat_id = orchestrator::new_root_chat_id();
        let config = ChatConfig {
            harness: HarnessId::Pi,
            model: None,
            reasoning: None,
            model_options: Default::default(),
            sandbox: zeron_proto::SandboxLevel::WorkspaceWrite,
        };
        let create = serde_json::json!({
            "op": "createChat",
            "chatId": chat_id,
            "deviceId": device,
            // A folder of its own, like any session without a project.
            "cwd": zeron_proto::projectless::scratch_cwd(&chat_id, Utc::now()),
            "config": config,
        });
        let rename = serde_json::json!({
            "op": "renameChat",
            "chatId": chat_id,
            "title": orchestrator::ROOT_CHAT_TITLE,
        });
        self.orchestrator_task = Some(cx.spawn(async move |this, cx| {
            let client = engine.client();
            let created = match client.call(methods::MUTATE, create).await {
                Ok(_) => client.call(methods::MUTATE, rename).await.map(|_| ()),
                Err(err) => Err(err),
            };
            this.update(cx, |shell, cx| match created {
                // Its goal and instructions come first: every session gets them.
                Ok(()) => {
                    shell.open_chat(chat_id.clone(), cx);
                    shell.open_orchestrator_settings(chat_id, cx);
                }
                Err(err) => {
                    shell.show_notice(
                        crate::toast::ToastKind::Error,
                        format!("Could not start the orchestrator: {err}"),
                        cx,
                    );
                    cx.notify();
                }
            })
            .ok();
        }));
    }

    /// The Orchestrator tab's lists, in place of the project filter and the
    /// session lists.
    pub(super) fn render_orchestrator_lists(
        &mut self,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> (AnyElement, AnyElement) {
        let now = Utc::now();
        let trees = {
            let state = self.state.read(cx);
            orchestrator_trees(&state.chats, &state.spaces)
        };
        let row_height = sidebar_row_height(
            self.settings.sidebar_compact,
            self.settings.sidebar_show_project_label,
            false,
            false,
        );
        let roots: Vec<String> = trees.iter().map(|tree| tree.root.clone()).collect();
        self.ensure_orchestrator_projects(&roots, cx);
        let mut sections: Vec<AnyElement> = Vec::with_capacity(trees.len());
        for (ix, tree) in trees.iter().enumerate() {
            let caption = {
                let state = self.state.read(cx);
                let statuses: Vec<ChatIndicator> = tree
                    .groups
                    .iter()
                    .flat_map(|group| &group.rows)
                    .filter_map(|(id, _)| state.chats.iter().find(|chat| chat.id == *id))
                    .map(|chat| state.display_status_for(chat, now))
                    .collect();
                tree_caption(&statuses, self.orchestrator_projects.paused(&tree.root))
            };
            let root = self.render_orchestrator_row(&tree.root, 0, caption.into(), now, theme, cx);
            let groups: Vec<AnyElement> = tree
                .groups
                .iter()
                .map(|group| {
                    self.render_orchestrator_group(&tree.root, group, row_height, now, theme, cx)
                })
                .collect();
            let empty = groups.is_empty();
            sections.push(
                div()
                    .id(SharedString::from(format!("orchestrator-tree-{}", tree.root)))
                    .flex()
                    .flex_col()
                    .when(ix > 0, |el| el.pt(px(spaces::SIDEBAR_SECTION_GAP)))
                    .children(root)
                    .when(empty, |el| {
                        el.child(
                            div()
                                .px(px(Theme::SPACE_SM))
                                .pt(px(Theme::SPACE_SM))
                                .text_size(crate::typography::ui_rems(12.0))
                                .text_color(theme.text_faint)
                                .child(SharedString::from(
                                    "Give the orchestrator a task. The sessions it starts list here by folder.",
                                )),
                        )
                    })
                    .children(groups)
                    .into_any_element(),
            );
        }
        let content = if sections.is_empty() {
            div()
                .id("orchestrator-start")
                .px(px(Theme::SPACE_SM))
                .h(px(29.0))
                .flex()
                .items_center()
                .rounded(px(8.0))
                .text_size(crate::typography::ui_rems(12.0))
                .text_color(motion::hover_blend(
                    "orchestrator-start",
                    theme.text_faint,
                    theme.text,
                ))
                .bg(motion::hover_blend(
                    "orchestrator-start",
                    theme.glass_hover().opacity(0.0),
                    theme.glass_hover(),
                ))
                .on_hover(motion::hover_listener("orchestrator-start"))
                .cursor_pointer()
                .role(gpui::Role::Button)
                .on_click(cx.listener(|this, _, _, cx| this.start_orchestrator(cx)))
                .child(SharedString::from("Start an orchestrator"))
                .into_any_element()
        } else {
            div()
                .flex()
                .flex_col()
                .pb(px(Theme::SPACE_SM))
                .children(sections)
                .into_any_element()
        };
        let lists = crate::edge_fade::edge_faded(
            SIDEBAR_GLASS_FADE_BAND,
            true,
            true,
            div().relative().flex_1().min_h_0().child(
                div()
                    .id("orchestrator-lists")
                    .debug_selector(|| "orchestrator-lists".into())
                    .relative()
                    .size_full()
                    .overflow_y_scroll()
                    .track_scroll(&self.orchestrator_scroll)
                    .px(px(Theme::SPACE_SM))
                    .pt(px(Theme::SPACE_SM))
                    .flex()
                    .flex_col()
                    .child(content),
            ),
        )
        .fade_overflow_y(&self.orchestrator_scroll)
        .into_any_element();
        (div().flex_none().into_any_element(), lists)
    }

    /// A session's sidebar row, indented `level` deep on the tree's rails.
    fn render_orchestrator_row(
        &mut self,
        chat_id: &str,
        level: usize,
        label: SharedString,
        now: DateTime<Utc>,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let (title, time_ago, status, selected) = {
            let state = self.state.read(cx);
            let chat = state.chats.iter().find(|chat| chat.id == chat_id)?;
            (
                SharedString::from(transcript::single_line(
                    &chat.title.clone().unwrap_or_else(|| "New session".into()),
                )),
                SharedString::from(format_time_ago(
                    chat.last_message_at.unwrap_or(chat.created_at),
                    now,
                )),
                state.display_status_for(chat, now),
                state.selected_chat.as_deref() == Some(chat_id),
            )
        };
        let row = self.render_chat_row(
            chat_id.to_owned(),
            title,
            time_ago,
            label,
            None,
            None,
            status,
            selected,
            false,
            false,
            None,
            None,
            false,
            SidebarRowRole::Plain,
            None,
            theme,
            cx,
        );
        if level == 0 {
            return Some(row);
        }
        // The transcript's tree rail: a hairline down each ancestor's gutter.
        let rail = theme.hairline(0.12);
        Some(
            div()
                .relative()
                .pl(px(level as f32 * TREE_INDENT))
                .children((0..level).map(|depth| {
                    div()
                        .absolute()
                        .left(px(depth as f32 * TREE_INDENT + TREE_INDENT / 2.0))
                        .top(px(-SIDEBAR_LIST_GAP))
                        .bottom_0()
                        .w(px(1.0))
                        .bg(rail)
                }))
                .child(row)
                .into_any_element(),
        )
    }

    /// One folder's sessions: the project groups' disclosure header and body.
    fn render_orchestrator_group(
        &mut self,
        root: &str,
        group: &OrchestratorGroup,
        row_height: f32,
        now: DateTime<Utc>,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let collapse_key = format!("orchestrator:{root}:{}", group.key);
        let motion_key = format!("group:{collapse_key}");
        let collapsed = self.sidebar_collapsed_groups.contains(&collapse_key);
        let (label, row_labels): (SharedString, Vec<SharedString>) = {
            let state = self.state.read(cx);
            let row_labels = group
                .rows
                .iter()
                .map(|(id, _)| {
                    let path = state
                        .chats
                        .iter()
                        .find(|chat| chat.id == *id)
                        .map(|chat| chat_folder(chat, &state.spaces))
                        .unwrap_or_default();
                    folder_label(&path, &state.spaces).into()
                })
                .collect();
            (folder_label(&group.path, &state.spaces).into(), row_labels)
        };
        let rows: Vec<AnyElement> = group
            .rows
            .iter()
            .zip(row_labels)
            .filter_map(|((id, level), row_label)| {
                self.render_orchestrator_row(id, *level, row_label, now, theme, cx)
            })
            .collect();
        let count = rows.len();
        let body_height = spaces::SIDEBAR_DISCLOSURE_BODY_INSET
            + row_height * count as f32
            + SIDEBAR_LIST_GAP * count.saturating_sub(1) as f32;
        self.begin_queued_sidebar_reveal(&motion_key, !collapsed, body_height);
        let body = div()
            .w_full()
            .flex()
            .flex_col()
            .pt(px(spaces::SIDEBAR_DISCLOSURE_BODY_INSET))
            .gap(px(SIDEBAR_LIST_GAP))
            .children(rows);
        let chevron = self.sidebar_disclosure_chevron(&motion_key, !collapsed, theme);
        let group_icon = group.rows.first().map(|(id, _)| {
            self.render_project_group_icon(id, SIDEBAR_ACTIVE_HARNESS_ICON_SIZE, cx)
        });
        let toggle_key = collapse_key.clone();
        let toggle_motion_key = motion_key.clone();
        let path: SharedString = group.path.clone().into();
        let header = spaces::sidebar_disclosure_header(
            theme,
            group_icon,
            label,
            collapsed.then_some(count),
            None,
            chevron,
        )
        .id(SharedString::from(format!("sidebar-group-{collapse_key}")))
        .when(!path.is_empty(), |el| {
            el.tooltip(crate::settings::widgets::text_tooltip(path))
        })
        .on_click(cx.listener(move |this, _, _, cx| {
            let was_open = !this.sidebar_collapsed_groups.contains(&toggle_key);
            this.begin_sidebar_disclosure_motion(
                &toggle_motion_key,
                if was_open { body_height } else { 0.0 },
                if was_open { 0.0 } else { body_height },
            );
            if was_open {
                this.sidebar_collapsed_groups.insert(toggle_key.clone());
            } else {
                this.sidebar_collapsed_groups.remove(&toggle_key);
            }
            cx.notify();
        }));
        let body = self.render_sidebar_disclosure_body(
            &motion_key,
            !collapsed,
            body_height,
            body.into_any_element(),
        );
        div()
            .w_full()
            .flex()
            .flex_col()
            .pt(px(spaces::SIDEBAR_SECTION_GAP))
            .child(header)
            .child(body)
            .into_any_element()
    }

    /// The open session's place in its orchestrator tree, root first, for
    /// the titlebar's title slot. Each level but the open one opens on click.
    /// `None` outside an orchestrator tree.
    pub(super) fn render_orchestrator_crumbs(
        &mut self,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let specs: Vec<(String, SharedString)> = {
            let state = self.state.read(cx);
            let id = state.selected_chat.as_deref()?;
            let path = orchestrator::lineage(&state.chats, id);
            if !path
                .first()
                .is_some_and(|root| orchestrator::is_root_chat(&root.id))
            {
                return None;
            }
            path.iter()
                .enumerate()
                .map(|(depth, chat)| {
                    let title = transcript::single_line(
                        &chat.title.clone().unwrap_or_else(|| "New session".into()),
                    );
                    // The root's sessions name their folder, as their group does.
                    let label = if depth == 1 {
                        format!(
                            "{} / {title}",
                            folder_label(&chat_folder(chat, &state.spaces), &state.spaces)
                        )
                    } else {
                        title
                    };
                    (chat.id.clone(), label.into())
                })
                .collect()
        };
        let last = specs.len().saturating_sub(1);
        let mut trail: Vec<AnyElement> = Vec::with_capacity(specs.len() * 2);
        for (ix, (chat_id, name)) in specs.into_iter().enumerate() {
            if ix > 0 {
                trail.push(
                    icon(icons::ALT_ARROW_RIGHT)
                        .size(px(12.0))
                        .flex_none()
                        .text_color(theme.text_faint)
                        .into_any_element(),
                );
            }
            let current = ix == last;
            let group: SharedString = format!("orchestrator-crumb-{ix}").into();
            let color = if current {
                theme.text
            } else {
                theme.text_muted
            };
            let long = name.chars().count() > 26;
            let mut el = div()
                .id(group.clone())
                .group(group.clone())
                .flex_shrink(1.0)
                .min_w_0()
                .h(px(24.0))
                .px(px(6.0))
                .rounded(px(6.0))
                .flex()
                .items_center()
                .gap(px(5.0))
                .text_color(color)
                .when(ix == 0, |el| {
                    el.child(
                        icon(icons::BOT)
                            .size(px(14.0))
                            .flex_none()
                            .text_color(color)
                            .group_hover(group.clone(), |s| s.text_color(theme.text)),
                    )
                })
                .child(
                    div()
                        .min_w_0()
                        .max_w(px(180.0))
                        .truncate()
                        .child(name.clone()),
                )
                .when(long, |el| {
                    el.tooltip(crate::settings::widgets::text_tooltip(name.clone()))
                });
            // The open crumb is where you already are: no hover, no click.
            // The others occlude the titlebar's drag strip, like its buttons.
            if !current {
                el = el
                    .cursor_pointer()
                    .role(gpui::Role::Button)
                    .aria_label(name)
                    .hover(|s| s.bg(theme.element_hover).text_color(theme.text))
                    .occlude()
                    .on_mouse_down(MouseButton::Left, |_, window, _| window.prevent_default())
                    .on_click(cx.listener(move |this, _, _, cx| {
                        cx.stop_propagation();
                        this.open_chat(chat_id.clone(), cx)
                    }));
            }
            trail.push(el.into_any_element());
        }
        Some(
            div()
                .id("orchestrator-crumbs")
                .debug_selector(|| "orchestrator-crumbs".into())
                .min_w_0()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(2.0))
                .overflow_hidden()
                .text_size(crate::typography::ui_rems(12.0))
                .font_weight(gpui::FontWeight::MEDIUM)
                .children(trail)
                .into_any_element(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chat(id: &str, parent: Option<&str>, cwd: &str, minute: u32) -> Chat {
        serde_json::from_value(serde_json::json!({
            "id": id, "deviceId": "dev", "title": id, "archived": false,
            "cwd": cwd, "branch": null, "checkoutId": null, "config": null,
            "lastMessagePreview": null, "lastMessageAt": null,
            "createdAt": format!("2026-10-14T00:{minute:02}:00Z"),
            "parentChatId": parent,
        }))
        .unwrap()
    }

    #[test]
    fn a_root_s_caption_puts_what_needs_the_user_first() {
        use ChatIndicator::*;
        assert_eq!(tree_caption(&[], false), "Orchestrator");
        assert_eq!(tree_caption(&[], true), "Paused");
        assert_eq!(tree_caption(&[Idle], false), "1 session");
        assert_eq!(
            tree_caption(&[Working, AwaitingInput, Idle, Errored, Working], true),
            "Paused · 1 needs you · 1 failed · 2 working · 5 sessions"
        );
        assert_eq!(
            tree_caption(&[AwaitingInput, AwaitingInput, Completed], false),
            "2 need you · 3 sessions"
        );
    }

    #[test]
    fn sessions_group_by_folder_and_nest_under_their_parent() {
        let root = "orchestrator-a";
        let chats = vec![
            chat(root, None, "/scratch", 0),
            chat("talk-1", Some(root), "D:\\OpenTalk", 1),
            chat("zeron-1", Some(root), "D:\\zeron", 2),
            chat("talk-2", Some(root), "D:\\OpenTalk\\", 3),
            chat("talk-1-sub", Some("talk-1"), "D:\\OpenTalk", 4),
            chat("talk-1-sub-sub", Some("talk-1-sub"), "C:\\pi", 5),
            chat("plain", None, "D:\\zeron", 6),
            chat("plain-side", Some("plain"), "D:\\zeron", 7),
        ];
        let trees = orchestrator_trees(&chats, &[]);
        assert_eq!(trees.len(), 1);
        assert_eq!(trees[0].root, root);
        let groups: Vec<(&str, Vec<(&str, usize)>)> = trees[0]
            .groups
            .iter()
            .map(|g| {
                (
                    g.path.as_str(),
                    g.rows
                        .iter()
                        .map(|(id, level)| (id.as_str(), *level))
                        .collect(),
                )
            })
            .collect();
        assert_eq!(
            groups,
            [
                (
                    "D:\\OpenTalk",
                    vec![
                        ("talk-1", 0),
                        ("talk-1-sub", 1),
                        ("talk-1-sub-sub", 2),
                        ("talk-2", 0)
                    ]
                ),
                ("D:\\zeron", vec![("zeron-1", 0)]),
            ],
            "one group per folder, trailing separator or not; the folder with the newest session first"
        );
        assert_eq!(folder_label("D:\\OpenTalk\\", &[]), "OpenTalk");
        assert_eq!(folder_label("", &[]), "No folder");
    }
}
