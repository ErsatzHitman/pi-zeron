//! Load the existing Zeron delegation tools into this Pi process only.
use crate::{HarnessError, process::Command, scratch::ScratchDir};

/// Appended to every Pi chat's system prompt.
pub(crate) const VISUALS_PROMPT: &str = include_str!("visuals.md");

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
    // Every chat learns the visuals Zeron draws (charts, diagrams, tables).
    // Pi reads the instructions from the file, which lives as long as the
    // process.
    let visuals = scratch.path().join("zeron-visuals.md");
    std::fs::write(&visuals, VISUALS_PROMPT)?;
    cmd.arg("--append-system-prompt").arg(visuals);
    // An orchestrator's root also runs its sessions through these tools.
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

#[cfg(test)]
mod tests {
    #[test]
    fn the_visuals_prompt_names_what_the_renderer_reads() {
        for needle in [
            "```chart",
            "```mermaid",
            "`series`",
            "\"label\"",
            "\"value\"",
            "`source`",
            "`zeron_html_render`",
        ] {
            assert!(super::VISUALS_PROMPT.contains(needle), "{needle}");
        }
    }
}
