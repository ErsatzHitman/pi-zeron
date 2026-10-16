//! Settings → API keys: every provider API key Zeron and its Pi use, in one
//! place. Each provider holds any number of labelled keys and one active
//! key; adding, editing, switching or removing a key takes effect on the
//! next request (Pi, its extensions, MCP servers such as video inspection,
//! dictation and voice mode) with no /reload and no restart.
//!
//! The engine owns the store (`zeron_engine::api_keys`); this page is its
//! client. Secrets go to the engine once, typed into a masked field, and
//! never come back: the page sees labels and the last four characters.

use gpui::{AnyElement, Context, Entity, SharedString, Subscription, Task, div, prelude::*, px};
use zeron_engine::api_keys::{ApiKeysView, KeyView, ProviderView};
use zeron_rpc::methods;

use crate::composer::{ComposerInput, ComposerInputEvent};
use crate::popover::{self, Loadable};
use crate::settings::widgets;
use crate::state::AppState;
use crate::theme::Theme;

/// One engine call; every reply is the whole page.
#[derive(Debug, Clone, PartialEq)]
enum Request {
    List,
    Add {
        provider: String,
        label: String,
        secret: String,
    },
    Update {
        provider: String,
        id: String,
        label: Option<String>,
        secret: Option<String>,
        disabled: Option<bool>,
    },
    Activate {
        provider: String,
        id: String,
    },
    Remove {
        provider: String,
        id: String,
    },
}

/// The open add or edit form: which provider (and key), its two fields.
struct Form {
    provider: String,
    /// Editing this key; `None` adds one.
    key: Option<String>,
    label: Entity<ComposerInput>,
    secret: Entity<ComposerInput>,
    _subs: Vec<Subscription>,
}

pub struct ApiKeysPage {
    state: Entity<AppState>,
    view: Loadable<ApiKeysView>,
    form: Option<Form>,
    /// The Remove waiting for its second click: `(provider, id)`.
    armed: Option<(String, String)>,
    /// Providers with no key, shown on request.
    show_all: bool,
    saving: bool,
    error: Option<String>,
    task: Option<Task<()>>,
    scroll: gpui::ScrollHandle,
    _state: Subscription,
}

impl ApiKeysPage {
    pub fn new(state: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        // Settings can open before the engine connects: load once it does.
        let state_sub = cx.observe(&state, |this: &mut Self, _, cx| {
            if matches!(this.view, Loadable::Idle) {
                this.call(Request::List, cx);
            }
        });
        let mut page = Self {
            state,
            view: Loadable::Idle,
            form: None,
            armed: None,
            show_all: false,
            saving: false,
            error: None,
            task: None,
            scroll: gpui::ScrollHandle::new(),
            _state: state_sub,
        };
        page.call(Request::List, cx);
        page
    }

    /// Refetch, e.g. when the page is shown again (the router may have
    /// marked a key exhausted meanwhile).
    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        if !self.saving {
            self.call(Request::List, cx);
        }
    }

    fn call(&mut self, request: Request, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let (method, params) = match &request {
            Request::List => (methods::LIST_API_KEYS, serde_json::json!({})),
            Request::Add {
                provider,
                label,
                secret,
            } => (
                methods::ADD_API_KEY,
                serde_json::json!({ "provider": provider, "label": label, "secret": secret }),
            ),
            Request::Update {
                provider,
                id,
                label,
                secret,
                disabled,
            } => (
                methods::UPDATE_API_KEY,
                serde_json::json!({
                    "provider": provider, "id": id, "label": label,
                    "secret": secret, "disabled": disabled,
                }),
            ),
            Request::Activate { provider, id } => (
                methods::SET_ACTIVE_API_KEY,
                serde_json::json!({ "provider": provider, "id": id }),
            ),
            Request::Remove { provider, id } => (
                methods::REMOVE_API_KEY,
                serde_json::json!({ "provider": provider, "id": id }),
            ),
        };
        if matches!(self.view, Loadable::Idle) {
            self.view = Loadable::Loading;
        }
        let closes_form = matches!(request, Request::Add { .. } | Request::Update { .. });
        self.saving = request != Request::List;
        self.task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(method, params)
                .await
                .map_err(|e| e.to_string())
                .and_then(|v| serde_json::from_value::<ApiKeysView>(v).map_err(|e| e.to_string()));
            this.update(cx, |page, cx| {
                page.saving = false;
                match result {
                    Ok(view) => {
                        page.view = Loadable::Ready(view);
                        page.error = None;
                        if closes_form {
                            page.form = None;
                        }
                    }
                    Err(error) if matches!(page.view, Loadable::Ready(_)) => {
                        page.error = Some(error);
                    }
                    Err(error) => page.view = Loadable::Error(error),
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    fn open_form(&mut self, provider: &str, key: Option<&KeyView>, cx: &mut Context<Self>) {
        let label = cx.new(|cx| {
            let mut input = ComposerInput::new("Label, e.g. Personal", cx).with_single_line();
            if let Some(key) = key {
                input.set_text(key.label.clone(), cx);
            }
            input
        });
        let secret = cx.new(|cx| {
            ComposerInput::new(
                if key.is_some() {
                    "New key (leave empty to keep it)"
                } else {
                    "Paste the API key"
                },
                cx,
            )
            .with_single_line()
            .with_masked()
        });
        let submit = |this: &mut Self,
                      _: Entity<ComposerInput>,
                      event: &ComposerInputEvent,
                      cx: &mut Context<Self>| {
            if matches!(event, ComposerInputEvent::Submitted) {
                this.submit(cx);
            }
        };
        let subs = vec![cx.subscribe(&label, submit), cx.subscribe(&secret, submit)];
        self.form = Some(Form {
            provider: provider.to_owned(),
            key: key.map(|k| k.id.clone()),
            label,
            secret,
            _subs: subs,
        });
        self.armed = None;
        cx.notify();
    }

    fn submit(&mut self, cx: &mut Context<Self>) {
        let Some(form) = &self.form else {
            return;
        };
        if self.saving {
            return;
        }
        let label = form.label.read(cx).text().trim().to_owned();
        let secret = form.secret.read(cx).text().trim().to_owned();
        let request = match &form.key {
            None if secret.is_empty() => {
                self.error = Some("Paste the key first.".into());
                cx.notify();
                return;
            }
            None => Request::Add {
                provider: form.provider.clone(),
                label,
                secret,
            },
            Some(id) => Request::Update {
                provider: form.provider.clone(),
                id: id.clone(),
                label: (!label.is_empty()).then_some(label),
                secret: (!secret.is_empty()).then_some(secret),
                disabled: None,
            },
        };
        self.call(request, cx);
    }
}

/// "Exhausted · resets in 3 days", "Cooling down", "Revoked".
fn health_text(key: &KeyView) -> Option<(String, bool)> {
    let health = key.health.as_ref()?;
    let until = health
        .until
        .and_then(|ms| chrono::DateTime::from_timestamp_millis(ms as i64))
        .filter(|t| *t > chrono::Utc::now())
        .map(|t| crate::pull_requests::relative_time(t, chrono::Utc::now()));
    let (text, bad) = match health.status.as_str() {
        "exhausted" => ("Out of quota", true),
        "cooldown" => ("Cooling down", false),
        "dead" => ("Rejected", true),
        "ok" => return Some(("Working".into(), false)),
        _ => return None,
    };
    Some(match (until, &health.reset_hint) {
        (Some(when), _) => (format!("{text} · back {when}"), bad),
        (None, Some(hint)) => (format!("{text} · {hint}"), bad),
        _ => (text.to_owned(), bad),
    })
}

impl Render for ApiKeysPage {
    fn render(&mut self, _: &mut gpui::Window, cx: &mut Context<Self>) -> impl IntoElement {
        crate::toast::take_error(&mut self.error, cx);
        let theme = Theme::of(cx).for_settings_surface();
        let mut column = widgets::page_column()
            .child(widgets::page_header(&theme, "API keys", None))
            .child(widgets::page_subtitle(
                &theme,
                "Every provider key Zeron and its Pi use, in one place: Pi models, \
                 Pi extensions, MCP servers such as video inspection, dictation and \
                 voice mode. Add several keys to a provider and pick the active one; \
                 a change applies to the next request, with no /reload or restart.",
            ));
        match &self.view {
            Loadable::Idle | Loadable::Loading => {
                column = column.child(
                    div()
                        .mt(px(48.0))
                        .text_center()
                        .text_size(crate::typography::ui_rems(13.0))
                        .text_color(theme.text_muted)
                        .child("Loading keys…"),
                );
            }
            Loadable::Error(error) => {
                column = column.child(
                    widgets::error_strip(&theme, format!("Keys unavailable: {error}")).mt(px(24.0)),
                );
            }
            Loadable::Ready(view) => {
                let view = view.clone();
                if let Some(error) = &view.error {
                    column = column.child(widgets::error_strip(&theme, error.clone()).mt(px(16.0)));
                }
                let (with_keys, without): (Vec<&ProviderView>, Vec<&ProviderView>) = view
                    .providers
                    .iter()
                    .partition(|p| !p.keys.is_empty() || p.pi_login.is_some() || p.oauth);
                for provider in &with_keys {
                    column = column.child(self.render_provider(provider, &theme, cx));
                }
                let form_on_hidden = self
                    .form
                    .as_ref()
                    .is_some_and(|f| without.iter().any(|p| p.name == f.provider));
                if self.show_all || form_on_hidden || with_keys.is_empty() {
                    for provider in &without {
                        column = column.child(self.render_provider(provider, &theme, cx));
                    }
                } else if !without.is_empty() {
                    let names: Vec<&str> = without.iter().map(|p| p.label.as_str()).collect();
                    column = column.child(
                        widgets::text_action(
                            &theme,
                            widgets::ActionTone::Outlined,
                            "Add a provider's key",
                        )
                        .id("api-keys-show-all")
                        .debug_selector(|| "api-keys-show-all".into())
                        .mt(px(16.0))
                        .tooltip(crate::settings::widgets::text_tooltip(names.join(", ")))
                        .on_click(cx.listener(|page, _, _, cx| {
                            page.show_all = true;
                            cx.notify();
                        })),
                    );
                }
                column = column.child(
                    div()
                        .mt(px(16.0))
                        .px(px(8.0))
                        .text_size(crate::typography::ui_rems(11.0))
                        .line_height(crate::typography::ui_rems(15.0))
                        .text_color(theme.text_faint)
                        .child(format!(
                            "Kept on this device in {} (readable by you only). Pi models use the \
                             active key through Pi's auth.json; a provider you signed in to with \
                             OAuth keeps that login. MCP servers configured with a fixed key in \
                             mcp.json pick a change up when they reconnect (/mcp reconnect).",
                            view.path
                        )),
                );
            }
        }
        div()
            .id("api-keys-page")
            .size_full()
            .overflow_y_scroll()
            .track_scroll(&self.scroll)
            .child(column.pb(px(48.0)))
    }
}

impl ApiKeysPage {
    fn render_provider(
        &self,
        provider: &ProviderView,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let name = provider.name.clone();
        let mut notes: Vec<String> = Vec::new();
        if provider.oauth {
            notes.push(
                "Pi is signed in to this provider with OAuth, so Pi models use that login.".into(),
            );
        }
        if let Some(login) = &provider.pi_login {
            notes.push(format!(
                "Pi also has a key of its own here ({login}); adding one here replaces it."
            ));
        }
        let mut card = widgets::section_card(theme).mt_0();
        let mut first = true;
        for key in &provider.keys {
            card = card.child(self.render_key(provider, key, first, theme, cx));
            first = false;
        }
        let editing_here = self.form.as_ref().filter(|f| f.provider == name);
        if let Some(form) = editing_here.filter(|f| f.key.is_none()) {
            card = card.child(self.render_form(form, first, theme, cx));
        } else {
            let selector = format!("api-keys-add-{name}");
            let provider_name = name.clone();
            card = card.child(
                widgets::card_row(theme, first)
                    .child(
                        div()
                            .flex_1()
                            .text_size(crate::typography::ui_rems(12.5))
                            .text_color(theme.text_muted)
                            .child(if provider.keys.is_empty() {
                                "No key yet"
                            } else {
                                "Another key, e.g. a second account"
                            }),
                    )
                    .child(
                        widgets::text_action(theme, widgets::ActionTone::Outlined, "Add key")
                            .id(SharedString::from(selector.clone()))
                            .debug_selector(move || selector.clone())
                            .on_click(cx.listener(move |page, _, _, cx| {
                                page.open_form(&provider_name, None, cx);
                            })),
                    ),
            );
        }
        let title = if provider.used_by.is_empty() {
            provider.label.clone()
        } else {
            format!("{} · {}", provider.label, provider.used_by)
        };
        let section_id = format!("api-keys-provider-{name}");
        div()
            .debug_selector(move || section_id.clone())
            .mt(px(20.0))
            .child(widgets::section_label(theme, title))
            .children(notes.into_iter().map(|note| {
                div()
                    .px(px(8.0))
                    .pb(px(6.0))
                    .text_size(crate::typography::ui_rems(11.5))
                    .text_color(theme.text_muted)
                    .child(note)
            }))
            .child(card)
            .into_any_element()
    }

    fn render_key(
        &self,
        provider: &ProviderView,
        key: &KeyView,
        first: bool,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        if let Some(form) = self
            .form
            .as_ref()
            .filter(|f| f.provider == provider.name && f.key.as_deref() == Some(&key.id))
        {
            return self.render_form(form, first, theme, cx).into_any_element();
        }
        let id = format!("api-keys-key-{}-{}", provider.name, key.id);
        let mut meta: Vec<AnyElement> = vec![
            div()
                .font_family(theme.font_mono.clone())
                .child(key.masked.clone())
                .into_any_element(),
        ];
        if key.disabled {
            meta.push(div().child("Off").into_any_element());
        }
        if provider.router_current.as_deref() == Some(key.id.as_str()) {
            meta.push(div().child("In use now (rotated)").into_any_element());
        }
        if let Some((text, bad)) = health_text(key) {
            meta.push(
                div()
                    .when(bad, |el| el.text_color(theme.danger))
                    .child(text)
                    .into_any_element(),
            );
        }
        let action = |label: &'static str, suffix: &'static str| {
            let selector = format!("{id}-{suffix}");
            widgets::text_action(theme, widgets::ActionTone::Quiet, label)
                .id(SharedString::from(selector.clone()))
                .debug_selector(move || selector.clone())
        };
        let (p, k) = (provider.name.clone(), key.id.clone());
        let armed = self.armed.as_ref() == Some(&(p.clone(), k.clone()));
        let mut actions: Vec<AnyElement> = Vec::new();
        if key.active {
            actions.push(widgets::badge_active(theme, "Active").into_any_element());
        } else {
            let (p, k) = (p.clone(), k.clone());
            actions.push(
                action("Use", "use")
                    .on_click(cx.listener(move |page, _, _, cx| {
                        page.call(
                            Request::Activate {
                                provider: p.clone(),
                                id: k.clone(),
                            },
                            cx,
                        );
                    }))
                    .into_any_element(),
            );
        }
        {
            let (p, k, off) = (p.clone(), k.clone(), !key.disabled);
            actions.push(
                action(if key.disabled { "Turn on" } else { "Turn off" }, "toggle")
                    .on_click(cx.listener(move |page, _, _, cx| {
                        page.call(
                            Request::Update {
                                provider: p.clone(),
                                id: k.clone(),
                                label: None,
                                secret: None,
                                disabled: Some(off),
                            },
                            cx,
                        );
                    }))
                    .into_any_element(),
            );
        }
        {
            let provider_name = p.clone();
            let key = key.clone();
            actions.push(
                action("Edit", "edit")
                    .on_click(cx.listener(move |page, _, _, cx| {
                        page.open_form(&provider_name, Some(&key), cx);
                    }))
                    .into_any_element(),
            );
        }
        actions.push(
            action(if armed { "Remove?" } else { "Remove" }, "remove")
                .text_color(theme.danger)
                .on_click(cx.listener(move |page, _, _, cx| {
                    let target = (p.clone(), k.clone());
                    if page.armed.as_ref() == Some(&target) {
                        page.armed = None;
                        page.call(
                            Request::Remove {
                                provider: target.0,
                                id: target.1,
                            },
                            cx,
                        );
                    } else {
                        page.armed = Some(target);
                        cx.notify();
                    }
                }))
                .into_any_element(),
        );
        widgets::card_row(theme, first)
            .debug_selector(move || id.clone())
            .when(key.disabled, |row| row.opacity(0.6))
            .child(
                div()
                    .flex_1()
                    .min_w(px(140.0))
                    .child(widgets::row_title(theme, key.label.clone()))
                    .child(widgets::meta_line(theme, meta)),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(4.0))
                    .children(actions),
            )
            .into_any_element()
    }

    fn render_form(
        &self,
        form: &Form,
        first: bool,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        let editing = form.key.is_some();
        let save = widgets::text_action(
            theme,
            widgets::ActionTone::Filled,
            if editing { "Save" } else { "Add" },
        )
        .id("api-keys-save")
        .debug_selector(|| "api-keys-save".into())
        .when(self.saving, |el| el.opacity(0.5))
        .on_click(cx.listener(|page, _, _, cx| page.submit(cx)));
        let cancel = widgets::text_action(theme, widgets::ActionTone::Quiet, "Cancel")
            .id("api-keys-cancel")
            .on_click(cx.listener(|page, _, _, cx| {
                page.form = None;
                cx.notify();
            }));
        widgets::card_row(theme, first)
            .debug_selector(|| "api-keys-form".into())
            .flex_col()
            .items_start()
            .gap(px(8.0))
            .child(
                popover::dialog_field(form.label.clone().into_any_element())
                    .w_full()
                    .text_size(crate::typography::ui_rems(13.0)),
            )
            .child(
                popover::dialog_field(form.secret.clone().into_any_element())
                    .w_full()
                    .font_family(theme.font_mono.clone())
                    .text_size(crate::typography::ui_rems(13.0)),
            )
            .child(
                div()
                    .w_full()
                    .flex()
                    .flex_row()
                    .justify_end()
                    .gap(px(6.0))
                    .child(cancel)
                    .child(save),
            )
    }
}
