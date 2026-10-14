//! The sidebar's activity digest (docs/reference/sidebar-activity.md): a
//! "Needs input" and a "Working" group above the session lists, each listing
//! the chats in that state across projects. The digest mirrors: every chat
//! keeps its own row and place below, so a status change never moves a row.
//! A group shows only while it has members.

use super::*;
use zeron_proto::ChatIndicator;
use zeron_proto::view::Activity;

/// A digest row: one line, the compact sidebar row's height.
pub(super) const ACTIVITY_ROW_HEIGHT: f32 = 29.0;
/// Rows per group before a "Show N more" row.
pub(super) const ACTIVITY_CAP: usize = 5;

/// The digest's session-transient state.
pub(super) struct SidebarActivity {
    /// Needs input, Working: unfolded.
    pub open: [bool; 2],
    /// Needs input, Working: "Show N more" clicked.
    pub expanded: [bool; 2],
    /// Members held still while a session drag runs, so the rows under the
    /// pointer cannot shift mid-drag.
    pub frozen: Option<Vec<(Activity, String)>>,
    /// The digest's laid-out height above the session lists; the pinned
    /// drag maps the pointer past it.
    pub lead: f32,
}

impl Default for SidebarActivity {
    fn default() -> Self {
        Self {
            open: [true, true],
            expanded: [false, false],
            frozen: None,
            lead: 0.0,
        }
    }
}

fn slot(kind: Activity) -> usize {
    match kind {
        Activity::NeedsInput => 0,
        Activity::Working => 1,
    }
}

fn label(kind: Activity) -> &'static str {
    match kind {
        Activity::NeedsInput => "Needs input",
        Activity::Working => "Working",
    }
}

/// Whether a group shows: never empty, and Working stays away when its only
/// member is the selected chat (that row already spins). Needs input always
/// shows.
pub(super) fn digest_visible(kind: Activity, members: &[String], selected: Option<&str>) -> bool {
    match members {
        [] => false,
        [only] if kind == Activity::Working => selected != Some(only.as_str()),
        _ => true,
    }
}

/// Rows a group draws, and how many it hides behind "Show N more".
pub(super) fn group_rows(total: usize, expanded: bool) -> (usize, usize) {
    if expanded || total <= ACTIVITY_CAP {
        (total, 0)
    } else {
        (ACTIVITY_CAP, total - ACTIVITY_CAP)
    }
}

/// An open group's body: the inset, then `rows` rows and their gaps.
pub(super) fn group_body_height(rows: usize) -> f32 {
    spaces::SIDEBAR_DISCLOSURE_BODY_INSET
        + rows as f32 * ACTIVITY_ROW_HEIGHT
        + rows.saturating_sub(1) as f32 * SIDEBAR_LIST_GAP
}

/// The digest's height for its groups (open, rows drawn): headers, open
/// bodies, a section gap between groups and one after the last.
pub(super) fn digest_height(groups: &[(bool, usize)]) -> f32 {
    groups
        .iter()
        .map(|(open, rows)| {
            spaces::SIDEBAR_DISCLOSURE_HEADER_HEIGHT
                + if *open { group_body_height(*rows) } else { 0.0 }
        })
        .sum::<f32>()
        + groups.len() as f32 * spaces::SIDEBAR_SECTION_GAP
}

impl Shell {
    /// The digest above the session lists, `None` while nothing asks or
    /// runs. Records its height in [`SidebarActivity::lead`].
    pub(super) fn render_attention_block(
        &mut self,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let now = Utc::now();
        let dragging = self.pinned_session_drag.is_some()
            || self.sidebar_session_transfer.is_some()
            || self.sidebar_session_return.is_some();
        let members = match self.sidebar_activity.frozen.clone().filter(|_| dragging) {
            Some(frozen) => frozen,
            None => {
                let members: Vec<(Activity, String)> = self
                    .state
                    .read(cx)
                    .activity_chats(now, self.settings.space_filter.as_deref())
                    .into_iter()
                    .map(|(kind, chat)| (kind, chat.id.clone()))
                    .collect();
                self.sidebar_activity.frozen = dragging.then(|| members.clone());
                members
            }
        };
        let selected = self.state.read(cx).selected_chat.clone();
        let groups: Vec<(Activity, Vec<String>)> = [Activity::NeedsInput, Activity::Working]
            .into_iter()
            .map(|kind| {
                let ids: Vec<String> = members
                    .iter()
                    .filter(|(member, _)| *member == kind)
                    .map(|(_, id)| id.clone())
                    .collect();
                (kind, ids)
            })
            .filter(|(kind, ids)| digest_visible(*kind, ids, selected.as_deref()))
            .collect();
        if groups.is_empty() {
            self.sidebar_activity.lead = 0.0;
            return None;
        }
        let mut layout = Vec::with_capacity(groups.len());
        let mut elements = Vec::with_capacity(groups.len());
        for (ix, (kind, ids)) in groups.into_iter().enumerate() {
            let group = slot(kind);
            let open = self.sidebar_activity.open[group];
            let (shown, more) = group_rows(ids.len(), self.sidebar_activity.expanded[group]);
            let rows = shown + usize::from(more > 0);
            layout.push((open, rows));
            elements.push(self.render_activity_group(
                kind,
                &ids,
                shown,
                more,
                open,
                ix > 0,
                selected.as_deref(),
                theme,
                cx,
            ));
        }
        self.sidebar_activity.lead = digest_height(&layout);
        Some(
            div()
                .id("sidebar-attention")
                .debug_selector(|| "sidebar-attention".into())
                .w_full()
                .flex_none()
                .flex()
                .flex_col()
                .pb(px(spaces::SIDEBAR_SECTION_GAP))
                .children(elements)
                .into_any_element(),
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn render_activity_group(
        &mut self,
        kind: Activity,
        ids: &[String],
        shown: usize,
        more: usize,
        open: bool,
        gap_above: bool,
        selected: Option<&str>,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let group = slot(kind);
        let name = match kind {
            Activity::NeedsInput => "needs",
            Activity::Working => "working",
        };
        let motion_key = format!("attention:{name}");
        let body_height = group_body_height(shown + usize::from(more > 0));
        let glyph = match kind {
            Activity::NeedsInput => icon(icons::DANGER_TRIANGLE)
                .size(px(12.0))
                .text_color(spaces::status_dot_color(
                    ChatIndicator::AwaitingInput,
                    theme,
                ))
                .into_any_element(),
            Activity::Working => loaders::mini_glyph_spinner(
                "attn-working-header",
                2.0,
                theme.glyph,
                self.sidebar_pane.entity_id(),
                cx,
            )
            .into_any_element(),
        };
        let glyph = div()
            .size(px(13.0))
            .flex_none()
            .flex()
            .items_center()
            .justify_center()
            .child(glyph)
            .into_any_element();
        let chevron = self.sidebar_disclosure_chevron(&motion_key, open, theme);
        let toggle_key = motion_key.clone();
        let header = spaces::sidebar_disclosure_header(
            theme,
            Some(glyph),
            label(kind).into(),
            (!open).then_some(ids.len()),
            None,
            chevron,
        )
        .id(SharedString::from(format!("attention-{name}-header")))
        .debug_selector(move || format!("attention-{name}-header"))
        .on_click(cx.listener(move |this, _, _, cx| {
            let was_open = this.sidebar_activity.open[group];
            this.cancel_pinned_session_drag(cx);
            this.begin_sidebar_disclosure_motion(
                &toggle_key,
                if was_open { body_height } else { 0.0 },
                if was_open { 0.0 } else { body_height },
            );
            this.sidebar_activity.open[group] = !was_open;
            // The disclosure owns this movement, not the resort glide.
            this.sidebar_prev_order.clear();
            this.sidebar_resort.clear();
            this.sidebar_new_keys.clear();
            cx.notify();
        }));
        let now = Utc::now();
        let mut rows: Vec<AnyElement> = ids
            .iter()
            .take(shown)
            .map(|id| {
                self.render_activity_row(kind, id, selected == Some(id.as_str()), now, theme, cx)
            })
            .collect();
        if more > 0 {
            rows.push(
                div()
                    .id(SharedString::from(format!("attention-{name}-more")))
                    .debug_selector(move || format!("attention-{name}-more"))
                    .h(px(ACTIVITY_ROW_HEIGHT))
                    .flex_none()
                    .flex()
                    .items_center()
                    .px(px(Theme::SPACE_SM))
                    .rounded(px(8.0))
                    .cursor_pointer()
                    .hover(|el| el.bg(theme.glass_hover()))
                    .text_size(crate::typography::ui_rems(12.0))
                    .text_color(theme.text_muted.opacity(0.7))
                    .child(SharedString::from(format!("Show {more} more")))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.sidebar_activity.expanded[group] = true;
                        cx.notify();
                    }))
                    .into_any_element(),
            );
        }
        let content = div()
            .w_full()
            .flex()
            .flex_col()
            .pt(px(spaces::SIDEBAR_DISCLOSURE_BODY_INSET))
            .gap(px(SIDEBAR_LIST_GAP))
            .children(rows)
            .into_any_element();
        let body = self.render_sidebar_disclosure_body(&motion_key, open, body_height, content);
        div()
            .id(SharedString::from(format!("attention-{name}")))
            .debug_selector(move || format!("attention-{name}"))
            .w_full()
            .flex()
            .flex_col()
            .when(gap_above, |el| el.pt(px(spaces::SIDEBAR_SECTION_GAP)))
            .child(header)
            .child(body)
            .into_any_element()
    }

    /// One digest row: status glyph, title, the subagent pill while working,
    /// and where the chat lives (its project, or the session a side chat
    /// hangs off). Click opens it as its own row would; right-click opens
    /// the chat menu.
    fn render_activity_row(
        &self,
        kind: Activity,
        id: &str,
        selected: bool,
        now: DateTime<Utc>,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let (title, context, subagents) = {
            let state = self.state.read(cx);
            let Some(chat) = state.chats.iter().find(|chat| chat.id == id) else {
                return div().into_any_element();
            };
            let title = transcript::single_line(
                chat.title
                    .as_deref()
                    .or(chat.last_message_preview.as_deref())
                    .unwrap_or("New session"),
            );
            let context = match chat.parent_chat_id.as_deref() {
                Some(parent) => state
                    .chats
                    .iter()
                    .find(|chat| chat.id == parent)
                    .and_then(|parent| parent.title.as_deref())
                    .map(|parent| format!("in {}", transcript::single_line(parent)))
                    .unwrap_or_default(),
                None => state
                    .space_for_chat(chat)
                    .map(|space| space.display_name().to_string())
                    .unwrap_or_default(),
            };
            (title, context, state.running_subagents_for(id, now))
        };
        let row_id = format!("attn-{id}");
        let fade_key = format!("{row_id}-hover");
        let glyph = match kind {
            Activity::NeedsInput => icon(icons::DANGER_TRIANGLE)
                .size(px(11.0))
                .text_color(spaces::status_dot_color(
                    ChatIndicator::AwaitingInput,
                    theme,
                ))
                .into_any_element(),
            Activity::Working => loaders::mini_glyph_spinner(
                format!("{row_id}-working"),
                2.0,
                theme.glyph,
                self.sidebar_pane.entity_id(),
                cx,
            )
            .into_any_element(),
        };
        let selected_wash = crate::theme::glass_selected_bg();
        let rest_bg = if selected {
            selected_wash
        } else {
            crate::theme::wash(0.0)
        };
        let hover_bg = if selected {
            selected_wash
        } else {
            theme.glass_hover()
        };
        let rest_text = if selected {
            theme.text
        } else {
            theme.text.opacity(0.8)
        };
        let aria = SharedString::from(format!("{title}, {}", label(kind)));
        let pill = (kind == Activity::Working && subagents > 0)
            .then(|| crate::running_pill::running_pill(format!("{row_id}-pill"), subagents, theme));
        let open_id = id.to_owned();
        let menu_id = id.to_owned();
        let selector = format!("attention-row-{id}");
        let status_selector = format!("attention-status-{id}");
        div()
            .id(SharedString::from(row_id.clone()))
            .debug_selector(move || selector.clone())
            .h(px(ACTIVITY_ROW_HEIGHT))
            .flex_none()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(6.0))
            .px(px(Theme::SPACE_SM))
            .rounded(px(8.0))
            .cursor_pointer()
            .role(gpui::Role::Button)
            .aria_label(aria)
            .text_color(motion::hover_blend(&fade_key, rest_text, theme.text))
            .bg(motion::hover_blend(&fade_key, rest_bg, hover_bg))
            .on_hover(motion::hover_listener(fade_key.clone()))
            .child(
                div()
                    .debug_selector(move || status_selector.clone())
                    .size(px(13.0))
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(glyph),
            )
            .child(sidebar_faded_label(
                format!("{row_id}-title").into(),
                true,
                div()
                    .text_size(crate::typography::ui_rems(13.0))
                    .line_height(px(17.0))
                    .child(SharedString::from(title)),
            ))
            .children(pill)
            .when(!context.is_empty(), |el| {
                el.child(
                    div()
                        .flex_none()
                        .max_w(px(96.0))
                        .truncate()
                        .text_size(crate::typography::ui_rems(11.0))
                        .text_color(theme.text_muted.opacity(0.5))
                        .child(SharedString::from(context)),
                )
            })
            .on_click(cx.listener(move |this, _, _, cx| {
                this.open_activity_chat(open_id.clone(), cx);
            }))
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                    this.chat_menu.open(ChatMenuState {
                        tab: None,
                        chat_id: menu_id.clone(),
                        position: event.position,
                        page: ChatMenuPage::Root,
                    });
                    cx.notify();
                }),
            )
            .into_any_element()
    }

    /// A digest row's click: a side chat opens beside its listed parent, as
    /// its nested row does; any other chat opens as the session.
    fn open_activity_chat(&mut self, chat_id: String, cx: &mut Context<Self>) {
        let nested = self
            .sidebar_child_parent(&chat_id, cx)
            .is_some_and(|parent| {
                self.state
                    .read(cx)
                    .chats
                    .iter()
                    .any(|chat| chat.id == parent && !chat.archived && chat.is_top_level())
            });
        if nested {
            self.open_sidebar_child(chat_id, false, cx);
        } else {
            self.open_chat(chat_id, cx);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(ids: &[&str]) -> Vec<String> {
        ids.iter().map(|id| (*id).to_owned()).collect()
    }

    #[test]
    fn a_lone_selected_working_chat_needs_no_group() {
        assert!(!digest_visible(Activity::Working, &[], None));
        assert!(!digest_visible(Activity::NeedsInput, &[], None));
        assert!(!digest_visible(Activity::Working, &ids(&["a"]), Some("a")));
        assert!(digest_visible(Activity::Working, &ids(&["a"]), Some("b")));
        assert!(digest_visible(
            Activity::Working,
            &ids(&["a", "b"]),
            Some("a")
        ));
        assert!(digest_visible(
            Activity::NeedsInput,
            &ids(&["a"]),
            Some("a")
        ));
    }

    #[test]
    fn long_groups_cap_until_expanded() {
        assert_eq!(group_rows(3, false), (3, 0));
        assert_eq!(group_rows(ACTIVITY_CAP, false), (ACTIVITY_CAP, 0));
        assert_eq!(group_rows(8, false), (ACTIVITY_CAP, 3));
        assert_eq!(group_rows(8, true), (8, 0));
    }

    #[test]
    fn the_digest_height_matches_its_layout() {
        assert_eq!(
            ACTIVITY_ROW_HEIGHT,
            sidebar_row_height(true, true, false, false)
        );
        assert_eq!(digest_height(&[]), 0.0);
        // Header 28, inset 4, two rows and a gap, then the trailing gap.
        assert_eq!(group_body_height(2), 4.0 + 58.0 + 2.0);
        assert_eq!(digest_height(&[(true, 2)]), 28.0 + 64.0 + 12.0);
        // A folded group is its header.
        assert_eq!(
            digest_height(&[(true, 1), (false, 4)]),
            28.0 + 33.0 + 28.0 + 24.0
        );
    }
}
