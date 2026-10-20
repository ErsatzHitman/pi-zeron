//! The composer footer's plan-usage ring: how much of the active account's
//! rate limit the session's harness has used. Clicking it opens the harness's accounts — each with its
//! usage meters — and clicking one switches to it, the same `ActivateAgentAccount` Settings →
//! Accounts runs. Both views share [`AccountsSnapshotCache`], so a switch in
//! either shows up in the other.
use std::time::{Duration, Instant};

use gpui::{
    Context, Entity, IntoElement, Render, SharedString, Subscription, Task, Window, div,
    prelude::*, px,
};
use chrono::{DateTime, Utc};
use zeron_proto::{AgentAccount, AgentAccountsSnapshot, AgentUsageWindow, HarnessId};
use zeron_rpc::methods;

use crate::popover;
use crate::settings::accounts::{
    self, AccountsSnapshotCache, UsageLevel, render_usage_meter, reports_usage, signs_in,
    usage_color, usage_level,
};
use crate::settings::widgets;
use crate::state::AppState;
use crate::theme::Theme;

/// Forced probes hit the provider; the engine throttles them too, but the
/// ring re-probes on every hover, so don't even ask more often than this.
const FORCE_MIN_INTERVAL: Duration = Duration::from_secs(30);
/// Background re-probe while a composer is alive: usage moves as turns run.
const POLL_INTERVAL: Duration = Duration::from_secs(2 * 60);
/// Re-probe cadence while the accounts card is open.
const OPEN_POLL_INTERVAL: Duration = Duration::from_secs(60);
/// After the session's chat moves (a turn landed), wait this long — the
/// provider books usage a beat after the reply — then re-probe.
const AFTER_TURN_DELAY: Duration = Duration::from_secs(6);
/// How often the poll loop checks whether a re-probe is due (no network
/// unless one is).
const TICK: Duration = Duration::from_secs(5);

/// Whether a forced re-probe is due. `since_forced` is the time since the
/// last one (`None`: never); `turn_landed` the time since the session's chat
/// last moved without a re-probe after it; `reset_passed` whether a shown
/// window's reset time has come since that probe. Pure.
pub(crate) fn refresh_due(
    since_forced: Option<Duration>,
    open: bool,
    turn_landed: Option<Duration>,
    reset_passed: bool,
) -> bool {
    let Some(since) = since_forced else {
        return true;
    };
    if since < FORCE_MIN_INTERVAL {
        return false;
    }
    since >= POLL_INTERVAL
        || (open && since >= OPEN_POLL_INTERVAL)
        || turn_landed.is_some_and(|ago| ago >= AFTER_TURN_DELAY)
        || reset_passed
}

/// The binding limit: the most-used window of the account still current
/// at `now`. Pure.
pub fn used_fraction(account: &AgentAccount, now: DateTime<Utc>) -> Option<f32> {
    current_windows(account, now)
        .map(|window| window.used_fraction.clamp(0.0, 1.0))
        .reduce(f32::max)
}

/// The windows still current at `now`: one whose reset time has passed
/// holds the last period's figure until the next probe, not this one's.
fn current_windows(
    account: &AgentAccount,
    now: DateTime<Utc>,
) -> impl Iterator<Item = &AgentUsageWindow> {
    account
        .usage_windows
        .iter()
        .filter(move |window| window.resets_at.is_none_or(|at| at > now))
}

/// What the footer reads usage for: the session's harness and, for an
/// agent with a login per provider (Pi), the provider of the session's
/// model (`openai-codex` of `openai-codex/gpt-5.5`).
#[derive(Debug, Clone, Copy)]
pub struct UsageScope<'a> {
    pub harness: HarnessId,
    pub model: Option<&'a str>,
}

impl UsageScope<'_> {
    /// Whether `account` is the login the session's model runs on. A model
    /// whose provider has no login here (OpenRouter, ClinePass) shows none
    /// of the others' plans; an unknown model shows them all.
    fn covers(&self, account: &AgentAccount) -> bool {
        let provider = self
            .model
            .and_then(|model| model.split_once('/'))
            .map(|(provider, _)| provider);
        match (provider, account.provider.as_deref()) {
            (Some(model), Some(login)) => model == login,
            _ => true,
        }
    }
}

/// The harness's live account, when it reports usage. Pure.
pub fn active_account(
    snapshot: &AgentAccountsSnapshot,
    harness: HarnessId,
) -> Option<&AgentAccount> {
    snapshot
        .accounts
        .iter()
        .find(|account| account.harness == harness && account.active)
}

/// The accounts the session is using that report usage: for Pi, which
/// keeps a login per provider, the one its model runs on, an OpenCode Go
/// API key included (the key Pi uses now). An API key without a quota
/// view has no windows and so no row. Pure.
pub fn usage_accounts<'a>(
    snapshot: &'a AgentAccountsSnapshot,
    scope: UsageScope<'_>,
    now: DateTime<Utc>,
) -> Vec<&'a AgentAccount> {
    snapshot
        .accounts
        .iter()
        .filter(|account| {
            account.harness == scope.harness
                && account.active
                && current_windows(account, now).next().is_some()
                && scope.covers(account)
        })
        .collect()
}

/// A usage window's short name: the 5-hour session, the week, the month.
fn window_name(label: &str) -> String {
    match label.trim().to_ascii_lowercase().as_str() {
        "session" | "5h" | "5-hour" | "five hour" => "5h".into(),
        "week" | "weekly" | "7d" => "wk".into(),
        "month" | "monthly" | "30d" => "mo".into(),
        "day" | "daily" | "24h" => "day".into(),
        _ => label.trim().to_owned(),
    }
}

/// The footer's usage counters: what is left of each limit window of every
/// signed-in account in use ("5h 98% · wk 90% left"), each account named by
/// its provider when there are several. Pure.
pub fn usage_counters(
    snapshot: &AgentAccountsSnapshot,
    scope: UsageScope<'_>,
    now: DateTime<Utc>,
) -> Option<String> {
    let accounts = usage_accounts(snapshot, scope, now);
    if accounts.is_empty() {
        return None;
    }
    let named = accounts.len() > 1;
    let parts: Vec<String> = accounts
        .iter()
        .map(|account| {
            let windows = current_windows(account, now)
                .map(|window| {
                    let left = ((1.0 - window.used_fraction.clamp(0.0, 1.0)) * 100.0).round() as u32;
                    format!("{} {left}%", window_name(&window.label))
                })
                .collect::<Vec<_>>()
                .join(" · ");
            match account.provider.as_deref().filter(|_| named) {
                Some(provider) => format!(
                    "{} {windows}",
                    zeron_proto::model_provider::display_name(provider)
                ),
                None => windows,
            }
        })
        .collect();
    Some(format!("{} left", parts.join(" | ")))
}

/// Loads (and switches) the accounts of the composer's target device.
pub struct AccountUsage {
    state: Entity<AppState>,
    /// `None` = this device, as in [`AccountsSnapshotCache`].
    target: Option<String>,
    harness: Option<HarnessId>,
    /// The session's model (`provider/id`), which picks Pi's login.
    model: Option<String>,
    /// The session chat's last message time: a change means a turn landed.
    activity: Option<DateTime<Utc>>,
    /// When `activity` last moved, until the re-probe after it runs.
    turn_landed: Option<Instant>,
    /// Once any list has been asked for the current target.
    loaded: bool,
    last_forced: Option<Instant>,
    /// Wall time of the last forced probe, for reset times passing after it.
    last_forced_at: Option<DateTime<Utc>>,
    error: Option<SharedString>,
    load_task: Option<Task<()>>,
    action_task: Option<Task<()>>,
    popup: popover::Popup<FooterCard>,
    _poll: Task<()>,
    _cache: Subscription,
    _state: Subscription,
}

impl AccountUsage {
    pub fn new(state: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        // Live, not periodic: re-probe after each turn of the session, when a
        // shown window resets, every minute while the card is open, and
        // every two minutes otherwise.
        let poll = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(TICK).await;
                if this
                    .update(cx, |usage, cx| {
                        if usage.harness.is_some() && usage.refresh_is_due(cx) {
                            usage.load(true, cx);
                        }
                    })
                    .is_err()
                {
                    break;
                }
            }
        });
        Self {
            _state: cx.observe(&state, |_, _, cx| cx.notify()),
            state,
            target: None,
            harness: None,
            model: None,
            activity: None,
            turn_landed: None,
            loaded: false,
            last_forced: None,
            last_forced_at: None,
            error: None,
            load_task: None,
            action_task: None,
            popup: popover::Popup::default(),
            _poll: poll,
            // Settings → Accounts writes the same cache.
            _cache: cx.observe_global::<AccountsSnapshotCache>(|_, cx| cx.notify()),
        }
    }

    /// Point at the session's harness and device; loads on first sight of a
    /// device. Cheap enough to call every render.
    pub fn track(
        &mut self,
        harness: Option<HarnessId>,
        model: Option<String>,
        target: Option<String>,
        activity: Option<DateTime<Utc>>,
        cx: &mut Context<Self>,
    ) {
        let harness = harness.filter(|h| signs_in(*h) && reports_usage(*h));
        self.harness = harness;
        self.model = model;
        if self.target != target {
            self.target = target;
            self.loaded = false;
            self.last_forced = None;
            self.last_forced_at = None;
            self.error = None;
            self.activity = activity;
        }
        if self.activity != activity {
            // First sight of a chat is not a turn: only a later move is.
            if self.activity.is_some() && activity.is_some() {
                self.turn_landed = Some(Instant::now());
            }
            self.activity = activity;
        }
        if harness.is_some() && !self.loaded {
            self.loaded = true;
            self.load(true, cx);
        }
    }

    fn refresh_is_due(&self, cx: &gpui::App) -> bool {
        let reset_passed = self.last_forced_at.is_some_and(|probed| {
            let now = Utc::now();
            self.snapshot(cx).is_some_and(|snapshot| {
                snapshot
                    .accounts
                    .iter()
                    .filter(|account| Some(account.harness) == self.harness)
                    .flat_map(|account| &account.usage_windows)
                    .filter_map(|window| window.resets_at)
                    .any(|at| at > probed && at <= now)
            })
        });
        refresh_due(
            self.last_forced.map(|at| at.elapsed()),
            self.popup.get() == Some(&FooterCard::Accounts),
            self.turn_landed.map(|at| at.elapsed()),
            reset_passed,
        )
    }

    fn snapshot<'a>(&self, cx: &'a gpui::App) -> Option<&'a AgentAccountsSnapshot> {
        cx.try_global::<AccountsSnapshotCache>()?
            .0
            .get(&self.target)
    }

    fn params(&self, mut value: serde_json::Value) -> serde_json::Value {
        if let (Some(target), Some(object)) = (&self.target, value.as_object_mut()) {
            object.insert("targetDeviceId".into(), serde_json::json!(target));
        }
        value
    }

    /// Plain list first when nothing is cached (the engine's persisted usage
    /// paints at once), then the forced probe replaces it.
    fn load(&mut self, force_usage: bool, cx: &mut Context<Self>) {
        if force_usage {
            if self
                .last_forced
                .is_some_and(|at| at.elapsed() < FORCE_MIN_INTERVAL)
            {
                return;
            }
            self.last_forced = Some(Instant::now());
            self.last_forced_at = Some(Utc::now());
            self.turn_landed = None;
        }
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let key = self.target.clone();
        let paint_first = force_usage && self.snapshot(cx).is_none();
        let plain = self.params(serde_json::json!({ "forceUsage": false }));
        let params = self.params(serde_json::json!({ "forceUsage": force_usage }));
        self.load_task = Some(cx.spawn(async move |this, cx| {
            let fetch = |params: serde_json::Value| {
                let engine = engine.clone();
                async move {
                    engine
                        .client()
                        .call(methods::LIST_AGENT_ACCOUNTS, params)
                        .await
                        .ok()
                        .and_then(|value| {
                            serde_json::from_value::<AgentAccountsSnapshot>(value).ok()
                        })
                }
            };
            let store = |snapshot: AgentAccountsSnapshot, cx: &mut gpui::AsyncApp| {
                let key = key.clone();
                this.update(cx, |_, cx| {
                    cx.default_global::<AccountsSnapshotCache>()
                        .0
                        .insert(key, snapshot);
                })
                .ok();
            };
            if paint_first && let Some(snapshot) = fetch(plain).await {
                store(snapshot, cx);
            }
            if let Some(snapshot) = fetch(params).await {
                store(snapshot, cx);
            }
        }));
    }

    /// Switch optimistically, like Settings → Accounts: the rows flip at once
    /// and the engine's reply replaces them; a refusal restores the list.
    fn switch(&mut self, account: &AgentAccount, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let key = self.target.clone();
        let cache = &mut cx.default_global::<AccountsSnapshotCache>().0;
        let previous = cache.get(&key).cloned();
        if let Some(snapshot) = cache.get_mut(&key) {
            // Per-provider agents keep one live login per provider: only the
            // switched-to row's group changes.
            accounts::mark_switched(snapshot, account);
        }
        self.error = None;
        let params = self.params(serde_json::json!({
            "id": account.id,
            "accountId": account.id,
            "harness": account.harness,
        }));
        self.action_task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(methods::ACTIVATE_AGENT_ACCOUNT, params)
                .await;
            this.update(cx, |usage, cx| {
                match result.map(serde_json::from_value::<AgentAccountsSnapshot>) {
                    Ok(Ok(snapshot)) => {
                        cx.default_global::<AccountsSnapshotCache>()
                            .0
                            .insert(key, snapshot);
                    }
                    Ok(Err(_)) => usage.load(false, cx),
                    Err(err) => {
                        if let Some(previous) = previous {
                            cx.default_global::<AccountsSnapshotCache>()
                                .0
                                .insert(key, previous);
                        }
                        usage.error = Some(err.to_string().into());
                    }
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    /// The ring's reading: the live account's most-used window, when the
    /// harness has a live account with usage to show.
    fn fraction(&self, cx: &gpui::App) -> Option<f32> {
        let now = Utc::now();
        usage_accounts(self.snapshot(cx)?, self.scope()?, now)
            .into_iter()
            .filter_map(|account| used_fraction(account, now))
            .reduce(f32::max)
    }

    fn counters(&self, cx: &gpui::App) -> Option<String> {
        usage_counters(self.snapshot(cx)?, self.scope()?, Utc::now())
    }

    fn scope(&self) -> Option<UsageScope<'_>> {
        Some(UsageScope {
            harness: self.harness?,
            model: self.model.as_deref(),
        })
    }

    /// A trigger click: open this ring's popover, or close it when the
    /// press found it open (the card's mouse-down-out already began that
    /// close — see `Popup::note_trigger_press`).
    fn toggle(&mut self, card: FooterCard, cx: &mut Context<Self>) {
        if self.popup.take_press_was_open() || self.popup.as_open() == Some(&card) {
            self.dismiss(cx);
            return;
        }
        if card == FooterCard::Accounts {
            // Opening the card is the moment someone cares: re-probe.
            self.load(true, cx);
        }
        self.popup.open(card);
        cx.notify();
    }

    fn dismiss(&mut self, cx: &mut Context<Self>) {
        if self.popup.begin_close() {
            popover::reap_popup(cx, |usage: &mut Self| &mut usage.popup);
        }
        cx.notify();
    }

    /// A footer ring that opens `card` above itself, right-aligned like the
    /// model picker.
    fn trigger(
        &self,
        chip: gpui::Stateful<gpui::Div>,
        card: FooterCard,
        content: impl FnOnce(&Self, &mut Context<Self>) -> gpui::Div,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let chip = chip
            .relative()
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(move |usage, _, _, _| {
                    usage
                        .popup
                        .note_trigger_press_matching(|open| *open == card);
                }),
            )
            .on_click(cx.listener(move |usage, _, _, cx| usage.toggle(card, cx)));
        if self.popup.get() != Some(&card) {
            return chip.into_any_element();
        }
        let content = content(self, cx)
            .on_mouse_down_out(cx.listener(|usage, _, _, cx| usage.dismiss(cx)))
            .into_any_element();
        chip.child(popover::anchored_menu_above_end(
            card.id(),
            content,
            self.popup.closing_since(),
        ))
        .into_any_element()
    }

    fn accounts_card(&self, cx: &mut Context<Self>) -> gpui::Div {
        let theme = &Theme::of(cx).for_popup();
        let harness = self.harness;
        let now = chrono::Utc::now();
        let rows: Vec<AgentAccount> = match (harness, self.snapshot(cx)) {
            (Some(harness), Some(snapshot)) => accounts::provider_accounts(snapshot, harness)
                .into_iter()
                .cloned()
                .collect(),
            _ => Vec::new(),
        };
        let title = format!(
            "{} accounts",
            harness.map_or("Agent", accounts::provider_name)
        );
        popover::popover_card(theme)
            .w(px(400.0))
            .flex()
            .flex_col()
            .child(popover::menu_heading(theme, &title))
            .children(rows.iter().enumerate().map(|(ix, account)| {
                let email: SharedString = account
                    .email
                    .clone()
                    .or_else(|| account.display_name.clone())
                    .unwrap_or_else(|| "Unknown account".into())
                    .into();
                let can_switch = !account.active && account.switchable;
                let mut meta: Vec<gpui::AnyElement> = account
                    .plan_label
                    .iter()
                    .map(|plan| {
                        div()
                            .child(SharedString::from(plan.clone()))
                            .into_any_element()
                    })
                    .collect();
                if account.active {
                    meta.push(
                        div()
                            .text_color(theme.accent)
                            .child(SharedString::from("In use"))
                            .into_any_element(),
                    );
                } else if let Some(reason) = account
                    .usage_error
                    .clone()
                    .filter(|_| account.usage_windows.is_empty())
                {
                    meta.push(div().child(SharedString::from(reason)).into_any_element());
                }
                let switch_to = account.clone();
                popover::menu_row(theme, account.active, format!("account-usage-row-{ix}"))
                    .id(("account-usage-row", ix))
                    .when(!can_switch, |row| row.cursor_default())
                    .when(can_switch, |row| {
                        row.on_click(cx.listener(move |usage, _, _, cx| {
                            usage.switch(&switch_to, cx);
                        }))
                    })
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .child(
                                div()
                                    .truncate()
                                    .text_size(px(12.5))
                                    .font_weight(gpui::FontWeight::MEDIUM)
                                    .child(email),
                            )
                            .when(!meta.is_empty(), |el| {
                                el.child(crate::settings::widgets::meta_line(theme, meta))
                            }),
                    )
                    .child(
                        div()
                            .id(("account-usage-meters", ix))
                            .flex_none()
                            .flex()
                            .flex_col()
                            .gap(px(2.0))
                            .when(!account.usage_windows.is_empty(), |meters| {
                                meters.tooltip(widgets::text_tooltip(accounts::usage_resets(
                                    &account.usage_windows,
                                    now,
                                )))
                            })
                            .children(account.usage_windows.iter().map(|window| {
                                render_usage_meter(&accounts::current_window(window, now), theme)
                            })),
                    )
            }))
            .children(self.error.clone().map(|error| {
                div()
                    .px(px(8.0))
                    .py(px(4.0))
                    .text_size(px(12.0))
                    .text_color(theme.danger)
                    .child(error)
            }))
    }
}

/// Which footer ring's popover is up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FooterCard {
    Accounts,
}

impl FooterCard {
    fn id(self) -> &'static str {
        match self {
            FooterCard::Accounts => "account-usage-menu",
        }
    }
}

/// The footer's plan-usage ring; it opens the accounts popover on click.
/// Context occupancy lives in Session Details, not here.
impl Render for AccountUsage {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx).clone();
        let counters = self.counters(cx);
        let account = self.fraction(cx).zip(counters).map(|(fraction, counters)| {
            let level = usage_level(fraction);
            let chip = crate::context_usage::ring_chip(
                "account-usage",
                fraction,
                usage_color(level, &theme),
                match level {
                    UsageLevel::Normal => theme.text_muted,
                    _ => usage_color(level, &theme),
                },
                counters,
                self.popup.get() == Some(&FooterCard::Accounts),
                &theme,
            );
            self.trigger(
                chip,
                FooterCard::Accounts,
                |usage, cx| usage.accounts_card(cx),
                cx,
            )
        });
        div()
            .flex()
            .items_center()
            .gap(px(4.0))
            .children(account)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zeron_proto::AgentUsageWindow;

    #[test]
    fn the_counters_say_what_is_left_of_each_window() {
        let mut claude = account(HarnessId::Pi, true, &[0.02, 0.1]);
        claude.usage_windows[0].label = "Session".into();
        claude.usage_windows[1].label = "Week".into();
        claude.provider = Some("anthropic".into());
        let mut snapshot: AgentAccountsSnapshot =
            serde_json::from_value(serde_json::json!({ "accounts": [], "warnings": [] })).unwrap();
        snapshot.accounts = vec![claude.clone(), account(HarnessId::Pi, false, &[0.9])];
        let now = Utc::now();
        let pi = UsageScope {
            harness: HarnessId::Pi,
            model: None,
        };
        assert_eq!(
            usage_counters(&snapshot, pi, now).as_deref(),
            Some("5h 98% · wk 90% left")
        );
        let mut codex = account(HarnessId::Pi, true, &[0.5]);
        codex.usage_windows[0].label = "Month".into();
        codex.provider = Some("openai-codex".into());
        codex.id = "codex".into();
        snapshot.accounts.push(codex);
        let both = usage_counters(&snapshot, pi, now).unwrap();
        assert!(both.starts_with("Anthropic 5h 98% · wk 90% | "), "{both}");
        assert!(both.ends_with("mo 50% left"), "{both}");
        // A session names its model: only that provider's plan is shown,
        // and none for a provider without a login here.
        let on = |model| UsageScope {
            harness: HarnessId::Pi,
            model: Some(model),
        };
        assert_eq!(
            usage_counters(&snapshot, on("openai-codex/gpt-5.5"), now).as_deref(),
            Some("mo 50% left")
        );
        assert_eq!(usage_counters(&snapshot, on("cline/claude-sonnet"), now), None);
        // A window past its reset holds the last period's figure: dropped.
        let mut expired = snapshot.clone();
        expired.accounts[2].usage_windows[0].resets_at =
            Some(now - chrono::TimeDelta::minutes(1));
        assert_eq!(
            usage_counters(&expired, on("openai-codex/gpt-5.5"), now),
            None
        );
        // An OpenCode Go key in use reports its quota like a login; one
        // without windows (an API key with no quota view) shows nothing.
        let mut go = account(HarnessId::Pi, true, &[0.3, 0.6]);
        go.usage_windows[1].label = "Week".into();
        go.provider = Some("opencode-go".into());
        go.auth_kind = Some(zeron_proto::AgentAuthKind::ApiKey);
        snapshot.accounts = vec![go.clone()];
        assert_eq!(
            usage_counters(&snapshot, on("opencode-go/kimi-k2"), now).as_deref(),
            Some("5h 70% · wk 40% left")
        );
        go.usage_windows.clear();
        snapshot.accounts = vec![go];
        assert_eq!(usage_counters(&snapshot, pi, now), None);
    }

    #[test]
    fn usage_refreshes_live_not_only_on_a_timer() {
        let s = Duration::from_secs;
        // Never probed: now.
        assert!(refresh_due(None, false, None, false));
        // Never sooner than the throttle, whatever happened.
        assert!(!refresh_due(Some(s(10)), true, Some(s(60)), true));
        // A quiet composer: every two minutes.
        assert!(!refresh_due(Some(s(90)), false, None, false));
        assert!(refresh_due(Some(s(120)), false, None, false));
        // The card open: every minute.
        assert!(refresh_due(Some(s(60)), true, None, false));
        // A turn landed: a beat after it.
        assert!(!refresh_due(Some(s(40)), false, Some(s(2)), false));
        assert!(refresh_due(Some(s(40)), false, Some(s(6)), false));
        // A shown window reset since the last probe.
        assert!(refresh_due(Some(s(40)), false, None, true));
    }

    fn account(harness: HarnessId, active: bool, used: &[f32]) -> AgentAccount {
        serde_json::from_value(serde_json::json!({
            "id": format!("{harness:?}-{active}"),
            "harness": harness,
            "email": null,
            "planLabel": null,
            "active": active,
            "switchable": true,
            "usageWindows": used
                .iter()
                .map(|fraction| AgentUsageWindow {
                    label: "5h".into(),
                    used_fraction: *fraction,
                    resets_at: None,
                })
                .collect::<Vec<_>>(),
        }))
        .unwrap()
    }

    #[test]
    fn ring_shows_the_most_used_window() {
        assert_eq!(used_fraction(&account(HarnessId::Codex, true, &[]), Utc::now()), None);
        assert_eq!(
            used_fraction(&account(HarnessId::Codex, true, &[0.12, 0.64]), Utc::now()),
            Some(0.64)
        );
        assert_eq!(
            used_fraction(&account(HarnessId::Codex, true, &[1.4]), Utc::now()),
            Some(1.0)
        );
    }

    #[test]
    fn active_account_is_scoped_to_the_harness() {
        let snapshot = AgentAccountsSnapshot {
            accounts: vec![
                account(HarnessId::ClaudeCode, true, &[0.3]),
                account(HarnessId::Codex, false, &[0.1]),
                account(HarnessId::Codex, true, &[0.2]),
            ],
            warnings: Vec::new(),
        };
        assert_eq!(
            active_account(&snapshot, HarnessId::Codex).map(|a| a.id.as_str()),
            Some("Codex-true")
        );
        assert!(active_account(&snapshot, HarnessId::Cursor).is_none());
    }
}
