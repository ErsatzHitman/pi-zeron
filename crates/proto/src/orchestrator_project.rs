//! What an orchestrator keeps for its whole tree, like a project in Claude
//! Code or Cursor: a goal, standing instructions, and memory: notes the
//! orchestrator saves as the work teaches it something. Every session of the
//! tree starts with them in its prompt ([`OrchestratorProject::prompt`]).
//! The user can also pause the tree: its agents then start no sessions and
//! send no messages until the user resumes it.
use serde::{Deserialize, Serialize};

/// Longest goal, in characters: one line.
pub const GOAL_MAX_CHARS: usize = 300;
/// Longest standing instructions, in characters. A registry op is capped at
/// 16 KiB serialized, and JSON escaping or multi-byte text can double it.
pub const INSTRUCTIONS_MAX_CHARS: usize = 6_000;
/// And in bytes, for text that is mostly multi-byte.
pub const INSTRUCTIONS_MAX_BYTES: usize = 10 * 1024;
/// Longest memory note, in characters.
pub const NOTE_MAX_CHARS: usize = 500;
/// Notes kept per tree; the oldest go first.
pub const MAX_NOTES: usize = 40;
/// The prompt section stays under this, so it fits in a process's env.
pub const PROMPT_MAX_BYTES: usize = 8 * 1024;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OrchestratorProject {
    pub root_id: String,
    #[serde(default)]
    pub goal: String,
    #[serde(default)]
    pub instructions: String,
    /// Set by the user: the tree's agents start and steer nothing.
    #[serde(default)]
    pub paused: bool,
    /// Unix ms.
    #[serde(default)]
    pub updated_at: i64,
}

/// A fact the orchestrator saved for every later session of its tree.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MemoryNote {
    /// The registry row id: an HLC, so ids sort in saving order.
    pub id: String,
    pub root_id: String,
    pub text: String,
    /// Unix ms.
    pub at: i64,
}

/// A change to a project's settings; `None` keeps a field.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectUpdate {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub goal: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub paused: Option<bool>,
}

impl ProjectUpdate {
    /// The update with its text trimmed, or why it is refused.
    pub fn validated(mut self) -> Result<Self, String> {
        if let Some(goal) = self.goal.as_mut() {
            *goal = one_line(goal);
            if goal.chars().count() > GOAL_MAX_CHARS {
                return Err(format!(
                    "the goal is longer than {GOAL_MAX_CHARS} characters"
                ));
            }
        }
        if let Some(instructions) = self.instructions.as_mut() {
            *instructions = instructions.trim().to_owned();
            if instructions.chars().count() > INSTRUCTIONS_MAX_CHARS
                || instructions.len() > INSTRUCTIONS_MAX_BYTES
            {
                return Err(format!(
                    "the instructions are longer than {INSTRUCTIONS_MAX_CHARS} characters"
                ));
            }
        }
        Ok(self)
    }

    pub fn apply(&self, project: &mut OrchestratorProject) {
        if let Some(goal) = &self.goal {
            project.goal = goal.clone();
        }
        if let Some(instructions) = &self.instructions {
            project.instructions = instructions.clone();
        }
        if let Some(paused) = self.paused {
            project.paused = paused;
        }
    }
}

/// A note's text, trimmed, or why it is refused.
pub fn note_text(text: &str) -> Result<String, String> {
    let text = text.trim();
    if text.is_empty() {
        return Err("the note is empty".into());
    }
    if text.chars().count() > NOTE_MAX_CHARS {
        return Err(format!("a note is at most {NOTE_MAX_CHARS} characters"));
    }
    Ok(text.to_owned())
}

fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

impl OrchestratorProject {
    /// The section every session of the tree gets in its prompt: the goal,
    /// the standing instructions and the memory, newest notes kept when it
    /// runs long. `None` when there is nothing to say.
    pub fn prompt(&self, notes: &[MemoryNote]) -> Option<String> {
        let goal = self.goal.trim();
        let instructions = self.instructions.trim();
        if goal.is_empty() && instructions.is_empty() && notes.is_empty() {
            return None;
        }
        let mut out = String::from(
            "# Project\n\nYou work in an orchestrator's project. What follows is the user's \
             goal and standing instructions for every session of it, and the project's memory. \
             Follow the instructions unless your task says otherwise.\n",
        );
        if !goal.is_empty() {
            out.push_str("\n## Goal\n\n");
            out.push_str(goal);
            out.push('\n');
        }
        if !instructions.is_empty() {
            out.push_str("\n## Standing instructions\n\n");
            out.push_str(instructions);
            out.push('\n');
        }
        if !notes.is_empty() {
            let heading = "\n## Project memory\n\nFacts the orchestrator saved earlier, oldest first. \
                           Read the latest with zeron_project_read.\n\n";
            let room = PROMPT_MAX_BYTES.saturating_sub(out.len() + heading.len());
            // The newest notes that fit, in saving order.
            let mut kept: Vec<String> = Vec::new();
            let mut used = 0;
            for note in notes.iter().rev() {
                let line = format!("- {}\n", one_line(&note.text));
                if used + line.len() > room {
                    break;
                }
                used += line.len();
                kept.push(line);
            }
            if !kept.is_empty() {
                out.push_str(heading);
                for line in kept.iter().rev() {
                    out.push_str(line);
                }
            }
        }
        Some(truncate_bytes(out, PROMPT_MAX_BYTES))
    }
}

/// `text` cut to at most `max` bytes, on a character boundary.
fn truncate_bytes(mut text: String, max: usize) -> String {
    if text.len() > max {
        let mut end = max;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    fn note(id: &str, text: &str) -> MemoryNote {
        MemoryNote {
            id: id.into(),
            root_id: "orchestrator-r".into(),
            text: text.into(),
            at: 1,
        }
    }

    #[test]
    fn an_empty_project_says_nothing() {
        let project = OrchestratorProject {
            root_id: "orchestrator-r".into(),
            ..Default::default()
        };
        assert_eq!(project.prompt(&[]), None);
    }

    #[test]
    fn the_prompt_carries_the_goal_instructions_and_memory_in_order() {
        let project = OrchestratorProject {
            root_id: "orchestrator-r".into(),
            goal: "Ship the parser rewrite".into(),
            instructions: "Run `cargo test -p parser` before you report.".into(),
            ..Default::default()
        };
        let prompt = project
            .prompt(&[
                note("1", "CI needs ZERON_X=1"),
                note("2", "main is\nprotected"),
            ])
            .unwrap();
        let goal = prompt.find("## Goal\n\nShip the parser rewrite").unwrap();
        let rules = prompt
            .find("## Standing instructions\n\nRun `cargo test")
            .unwrap();
        let memory = prompt.find("## Project memory").unwrap();
        assert!(goal < rules && rules < memory, "{prompt}");
        let first = prompt.find("- CI needs ZERON_X=1").unwrap();
        let second = prompt.find("- main is protected").unwrap();
        assert!(first < second, "oldest first, one line each: {prompt}");
    }

    #[test]
    fn a_long_memory_keeps_its_newest_notes_within_the_cap() {
        let project = OrchestratorProject {
            root_id: "orchestrator-r".into(),
            ..Default::default()
        };
        let notes: Vec<MemoryNote> = (0..MAX_NOTES)
            .map(|n| note(&format!("{n:03}"), &format!("{n:03} {}", "é".repeat(240))))
            .collect();
        let prompt = project.prompt(&notes).unwrap();
        assert!(prompt.len() <= PROMPT_MAX_BYTES);
        assert!(prompt.contains("- 039 "), "the newest stays");
        assert!(!prompt.contains("- 000 "), "the oldest goes");
    }

    #[test]
    fn updates_are_trimmed_and_bounded() {
        let update = ProjectUpdate {
            goal: Some("  Ship\n the   parser ".into()),
            instructions: Some("\n rules \n".into()),
            paused: None,
        }
        .validated()
        .unwrap();
        assert_eq!(update.goal.as_deref(), Some("Ship the parser"));
        assert_eq!(update.instructions.as_deref(), Some("rules"));
        let mut project = OrchestratorProject {
            paused: true,
            ..Default::default()
        };
        update.apply(&mut project);
        assert!(project.paused, "an unset field is kept");
        assert_eq!(project.goal, "Ship the parser");
        assert!(
            ProjectUpdate {
                goal: Some("x".repeat(GOAL_MAX_CHARS + 1)),
                ..Default::default()
            }
            .validated()
            .is_err()
        );
        assert!(
            ProjectUpdate {
                instructions: Some("x".repeat(INSTRUCTIONS_MAX_CHARS + 1)),
                ..Default::default()
            }
            .validated()
            .is_err()
        );
        assert_eq!(note_text("  a fact  ").unwrap(), "a fact");
        assert!(note_text(" ").is_err());
        assert!(note_text(&"x".repeat(NOTE_MAX_CHARS + 1)).is_err());
    }
}
