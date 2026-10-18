//! Load the existing Zeron delegation tools into this Pi process only, and
//! give an orchestrator's root and members their own profile.
use std::ffi::OsString;
use std::path::Path;

use zeron_proto::orchestrator::{self, Place, Role};

use crate::{HarnessError, process::Command, scratch::ScratchDir};

/// Appended to every Pi chat's system prompt.
pub(crate) const VISUALS_PROMPT: &str = include_str!("visuals.md");

/// A btw side chat's tools (Pi `--tools`): it answers questions about the
/// session it forked from, so it reads files and the web and changes
/// nothing. Pi's allowlist only filters MCP tools when it names one, so the
/// chat also runs with `--no-mcp` and gets no server's tools.
pub(crate) const BTW_TOOLS: &str = "read,grep,find,ls,web_search,web_fetch";
const BTW_PROMPT: &str = "This is a btw side chat: the user is asking about the conversation \
above without interrupting it. Answer from that conversation. You can read files with read, \
grep, find and ls, and search and read the web with web_search and web_fetch, and nothing else: \
you cannot run commands, edit files, or start agents or sessions, and nothing you do here \
reaches the main conversation.\n";

pub(super) fn configure(
    cmd: &mut Command,
    config: &zeron_proto::McpServer,
) -> Result<ScratchDir, HarnessError> {
    let scratch = ScratchDir::new("pi-mcp")?;
    let place = Place::from_env(&config.env);
    // The project section is for this Pi's prompt, not the MCP server's env.
    let mut config = config.clone();
    let project = config
        .env
        .remove(orchestrator::ENV_PROJECT_PROMPT)
        .filter(|prompt| !prompt.trim().is_empty());
    // So are the session's model restrictions: every Pi it starts inherits
    // them, whatever MCP config it loads.
    if let Some(policy) = config
        .env
        .remove(orchestrator::ENV_MODEL_POLICY)
        .filter(|files| !files.is_empty())
    {
        cmd.env(orchestrator::ENV_MODEL_POLICY, policy);
    }
    // And the tree's policy, which the guard enforces in each of them.
    if let Some(policy) = config
        .env
        .remove(orchestrator::ENV_TREE_POLICY)
        .filter(|file| !file.is_empty())
    {
        cmd.env(orchestrator::ENV_TREE_POLICY, policy);
    }
    for arg in role_args(&place, project.as_deref(), scratch.path())? {
        cmd.arg(arg);
    }
    cmd.env(
        "ZERON_PI_MCP",
        serde_json::to_string(&config).expect("serializable MCP config"),
    );
    cmd.env_remove(orchestrator::ENV_PROJECT_PROMPT);
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
///
/// A run in an orchestrator's tree whose project has a goal, instructions or
/// memory also gets that `project` section appended.
fn role_args(
    place: &Place,
    project: Option<&str>,
    scratch: &Path,
) -> Result<Vec<OsString>, HarnessError> {
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
    // A btw side chat only asks, wherever it sits: no role prompt, project
    // or tools beyond its own.
    if place.chat_id.starts_with(zeron_proto::BTW_CHAT_PREFIX) {
        let prompt = scratch.join("zeron-btw.md");
        std::fs::write(&prompt, BTW_PROMPT)?;
        args.extend(["--append-system-prompt".into(), prompt.into()]);
        args.extend(["--tools".into(), BTW_TOOLS.into(), "--no-mcp".into()]);
        return Ok(args);
    }
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
    if let Some(project) = project.filter(|_| place.role != Role::Chat) {
        let prompt = scratch.join("zeron-project.md");
        std::fs::write(&prompt, project)?;
        args.extend(["--append-system-prompt".into(), prompt.into()]);
    }
    Ok(args)
}

/// Why a run cannot start: an agent started it (it sits in an orchestrator's
/// tree and is not a btw chat) on a reserved model, Claude Fable or GPT Astra
/// class. A chat with a parent is not enough: the user's own forks and side
/// chats have one too. Chats an agent creates outside a tree are refused by
/// the create_chat tool. Only a model Zeron configured is checked: with none
/// the run takes Pi's own default, which create_chat already refuses to leave
/// open when its caller runs on a reserved one.
pub(super) fn launch_refusal(
    model: Option<&str>,
    mcp: Option<&zeron_proto::McpServer>,
) -> Option<String> {
    use zeron_proto::reserved_models::{refusal, reserved_family};
    let model = model.filter(|m| !m.is_empty() && *m != "default")?;
    reserved_family(model)?;
    let place = Place::from_env(&mcp?.env);
    if place.chat_id.starts_with(zeron_proto::BTW_CHAT_PREFIX) {
        return None;
    }
    matches!(place.role, Role::Member { .. })
        .then(|| refusal(model, "A session started by an agent"))
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
        let args = role_args(place, None, scratch.path())
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
    fn a_btw_side_chat_only_reads_files_and_the_web() {
        for role in [Role::Chat, Role::Member { depth: 2 }] {
            let mut place = at(role, Some("parent-1"));
            place.chat_id = format!("{}side-1", zeron_proto::BTW_CHAT_PREFIX);
            let (_scratch, args) = args_at(&place);
            let tools: Vec<&str> = flag(&args, "--tools").unwrap().split(',').collect();
            assert_eq!(
                tools,
                ["read", "grep", "find", "ls", "web_search", "web_fetch"]
            );
            assert!(args.iter().any(|arg| arg == "--no-mcp"));
            assert!(args.iter().any(|arg| arg.ends_with("zeron-btw.md")));
            assert!(!args.iter().any(|arg| arg.ends_with("zeron-member.md")));
            assert!(!args.iter().any(|arg| arg == "--system-prompt"));
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
    fn the_root_and_every_member_depth_carry_the_research_rule() {
        let (_scratch, args) = args(Role::Root);
        let root = std::fs::read_to_string(flag(&args, "--system-prompt").unwrap()).unwrap();
        assert!(root.contains("Research is never yours"), "{root}");
        assert!(root.contains("write false session"), "{root}");
        for depth in 1..=orchestrator::MAX_DEPTH {
            let (_scratch, args) = args_at(&at(Role::Member { depth }, Some("parent-1")));
            let member = args
                .iter()
                .find(|arg| arg.ends_with("zeron-member.md"))
                .expect("the member prompt");
            let member = std::fs::read_to_string(member).unwrap();
            assert!(member.contains("never explore the codebase"), "{member}");
            assert!(member.contains("read-only sub-agents"), "{member}");
            assert!(member.contains("One named file or one grep"), "{member}");
        }
    }

    #[test]
    fn a_tree_s_runs_get_their_project_and_a_plain_chat_does_not() {
        let project = "# Project\n\n## Goal\n\nShip the parser\n";
        for role in [Role::Root, Role::Member { depth: 1 }] {
            let scratch = ScratchDir::new("pi-mcp-test").unwrap();
            let args: Vec<String> = role_args(
                &at(role, (role != Role::Root).then_some("parent-1")),
                Some(project),
                scratch.path(),
            )
            .unwrap()
            .into_iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
            let file = args
                .iter()
                .find(|arg| arg.ends_with("zeron-project.md"))
                .unwrap_or_else(|| panic!("{role:?} gets its project: {args:?}"));
            let at = args.iter().position(|arg| arg == file).unwrap();
            assert_eq!(args[at - 1], "--append-system-prompt");
            assert_eq!(std::fs::read_to_string(file).unwrap(), project);
        }
        // A plain chat is in no project.
        let scratch = ScratchDir::new("pi-mcp-test").unwrap();
        let args = role_args(&at(Role::Chat, None), Some(project), scratch.path()).unwrap();
        assert_eq!(args.len(), 4);
    }

    #[cfg(not(windows))]
    #[test]
    fn the_project_section_stays_out_of_the_mcp_server_s_env() {
        let project = "# Project\n";
        let mut cmd = Command::new("pi");
        let config = zeron_proto::McpServer {
            name: "zeron".into(),
            command: "zeron".into(),
            args: vec!["mcp".into()],
            env: [
                ("ZERON_CHAT_ID".to_owned(), "chat-1".to_owned()),
                (orchestrator::ENV_ROLE.to_owned(), "member".to_owned()),
                (
                    orchestrator::ENV_PROJECT_PROMPT.to_owned(),
                    project.to_owned(),
                ),
            ]
            .into(),
        };
        let _scratch = configure(&mut cmd, &config).unwrap();
        let mcp = cmd
            .as_std()
            .get_envs()
            .find(|(key, _)| *key == "ZERON_PI_MCP")
            .and_then(|(_, value)| value)
            .map(|value| value.to_string_lossy().into_owned())
            .unwrap();
        assert!(!mcp.contains("ZERON_PROJECT_PROMPT"), "{mcp}");
    }

    fn server_at(place: &Place) -> zeron_proto::McpServer {
        zeron_proto::McpServer {
            name: "zeron".into(),
            command: "zeron".into(),
            args: vec!["mcp".into()],
            env: place
                .env()
                .into_iter()
                .map(|(key, value)| (key.to_owned(), value))
                .chain([("ZERON_CHAT_ID".to_owned(), place.chat_id.clone())])
                .collect(),
        }
    }

    #[test]
    fn a_session_an_agent_started_cannot_start_on_a_reserved_model() {
        const FABLE: Option<&str> = Some("anthropic/claude-fable-5");
        let member = server_at(&at(Role::Member { depth: 1 }, Some("orchestrator-r")));
        let error = launch_refusal(FABLE, Some(&member)).expect("a member is agent-started");
        assert!(
            error.starts_with(
                "A session started by an agent cannot start on anthropic/claude-fable-5"
            ),
            "{error}"
        );
        let deeper = server_at(&at(Role::Member { depth: 2 }, Some("member-1")));
        assert!(launch_refusal(Some("openai/gpt-6-astra"), Some(&deeper)).is_some());
        // Other models, no model and the default model pass.
        for model in [
            Some("anthropic/claude-opus-5"),
            Some("default"),
            Some(""),
            None,
        ] {
            assert_eq!(launch_refusal(model, Some(&member)), None, "{model:?}");
        }
    }

    #[test]
    fn the_user_s_own_sessions_and_btw_chats_may_use_a_reserved_model() {
        const FABLE: Option<&str> = Some("anthropic/claude-fable-5");
        for role in [Role::Chat, Role::Root] {
            let server = server_at(&at(role, None));
            assert_eq!(launch_refusal(FABLE, Some(&server)), None, "{role:?}");
        }
        // A fork or a side chat the user opened has a parent chat and no tree.
        let fork = server_at(&at(Role::Chat, Some("parent-1")));
        assert_eq!(launch_refusal(FABLE, Some(&fork)), None);
        // No MCP config, no place: a title run or an old host.
        assert_eq!(launch_refusal(FABLE, None), None);
        for role in [Role::Chat, Role::Member { depth: 2 }] {
            let mut place = at(role, Some("parent-1"));
            place.chat_id = format!("{}side-1", zeron_proto::BTW_CHAT_PREFIX);
            assert_eq!(
                launch_refusal(FABLE, Some(&server_at(&place))),
                None,
                "{role:?}"
            );
        }
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
            "wait_for_turns",
            "read_chat",
            "respond_to_input",
            "interrupt_chat",
            "board_post",
            "board_read",
            "show_visual",
            "project_read",
            "project_remember",
            "project_forget",
            "project_update",
            "merge_preview",
            "stop_tree",
            "register_project",
            "github_preflight",
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
