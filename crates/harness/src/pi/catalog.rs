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
pub(super) async fn models(process: &mut Process) -> Result<Vec<Model>, HarnessError> {
    let mut backlog = vec![];
    let data = process
        .query(json!({"type":"get_available_models"}), &mut backlog)
        .await?;
    let mut result = vec![];
    for model in data["models"].as_array().into_iter().flatten() {
        let provider = string(model, "provider");
        let id = string(model, "id");
        process
            .query(
                json!({"type":"set_model","provider":provider,"modelId":id}),
                &mut backlog,
            )
            .await?;
        let supported = process
            .query(
                json!({"type":"get_available_thinking_levels"}),
                &mut backlog,
            )
            .await?;
        result.push(Model {
            id: format!("{provider}/{id}"),
            label: model["name"].as_str().unwrap_or(id).into(),
            description: Some(provider.into()),
            reasoning_levels: levels(&supported),
            options: thinking_option(&supported),
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
/// Extension commands that act only on Pi's terminal screen.
const TERMINAL_ONLY: [&str; 1] = ["claude-refusal-rewind"];
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
    fn piui_transport_commands_are_hidden_from_the_palette() {
        let listed = commands(&json!({"commands":[
            {"name":"pi_ui_event","description":"Internal","source":"extension"},
            {"name":"claude-refusal-rewind","description":"Redraw","source":"extension"},
            {"name":"todo","description":"Todos","source":"extension"},
        ]}));
        assert!(listed.iter().all(|c| !c.name.starts_with("pi_ui_")));
        assert!(listed.iter().all(|c| c.name != "claude-refusal-rewind"));
        assert!(listed.iter().any(|c| c.name == "todo"));
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
