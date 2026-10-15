//! Settings → Usage: tokens and spend for every Pi provider and model on the
//! machine this window is attached to, from its engine (`GetUsageReport`).
//! Providers come from the records, so one added through `/login` shows up
//! the first time it is used.

use std::{collections::BTreeSet, rc::Rc};

use chrono::{Local, TimeZone};
use gpui::{
    AnyElement, Context, Entity, SharedString, Subscription, Task, Window, div, prelude::*, px,
};
use zeron_proto::usage::{UsageBucket, UsageModelRow, UsageQuery, UsageReport};
use zeron_rpc::{RpcError, methods};

use crate::markdown::chart;
use crate::popover;
use crate::settings::widgets;
use crate::state::AppState;
use crate::theme::Theme;

/// While the engine is still reading session files, ask again this often.
const SCANNING_POLL: std::time::Duration = std::time::Duration::from_millis(1500);
/// Providers drawn as their own line; the rest share one.
const CHART_PROVIDERS: usize = chart::MAX_SERIES - 1;
/// Points per line at most; longer ranges merge days.
const CHART_POINTS: i64 = 120;
const HOUR_MS: i64 = 3_600_000;
const DAY_MS: i64 = 24 * HOUR_MS;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Period {
    Day,
    Week,
    Month,
    Quarter,
    All,
}

impl Period {
    pub const ALL: [Period; 5] = [
        Period::Day,
        Period::Week,
        Period::Month,
        Period::Quarter,
        Period::All,
    ];

    fn label(self) -> &'static str {
        match self {
            Period::Day => "24h",
            Period::Week => "7d",
            Period::Month => "30d",
            Period::Quarter => "90d",
            Period::All => "All",
        }
    }

    fn bucket(self) -> UsageBucket {
        if self == Period::Day {
            UsageBucket::Hour
        } else {
            UsageBucket::Day
        }
    }

    /// The window's start: the last 24 whole hours, or whole local days
    /// ending today.
    fn since_ms(self, now_ms: i64, offset_ms: i64) -> Option<i64> {
        let days = match self {
            Period::Day => {
                return Some((now_ms.div_euclid(HOUR_MS) - 23) * HOUR_MS);
            }
            Period::Week => 7,
            Period::Month => 30,
            Period::Quarter => 90,
            Period::All => return None,
        };
        let today = (now_ms + offset_ms).div_euclid(DAY_MS) * DAY_MS - offset_ms;
        Some(today - (days - 1) * DAY_MS)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Metric {
    Cost,
    Tokens,
}

/// "19.9B", "76.7M", "804K", "950".
pub fn format_tokens(tokens: u64) -> String {
    let n = tokens as f64;
    let (value, suffix) = if n >= 1e9 {
        (n / 1e9, "B")
    } else if n >= 1e6 {
        (n / 1e6, "M")
    } else if n >= 1e3 {
        (n / 1e3, "K")
    } else {
        return tokens.to_string();
    };
    let text = if value >= 100.0 {
        format!("{value:.0}")
    } else {
        format!("{value:.1}")
    };
    format!("{}{suffix}", text.trim_end_matches(".0"))
}

/// "$0", "<$0.01", "$12.34", "$5,078".
pub fn format_usd(usd: f64) -> String {
    if usd <= 0.0 {
        return "$0".into();
    }
    if usd < 0.01 {
        return "<$0.01".into();
    }
    if usd < 1000.0 {
        return format!("${usd:.2}");
    }
    let digits = format!("{:.0}", usd);
    let mut grouped = String::new();
    for (ix, digit) in digits.chars().enumerate() {
        if ix > 0 && (digits.len() - ix) % 3 == 0 {
            grouped.push(',');
        }
        grouped.push(digit);
    }
    format!("${grouped}")
}

/// "42%", "3.5%", "<0.1%".
pub fn format_share(share: f64) -> String {
    let percent = share * 100.0;
    if percent <= 0.0 {
        "0%".into()
    } else if percent < 0.1 {
        "<0.1%".into()
    } else if percent < 10.0 {
        format!("{percent:.1}%")
    } else {
        format!("{percent:.0}%")
    }
}

/// How a provider is named on the page.
pub fn provider_label(provider: &str) -> String {
    if provider.is_empty() {
        "Tools & summaries".into()
    } else {
        provider.into()
    }
}

/// One provider's totals over the window.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ProviderTotal {
    pub provider: String,
    pub tokens: u64,
    pub recorded_usd: f64,
    pub estimated_usd: f64,
    pub requests: u64,
}

impl ProviderTotal {
    pub fn value(&self, metric: Metric) -> f64 {
        match metric {
            Metric::Cost => self.recorded_usd + self.estimated_usd,
            Metric::Tokens => self.tokens as f64,
        }
    }
}

/// Providers, largest first by `metric`.
pub fn providers(report: &UsageReport, metric: Metric) -> Vec<ProviderTotal> {
    let mut totals: Vec<ProviderTotal> = vec![];
    for row in &report.models {
        let ix = match totals.iter().position(|t| t.provider == row.provider) {
            Some(ix) => ix,
            None => {
                totals.push(ProviderTotal {
                    provider: row.provider.clone(),
                    ..Default::default()
                });
                totals.len() - 1
            }
        };
        let total = &mut totals[ix];
        total.tokens += row.tokens.total();
        total.recorded_usd += row.recorded_usd;
        total.estimated_usd += row.estimated_usd;
        total.requests += row.requests;
    }
    totals.sort_by(|a, b| {
        b.value(metric)
            .total_cmp(&a.value(metric))
            .then(b.tokens.cmp(&a.tokens))
            .then_with(|| a.provider.cmp(&b.provider))
    });
    totals
}

/// A model's spend as a cell: recorded, `~` estimated, Free, or Unpriced.
pub fn model_cost_label(row: &UsageModelRow) -> String {
    let cost = row.cost_usd();
    if cost > 0.0 {
        let text = format_usd(cost);
        if row.recorded_usd > 0.0 {
            text
        } else {
            format!("~{text}")
        }
    } else if row.unpriced_requests > 0 {
        "Unpriced".into()
    } else if row.free_requests > 0 {
        "Free".into()
    } else {
        "$0".into()
    }
}

/// The provider lines over the window: one per top provider, the rest as
/// "Other", every bucket present even when nothing was spent.
pub fn chart_for(
    report: &UsageReport,
    metric: Metric,
    period: Period,
    now_ms: i64,
    offset_ms: i64,
) -> chart::Chart {
    let ranked = providers(report, metric);
    let width = match report.bucket {
        UsageBucket::Hour => HOUR_MS,
        UsageBucket::Day => DAY_MS,
    };
    let align = |ms: i64| (ms + offset_ms).div_euclid(width) * width - offset_ms;
    let end = align(now_ms);
    let start = period
        .since_ms(now_ms, offset_ms)
        .or(report.first_ms)
        .map_or(end, align)
        .min(end);
    let buckets = (end - start) / width + 1;
    let merge = ((buckets + CHART_POINTS - 1) / CHART_POINTS).max(1);
    let step = width * merge;
    let slots = ((end - start) / step + 1) as usize;
    let mut names: Vec<String> = ranked
        .iter()
        .take(CHART_PROVIDERS)
        .map(|p| provider_label(&p.provider))
        .collect();
    let other = ranked.len() > CHART_PROVIDERS;
    if other {
        names.push("Other".into());
    }
    let mut values = vec![vec![0.0; slots]; names.len()];
    for point in &report.series {
        let slot = ((point.start_ms - start).max(0) / step) as usize;
        if slot >= slots {
            continue;
        }
        let line = ranked
            .iter()
            .position(|p| p.provider == point.provider)
            .map_or(names.len() - 1, |ix| ix.min(names.len() - 1));
        values[line][slot] += match metric {
            Metric::Cost => point.cost_usd,
            Metric::Tokens => point.tokens as f64,
        };
    }
    let labels: Vec<String> = (0..slots)
        .map(|slot| {
            let ms = start + slot as i64 * step;
            let time = Local.timestamp_millis_opt(ms).single();
            match (report.bucket, time) {
                (UsageBucket::Hour, Some(time)) => time.format("%H:00").to_string(),
                (UsageBucket::Day, Some(time)) => time.format("%b %-d").to_string(),
                (_, None) => String::new(),
            }
        })
        .collect();
    chart::Chart {
        kind: if slots > 1 {
            chart::Kind::Line
        } else {
            chart::Kind::Bar
        },
        title: None,
        description: None,
        series: names
            .into_iter()
            .zip(values)
            .map(|(name, values)| chart::Series {
                name,
                points: labels
                    .iter()
                    .zip(values)
                    .map(|(label, y)| chart::Point {
                        label: Some(label.clone()),
                        x: 0.0,
                        y,
                    })
                    .collect(),
            })
            .collect(),
        labels,
        x_label: None,
        y_label: None,
        unit: (metric == Metric::Cost).then(|| "$".into()),
        source: None,
    }
}

/// The page's data: the report, or why there is none.
#[derive(Debug, Clone, PartialEq)]
pub enum Status {
    Loading,
    Ready(UsageReport),
    /// The machine's engine predates usage reports.
    Unsupported,
    Error(String),
}

pub struct UsagePage {
    state: Entity<AppState>,
    scroll: widgets::PageScroll,
    period: Period,
    metric: Metric,
    status: Status,
    /// A request is in flight; the painted report stays meanwhile.
    loading: bool,
    hidden: BTreeSet<usize>,
    task: Option<Task<()>>,
    _observe: Subscription,
}

impl UsagePage {
    pub fn new(state: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        let observe = cx.observe(&state, |_, _, cx| cx.notify());
        let mut page = Self {
            state,
            scroll: widgets::PageScroll::default(),
            period: Period::Month,
            metric: Metric::Cost,
            status: Status::Loading,
            loading: false,
            hidden: BTreeSet::new(),
            task: None,
            _observe: observe,
        };
        page.load(false, cx);
        page
    }

    pub fn status(&self) -> &Status {
        &self.status
    }

    fn offset_ms() -> i64 {
        i64::from(Local::now().offset().local_minus_utc()) * 1000
    }

    fn load(&mut self, refresh: bool, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            self.status = Status::Error("Engine not connected".into());
            return;
        };
        let offset_ms = Self::offset_ms();
        let query = UsageQuery {
            since_ms: self
                .period
                .since_ms(chrono::Utc::now().timestamp_millis(), offset_ms),
            bucket: self.period.bucket(),
            utc_offset_minutes: (offset_ms / 60_000) as i32,
            refresh,
        };
        let params = serde_json::to_value(&query).unwrap_or_default();
        self.loading = true;
        self.task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call_as::<UsageReport>(methods::GET_USAGE_REPORT, params)
                .await;
            let scanning = result.as_ref().is_ok_and(|report| report.scanning);
            this.update(cx, |page, cx| {
                page.loading = false;
                page.status = match result {
                    Ok(report) => Status::Ready(report),
                    Err(RpcError::UnknownMethod(_)) => Status::Unsupported,
                    // A failed refresh keeps the numbers on screen.
                    Err(_) if matches!(page.status, Status::Ready(_)) => page.status.clone(),
                    Err(error) => Status::Error(error.to_string()),
                };
                cx.notify();
            })
            .ok();
            if scanning {
                cx.background_executor().timer(SCANNING_POLL).await;
                this.update(cx, |page, cx| page.load(false, cx)).ok();
            }
        }));
        cx.notify();
    }

    fn set_period(&mut self, period: Period, cx: &mut Context<Self>) {
        if self.period != period {
            self.period = period;
            self.hidden.clear();
            self.load(false, cx);
        }
    }

    fn set_metric(&mut self, metric: Metric, cx: &mut Context<Self>) {
        if self.metric != metric {
            self.metric = metric;
            self.hidden.clear();
            cx.notify();
        }
    }

    fn on_scroll_hovered(&mut self, hovered: &bool, _: &mut Window, cx: &mut Context<Self>) {
        if self.scroll.set_list_hovered(*hovered) {
            cx.notify();
        }
    }

    fn render_tabs(
        &self,
        theme: &Theme,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let reduced_motion = crate::motion::reduced_motion(cx);
        let mut periods = div().flex().flex_row().gap(px(2.0));
        for period in Period::ALL {
            let selected = period == self.period;
            let t = widgets::tab_selection_t(
                window,
                format!("usage-period-{}-selection", period.label()),
                selected,
                reduced_motion,
            );
            periods = periods.child(
                widgets::section_tab(
                    theme,
                    selected,
                    t,
                    format!("usage-period-{}", period.label()),
                    format!("usage-period-{}-hover", period.label()),
                )
                .tab_index(0)
                .role(gpui::Role::Tab)
                .focus_visible(|s| s.border_2().border_color(theme.accent))
                .on_click(cx.listener(move |page, _, _, cx| page.set_period(period, cx)))
                .child(period.label()),
            );
        }
        let mut metrics = div().flex().flex_row().gap(px(2.0));
        for (metric, label) in [(Metric::Cost, "Cost"), (Metric::Tokens, "Tokens")] {
            let selected = metric == self.metric;
            let t = widgets::tab_selection_t(
                window,
                format!("usage-metric-{label}-selection"),
                selected,
                reduced_motion,
            );
            metrics = metrics.child(
                widgets::section_tab(
                    theme,
                    selected,
                    t,
                    format!("usage-metric-{label}"),
                    format!("usage-metric-{label}-hover"),
                )
                .tab_index(0)
                .role(gpui::Role::Tab)
                .focus_visible(|s| s.border_2().border_color(theme.accent))
                .on_click(cx.listener(move |page, _, _, cx| page.set_metric(metric, cx)))
                .child(label),
            );
        }
        let refresh = widgets::ghost_action(theme)
            .id("usage-refresh")
            .role(gpui::Role::Button)
            .aria_label("Look for new usage")
            .tab_index(0)
            .focus_visible(|s| s.border_2().border_color(theme.accent))
            .when(self.loading, |el| el.opacity(0.5))
            .on_click(cx.listener(|page, _, _, cx| page.load(true, cx)))
            .child(
                crate::icons::icon(crate::icons::REFRESH)
                    .size(px(14.0))
                    .text_color(theme.text_muted),
            );
        div()
            .mt(px(20.0))
            .flex()
            .flex_row()
            .flex_wrap()
            .items_center()
            .justify_between()
            .gap(px(12.0))
            .child(periods)
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(8.0))
                    .child(metrics)
                    .child(refresh),
            )
            .into_any_element()
    }

    fn render_report(
        &self,
        report: &UsageReport,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let metric = self.metric;
        let ranked = providers(report, metric);
        let total: f64 = ranked.iter().map(|p| p.value(metric)).sum();
        let recorded: f64 = ranked.iter().map(|p| p.recorded_usd).sum();
        let estimated: f64 = ranked.iter().map(|p| p.estimated_usd).sum();
        let tokens: u64 = ranked.iter().map(|p| p.tokens).sum();
        let requests: u64 = ranked.iter().map(|p| p.requests).sum();
        let value_text = |value: f64| match metric {
            Metric::Cost => format_usd(value),
            Metric::Tokens => format_tokens(value as u64),
        };
        let mut out = vec![];

        if report.models.is_empty() {
            out.push(
                div()
                    .mt(px(72.0))
                    .flex()
                    .flex_col()
                    .items_center()
                    .text_center()
                    .text_color(theme.text_muted)
                    .child(
                        crate::icons::icon(crate::icons::CHART)
                            .size(px(28.0))
                            .text_color(theme.text_muted.opacity(0.3)),
                    )
                    .child(
                        div()
                            .mt(px(12.0))
                            .text_size(crate::typography::ui_rems(14.0))
                            .child(SharedString::from(if report.scanning {
                                "Reading Pi's sessions…"
                            } else {
                                "No usage in this period"
                            })),
                    )
                    .into_any_element(),
            );
            return out;
        }

        // Hero: the total, then how it splits.
        let breakdown = match metric {
            Metric::Cost if estimated > 0.0 => format!(
                "{} recorded · ~{} estimated · {} tokens",
                format_usd(recorded),
                format_usd(estimated),
                format_tokens(tokens)
            ),
            Metric::Cost => format!("{} tokens · {requests} requests", format_tokens(tokens)),
            Metric::Tokens => format!("{} · {requests} requests", format_usd(recorded + estimated)),
        };
        out.push(
            div()
                .mt(px(24.0))
                .px(px(8.0))
                .flex()
                .flex_col()
                .gap(px(4.0))
                .child(
                    div()
                        .text_size(crate::typography::ui_rems(32.0))
                        .line_height(crate::typography::ui_rems(38.0))
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_color(theme.text)
                        .child(SharedString::from(value_text(total))),
                )
                .child(
                    div()
                        .text_size(crate::typography::ui_rems(12.0))
                        .text_color(theme.text_muted)
                        .child(SharedString::from(breakdown)),
                )
                .into_any_element(),
        );

        // Providers, each with its line's color.
        let rows: Vec<AnyElement> = ranked
            .iter()
            .enumerate()
            .map(|(ix, provider)| {
                let value = provider.value(metric);
                let share = if total > 0.0 { value / total } else { 0.0 };
                let swatch = chart::color(theme, ix.min(CHART_PROVIDERS));
                let detail = match metric {
                    Metric::Cost => format!(
                        "{} tokens · {} requests",
                        format_tokens(provider.tokens),
                        provider.requests
                    ),
                    Metric::Tokens => format!(
                        "{} · {} requests",
                        format_usd(provider.recorded_usd + provider.estimated_usd),
                        provider.requests
                    ),
                };
                widgets::card_row(theme, ix == 0)
                    .min_h(px(48.0))
                    .child(div().size(px(8.0)).flex_none().rounded_full().bg(swatch))
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(160.0))
                            .flex()
                            .flex_col()
                            .child(widgets::row_title(
                                theme,
                                provider_label(&provider.provider),
                            ))
                            .child(widgets::meta_line(
                                theme,
                                vec![SharedString::from(detail).into_any_element()],
                            )),
                    )
                    .child(
                        div()
                            .flex_none()
                            .text_size(crate::typography::ui_rems(12.0))
                            .text_color(theme.text_muted)
                            .child(SharedString::from(format_share(share))),
                    )
                    .child(
                        div()
                            .flex_none()
                            .min_w(px(72.0))
                            .text_right()
                            .text_size(crate::typography::ui_rems(13.0))
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_color(theme.text)
                            .child(SharedString::from(value_text(value))),
                    )
                    .into_any_element()
            })
            .collect();
        out.push(
            widgets::section(
                theme,
                "Providers",
                widgets::section_card(theme).mt_0().children(rows),
            )
            .into_any_element(),
        );

        // Over time.
        let chart = chart_for(
            report,
            metric,
            self.period,
            chrono::Utc::now().timestamp_millis(),
            Self::offset_ms(),
        );
        let page = cx.entity().downgrade();
        let toggle: chart::Toggle = Rc::new(move |ix, _window, cx| {
            page.update(cx, |page, cx| {
                if !page.hidden.remove(&ix) {
                    page.hidden.insert(ix);
                }
                cx.notify();
            })
            .ok();
        });
        out.push(
            widgets::section(
                theme,
                "Over time",
                widgets::section_card(theme).mt_0().child(chart::render(
                    &chart,
                    "usage-chart".into(),
                    &self.hidden,
                    toggle,
                    theme,
                )),
            )
            .into_any_element(),
        );

        // Models.
        let mut models: Vec<&UsageModelRow> = report.models.iter().collect();
        if metric == Metric::Tokens {
            models.sort_by(|a, b| b.tokens.total().cmp(&a.tokens.total()));
        }
        let rows: Vec<AnyElement> = models
            .into_iter()
            .enumerate()
            .map(|(ix, row)| {
                let t = &row.tokens;
                let mut meta = vec![
                    provider_label(&row.provider),
                    format!("{} requests", row.requests),
                    format!(
                        "{} in · {} out · {} cache read · {} cache write",
                        format_tokens(t.input),
                        format_tokens(t.output),
                        format_tokens(t.cache_read),
                        format_tokens(t.cache_write)
                    ),
                ];
                if row.unpriced_requests > 0 && row.cost_usd() > 0.0 {
                    meta.push(format!("{} unpriced", row.unpriced_requests));
                }
                let model = if row.model.is_empty() {
                    "Tool results and summaries".to_owned()
                } else {
                    row.model.clone()
                };
                let value = match metric {
                    Metric::Cost => model_cost_label(row),
                    Metric::Tokens => format_tokens(t.total()),
                };
                widgets::card_row(theme, ix == 0)
                    .min_h(px(52.0))
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(200.0))
                            .flex()
                            .flex_col()
                            .child(widgets::row_title(theme, model).truncate())
                            .child(widgets::meta_line(
                                theme,
                                meta.into_iter()
                                    .map(|m| SharedString::from(m).into_any_element())
                                    .collect(),
                            )),
                    )
                    .child(
                        div()
                            .flex_none()
                            .min_w(px(72.0))
                            .text_right()
                            .text_size(crate::typography::ui_rems(13.0))
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_color(theme.text)
                            .child(SharedString::from(value)),
                    )
                    .into_any_element()
            })
            .collect();
        out.push(
            widgets::section(
                theme,
                format!("Models · {}", report.models.len()),
                widgets::section_card(theme).mt_0().children(rows),
            )
            .into_any_element(),
        );
        out
    }

    /// What the numbers cover, under them.
    fn coverage(report: &UsageReport) -> String {
        let mut parts = vec![format!(
            "{} session files · {} sessions in this period",
            report.files, report.sessions
        )];
        if report.duplicates > 0 {
            parts.push(format!("{} forked copies counted once", report.duplicates));
        }
        if let Some(at) = report.scanned_at_ms
            && let Some(at) = chrono::Utc.timestamp_millis_opt(at).single()
        {
            parts.push(format!(
                "read {}",
                crate::state::format_time_ago(at, chrono::Utc::now())
            ));
        }
        format!(
            "{}. ~ marks spend priced here where Pi recorded $0. Subagents and workflows that keep no session file are not counted.",
            parts.join(" · ")
        )
    }
}

impl popover::ScrollRailHost for UsagePage {
    fn rail_bar(&mut self) -> &mut popover::MenuScrollbarState {
        self.scroll.rail_bar()
    }

    fn rail_scroll(&self) -> Option<gpui::ScrollHandle> {
        self.scroll.rail_scroll()
    }
}

impl Render for UsagePage {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx).for_settings_surface();
        let tabs = self.render_tabs(&theme, window, cx);
        let status = self.status.clone();
        let body: Vec<AnyElement> = match &status {
            Status::Loading => vec![
                div()
                    .mt(px(72.0))
                    .text_center()
                    .text_size(crate::typography::ui_rems(13.0))
                    .text_color(theme.text_muted)
                    .child(SharedString::from("Loading usage…"))
                    .into_any_element(),
            ],
            Status::Unsupported => vec![
                widgets::warning_strip(&theme, "Update Zeron on this machine to see its usage.")
                    .mt(px(24.0))
                    .into_any_element(),
            ],
            Status::Error(error) => vec![
                widgets::error_strip(&theme, format!("Usage unavailable: {error}"))
                    .mt(px(24.0))
                    .into_any_element(),
            ],
            Status::Ready(report) => {
                let mut body = self.render_report(report, &theme, cx);
                body.push(
                    div()
                        .mt(px(16.0))
                        .px(px(8.0))
                        .text_size(crate::typography::ui_rems(11.0))
                        .line_height(crate::typography::ui_rems(15.0))
                        .text_color(theme.text_faint)
                        .child(SharedString::from(Self::coverage(report)))
                        .into_any_element(),
                );
                body
            }
        };
        let stale = self.loading && matches!(status, Status::Ready(_));
        let scrollbar = popover::rail(self, "usage-page-scrollbar", &theme, cx);
        div()
            .id("usage-page-host")
            .relative()
            .size_full()
            .on_hover(cx.listener(Self::on_scroll_hovered))
            .child(
                crate::edge_fade::edge_faded(
                    16.0,
                    true,
                    true,
                    div()
                        .id("usage-page")
                        .size_full()
                        .overflow_y_scroll()
                        .track_scroll(&self.scroll.scroll)
                        .child(
                            widgets::page_column()
                                .child(widgets::page_header(&theme, "Usage", None))
                                .child(widgets::page_subtitle(
                                    &theme,
                                    "Tokens and spend for every Pi provider and model, from the sessions on this machine.",
                                ))
                                .child(tabs)
                                // Stale numbers dim while fresh ones load.
                                .child(div().when(stale, |el| el.opacity(0.6)).children(body)),
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
    use zeron_proto::usage::{UsagePoint, UsageTokens};

    fn row(
        provider: &str,
        model: &str,
        tokens: u64,
        recorded: f64,
        estimated: f64,
    ) -> UsageModelRow {
        UsageModelRow {
            provider: provider.into(),
            model: model.into(),
            requests: 1,
            tokens: UsageTokens {
                input: tokens,
                ..Default::default()
            },
            recorded_usd: recorded,
            estimated_usd: estimated,
            ..Default::default()
        }
    }

    #[test]
    fn numbers_read_compactly() {
        assert_eq!(format_tokens(19_900_000_000), "19.9B");
        assert_eq!(format_tokens(76_700_000), "76.7M");
        assert_eq!(format_tokens(804_000), "804K");
        assert_eq!(format_tokens(2_000_000), "2M");
        assert_eq!(format_tokens(950), "950");
        assert_eq!(format_usd(0.0), "$0");
        assert_eq!(format_usd(0.004), "<$0.01");
        assert_eq!(format_usd(12.346), "$12.35");
        assert_eq!(format_usd(5_077.92), "$5,078");
        assert_eq!(format_share(0.0004), "<0.1%");
        assert_eq!(format_share(0.035), "3.5%");
        assert_eq!(format_share(0.42), "42%");
        assert_eq!(provider_label(""), "Tools & summaries");
    }

    #[test]
    fn a_models_cost_says_how_it_was_found() {
        assert_eq!(model_cost_label(&row("a", "m", 1, 1.5, 0.0)), "$1.50");
        assert_eq!(model_cost_label(&row("c", "m", 1, 0.0, 2.0)), "~$2.00");
        let mut free = row("o", "m-free", 1, 0.0, 0.0);
        free.free_requests = 1;
        assert_eq!(model_cost_label(&free), "Free");
        let mut unknown = row("acme-llm", "x-1", 1, 0.0, 0.0);
        unknown.unpriced_requests = 1;
        assert_eq!(model_cost_label(&unknown), "Unpriced");
    }

    #[test]
    fn providers_rank_by_the_chosen_metric() {
        let report = UsageReport {
            models: vec![
                row("anthropic", "opus", 100, 50.0, 0.0),
                row("clinepass", "glm", 900, 0.0, 3.0),
                row("anthropic", "sonnet", 50, 10.0, 0.0),
            ],
            ..Default::default()
        };
        let by_cost = providers(&report, Metric::Cost);
        assert_eq!(by_cost[0].provider, "anthropic");
        assert_eq!(by_cost[0].tokens, 150);
        assert!((by_cost[0].value(Metric::Cost) - 60.0).abs() < 1e-9);
        assert_eq!(providers(&report, Metric::Tokens)[0].provider, "clinepass");
    }

    #[test]
    fn the_chart_has_a_line_per_top_provider_and_every_day_of_the_window() {
        let now = 1_790_884_800_000; // 2026-10-01 20:00 UTC
        let day = |n: i64| now.div_euclid(DAY_MS) * DAY_MS - n * DAY_MS;
        let mut models = vec![];
        let mut series = vec![];
        for ix in 0..9 {
            let provider = format!("p{ix}");
            models.push(row(&provider, "m", 10, 10.0 - ix as f64, 0.0));
            series.push(UsagePoint {
                start_ms: day(ix % 3),
                provider,
                tokens: 10,
                cost_usd: 1.0,
            });
        }
        let report = UsageReport {
            models,
            series,
            bucket: UsageBucket::Day,
            ..Default::default()
        };
        let chart = chart_for(&report, Metric::Cost, Period::Week, now, 0);
        assert_eq!(chart.kind, chart::Kind::Line);
        assert_eq!(chart.labels.len(), 7);
        assert_eq!(chart.series.len(), chart::MAX_SERIES);
        assert_eq!(chart.series.last().unwrap().name, "Other");
        // p7 and p8 share "Other", on days 1 and 2 back.
        let other: Vec<f64> = chart.series[7].points.iter().map(|p| p.y).collect();
        assert_eq!(other[4..], [1.0, 1.0, 0.0]);
        assert_eq!(chart.series[0].points[6].y, 1.0);
        assert_eq!(chart.unit.as_deref(), Some("$"));
    }

    #[test]
    fn a_long_history_merges_days_to_stay_readable() {
        let now = 1_790_884_800_000;
        let report = UsageReport {
            models: vec![row("p", "m", 10, 1.0, 0.0)],
            series: vec![UsagePoint {
                start_ms: now - 400 * DAY_MS,
                provider: "p".into(),
                tokens: 10,
                cost_usd: 1.0,
            }],
            bucket: UsageBucket::Day,
            first_ms: Some(now - 400 * DAY_MS),
            ..Default::default()
        };
        let chart = chart_for(&report, Metric::Tokens, Period::All, now, 0);
        assert!(
            chart.labels.len() <= CHART_POINTS as usize,
            "{}",
            chart.labels.len()
        );
        assert_eq!(chart.series[0].points[0].y, 10.0);
    }

    #[gpui::test]
    fn the_page_renders_loading_a_report_an_empty_scan_and_an_old_engine(
        cx: &mut gpui::TestAppContext,
    ) {
        use gpui::AppContext;
        let dir = tempfile::tempdir().unwrap();
        cx.update(|cx| {
            crate::settings::init(Default::default(), dir.path(), cx);
            gpui_base::init(cx);
            cx.set_global(crate::theme::Theme::default());
        });
        let window = cx.add_window(|_, cx| {
            let state = cx.new(|_| crate::state::AppState::new());
            UsagePage::new(state, cx)
        });
        let now = chrono::Utc::now().timestamp_millis();
        let mut unpriced = row("acme-llm", "x-1", 5_000, 0.0, 0.0);
        unpriced.unpriced_requests = 1;
        let report = UsageReport {
            models: vec![
                row("anthropic", "claude-opus-5-5", 4_000_000, 120.0, 0.0),
                row(
                    "clinepass",
                    "deepseek/deepseek-v4.1-flash",
                    9_000_000,
                    0.0,
                    2.5,
                ),
                unpriced,
                row("", "", 300, 0.0, 0.0),
            ],
            series: vec![UsagePoint {
                start_ms: now - DAY_MS,
                provider: "anthropic".into(),
                tokens: 4_000_000,
                cost_usd: 120.0,
            }],
            files: 3,
            sessions: 2,
            duplicates: 4,
            scanned_at_ms: Some(now),
            first_ms: Some(now - DAY_MS),
            ..Default::default()
        };
        let empty = UsageReport {
            scanning: true,
            ..Default::default()
        };
        for (status, metric) in [
            (Status::Loading, Metric::Cost),
            (Status::Ready(report.clone()), Metric::Cost),
            (Status::Ready(report), Metric::Tokens),
            (Status::Ready(empty), Metric::Cost),
            (Status::Unsupported, Metric::Cost),
            (Status::Error("engine gone".into()), Metric::Cost),
        ] {
            window
                .update(cx, |page, _, cx| {
                    page.status = status;
                    page.metric = metric;
                    cx.notify();
                })
                .unwrap();
            cx.update_window(window.into(), |_, window, cx| window.draw(cx).clear())
                .unwrap();
        }
        let coverage = UsagePage::coverage(&UsageReport {
            files: 3,
            sessions: 2,
            duplicates: 4,
            ..Default::default()
        });
        assert!(
            coverage.starts_with(
                "3 session files · 2 sessions in this period · 4 forked copies counted once."
            ),
            "{coverage}"
        );
    }

    #[test]
    fn periods_start_at_whole_hours_and_local_days() {
        let now = 1_790_884_800_000 + 1_234; // 2026-10-01 20:00:01.234 UTC
        assert_eq!(
            Period::Day.since_ms(now, 0),
            Some(1_790_884_800_000 - 23 * HOUR_MS)
        );
        // At UTC+5:30 the local day began 2026-10-01 18:30 UTC.
        assert_eq!(
            Period::Week.since_ms(now, 330 * 60_000),
            Some(1_790_879_400_000 - 6 * DAY_MS)
        );
        assert_eq!(Period::All.since_ms(now, 0), None);
    }
}
