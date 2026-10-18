//! Isolated one-shot model runs: a throwaway, tool-less call on the cheap side
//! model that never touches a repository or a coding session. Chat titles and
//! recaps both go through here; each supplies its own instructions, prompt,
//! cleaning and usage label.

use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;

use zeron_harness::{CancellationToken, RunControls, SteerMessage};
use zeron_proto::{
    AgentEvent, DoneStatus, HarnessId, Model, ReasoningLevel, RunRequest, SandboxLevel,
    UserInputAnswer, UserInputQuestion,
};

use crate::EngineError;
use crate::registry::HarnessRegistry;

/// One isolated run: what to ask, who may answer, how to read the reply.
pub(crate) struct IsolatedSpec {
    /// The harness to try first when the Thread-naming preference names none
    /// (usually the harness that ran the chat).
    pub preferred: HarnessId,
    /// Which drivers can honour `instructions` (`supports_titles`,
    /// `supports_recaps`).
    pub supports: fn(HarnessId) -> bool,
    pub instructions: &'static str,
    /// Usage-ledger label of the call.
    pub usage_surface: &'static str,
    /// The whole user input, already framed as quoted data.
    pub prompt: String,
    pub timeout: Duration,
    pub retry_delays_ms: &'static [u64],
    /// What the run is for ("title", "recap"), used in error text.
    pub noun: &'static str,
    /// Reply -> final text; an empty result counts as a failed attempt.
    pub clean: fn(&str) -> String,
}

#[derive(Clone)]
pub(crate) struct IsolatedRunner {
    registry: Arc<HarnessRegistry>,
}

impl IsolatedRunner {
    pub(crate) fn new(registry: Arc<HarnessRegistry>) -> Self {
        Self { registry }
    }

    /// The cleaned reply, or `None` when no harness can serve the run or every
    /// attempt failed. The Thread-naming harness and model apply when that
    /// harness can serve this run; otherwise the preferred harness (else any
    /// enabled one that can) runs on its cheapest model.
    pub(crate) async fn run(&self, spec: IsolatedSpec) -> Option<String> {
        let settings = self.registry.title_settings();
        let enabled = self.registry.enabled_set();
        let (harness_id, configured_model) =
            match settings.harness.filter(|id| (spec.supports)(*id)) {
                Some(id) => (id, settings.model),
                None => {
                    let id = if (spec.supports)(spec.preferred) {
                        spec.preferred
                    } else {
                        enabled.iter().copied().find(|id| (spec.supports)(*id))?
                    };
                    (id, None)
                }
            };
        // Order this entire isolated subprocess against a queued update for
        // the same CLI. The fair registry gate prevents late side work from
        // jumping ahead of an accepted writer.
        let execution_lease = Arc::new(self.registry.execution_lease(harness_id).await);
        // No repository instructions, files, or active coding-session context.
        let scratch = tempfile::tempdir().ok()?;
        let harness = match self.registry.resolve(harness_id) {
            Ok(harness) => harness,
            Err(err) => {
                tracing::debug!(error = %err, noun = spec.noun, "isolated harness unavailable");
                return None;
            }
        };
        let model = match configured_model {
            Some(model) => Some(model),
            None => cheapest_model(
                &tokio::time::timeout(
                    Duration::from_secs(10),
                    self.registry
                        .discover_models_with_lease(harness_id, execution_lease.clone()),
                )
                .await
                .ok()?
                .unwrap_or_default(),
            ),
        };
        for attempt in 0..=spec.retry_delays_ms.len() {
            let request = RunRequest {
                mcp: None,
                prompt: spec.prompt.clone(),
                harness: Some(harness_id),
                model: model.clone(),
                reasoning: Some(ReasoningLevel::Minimal),
                model_options: serde_json::Map::new(),
                cwd: scratch.path().to_string_lossy().into_owned(),
                sandbox: SandboxLevel::ReadOnly,
                auto_approve: false,
                attachments: Vec::new(),
                resume: None,
                worktree: None,
            };
            match tokio::time::timeout(
                spec.timeout,
                collect_text(
                    harness.as_ref(),
                    request,
                    Some(execution_lease.clone()),
                    spec.instructions,
                    spec.usage_surface,
                    spec.noun,
                ),
            )
            .await
            .unwrap_or_else(|_| {
                Err(EngineError::Other(format!(
                    "{} generation timed out",
                    spec.noun
                )))
            }) {
                Ok(raw) => {
                    let candidate = (spec.clean)(&raw);
                    if !candidate.is_empty() {
                        return Some(candidate);
                    }
                }
                Err(err) => {
                    tracing::warn!(attempt = attempt + 1, error = %err, noun = spec.noun,
                        "isolated model run attempt failed");
                }
            }
            if let Some(delay) = spec.retry_delays_ms.get(attempt) {
                tokio::time::sleep(Duration::from_millis(*delay)).await;
            }
        }
        None
    }
}

/// The cheapest model a harness offers (zeron's `cheapestModel` heuristic):
/// prefer a small-tier name (haiku/mini/nano/flash/small/lite), else the last
/// listed model; `None` when the catalog is empty (harness picks its default).
pub(crate) fn cheapest_model(models: &[Model]) -> Option<String> {
    if models.is_empty() {
        return None;
    }
    let small = models.iter().find(|m| {
        let haystack = format!("{} {}", m.id, m.label).to_lowercase();
        ["haiku", "mini", "nano", "flash", "small", "lite"]
            .iter()
            .any(|tier| haystack.contains(tier))
    });
    small.or(models.last()).map(|m| m.id.clone())
}

/// Drive one isolated run through the harness: no steering, questions resolved
/// empty immediately (a side prompt must never block on input).
pub(crate) async fn collect_text(
    harness: &dyn zeron_harness::Harness,
    request: RunRequest,
    execution_lease: Option<Arc<tokio::sync::OwnedRwLockReadGuard<()>>>,
    instructions: &'static str,
    usage_surface: &'static str,
    noun: &str,
) -> Result<String, EngineError> {
    let (steer_tx, steer_rx) = tokio::sync::mpsc::channel::<SteerMessage>(1);
    let interrupt = CancellationToken::new();
    let _cancel_on_drop = interrupt.clone().drop_guard();
    let controls = RunControls {
        realtime: None,
        execution_lease,
        request_input: Box::new(|_questions: Vec<UserInputQuestion>| {
            let (tx, rx) = tokio::sync::oneshot::channel::<Vec<UserInputAnswer>>();
            let _ = tx.send(Vec::new());
            rx
        }),
        steering: steer_rx,
        interrupt: interrupt.clone(),
        turn: Default::default(),
        extension_ui: Default::default(),
    };
    let mut stream = harness
        .run_isolated(request, controls, instructions, usage_surface)
        .await?;
    let mut text = String::new();
    let mut completed = false;
    while let Some(event) = stream.next().await {
        match event? {
            AgentEvent::TextDelta { text: delta } => text.push_str(&delta),
            AgentEvent::ToolCall { .. } => {
                return Err(EngineError::Other(format!(
                    "{noun} generation attempted to use a tool"
                )));
            }
            AgentEvent::Error { message } => {
                return Err(EngineError::Other(format!("{noun} run error: {message}")));
            }
            AgentEvent::Done { status, error, .. } => {
                if status == DoneStatus::Completed {
                    completed = true;
                    break;
                }
                return Err(EngineError::Other(format!(
                    "{noun} run ended {status:?}: {}",
                    error.unwrap_or_default()
                )));
            }
            _ => {}
        }
    }
    drop(steer_tx); // keep the mailbox open for the run's whole lifetime
    if completed {
        Ok(text)
    } else {
        Err(EngineError::Other(format!(
            "{noun} stream ended without completion"
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model(id: &str, label: &str) -> Model {
        Model {
            id: id.into(),
            label: label.into(),
            description: None,
            reasoning_levels: vec![],
            options: vec![],
        }
    }

    #[test]
    fn cheapest_prefers_small_tier_then_last() {
        let models = vec![
            model("opus-4", "Opus"),
            model("haiku-3", "Haiku"),
            model("sonnet-4", "Sonnet"),
        ];
        assert_eq!(cheapest_model(&models).as_deref(), Some("haiku-3"));
        let no_small = vec![model("opus-4", "Opus"), model("sonnet-4", "Sonnet")];
        assert_eq!(cheapest_model(&no_small).as_deref(), Some("sonnet-4"));
        assert_eq!(cheapest_model(&[]), None);
    }

    #[tokio::test]
    async fn tool_use_rejects_the_title_instead_of_accepting_coding_output() {
        let harness = zeron_harness::mock::MockHarness {
            script: vec![
                AgentEvent::TextDelta {
                    text: "I will change your code".into(),
                },
                AgentEvent::ToolCall {
                    id: "tool".into(),
                    call: zeron_proto::ToolCall::Unknown {
                        name: "write".into(),
                        input: None,
                    },
                },
            ],
        };
        let request = RunRequest {
            mcp: None,
            prompt: "Title only".into(),
            harness: None,
            model: None,
            reasoning: None,
            model_options: Default::default(),
            cwd: String::new(),
            sandbox: SandboxLevel::ReadOnly,
            auto_approve: false,
            resume: None,
            attachments: vec![],
            worktree: None,
        };
        let result = collect_text(
            &harness,
            request,
            None,
            zeron_harness::TITLE_INSTRUCTIONS,
            "zeron-titles",
            "title",
        )
        .await;
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("attempted to use a tool")
        );
    }
}
