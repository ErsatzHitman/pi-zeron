use super::{Process, normalize::string};
use crate::HarnessError;
use serde_json::{Value, json};
use zeron_proto::{Model, ModelOption, ModelOptionChoice, ReasoningLevel, SlashCommand};

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
pub(super) fn commands(data: &Value) -> Vec<SlashCommand> {
    let mut commands: Vec<_> = data["commands"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|v| {
            let name = v["name"].as_str()?;
            // Transport commands for GUI clients (PIUI's `pi_ui_event`), not
            // something a person types.
            if name.starts_with("pi_ui_") {
                return None;
            }
            Some(SlashCommand {
                name: name.into(),
                description: string(v, "description").into(),
                input_hint: None,
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
            {"name":"todo","description":"Todos","source":"extension"},
        ]}));
        assert!(listed.iter().all(|c| !c.name.starts_with("pi_ui_")));
        assert!(listed.iter().any(|c| c.name == "todo"));
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
