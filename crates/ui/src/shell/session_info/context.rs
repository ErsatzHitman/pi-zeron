//! The session panel's Context section: how full the chat's context window
//! is and what fills it, the way Claude Code's /context breaks it down.
//! Pure, so it is unit-tested without a window; drawn by
//! `Shell::render_context`.
//!
//! The total and the window come from the chat's own `ContextUsage` (the
//! footer ring's numbers), so the two always agree; Pi's context-window
//! extension (`context:window`) supplies the split. Without the split the
//! section still shows what is used, the autocompact buffer and the free
//! space. Messages take whatever the measured total leaves after the fixed
//! parts, so the rows always add up to the headline.

use chrono::{DateTime, Utc};
use zeron_proto::ContextUsage;
use zeron_proto::extension_ui::{CompactStatus, ContextBreakdown, ContextMcpServer};

/// What a row stands for, which also picks its swatch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(super) enum Kind {
    SystemTools,
    McpTools,
    SystemPrompt,
    Messages,
    Skills,
    McpInstructions,
    Other,
    /// Tokens the provider has not measured yet (estimated).
    SinceUpdate,
    /// Used, when there is no split.
    Used,
    Autocompact,
    Free,
    DeferredMcp,
    DeferredSystem,
}

impl Kind {
    pub fn label(self) -> &'static str {
        match self {
            Self::SystemTools => "System tools",
            Self::McpTools => "MCP tools",
            Self::SystemPrompt => "System prompt",
            Self::Messages => "Messages",
            Self::Skills => "Skills",
            Self::McpInstructions => "MCP server instructions",
            Self::Other => "Other prompt text",
            Self::SinceUpdate => "Since last update",
            Self::Used => "Used",
            Self::Autocompact => "Autocompact buffer",
            Self::Free => "Free space",
            Self::DeferredMcp => "MCP tools (deferred)",
            Self::DeferredSystem => "System tools (deferred)",
        }
    }

    /// What the row's tooltip explains.
    pub fn about(self) -> &'static str {
        match self {
            Self::SystemTools => "Built-in and extension tools the model is sent",
            Self::McpTools => "MCP server tools the model is sent",
            Self::SystemPrompt => "Pi's system prompt and project context",
            Self::Messages => "The conversation: prompts, replies and tool results",
            Self::Skills => "The list of skills the model can load",
            Self::McpInstructions => "Instructions MCP servers add to the prompt",
            Self::Other => "Prompt text an extension supplied",
            Self::SinceUpdate => "Added since the provider last counted (estimated)",
            Self::Used => "In use, as the provider last counted",
            Self::Autocompact => "Kept free: Pi compacts the conversation before it fills",
            Self::Free => "Room left before Pi compacts",
            Self::DeferredMcp => "MCP tools the model is not sent until it looks them up",
            Self::DeferredSystem => "Tools the model is not sent until it looks them up",
        }
    }

    /// Part of the bar (the used rows and the buffer).
    fn on_bar(self) -> bool {
        !matches!(self, Self::Free | Self::DeferredMcp | Self::DeferredSystem)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(super) struct Row {
    pub kind: Kind,
    pub tokens: u64,
    /// Share of the window; deferred rows have none (they take no room).
    pub percent: Option<f64>,
    /// Tools, skills: how many.
    pub count: Option<u32>,
}

#[derive(Debug, Clone, PartialEq)]
pub(super) struct ContextView {
    pub used: u64,
    pub window: u64,
    /// The rows in Claude Code's order, Free space last.
    pub rows: Vec<Row>,
    /// Deferred tools: listed, but they take no room.
    pub deferred: Vec<Row>,
    /// MCP tools registered, and per server.
    pub mcp_total: u32,
    pub mcp_servers: Vec<ContextMcpServer>,
    /// The split is known (the extension reported).
    pub detailed: bool,
    /// The split is estimated rather than counted.
    pub estimated: bool,
    /// When the split last changed.
    pub updated: Option<DateTime<Utc>>,
}

impl ContextView {
    /// `None` when there is no window to measure against.
    pub fn of(
        breakdown: Option<(&ContextBreakdown, DateTime<Utc>)>,
        usage: Option<ContextUsage>,
        compact: Option<&CompactStatus>,
    ) -> Option<Self> {
        let window = usage
            .and_then(|u| u.window)
            .filter(|w| *w > 0)
            .or_else(|| breakdown.map(|(b, _)| b.window).filter(|w| *w > 0))?;
        let split = breakdown.map(|(b, _)| b);
        // Right after a compaction the measured total is stale: Pi's estimate
        // of what is left stands in, and nothing is "since" it.
        let after_compaction = compact.and_then(|c| c.tokens_after);
        let trailing = if after_compaction.is_some() {
            0
        } else {
            split.map_or(0, |b| b.trailing_tokens)
        };
        let measured = after_compaction
            .or_else(|| usage.and_then(|u| u.tokens))
            .or_else(|| split.and_then(|b| b.measured.tokens))
            .unwrap_or(0);
        let used = measured + trailing;
        let percent = |tokens: u64| Some(tokens as f64 * 100.0 / window as f64);
        let row = |kind, tokens, count| Row {
            kind,
            tokens,
            percent: percent(tokens),
            count,
        };

        let buffer = split
            .and_then(|b| b.autocompact.as_ref())
            .map(|a| if a.enabled { a.buffer_tokens } else { 0 })
            .or_else(|| {
                let c = compact.filter(|c| c.enabled)?;
                Some(window.saturating_sub(c.threshold_tokens.filter(|t| *t > 0)?))
            })
            .unwrap_or(0)
            .min(window);

        let mut rows = Vec::new();
        let mut deferred = Vec::new();
        if let Some(b) = split {
            let c = &b.categories;
            let fixed = [
                (Kind::SystemTools, c.system_tools),
                (Kind::McpTools, c.mcp_tools),
                (Kind::SystemPrompt, c.system_prompt),
            ];
            let after = [
                (Kind::Skills, c.skills),
                (Kind::McpInstructions, c.mcp_instructions),
                (Kind::Other, c.other),
            ];
            let static_tokens: u64 = fixed.iter().chain(&after).map(|(_, c)| c.tokens).sum();
            let messages = measured.saturating_sub(static_tokens);
            for (kind, category) in fixed {
                rows.push(row(kind, category.tokens, category.count));
            }
            rows.push(row(Kind::Messages, messages, None));
            for (kind, category) in after {
                if kind == Kind::Other && category.tokens == 0 {
                    continue;
                }
                rows.push(row(kind, category.tokens, category.count));
            }
            if trailing > 0 {
                rows.push(row(Kind::SinceUpdate, trailing, None));
            }
            for (kind, category) in [
                (Kind::DeferredMcp, b.deferred.mcp_tools),
                (Kind::DeferredSystem, b.deferred.system_tools),
            ] {
                if category.tokens > 0 || category.count.unwrap_or(0) > 0 {
                    deferred.push(Row {
                        kind,
                        tokens: category.tokens,
                        percent: None,
                        count: category.count,
                    });
                }
            }
        } else {
            rows.push(row(Kind::Used, used, None));
        }
        if buffer > 0 {
            rows.push(row(Kind::Autocompact, buffer, None));
        }
        rows.push(row(
            Kind::Free,
            window.saturating_sub(used).saturating_sub(buffer),
            None,
        ));

        Some(Self {
            used,
            window,
            rows,
            deferred,
            mcp_total: split.map_or(0, |b| b.mcp.total),
            mcp_servers: split.map(|b| b.mcp.servers.clone()).unwrap_or_default(),
            detailed: split.is_some(),
            estimated: split.is_some_and(|b| b.estimate.method != "exact"),
            updated: breakdown.map(|(_, at)| at),
        })
    }

    pub fn fraction(&self) -> f64 {
        (self.used as f64 / self.window as f64).clamp(0.0, 1.0)
    }

    /// "7% used".
    pub fn headline(&self) -> String {
        format!("{}% used", percent_whole(self.fraction()))
    }

    /// "10%" for the folded header.
    pub fn short(&self) -> String {
        format!("{}%", percent_whole(self.fraction()))
    }

    /// "71.3k / 1M".
    pub fn of_window(&self) -> String {
        format!("{} / {}", tokens(self.used), tokens(self.window))
    }

    /// The bar: each used row and the buffer, as shares of the window.
    pub fn segments(&self) -> Vec<(Kind, f32)> {
        self.rows
            .iter()
            .filter(|r| r.kind.on_bar() && r.tokens > 0)
            .map(|r| (r.kind, (r.tokens as f64 / self.window as f64) as f32))
            .collect()
    }

    /// "Last updated just now."
    pub fn updated_label(&self, now: DateTime<Utc>) -> Option<String> {
        let at = self.updated?;
        Some(format!(
            "Last updated {}.",
            crate::pull_requests::relative_time(at.min(now), now)
        ))
    }
}

/// `1%` from 0.5% up, `0%` below; never 100% until it is full.
fn percent_whole(fraction: f64) -> u32 {
    let p = (fraction * 100.0).round() as u32;
    if fraction < 1.0 { p.min(99) } else { p }
}

/// `941`, `2k`, `24.5k`, `895.7k`, `1M`, `1.5M`: Claude Code's /context.
pub(super) fn tokens(n: u64) -> String {
    let trim = |s: String| s.strip_suffix(".0").map(str::to_owned).unwrap_or(s);
    if n >= 1_000_000 {
        trim(format!("{:.1}", n as f64 / 1_000_000.0)) + "M"
    } else if n >= 1_000 {
        trim(format!("{:.1}", n as f64 / 1_000.0)) + "k"
    } else {
        n.to_string()
    }
}

/// `2.5%`, `0.1%`, `89.6%`.
pub(super) fn percent(p: f64) -> String {
    format!("{p:.1}%")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn now() -> DateTime<Utc> {
        DateTime::from_timestamp(1_760_000_000, 0).unwrap()
    }

    /// The breakdown in Claude Code's /context screenshot.
    fn breakdown() -> ContextBreakdown {
        serde_json::from_value(json!({
            "v": 1, "window": 1_000_000,
            "measured": {"tokens": 69_300, "source": "provider"}, "trailingTokens": 2_000,
            "categories": {
                "systemTools": {"tokens": 24_500, "count": 11},
                "mcpTools": {"tokens": 18_400, "count": 20},
                "systemPrompt": {"tokens": 11_500},
                "skills": {"tokens": 4_600, "count": 31},
                "mcpInstructions": {"tokens": 1_400},
                "messages": {"tokens": 1}
            },
            "deferred": {"systemTools": {"tokens": 23_900, "count": 7},
                         "mcpTools": {"tokens": 72_000, "count": 124}},
            "autocompact": {"enabled": true, "atTokens": 967_000, "bufferTokens": 33_000},
            "mcp": {"total": 144, "servers": [{"name": "dev", "count": 144, "tokens": 90_400, "declared": 20}]},
            "estimate": {"method": "chars/4", "scale": 1.0}
        }))
        .unwrap()
    }

    fn usage(tokens: u64) -> Option<ContextUsage> {
        Some(ContextUsage {
            tokens: Some(tokens),
            window: Some(1_000_000),
        })
    }

    #[test]
    fn the_screenshots_breakdown_adds_up() {
        let b = breakdown();
        let view = ContextView::of(Some((&b, now())), usage(69_300), None).unwrap();
        assert_eq!(view.headline(), "7% used");
        assert_eq!(view.of_window(), "71.3k / 1M");
        let rows: Vec<(&str, String, String)> = view
            .rows
            .iter()
            .map(|r| {
                (
                    r.kind.label(),
                    tokens(r.tokens),
                    percent(r.percent.unwrap()),
                )
            })
            .collect();
        let want = [
            ("System tools", "24.5k", "2.5%"),
            ("MCP tools", "18.4k", "1.8%"),
            ("System prompt", "11.5k", "1.1%"),
            ("Messages", "8.9k", "0.9%"),
            ("Skills", "4.6k", "0.5%"),
            ("MCP server instructions", "1.4k", "0.1%"),
            ("Since last update", "2k", "0.2%"),
            ("Autocompact buffer", "33k", "3.3%"),
            ("Free space", "895.7k", "89.6%"),
        ];
        assert_eq!(
            rows,
            want.map(|(a, b, c)| (a, b.to_string(), c.to_string()))
        );
        let total: u64 = view.rows.iter().map(|r| r.tokens).sum();
        assert_eq!(total, 1_000_000, "used, buffer and free fill the window");
        assert_eq!(
            view.deferred
                .iter()
                .map(|r| (r.kind.label(), tokens(r.tokens), r.percent))
                .collect::<Vec<_>>(),
            [
                ("MCP tools (deferred)", "72k".into(), None),
                ("System tools (deferred)", "23.9k".into(), None)
            ],
            "deferred tools take no room"
        );
        assert_eq!(view.mcp_total, 144);
        assert!(view.estimated && view.detailed);
        assert_eq!(
            view.updated_label(now()).as_deref(),
            Some("Last updated just now.")
        );
        let bar: f32 = view.segments().iter().map(|(_, f)| f).sum();
        assert!((bar - 0.1043).abs() < 1e-3, "used plus the buffer: {bar}");
        assert!(view.segments().iter().all(|(k, _)| *k != Kind::Free));
    }

    #[test]
    fn the_headline_follows_the_chats_own_usage() {
        // The extension's measurement lags the reply the footer already shows.
        let b = breakdown();
        let view = ContextView::of(Some((&b, now())), usage(80_000), None).unwrap();
        assert_eq!(view.used, 82_000);
        let messages = view.rows.iter().find(|r| r.kind == Kind::Messages).unwrap();
        assert_eq!(messages.tokens, 80_000 - 60_400);
    }

    #[test]
    fn without_a_split_it_shows_used_buffer_and_free() {
        let compact = CompactStatus {
            enabled: true,
            threshold_tokens: Some(983_616),
            ..Default::default()
        };
        let view = ContextView::of(None, usage(100_000), Some(&compact)).unwrap();
        assert!(!view.detailed);
        assert_eq!(
            view.rows
                .iter()
                .map(|r| (r.kind, r.tokens))
                .collect::<Vec<_>>(),
            [
                (Kind::Used, 100_000),
                (Kind::Autocompact, 16_384),
                (Kind::Free, 883_616)
            ]
        );
        assert_eq!(view.short(), "10%");
        assert_eq!(view.updated_label(now()), None);
    }

    #[test]
    fn a_real_pi_breakdown_shows_its_categories_and_deferred_tools() {
        // The payload a real Pi published after its first turn.
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../../tests/fixtures/context-window-live-pi.json"
        ))
        .unwrap();
        let mut ui = zeron_proto::extension_ui::ExtensionUiState::default();
        ui.channels.insert(
            zeron_proto::extension_ui::CONTEXT_WINDOW_CHANNEL.into(),
            fixture["payload"].clone(),
        );
        let b = ContextBreakdown::of(&ui).expect("the channel parses");
        let view = ContextView::of(Some((&b, now())), None, None).unwrap();
        assert!(view.detailed);
        assert_eq!(view.of_window(), "9 / 1M");
        let tokens = |kind: Kind| view.rows.iter().find(|r| r.kind == kind).map(|r| r.tokens);
        assert_eq!(tokens(Kind::SystemTools), Some(578));
        assert_eq!(tokens(Kind::SystemPrompt), Some(565));
        assert_eq!(tokens(Kind::Autocompact), Some(16_384));
        assert_eq!(view.deferred.iter().map(|r| r.tokens).sum::<u64>(), 401);
    }

    #[test]
    fn no_window_means_no_section() {
        assert_eq!(ContextView::of(None, None, None), None);
        let none = Some(ContextUsage {
            tokens: Some(5),
            window: Some(0),
        });
        assert_eq!(ContextView::of(None, none, None), None);
        // The extension's window stands in when the chat has none yet.
        let b = breakdown();
        assert_eq!(
            ContextView::of(Some((&b, now())), None, None)
                .unwrap()
                .window,
            1_000_000
        );
    }

    #[test]
    fn right_after_compaction_its_estimate_stands_in() {
        let b = breakdown();
        let compact = CompactStatus {
            enabled: true,
            tokens_after: Some(40_000),
            ..Default::default()
        };
        let view = ContextView::of(Some((&b, now())), usage(900_000), Some(&compact)).unwrap();
        assert_eq!(view.used, 40_000);
        assert!(view.rows.iter().all(|r| r.kind != Kind::SinceUpdate));
    }

    #[test]
    fn an_overfull_context_never_goes_negative() {
        let b = breakdown();
        let view = ContextView::of(Some((&b, now())), usage(990_000), None).unwrap();
        let free = view.rows.last().unwrap();
        assert_eq!((free.kind, free.tokens), (Kind::Free, 0));
        assert_eq!(view.headline(), "99% used");
        let tiny = ContextView::of(Some((&b, now())), usage(10), None).unwrap();
        let messages = tiny.rows.iter().find(|r| r.kind == Kind::Messages).unwrap();
        assert_eq!(
            messages.tokens, 0,
            "a stale split never makes messages negative"
        );
    }

    #[test]
    fn autocompact_off_has_no_buffer() {
        let mut b = breakdown();
        b.autocompact.as_mut().unwrap().enabled = false;
        let view = ContextView::of(Some((&b, now())), usage(69_300), None).unwrap();
        assert!(view.rows.iter().all(|r| r.kind != Kind::Autocompact));
        assert_eq!(view.rows.last().unwrap().tokens, 1_000_000 - 71_300);
    }

    #[test]
    fn token_counts_read_like_claude_codes() {
        assert_eq!(tokens(941), "941");
        assert_eq!(tokens(2_000), "2k");
        assert_eq!(tokens(24_500), "24.5k");
        assert_eq!(tokens(895_700), "895.7k");
        assert_eq!(tokens(1_000_000), "1M");
        assert_eq!(tokens(1_500_000), "1.5M");
        assert_eq!(percent(2.45), "2.5%");
        assert_eq!(percent(89.63), "89.6%");
    }
}
