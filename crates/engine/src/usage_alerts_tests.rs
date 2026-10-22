use super::*;
use chrono::TimeZone;
use zeron_proto::AgentUsageWindow;

fn now() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, 18, 0, 0, 0).unwrap()
}

fn account(provider: &str, windows: &[(&str, f32, Option<i64>)]) -> AgentAccount {
    let mut a: AgentAccount = serde_json::from_value(serde_json::json!({
        "id": format!("slot-{provider}"),
        "harness": "pi",
        "email": null,
        "planLabel": "Max",
        "active": true,
        "switchable": true,
        "provider": provider,
        "authKind": "oauth",
    }))
    .unwrap();
    a.usage_fetched_at = Some((now() - chrono::Duration::minutes(3)).timestamp_millis());
    a.usage_windows = windows
        .iter()
        .map(|(label, used, reset_in_min)| AgentUsageWindow {
            label: (*label).into(),
            used_fraction: *used,
            resets_at: reset_in_min.map(|m| now() + chrono::Duration::minutes(m)),
        })
        .collect();
    a
}

fn snap(accounts: Vec<AgentAccount>) -> AgentAccountsSnapshot {
    AgentAccountsSnapshot {
        accounts,
        warnings: Vec::new(),
    }
}

// ── threshold

#[test]
fn threshold_env_defaults_disables_and_clamps() {
    assert_eq!(threshold_from_env(None), Some(10.0));
    assert_eq!(threshold_from_env(Some("")), Some(10.0));
    assert_eq!(threshold_from_env(Some("25")), Some(25.0));
    assert_eq!(threshold_from_env(Some("15%")), Some(15.0));
    assert_eq!(threshold_from_env(Some("0")), None);
    assert_eq!(threshold_from_env(Some("-3")), None);
    assert_eq!(threshold_from_env(Some("400")), Some(100.0));
    assert_eq!(threshold_from_env(Some("lots")), Some(10.0));
}

#[test]
fn boundary_is_strict_below() {
    // 90% used = exactly 10% left: no alert. 90.01% used: alert.
    let at = |used: f32| {
        evaluate(
            &snap(vec![account("anthropic", &[("5h", used, Some(60))])]),
            now(),
            10.0,
        )
        .alerts
    };
    assert!(at(0.90).is_empty(), "exactly the threshold does not alert");
    assert!(at(0.899).is_empty());
    assert_eq!(at(0.9001).len(), 1);
    assert_eq!(at(1.0)[0].remaining_pct, 0.0);
    assert!(below(9.99, 10.0));
    assert!(!below(10.0, 10.0));
    assert!(!below(10.0004, 10.0));
}

#[test]
fn either_window_alerts_with_its_own_key() {
    let eval = evaluate(
        &snap(vec![account(
            "anthropic",
            &[("session", 0.2, Some(60)), ("week", 0.95, Some(3000))],
        )]),
        now(),
        10.0,
    );
    assert_eq!(eval.alerts.len(), 1);
    let alert = &eval.alerts[0];
    assert_eq!(alert.window, "wk");
    assert!((alert.remaining_pct - 5.0).abs() < 0.01);
    assert!(alert.key.starts_with("pi:anthropic:slot-anthropic:wk:"));
    assert_eq!(eval.coverage[0].status, "low");

    let five = evaluate(
        &snap(vec![account(
            "openai-codex",
            &[("5h", 0.97, Some(30)), ("wk", 0.1, Some(900))],
        )]),
        now(),
        10.0,
    );
    assert_eq!(five.alerts[0].window, "5h");
    assert_eq!(five.alerts[0].provider.as_deref(), Some("openai-codex"));
}

#[test]
fn configurable_threshold() {
    let s = snap(vec![account("anthropic", &[("5h", 0.8, Some(60))])]);
    assert!(evaluate(&s, now(), 10.0).alerts.is_empty());
    assert_eq!(evaluate(&s, now(), 25.0).alerts.len(), 1);
}

// ── unknown, reset, stale, scope

#[test]
fn unknown_never_alerts() {
    let mut none = account("anthropic", &[]);
    none.usage_error = Some("Rate limited".into());
    // An API key with no quota view: nothing to read, nothing to alert.
    let mut key: AgentAccount = account("openrouter", &[]);
    key.auth_kind = Some(AgentAuthKind::ApiKey);
    key.usage_error = Some("API keys have no plan usage".into());
    let mut inactive = account("github-copilot", &[("month", 1.0, Some(60))]);
    inactive.active = false;
    let eval = evaluate(&snap(vec![none, key, inactive]), now(), 10.0);
    assert!(eval.alerts.is_empty());
    assert_eq!(
        eval.coverage.len(),
        1,
        "an API key with no windows and an inactive slot are not coverage"
    );
    assert_eq!(eval.coverage[0].status, "unknown");
    assert_eq!(eval.coverage[0].reason.as_deref(), Some("Rate limited"));
}

/// A key that reports windows (OpenCode Go) is a subscription: it is coverage,
/// it can alert, and the alert names its provider so only that provider's
/// sessions are affected.
#[test]
fn an_opencode_go_key_with_windows_is_coverage_and_can_alert() {
    let mut go = account("opencode-go", &[("5h", 0.96, Some(45))]);
    go.auth_kind = Some(AgentAuthKind::ApiKey);
    go.plan_label = Some("OpenCode Go".into());
    let eval = evaluate(&snap(vec![go]), now(), 10.0);
    assert_eq!(eval.coverage.len(), 1, "a quota-bearing key is coverage");
    assert_eq!(eval.coverage[0].status, "low");
    assert_eq!(eval.coverage[0].provider.as_deref(), Some("opencode-go"));
    assert_eq!(eval.alerts.len(), 1);
    let alert = &eval.alerts[0];
    assert_eq!(alert.provider.as_deref(), Some("opencode-go"));
    assert_eq!(alert.window, "5h");
    assert_eq!(alert.plan.as_deref(), Some("OpenCode Go"));
    assert!((alert.remaining_pct - 4.0).abs() < 0.01);
    assert!(alert.key.starts_with("pi:opencode-go:slot-opencode-go:5h:"));
    // Only sessions on that provider are affected; another one stays usable.
    assert!(affects(
        alert,
        HarnessId::Pi,
        Some("opencode-go/deepseek-v4.1-flash"),
        None
    ));
    assert!(!affects(
        alert,
        HarnessId::Pi,
        Some("anthropic/claude-opus-5-5"),
        None
    ));
}

#[test]
fn an_api_key_without_windows_stays_unknown_and_never_alerts() {
    let mut key: AgentAccount = account("openrouter", &[]);
    key.auth_kind = Some(AgentAuthKind::ApiKey);
    let eval = evaluate(&snap(vec![key]), now(), 10.0);
    assert!(eval.alerts.is_empty(), "no windows, nothing to alert");
    assert!(
        eval.coverage.is_empty(),
        "a key with no quota view is not a subscription to list as coverage"
    );
}

#[test]
fn an_inactive_opencode_go_key_never_alerts() {
    let mut go = account("opencode-go", &[("5h", 0.99, Some(45))]);
    go.auth_kind = Some(AgentAuthKind::ApiKey);
    go.active = false;
    let eval = evaluate(&snap(vec![go]), now(), 10.0);
    assert!(
        eval.alerts.is_empty(),
        "a saved, unused key dispatches nothing"
    );
    assert!(eval.coverage.is_empty());
}

#[test]
fn a_past_due_opencode_go_window_does_not_alert() {
    let mut go = account(
        "opencode-go",
        &[("5h", 0.99, Some(-5)), ("mo", 0.3, Some(9000))],
    );
    go.auth_kind = Some(AgentAuthKind::ApiKey);
    let eval = evaluate(&snap(vec![go]), now(), 10.0);
    assert!(eval.alerts.is_empty(), "the old window's numbers are over");
    assert_eq!(eval.coverage.len(), 1);
    assert_eq!(eval.coverage[0].status, "ok");
}

#[test]
fn stale_or_never_fetched_snapshot_is_unknown() {
    let mut stale = account("anthropic", &[("5h", 1.0, Some(60))]);
    stale.usage_fetched_at = Some((now() - chrono::Duration::hours(7)).timestamp_millis());
    let mut never = account("openai-codex", &[("5h", 1.0, Some(60))]);
    never.usage_fetched_at = None;
    let eval = evaluate(&snap(vec![stale, never]), now(), 10.0);
    assert!(eval.alerts.is_empty(), "an old snapshot cannot block work");
    assert!(eval.coverage.iter().all(|c| c.status == "unknown"));
    // A failed probe that still serves recent last-good numbers still counts.
    let mut recent = account("anthropic", &[("5h", 0.95, Some(60))]);
    recent.usage_error = Some("Rate limited — retrying in 2m".into());
    assert_eq!(evaluate(&snap(vec![recent]), now(), 10.0).alerts.len(), 1);
}

#[test]
fn expired_window_counts_as_reset() {
    let s = snap(vec![account(
        "anthropic",
        &[("5h", 0.99, Some(-1)), ("wk", 0.5, Some(600))],
    )]);
    let eval = evaluate(&s, now(), 10.0);
    assert!(eval.alerts.is_empty());
    assert_eq!(eval.coverage[0].status, "ok");
}

#[test]
fn scope_follows_the_models_provider() {
    let alert = &evaluate(
        &snap(vec![account("anthropic", &[("5h", 0.99, Some(60))])]),
        now(),
        10.0,
    )
    .alerts[0];
    assert!(affects(
        alert,
        HarnessId::Pi,
        Some("anthropic/claude-opus-5-5"),
        None
    ));
    assert!(affects(
        alert,
        HarnessId::Pi,
        Some("Anthropic/claude-opus-5-5"),
        None
    ));
    assert!(!affects(
        alert,
        HarnessId::Pi,
        Some("openai-codex/gpt-5.5"),
        None
    ));
    assert!(!affects(
        alert,
        HarnessId::Pi,
        Some("opencode-go/deepseek-v4.1-flash"),
        None
    ));
    assert!(!affects(alert, HarnessId::Pi, Some("default"), None));
    assert!(!affects(alert, HarnessId::Pi, None, None));
    assert!(!affects(
        alert,
        HarnessId::Opencode,
        Some("anthropic/claude-opus-5-5"),
        None
    ));
    // Pi's own default model runs on the provider Pi's settings name.
    assert!(affects(
        alert,
        HarnessId::Pi,
        Some("default"),
        Some("anthropic")
    ));
    assert!(affects(alert, HarnessId::Pi, None, Some("Anthropic")));
    assert!(!affects(
        alert,
        HarnessId::Pi,
        Some("default"),
        Some("google")
    ));
    // A model that names its provider is never read as the default one.
    assert!(!affects(
        alert,
        HarnessId::Pi,
        Some("google/gemini-4-pro"),
        Some("anthropic")
    ));
    assert_eq!(
        model_provider(Some("clinepass/cline-pass/deepseek-v4.1-flash")).as_deref(),
        Some("clinepass")
    );
}

#[test]
fn pi_default_provider_comes_from_pi_settings() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("settings.json");
    assert_eq!(pi_default_provider(&file), None, "no settings, no default");
    let write = |text: &str| std::fs::write(&file, text).unwrap();
    write(r#"{"defaultProvider":"Anthropic","defaultModel":"claude-opus-5-5"}"#);
    assert_eq!(pi_default_provider(&file).as_deref(), Some("anthropic"));
    write(r#"{"defaultModel":"openai-codex/gpt-5.5"}"#);
    assert_eq!(pi_default_provider(&file).as_deref(), Some("openai-codex"));
    write(r#"{"defaultModel":"gpt-5.5"}"#);
    assert_eq!(
        pi_default_provider(&file),
        None,
        "a bare id names no provider"
    );
    write("not json");
    assert_eq!(pi_default_provider(&file), None);
}

// ── dedup

#[test]
fn dedup_until_the_window_resets() {
    let alert = evaluate(
        &snap(vec![account("anthropic", &[("5h", 0.99, Some(60))])]),
        now(),
        10.0,
    )
    .alerts
    .remove(0);
    let mut d = Delivered::default();
    assert!(!d.has("c1", &alert.key));
    d.record("c1", &alert, now());
    assert!(d.has("c1", &alert.key));
    assert!(!d.has("c2", &alert.key), "per chat");
    d.prune(now() + chrono::Duration::minutes(59));
    assert!(
        d.has("c1", &alert.key),
        "kept through flapping and failed probes"
    );
    d.prune(now() + chrono::Duration::minutes(61));
    assert!(!d.has("c1", &alert.key), "re-armed once the window reset");
    // The next period is a new key.
    let next = evaluate(
        &snap(vec![account("anthropic", &[("5h", 0.99, Some(360))])]),
        now(),
        10.0,
    )
    .alerts
    .remove(0);
    assert_ne!(next.key, alert.key);
    // No reset time: kept for the TTL.
    let mut none = alert.clone();
    none.resets_at = None;
    none.key = "k".into();
    d.record("c1", &none, now());
    d.prune(now() + chrono::Duration::days(6));
    assert!(d.has("c1", "k"));
    d.prune(now() + chrono::Duration::days(8));
    assert!(!d.has("c1", "k"));
    // Persisted shape round-trips.
    d.record("c1", &alert, now());
    let text = serde_json::to_string(&d).unwrap();
    assert_eq!(serde_json::from_str::<Delivered>(&text).unwrap(), d);
}

// ── delivery matrix

fn chat(id: &str, parent: Option<&str>, device: &str) -> Chat {
    serde_json::from_value(serde_json::json!({
        "id": id,
        "deviceId": device,
        "title": format!("T {id}"),
        "archived": false,
        "cwd": "/tmp",
        "branch": null,
        "checkoutId": null,
        "config": null,
        "lastMessagePreview": null,
        "lastMessageAt": null,
        "createdAt": "2026-10-17T00:00:00Z",
        "parentChatId": parent,
    }))
    .unwrap()
}

fn view(id: &str, parent: Option<&str>, status: Option<SessionStatus>, model: &str) -> ChatView {
    ChatView {
        chat: chat(id, parent, "dev"),
        status,
        model: Some(model.into()),
        harness: HarnessId::Pi,
    }
}

const OPUS: &str = "anthropic/claude-opus-5-5";
const CODEX: &str = "openai-codex/gpt-5.5";
const FLASH: &str = "opencode-go/deepseek-v4.1-flash";

fn anthropic_low() -> Vec<UsageAlert> {
    evaluate(
        &snap(vec![account("anthropic", &[("5h", 0.95, Some(60))])]),
        now(),
        10.0,
    )
    .alerts
}

fn kinds(plan: &[Delivery]) -> Vec<(String, DeliveryKind, bool)> {
    plan.iter()
        .map(|d| (d.chat_id.clone(), d.kind, d.root))
        .collect()
}

#[test]
fn delivery_matrix_for_every_zeron_surface() {
    use SessionStatus::*;
    let root = "orchestrator-r1";
    let views = vec![
        // main sessions
        view("main-working", None, Some(Working), OPUS),
        view("main-asking", None, Some(AwaitingInput), OPUS),
        view("main-idle", None, Some(Idle), OPUS),
        view("main-errored", None, Some(Errored), OPUS),
        view("main-other-account", None, Some(Working), CODEX),
        // btw pane side chat
        view("btw-1", Some("main-working"), Some(Working), OPUS),
        // orchestrator tree: the root on another provider, members mixed
        view(root, None, Some(Idle), FLASH),
        view("m-opus", Some(root), Some(Working), OPUS),
        view("m-flash", Some(root), Some(Working), FLASH),
        view("m-deep", Some("m-opus"), Some(AwaitingInput), OPUS),
    ];
    let mut remote = view("remote", None, Some(Working), OPUS);
    remote.chat.device_id = "other-device".into();
    let mut archived = view("archived", None, Some(Working), OPUS);
    archived.chat.archived = true;
    let mut all = views.clone();
    all.extend([remote, archived]);

    let plan = plan(
        &all,
        &anthropic_low(),
        &Delivered::default(),
        "dev",
        None,
        &|_| false,
    );
    assert_eq!(
        kinds(&plan),
        vec![
            ("btw-1".into(), DeliveryKind::Steer, false),
            ("m-deep".into(), DeliveryKind::Held, false),
            ("m-opus".into(), DeliveryKind::Steer, false),
            ("main-asking".into(), DeliveryKind::Held, false),
            ("main-working".into(), DeliveryKind::Steer, false),
            // The root runs on another provider, yet is told its tree is hit.
            (root.into(), DeliveryKind::Run, true),
        ]
    );
    let root_note = plan.iter().find(|d| d.root).unwrap();
    assert_eq!(root_note.members.len(), 2);
    assert!(root_note.members.contains(&"T m-opus".to_string()));
    assert!(!root_note.members.contains(&"T m-flash".to_string()));
}

#[test]
fn chat_on_pis_default_model_is_told_through_pi_settings() {
    use SessionStatus::*;
    let mut on_default = view("on-default", None, Some(Working), "default");
    let mut unset = on_default.clone();
    unset.chat.id = "unset".into();
    unset.model = None;
    let views = vec![on_default.clone(), unset];
    let alerts = anthropic_low();
    assert!(
        plan(&views, &alerts, &Delivered::default(), "dev", None, &|_| {
            false
        })
        .is_empty(),
        "an unknown default provider cannot be attributed"
    );
    assert_eq!(
        kinds(&plan(
            &views.clone()[..1],
            &alerts,
            &Delivered::default(),
            "dev",
            Some("anthropic"),
            &|_| false
        )),
        vec![("on-default".into(), DeliveryKind::Steer, false)]
    );
    assert_eq!(
        kinds(&plan(
            &views[1..],
            &alerts,
            &Delivered::default(),
            "dev",
            Some("anthropic"),
            &|_| false
        )),
        vec![("unset".into(), DeliveryKind::Steer, false)]
    );
}

#[test]
fn paused_root_and_dedup_suppress_delivery() {
    use SessionStatus::*;
    let root = "orchestrator-r1";
    let views = vec![
        view(root, None, Some(Idle), FLASH),
        view("m", Some(root), Some(Working), OPUS),
    ];
    let alerts = anthropic_low();
    let paused = plan(&views, &alerts, &Delivered::default(), "dev", None, &|_| {
        true
    });
    assert_eq!(
        kinds(&paused),
        vec![("m".into(), DeliveryKind::Steer, false)],
        "a paused root is not woken"
    );

    let mut d = Delivered::default();
    d.record("m", &alerts[0], now());
    let mut tree = alerts[0].clone();
    tree.key = tree_key(&tree.key);
    d.record(root, &tree, now());
    assert!(
        plan(&views, &alerts, &d, "dev", None, &|_| false).is_empty(),
        "told once per key"
    );

    // A working root on the affected account gets both its own and its tree's note.
    let views = vec![
        view(root, None, Some(Working), OPUS),
        view("m", Some(root), Some(Working), OPUS),
    ];
    let both = plan(&views, &alerts, &Delivered::default(), "dev", None, &|_| {
        false
    });
    assert_eq!(
        kinds(&both),
        vec![
            ("m".into(), DeliveryKind::Steer, false),
            (root.into(), DeliveryKind::Steer, false),
            (root.into(), DeliveryKind::Steer, true),
        ]
    );
}

#[test]
fn no_alerts_no_deliveries_and_idle_is_told_when_it_works() {
    use SessionStatus::*;
    let idle = vec![view("a", None, Some(Idle), OPUS)];
    assert!(plan(&idle, &[], &Delivered::default(), "dev", None, &|_| false).is_empty());
    assert!(plan(
        &idle,
        &anthropic_low(),
        &Delivered::default(),
        "dev",
        None,
        &|_| false
    )
    .is_empty());
    let working = vec![view("a", None, Some(Working), OPUS)];
    assert_eq!(
        plan(
            &working,
            &anthropic_low(),
            &Delivered::default(),
            "dev",
            None,
            &|_| false
        )
        .len(),
        1
    );
}

#[test]
fn notes_say_finish_safely_write_handoff_and_end() {
    let alerts = anthropic_low();
    let d = Delivery {
        chat_id: "a".into(),
        kind: DeliveryKind::Steer,
        alerts: alerts.clone(),
        members: vec![],
        root: false,
    };
    let text = render(&d, now());
    assert!(text.starts_with(MARKER));
    assert!(text.contains("Finish the step you are on now"));
    assert!(text.contains("do not abandon a half-done action"));
    assert!(text.contains("start nothing new"));
    assert!(text.contains(".pi/handoffs/usage-anthropic-20261018T0000Z.md"));
    assert!(text.contains("end your turn"));
    assert!(text.contains("5h window 5.0% left"));
    let root = render(
        &Delivery {
            root: true,
            members: vec!["T m".into()],
            ..d
        },
        now(),
    );
    assert!(root.contains("Do not start or send new work to sessions on that account"));
    assert!(root.contains("unaffected accounts stay usable"));
}

#[test]
fn snapshot_file_has_no_secrets_and_names_unsupported_logins() {
    let dir = tempfile::tempdir().unwrap();
    let auth = dir.path().join("auth.json");
    std::fs::write(
        &auth,
        r#"{"anthropic":{"type":"oauth","access":"SECRET-A"},"openai":{"type":"oauth","access":"SECRET-B"},
            "clinepass":{"type":"oauth","access":"SECRET-C"},"openrouter":{"type":"api_key","key":"SECRET-D"}}"#,
    )
    .unwrap();
    let mut unsupported = unsupported_pi_oauth(&auth);
    unsupported.sort();
    assert_eq!(
        unsupported,
        vec!["pi:openai"],
        "every provider the engine probes is supported, Cline included"
    );
    let eval = evaluate(
        &snap(vec![account("anthropic", &[("5h", 0.95, Some(60))])]),
        now(),
        10.0,
    );
    let file = alerts_file(&eval, now(), Some(10.0), unsupported);
    let text = serde_json::to_string(&file).unwrap();
    assert!(!text.contains("SECRET"));
    assert!(text.contains("\"validForSecs\":900"));
    let path = dir.path().join(FILE_NAME);
    write_json(&path, &file).unwrap();
    let back: AlertsFile = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    assert_eq!(back, file);
    assert_eq!(
        alerted_providers(&back.alerts),
        HashSet::from(["anthropic".to_string()])
    );
}
