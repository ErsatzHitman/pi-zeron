//! The session panel: where this chat runs and what is working under it, in
//! one card at the transcript's right. Its essentials (machine, project,
//! working directory, session, branch and changes) always show. The goal,
//! the agent's todo list, the sub-agents and sub-sessions, a Pi workflow
//! run, the commands Pi runs in the background, and Pi's auto-compaction
//! once the context nears where it compacts each get a section only while
//! active: a
//! section grows in at the bottom when its work starts and folds away when
//! it ends (see `sections.rs`). A section's header folds it.
//!
//! While the column has room the panel stays docked at its right, the
//! transcript and composer shifting left as needed to clear it. Once
//! clearing it would narrow the transcript, the panel steps aside, and the
//! titlebar's toggle floats it over the transcript instead.
use super::*;

use std::cell::Cell;
use std::collections::HashSet;
use std::rc::Rc;

use gpui::Animation;
use zeron_proto::extension_ui::{ExtensionUiAction, PiUiAction};
use zeron_proto::{Chat, ChatIndicator, CheckoutDiff};

mod agents;
mod compact;
mod sections;
mod todos;
mod workflow;

use crate::settings::accounts;
use crate::todo_panel::{self, FoldSide, TodoPanelState, TodoRow};
use agents::AgentList;
pub(super) use agents::{AgentKind, AgentRow};
use compact::{CompactMeter, Phase};
use sections::{ActiveSections, Entry, SectionKey, stack_height};
use todos::TodoList;
use workflow::{RunPhase, TaskRow, WorkflowView};
use zeron_proto::TodoItem;
use zeron_proto::extension_ui::{CompactStatus, WorkflowAction, WorkflowState, WorkflowTaskState};

/// The card's width, its inset from the column's right edge, and the
/// least gap the reading column keeps from it.
pub(super) const SESSION_CARD_WIDTH: f32 = 272.0;
pub(super) const SESSION_CARD_INSET: f32 = 12.0;
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
const COMPACT: &str = "compact";
const WORKFLOW: &str = "workflow";
/// The Compaction section's body: the bar line, a gap, the caption line and
/// its bottom padding.
const COMPACT_BODY_HEIGHT: f32 = 40.0;

/// The panel's own state, held on the shell across chats.
#[derive(Default)]
pub(super) struct SessionPanel {
    /// Sections the user folded away.
    folded: HashSet<&'static str>,
    /// Previous agents stays closed until opened.
    previous_open: bool,
    /// The sub-agent whose Stop asked for a second click (also a workflow's
    /// Stop or Discard, keyed `workflow/<run>/<action>`).
    armed_stop: Option<String>,
    /// The Workflow section's Queued and Finished groups are revealed.
    workflow_queued_open: bool,
    workflow_finished_open: bool,
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
    /// The sections up now, in the order they became active.
    sections: ActiveSections<SectionData>,
    body_scroll: gpui::ScrollHandle,
    /// While a newcomer grows in, the body keeps scrolled to the bottom so
    /// it is seen.
    follow_until: Option<std::time::Instant>,
    /// A side chat's title is being edited in its row: Sub-agents stays up
    /// even with nothing running.
    pub(super) rename_pin: bool,
    /// The chat whose Compaction section is up, for its hysteresis.
    compact_shown: (String, bool),
    /// The card's painted height, which the btw pane opens at.
    card_height: Rc<Cell<f32>>,
}

/// What a section draws from: moved in each frame, kept by a section
/// folding away.
enum SectionData {
    Goal(serde_json::Value),
    Todos(TodoList),
    Agents(AgentList),
    Background(Vec<AgentRow>),
    Compact(CompactMeter),
    Workflow(WorkflowView),
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
    pub(super) fn card_height(&self) -> f32 {
        self.card_height.get()
    }

    fn is_open(&self, section: &'static str) -> bool {
        !self.folded.contains(section)
    }

    /// Show and unfold Sub-agents, so a side chat's row can take an inline
    /// rename.
    pub(super) fn reveal_agents(&mut self) -> bool {
        self.rename_pin = true;
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

/// [`session_card_reserve`] with `beside` px more at the card's left (the
/// btw pane): the transcript shifts further left for it, as far as it can
/// without narrowing. `None` only when the card itself does not fit.
pub(super) fn session_card_reserve_beside(main: f32, transcript: f32, beside: f32) -> Option<f32> {
    let card = session_card_reserve(main, transcript)?;
    let left = main - SESSION_CARD_WIDTH - SESSION_CARD_INSET - beside;
    let wanted = (main + transcript - 2.0 * (left - SESSION_CARD_GAP)).max(0.0);
    Some(
        wanted
            .min(main - 2.0 * TRANSCRIPT_GUTTER - transcript)
            .max(card),
    )
}

/// A section clipped to `t` of its height and faded with it. A probe records
/// the content's painted height; until the first one, `estimate` stands in.
fn section_frame(
    id: &'static str,
    t: f32,
    estimate: f32,
    measured: Rc<Cell<f32>>,
    rows: Vec<AnyElement>,
) -> AnyElement {
    let natural = match measured.get() {
        height if height > 0.0 => height,
        _ => estimate,
    };
    let probe = gpui::canvas(
        move |bounds, _, _| measured.set(f32::from(bounds.size.height)),
        |_, _, _, _| {},
    )
    .absolute()
    .inset_0();
    // It re-pads by the card's inset: dividers bleed through it, and a frame
    // clipping at the content's width would cut them.
    let content = div()
        .relative()
        .w_full()
        .flex()
        .flex_col()
        .gap(px(popover::MENU_GAP))
        .pt(px(popover::MENU_GAP))
        .px(px(popover::CARD_INSET))
        .child(probe)
        .children(rows);
    let frame = div()
        .id(SharedString::from(format!("session-info-section-{id}")))
        .debug_selector(move || format!("session-info-section-{id}"))
        .w_full()
        .flex_none()
        .overflow_hidden()
        .child(content);
    if t >= 0.999 {
        return frame.into_any_element();
    }
    frame.h(px(natural * t)).opacity(t).into_any_element()
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

/// The selected chat's sub-agents at work (the Pi fleet's and a harness's),
/// for the sidebar to nest under its row. Only the selected chat's roster
/// and transcript are watched, so other rows keep their "● N" pill.
pub(super) fn sidebar_agents(state: &AppState, chat_id: &str, now: DateTime<Utc>) -> Vec<AgentRow> {
    if state.selected_chat.as_deref() != Some(chat_id) {
        return Vec::new();
    }
    state
        .chats
        .iter()
        .find(|chat| chat.id == chat_id)
        .map_or_else(Vec::new, |chat| agents::agent_list(state, chat, now).active)
}

/// A row's small trailing text.
/// "+N more" under a Workflow fold that lists only its first rows.
fn workflow_more_row(key: &'static str, hidden: usize, theme: &Theme) -> Option<AnyElement> {
    (hidden > 0).then(|| {
        div()
            .debug_selector(move || format!("session-info-workflow-{key}-more"))
            .h(px(22.0))
            .px(px(33.0))
            .flex()
            .items_center()
            .text_size(crate::typography::ui_rems(11.5))
            .text_color(theme.text_muted.opacity(0.7))
            .child(format!("+{hidden} more"))
            .into_any_element()
    })
}

fn trailing_text(text: impl Into<SharedString>, color: gpui::Hsla) -> gpui::Div {
    div()
        .flex_none()
        .text_size(crate::typography::ui_rems(11.0))
        .text_color(color)
        .child(text.into())
}

/// Running time since `started`, ticking while it shows.
pub(super) fn elapsed_label(id: String, started: DateTime<Utc>, color: gpui::Hsla) -> AnyElement {
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
        // The btw pane, easing in beside the card, wants room too.
        let btw = self.eval_tween(self.btw_tween, if self.btw_shown { 1.0 } else { 0.0 });
        session_card_reserve_beside(
            main,
            crate::settings::transcript_width(cx),
            (SESSION_CARD_WIDTH + super::btw_pane::BTW_GAP) * btw,
        )
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
        // The floating card has no room beside it for the btw pane.
        if show {
            self.close_btw_pane(cx);
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
    pub(super) fn send_fleet_action(
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
            self.session_panel.sections.reset();
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

        // ── Sections: only what is active, in the order it became so.
        let mut agents = agents;
        let background = std::mem::take(&mut agents.background);
        let mut active = Vec::new();
        if let Some(goal) = goal {
            let since = goal["startedAt"]
                .as_i64()
                .filter(|ms| *ms > 0)
                .and_then(DateTime::<Utc>::from_timestamp_millis);
            active.push((SectionKey::Goal, since, SectionData::Goal(goal)));
        }
        if let Some(todos) = todos {
            active.push((SectionKey::Todos, None, SectionData::Todos(todos)));
        }
        // A rename keeps the row it edits up; its chat gone, the pin ends.
        let pinned = self.session_panel.rename_pin && !agents.sessions.is_empty();
        if agents.is_active() || pinned {
            let since = agents.since();
            active.push((SectionKey::Agents, since, SectionData::Agents(agents)));
        }
        if agents::background_active(&background) {
            let since = agents::background_since(&background);
            active.push((
                SectionKey::Background,
                since,
                SectionData::Background(background),
            ));
        }
        let (compact, usage) = {
            let state = self.state.read(cx);
            (
                state.extension_ui.of(&chat.id).and_then(CompactStatus::of),
                state.context_usage,
            )
        };
        let shown = matches!(&self.session_panel.compact_shown, (id, true) if *id == chat.id);
        let meter = CompactMeter::of(compact.as_ref(), usage, shown);
        self.session_panel.compact_shown = (chat.id.clone(), meter.is_some());
        if let Some(meter) = meter {
            active.push((
                SectionKey::Compact,
                meter.since(),
                SectionData::Compact(meter),
            ));
        }
        let workflow = {
            let state = self.state.read(cx);
            state
                .extension_ui
                .of(&chat.id)
                .and_then(|ui| WorkflowView::of(ui, now))
        };
        if let Some(view) = workflow {
            active.push((
                SectionKey::Workflow,
                view.since(),
                SectionData::Workflow(view),
            ));
        }
        let frame_time = self.render_time.unwrap_or_else(|| self.motion_time());
        let pulse =
            self.session_panel
                .sections
                .observe(&chat.id, active, frame_time, self.reduced_motion);
        if pulse.animating {
            self.motion_active.set(true);
        }
        if pulse.entered {
            self.session_panel.follow_until = Some(frame_time + crate::motion::COLLAPSE.total());
        }
        let entries = self.session_panel.sections.take();
        let sections: Vec<AnyElement> = entries
            .iter()
            .map(|entry| self.render_section(entry, &chat.id, frame_time, &theme, cx))
            .collect();
        self.session_panel.sections.restore(entries);
        if self
            .session_panel
            .follow_until
            .is_some_and(|until| frame_time < until)
        {
            self.session_panel.body_scroll.scroll_to_bottom();
        }

        let body = div()
            .id("session-info-body")
            .track_scroll(&self.session_panel.body_scroll)
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
            .when(!sections.is_empty(), |body| {
                // The body's gap already spaces the column and each section
                // carries its own top gap, so cancel one. The column bleeds
                // through the body's inset so dividers still reach the
                // card's edges.
                body.child(
                    div()
                        .flex()
                        .flex_col()
                        .mx(px(-popover::CARD_INSET))
                        .mt(px(-popover::MENU_GAP))
                        .children(sections),
                )
            });

        let card = div()
            .w(px(SESSION_CARD_WIDTH))
            .rounded(px(popover::CARD_RADIUS))
            .overflow_hidden()
            .border_1()
            .border_color(theme.border)
            .bg(popover::surface_bg(&theme))
            .when(!docked && !theme.is_frost(), |card| card.shadow_lg())
            .relative()
            .child(body)
            .child({
                let card_height = self.session_panel.card_height.clone();
                gpui::canvas(
                    move |bounds, _, _| card_height.set(f32::from(bounds.size.height)),
                    |_, _, _, _| {},
                )
                .absolute()
                .inset_0()
            });

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
                trailing_text("Needs input", theme.warning).into_any_element()
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
            .child(self.render_btw_button(theme, cx))
            .into_any_element()
    }

    /// btw: a side chat off this one, from the Session row.
    fn render_btw_button(&self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        div()
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
            .cursor_pointer()
            .hover(|style| style.bg(crate::theme::ink(0.08)))
            .tooltip(crate::settings::widgets::text_tooltip("btw (by the way)"))
            .on_click(cx.listener(|this, _, _, cx| {
                cx.stop_propagation();
                this.close_session_info_overlay(cx);
                this.open_btw_pane(cx);
            }))
            .child(
                icon(icons::PLUS)
                    .size(px(13.0))
                    .text_color(theme.text_muted),
            )
            .into_any_element()
    }

    /// One active section, clipped to how far it has appeared.
    fn render_section(
        &mut self,
        entry: &Entry<SectionData>,
        chat_id: &str,
        now: std::time::Instant,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let t = entry.reveal.value(now, self.reduced_motion);
        let (rows, estimate) = match &entry.data {
            SectionData::Goal(goal) => {
                let chars = goal["text"].as_str().unwrap_or_default().chars().count();
                (
                    self.render_goal(goal, theme, cx),
                    stack_height(0) + (chars / 30 + 1) as f32 * 18.0 + 4.0,
                )
            }
            SectionData::Todos(todos) => (
                self.render_todos(todos, chat_id, theme, cx),
                stack_height(todos.items.len().min(8)),
            ),
            SectionData::Agents(list) => (
                self.render_agents(list, theme, cx),
                stack_height(list.sessions.len() + list.active.len()),
            ),
            SectionData::Background(rows) => (
                self.render_background(rows, theme, cx),
                stack_height(rows.len()),
            ),
            SectionData::Compact(meter) => (
                self.render_compact(meter, theme, cx),
                stack_height(0) + popover::MENU_GAP + COMPACT_BODY_HEIGHT,
            ),
            SectionData::Workflow(view) => {
                let rows = view.estimated_rows(
                    self.session_panel.workflow_queued_open,
                    self.session_panel.workflow_finished_open,
                );
                (
                    self.render_workflow(view, theme, cx),
                    stack_height(rows) + 16.0,
                )
            }
        };
        section_frame(entry.key.id(), t, estimate, entry.measured.clone(), rows)
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

    /// The agent's checklist: what is done, what it is on and what is left.
    /// Active while there is a list in play (see [`TodoList::of`]).
    fn render_todos(
        &mut self,
        todos: &TodoList,
        chat_id: &str,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
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

    /// Sub-agents: the sub-sessions this chat hangs off and owns, the
    /// sub-agents at work, and the settled ones under Previous agents.
    fn render_agents(
        &mut self,
        list: &AgentList,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let running = list.running();
        let mut rows = vec![
            popover::picker_divider().into_any_element(),
            self.render_section_header(
                AGENTS,
                "Sub-agents",
                (running > 0).then(|| format!("· {running} running").into()),
                None,
                theme,
                cx,
            ),
        ];
        if !self.session_panel.is_open(AGENTS) {
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
            } => Some(self.render_fleet_stop(element, row_id, stop, theme, cx)),
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
            .children(time)
            .children(stop);
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

    /// A running Pi sub-agent's or background command's Stop, always at the
    /// row's right end: a first click asks ("Stop?"), the second stops it.
    fn render_fleet_stop(
        &self,
        element: &str,
        row: &str,
        stop: &PiUiAction,
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
            .min_w(px(20.0))
            .px(px(if armed { 5.0 } else { 0.0 }))
            .flex()
            .items_center()
            .justify_center()
            .gap(px(3.0))
            .rounded(px(5.0))
            .cursor_pointer()
            .text_size(crate::typography::ui_rems(11.0))
            .text_color(theme.danger)
            .bg(theme.danger.opacity(if armed { 0.12 } else { 0.06 }))
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
            .when(armed, |button| button.child(label))
            .into_any_element()
    }

    /// Bash commands Pi moved to the background, each with a live timer
    /// and a Stop. Active while one runs.
    fn render_background(
        &mut self,
        rows: &[AgentRow],
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
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

    /// Send a workflow action (Pause, Stop, Resume, Discard) to the Pi
    /// workflows extension of the selected chat.
    fn send_workflow_action(&mut self, action: &WorkflowAction, cx: &mut Context<Self>) {
        let request = WorkflowState::send(action);
        self.state
            .update(cx, |state, cx| state.extension_ui_action(request, cx));
    }

    /// A Pi workflow run: its name and goal, a bar toward done, the phase it
    /// is in, the tasks running, retrying or failed (queued and finished ones
    /// fold), its last checkpoint, and Resume or Discard when it stopped
    /// early. Active while the run is (see [`WorkflowView::of`]).
    fn render_workflow(
        &mut self,
        view: &WorkflowView,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let tone = match view.phase {
            RunPhase::Running => theme.accent,
            RunPhase::Retrying | RunPhase::Paused => theme.warning,
            RunPhase::Failed | RunPhase::Interrupted => theme.danger,
            RunPhase::Completed => theme.success,
            RunPhase::Stopped => theme.text_muted,
        };
        let spinner = view.phase.live().then(|| {
            crate::loaders::mini_glyph_spinner(
                "session-info-workflow-running",
                2.0,
                theme.glyph,
                cx.entity_id(),
                cx,
            )
            .into_any_element()
        });
        let pill = div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(6.0))
            .children(spinner)
            .child(
                div()
                    .flex_none()
                    .px(px(6.0))
                    .rounded_full()
                    .bg(tone.opacity(0.14))
                    .text_size(crate::typography::ui_rems(10.5))
                    .text_color(tone)
                    .child(view.phase.label()),
            );
        let count = (view.total > 0).then(|| format!("· {}/{}", view.done, view.total).into());
        let mut rows = vec![
            popover::picker_divider().into_any_element(),
            self.render_section_header(
                WORKFLOW,
                "Workflow",
                count,
                Some(pill.into_any_element()),
                theme,
                cx,
            ),
        ];
        if !self.session_panel.is_open(WORKFLOW) {
            return rows;
        }

        // ── What it is.
        rows.push(
            div()
                .id("session-info-workflow")
                .debug_selector(|| "session-info-workflow".into())
                .px(px(9.0))
                .flex()
                .flex_col()
                .gap(px(2.0))
                .child(
                    div()
                        .truncate()
                        .text_size(crate::typography::ui_rems(13.0))
                        .line_height(px(18.0))
                        .text_color(theme.text)
                        .child(view.name.clone()),
                )
                .when(!view.goal.is_empty(), |block| {
                    block.child(
                        div()
                            .text_size(crate::typography::ui_rems(11.5))
                            .line_height(px(16.0))
                            .text_color(theme.text_muted)
                            .child(view.goal.clone()),
                    )
                })
                .into_any_element(),
        );

        // ── How far: the bar, the phase, the clock, and Pause / Stop.
        let fraction = view.fraction();
        let caption = view.step.as_ref().map(|(title, index, count)| {
            if *count > 1 && *index > 0 {
                format!("{title} · {index}/{count}")
            } else {
                title.clone()
            }
        });
        let elapsed = view.started.filter(|_| view.phase.live()).map(|started| {
            elapsed_label(
                "session-info-workflow-elapsed".into(),
                started,
                theme.text_muted,
            )
        });
        let controls: Vec<AnyElement> = [view.pause.as_ref(), view.stop.as_ref()]
            .into_iter()
            .flatten()
            .map(|action| self.render_workflow_button(action, false, theme, cx))
            .collect();
        rows.push(
            div()
                .id("session-info-workflow-progress")
                .debug_selector(|| "session-info-workflow-progress".into())
                .px(px(9.0))
                .pb(px(2.0))
                .flex()
                .flex_col()
                .gap(px(4.0))
                .child(
                    div()
                        .h(px(16.0))
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(8.0))
                        .child(
                            div()
                                .flex_1()
                                .h(px(4.0))
                                .rounded_full()
                                .overflow_hidden()
                                .bg(theme.wash(0.08))
                                .child(
                                    div()
                                        .h_full()
                                        .w(gpui::relative(fraction.max(0.015)))
                                        .rounded_full()
                                        .bg(tone.opacity(0.85)),
                                ),
                        )
                        .child(
                            div()
                                .w(px(34.0))
                                .flex_none()
                                .text_right()
                                .text_size(crate::typography::ui_rems(11.0))
                                .text_color(theme.text_muted)
                                .child(format!("{}%", (fraction * 100.0).round() as u32)),
                        ),
                )
                .when(
                    caption.is_some() || elapsed.is_some() || !controls.is_empty(),
                    |block| {
                        block.child(
                            div()
                                .min_h(px(20.0))
                                .flex()
                                .flex_row()
                                .items_center()
                                .gap(px(6.0))
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w_0()
                                        .truncate()
                                        .text_size(crate::typography::ui_rems(11.5))
                                        .text_color(theme.text_muted)
                                        .children(caption),
                                )
                                .children(elapsed)
                                .children(controls),
                        )
                    },
                )
                .into_any_element(),
        );

        // ── The tasks: live and failed work, then the folds.
        for row in &view.lead {
            rows.push(self.render_workflow_task(row, theme, cx));
        }
        let queued = view.queued.len() + view.queued_more as usize;
        if queued > 0 {
            let open = self.session_panel.workflow_queued_open;
            rows.push(self.render_workflow_group(
                "queued",
                format!("Queued ({queued})"),
                open,
                |panel| panel.workflow_queued_open = !panel.workflow_queued_open,
                theme,
                cx,
            ));
            if open {
                for row in view.queued.iter().take(workflow::GROUP_ROWS) {
                    rows.push(self.render_workflow_task(row, theme, cx));
                }
                let shown = view.queued.len().min(workflow::GROUP_ROWS);
                rows.extend(workflow_more_row("queued", queued - shown, theme));
            }
        }
        if !view.finished.is_empty() {
            let open = self.session_panel.workflow_finished_open;
            rows.push(self.render_workflow_group(
                "finished",
                format!("Finished ({})", view.finished.len()),
                open,
                |panel| panel.workflow_finished_open = !panel.workflow_finished_open,
                theme,
                cx,
            ));
            if open {
                for row in view.finished.iter().take(workflow::GROUP_ROWS) {
                    rows.push(self.render_workflow_task(row, theme, cx));
                }
                let hidden = view.finished.len().saturating_sub(workflow::GROUP_ROWS);
                rows.extend(workflow_more_row("finished", hidden, theme));
            }
        }

        // ── Its durability: the last checkpoint, results reused on resume.
        let mut notes: Vec<String> = Vec::new();
        notes.extend(view.checkpoint.clone());
        notes.extend(view.reuse.clone());
        if view.truncated > 0 {
            notes.push(format!(
                "{} more task{} not listed",
                view.truncated,
                if view.truncated == 1 { "" } else { "s" }
            ));
        }
        if !notes.is_empty() {
            rows.push(
                div()
                    .id("session-info-workflow-notes")
                    .debug_selector(|| "session-info-workflow-notes".into())
                    .px(px(9.0))
                    .pb(px(2.0))
                    .flex()
                    .flex_col()
                    .gap(px(2.0))
                    .text_size(crate::typography::ui_rems(11.0))
                    .text_color(theme.text_muted.opacity(0.85))
                    .children(notes.into_iter().map(|note| div().truncate().child(note)))
                    .into_any_element(),
            );
        }

        // ── Recovery: a run that stopped early can pick up where it left off.
        if let Some(recovery) = &view.recovery {
            let buttons: Vec<AnyElement> = [
                recovery.resume.as_ref().map(|a| (a, true)),
                recovery.discard.as_ref().map(|a| (a, false)),
            ]
            .into_iter()
            .flatten()
            .map(|(action, primary)| self.render_workflow_button(action, primary, theme, cx))
            .collect();
            rows.push(
                div()
                    .id("session-info-workflow-recovery")
                    .debug_selector(|| "session-info-workflow-recovery".into())
                    .mx(px(9.0))
                    .mb(px(4.0))
                    .px(px(8.0))
                    .py(px(6.0))
                    .rounded(px(6.0))
                    .bg(theme.wash(0.05))
                    .flex()
                    .flex_col()
                    .gap(px(6.0))
                    .child(
                        div()
                            .text_size(crate::typography::ui_rems(11.5))
                            .line_height(px(16.0))
                            .text_color(theme.text_muted)
                            .child(recovery.reason.clone()),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(6.0))
                            .children(buttons),
                    )
                    .into_any_element(),
            );
        }
        rows
    }

    /// One workflow task: its state, label, and a timer or what it is
    /// waiting on; the tooltip has its error and dependencies.
    fn render_workflow_task(
        &self,
        row: &TaskRow,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let id = SharedString::from(format!("session-info-workflow-task-{}", row.id));
        let muted = theme.text_muted.opacity(0.7);
        let glyph: AnyElement = match row.state {
            WorkflowTaskState::Running => crate::loaders::mini_glyph_spinner(
                format!("{id}-running"),
                2.0,
                theme.glyph,
                cx.entity_id(),
                cx,
            )
            .into_any_element(),
            WorkflowTaskState::Retrying => icon(icons::REFRESH)
                .size(px(13.0))
                .text_color(theme.warning)
                .into_any_element(),
            WorkflowTaskState::Failed => icon(icons::DANGER_TRIANGLE)
                .size(px(13.0))
                .text_color(theme.danger)
                .into_any_element(),
            WorkflowTaskState::Aborted => icon(icons::STOP)
                .size(px(11.0))
                .text_color(theme.text_muted)
                .into_any_element(),
            WorkflowTaskState::Done | WorkflowTaskState::Cached => icon(icons::CHECK)
                .size(px(13.0))
                .text_color(theme.text_muted)
                .into_any_element(),
            WorkflowTaskState::Skipped => icon(icons::CLOSE_CIRCLE)
                .size(px(12.0))
                .text_color(theme.text_faint)
                .into_any_element(),
            WorkflowTaskState::Queued | WorkflowTaskState::Other => div()
                .size(px(5.0))
                .rounded_full()
                .bg(theme.text_muted.opacity(0.5))
                .into_any_element(),
        };
        let label_color = match row.state {
            WorkflowTaskState::Running
            | WorkflowTaskState::Retrying
            | WorkflowTaskState::Failed => theme.text,
            _ => theme.text_muted,
        };
        let trailing_color = match row.state {
            WorkflowTaskState::Retrying => theme.warning,
            WorkflowTaskState::Failed => theme.danger,
            _ => muted,
        };
        let trailing = match (row.started, &row.trailing) {
            (Some(started), None) => Some(elapsed_label(format!("{id}-elapsed"), started, muted)),
            (_, Some(text)) => Some(trailing_text(text.clone(), trailing_color).into_any_element()),
            (None, None) => None,
        };
        let mut element = div()
            .id(id.clone())
            .debug_selector({
                let id = id.clone();
                move || id.to_string()
            })
            .flex()
            .flex_row()
            .items_center()
            .gap(px(8.0))
            .h(px(AGENT_ROW_HEIGHT))
            .px(px(9.0))
            .rounded(px(popover::MENU_ITEM_RADIUS))
            .text_size(crate::typography::ui_rems(13.0))
            .text_color(label_color)
            .child(
                div()
                    .size(px(16.0))
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(glyph),
            )
            .child(div().flex_1().min_w_0().truncate().child(row.label.clone()))
            .children(trailing);
        if let Some(detail) = row.detail.clone() {
            element = element.tooltip(crate::settings::widgets::text_tooltip(detail));
        }
        element.into_any_element()
    }

    /// The Queued or Finished fold, styled like Previous agents.
    fn render_workflow_group(
        &self,
        key: &'static str,
        label: String,
        open: bool,
        toggle: fn(&mut SessionPanel),
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        div()
            .id(SharedString::from(format!("session-info-workflow-{key}")))
            .debug_selector(move || format!("session-info-workflow-{key}"))
            .role(gpui::Role::Button)
            .aria_label(SharedString::from(label.clone()))
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
                toggle(&mut this.session_panel);
                cx.notify();
            }))
            .child(label)
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

    /// A workflow action button. One the extension marks `confirm` (Stop,
    /// Discard) asks on the first click and acts on the second.
    fn render_workflow_button(
        &self,
        action: &WorkflowAction,
        primary: bool,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let armed_key = format!("workflow/{}/{}", action.run_id, action.id);
        let armed = self.session_panel.armed_stop.as_deref() == Some(armed_key.as_str());
        let destructive = matches!(action.id.as_str(), "stop" | "discard");
        let tone = if destructive {
            theme.danger
        } else if primary {
            theme.accent
        } else {
            theme.text_muted
        };
        let base = match (action.label.trim(), action.id.as_str()) {
            ("", "stop") => "Stop".to_owned(),
            ("", "pause") => "Pause".to_owned(),
            ("", "resume") => "Resume".to_owned(),
            ("", "discard") => "Discard".to_owned(),
            ("", other) => other.to_owned(),
            (label, _) => workflow::clean(label),
        };
        let label = SharedString::from(if armed { format!("{base}?") } else { base });
        let tip = match action.id.as_str() {
            "stop" if armed => "Click again to stop the workflow",
            "stop" => "Stop the workflow; it can be resumed later",
            "pause" => "Let running tasks finish, then pause",
            "resume" => "Pick up where it left off; finished work is reused",
            "discard" if armed => "Click again to discard this run",
            "discard" => "Stop offering to resume this run",
            _ => "",
        };
        let action = action.clone();
        let element_id = SharedString::from(format!("session-info-workflow-{}", action.id));
        div()
            .id(element_id.clone())
            .debug_selector(move || element_id.to_string())
            .role(gpui::Role::Button)
            .aria_label(label.clone())
            .flex_none()
            .h(px(20.0))
            .px(px(7.0))
            .flex()
            .items_center()
            .rounded(px(5.0))
            .cursor_pointer()
            .text_size(crate::typography::ui_rems(11.0))
            .font_weight(gpui::FontWeight::MEDIUM)
            .text_color(tone)
            .when(primary || armed, |button| button.bg(tone.opacity(0.14)))
            .hover(|style| style.bg(tone.opacity(0.18)))
            .when(!tip.is_empty(), |button| {
                button.tooltip(crate::settings::widgets::text_tooltip(tip))
            })
            .on_click(cx.listener(move |this, _, _, cx| {
                cx.stop_propagation();
                let confirmed = !action.confirm
                    || this.session_panel.armed_stop.as_deref() == Some(armed_key.as_str());
                if confirmed {
                    this.session_panel.armed_stop = None;
                    this.send_workflow_action(&action, cx);
                } else {
                    this.session_panel.armed_stop = Some(armed_key.clone());
                }
                cx.notify();
            }))
            .child(label)
            .into_any_element()
    }

    /// Pi's auto-compaction near its limit, or running: a bar toward where
    /// Pi compacts, and what happens next. Near and compacting share one
    /// fixed-height body, so the switch moves nothing.
    fn render_compact(
        &mut self,
        meter: &CompactMeter,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let compacting = meter.compacting();
        let fraction = meter.fraction();
        let (pill, tone) = if compacting {
            ("Compacting", theme.accent)
        } else if meter.error.is_some() {
            ("Failed", theme.danger)
        } else {
            (
                "Near limit",
                accounts::usage_color(accounts::usage_level(fraction), theme),
            )
        };
        let spinner = compacting.then(|| {
            crate::loaders::mini_glyph_spinner(
                "session-info-compacting",
                2.0,
                theme.glyph,
                cx.entity_id(),
                cx,
            )
            .into_any_element()
        });
        let action = div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(6.0))
            .children(spinner)
            .child(
                div()
                    .flex_none()
                    .px(px(6.0))
                    .rounded_full()
                    .bg(tone.opacity(0.14))
                    .text_size(crate::typography::ui_rems(10.5))
                    .text_color(tone)
                    .child(pill),
            );
        let mut rows = vec![
            popover::picker_divider().into_any_element(),
            self.render_section_header(
                COMPACT,
                "Compaction",
                None,
                Some(action.into_any_element()),
                theme,
                cx,
            ),
        ];
        if !self.session_panel.is_open(COMPACT) {
            return rows;
        }
        // Compacting, the bar is full: the context is past where Pi starts.
        let filled = if compacting { 1.0 } else { fraction };
        let percent = (!compacting).then(|| format!("{}%", (fraction * 100.0).round() as u32));
        let elapsed = match &meter.phase {
            Phase::Compacting {
                since: Some(since), ..
            } => Some(elapsed_label(
                "session-info-compact-elapsed".into(),
                // A remote host's clock may run ahead of this one.
                (*since).min(Utc::now()),
                theme.text_muted,
            )),
            _ => None,
        };
        let caption_color = if meter.error.is_some() && !compacting {
            theme.danger
        } else {
            theme.text_muted
        };
        rows.push(
            div()
                .id("session-info-compact")
                .debug_selector(|| "session-info-compact".into())
                .h(px(COMPACT_BODY_HEIGHT))
                .px(px(9.0))
                .pb(px(4.0))
                .flex()
                .flex_col()
                .gap(px(4.0))
                .child(
                    div()
                        .h(px(16.0))
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(8.0))
                        .child(
                            div()
                                .flex_1()
                                .h(px(4.0))
                                .rounded_full()
                                .overflow_hidden()
                                .bg(theme.wash(0.08))
                                .child(
                                    div()
                                        .h_full()
                                        .w(gpui::relative(filled.max(0.015)))
                                        .rounded_full()
                                        .bg(tone.opacity(0.85)),
                                ),
                        )
                        .child(
                            div()
                                .w(px(34.0))
                                .flex_none()
                                .text_right()
                                .text_size(crate::typography::ui_rems(11.0))
                                .text_color(theme.text_muted)
                                .children(percent),
                        ),
                )
                .child(
                    div()
                        .h(px(16.0))
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(8.0))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .text_size(crate::typography::ui_rems(11.5))
                                .text_color(caption_color)
                                .child(meter.caption()),
                        )
                        .children(elapsed),
                )
                .into_any_element(),
        );
        rows
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_btw_pane_shifts_the_transcript_as_far_as_it_can() {
        let beside = SESSION_CARD_WIDTH + super::super::btw_pane::BTW_GAP;
        // Wide: the column moves left just enough to clear the pane too.
        let reserve = session_card_reserve_beside(1600.0, 736.0, beside).unwrap();
        let pane_left = 1600.0 - SESSION_CARD_WIDTH - SESSION_CARD_INSET - beside;
        assert!(((1600.0 - reserve) / 2.0 + 368.0 - (pane_left - SESSION_CARD_GAP)).abs() < 0.01);
        // Tighter: it stops where the column would narrow, and the pane
        // overlaps the transcript instead.
        let main = 1200.0;
        let reserve = session_card_reserve_beside(main, 736.0, beside).unwrap();
        assert!((reserve - (main - 2.0 * TRANSCRIPT_GUTTER - 736.0)).abs() < 0.01);
        // Nothing beside: the card's own reserve.
        assert_eq!(
            session_card_reserve_beside(main, 736.0, 0.0),
            session_card_reserve(main, 736.0)
        );
        assert_eq!(session_card_reserve_beside(800.0, 736.0, beside), None);
    }

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
