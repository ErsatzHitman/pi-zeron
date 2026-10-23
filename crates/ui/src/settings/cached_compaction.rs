//! Settings → General → Cached compaction: the cached-compaction extension's
//! switches — whether a compaction replays the last request through the prompt
//! cache, the idle compact-before-expiry timer's context floor and lead, the
//! cache retention the session asks for, and whether its cost card and footer
//! countdown are drawn. The engine keeps them in the extension's own
//! `cached-compaction.json`, which the extension re-reads at every compaction,
//! so a change reaches running chats at their next one. Pi's `settings.json`
//! is never written.

use gpui::{Context, Entity, Subscription, Task, div, prelude::*, px};
use zeron_engine::cached_compaction::{CachedCompactionSettings, RETENTIONS};
use zeron_rpc::methods;

use crate::composer::{ComposerInput, ComposerInputEvent};
use crate::popover::{self, Loadable};
use crate::settings::widgets;
use crate::state::{AppState, EngineHandle};
use crate::theme::Theme;

const DESCRIPTION: &str = "Compaction resends the exact last request plus one 'summarize' \
     instruction, so the model reads the prompt cache instead of the whole conversation. \
     That path only runs while the conversation is unchanged: an edited, rewound or forked \
     chat falls back to Pi's own summary, which resends the transcript.";

const PRE_EXPIRY: &str = "Compact while the chat is idle, shortly before the prompt cache \
     entry expires, instead of waiting for the context threshold. It never fires while a \
     prompt is queued or the agent is working, and it also builds the countdown.";

const MIN_TOKENS: &str = "The idle timer never compacts a conversation smaller than this.";

const LEAD: &str = "How long before the cache entry expires the idle timer compacts, in \
     seconds (5–3600). Most providers report no lifetime: then there is no countdown and \
     no pre-expiry compaction, only the cached summary.";

const RETENTION: &str = "How long the provider keeps the prompt cached. Auto keeps it an \
     hour for a chat you are watching and 'short' for sub-agents and headless runs, which \
     nobody is watching. On Anthropic 'long' bills cache writes at 2x, so a chat that \
     stays warm pays it back and a one-off does not.";

const COST_CARD: &str = "Appends the cached-vs-native comparison to the chat after each \
     compaction. Both sides are estimates, so read the card as 'about'.";

const FOOTER: &str = "Publishes the cache countdown to Zeron's footer chip and the session \
     panel's Compaction section.";

const COUNT_HINT: &str = "Tokens, e.g. 50000 or 50k";

/// The rows as one list, in the order they are drawn: the master switch, the
/// idle timer with its two numbers, then what the session asks the provider for
/// and what it draws.
const ROWS: [&str; 7] = [
    "Enabled",
    "Compact before the cache expires",
    "Minimum context",
    "Lead time",
    "Cache retention",
    "Cost card",
    "Footer countdown",
];

/// What one engine call does; every reply is the device's settings.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Request {
    Get,
    /// A re-read on opening Settings: the page outlives visits to it.
    Refresh,
    Enabled(bool),
    PreExpiry(bool),
    MinTokens(u64),
    Lead(u64),
    Retention(String),
    CostCard(bool),
    FooterCountdown(bool),
}

impl Request {
    /// Whether this only re-reads the device (a save applies the change to a
    /// fresh copy first, so a hand edit from a terminal or another window is
    /// not undone).
    fn is_load(&self) -> bool {
        matches!(self, Self::Get | Self::Refresh)
    }

    /// `settings` with this request's one change applied.
    fn apply(&self, settings: &mut CachedCompactionSettings) {
        match self {
            Self::Get | Self::Refresh => {}
            Self::Enabled(on) => settings.enabled = *on,
            Self::PreExpiry(on) => settings.compact_before_expiry = *on,
            Self::MinTokens(tokens) => settings.min_context_tokens = *tokens,
            Self::Lead(ms) => settings.lead_ms = *ms,
            Self::Retention(retention) => settings.cache_retention = retention.clone(),
            Self::CostCard(on) => settings.cost_card = *on,
            Self::FooterCountdown(on) => settings.footer_countdown = *on,
        }
    }

    /// The change as the engine's refusal would name it, for an error that
    /// has to say what was refused.
    fn label(&self) -> &'static str {
        match self {
            Self::Get | Self::Refresh => "cached compaction",
            Self::Enabled(_) => "enabled",
            Self::PreExpiry(_) => "compact before the cache expires",
            Self::MinTokens(_) => "minimum context",
            Self::Lead(_) => "lead time",
            Self::Retention(_) => "cache retention",
            Self::CostCard(_) => "cost card",
            Self::FooterCountdown(_) => "footer countdown",
        }
    }
}

pub struct CachedCompactionCard {
    state: Entity<AppState>,
    /// The device whose `cached-compaction.json` is shown (`None` = this
    /// engine's own).
    device: Option<String>,
    /// The engine `settings` came from: an account switch replaces it.
    engine: Option<EngineHandle>,
    settings: Loadable<CachedCompactionSettings>,
    tokens_input: Entity<ComposerInput>,
    lead_input: Entity<ComposerInput>,
    retention_select: widgets::SelectState,
    saving: bool,
    error: Option<String>,
    task: Option<Task<()>>,
    _inputs: Vec<Subscription>,
    _state: Subscription,
}

impl CachedCompactionCard {
    pub fn new(state: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        let tokens_input = cx.new(|cx| ComposerInput::new(COUNT_HINT, cx).with_single_line());
        let lead_input = cx.new(|cx| ComposerInput::new("Seconds, e.g. 20", cx).with_single_line());
        let mut inputs = Vec::new();
        inputs.push(cx.subscribe(&tokens_input, |this: &mut Self, _, event, cx| {
            if matches!(event, ComposerInputEvent::Submitted) {
                this.save_tokens(cx);
            }
        }));
        inputs.push(cx.subscribe(&lead_input, |this: &mut Self, _, event, cx| {
            if matches!(event, ComposerInputEvent::Submitted) {
                this.save_lead(cx);
            }
        }));
        // Settings can open before the engine connects: load once it does. A
        // device switch is another Pi, and a replaced engine (an account
        // switch, a reconnect) another answer: reload through both.
        let state_sub = cx.observe(&state, |this: &mut Self, state, cx| {
            let (device, engine) = {
                let state = state.read(cx);
                (
                    crate::settings::target_device(&state),
                    state.engine().cloned(),
                )
            };
            if device != this.device
                || crate::settings::engine_replaced(this.engine.as_ref(), engine.as_ref())
            {
                this.device = device;
                this.settings = Loadable::Idle;
                this.call(Request::Get, cx);
            } else if matches!(this.settings, Loadable::Idle) {
                this.call(Request::Get, cx);
            }
        });
        let mut card = Self {
            device: crate::settings::target_device(&state.read(cx)),
            engine: None,
            state,
            settings: Loadable::Idle,
            tokens_input,
            lead_input,
            retention_select: widgets::SelectState::default(),
            saving: false,
            error: None,
            task: None,
            _inputs: inputs,
            _state: state_sub,
        };
        card.call(Request::Get, cx);
        card
    }

    /// Re-read what the device sets: the settings page outlives visits to it,
    /// so what this card loaded when it opened can be well out of date.
    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        if !self.saving && !matches!(self.settings, Loadable::Idle | Loadable::Loading) {
            self.call(Request::Refresh, cx);
        }
    }

    fn set_enabled(&mut self, on: bool, cx: &mut Context<Self>) {
        self.call(Request::Enabled(on), cx);
    }

    fn set_pre_expiry(&mut self, on: bool, cx: &mut Context<Self>) {
        self.call(Request::PreExpiry(on), cx);
    }

    fn set_cost_card(&mut self, on: bool, cx: &mut Context<Self>) {
        self.call(Request::CostCard(on), cx);
    }

    fn set_footer_countdown(&mut self, on: bool, cx: &mut Context<Self>) {
        self.call(Request::FooterCountdown(on), cx);
    }

    /// The typed context floor, as a save. Text that is not a count stays in
    /// the field and says why instead of being silently dropped.
    fn save_tokens(&mut self, cx: &mut Context<Self>) {
        match parse_count(self.tokens_input.read(cx).text()) {
            Ok(tokens) => self.call(Request::MinTokens(tokens), cx),
            Err(error) => {
                self.error = Some(error);
                cx.notify();
            }
        }
    }

    /// The typed lead is in seconds; the file's `leadMs` is milliseconds.
    fn save_lead(&mut self, cx: &mut Context<Self>) {
        match parse_count(self.lead_input.read(cx).text()).and_then(lead_ms) {
            Ok(lead_ms) => self.call(Request::Lead(lead_ms), cx),
            Err(error) => {
                self.error = Some(error);
                cx.notify();
            }
        }
    }

    /// `GetCachedCompactionSettings` for a load, or a save of the one field a
    /// change names: [`Request::is_load`] picks the path.
    fn call(&mut self, request: Request, cx: &mut Context<Self>) {
        if request.is_load() {
            self.load(cx);
        } else {
            self.change(request, cx);
        }
    }

    /// Apply `request` to what the device sets *now* and save that: this
    /// card's copy can be stale (the page outlives visits, a terminal
    /// `/cached-compaction` may have saved since), and writing it back whole
    /// would undo whatever else changed. Each change waits for the one before
    /// it, so two quick toggles both land.
    fn change(&mut self, request: Request, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        self.engine = Some(engine.clone());
        let device = self.device.clone();
        self.saving = true;
        let previous = self.task.take();
        let asked = engine.clone();
        self.task = Some(cx.spawn(async move |this, cx| {
            if let Some(previous) = previous {
                previous.await;
            }
            let outcome = match get(&engine, device.as_deref()).await {
                Ok(current) => {
                    let mut wanted = current.clone();
                    request.apply(&mut wanted);
                    match set(&engine, wanted, device.as_deref()).await {
                        Ok(settings) => Outcome::Set(settings),
                        Err(error) => Outcome::Refused {
                            error: format!("Cached compaction: {} could not be saved: {error}", request.label()),
                            current,
                        },
                    }
                }
                Err(error) => Outcome::Failed(error),
            };
            this.update(cx, |card, cx| {
                if card.device == device && card.answered(&asked) {
                    card.settled(outcome, cx);
                }
            })
            .ok();
        }));
        cx.notify();
    }

    /// `GetCachedCompactionSettings`, or a re-read of the device.
    fn load(&mut self, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        self.engine = Some(engine.clone());
        if matches!(self.settings, Loadable::Idle) {
            self.settings = Loadable::Loading;
        }
        let device = self.device.clone();
        let previous = self.task.take();
        let asked = engine.clone();
        self.task = Some(cx.spawn(async move |this, cx| {
            if let Some(previous) = previous {
                previous.await;
            }
            let outcome = match get(&engine, device.as_deref()).await {
                Ok(settings) => Outcome::Set(settings),
                Err(error) => Outcome::Failed(error),
            };
            this.update(cx, |card, cx| {
                if card.device == device && card.answered(&asked) {
                    card.settled(outcome, cx);
                }
            })
            .ok();
        }));
        cx.notify();
    }

    /// Whether a reply from `asked` still belongs to this card: a replaced
    /// engine (another account, a reconnect) owns it now.
    fn answered(&self, asked: &EngineHandle) -> bool {
        self.engine
            .as_ref()
            .is_some_and(|engine| engine.same_connection(asked))
    }

    /// A finished load or save, applied to the card and its fields.
    fn settled(&mut self, outcome: Outcome, cx: &mut Context<Self>) {
        self.saving = false;
        match outcome {
            Outcome::Set(settings) => {
                self.show(&settings, cx);
                self.settings = Loadable::Ready(settings);
                self.error = None;
            }
            // The engine's refusal (an unknown retention, a file that is not
            // JSON) goes to the toast, and the card shows what the device
            // still sets: a change that was not saved is never shown as if it
            // were.
            Outcome::Refused { error, current } => {
                self.show(&current, cx);
                self.settings = Loadable::Ready(current);
                self.error = Some(error);
            }
            Outcome::Failed(error) if matches!(self.settings, Loadable::Ready(_)) => {
                if let Some(settings) = self.settings.ready().cloned() {
                    self.show(&settings, cx);
                }
                self.error = Some(error);
            }
            Outcome::Failed(error) => self.settings = Loadable::Error(error),
        }
        cx.notify();
    }

    /// The fields show `settings`: the two numbers as the file holds them
    /// (clamped by the engine, so a specimen of what the extension will use).
    fn show(&self, settings: &CachedCompactionSettings, cx: &mut Context<Self>) {
        let tokens = settings.min_context_tokens.to_string();
        let seconds = settings.lead_ms / 1_000;
        let seconds = if settings.lead_ms % 1_000 == 0 {
            seconds.to_string()
        } else {
            format!("{:.1}", settings.lead_ms as f64 / 1_000.0)
        };
        self.tokens_input
            .update(cx, |input, cx| input.set_text(tokens, cx));
        self.lead_input
            .update(cx, |input, cx| input.set_text(seconds, cx));
    }
}

/// How a call to the engine ended.
enum Outcome {
    /// A load's reply, or a save the engine accepted: what the device sets.
    Set(CachedCompactionSettings),
    /// A save the engine refused, with what the device set before it.
    Refused {
        error: String,
        current: CachedCompactionSettings,
    },
    /// The call itself failed (no engine, a reply that does not parse).
    Failed(String),
}

/// The device's `cached-compaction.json`, as the engine has it.
async fn get(
    engine: &EngineHandle,
    device: Option<&str>,
) -> Result<CachedCompactionSettings, String> {
    engine
        .client()
        .call(
            methods::GET_CACHED_COMPACTION_SETTINGS,
            crate::settings::with_target_device(serde_json::json!({}), device),
        )
        .await
        .map_err(|e| e.to_string())
        .and_then(|v| serde_json::from_value(v).map_err(|e| e.to_string()))
}

/// Save `settings`; the reply is what the device now sets.
async fn set(
    engine: &EngineHandle,
    settings: CachedCompactionSettings,
    device: Option<&str>,
) -> Result<CachedCompactionSettings, String> {
    let params = crate::settings::with_target_device(
        serde_json::to_value(settings).unwrap_or_default(),
        device,
    );
    engine
        .client()
        .call(methods::SET_CACHED_COMPACTION_SETTINGS, params)
        .await
        .map_err(|e| e.to_string())
        .and_then(|v| serde_json::from_value(v).map_err(|e| e.to_string()))
}

/// A count typed into a number row: digits, with spaces, commas or underscores
/// between them, and an optional `k`/`m` (or a fraction of one) for thousands
/// or millions. A plain number is whole: `12.5` tokens is a typo, while `1.5k`
/// is 1500.
fn parse_count(text: &str) -> Result<u64, String> {
    let text = text.trim();
    let (digits, scale) = match text.chars().last() {
        Some('k' | 'K') => (&text[..text.len() - 1], 1_000),
        Some('m' | 'M') => (&text[..text.len() - 1], 1_000_000),
        _ => (text, 1),
    };
    let cleaned: String = digits
        .chars()
        .filter(|c| !matches!(c, ' ' | ',' | '_' | '\''))
        .collect();
    let not_a_number = || format!("{text} is not a number");
    let too_large = || format!("{text} is too large");
    if scale == 1 {
        return cleaned.parse::<u64>().map_err(|_| not_a_number());
    }
    let scaled = cleaned.parse::<f64>().map_err(|_| not_a_number())? * scale as f64;
    (scaled.is_finite() && scaled >= 0.0 && scaled <= u64::MAX as f64)
        .then(|| scaled.round() as u64)
        .ok_or_else(too_large)
}

/// A lead typed in seconds as the file's milliseconds.
fn lead_ms(seconds: u64) -> Result<u64, String> {
    seconds
        .checked_mul(1_000)
        .ok_or_else(|| format!("{seconds} seconds is too long a lead"))
}

/// The retention menu's row for a value: an unknown one (only possible from a
/// hand-edited file) shows `auto`, which is what the extension falls back to.
fn retention_index(retention: &str) -> usize {
    RETENTIONS
        .iter()
        .position(|known| *known == retention)
        .unwrap_or(0)
}

impl Render for CachedCompactionCard {
    fn render(&mut self, _: &mut gpui::Window, cx: &mut Context<Self>) -> impl IntoElement {
        crate::toast::take_error(&mut self.error, cx);
        let theme = Theme::of(cx).for_settings_surface();
        let block = widgets::section_card(&theme).mt_0();
        let Some(settings) = self.settings.ready().cloned() else {
            let line = match &self.settings {
                Loadable::Error(error) => error.clone(),
                _ => "Loading…".into(),
            };
            return widgets::section(
                &theme,
                "Cached compaction",
                block.child(
                    widgets::card_row(&theme, true)
                        .child(
                            div()
                                .flex_1()
                                .min_w(px(160.0))
                                .child(widgets::row_title(&theme, ROWS[0]))
                                .child(widgets::meta_line(
                                    &theme,
                                    vec![div().child(DESCRIPTION).into_any_element()],
                                )),
                        )
                        .child(div().text_color(theme.text_muted).child(line)),
                ),
            );
        };
        let rows = [
            self.render_enabled(&theme, &settings, cx),
            self.render_pre_expiry(&theme, &settings, cx),
            self.render_tokens(&theme, &settings, cx),
            self.render_lead(&theme, &settings, cx),
            self.render_retention(&theme, cx),
            self.render_cost_card(&theme, &settings, cx),
            self.render_footer(&theme, &settings, cx),
        ];
        widgets::section(
            &theme,
            "Cached compaction",
            widgets::section_card(&theme).mt_0().children(rows),
        )
    }
}

impl CachedCompactionCard {
    /// One switch row: title and note on the left, the switch on the right.
    fn toggle_row(
        &self,
        theme: &Theme,
        row: usize,
        on: bool,
        meta: &'static str,
        key: &'static str,
        cx: &mut Context<Self>,
        change: impl Fn(&mut Self, bool, &mut Context<Self>) + 'static,
    ) -> gpui::Div {
        let inert = self.saving || !matches!(self.settings, Loadable::Ready(_));
        widgets::card_row(theme, row == 0)
            .child(
                div()
                    .flex_1()
                    .min_w(px(160.0))
                    .child(widgets::row_title(theme, ROWS[row]))
                    .child(widgets::meta_line(
                        theme,
                        vec![div().child(meta).into_any_element()],
                    )),
            )
            .child(
                widgets::toggle_switch(theme, on, key)
                    .id(key)
                    .debug_selector(move || key.into())
                    .tab_index(0)
                    .role(gpui::Role::Switch)
                    .aria_label(ROWS[row])
                    .aria_toggled(if on {
                        gpui::Toggled::True
                    } else {
                        gpui::Toggled::False
                    })
                    .focus_visible(|s| s.border_2().border_color(theme.accent))
                    .when(inert, |el| el.opacity(0.5))
                    .when(!inert, |el| el.cursor_pointer())
                    .on_click(cx.listener(move |card, _, _, cx| change(card, !on, cx))),
            )
    }

    fn render_enabled(
        &self,
        theme: &Theme,
        settings: &CachedCompactionSettings,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        self.toggle_row(
            theme,
            0,
            settings.enabled,
            DESCRIPTION,
            "cached-compaction-enabled",
            cx,
            |card, on, cx| card.set_enabled(on, cx),
        )
    }

    fn render_pre_expiry(
        &self,
        theme: &Theme,
        settings: &CachedCompactionSettings,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        self.toggle_row(
            theme,
            1,
            settings.compact_before_expiry,
            PRE_EXPIRY,
            "cached-compaction-pre-expiry",
            cx,
            |card, on, cx| card.set_pre_expiry(on, cx),
        )
    }

    fn render_cost_card(
        &self,
        theme: &Theme,
        settings: &CachedCompactionSettings,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        self.toggle_row(
            theme,
            5,
            settings.cost_card,
            COST_CARD,
            "cached-compaction-cost-card",
            cx,
            |card, on, cx| card.set_cost_card(on, cx),
        )
    }

    fn render_footer(
        &self,
        theme: &Theme,
        settings: &CachedCompactionSettings,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        self.toggle_row(
            theme,
            6,
            settings.footer_countdown,
            FOOTER,
            "cached-compaction-footer",
            cx,
            |card, on, cx| card.set_footer_countdown(on, cx),
        )
    }

    /// A number row: the field saves on Enter.
    fn number_row(
        &self,
        theme: &Theme,
        row: usize,
        meta: &'static str,
        input: Entity<ComposerInput>,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        let inert = self.saving;
        let save = widgets::text_action(theme, widgets::ActionTone::Outlined, "Save")
            .id(("cached-compaction-save", row))
            .tab_index(0)
            .role(gpui::Role::Button)
            .aria_label(ROWS[row])
            .focus_visible(|s| s.border_2().border_color(theme.accent))
            .when(inert, |el| el.opacity(0.5))
            .on_click(cx.listener(move |card, _, _, cx| match row {
                2 => card.save_tokens(cx),
                _ => card.save_lead(cx),
            }));
        widgets::card_row(theme, false)
            .child(
                div()
                    .flex_1()
                    .min_w(px(160.0))
                    .child(widgets::row_title(theme, ROWS[row]))
                    .child(widgets::meta_line(
                        theme,
                        vec![div().child(meta).into_any_element()],
                    )),
            )
            .child(
                div()
                    .flex_none()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(8.0))
                    .child(
                        div()
                            .w(px(150.0))
                            .child(popover::dialog_field(input.into_any_element())),
                    )
                    .child(save),
            )
    }

    fn render_tokens(
        &self,
        theme: &Theme,
        _settings: &CachedCompactionSettings,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        self.number_row(theme, 2, MIN_TOKENS, self.tokens_input.clone(), cx)
    }

    fn render_lead(
        &self,
        theme: &Theme,
        _settings: &CachedCompactionSettings,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        self.number_row(theme, 3, LEAD, self.lead_input.clone(), cx)
    }

    /// The retention menu: `auto`, then the two tiers.
    fn render_retention(&self, theme: &Theme, cx: &mut Context<Self>) -> gpui::Div {
        let selected = self
            .settings
            .ready()
            .map(|settings| retention_index(&settings.cache_retention))
            .unwrap_or(0);
        let select = widgets::select(
            "cached-compaction-retention",
            ROWS[4],
            theme,
            |card: &mut Self| &mut card.retention_select,
        )
        .options(
            RETENTIONS
                .iter()
                .map(|retention| widgets::SelectOption::new(*retention)),
            selected,
        )
        .width(150.0)
        .on_select(|card, ix, _, cx| {
            if let Some(retention) = RETENTIONS.get(ix) {
                if !card.saving {
                    card.call(Request::Retention((*retention).to_owned()), cx);
                }
            }
        })
        .render(&self.retention_select, cx);
        widgets::card_row(theme, false)
            .child(
                div()
                    .flex_1()
                    .min_w(px(160.0))
                    .child(widgets::row_title(theme, ROWS[4]))
                    .child(widgets::meta_line(
                        theme,
                        vec![div().child(RETENTION).into_any_element()],
                    )),
            )
            .child(select)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pull_request_test_support::{self as fixture, ScriptedRpc};
    use zeron_engine::cached_compaction::{MAX_LEAD_MS, MIN_CONTEXT_TOKENS, MIN_LEAD_MS};
    use zeron_rpc::RpcReply;

    fn default_ring() -> serde_json::Value {
        serde_json::to_value(CachedCompactionSettings::default()).unwrap()
    }

    /// A number row reads a typed count, with separators and a k/m suffix.
    #[test]
    fn a_typed_count_is_read_as_a_number() {
        assert_eq!(parse_count("50000"), Ok(50_000));
        assert_eq!(parse_count(" 50 000 "), Ok(50_000));
        assert_eq!(parse_count("50,000"), Ok(50_000));
        assert_eq!(parse_count("50_000"), Ok(50_000));
        assert_eq!(parse_count("50k"), Ok(50_000));
        assert_eq!(parse_count("1.5m"), Ok(1_500_000));
        assert_eq!(parse_count("1.5k"), Ok(1_500));
        assert!(parse_count("").is_err());
        assert!(parse_count("lots").is_err());
        assert!(parse_count("12.5").is_err());
        // A lead is seconds; the file holds milliseconds.
        assert_eq!(lead_ms(20), Ok(20_000));
        assert_eq!(lead_ms(0), Ok(0));
        assert_eq!(parse_count("20").and_then(lead_ms), Ok(20_000));
    }

    #[test]
    fn the_retention_menu_rows_are_auto_short_long() {
        assert_eq!(RETENTIONS, ["auto", "short", "long"]);
        assert_eq!(retention_index("auto"), 0);
        assert_eq!(retention_index("short"), 1);
        assert_eq!(retention_index("long"), 2);
        assert_eq!(retention_index("forever"), 0, "unknown reads as auto");
        assert!(MIN_CONTEXT_TOKENS > 0 && MAX_LEAD_MS >= MIN_LEAD_MS);
    }

    /// Each request changes exactly the one value it names, on a copy of what
    /// the device has.
    #[test]
    fn a_request_changes_only_its_own_field() {
        let base = CachedCompactionSettings::default();
        let mut settings = base.clone();
        Request::Enabled(false).apply(&mut settings);
        assert!(!settings.enabled);
        assert_eq!(
            CachedCompactionSettings {
                enabled: true,
                ..settings.clone()
            },
            base
        );
        let mut settings = base.clone();
        Request::Retention("long".into()).apply(&mut settings);
        assert_eq!(settings.cache_retention, "long");
        let mut settings = base.clone();
        Request::MinTokens(90_000).apply(&mut settings);
        assert_eq!(settings.min_context_tokens, 90_000);
        let mut settings = base.clone();
        Request::Lead(45_000).apply(&mut settings);
        assert_eq!(settings.lead_ms, 45_000);
        assert!(Request::Get.is_load() && Request::Refresh.is_load());
        assert!(!Request::Enabled(true).is_load());
        assert_eq!(
            Request::Retention("long".into()).label(),
            "cache retention"
        );
    }

    /// The card loads what the device sets, and a toggle saves and lands.
    #[gpui::test]
    fn a_toggle_saves_and_reloads(cx: &mut gpui::TestAppContext) {
        let runtime = fixture::runtime();
        let _guard = runtime.enter();
        let _settings = fixture::settings(cx, Default::default());
        let rpc = ScriptedRpc::new(|method, params| async move {
            match method.as_str() {
                methods::GET_CACHED_COMPACTION_SETTINGS => RpcReply::value(&default_ring()),
                methods::SET_CACHED_COMPACTION_SETTINGS => {
                    assert_eq!(params["enabled"], serde_json::json!(false));
                    assert_eq!(params["cacheRetention"], serde_json::json!("auto"));
                    let mut settings: CachedCompactionSettings =
                        serde_json::from_value(params).unwrap();
                    settings.enabled = false;                    RpcReply::value(&settings)
                }
                other => panic!("unexpected method {other}"),
            }
        });
        let state = cx.update(|cx| fixture::state(cx, Some(rpc.client())));
        let card = cx.new(|cx| CachedCompactionCard::new(state.clone(), cx));
        rpc.settle(cx, &runtime, |cx| {
            card.read_with(cx, |card, _| {
                matches!(&card.settings, Loadable::Ready(settings) if settings.enabled)
            })
        });
        card.update(cx, |card, cx| card.set_enabled(false, cx));
        rpc.settle(cx, &runtime, |cx| {
            card.read_with(cx, |card, _| {
                matches!(&card.settings, Loadable::Ready(settings) if !settings.enabled)
            })
        });
        card.read_with(cx, |card, _| {
            assert!(card.error.is_none());
            assert!(!card.saving);
        });
        assert!(rpc.completed() >= 3, "a load, the change's read, then the save");
    }

    /// A save the engine refuses leaves the card on what the device still
    /// sets — never the value that failed to save — and says why.
    #[gpui::test]
    fn a_failed_save_surfaces_an_error_and_keeps_the_saved_value(
        cx: &mut gpui::TestAppContext,
    ) {
        let runtime = fixture::runtime();
        let _guard = runtime.enter();
        let _settings = fixture::settings(cx, Default::default());
        let rpc = ScriptedRpc::new(|method, _| async move {
            match method.as_str() {
                methods::GET_CACHED_COMPACTION_SETTINGS => RpcReply::value(&default_ring()),
                _ => Err(zeron_rpc::RpcError::Failed(
                    "forever is not a cache retention (auto, short, long)".into(),
                )),
            }
        });
        let state = cx.update(|cx| fixture::state(cx, Some(rpc.client())));
        let card = cx.new(|cx| CachedCompactionCard::new(state.clone(), cx));
        rpc.settle(cx, &runtime, |cx| {
            card.read_with(cx, |card, _| matches!(card.settings, Loadable::Ready(_)))
        });
        card.update(cx, |card, cx| {
            card.call(Request::Retention("forever".into()), cx)
        });
        rpc.settle(cx, &runtime, |cx| {
            card.read_with(cx, |card, _| card.error.is_some())
        });
        card.read_with(cx, |card, _| {
            let shown = card.settings.ready().expect("the device's settings");
            assert_eq!(
                shown.cache_retention, "auto",
                "the refused value is not left in place"
            );
            let error = card.error.as_deref().unwrap_or_default();
            assert!(error.contains("cache retention"), "{error}");
            assert!(error.contains("could not be saved"), "{error}");
        });
    }

    /// Opening Settings re-reads the device.
    #[gpui::test]
    fn refresh_re_reads_the_device(cx: &mut gpui::TestAppContext) {
        let runtime = fixture::runtime();
        let _guard = runtime.enter();
        let _settings = fixture::settings(cx, Default::default());
        let rpc = ScriptedRpc::new(|method, _| async move {
            assert_eq!(method, methods::GET_CACHED_COMPACTION_SETTINGS);
            let settings = CachedCompactionSettings {
                lead_ms: 30_000,
                ..CachedCompactionSettings::default()
            };
            RpcReply::value(&settings)
        });
        let state = cx.update(|cx| fixture::state(cx, Some(rpc.client())));
        let card = cx.new(|cx| CachedCompactionCard::new(state.clone(), cx));
        rpc.settle(cx, &runtime, |cx| {
            card.read_with(cx, |card, _| {
                matches!(&card.settings, Loadable::Ready(settings) if settings.lead_ms == 30_000)
            })
        });
        card.read_with(cx, |card, cx| {
            assert_eq!(card.lead_input.read(cx).text(), "30", "seconds, not ms");
            assert_eq!(card.tokens_input.read(cx).text(), "50000");
        });
        card.update(cx, |card, cx| card.refresh(cx));
        rpc.settle(cx, &runtime, |_| rpc.completed() >= 2);
    }
}
