use super::{Process, normalize::string};
use crate::HarnessError;
use serde_json::{Value, json};
use zeron_proto::{
    ArgumentCompletion, Model, ModelOption, ModelOptionChoice, ReasoningLevel, SlashCommand,
};

pub(super) fn levels(data: &Value) -> Vec<ReasoningLevel> {
    data["levels"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|v| serde_json::from_value(v.clone()).ok())
        .collect()
}
/// The probe-only extension (`catalog-refresh.mjs`) that refreshes every
/// provider's catalog before the probe reads it.
const REFRESH_SOURCE: &str = include_str!("catalog-refresh.mjs");
const REFRESH_COMMAND: &str = "pi_ui_refresh_models";
/// Above the extension's own 20 s, so its timeout answers first.
const REFRESH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(25);
/// How long one model's thinking-level probe may take. Pi answers these
/// locally; a stuck one must not spend the whole discovery budget and take
/// every other model down with it.
const MODEL_PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
/// Installed beside the delete guard, content-addressed the same way.
pub(super) fn install_refresh(dir: &std::path::Path) -> std::io::Result<std::path::PathBuf> {
    super::guard::install_file(dir, "zeron-catalog-refresh", REFRESH_SOURCE)
}
pub(super) fn refresh_prompt(force: bool) -> Value {
    let args = if force { " force" } else { "" };
    json!({"type":"prompt","message":format!("/{REFRESH_COMMAND}{args}")})
}
const PROVIDERS_PREFIX: &str = "ZERON-PROVIDERS ";
/// Provider id → Pi's display name, from the refresh's notify.
type ProviderNames = std::collections::HashMap<String, String>;
/// The provider names a notify carries, when it is the refresh's answer. A
/// "name" that is only the id again leaves the provider to Zeron's naming.
fn providers_notice(frame: &Value) -> Option<ProviderNames> {
    if frame["type"] != "extension_ui_request" || frame["method"] != "notify" {
        return None;
    }
    let payload = frame["message"].as_str()?.strip_prefix(PROVIDERS_PREFIX)?;
    let answer: Value = serde_json::from_str(payload).ok()?;
    Some(
        answer["names"]
            .as_object()?
            .iter()
            .filter_map(|(id, name)| Some((id.clone(), name.as_str()?.trim().to_owned())))
            .filter(|(id, name)| !name.is_empty() && name != id)
            .collect(),
    )
}
/// Run the refresh when this Pi lists the command: an unknown `/command`
/// would go to the model as a prompt. A failed or slow refresh leaves the
/// stored catalog, which the probe then reads as before. Answers Pi's name
/// for each available model's provider, empty when the refresh didn't run.
async fn refresh(process: &mut Process, force: bool, backlog: &mut Vec<Value>) -> ProviderNames {
    let commands = match process.query(json!({"type":"get_commands"}), backlog).await {
        Ok(commands) => commands,
        Err(error) => {
            tracing::debug!(%error, "Pi listed no commands; catalog not refreshed");
            return ProviderNames::new();
        }
    };
    let listed = commands["commands"]
        .as_array()
        .into_iter()
        .flatten()
        .any(|c| c["name"] == REFRESH_COMMAND);
    if !listed {
        return ProviderNames::new();
    }
    let asked = tokio::time::timeout(
        REFRESH_TIMEOUT,
        process.query(refresh_prompt(force), backlog),
    )
    .await;
    match asked {
        Ok(Ok(_)) => {}
        Ok(Err(error)) => tracing::debug!(%error, "Pi refused the catalog refresh"),
        Err(_) => tracing::debug!("Pi's catalog refresh timed out"),
    }
    backlog
        .iter()
        .find_map(providers_notice)
        .unwrap_or_default()
}
/// `refresh`: refresh the catalogs first (`Some(force)`), when this Pi
/// loaded the refresh extension.
pub(super) async fn models(
    process: &mut Process,
    refresh_first: Option<bool>,
) -> Result<Vec<Model>, HarnessError> {
    let mut backlog = vec![];
    let mut names = ProviderNames::new();
    if let Some(force) = refresh_first {
        names = refresh(process, force, &mut backlog).await;
        backlog.clear();
    }
    let data = process
        .query(json!({"type":"get_available_models"}), &mut backlog)
        .await?;
    let mut result = vec![];
    for model in data["models"].as_array().into_iter().flatten() {
        let provider = string(model, "provider");
        let id = string(model, "id");
        // Selecting a model is how Pi reports its thinking levels, but one
        // model Pi will not select (a provider that vanished, a wedged
        // process) must not discard the whole catalog: the model stays in
        // the list, just without reasoning levels.
        let supported = match tokio::time::timeout(MODEL_PROBE_TIMEOUT, async {
            process
                .query(
                    json!({"type":"set_model","provider":provider,"modelId":id}),
                    &mut backlog,
                )
                .await?;
            process
                .query(
                    json!({"type":"get_available_thinking_levels"}),
                    &mut backlog,
                )
                .await
        })
        .await
        {
            Ok(Ok(supported)) => Some(supported),
            Ok(Err(error)) => {
                tracing::debug!(%error, provider, id, "Pi would not select a model");
                None
            }
            Err(_) => {
                tracing::debug!(provider, id, "Pi's model probe timed out");
                None
            }
        };
        result.push(Model {
            id: format!("{provider}/{id}"),
            label: model["name"].as_str().unwrap_or(id).into(),
            // The picker groups by provider: Pi's name for it, else Zeron's.
            description: Some(
                names
                    .get(provider)
                    .cloned()
                    .unwrap_or_else(|| zeron_proto::model_provider::display_name(provider)),
            ),
            reasoning_levels: supported.as_ref().map(levels).unwrap_or_default(),
            options: supported.as_ref().map(thinking_option).unwrap_or_default(),
        });
        backlog.clear();
    }
    Ok(result)
}
fn thinking_option(data: &Value) -> Vec<ModelOption> {
    if !data["levels"]
        .as_array()
        .is_some_and(|v| v.iter().any(|s| s == "off"))
    {
        return vec![];
    }
    vec![ModelOption {
        id: "pi_thinking".into(),
        label: "Thinking".into(),
        default_choice: "auto".into(),
        choices: vec![
            ModelOptionChoice {
                id: "auto".into(),
                label: "Use reasoning level".into(),
            },
            ModelOptionChoice {
                id: "off".into(),
                label: "Off".into(),
            },
        ],
    }]
}
/// Extension commands that act only on Pi's terminal screen: they pick
/// through Pi's own `ui.select` and write to its editor, none of which Zeron
/// sees, so offering them would leave the two out of step. Zeron's rewind is
/// its own (`pi_ui_rewind`, from the message's menu).
const TERMINAL_ONLY: [&str; 2] = ["claude-refusal-rewind", "rewind"];
/// The user extension `00-zeron-completions.ts`: run as a prompt, it answers
/// with one notify carrying every command's argument completions, which
/// Pi's RPC `get_commands` leaves out.
pub(super) const COMPLETIONS_COMMAND: &str = "pi_ui_completions";
const COMPLETIONS_PREFIX: &str = "ZERON-COMPLETIONS ";
/// Where Zeron keeps that answer beside Pi's `get_commands` data, keyed by
/// command name.
pub(super) const COMPLETIONS_KEY: &str = "zeronCompletions";
/// The prompt that asks for the completions.
pub(super) fn completions_prompt() -> Value {
    json!({"type":"prompt","message":format!("/{COMPLETIONS_COMMAND}")})
}
/// The completions an extension notify carries, when it is that answer.
pub(super) fn completions_notice(frame: &Value) -> Option<Value> {
    if frame["type"] != "extension_ui_request" || frame["method"] != "notify" {
        return None;
    }
    let payload = frame["message"]
        .as_str()?
        .strip_prefix(COMPLETIONS_PREFIX)?;
    let answer: Value = serde_json::from_str(payload).ok()?;
    Some(answer["commands"].clone())
}
const PACKAGE_UPDATES_PREFIX: &str = "ZERON-PACKAGE-UPDATES ";
/// The guard's answer to a package update check (`ZERON_PACKAGE_CHECK`),
/// when the notify is that answer: the toast to show, if any package has one.
pub(super) fn package_updates_notice(frame: &Value) -> Option<Option<String>> {
    if frame["type"] != "extension_ui_request" || frame["method"] != "notify" {
        return None;
    }
    let payload = frame["message"]
        .as_str()?
        .strip_prefix(PACKAGE_UPDATES_PREFIX)?;
    let answer: Value = serde_json::from_str(payload).unwrap_or(Value::Null);
    let names: Vec<&str> = answer["packages"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|package| package["name"].as_str())
        .collect();
    Some(package_updates_text(&names))
}
/// Worded like the app's own update toast.
fn package_updates_text(names: &[&str]) -> Option<String> {
    const SHOWN: usize = 4;
    let first = names.first()?;
    let hint = "run `pi update --extensions`";
    if names.len() == 1 {
        return Some(format!("Pi package update available — {first} · {hint}"));
    }
    let mut list = names[..names.len().min(SHOWN)].join(", ");
    if names.len() > SHOWN {
        list.push_str(&format!(" and {} more", names.len() - SHOWN));
    }
    Some(format!(
        "{} Pi package updates available — {list} · {hint}",
        names.len()
    ))
}
/// Ask a Pi that lists the completions command for them; `Null` when it
/// does not, or it fails to answer. Other frames go to `backlog`.
pub(super) async fn completions(
    process: &mut Process,
    commands: &Value,
    backlog: &mut Vec<Value>,
) -> Value {
    let listed = commands["commands"]
        .as_array()
        .into_iter()
        .flatten()
        .any(|c| c["name"] == COMPLETIONS_COMMAND);
    if !listed {
        return Value::Null;
    }
    let mut frames = vec![];
    let asked = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        process.query(completions_prompt(), &mut frames),
    )
    .await;
    let mut found = Value::Null;
    for frame in frames {
        match completions_notice(&frame) {
            Some(completions) => found = completions,
            None => backlog.push(frame),
        }
    }
    match asked {
        Ok(Ok(_)) => {}
        Ok(Err(error)) => tracing::debug!(%error, "Pi refused the completions command"),
        Err(_) => tracing::debug!("Pi's completions command timed out"),
    }
    found
}
fn arguments(data: &Value) -> Vec<ArgumentCompletion> {
    data.as_array()
        .into_iter()
        .flatten()
        .filter_map(|v| {
            let value = v["value"].as_str().filter(|s| !s.trim().is_empty())?;
            Some(ArgumentCompletion {
                value: value.into(),
                label: v["label"]
                    .as_str()
                    .filter(|s| !s.is_empty())
                    .unwrap_or(value.trim())
                    .into(),
                description: v["description"]
                    .as_str()
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned),
                children: arguments(&v["children"]),
            })
        })
        .collect()
}
pub(super) fn commands(data: &Value) -> Vec<SlashCommand> {
    let mut commands: Vec<_> = data["commands"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|v| {
            let name = v["name"].as_str()?;
            // Transport commands for GUI clients (PIUI's `pi_ui_event`), not
            // something a person types, and commands that only redraw the
            // terminal (pi-claude-auth's rewind after a refusal).
            if name.starts_with("pi_ui_") || TERMINAL_ONLY.contains(&name) {
                return None;
            }
            Some(SlashCommand {
                name: name.into(),
                description: string(v, "description").into(),
                input_hint: None,
                arguments: arguments(&data[COMPLETIONS_KEY][name]),
                // An extension's command works the extension, beside the
                // conversation; templates and skills are prompts.
                side: v["source"] == "extension",
            })
        })
        .collect();
    for (name, description) in [
        ("compact", "Compact context"),
        ("session", "Session statistics"),
        ("name", "Set session name"),
        ("export", "Export session to HTML"),
        ("autocompact", "Toggle automatic compaction"),
        ("steering", "Steering queue mode"),
        ("follow-up", "Follow-up queue mode"),
        // Zeron's guard extension runs it; listed before Pi's first answer.
        (
            "reload",
            "Reload keybindings, extensions, skills, prompts, themes, and context files",
        ),
    ] {
        if !commands.iter().any(|c| c.name == name) {
            commands.push(SlashCommand {
                name: name.into(),
                description: description.into(),
                input_hint: None,
                arguments: vec![],
                side: false,
            });
        }
    }
    commands
}
pub(super) fn builtin(text: &str, auto: bool) -> Option<Value> {
    let text = text.trim();
    let (name, args) = text.split_once(char::is_whitespace).unwrap_or((text, ""));
    let args = args.trim();
    Some(match name {
        "/compact" => json!({"type":"compact","customInstructions":args}),
        "/session" => json!({"type":"get_session_stats"}),
        "/name" => json!({"type":"set_session_name","name":args}),
        "/export" => {
            if args.is_empty() {
                json!({"type":"export_html"})
            } else {
                json!({"type":"export_html","outputPath":args})
            }
        }
        "/autocompact" => {
            json!({"type":"set_auto_compaction","enabled":match args {"on"=>true,"off"=>false,_=>!auto}})
        }
        // Pi persists the mode to its global settings without validating it,
        // so only its two real modes are ever sent; anything else shows the
        // current state instead.
        "/steering" if is_queue_mode(args) => json!({"type":"set_steering_mode","mode":args}),
        "/follow-up" if is_queue_mode(args) => json!({"type":"set_follow_up_mode","mode":args}),
        "/steering" | "/follow-up" => json!({"type":"get_state"}),
        _ => return None,
    })
}
fn is_queue_mode(mode: &str) -> bool {
    matches!(mode, "all" | "one-at-a-time")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_refresh_names_each_provider_the_way_pi_does() {
        assert!(REFRESH_SOURCE.contains(&format!("\"{PROVIDERS_PREFIX}\"")));
        let notify = |message: &str| json!({"type":"extension_ui_request","method":"notify","message":message});
        let names = providers_notice(&notify(
            r#"ZERON-PROVIDERS {"names":{"opencode-go":"OpenCode Go","acme":" ","x":1,"y":"y"}}"#,
        ))
        .unwrap();
        assert_eq!(names.len(), 1);
        assert_eq!(names["opencode-go"], "OpenCode Go");
        assert!(providers_notice(&notify("ZERON-COMPLETIONS {}")).is_none());
        assert!(
            providers_notice(&json!({"type":"extension_ui_request","method":"setStatus"}))
                .is_none()
        );
    }

    #[test]
    fn the_catalog_refresh_is_a_hidden_extension_command() {
        assert_eq!(
            refresh_prompt(true),
            json!({"type":"prompt","message":"/pi_ui_refresh_models force"})
        );
        assert_eq!(
            refresh_prompt(false),
            json!({"type":"prompt","message":"/pi_ui_refresh_models"})
        );
        assert!(REFRESH_SOURCE.contains(&format!("\"{REFRESH_COMMAND}\"")));
        let listed = commands(&json!({"commands":[
            {"name":REFRESH_COMMAND,"description":"Refresh","source":"extension"},
        ]}));
        assert!(
            listed.iter().all(|c| c.name != REFRESH_COMMAND),
            "never in the palette"
        );
        let dir = tempfile::tempdir().unwrap();
        let path = install_refresh(dir.path()).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), REFRESH_SOURCE);
        assert_eq!(
            install_refresh(dir.path()).unwrap(),
            path,
            "content-addressed"
        );
    }

    #[test]
    fn piui_transport_commands_are_hidden_from_the_palette() {
        let listed = commands(&json!({"commands":[
            {"name":"pi_ui_event","description":"Internal","source":"extension"},
            {"name":"claude-refusal-rewind","description":"Redraw","source":"extension"},
            {"name":"rewind","description":"Rewind","source":"extension"},
            {"name":"todo","description":"Todos","source":"extension"},
        ]}));
        assert!(listed.iter().all(|c| !c.name.starts_with("pi_ui_")));
        assert!(listed.iter().all(|c| c.name != "claude-refusal-rewind"));
        assert!(listed.iter().all(|c| c.name != "rewind"), "terminal only");
        assert!(listed.iter().any(|c| c.name == "todo"));
    }

    #[test]
    fn package_update_answers_become_one_toast() {
        let notice = |message: &str| {
            package_updates_notice(&json!({"type":"extension_ui_request",
                "method":"notify","message":message}))
        };
        assert_eq!(notice("Goal paused"), None, "an ordinary notice");
        assert_eq!(
            notice(r#"ZERON-PACKAGE-UPDATES {"v":1,"packages":[]}"#),
            Some(None)
        );
        assert_eq!(
            notice(r#"ZERON-PACKAGE-UPDATES {"v":1,"packages":[{"name":"pi-lsp"}]}"#),
            Some(Some(
                "Pi package update available — pi-lsp · run `pi update --extensions`".into()
            ))
        );
        let many = r#"ZERON-PACKAGE-UPDATES {"v":1,"packages":[{"name":"a"},{"name":"b"},
            {"name":"c"},{"name":"d"},{"name":"e"}]}"#;
        assert_eq!(
            notice(many),
            Some(Some(
                "5 Pi package updates available — a, b, c, d and 1 more · run `pi update --extensions`"
                    .into()
            ))
        );
    }

    #[test]
    fn reload_is_listed_and_the_guard_command_runs_it() {
        let reload = |listed: Vec<SlashCommand>| listed.into_iter().find(|c| c.name == "reload");
        let before = reload(commands(&json!({"commands":[]}))).unwrap();
        assert!(
            !before.side,
            "sent as a prompt, which the guard's command answers"
        );
        let guarded = reload(commands(&json!({"commands":[
            {"name":"reload","description":"Reload","source":"extension"},
        ]})))
        .unwrap();
        assert!(guarded.side);
        assert_eq!(
            builtin("/reload", true),
            None,
            "no RPC command: Pi's extension runs it"
        );
    }

    #[test]
    fn extension_sub_commands_ride_their_command() {
        let notice = json!({"type":"extension_ui_request","id":"n1","method":"notify",
            "message":r#"ZERON-COMPLETIONS {"v":1,"commands":{"goal":[
                {"value":"draft ","label":"draft","description":"Draft a goal"},
                {"value":"pause","label":"pause"}],
              "mcp":[{"value":"reconnect ","label":"reconnect","children":[
                {"value":"reconnect github","label":"github"}]}]}}"#});
        assert_eq!(
            completions_notice(&json!({"type":"extension_ui_request",
            "method":"notify","message":"Goal paused"})),
            None
        );
        let mut data = json!({"commands":[
            {"name":"goal","description":"Goals","source":"extension"},
            {"name":"mcp","description":"MCP","source":"extension"},
            {"name":"todo","description":"Todos","source":"extension"},
            {"name":COMPLETIONS_COMMAND,"description":"Internal","source":"extension"},
        ]});
        data[COMPLETIONS_KEY] = completions_notice(&notice).unwrap();
        let listed = commands(&data);
        assert!(listed.iter().all(|c| c.name != COMPLETIONS_COMMAND));
        let goal = listed.iter().find(|c| c.name == "goal").unwrap();
        assert_eq!(goal.arguments.len(), 2);
        assert_eq!(goal.arguments[0].value, "draft ");
        assert_eq!(
            goal.arguments[0].description.as_deref(),
            Some("Draft a goal")
        );
        let mcp = listed.iter().find(|c| c.name == "mcp").unwrap();
        assert_eq!(mcp.arguments[0].children[0].label, "github");
        assert!(
            listed
                .iter()
                .find(|c| c.name == "todo")
                .unwrap()
                .arguments
                .is_empty()
        );
    }

    #[test]
    fn only_real_queue_modes_are_persisted() {
        assert_eq!(
            builtin("/steering all", true).unwrap(),
            json!({"type":"set_steering_mode","mode":"all"})
        );
        assert_eq!(
            builtin("/steering bogus", true).unwrap(),
            json!({"type":"get_state"})
        );
        assert_eq!(
            builtin("/follow-up one-at-a-time", true).unwrap(),
            json!({"type":"set_follow_up_mode","mode":"one-at-a-time"})
        );
    }
}
