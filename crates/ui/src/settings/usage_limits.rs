//! Settings → Usage limits: plan rate limits for every connected
//! subscription on this device, in one list. Settings → Providers shows the
//! same meters, but only inside one collapsed provider card and only for Pi;
//! Settings → Usage is token/spend accounting from `GetUsageReport` and has
//! nothing to do with plan quotas. This page is the consolidated view: one
//! card per subscription with every window the engine reports (Claude 5h and
//! Week, ChatGPT 5h and Weekly, Cline 5h/weekly/monthly, one row per OpenCode
//! Go key, Copilot month, …), the plan badge, the engine's reason when a
//! probe failed, and when it was last checked.
//!
//! It reads the same `ListAgentAccounts` snapshot as Providers and the
//! composer footer ring (all three share [`AccountsSnapshotCache`]) and shows
//! the share of each window **used**, matching the Providers meters — the
//! footer shows what is left. Every visit forces one probe; the engine
//! throttles it (30 s per account, per-provider backoff), so the page needs
//! no timer of its own.

use chrono::{DateTime, Utc};
use gpui::{
    AnyElement, Context, Entity, SharedString, Subscription, Task, Window, div, prelude::*, px,
};
use zeron_proto::{AgentAccount, AgentAccountsSnapshot, AgentUsageWindow, HarnessId};
use zeron_rpc::methods;

use crate::account_usage;
use crate::popover::{self, Loadable};
use crate::settings::accounts::{self, AccountsCacheKey, AccountsSnapshotCache, LoadTrigger};
use crate::settings::widgets;
use crate::state::AppState;
use crate::theme::Theme;

/// The usage bar geometry: a window label, the bar, the percentage and the
/// reset text share one row down the whole page.
const WINDOW_LABEL_WIDTH: f32 = 64.0;
const WINDOW_BAR_WIDTH: f32 = 160.0;
const WINDOW_PERCENT_WIDTH: f32 = 72.0;

// ---------------------------------------------------------------------------
// Pure: labels, ordering, summary
// ---------------------------------------------------------------------------

/// The product name behind a Pi per-provider login key (`anthropic`,
/// `openai-codex`, …). `None` for a login this build does not name. Pure.
fn pi_provider_label(provider: &str) -> Option<&'static str> {
    Some(match provider {
        "anthropic" => "Claude",
        "openai-codex" => "ChatGPT (Codex)",
        "openai" => "OpenAI",
        "clinepass" => "Cline",
        "opencode-go" => "OpenCode Go",
        "github-copilot" => "GitHub Copilot",
        "nous" => "Nous Portal",
        provider if provider.starts_with("xai") => "Grok",
        _ => return None,
    })
}

/// What one account row is called: the product the user signed in to
/// ("Claude", "OpenCode Go") rather than the harness's provider key. A
/// harness-only login falls back to its provider name ("Claude Code"). Pure.
pub fn subscription_label(account: &AgentAccount) -> String {
    match account.provider.as_deref() {
        Some(provider) => pi_provider_label(provider)
            .map(str::to_string)
            .unwrap_or_else(|| format!("{} ({provider})", accounts::provider_name(account.harness))),
        None => accounts::provider_name(account.harness).to_string(),
    }
}

/// The most-used window still current at `now` — the account's binding limit.
/// `None` when the account reports no windows at all; an account whose only
/// windows have passed their reset reads `Some(0.0)` (the last period's
/// figure is stale, not this one's). Reuses the footer ring's
/// [`account_usage::used_fraction`]. Pure.
pub fn worst_used(account: &AgentAccount, now: DateTime<Utc>) -> Option<f32> {
    if account.usage_windows.is_empty() {
        return None;
    }
    Some(account_usage::used_fraction(account, now).unwrap_or(0.0))
}

/// Every account, worst-used window first, then label, then id. Unlike the
/// Providers cards — which keep the engine's slot order so switching never
/// moves a row — this page is a diagnostic list, so the tightest quota is
/// the most useful thing at the top. Pure.
pub fn ordered(snapshot: &AgentAccountsSnapshot, now: DateTime<Utc>) -> Vec<&AgentAccount> {
    let mut rows: Vec<&AgentAccount> = snapshot.accounts.iter().collect();
    rows.sort_by(|a, b| {
        worst_used(b, now)
            .partial_cmp(&worst_used(a, now))
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| subscription_label(a).cmp(&subscription_label(b)))
            .then_with(|| a.id.cmp(&b.id))
    });
    rows
}

/// The page's three groups: Pi subscriptions with windows (the main list),
/// Pi rows the provider has no quota view for, and other agents' logins
/// found on this device (shown read-only). A harness that reports nothing
/// (Antigravity) is silent, not listed as "no quota". Pure.
pub fn sections<'a>(
    snapshot: &'a AgentAccountsSnapshot,
    now: DateTime<Utc>,
) -> (
    Vec<&'a AgentAccount>,
    Vec<&'a AgentAccount>,
    Vec<&'a AgentAccount>,
) {
    let mut main = Vec::new();
    let mut no_quota = Vec::new();
    let mut other = Vec::new();
    for account in ordered(snapshot, now) {
        // A harness that reports nothing at all (Antigravity) has no missing
        // view to explain — staying silent is the honest state.
        if !accounts::reports_usage(account.harness) {
            continue;
        }
        if account.harness != HarnessId::Pi {
            other.push(account);
        } else if !account.usage_windows.is_empty() {
            main.push(account);
        } else {
            no_quota.push(account);
        }
    }
    (main, no_quota, other)
}

/// When the engine last refreshed this row's windows: "checked just now"
/// under a minute, then minutes, then hours. `None` = never probed. Pure.
pub fn checked_label(fetched_at: Option<i64>, now: DateTime<Utc>) -> String {
    let Some(fetched_at) = fetched_at else {
        return "never checked".to_string();
    };
    let fetched = DateTime::<Utc>::from_timestamp_millis(fetched_at).unwrap_or(now);
    let seconds = now.signed_duration_since(fetched).num_seconds().max(0);
    if seconds < 60 {
        "checked just now".to_string()
    } else if seconds < 60 * 60 {
        format!("checked {}m ago", seconds / 60)
    } else {
        format!("checked {}h ago", seconds / 3600)
    }
}

/// A window as of `now`: its used fraction (a past-due window starts over at
/// 0) and its reset text ("resets 3:45 PM", or "reset, refreshing" until the
/// next probe). Mirrors the Providers meters' [`accounts::current_window`]
/// and [`accounts::usage_resets`] wording so the two never disagree. Pure.
pub fn window_state(window: &AgentUsageWindow, now: DateTime<Utc>) -> (f32, Option<String>) {
    let current = accounts::current_window(window, now);
    let fraction = current.used_fraction.clamp(0.0, 1.0);
    let reset = match window.resets_at {
        Some(at) if at <= now => Some("reset, refreshing".to_string()),
        at => accounts::format_reset(at, now),
    };
    (fraction, reset)
}

/// The line under the page subtitle: "5 subscriptions · Claude 5h 98% used"
/// names the count and the tightest window across them. Pure.
pub fn summary(accounts: &[&AgentAccount], now: DateTime<Utc>) -> String {
    let count = accounts.len();
    let noun = if count == 1 {
        "subscription"
    } else {
        "subscriptions"
    };
    let worst = accounts.iter().find_map(|account| {
        let label = subscription_label(account);
        account
            .usage_windows
            .iter()
            .map(|window| {
                let current = accounts::current_window(window, now);
                (current.used_fraction.clamp(0.0, 1.0), window.label.clone())
            })
            .max_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal))
            .map(|(fraction, window)| (fraction, window, label))
    });
    match worst {
        Some((fraction, window, label)) => format!(
            "{count} {noun} · {label} {window} {}% used",
            (fraction * 100.0).round() as u32
        ),
        None => format!("{count} {noun}"),
    }
}

// ---------------------------------------------------------------------------
// Entity
// ---------------------------------------------------------------------------

pub struct UsageLimitsPage {
    state: Entity<AppState>,
    scroll: widgets::PageScroll,
    snapshot: Loadable<AgentAccountsSnapshot>,
    /// A list is in flight over an already-painted snapshot.
    refreshing: bool,
    load_task: Option<Task<()>>,
    /// A failed revalidation's reason, surfaced as a toast (the painted rows
    /// stay up). `Loadable::Error` is only used when nothing is painted.
    error: Option<SharedString>,
    _observe: Subscription,
}

impl UsageLimitsPage {
    pub fn new(state: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        let observe = cx.observe(&state, |_, _, cx| cx.notify());
        let mut page = Self {
            state,
            scroll: widgets::PageScroll::default(),
            snapshot: Loadable::Idle,
            refreshing: false,
            load_task: None,
            error: None,
            _observe: observe,
        };
        // Paint from the last list (this session) or the engine's persisted
        // usage, then force one probe to update in place — the skeleton only
        // ever shows on a genuinely first load.
        page.load(accounts::force_usage_for(LoadTrigger::Mount), cx);
        page
    }

    fn on_scroll_hovered(&mut self, hovered: &bool, _: &mut Window, cx: &mut Context<Self>) {
        if self.scroll.set_list_hovered(*hovered) {
            cx.notify();
        }
    }

    /// Stale-while-revalidate, shared with Providers and the composer footer:
    /// a painted snapshot stays on screen while the list runs. With nothing
    /// to paint yet, a forced load first takes the engine's plain list (last
    /// known usage, no network) so rows appear at once, then the probed list
    /// replaces it. This device only — device switching stays on Providers.
    fn load(&mut self, force_usage: bool, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            self.snapshot = Loadable::Error("Engine not connected".into());
            return;
        };
        let key = AccountsCacheKey::new(self.state.read(cx), None);
        if !matches!(self.snapshot, Loadable::Ready(_)) {
            self.snapshot = match cx
                .try_global::<AccountsSnapshotCache>()
                .and_then(|cache| cache.0.get(&key))
            {
                Some(cached) => Loadable::Ready(cached.clone()),
                None => Loadable::Loading,
            };
        }
        let paint_first = force_usage && !matches!(self.snapshot, Loadable::Ready(_));
        self.refreshing = true;
        let params = serde_json::json!({ "forceUsage": force_usage });
        let plain = serde_json::json!({ "forceUsage": false });
        self.load_task = Some(cx.spawn(async move |this, cx| {
            let fetch = |params: serde_json::Value| {
                let engine = engine.clone();
                async move {
                    engine
                        .client()
                        .call(methods::LIST_AGENT_ACCOUNTS, params)
                        .await
                        .map_err(|err| err.to_string())
                        .and_then(|value| {
                            serde_json::from_value::<AgentAccountsSnapshot>(value)
                                .map_err(|err| err.to_string())
                        })
                }
            };
            if paint_first && let Ok(snapshot) = fetch(plain).await {
                let key = key.clone();
                this.update(cx, |page, cx| {
                    cx.default_global::<AccountsSnapshotCache>()
                        .0
                        .insert(key, snapshot.clone());
                    page.snapshot = Loadable::Ready(snapshot);
                    cx.notify();
                })
                .ok();
            }
            let result = fetch(params).await;
            this.update(cx, |page, cx| {
                page.refreshing = false;
                match result {
                    Ok(snapshot) => {
                        cx.default_global::<AccountsSnapshotCache>()
                            .0
                            .insert(key, snapshot.clone());
                        page.snapshot = Loadable::Ready(snapshot);
                    }
                    // A failed revalidation keeps the painted rows.
                    Err(err) if matches!(page.snapshot, Loadable::Ready(_)) => {
                        page.error = Some(err.into());
                    }
                    Err(err) => page.snapshot = Loadable::Error(err),
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }
}

impl popover::ScrollRailHost for UsageLimitsPage {
    fn rail_bar(&mut self) -> &mut popover::MenuScrollbarState {
        self.scroll.rail_bar()
    }

    fn rail_scroll(&self) -> Option<gpui::ScrollHandle> {
        self.scroll.rail_scroll()
    }
}

// ---------------------------------------------------------------------------
// Render
// ---------------------------------------------------------------------------

/// The muted meta line under a card's title: the identity fragments joined
/// by dots (email, display name, organization, id).
fn identity_line(account: &AgentAccount, theme: &Theme) -> Option<AnyElement> {
    let mut fragments: Vec<String> = Vec::new();
    if let Some(email) = account.email.clone() {
        fragments.push(email);
    }
    if let Some(name) = account.display_name.clone()
        && !fragments.contains(&name)
    {
        fragments.push(name);
    }
    if let Some(org) = account.organization.clone()
        && !fragments.iter().any(|f| org.starts_with(f.as_str()))
    {
        fragments.push(org);
    }
    fragments.push(account.id.clone());
    Some(
        widgets::meta_line(
            theme,
            fragments
                .into_iter()
                .map(|fragment| div().child(SharedString::from(fragment)).into_any_element())
                .collect(),
        )
        .into_any_element(),
    )
}

/// One window row: label, bar (80% amber / 95% red), "% used", reset text.
fn render_window_row(window: &AgentUsageWindow, theme: &Theme, now: DateTime<Utc>) -> AnyElement {
    let (fraction, reset) = window_state(window, now);
    let level = accounts::usage_level(fraction);
    let color = accounts::usage_color(level, theme);
    div()
        .h(px(20.0))
        .flex()
        .flex_row()
        .items_center()
        .gap(px(10.0))
        .text_size(crate::typography::ui_rems(12.0))
        .child(
            div()
                .w(px(WINDOW_LABEL_WIDTH))
                .flex_none()
                .truncate()
                .text_color(theme.text_muted)
                .child(SharedString::from(window.label.clone())),
        )
        .child(
            div()
                .w(px(WINDOW_BAR_WIDTH))
                .flex_none()
                .h(px(6.0))
                .rounded_full()
                .overflow_hidden()
                .bg(theme.wash(0.08))
                .when(fraction > 0.0, |el| {
                    el.child(
                        div()
                            .h_full()
                            // A 1.5% floor keeps tiny non-zero usage visible,
                            // matching the Providers meters.
                            .w(gpui::relative(fraction.max(0.015)))
                            .rounded_full()
                            .bg(color.opacity(0.85)),
                    )
                }),
        )
        .child(
            div()
                .w(px(WINDOW_PERCENT_WIDTH))
                .flex_none()
                .text_right()
                .text_color(match level {
                    accounts::UsageLevel::Normal => theme.text_muted,
                    _ => color,
                })
                .child(SharedString::from(format!(
                    "{}% used",
                    (fraction * 100.0).round() as u32
                ))),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .text_color(theme.text_faint)
                .children(reset),
        )
        .into_any_element()
}

/// One subscription card: product name, plan badge, "In use now", identity,
/// every window (or the engine's reason there are none), last-checked line.
fn render_card(
    account: &AgentAccount,
    theme: &Theme,
    now: DateTime<Utc>,
    tag: Option<&str>,
) -> AnyElement {
    let mut header = div()
        .flex()
        .flex_row()
        .flex_wrap()
        .items_center()
        .gap(px(8.0))
        .child(widgets::row_title(theme, subscription_label(account)));
    if let Some(plan) = account.plan_label.clone() {
        header = header.child(widgets::badge(theme, plan));
    }
    if account.active {
        header = header.child(widgets::badge_active(theme, "In use now"));
    }
    if let Some(tag) = tag {
        header = header.child(widgets::badge(theme, tag.to_string()));
    }

    let mut rows: Vec<AnyElement> = Vec::new();
    if account.usage_windows.is_empty() {
        if let Some(reason) = account.usage_error.clone() {
            rows.push(
                div()
                    .text_size(crate::typography::ui_rems(12.0))
                    .text_color(theme.text_muted)
                    .child(SharedString::from(reason))
                    .into_any_element(),
            );
        } else if accounts::reports_usage(account.harness) {
            rows.push(
                div()
                    .text_size(crate::typography::ui_rems(12.0))
                    .text_color(theme.text_muted)
                    .child(SharedString::from("No usage view for this login"))
                    .into_any_element(),
            );
        }
    } else {
        rows.extend(
            account
                .usage_windows
                .iter()
                .map(|window| render_window_row(window, theme, now)),
        );
        // Stale windows stay up with the engine's reason beside them.
        if let Some(error) = account.usage_error.clone() {
            rows.push(
                div()
                    .text_size(crate::typography::ui_rems(12.0))
                    .text_color(theme.danger)
                    .child(SharedString::from(error))
                    .into_any_element(),
            );
        }
    }
    rows.push(
        div()
            .text_size(crate::typography::ui_rems(11.5))
            .text_color(theme.text_faint)
            .child(SharedString::from(checked_label(account.usage_fetched_at, now)))
            .into_any_element(),
    );

    let mut card = widgets::section_card(theme)
        .p(px(16.0))
        .child(header);
    if let Some(identity) = identity_line(account, theme) {
        card = card.child(identity);
    }
    card.child(div().mt(px(8.0)).flex().flex_col().gap(px(4.0)).children(rows))
        .into_any_element()
}

/// A group heading above a block of cards.
fn group_header(theme: &Theme, title: &str, note: &str) -> AnyElement {
    div()
        .mt(px(24.0))
        .flex()
        .flex_col()
        .gap(px(2.0))
        .child(widgets::section_label(theme, title.to_string()))
        .child(
            div()
                .px(px(8.0))
                .text_size(crate::typography::ui_rems(11.5))
                .text_color(theme.text_faint)
                .child(SharedString::from(note.to_string())),
        )
        .into_any_element()
}

fn footer_note(theme: &Theme, text: &str) -> AnyElement {
    div()
        .text_size(crate::typography::ui_rems(11.5))
        .line_height(px(17.0))
        .text_color(theme.text_muted)
        .child(SharedString::from(text.to_string()))
        .into_any_element()
}

impl Render for UsageLimitsPage {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        crate::toast::take_error(&mut self.error, cx);
        let theme = Theme::of(cx).for_settings_surface();
        let now = Utc::now();
        let refreshing = self.refreshing || matches!(self.snapshot, Loadable::Loading);

        let snapshot = self.snapshot.ready().cloned();
        let (main, no_quota, other) = match &snapshot {
            Some(snapshot) => sections(snapshot, now),
            None => (Vec::new(), Vec::new(), Vec::new()),
        };
        let subscriptions: Vec<&AgentAccount> =
            main.iter().chain(no_quota.iter()).copied().collect();
        let count = subscriptions.len() + other.len();
        let summary_line = (count > 0).then(|| summary(&subscriptions, now));

        let body: Vec<AnyElement> = match &self.snapshot {
            Loadable::Idle | Loadable::Loading => vec![
                div()
                    .mt(px(72.0))
                    .text_center()
                    .text_size(crate::typography::ui_rems(13.0))
                    .text_color(theme.text_muted)
                    .child(SharedString::from("Loading usage limits…"))
                    .into_any_element(),
            ],
            Loadable::Error(message) => {
                let message = message.clone();
                vec![
                    widgets::error_strip(&theme, message)
                        .id("usage-limits-load-error")
                        .cursor_pointer()
                        .tab_index(0)
                        .role(gpui::Role::Button)
                        .focus_visible(|s| s.border_2().border_color(theme.accent).opacity(1.0))
                        .on_click(cx.listener(|this, _, _, cx| {
                            // Retry IS the visit's first successful list.
                            this.load(accounts::force_usage_for(LoadTrigger::Retry), cx)
                        }))
                        .child(
                            div()
                                .mt(px(4.0))
                                .text_size(crate::typography::ui_rems(11.5))
                                .text_color(theme.text_muted)
                                .child(SharedString::from("Click to retry")),
                        )
                        .into_any_element(),
                ]
            }
            Loadable::Ready(snapshot) => {
                let snapshot = snapshot.clone();
                let mut body: Vec<AnyElement> = snapshot
                    .warnings
                    .iter()
                    .map(|warning| {
                        widgets::warning_strip(&theme, warning.message.clone()).into_any_element()
                    })
                    .collect();
                if count == 0 {
                    body.push(
                        div()
                            .mt(px(24.0))
                            .text_size(crate::typography::ui_rems(13.0))
                            .text_color(theme.text_muted)
                            .child(SharedString::from(
                                "No subscriptions connected. Connect one in Settings → Providers.",
                            ))
                            .into_any_element(),
                    );
                } else {
                    for account in &main {
                        body.push(render_card(account, &theme, now, None));
                    }
                    if !no_quota.is_empty() {
                        body.push(group_header(
                            &theme,
                            "Connected, no usage view",
                            "The provider reports no plan windows for these logins.",
                        ));
                        for account in &no_quota {
                            body.push(render_card(account, &theme, now, None));
                        }
                    }
                    if !other.is_empty() {
                        body.push(group_header(
                            &theme,
                            "Other agent logins",
                            "Other agents on this device, shown read-only. Switch or forget them in Settings → Providers.",
                        ));
                        for account in &other {
                            let tag = accounts::provider_name(account.harness).to_string();
                            body.push(render_card(account, &theme, now, Some(&tag)));
                        }
                    }
                }
                body
            }
        };

        let scrollbar = popover::rail(self, "usage-limits-page-scrollbar", &theme, cx);
        div()
            .id("usage-limits-page-host")
            .relative()
            .size_full()
            .on_hover(cx.listener(Self::on_scroll_hovered))
            .child(
                crate::edge_fade::edge_faded(
                    16.0,
                    true,
                    true,
                    div()
                        .id("usage-limits-page")
                        .size_full()
                        .overflow_y_scroll()
                        .track_scroll(&self.scroll.scroll)
                        .child(
                            widgets::page_column()
                                .child(
                                    div()
                                        .flex()
                                        .flex_row()
                                        .flex_wrap()
                                        .items_center()
                                        .gap(px(10.0))
                                        .child(widgets::page_header(
                                            &theme,
                                            "Usage limits",
                                            (count > 0).then_some(count),
                                        ))
                                        .child(div().flex_1())
                                        .child(
                                            widgets::action_button(
                                                &theme,
                                                widgets::ActionTone::Quiet,
                                            )
                                            .id("usage-limits-refresh")
                                            .flex_none()
                                            .when(refreshing, |el| el.opacity(0.5))
                                            .tab_index(0)
                                            .role(gpui::Role::Button)
                                            .aria_label("Refresh usage limits")
                                            .tooltip(widgets::text_tooltip("Refresh usage limits"))
                                            .focus_visible(|s| {
                                                s.border_2().border_color(theme.accent)
                                            })
                                            .on_click(cx.listener(|this, _, _, cx| {
                                                this.load(
                                                    accounts::force_usage_for(LoadTrigger::Refresh),
                                                    cx,
                                                )
                                            }))
                                            .child(
                                                crate::icons::icon(crate::icons::REFRESH)
                                                    .size(px(16.0))
                                                    .text_color(theme.text_muted),
                                            )
                                            .child(SharedString::from("Refresh")),
                                        ),
                                )
                                .child(widgets::page_subtitle(
                                    &theme,
                                    "Plan rate limits for every connected subscription — the same numbers the composer footer and Providers show.",
                                ))
                                .children(summary_line.map(|line| {
                                    div()
                                        .px(px(8.0))
                                        .mt(px(6.0))
                                        .text_size(crate::typography::ui_rems(12.0))
                                        .text_color(theme.text_muted)
                                        .child(SharedString::from(line))
                                }))
                                .children(body)
                                .child(
                                    div()
                                        .mt(px(24.0))
                                        .flex()
                                        .flex_col()
                                        .gap(px(4.0))
                                        .child(footer_note(
                                            &theme,
                                            "Bars show the share of each window already used; alerts fire below 10 % remaining (ZERON_USAGE_ALERT_THRESHOLD on the engine).",
                                        ))
                                        .child(footer_note(
                                            &theme,
                                            "Token and spend accounting is in Settings → Usage.",
                                        )),
                                ),
                        ),
                )
                .fade_overflow_y(&self.scroll.scroll),
            )
            .children(scrollbar)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn account(id: &str, harness: HarnessId, provider: Option<&str>) -> AgentAccount {
        AgentAccount {
            id: id.into(),
            harness,
            email: None,
            plan_label: None,
            active: false,
            usage_windows: vec![],
            usage_fetched_at: None,
            usage_error: None,
            display_name: None,
            organization: None,
            auth_kind: None,
            switchable: true,
            saved_at: None,
            provider: provider.map(str::to_string),
        }
    }

    fn window(label: &str, used: f32, resets_in_hours: i64) -> AgentUsageWindow {
        AgentUsageWindow {
            label: label.into(),
            used_fraction: used,
            resets_at: Some(Utc::now() + chrono::Duration::hours(resets_in_hours)),
        }
    }

    #[test]
    fn subscription_labels_cover_every_pi_provider() {
        let label = |provider: &str| subscription_label(&account("a", HarnessId::Pi, Some(provider)));
        assert_eq!(label("anthropic"), "Claude");
        assert_eq!(label("openai-codex"), "ChatGPT (Codex)");
        assert_eq!(label("openai"), "OpenAI");
        assert_eq!(label("clinepass"), "Cline");
        assert_eq!(label("opencode-go"), "OpenCode Go");
        assert_eq!(label("github-copilot"), "GitHub Copilot");
        assert_eq!(label("nous"), "Nous Portal");
        assert_eq!(label("xai"), "Grok");
        assert_eq!(label("xai-something"), "Grok");
        // An unknown provider still names its harness, not a bare key.
        assert_eq!(label("mystery"), "Pi (mystery)");
        // A harness-only login falls back to the provider name.
        assert_eq!(
            subscription_label(&account("b", HarnessId::ClaudeCode, None)),
            "Claude Code"
        );
    }

    #[test]
    fn ordering_puts_the_worst_window_first_and_ignores_reset_windows() {
        let now = Utc::now();
        let mut low = account("low", HarnessId::Pi, Some("anthropic"));
        low.usage_windows = vec![window("5h", 0.1, 2)];
        let mut high = account("high", HarnessId::Pi, Some("opencode-go"));
        high.usage_windows = vec![window("5h", 0.9, 2)];
        // Its only window's reset has passed: the last period's figure is
        // stale, so it sorts as 0%.
        let mut reset = account("reset", HarnessId::Pi, Some("clinepass"));
        reset.usage_windows = vec![AgentUsageWindow {
            label: "5h".into(),
            used_fraction: 0.99,
            resets_at: Some(now - chrono::Duration::hours(1)),
        }];
        let mut none = account("none", HarnessId::Pi, Some("openai"));
        none.usage_windows = vec![];
        let snapshot = AgentAccountsSnapshot {
            accounts: vec![reset, low, none, high],
            warnings: vec![],
        };
        let ids: Vec<&str> = ordered(&snapshot, now)
            .iter()
            .map(|a| a.id.as_str())
            .collect();
        assert_eq!(ids, ["high", "low", "reset", "none"]);
        assert_eq!(worst_used(&snapshot.accounts[0], now), Some(0.0));
        assert_eq!(worst_used(&snapshot.accounts[2], now), None);
    }

    #[test]
    fn sections_split_subscriptions_no_quota_and_other_agent_logins() {
        let now = Utc::now();
        let mut pi = account("pi", HarnessId::Pi, Some("anthropic"));
        pi.usage_windows = vec![window("Session", 0.2, 3)];
        let no_quota = account("key", HarnessId::Pi, Some("openai"));
        let other = account("cli", HarnessId::ClaudeCode, None);
        let mut silent = account("anti", HarnessId::Antigravity, None);
        silent.usage_windows = vec![];
        let snapshot = AgentAccountsSnapshot {
            accounts: vec![other.clone(), pi.clone(), no_quota.clone(), silent],
            warnings: vec![],
        };
        let (main, no_quota_rows, other_rows) = sections(&snapshot, now);
        assert_eq!(main.iter().map(|a| a.id.as_str()).collect::<Vec<_>>(), ["pi"]);
        assert_eq!(
            no_quota_rows
                .iter()
                .map(|a| a.id.as_str())
                .collect::<Vec<_>>(),
            ["key"]
        );
        assert_eq!(
            other_rows.iter().map(|a| a.id.as_str()).collect::<Vec<_>>(),
            ["cli"]
        );
    }

    #[test]
    fn checked_label_buckets() {
        let now = Utc::now();
        assert_eq!(checked_label(None, now), "never checked");
        let ago = |secs: i64| Some((now - chrono::Duration::seconds(secs)).timestamp_millis());
        assert_eq!(checked_label(ago(30), now), "checked just now");
        assert_eq!(checked_label(ago(12 * 60), now), "checked 12m ago");
        assert_eq!(checked_label(ago(3 * 60 * 60), now), "checked 3h ago");
    }

    #[test]
    fn a_past_due_window_reads_zero_percent_and_reset() {
        let now = Utc::now();
        let past = AgentUsageWindow {
            label: "5h".into(),
            used_fraction: 0.98,
            resets_at: Some(now - chrono::Duration::minutes(1)),
        };
        let (fraction, reset) = window_state(&past, now);
        assert_eq!(fraction, 0.0);
        assert_eq!(reset.as_deref(), Some("reset, refreshing"));
        let current = window("Week", 0.42, 48);
        let (fraction, reset) = window_state(&current, now);
        assert_eq!(fraction, 0.42);
        assert!(reset.as_deref().is_some_and(|r| r.starts_with("resets ")));
    }

    #[test]
    fn summary_names_the_count_and_the_tightest_window() {
        let now = Utc::now();
        let mut claude = account("c", HarnessId::Pi, Some("anthropic"));
        claude.usage_windows = vec![window("5h", 0.98, 2)];
        let mut codex = account("x", HarnessId::Pi, Some("openai-codex"));
        codex.usage_windows = vec![window("Weekly", 0.3, 48)];
        let rows = [&claude, &codex];
        assert_eq!(summary(&rows, now), "2 subscriptions · Claude 5h 98% used");
        assert_eq!(summary(&[], now), "0 subscriptions");
    }

    #[test]
    fn an_empty_snapshot_renders_the_empty_state() {
        let now = Utc::now();
        let snapshot = AgentAccountsSnapshot::default();
        let (main, no_quota, other) = sections(&snapshot, now);
        assert!(main.is_empty() && no_quota.is_empty() && other.is_empty());
        assert_eq!(summary(&[], now), "0 subscriptions");
    }

    // ---- load behaviour (the FakeEngine pattern from settings::accounts) ----

    /// Answer every `ListAgentAccounts` in flight, returning the `forceUsage`
    /// flags asked for, in order. `accounts` is the reply's account array;
    /// `fail_forced` makes the forced reply an error.
    fn drain_lists_with(
        cx: &mut gpui::TestAppContext,
        runtime: &tokio::runtime::Runtime,
        engine: &mut FakeEngine,
        accounts: serde_json::Value,
        fail_forced: bool,
    ) -> Vec<bool> {
        let mut forced = Vec::new();
        let mut idle = 0;
        loop {
            cx.run_until_parked();
            runtime.block_on(tokio::time::sleep(std::time::Duration::from_millis(2)));
            let Ok(request) = engine.requests.try_recv() else {
                idle += 1;
                if idle >= 3 {
                    return forced;
                }
                continue;
            };
            idle = 0;
            let frame: zeron_rpc::ClientFrame = serde_json::from_str(&request).unwrap();
            let force = frame.params["forceUsage"].as_bool().unwrap_or(false);
            if frame.method.as_deref() == Some(zeron_rpc::methods::LIST_AGENT_ACCOUNTS) {
                forced.push(force);
            }
            let reply = if fail_forced && force {
                serde_json::json!({
                    "id": frame.id,
                    "err": "probe failed",
                })
            } else {
                serde_json::json!({
                    "id": frame.id,
                    "ok": { "accounts": accounts, "warnings": [] },
                })
            };
            runtime
                .block_on(engine.replies.send(reply.to_string()))
                .unwrap();
        }
    }

    fn drain_lists(
        cx: &mut gpui::TestAppContext,
        runtime: &tokio::runtime::Runtime,
        engine: &mut FakeEngine,
    ) -> Vec<bool> {
        drain_lists_with(cx, runtime, engine, serde_json::json!([]), false)
    }

    /// A fake engine behind a usage-limits page: the RPC frames it receives
    /// and the channel its replies are written to.
    struct FakeEngine {
        requests: tokio::sync::mpsc::Receiver<String>,
        replies: tokio::sync::mpsc::Sender<String>,
        inbound: Option<tokio::sync::mpsc::Receiver<String>>,
        out: tokio::sync::mpsc::Sender<String>,
    }

    impl FakeEngine {
        fn new() -> Self {
            let (out, requests) = tokio::sync::mpsc::channel(16);
            let (replies, inbound) = tokio::sync::mpsc::channel(16);
            Self {
                requests,
                replies,
                inbound: Some(inbound),
                out,
            }
        }

        fn handle(&mut self, device_id: &str) -> crate::state::EngineHandle {
            let client = zeron_rpc::RpcClient::new(
                self.out.clone(),
                self.inbound
                    .take()
                    .expect("one engine handle per fake engine"),
            );
            crate::state::EngineHandle::from_test_client_on(client, device_id)
        }
    }

    fn test_app(cx: &mut gpui::TestAppContext) -> (tempfile::TempDir, Entity<AppState>) {
        let dir = tempfile::tempdir().unwrap();
        cx.update(|cx| {
            crate::settings::init(Default::default(), dir.path(), cx);
            gpui_base::init(cx);
            cx.set_global(crate::theme::Theme::default());
        });
        let state = cx.new(|_| crate::state::AppState::new());
        (dir, state)
    }

    #[gpui::test]
    fn a_cold_mount_paints_the_plain_list_then_probes(cx: &mut gpui::TestAppContext) {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let _guard = runtime.enter();
        let mut engine = FakeEngine::new();
        let (_dir, state) = test_app(cx);
        state.update(cx, |state, _| {
            state.set_test_engine(engine.handle("local"));
        });
        let window = cx.add_window(|_, cx| UsageLimitsPage::new(state.clone(), cx));
        assert_eq!(drain_lists(cx, &runtime, &mut engine), [false, true]);
        window
            .update(cx, |page, _, _| assert!(matches!(page.snapshot, Loadable::Ready(_))))
            .unwrap();
    }

    #[gpui::test]
    fn a_warm_cache_paints_then_probes_once(cx: &mut gpui::TestAppContext) {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let _guard = runtime.enter();
        let mut engine = FakeEngine::new();
        let (_dir, state) = test_app(cx);
        state.update(cx, |state, _| {
            state.set_test_engine(engine.handle("local"));
        });
        let first = cx.add_window(|_, cx| UsageLimitsPage::new(state.clone(), cx));
        assert_eq!(drain_lists(cx, &runtime, &mut engine), [false, true]);
        drop(first);
        // A second visit paints from the shared cache, so it only forces the
        // one probe that keeps the numbers fresh.
        let second = cx.add_window(|_, cx| UsageLimitsPage::new(state.clone(), cx));
        second
            .update(cx, |page, _, _| assert!(matches!(page.snapshot, Loadable::Ready(_))))
            .unwrap();
        assert_eq!(drain_lists(cx, &runtime, &mut engine), [true]);
    }

    #[gpui::test]
    fn refresh_asks_for_a_forced_probe(cx: &mut gpui::TestAppContext) {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let _guard = runtime.enter();
        let mut engine = FakeEngine::new();
        let (_dir, state) = test_app(cx);
        state.update(cx, |state, _| {
            state.set_test_engine(engine.handle("local"));
        });
        let window = cx.add_window(|_, cx| UsageLimitsPage::new(state.clone(), cx));
        assert_eq!(drain_lists(cx, &runtime, &mut engine), [false, true]);
        window
            .update(cx, |page, _, cx| {
                page.load(accounts::force_usage_for(LoadTrigger::Refresh), cx)
            })
            .unwrap();
        assert_eq!(drain_lists(cx, &runtime, &mut engine), [true]);
    }

    #[gpui::test]
    fn a_failed_revalidation_keeps_the_painted_rows(cx: &mut gpui::TestAppContext) {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let _guard = runtime.enter();
        let mut engine = FakeEngine::new();
        let (_dir, state) = test_app(cx);
        state.update(cx, |state, _| {
            state.set_test_engine(engine.handle("local"));
        });
        let accounts = serde_json::json!([{
            "id": "a1",
            "harness": "pi",
            "provider": "anthropic",
            "planLabel": "Claude Pro",
            "active": true,
            "usageWindows": [
                { "label": "5h", "usedFraction": 0.98, "resetsAt": null }
            ],
        }]);
        let window = cx.add_window(|_, cx| UsageLimitsPage::new(state.clone(), cx));
        assert_eq!(
            drain_lists_with(cx, &runtime, &mut engine, accounts.clone(), false),
            [false, true]
        );
        window
            .update(cx, |page, _, _| {
                let snapshot = page.snapshot.ready().expect("painted");
                assert_eq!(snapshot.accounts.len(), 1);
            })
            .unwrap();
        // The next forced probe fails: the rows stay, the failure is a toast.
        window
            .update(cx, |page, _, cx| {
                page.load(accounts::force_usage_for(LoadTrigger::Refresh), cx)
            })
            .unwrap();
        assert_eq!(
            drain_lists_with(cx, &runtime, &mut engine, accounts, true),
            [true]
        );
        window
            .update(cx, |page, _, _| {
                let snapshot = page.snapshot.ready().expect("rows kept");
                assert_eq!(snapshot.accounts.len(), 1);
            })
            .unwrap();
        // The failure surfaces somewhere: inline until a render takes it, or
        // as the toast that render defers.
        let surfaced = window
            .update(cx, |page, _, _| page.error.is_some())
            .unwrap_or(false)
            || cx.update(|cx| !crate::toast::messages(cx).is_empty());
        assert!(surfaced, "a failed revalidation reports itself");
    }

    #[gpui::test]
    fn no_engine_shows_an_error_state(cx: &mut gpui::TestAppContext) {
        let (_dir, state) = test_app(cx);
        let window = cx.add_window(|_, cx| UsageLimitsPage::new(state.clone(), cx));
        window
            .update(cx, |page, _, _| {
                assert!(matches!(page.snapshot, Loadable::Error(_)));
            })
            .unwrap();
    }

    #[gpui::test]
    fn the_page_writes_its_snapshot_into_the_shared_cache(cx: &mut gpui::TestAppContext) {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let _guard = runtime.enter();
        let mut engine = FakeEngine::new();
        let (_dir, state) = test_app(cx);
        state.update(cx, |state, _| {
            state.set_test_engine(engine.handle("local"));
        });
        let accounts = serde_json::json!([{
            "id": "a1",
            "harness": "pi",
            "provider": "opencode-go",
            "planLabel": "OpenCode Go",
            "active": false,
            "usageWindows": [
                { "label": "5h", "usedFraction": 0.12, "resetsAt": null }
            ],
        }]);
        let window = cx.add_window(|_, cx| UsageLimitsPage::new(state.clone(), cx));
        assert_eq!(
            drain_lists_with(cx, &runtime, &mut engine, accounts, false),
            [false, true]
        );
        let cached = cx.update(|cx| {
            let key = AccountsCacheKey::new(state.read(cx), None);
            cx.try_global::<AccountsSnapshotCache>()
                .and_then(|cache| cache.0.get(&key).cloned())
        });
        let cached = cached.expect("the page shares its snapshot with Providers and the footer");
        assert_eq!(cached.accounts.len(), 1);
        assert_eq!(cached.accounts[0].provider.as_deref(), Some("opencode-go"));
        window
            .update(cx, |page, _, _| {
                let painted = page.snapshot.ready().expect("painted");
                assert_eq!(painted.accounts[0].id, "a1");
            })
            .unwrap();
    }
}
