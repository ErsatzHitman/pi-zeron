//! What an orchestrator keeps for its whole tree, like a project in Claude
//! Code or Cursor: a goal, standing instructions, and memory: notes the
//! orchestrator saves as the work teaches it something. Every session of the
//! tree starts with them in its prompt ([`OrchestratorProject::prompt`]).
//! The user can also pause the tree: its agents then start no sessions and
//! send no messages until the user resumes it. A [`TreePolicy`] holds the
//! rules the user sets for the whole tree (where tests run, how many
//! commands and writers at once, which models, how much it may spend); the
//! engine, the MCP server and the Pi guard enforce them, and the prompt
//! states them.
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
/// Most heavy local commands (compilers, test runners) a policy may allow at once.
pub const HEAVY_COMMAND_LIMIT_MAX: u8 = 8;
/// Most models a policy's allow-list names.
pub const ALLOWED_MODELS_MAX: usize = 32;
/// Longest model id in the allow-list, in characters.
pub const ALLOWED_MODEL_MAX_CHARS: usize = 120;
/// Highest spending cap, in cents ($100,000).
pub const SPEND_CAP_MAX_CENTS: u64 = 10_000_000;
/// Longest reason a pause records, in characters.
pub const PAUSE_REASON_MAX_CHARS: usize = 200;
/// Where the orchestrator is told to wrap up, unless the policy says other.
pub const DEFAULT_SPEND_WARN_PCT: u8 = 80;

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
    /// Why the tree is paused when Zeron paused it itself (the spending cap
    /// was reached); empty for the user's own pause.
    #[serde(default)]
    pub pause_reason: String,
    /// The rules for the whole tree. Only the user sets them: an agent that
    /// could would also be able to lift the limits it is bound by.
    #[serde(default)]
    pub policy: TreePolicy,
    /// Unix ms.
    #[serde(default)]
    pub updated_at: i64,
}

/// The rules an orchestrator's user sets for every session of its tree. All
/// off by default, so a project without a policy behaves as it always did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct TreePolicy {
    /// Builds and tests run on GitHub, not on this machine: the Pi guard
    /// refuses compilers and test runners in the tree's shells.
    pub github_only_tests: bool,
    /// Most heavy local commands running at once across the tree, per
    /// device; a further one waits for a slot, then is refused.
    pub heavy_command_limit: Option<u8>,
    /// Most writing sessions running a turn at once; further ones queue.
    pub writer_limit: Option<u8>,
    /// Models the tree may use; empty allows any. Disabled models are a
    /// separate list on the root chat (the model policy).
    pub allowed_models: Vec<String>,
    /// Spending cap for all sessions of the tree, in cents. At the cap no
    /// session or message starts and the tree is paused.
    pub spend_cap_cents: Option<u64>,
    /// Spend counts from here, Unix ms. Set by Zeron whenever the cap
    /// changes, so raising the cap starts a fresh budget.
    pub spend_since_ms: i64,
    /// Percent of the cap at which the orchestrator is told to wrap up.
    pub spend_warn_pct: u8,
    /// A writing session starts in a GitHub project only after
    /// `github_preflight` reports no blockers.
    pub require_preflight: bool,
}

impl Default for TreePolicy {
    fn default() -> Self {
        Self {
            github_only_tests: false,
            heavy_command_limit: None,
            writer_limit: None,
            allowed_models: Vec::new(),
            spend_cap_cents: None,
            spend_since_ms: 0,
            spend_warn_pct: DEFAULT_SPEND_WARN_PCT,
            require_preflight: false,
        }
    }
}

impl TreePolicy {
    /// What the settings dialog offers a new orchestrator. Never applied on
    /// its own to an existing project.
    pub fn recommended() -> Self {
        Self {
            github_only_tests: true,
            heavy_command_limit: Some(1),
            writer_limit: Some(4),
            require_preflight: true,
            ..Self::default()
        }
    }

    /// Nothing is restricted.
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }

    /// The policy with its lists tidied, or why it is refused.
    pub fn validated(mut self) -> Result<Self, String> {
        if let Some(limit) = self.heavy_command_limit
            && !(1..=HEAVY_COMMAND_LIMIT_MAX).contains(&limit)
        {
            return Err(format!(
                "the heavy command limit is between 1 and {HEAVY_COMMAND_LIMIT_MAX}"
            ));
        }
        if let Some(limit) = self.writer_limit
            && !(1..=crate::orchestrator::MAX_LIVE_SESSIONS as u8).contains(&limit)
        {
            return Err(format!(
                "the writer limit is between 1 and {}",
                crate::orchestrator::MAX_LIVE_SESSIONS
            ));
        }
        if let Some(cents) = self.spend_cap_cents
            && !(1..=SPEND_CAP_MAX_CENTS).contains(&cents)
        {
            return Err(format!(
                "the spending cap is between $0.01 and {}",
                dollars(SPEND_CAP_MAX_CENTS)
            ));
        }
        if !(1..=100).contains(&self.spend_warn_pct) {
            return Err("the spending warning is between 1 and 100 percent".into());
        }
        let mut models: Vec<String> = Vec::new();
        for model in std::mem::take(&mut self.allowed_models) {
            let model = model.trim().to_owned();
            if model.is_empty() || models.iter().any(|m| m.eq_ignore_ascii_case(&model)) {
                continue;
            }
            if model.chars().count() > ALLOWED_MODEL_MAX_CHARS {
                return Err(format!(
                    "a model id is at most {ALLOWED_MODEL_MAX_CHARS} characters"
                ));
            }
            models.push(model);
        }
        if models.len() > ALLOWED_MODELS_MAX {
            return Err(format!("at most {ALLOWED_MODELS_MAX} allowed models"));
        }
        self.allowed_models = models;
        Ok(self)
    }

    /// Whether the allow-list admits `model`; no list admits any. See
    /// [`model_matches`].
    pub fn allows_model(&self, model: &str) -> bool {
        self.allowed_models.is_empty()
            || self
                .allowed_models
                .iter()
                .any(|allowed| model_matches(allowed, model))
    }

    /// The cap in dollars, for messages.
    pub fn spend_cap_label(&self) -> Option<String> {
        self.spend_cap_cents.map(dollars)
    }

    /// One bullet per active rule, as the prompt states them.
    pub fn prompt_lines(&self) -> Vec<String> {
        let mut lines = Vec::new();
        if self.github_only_tests {
            lines.push(
                "Builds and tests run on GitHub only. Never run the project's build or test \
                 suite on this machine (cargo, npm, tsc, pytest and the like are refused): push \
                 your branch, open a draft pull request and read CI with `gh pr checks` or \
                 `gh run watch`."
                    .to_owned(),
            );
        }
        if let Some(limit) = self.heavy_command_limit {
            lines.push(format!(
                "At most {limit} heavy local command{} (compilers, linkers, bundlers) may run \
                 at once across the tree; one that cannot get a slot is refused.",
                if limit == 1 { "" } else { "s" }
            ));
        }
        if let Some(limit) = self.writer_limit {
            lines.push(format!(
                "At most {limit} writing session{} run{} at once; further ones queue until a \
                 slot frees.",
                if limit == 1 { "" } else { "s" },
                if limit == 1 { "s" } else { "" }
            ));
        }
        if !self.allowed_models.is_empty() {
            lines.push(format!(
                "Only these models may be used, and a session on another is refused: {}.",
                self.allowed_models.join(", ")
            ));
        }
        if let Some(cap) = self.spend_cap_label() {
            lines.push(format!(
                "All sessions of the tree together may spend {cap}. When it is reached no \
                 session or message starts and the tree is paused; at {}% the orchestrator is \
                 told to wrap up.",
                self.spend_warn_pct
            ));
        }
        if self.require_preflight {
            lines.push(
                "A writing session starts in a GitHub project only after zeron_github_preflight \
                 reports no blockers."
                    .to_owned(),
            );
        }
        lines
    }
}

/// Thinking levels Pi may append to a model reference after a colon.
const THINKING_SUFFIXES: [&str; 7] = ["off", "minimal", "low", "medium", "high", "xhigh", "max"];

/// `(provider, id)` of a model reference: lower case, without a `:thinking`
/// suffix; the provider is absent from a bare id.
fn split_model_ref(reference: &str) -> (Option<String>, String) {
    let lower = reference.trim().to_lowercase();
    let bare = match lower.rsplit_once(':') {
        Some((id, level)) if THINKING_SUFFIXES.contains(&level) => id,
        _ => lower.as_str(),
    };
    match bare.split_once('/') {
        Some((provider, id)) => (Some(provider.to_owned()), id.to_owned()),
        None => (None, bare.to_owned()),
    }
}

/// Whether `entry` names `model`; both are a bare id or `provider/id`,
/// compared without case or thinking suffix. A bare side names the id on any
/// provider, because the models an agent is offered carry bare ids, while a
/// pick in the UI carries its provider.
pub fn model_matches(entry: &str, model: &str) -> bool {
    let (entry_provider, entry_id) = split_model_ref(entry);
    let (model_provider, model_id) = split_model_ref(model);
    entry_id == model_id
        && match (entry_provider, model_provider) {
            (Some(a), Some(b)) => a == b,
            _ => true,
        }
}

/// `$12.34` for a number of cents.
pub fn dollars(cents: u64) -> String {
    format!("${}.{:02}", cents / 100, cents % 100)
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
    /// Why Zeron paused the tree itself; a resume clears it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pause_reason: Option<String>,
    /// The tree policy, replaced whole. `spend_since_ms` in it is ignored:
    /// Zeron moves it when the cap changes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub policy: Option<TreePolicy>,
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
        if let Some(reason) = self.pause_reason.as_mut() {
            *reason = one_line(reason)
                .chars()
                .take(PAUSE_REASON_MAX_CHARS)
                .collect();
        }
        if let Some(policy) = self.policy.take() {
            self.policy = Some(policy.validated()?);
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
            if !paused {
                project.pause_reason.clear();
            }
        }
        if let Some(reason) = &self.pause_reason {
            project.pause_reason = reason.clone();
        }
        if let Some(policy) = &self.policy {
            let mut next = policy.clone();
            // A changed cap is a fresh budget; any other edit keeps the
            // count running.
            next.spend_since_ms = if next.spend_cap_cents != project.policy.spend_cap_cents {
                chrono::Utc::now().timestamp_millis()
            } else {
                project.policy.spend_since_ms
            };
            project.policy = next;
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
        let policy = self.policy.prompt_lines();
        if goal.is_empty() && instructions.is_empty() && notes.is_empty() && policy.is_empty() {
            return None;
        }
        let mut out = String::from(
            "# Project\n\nYou work in an orchestrator's project. What follows is the user's \
             goal and standing instructions for every session of it, and the project's memory. \
             Follow the instructions unless your task says otherwise; the tree policy, when \
             there is one, is the exception.\n",
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
        if !policy.is_empty() {
            out.push_str(
                "\n## Tree policy\n\nThe user set these rules for the whole tree. Zeron enforces \
                 them, and they override the standing instructions and your task: a refused \
                 command, model or session is a rule, not a glitch. Do not retry it, rephrase it \
                 or route around it; report it in your final reply. Read the current values with \
                 zeron_project_read.\n\n",
            );
            for line in &policy {
                out.push_str("- ");
                out.push_str(line);
                out.push('\n');
            }
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
            ..Default::default()
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

    fn policy_project(policy: TreePolicy) -> OrchestratorProject {
        OrchestratorProject {
            root_id: "orchestrator-r".into(),
            policy,
            ..Default::default()
        }
    }

    #[test]
    fn a_default_policy_says_nothing_and_an_old_row_decodes_to_it() {
        assert!(TreePolicy::default().is_default());
        assert!(TreePolicy::default().prompt_lines().is_empty());
        assert_eq!(policy_project(TreePolicy::default()).prompt(&[]), None);
        let old: OrchestratorProject =
            serde_json::from_value(serde_json::json!({ "rootId": "r", "goal": "g" })).unwrap();
        assert!(old.policy.is_default());
        assert_eq!(old.policy.spend_warn_pct, DEFAULT_SPEND_WARN_PCT);
    }

    #[test]
    fn a_policy_renders_between_the_instructions_and_the_memory() {
        let mut project = policy_project(TreePolicy {
            github_only_tests: true,
            heavy_command_limit: Some(2),
            writer_limit: Some(1),
            allowed_models: vec!["gpt-5.6".into()],
            spend_cap_cents: Some(2_550),
            require_preflight: true,
            ..Default::default()
        });
        project.instructions = "Run `cargo test -p parser` before you report.".into();
        let prompt = project.prompt(&[note("1", "CI needs ZERON_X=1")]).unwrap();
        let rules = prompt.find("## Standing instructions").unwrap();
        let policy = prompt.find("## Tree policy").unwrap();
        let memory = prompt.find("## Project memory").unwrap();
        assert!(rules < policy && policy < memory, "{prompt}");
        assert!(prompt.contains("they override the standing instructions and your task"));
        assert!(prompt.contains("Builds and tests run on GitHub only"));
        assert!(prompt.contains("At most 2 heavy local commands"));
        assert!(prompt.contains("At most 1 writing session runs at once"));
        assert!(prompt.contains("Only these models may be used"));
        assert!(prompt.contains("may spend $25.50"));
        assert!(prompt.contains("zeron_github_preflight"));
        // A policy alone is worth a prompt.
        let only = policy_project(TreePolicy::recommended());
        assert!(only.prompt(&[]).unwrap().contains("## Tree policy"));
    }

    #[test]
    fn a_long_memory_cannot_push_the_policy_out_of_the_prompt() {
        let project = policy_project(TreePolicy::recommended());
        let notes: Vec<MemoryNote> = (0..MAX_NOTES)
            .map(|n| note(&format!("{n:03}"), &format!("{n:03} {}", "é".repeat(240))))
            .collect();
        let prompt = project.prompt(&notes).unwrap();
        assert!(prompt.len() <= PROMPT_MAX_BYTES);
        assert!(prompt.contains("Builds and tests run on GitHub only"));
        assert!(prompt.contains("- 039 "), "the newest note stays");
    }

    #[test]
    fn a_policy_is_checked_and_its_lists_tidied() {
        let update = |policy: TreePolicy| ProjectUpdate {
            policy: Some(policy),
            ..Default::default()
        };
        for policy in [
            TreePolicy {
                heavy_command_limit: Some(0),
                ..Default::default()
            },
            TreePolicy {
                heavy_command_limit: Some(HEAVY_COMMAND_LIMIT_MAX + 1),
                ..Default::default()
            },
            TreePolicy {
                writer_limit: Some(crate::orchestrator::MAX_LIVE_SESSIONS as u8 + 1),
                ..Default::default()
            },
            TreePolicy {
                spend_cap_cents: Some(0),
                ..Default::default()
            },
            TreePolicy {
                spend_cap_cents: Some(SPEND_CAP_MAX_CENTS + 1),
                ..Default::default()
            },
            TreePolicy {
                allowed_models: (0..=ALLOWED_MODELS_MAX).map(|n| format!("m{n}")).collect(),
                ..Default::default()
            },
            TreePolicy {
                allowed_models: vec!["x".repeat(ALLOWED_MODEL_MAX_CHARS + 1)],
                ..Default::default()
            },
            TreePolicy {
                spend_warn_pct: 0,
                ..Default::default()
            },
        ] {
            assert!(update(policy.clone()).validated().is_err(), "{policy:?}");
        }
        let tidy = TreePolicy {
            allowed_models: vec![
                " gpt-5.6 ".into(),
                "GPT-5.6".into(),
                " ".into(),
                "o4".into(),
            ],
            ..Default::default()
        }
        .validated()
        .unwrap();
        assert_eq!(tidy.allowed_models, ["gpt-5.6", "o4"]);
    }

    #[test]
    fn the_allow_list_matches_bare_ids_and_provider_ids() {
        let policy = TreePolicy {
            allowed_models: vec!["gpt-5.6".into(), "anthropic/claude-sonnet-5".into()],
            ..Default::default()
        };
        assert!(policy.allows_model("gpt-5.6"));
        assert!(
            policy.allows_model("OpenAI/GPT-5.6"),
            "a bare entry spans providers"
        );
        assert!(policy.allows_model("anthropic/claude-sonnet-5"));
        assert!(
            policy.allows_model("claude-sonnet-5"),
            "a bare id, as list_models offers it, is the entry's model"
        );
        assert!(
            policy.allows_model("gpt-5.6:high"),
            "a thinking level is not the model"
        );
        assert!(
            !policy.allows_model("other/claude-sonnet-5"),
            "another provider's"
        );
        assert!(!policy.allows_model("o4"));
        assert!(TreePolicy::default().allows_model("anything"));
    }

    #[test]
    fn changing_the_cap_starts_a_fresh_budget_and_other_edits_keep_the_count() {
        let mut project = policy_project(TreePolicy::default());
        let set = |cap: Option<u64>, writers: Option<u8>| ProjectUpdate {
            policy: Some(TreePolicy {
                spend_cap_cents: cap,
                writer_limit: writers,
                // The caller's own value is ignored.
                spend_since_ms: 42,
                ..Default::default()
            }),
            ..Default::default()
        };
        set(Some(1_000), None).apply(&mut project);
        assert!(
            project.policy.spend_since_ms > 42,
            "set by Zeron, not by the caller"
        );
        project.policy.spend_since_ms = 7;
        set(Some(1_000), Some(2)).apply(&mut project);
        assert_eq!(project.policy.writer_limit, Some(2));
        assert_eq!(
            project.policy.spend_since_ms, 7,
            "the same cap keeps counting"
        );
        set(Some(5_000), Some(2)).apply(&mut project);
        assert!(project.policy.spend_since_ms > 7, "a new cap starts again");
        // An update without a policy leaves it be.
        ProjectUpdate {
            goal: Some("g".into()),
            ..Default::default()
        }
        .apply(&mut project);
        assert_eq!(project.policy.spend_cap_cents, Some(5_000));
    }

    #[test]
    fn a_resume_clears_the_reason_a_pause_gave() {
        let mut project = policy_project(TreePolicy::default());
        ProjectUpdate {
            paused: Some(true),
            pause_reason: Some("  spending\ncap reached ".into()),
            ..Default::default()
        }
        .validated()
        .unwrap()
        .apply(&mut project);
        assert!(project.paused);
        assert_eq!(project.pause_reason, "spending cap reached");
        ProjectUpdate {
            paused: Some(false),
            ..Default::default()
        }
        .apply(&mut project);
        assert!(!project.paused && project.pause_reason.is_empty());
    }
}
