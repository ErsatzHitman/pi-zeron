//! Load the existing Zeron delegation tools into this Pi process only, and
//! give an orchestrator's root and members their own profile.
use std::ffi::OsString;
use std::path::Path;

use zeron_proto::orchestrator::{self, Place, Role};

use crate::{HarnessError, process::Command, scratch::ScratchDir};

/// Appended to every Pi chat's system prompt.
pub(crate) const VISUALS_PROMPT: &str = include_str!("visuals.md");

pub(super) fn configure(
    cmd: &mut Command,
    config: &zeron_proto::McpServer,
) -> Result<ScratchDir, HarnessError> {
    let scratch = ScratchDir::new("pi-mcp")?;
    let place = Place::from_env(&config.env);
    for arg in role_args(&place, scratch.path())? {
        cmd.arg(arg);
    }
    cmd.env(
        "ZERON_PI_MCP",
        serde_json::to_string(config).expect("serializable MCP config"),
    );
    // Extensions and sub-agents read the place too; a run never inherits a
    // stale one.
    for key in orchestrator::PLACE_ENV {
        cmd.env_remove(key);
    }
    for (key, value) in place.env() {
        cmd.env(key, value);
    }
    Ok(scratch)
}

/// Pi's arguments for a run at `place`, writing the files they name into
/// `scratch`, which lives as long as the process.
///
/// Every run loads the Zeron tools and learns the visuals Zeron draws
/// (charts, diagrams, tables). A root swaps Pi's coding prompt for the
/// orchestrator's and keeps only its own tools: no bash, edits, MCP servers,
/// skills, prompt templates or context files. A member gets the tree's
/// working rules.
///
/// Where a run sits (its chat, parent, tree, read-only) is not written into
/// the prompt here: Zeron publishes it as env ([`Place::env`],
/// `ZERON_READ_ONLY`) and the Pi harness says it, with the rest of the
/// session's capability map, on every surface including the Pis it starts.
fn role_args(place: &Place, scratch: &Path) -> Result<Vec<OsString>, HarnessError> {
    let extension = scratch.join("zeron-mcp.mjs");
    std::fs::write(&extension, include_str!("mcp.mjs"))?;
    let visuals = scratch.join("zeron-visuals.md");
    std::fs::write(&visuals, VISUALS_PROMPT)?;
    let mut args: Vec<OsString> = vec![
        "--extension".into(),
        extension.into(),
        "--append-system-prompt".into(),
        visuals.into(),
    ];
    match place.role {
        Role::Chat => {}
        Role::Root => {
            let prompt = scratch.join("zeron-orchestrator.md");
            std::fs::write(&prompt, orchestrator::ROOT_SYSTEM_PROMPT)?;
            args.extend(["--system-prompt".into(), prompt.into()]);
            args.extend(
                [
                    "--tools",
                    orchestrator::ROOT_TOOLS,
                    "--no-mcp",
                    "--no-skills",
                    "--no-prompt-templates",
                    "--no-context-files",
                ]
                .map(OsString::from),
            );
        }
        Role::Member { .. } => {
            let prompt = scratch.join("zeron-member.md");
            std::fs::write(&prompt, orchestrator::member_prompt(place))?;
            args.extend(["--append-system-prompt".into(), prompt.into()]);
        }
    }
    Ok(args)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(role: Role, parent: Option<&str>) -> Place {
        Place {
            role,
            chat_id: "chat-1".into(),
            title: Some("Fix the parser".into()),
            parent_id: parent.map(str::to_owned),
            parent_title: Some("Plan".into()),
            root_id: (role != Role::Chat).then(|| "orchestrator-r".to_owned()),
        }
    }

    fn args(role: Role) -> (crate::scratch::ScratchDir, Vec<String>) {
        let parent = (role != Role::Root).then_some("parent-1");
        args_at(&at(role, parent.filter(|_| role != Role::Chat)))
    }

    fn args_at(place: &Place) -> (crate::scratch::ScratchDir, Vec<String>) {
        let scratch = ScratchDir::new("pi-mcp-test").unwrap();
        let args = role_args(place, scratch.path())
            .unwrap()
            .into_iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        (scratch, args)
    }

    fn flag<'a>(args: &'a [String], name: &str) -> Option<&'a str> {
        let at = args.iter().position(|arg| arg == name)?;
        args.get(at + 1).map(String::as_str)
    }

    #[test]
    fn a_plain_chat_keeps_pi_s_prompt_and_every_tool() {
        let (_scratch, args) = args(Role::Chat);
        assert_eq!(args.len(), 4, "{args:?}");
        assert!(flag(&args, "--extension").is_some_and(|f| f.ends_with("zeron-mcp.mjs")));
        assert!(
            flag(&args, "--append-system-prompt").is_some_and(|f| f.ends_with("zeron-visuals.md"))
        );
        for absent in ["--system-prompt", "--tools", "--no-mcp"] {
            assert!(!args.iter().any(|arg| arg == absent), "{absent}");
        }
    }

    #[test]
    fn a_root_plans_and_delegates_with_its_own_prompt_and_tools() {
        let (_scratch, args) = args(Role::Root);
        let prompt = flag(&args, "--system-prompt").expect("the root's own prompt");
        assert_eq!(
            std::fs::read_to_string(prompt).unwrap(),
            orchestrator::ROOT_SYSTEM_PROMPT
        );
        let tools: Vec<&str> = flag(&args, "--tools").unwrap().split(',').collect();
        assert!(tools.contains(&"zeron_*") && tools.contains(&"ask_user"));
        for coding in [
            "bash",
            "edit",
            "write",
            "codemode",
            "delegate",
            "subagent_start",
        ] {
            assert!(!tools.contains(&coding), "{coding}");
        }
        for off in [
            "--no-mcp",
            "--no-skills",
            "--no-prompt-templates",
            "--no-context-files",
        ] {
            assert!(args.iter().any(|arg| arg == off), "{off}");
        }
        // Still draws Zeron's visuals.
        assert!(args.iter().any(|arg| arg.ends_with("zeron-visuals.md")));
    }

    #[test]
    fn a_member_gets_the_tree_s_working_rules() {
        let (_scratch, args) = args(Role::Member { depth: 2 });
        let member = args
            .iter()
            .find(|arg| arg.ends_with("zeron-member.md"))
            .expect("the member prompt");
        let member = std::fs::read_to_string(member).unwrap();
        assert!(member.contains("You are 2 levels below the root"));
        assert!(member.contains("zeron_board_read"));
        // Its place rides the env; the Pi harness says it.
        assert!(!member.contains("Fix the parser"));
        assert!(!args.iter().any(|arg| arg == "--tools"));
    }

    #[test]
    fn a_side_chat_s_place_rides_the_env_not_a_prompt_file() {
        let (_scratch, args) = args_at(&at(Role::Chat, Some("parent-1")));
        assert_eq!(args.len(), 4, "{args:?}");
        let place = Place::from_env(
            &at(Role::Chat, Some("parent-1"))
                .env()
                .into_iter()
                .map(|(k, v)| (k.to_owned(), v))
                .chain([("ZERON_CHAT_ID".to_owned(), "chat-1".to_owned())])
                .collect(),
        );
        assert_eq!(place.parent_id.as_deref(), Some("parent-1"));
        assert_eq!(place.parent_title.as_deref(), Some("Plan"));
    }

    #[test]
    fn the_prompts_name_only_zeron_tools_that_exist() {
        let prompts = [
            orchestrator::ROOT_SYSTEM_PROMPT.to_owned(),
            orchestrator::member_prompt(&at(Role::Member { depth: 1 }, Some("orchestrator-r"))),
            VISUALS_PROMPT.to_owned(),
        ];
        let known = [
            "whoami",
            "list_projects",
            "list_chats",
            "list_models",
            "create_chat",
            "create_chats",
            "send_message",
            "wait_for_turn",
            "read_chat",
            "respond_to_input",
            "interrupt_chat",
            "board_post",
            "board_read",
            "show_visual",
        ];
        for prompt in prompts {
            for word in prompt.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_')) {
                if let Some(tool) = word.strip_prefix("zeron_").filter(|t| !t.is_empty()) {
                    assert!(known.contains(&tool), "unknown tool zeron_{tool}");
                }
            }
        }
    }

    #[test]
    fn the_visuals_prompt_names_what_the_renderer_reads() {
        for needle in [
            "```chart",
            "```mermaid",
            "`series`",
            "\"label\"",
            "\"value\"",
            "`source`",
            "`zeron_show_visual`",
        ] {
            assert!(super::VISUALS_PROMPT.contains(needle), "{needle}");
        }
    }
}
