//! Load the existing Zeron delegation tools into this Pi process only.
use crate::{HarnessError, process::Command, scratch::ScratchDir};

pub(super) fn configure(
    cmd: &mut Command,
    config: &zeron_proto::McpServer,
) -> Result<ScratchDir, HarnessError> {
    let scratch = ScratchDir::new("pi-mcp")?;
    let extension = scratch.path().join("zeron-mcp.mjs");
    std::fs::write(&extension, include_str!("mcp.mjs"))?;
    cmd.arg("--extension").arg(extension).env(
        "ZERON_PI_MCP",
        serde_json::to_string(config).expect("serializable MCP config"),
    );
    // An orchestrator's root runs its sessions through these tools; Pi reads
    // the instructions from the file, which lives as long as the process.
    if config
        .env
        .get("ZERON_CHAT_ID")
        .is_some_and(|chat| zeron_proto::orchestrator::is_root_chat(chat))
    {
        let prompt = scratch.path().join("zeron-orchestrator.md");
        std::fs::write(&prompt, zeron_proto::orchestrator::ROOT_PROMPT)?;
        cmd.arg("--append-system-prompt").arg(prompt);
    }
    Ok(scratch)
}
