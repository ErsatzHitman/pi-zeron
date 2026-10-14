//! The session panel: where this chat runs and what is working under it, in
//! one card at the transcript's right. Its essentials (machine, project,
//! working directory, session, branch and changes) always show; the goal,
//! the agent's todo list, the sub-agents and sub-sessions, and the commands
//! Pi runs in the background sit in sections that fold.
//!
//! While the column has room the panel stays docked at its right, the
//! transcript and composer shifting left as needed to clear it. Once
//! clearing it would narrow the transcript, the panel steps aside, and the
//! titlebar's toggle floats it over the transcript instead.
use super::*;

use std::collections::HashSet;

use gpui::Animation;
use zeron_proto::extension_ui::{ExtensionUiAction, PiUiAction};
use zeron_proto::{Chat, ChatIndicator, CheckoutDiff};

mod agents;
mod todos;

use crate::todo_panel::{self, FoldSide, TodoPanelState, TodoRow};
use agents::{AgentKind, AgentList, AgentRow};
use todos::TodoList;
use zeron_proto::TodoItem;

/// The card's width, its inset from the column's right edge, and the
/// least gap the reading column keeps from it.
pub(super) const SESSION_CARD_WIDTH: f32 = 272.0;
const SESSION_CARD_INSET: f32 = 12.0;
const SESSION_CARD_GAP: f32 = 24.0;
/// The transcript's own side gutters (its rows pad 48px each side).
const TRANSCRIPT_GUTTER: f32 = 48.0;
const SECTION_HEADER_HEIGHT: f32 = 26.0;
const AGENT_ROW_HEIGHT: f32 = 28.0;
/// Sections that fold, by key.
const GOAL: &str = "goal";
const TODOS: &str = "todos";
const AGENTS: &str = "agents";
const BACKGROUND: &str = "background";

/// The panel's own state, held on the shell across chats.
#[derive(Default)]
pub(super) struct SessionPanel {
    /// Sections the user folded away.
    folded: HashSet<&'static str>,
    /// Previous agents stays closed until opened.
    previous_open: bool,
    /// The sub-agent whose Stop asked for a second click.
    armed_stop: Option<String>,
    /// Every checkout's working-tree totals (`WatchCheckoutDiffs`, patches
    /// dropped), and the device the watch follows.
    diffs: Vec<CheckoutDiff>,
    diff_target: Option<Option<String>>,
    diff_task: Option<Task<()>>,
    /// The agent's checklist for the selected chat, rescanned only when its
    /// transcript changes (a chat with no todo walks the whole transcript).
    todo_cache: todo_panel::TodoCache,
    /// Per chat: whether the folded-away todo items are revealed.
    todo_folds: std::collections::HashMap<String, TodoPanelState>,
}

/// One `WatchCheckoutDiffs` frame: every checkout, or one that changed.
enum DiffFrame {
    All(Vec<CheckoutDiff>),
    One(CheckoutDiff),
}

impl DiffFrame {
    /// Decode a frame, keeping only the totals: patches run to megabytes.
    fn decode(value: serde_json::Value) -> Option<Self> {
        let strip = |mut diff: CheckoutDiff| {
            diff.patch = String::new();
            diff
        };
        if value.is_array() {
            let all = serde_json::from_value::<Vec<CheckoutDiff>>(value).ok()?;
            Some(Self::All(all.into_iter().map(strip).collect()))
        } else {
            let one = serde_json::from_value::<CheckoutDiff>(value).ok()?;
            Some(Self::One(strip(one)))
        }
    }
}

impl SessionPanel {
    fn is_open(&self, section: &'static str) -> bool {
        !self.folded.contains(section)
    }

    /// Unfold Sub-agents, so a side chat's row can take an inline rename.
    pub(super) fn reveal_agents(&mut self) -> bool {
        self.folded.remove(AGENTS);
        true
    }

    /// Fold an applied frame in; true when the totals changed.
    fn apply(&mut self, frame: DiffFrame) -> bool {
        match frame {
            DiffFrame::All(all) => {
                let changed = self.diffs != all;
                self.diffs = all;
                changed
            }
            DiffFrame::One(one) => {
                match self
                    .diffs
                    .iter_mut()
                    .find(|d| d.checkout_id == one.checkout_id)
                {
                    Some(existing) if *existing == one => return false,
                    Some(existing) => *existing = one,
                    None => self.diffs.push(one),
                }
                true
            }
        }
    }
}

/// The room the transcript holds free on its right for the card docked in a
/// main column `main` wide, for a configured reading width `transcript`: none
/// while the centered column already clears the card, else just enough to
/// shift it left. `None` once that would narrow the column: the card no
/// longer fits beside it.
pub(super) fn session_card_reserve(main: f32, transcript: f32) -> Option<f32> {
    let card_left = main - SESSION_CARD_WIDTH - SESSION_CARD_INSET;
    // Centered in `main - reserve`, the column's right edge is
    // (main - reserve) / 2 + transcript / 2; keep it at `card_left - gap`.
    let reserve = (main + transcript - 2.0 * (card_left - SESSION_CARD_GAP)).max(0.0);
    (main - reserve - 2.0 * TRANSCRIPT_GUTTER >= transcript - 0.5).then_some(reserve)
}

/// A non-interactive card row: icon slot, label, optional trailing detail.
fn info_row(theme: &Theme, glyph: AnyElement, label: SharedString) -> gpui::Div {
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(8.0))
        .min_h(px(popover::PICKER_ROW_HEIGHT))
        .px(px(9.0))
        .text_size(crate::typography::ui_rems(13.0))
        .text_color(theme.text)
        .child(popover::picker_icon_slot(glyph))
        .child(div().flex_1().min_w_0().truncate().child(label))
}

fn row_glyph(path: &'static str, theme: &Theme) -> AnyElement {
    icon(path)
        .size(px(15.0))
        .text_color(theme.text_muted)
        .into_any_element()
}

/// A row's small trailing text.
fn trailing_text(text: impl Into<SharedString>, color: gpui::Hsla) -> gpui::Div {
    div()
        .flex_none()
        .text_size(crate::typography::ui_rems(11.0))
        .text_color(color)
        .child(text.into())
}

/// Running time since `started`, ticking while it shows.
fn elapsed_label(id: String, started: DateTime<Utc>, color: gpui::Hsla) -> AnyElement {
    div()
        .flex_none()
        .text_size(crate::typography::ui_rems(11.0))
        .text_color(color)
        .with_animation(
            SharedString::from(id),
            Animation::new(Duration::from_secs(1)).repeat(),
            move |label, _| {
                label.child(crate::transcript::format_elapsed(
                    (Utc::now() - started).num_seconds(),
                ))
            },
        )
        .into_any_element()
}

/// A long working directory, shortened from the left: past three parts,
/// only the last two folders. The path may be another device's, so it is
/// never resolved against this one.
fn compact_path(path: &str) -> String {
    let sep = if path.contains('\\') { '\\' } else { '/' };
    let parts: Vec<&str> = path.split(sep).filter(|p| !p.is_empty()).collect();
    if parts.len() <= 3 {
        return path.to_owned();
    }
    format!(
        "…{sep}{}{sep}{}",
        parts[parts.len() - 2],
        parts[parts.len() - 1]
    )
}

/// What the goal's status reads as, and whether it is at work.
fn goal_status(status: &str) -> (&'static str, bool) {
    match status {
        "active" => ("Running", true),
        "waiting" => ("Waiting", false),
        "paused" => ("Paused", false),
        "blocked" => ("Blocked", false),
        "budget_limited" => ("Budget reached", false),
        _ => ("Set", false),
    }
}

impl Shell {
    /// The card's reserve at the main column's LIVE width (see
    /// [`session_card_reserve`]): while the sidebar or a pane animates, the
    /// settled width would move or hide the card ahead of the column.
    fn session_info_reserve(&self, cx: &App) -> Option<f32> {
        let main = (self.viewport_width
            - self.sidebar_now()
            - self.right_visible_width(cx)
            - self.files_visible_width(cx))
        .max(0.0);
        session_card_reserve(main, crate::settings::transcript_width(cx))
    }

    /// Whether the card fits beside the transcript without narrowing it.
    pub(super) fn session_info_fits(&self, cx: &App) -> bool {
        self.session_info_reserve(cx).is_some()
    }

    /// Beside the transcript it always shows; over it, only while opened.
    pub(super) fn session_info_visible(&self, cx: &App) -> bool {
        if self.session_info_fits(cx) {
            !self.settings.session_info_hidden
        } else {
            self.session_info_overlay
        }
    }

    /// The toggle's mouse-down: note whether the card showed, so the click
    /// that follows closes it rather than reopening it. Reads the last
    /// painted state, which the overlay's click-outside dismissal (handled
    /// on the same press, in either order) leaves alone.
    pub(super) fn note_session_info_press(&mut self) {
        self.session_info_pressed_open = Some(self.session_info_shown);
    }

    /// Open or close the card floating over a narrow column. Docked, it
    /// stays: the toggle only shows while the card does not fit.
    pub(super) fn toggle_session_info(&mut self, cx: &mut Context<Self>) {
        let show = !self
            .session_info_pressed_open
            .take()
            .unwrap_or(self.session_info_shown);
        if self.session_info_fits(cx) {
            return;
        }
        self.session_info_overlay = show;
        cx.notify();
    }

    /// Dismiss the card floating over the transcript; one shown beside it
    /// stays put.
    fn close_session_info_overlay(&mut self, cx: &mut Context<Self>) {
        if self.session_info_overlay {
            self.session_info_overlay = false;
            cx.notify();
        }
    }

    /// This frame's card: docked beside the transcript (else floating over
    /// it), its reveal, 0 (hidden) → 1 (shown), and the room the transcript
    /// holds free for it. The reveal eases on the shell's resize clock
    /// whenever the card shows or hides, by toggle or by the column crossing
    /// the fit, and the reserve eases with it.
    pub(super) fn session_info_frame(&mut self, has_selection: bool, cx: &App) -> (bool, f32, f32) {
        let reserve = self.session_info_reserve(cx);
        if let Some(reserve) = reserve {
            // An overlay opened on a tighter column ends once the card fits.
            self.session_info_overlay = false;
            self.session_info_last_reserve = reserve;
        }
        let shown = has_selection && self.session_info_visible(cx);
        if shown {
            self.session_info_docked = reserve.is_some();
        }
        let target = if shown { 1.0 } else { 0.0 };
        if shown != self.session_info_shown {
            let from = self.eval_tween(self.session_info_tween, 1.0 - target);
            self.session_info_tween = Some(WidthTween::new(from, target));
            self.session_info_shown = shown;
        }
        let reveal = self.eval_tween(self.session_info_tween, target);
        // Hiding as the column stops fitting, the card keeps its docked look
        // and the transcript eases back from the last room it held.
        let docked = self.session_info_docked;
        let reserve = if docked {
            reserve.unwrap_or(self.session_info_last_reserve) * reveal
        } else {
            0.0
        };
        (docked, reveal, reserve)
    }

    /// Keep the working-tree totals of `chat`'s device streaming. Retries
    /// with a flat 2 s delay if the stream fails or ends.
    fn ensure_session_diff_watch(&mut self, chat: &Chat, cx: &mut Context<Self>) {
        let (engine, target) = {
            let state = self.state.read(cx);
            let Some(engine) = state.engine().cloned() else {
                return;
            };
            let local = state.local_device_id.as_deref() == Some(chat.device_id.as_str());
            (engine, (!local).then(|| chat.device_id.clone()))
        };
        let panel = &mut self.session_panel;
        if panel.diff_task.is_some() && panel.diff_target.as_ref() == Some(&target) {
            return;
        }
        // Another device's checkouts would resolve against the wrong rows.
        panel.diffs.clear();
        panel.diff_target = Some(target.clone());
        panel.diff_task = Some(cx.spawn(async move |this, cx| {
            loop {
                let mut params = serde_json::Map::new();
                if let Some(target) = &target {
                    params.insert("targetDeviceId".into(), target.clone().into());
                }
                let subscribed = engine
                    .client()
                    .subscribe(
                        methods::WATCH_CHECKOUT_DIFFS,
                        serde_json::Value::Object(params),
                    )
                    .await;
                if let Ok(mut frames) = subscribed {
                    while let Some(value) = frames.recv().await {
                        let frame = cx
                            .background_executor()
                            .spawn(async move { DiffFrame::decode(value) })
                            .await;
                        let alive = this.update(cx, |shell, cx| {
                            if let Some(frame) = frame
                                && shell.session_panel.apply(frame)
                            {
                                cx.notify();
                            }
                        });
                        if alive.is_err() {
                            return;
                        }
                    }
                }
                cx.background_executor().timer(Duration::from_secs(2)).await;
                if this.update(cx, |_, _| {}).is_err() {
                    return;
                }
            }
        }));
    }

    /// Bring the chat's changes forward: its diff tab if one is open, else a
    /// new one.
    fn open_session_diff(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.close_session_info_overlay(cx);
        let key = self.panel_key(cx);
        let open = self.right_tabs.get(&key).and_then(|tabs| {
            tabs.iter()
                .rev()
                .find(|surface| matches!(surface, RightSurface::Diff(_)))
                .copied()
        });
        match open {
            Some(surface) => self.set_right_active(surface, cx),
            None => self.add_diff_surface(window, cx),
        }
        self.set_surfaces_open(true, cx);
        cx.notify();
    }

    /// Send a Pi sub-agent's roster action (View, Kill).
    fn send_fleet_action(
        &mut self,
        element: String,
        row: String,
        action: &PiUiAction,
        cx: &mut Context<Self>,
    ) {
        let request = ExtensionUiAction {
            element_id: element,
            action_id: action.id.clone(),
            value: None,
            row_id: Some(row),
        };
        self.state
            .update(cx, |state, cx| state.extension_ui_action(request, cx));
    }

    /// The card, positioned within the main column; `None` while closed or
    /// with no chat selected.
    pub(super) fn render_session_info(
        &mut self,
        docked: bool,
        reveal: f32,
        max_height: f32,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if reveal <= 0.001 {
            return None;
        }
        let theme = Theme::of(cx).for_popup();
        let now = Utc::now();
        let todo_cache = &mut self.session_panel.todo_cache;
        let (chat, device, local, online, project, branch, status, started, goal, agents, todos) = {
            let state = self.state.read(cx);
            let chat = state.selected_chat_row()?.clone();
            let device: SharedString = state
                .device_name(&chat.device_id)
                .unwrap_or("Unknown device")
                .to_owned()
                .into();
            let local = state.local_device_id.as_deref() == Some(chat.device_id.as_str());
            let online = state.device_online(&chat.device_id, now);
            let project = chat
                .space_id
                .as_deref()
                .and_then(|id| state.space_row(id))
                .map(|space| {
                    (
                        SharedString::from(space.display_name().to_owned()),
                        space.path.clone(),
                    )
                });
            let branch = crate::change_requests::conversation_branch(&chat, &state.spaces)
                .map(str::trim)
                .filter(|branch| !branch.is_empty())
                .map(|branch| SharedString::from(branch.to_owned()));
            let status = state.display_status_for(&chat, now);
            let started = state.session_for(&chat.id).and_then(|s| s.started_at);
            let goal = state
                .extension_ui
                .of(&chat.id)
                .and_then(|ui| ui.channels.get(zeron_proto::extension_ui::GOAL_CHANNEL))
                .filter(|goal| goal["active"] == true)
                .cloned();
            let agents = agents::agent_list(state, &chat, now);
            let live = matches!(
                status,
                ChatIndicator::Working | ChatIndicator::AwaitingInput
            );
            // `state.transcript` is the selected chat's own (selecting a chat
            // clears it), and `chat` is the selected row.
            let todos = TodoList::of(
                todo_cache
                    .latest(&chat.id, state.transcript_revision, &state.transcript)
                    .map(<[TodoItem]>::to_vec),
                live,
                || todos::written_this_turn(&state.transcript),
            );
            (
                chat, device, local, online, project, branch, status, started, goal, agents, todos,
            )
        };
        self.ensure_session_diff_watch(&chat, cx);

        // ── Where it runs: always shown.
        let device_row = info_row(
            &theme,
            row_glyph(if local { icons::LAPTOP } else { icons::MONITOR }, &theme),
            device,
        )
        .id("session-info-device")
        .debug_selector(|| "session-info-device".into())
        .when(!online, |row| {
            row.child(
                icon(icons::WIFI_OFF)
                    .size(px(12.0))
                    .flex_none()
                    .text_color(theme.warning.opacity(0.8)),
            )
        });
        let project_path = project.as_ref().map(|(_, path)| path.clone());
        let project_row = match project {
            Some((name, path)) => info_row(
                &theme,
                self.render_project_icon(&chat.id, 16.0, false, cx),
                name,
            )
            .id("session-info-project")
            .tooltip(crate::settings::widgets::text_tooltip(path))
            .into_any_element(),
            None => info_row(&theme, row_glyph(icons::HOME, &theme), "No project".into())
                .into_any_element(),
        };
        // The folder the agent works in; a worktree says so.
        let directory_row = chat.cwd.clone().or(project_path.clone()).map(|cwd| {
            let worktree = project_path.as_deref().is_some_and(|path| path != cwd);
            info_row(
                &theme,
                row_glyph(
                    if worktree {
                        icons::WORKTREE
                    } else {
                        icons::FOLDER
                    },
                    &theme,
                ),
                compact_path(&cwd).into(),
            )
            .id("session-info-directory")
            .debug_selector(|| "session-info-directory".into())
            .tooltip(crate::settings::widgets::text_tooltip(cwd))
            .when(worktree, |row| {
                row.child(trailing_text("Worktree", theme.text_muted.opacity(0.7)))
            })
        });
        let session_row = self.render_session_row(&chat, status, started, &theme, cx);

        // ── Version control: the branch and its uncommitted changes.
        let branch_row = branch.map(|branch| {
            info_row(&theme, row_glyph(icons::GIT_BRANCH, &theme), branch.clone())
                .id("session-info-branch")
                .tooltip(crate::settings::widgets::text_tooltip(branch))
        });
        let changes_row = crate::changes::resolve_diff(&self.session_panel.diffs, &chat)
            .map(|diff| self.render_changes_row(diff, &theme, cx));

        // ── Sections that fold.
        let goal = goal.map(|goal| self.render_goal(&goal, &theme, cx));
        let todos = self.render_todos(todos.as_ref(), &chat.id, &theme, cx);
        let background = self.render_background(&agents.background, &theme, cx);
        let agents = self.render_agents(&agents, &theme, cx);

        let body = div()
            .id("session-info-body")
            .max_h(px(max_height.max(120.0)))
            .overflow_y_scroll()
            // The card's inset lives inside the scroller, so dividers can
            // bleed through it edge to edge without being clipped.
            .p(px(popover::CARD_INSET))
            .flex()
            .flex_col()
            .gap(px(popover::MENU_GAP))
            .child(device_row)
            .child(project_row)
            .children(directory_row)
            .child(session_row)
            .when(branch_row.is_some() || changes_row.is_some(), |body| {
                body.child(popover::picker_divider())
                    .children(branch_row)
                    .children(changes_row)
            })
            .children(goal.into_iter().flatten())
            .children(todos)
            .children(agents)
            .children(background);

        let card = div()
            .w(px(SESSION_CARD_WIDTH))
            .rounded(px(popover::CARD_RADIUS))
            .overflow_hidden()
            .border_1()
            .border_color(theme.border)
            .bg(popover::surface_bg(&theme))
            .when(!docked && !theme.is_frost(), |card| card.shadow_lg())
            .child(body);

        // Docked, the card slides in from the right beside the column;
        // floating, it drops in from just above. Either way it is placed from
        // the column's right edge, so it rides that edge while panes animate.
        let ease = crate::motion::RESIZE.progress(reveal.clamp(0.0, 1.0));
        let (dx, dy) = if docked {
            (12.0 * (1.0 - ease), 0.0)
        } else {
            (0.0, -6.0 * (1.0 - ease))
        };
        Some(
            div()
                .id("session-info")
                .debug_selector(|| "session-info".into())
                .absolute()
                .top(px(Theme::TITLEBAR_HEIGHT + 12.0 + dy))
                .right(px(SESSION_CARD_INSET - dx))
                .opacity(ease)
                .occlude()
                // Over the transcript it is a popover: a press elsewhere, or
                // choosing one of its rows, dismisses it.
                .when(!docked, |card| {
                    card.on_mouse_down_out(
                        cx.listener(|this, _, _, cx| this.close_session_info_overlay(cx)),
                    )
                })
                .child(crate::frost::frosted(
                    popover::CARD_RADIUS,
                    crate::frost::MENU_BLUR,
                    card,
                ))
                .into_any_element(),
        )
    }

    /// The session row: the agent's mark and the chat's title, with what it
    /// is doing (a live timer while it works) or else its model.
    fn render_session_row(
        &self,
        chat: &Chat,
        status: ChatIndicator,
        started: Option<DateTime<Utc>>,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let harness = chat.config.as_ref().map(|config| config.harness);
        let glyph = match harness.map(crate::pickers::harness_brand_icon) {
            Some((path, tint)) => icon(path)
                .size(px(14.0))
                .text_color(tint.unwrap_or(theme.text_muted))
                .into_any_element(),
            None => row_glyph(icons::CHAT_ROUND_LINE, theme),
        };
        let title: SharedString = chat
            .title
            .clone()
            .filter(|title| !title.trim().is_empty())
            .unwrap_or_else(|| "New session".into())
            .into();
        let model = chat
            .config
            .as_ref()
            .and_then(|config| config.model.clone())
            .filter(|model| !model.is_empty());
        let muted = theme.text_muted.opacity(0.7);
        let trailing = match (status, started) {
            (ChatIndicator::Working, Some(started)) => {
                elapsed_label("session-info-elapsed".into(), started, theme.text_muted)
            }
            (ChatIndicator::Working, None) => {
                trailing_text("Working", theme.text_muted).into_any_element()
            }
            (ChatIndicator::AwaitingInput, _) => {
                trailing_text("Needs you", theme.warning).into_any_element()
            }
            (ChatIndicator::Errored, _) => trailing_text("Error", theme.danger).into_any_element(),
            _ => match &model {
                Some(model) => div()
                    .max_w(px(96.0))
                    .truncate()
                    .child(trailing_text(
                        model.rsplit('/').next().unwrap_or(model).to_owned(),
                        muted,
                    ))
                    .into_any_element(),
                None => div().into_any_element(),
            },
        };
        let spinner = (status == ChatIndicator::Working).then(|| {
            crate::loaders::mini_glyph_spinner(
                "session-info-working",
                2.0,
                theme.glyph,
                cx.entity_id(),
                cx,
            )
            .into_any_element()
        });
        info_row(theme, glyph, title)
            .id("session-info-session")
            .debug_selector(|| "session-info-session".into())
            .when_some(model, |row, model| {
                row.tooltip(crate::settings::widgets::text_tooltip(model))
            })
            .children(spinner)
            .child(trailing)
            .into_any_element()
    }

    /// Changes: the working tree's line totals; it opens the diff.
    fn render_changes_row(
        &self,
        diff: &CheckoutDiff,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let clean = diff.additions == 0 && diff.deletions == 0 && diff.files.is_empty();
        let files = diff.files.len();
        let tip = match files {
            0 => "No uncommitted changes".to_owned(),
            1 => "1 file changed".to_owned(),
            n => format!("{n} files changed"),
        };
        popover::picker_row(theme, false, false, "session-info-changes")
            .id("session-info-changes")
            .debug_selector(|| "session-info-changes".into())
            .tooltip(crate::settings::widgets::text_tooltip(tip))
            .on_click(cx.listener(|this, _, window, cx| this.open_session_diff(window, cx)))
            .child(popover::picker_icon_slot(row_glyph(
                icons::FILE_CODE,
                theme,
            )))
            .child(div().flex_1().child("Changes"))
            .when(clean, |row| {
                row.child(trailing_text("None", theme.text_muted.opacity(0.7)))
            })
            .when(!clean, |row| {
                row.child(trailing_text(format!("+{}", diff.additions), theme.success))
                    .child(trailing_text(format!("−{}", diff.deletions), theme.danger))
            })
            .into_any_element()
    }

    /// A section's header: its label and count, an optional action, and the
    /// fold caret. A click anywhere else on it folds the section.
    fn render_section_header(
        &self,
        section: &'static str,
        label: &str,
        count: Option<SharedString>,
        action: Option<AnyElement>,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let open = self.session_panel.is_open(section);
        div()
            .id(SharedString::from(format!("session-info-{section}-header")))
            .debug_selector(move || format!("session-info-{section}-header"))
            .role(gpui::Role::Button)
            .aria_label(SharedString::from(label.to_owned()))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(6.0))
            .h(px(SECTION_HEADER_HEIGHT))
            .px(px(8.0))
            .cursor_pointer()
            .rounded(px(popover::MENU_ITEM_RADIUS))
            .on_click(cx.listener(move |this, _, _, cx| {
                let folded = &mut this.session_panel.folded;
                if !folded.remove(section) {
                    folded.insert(section);
                }
                cx.notify();
            }))
            .child(
                div()
                    .text_size(crate::typography::ui_rems(10.0))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_color(theme.text_muted)
                    .child(SharedString::from(popover::tracked_upper(label))),
            )
            .children(count.map(|count| trailing_text(count, theme.text_muted.opacity(0.7))))
            .child(div().flex_1())
            .children(action)
            .child(
                icon(if open {
                    icons::ALT_ARROW_DOWN
                } else {
                    icons::ALT_ARROW_RIGHT
                })
                .size(px(12.0))
                .flex_none()
                .text_color(theme.text_muted.opacity(0.7)),
            )
            .into_any_element()
    }

    /// The goal pi-goal is working toward: its status, objective and
    /// progress line.
    fn render_goal(
        &self,
        goal: &serde_json::Value,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let (status, running) = goal_status(goal["status"].as_str().unwrap_or_default());
        let tone = if running {
            theme.accent
        } else if matches!(status, "Blocked" | "Budget reached") {
            theme.warning
        } else {
            theme.text_muted
        };
        let pill = div()
            .flex_none()
            .px(px(6.0))
            .rounded_full()
            .bg(tone.opacity(0.14))
            .text_size(crate::typography::ui_rems(10.5))
            .text_color(tone)
            .child(status);
        let mut rows = vec![
            popover::picker_divider().into_any_element(),
            self.render_section_header(
                GOAL,
                "Goal",
                None,
                Some(pill.into_any_element()),
                theme,
                cx,
            ),
        ];
        if !self.session_panel.is_open(GOAL) {
            return rows;
        }
        let text = goal["text"].as_str().unwrap_or_default().trim().to_owned();
        let detail = goal["detail"]
            .as_str()
            .unwrap_or_default()
            .trim()
            .to_owned();
        rows.push(
            div()
                .id("session-info-goal")
                .debug_selector(|| "session-info-goal".into())
                .px(px(9.0))
                .pb(px(4.0))
                .flex()
                .flex_col()
                .gap(px(2.0))
                .child(
                    div()
                        .text_size(crate::typography::ui_rems(13.0))
                        .line_height(px(18.0))
                        .text_color(theme.text)
                        .child(if text.is_empty() {
                            "Working toward a goal".to_owned()
                        } else {
                            text
                        }),
                )
                .when(!detail.is_empty(), |goal| {
                    goal.child(
                        div()
                            .text_size(crate::typography::ui_rems(11.5))
                            .text_color(theme.text_muted)
                            .child(detail),
                    )
                })
                .into_any_element(),
        );
        rows
    }

    /// Sub-agents: the sub-sessions this chat hangs off and owns, the
    /// sub-agents at work, and the settled ones under Previous agents.
    /// The agent's checklist: what is done, what it is on and what is left.
    /// Absent while there is no list in play (see [`TodoList::of`]).
    fn render_todos(
        &mut self,
        todos: Option<&TodoList>,
        chat_id: &str,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let Some(todos) = todos else {
            return Vec::new();
        };
        let mut out = vec![
            popover::picker_divider().into_any_element(),
            self.render_section_header(TODOS, "Todo", Some(todos.count_label()), None, theme, cx),
        ];
        if !self.session_panel.is_open(TODOS) {
            return out;
        }
        let (show_earlier, show_later) = self
            .session_panel
            .todo_folds
            .get(chat_id)
            .map(TodoPanelState::folds)
            .unwrap_or_default();
        for row in todo_panel::rows(&todos.items, show_earlier, show_later) {
            out.push(match row {
                TodoRow::Item(ix) => {
                    Self::render_todo_row(ix, &todos.items[ix], todos.live, theme, cx)
                }
                TodoRow::Fold { side, count, open } => {
                    self.render_todo_fold(chat_id, side, count, open, theme, cx)
                }
            });
        }
        out
    }

    /// One checklist item: status glyph and text, wrapping. Completed work
    /// recedes and the in-progress item is the brightest, as in the composer's
    /// tray. Not interactive.
    fn render_todo_row(
        ix: usize,
        item: &TodoItem,
        live: bool,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let status = item.status();
        let (color, weight) = match status {
            zeron_proto::TodoStatus::Completed => (theme.text_faint, gpui::FontWeight::NORMAL),
            zeron_proto::TodoStatus::InProgress => (theme.text, gpui::FontWeight::MEDIUM),
            zeron_proto::TodoStatus::Pending => (theme.text_muted, gpui::FontWeight::NORMAL),
        };
        let id = SharedString::from(format!("session-info-todo-{ix}"));
        let view = cx.entity_id();
        div()
            .id(id.clone())
            .debug_selector(move || id.to_string())
            .flex()
            .flex_row()
            .items_start()
            .gap(px(8.0))
            .min_h(px(AGENT_ROW_HEIGHT))
            .px(px(9.0))
            .py(px(5.0))
            .child(
                div()
                    .w(px(16.0))
                    .h(px(18.0))
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(todo_panel::status_glyph(
                        format!("session-info-todo-active-{ix}"),
                        2.0,
                        status,
                        live,
                        theme,
                        view,
                        cx,
                    )),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_size(crate::typography::ui_rems(13.0))
                    .line_height(px(18.0))
                    .font_weight(weight)
                    .text_color(color)
                    .child(SharedString::from(item.text.clone())),
            )
            .into_any_element()
    }

    /// "N earlier" / "N later": stands in for the items folded off one end of
    /// a long list, styled like Previous agents.
    fn render_todo_fold(
        &self,
        chat_id: &str,
        side: FoldSide,
        count: usize,
        open: bool,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let noun = match side {
            FoldSide::Earlier => "earlier",
            FoldSide::Later => "later",
        };
        let id = SharedString::from(format!("session-info-todo-fold-{noun}"));
        let chat_id = chat_id.to_owned();
        div()
            .id(id.clone())
            .debug_selector(move || id.to_string())
            .role(gpui::Role::Button)
            .aria_label(SharedString::from(format!(
                "{} {count} {noun} items",
                if open { "Hide" } else { "Show" }
            )))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(6.0))
            .h(px(AGENT_ROW_HEIGHT))
            .px(px(9.0))
            .rounded(px(popover::MENU_ITEM_RADIUS))
            .cursor_pointer()
            .text_size(crate::typography::ui_rems(12.0))
            .text_color(theme.text_muted)
            .hover(|style| style.bg(crate::theme::ink(0.05)))
            .on_click(cx.listener(move |this, _, _, cx| {
                this.session_panel
                    .todo_folds
                    .entry(chat_id.clone())
                    .or_default()
                    .toggle_fold(side);
                cx.notify();
            }))
            .child(format!("{count} {noun}"))
            .child(div().flex_1())
            .child(
                icon(if open {
                    icons::ALT_ARROW_DOWN
                } else {
                    icons::ALT_ARROW_RIGHT
                })
                .size(px(12.0))
                .text_color(theme.text_muted.opacity(0.7)),
            )
            .into_any_element()
    }

    fn render_agents(
        &mut self,
        list: &AgentList,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let running = list.running();
        let new_side_chat = div()
            .id("session-info-new-side-chat")
            .debug_selector(|| "session-info-new-side-chat".into())
            .role(gpui::Role::Button)
            .aria_label("btw")
            .size(px(20.0))
            .flex_none()
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(5.0))
            .hover(|style| style.bg(crate::theme::ink(0.08)))
            .tooltip(crate::settings::widgets::text_tooltip("btw (by the way)"))
            .on_click(cx.listener(|this, _, _, cx| {
                cx.stop_propagation();
                this.close_session_info_overlay(cx);
                this.create_child_chat(None, cx)
            }))
            .child(
                icon(icons::PLUS)
                    .size(px(13.0))
                    .text_color(theme.text_muted),
            );
        let mut rows = vec![
            popover::picker_divider().into_any_element(),
            self.render_section_header(
                AGENTS,
                "Sub-agents",
                (running > 0).then(|| format!("· {running} running").into()),
                Some(new_side_chat.into_any_element()),
                theme,
                cx,
            ),
        ];
        if !self.session_panel.is_open(AGENTS) {
            return rows;
        }
        if list.is_empty() {
            rows.push(
                div()
                    .px(px(9.0))
                    .py(px(5.0))
                    .text_size(crate::typography::ui_rems(12.0))
                    .text_color(theme.text_muted)
                    .child("No sub-agents or side chats yet")
                    .into_any_element(),
            );
            return rows;
        }
        for row in list.sessions.iter().chain(&list.active) {
            rows.push(self.render_agent_row(row, theme, cx));
        }
        if !list.previous.is_empty() {
            let open = self.session_panel.previous_open;
            rows.push(
                div()
                    .id("session-info-previous")
                    .debug_selector(|| "session-info-previous".into())
                    .role(gpui::Role::Button)
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(6.0))
                    .h(px(AGENT_ROW_HEIGHT))
                    .px(px(9.0))
                    .rounded(px(popover::MENU_ITEM_RADIUS))
                    .cursor_pointer()
                    .text_size(crate::typography::ui_rems(12.0))
                    .text_color(theme.text_muted)
                    .hover(|style| style.bg(crate::theme::ink(0.05)))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.session_panel.previous_open = !this.session_panel.previous_open;
                        cx.notify();
                    }))
                    .child(format!("Previous agents ({})", list.previous.len()))
                    .child(div().flex_1())
                    .child(
                        icon(if open {
                            icons::ALT_ARROW_DOWN
                        } else {
                            icons::ALT_ARROW_RIGHT
                        })
                        .size(px(12.0))
                        .text_color(theme.text_muted.opacity(0.7)),
                    )
                    .into_any_element(),
            );
            if open {
                for row in &list.previous {
                    rows.push(self.render_agent_row(row, theme, cx));
                }
            }
        }
        rows
    }

    /// One sub-agent or sub-session: its state, title and time. A click
    /// opens it; a running Pi sub-agent can be stopped from its row.
    fn render_agent_row(
        &mut self,
        row: &AgentRow,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let key = row.key();
        let id = SharedString::from(format!("session-info-agent-{key}"));
        let group = SharedString::from(format!("session-info-agent-group-{key}"));
        let kind_icon = match &row.kind {
            AgentKind::Fleet { .. } | AgentKind::Subagent { .. } => icons::BOT,
            AgentKind::Background { .. } => icons::TERMINAL,
            AgentKind::SideChat { .. } => icons::CHAT_ROUND_LINE,
            AgentKind::Parent { .. } => icons::ARROW_UP,
        };
        let glyph: AnyElement = match row.status {
            ChatIndicator::Working => crate::loaders::mini_glyph_spinner(
                format!("{id}-working"),
                2.0,
                theme.glyph,
                cx.entity_id(),
                cx,
            )
            .into_any_element(),
            ChatIndicator::AwaitingInput => div()
                .size(px(6.0))
                .rounded_full()
                .bg(super::spaces::status_dot_color(row.status, theme))
                .into_any_element(),
            ChatIndicator::Errored => icon(kind_icon)
                .size(px(14.0))
                .text_color(theme.danger)
                .into_any_element(),
            _ => icon(kind_icon)
                .size(px(14.0))
                .text_color(theme.text_muted)
                .into_any_element(),
        };
        let rename = match &row.kind {
            AgentKind::SideChat { chat_id, .. } => {
                self.rename_input_for(chat_id, ChatRenameSurface::Session)
            }
            _ => None,
        };
        let title = match rename {
            Some(input) => super::chat_title_editor(
                format!("session-info-chat-title-editor-{key}").into(),
                input,
                theme,
            ),
            None => div()
                .flex_1()
                .min_w_0()
                .truncate()
                .child(row.title.clone())
                .into_any_element(),
        };
        let muted = theme.text_muted.opacity(0.7);
        let time = match (row.started, &row.trailing) {
            (Some(started), _) => Some(elapsed_label(format!("{id}-elapsed"), started, muted)),
            (None, Some(text)) => Some(trailing_text(text.clone(), muted).into_any_element()),
            (None, None) => None,
        };
        let badge = match &row.kind {
            AgentKind::SideChat {
                chat_id,
                device_id,
                change_request: Some(summary),
            } => Some(crate::change_requests::pull_request_badge(
                format!("session-info-chat-pr-{chat_id}").into(),
                summary.clone(),
                crate::change_requests::ChangeRequestBadgeSurface::Sidebar,
                Some(device_id.clone()),
                theme,
            )),
            _ => None,
        };
        let stop = match &row.kind {
            AgentKind::Fleet {
                element,
                row: row_id,
                stop: Some(stop),
                ..
            }
            | AgentKind::Background {
                element,
                row: row_id,
                stop: Some(stop),
            } => Some(self.render_fleet_stop(element, row_id, stop, &group, theme, cx)),
            _ => None,
        };
        let clickable = !matches!(
            &row.kind,
            AgentKind::Fleet { view: None, .. } | AgentKind::Background { .. }
        );
        let mut element = div()
            .id(id.clone())
            .debug_selector({
                let id = id.clone();
                move || id.to_string()
            })
            .group(group)
            .role(gpui::Role::Button)
            .aria_label(row.title.clone())
            .flex()
            .flex_row()
            .items_center()
            .gap(px(8.0))
            .h(px(AGENT_ROW_HEIGHT))
            .px(px(9.0))
            .rounded(px(popover::MENU_ITEM_RADIUS))
            .text_size(crate::typography::ui_rems(13.0))
            .text_color(theme.text)
            .when(clickable, |row| {
                row.cursor_pointer()
                    .hover(|style| style.bg(crate::theme::ink(0.05)))
            })
            .child(
                div()
                    .size(px(16.0))
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(glyph),
            )
            .child(title)
            .children(badge)
            .children(stop)
            .children(time);
        if let Some(detail) = row.detail.clone() {
            element = element.tooltip(crate::settings::widgets::text_tooltip(detail));
        }
        let kind = row.kind.clone();
        let title = row.title.to_string();
        element = element.on_click(cx.listener(move |this, event: &gpui::ClickEvent, _, cx| {
            match &kind {
                AgentKind::Fleet {
                    element,
                    row,
                    view: Some(view),
                    ..
                } => {
                    this.close_session_info_overlay(cx);
                    this.send_fleet_action(element.clone(), row.clone(), view, cx);
                }
                AgentKind::Fleet { .. } | AgentKind::Background { .. } => {}
                AgentKind::Subagent { doc_id, frozen } => {
                    this.close_session_info_overlay(cx);
                    this.add_subagent_surface(
                        this.active_chat.clone(),
                        doc_id.clone(),
                        title.clone(),
                        *frozen,
                        cx,
                    );
                }
                // The first click opened the tab; the second edits the
                // title in place.
                AgentKind::SideChat { chat_id, .. } if event.click_count() >= 2 => {
                    this.open_rename_chat(chat_id.clone(), cx)
                }
                AgentKind::SideChat { chat_id, .. } => {
                    this.close_session_info_overlay(cx);
                    this.open_child_chat_tab(chat_id, cx);
                }
                AgentKind::Parent { chat_id } => {
                    this.close_session_info_overlay(cx);
                    this.open_chat(chat_id.clone(), cx);
                }
            }
        }));
        if let AgentKind::SideChat { chat_id, .. } = &row.kind {
            let chat_id = chat_id.clone();
            element = element.on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                    cx.stop_propagation();
                    this.chat_menu.open(ChatMenuState {
                        chat_id: chat_id.clone(),
                        tab: None,
                        position: event.position,
                        page: ChatMenuPage::Root,
                    });
                    cx.notify();
                }),
            );
        }
        element.into_any_element()
    }

    /// A running Pi sub-agent's or background command's Stop: shown on
    /// hover; a first click asks, the second stops it.
    fn render_fleet_stop(
        &self,
        element: &str,
        row: &str,
        stop: &PiUiAction,
        group: &SharedString,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let armed_key = format!("{element}/{row}");
        let armed = self.session_panel.armed_stop.as_deref() == Some(armed_key.as_str());
        let label = if armed { "Stop?" } else { "Stop" };
        let tip = stop.confirm.clone().filter(|_| armed).unwrap_or_else(|| {
            if element.starts_with("bash:") {
                "Stop this command".into()
            } else {
                "Stop this sub-agent".into()
            }
        });
        let (element, row, stop) = (element.to_owned(), row.to_owned(), stop.clone());
        div()
            .id(SharedString::from(format!("session-info-stop-{armed_key}")))
            .debug_selector({
                let armed_key = armed_key.clone();
                move || format!("session-info-stop-{armed_key}")
            })
            .role(gpui::Role::Button)
            .aria_label(label)
            .flex_none()
            .h(px(20.0))
            .px(px(5.0))
            .flex()
            .items_center()
            .gap(px(3.0))
            .rounded(px(5.0))
            .text_size(crate::typography::ui_rems(11.0))
            .text_color(theme.danger)
            .when(!armed, |button| {
                button
                    .invisible()
                    .group_hover(group.clone(), |style| style.visible())
            })
            .when(armed, |button| button.bg(theme.danger.opacity(0.12)))
            .hover(|style| style.bg(theme.danger.opacity(0.16)))
            .tooltip(crate::settings::widgets::text_tooltip(tip))
            .on_click(cx.listener(move |this, _, _, cx| {
                cx.stop_propagation();
                // A stop with nothing to confirm goes at once.
                let confirmed = stop.confirm.is_none()
                    || this.session_panel.armed_stop.as_deref() == Some(armed_key.as_str());
                if confirmed {
                    this.session_panel.armed_stop = None;
                    this.send_fleet_action(element.clone(), row.clone(), &stop, cx);
                } else {
                    this.session_panel.armed_stop = Some(armed_key.clone());
                }
                cx.notify();
            }))
            .child(icon(icons::STOP).size(px(10.0)).text_color(theme.danger))
            .child(label)
            .into_any_element()
    }

    /// Bash commands Pi moved to the background, each with a live timer
    /// and a Stop on hover. Hidden while there are none.
    fn render_background(
        &mut self,
        rows: &[AgentRow],
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        if rows.is_empty() {
            return Vec::new();
        }
        let running = rows.iter().filter(|row| row.running()).count();
        let mut out = vec![
            popover::picker_divider().into_any_element(),
            self.render_section_header(
                BACKGROUND,
                "Background",
                (running > 0).then(|| format!("· {running} running").into()),
                None,
                theme,
                cx,
            ),
        ];
        if !self.session_panel.is_open(BACKGROUND) {
            return out;
        }
        for row in rows {
            out.push(self.render_agent_row(row, theme, cx));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_wide_column_keeps_the_transcript_centered_beside_the_card() {
        assert_eq!(session_card_reserve(1600.0, 736.0), Some(0.0));
        // The centered column's right edge clears the card by the gap.
        let main = 1600.0;
        let card_left = main - SESSION_CARD_WIDTH - SESSION_CARD_INSET;
        assert!(main / 2.0 + 736.0 / 2.0 <= card_left - SESSION_CARD_GAP);
    }

    #[test]
    fn a_tighter_column_shifts_the_transcript_just_enough() {
        let main = 1200.0;
        let reserve = session_card_reserve(main, 736.0).expect("it still fits");
        assert!(reserve > 0.0);
        let card_left = main - SESSION_CARD_WIDTH - SESSION_CARD_INSET;
        let right = (main - reserve) / 2.0 + 736.0 / 2.0;
        assert!((right - (card_left - SESSION_CARD_GAP)).abs() < 0.01);
        // At full width: the column keeps its configured width.
        assert!(main - reserve - 2.0 * TRANSCRIPT_GUTTER >= 736.0);
    }

    #[test]
    fn the_card_stops_fitting_where_the_transcript_would_narrow() {
        // Gutter, column, gap, card, inset.
        let least =
            TRANSCRIPT_GUTTER + 736.0 + SESSION_CARD_GAP + SESSION_CARD_WIDTH + SESSION_CARD_INSET;
        assert!(session_card_reserve(least, 736.0).is_some());
        assert!(session_card_reserve(least - 1.0, 736.0).is_none());
        assert!(session_card_reserve(700.0, 736.0).is_none());
        // A narrower reading column leaves room sooner.
        assert!(session_card_reserve(least - 1.0, 560.0).is_some());
    }

    #[test]
    fn long_directories_keep_their_last_two_folders() {
        assert_eq!(compact_path("/srv/app"), "/srv/app");
        assert_eq!(compact_path("/srv/work/zeron/crates/ui"), "…/crates/ui");
        assert_eq!(compact_path(r"D:\zeron"), r"D:\zeron");
        assert_eq!(compact_path(r"D:\work\zeron\crates\ui"), r"…\crates\ui");
    }

    #[test]
    fn goal_statuses_read_plainly() {
        assert_eq!(goal_status("active"), ("Running", true));
        assert_eq!(goal_status("waiting"), ("Waiting", false));
        assert_eq!(goal_status("budget_limited"), ("Budget reached", false));
        assert_eq!(goal_status("something-new"), ("Set", false));
    }

    #[test]
    fn diff_frames_keep_totals_and_drop_patches() {
        let diff = |checkout: &str, additions: u32| {
            serde_json::json!({
                "checkoutId": checkout, "deviceId": "dev", "cwd": "/repo",
                "patch": "diff --git a/x b/x\n+line\n", "files": [],
                "additions": additions, "deletions": 1, "truncated": false,
                "checksum": format!("{checkout}-{additions}"),
                "updatedAt": "2026-10-14T00:00:00Z",
            })
        };
        let mut panel = SessionPanel::default();
        let all = DiffFrame::decode(serde_json::json!([diff("a", 3)])).unwrap();
        assert!(panel.apply(all));
        assert_eq!(panel.diffs[0].additions, 3);
        assert!(panel.diffs[0].patch.is_empty());
        // One checkout's update replaces its totals; a repeat changes nothing.
        assert!(panel.apply(DiffFrame::decode(diff("a", 5)).unwrap()));
        assert!(!panel.apply(DiffFrame::decode(diff("a", 5)).unwrap()));
        assert!(panel.apply(DiffFrame::decode(diff("b", 1)).unwrap()));
        assert_eq!(
            panel
                .diffs
                .iter()
                .map(|d| (d.checkout_id.as_str(), d.additions))
                .collect::<Vec<_>>(),
            [("a", 5), ("b", 1)]
        );
        assert!(DiffFrame::decode(serde_json::json!({"nope": true})).is_none());
    }
}
