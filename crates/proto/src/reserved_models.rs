//! Models no agent may start a child on.
//!
//! Claude Fable and GPT Astra class models, any version, current or future,
//! are for sessions the user starts. A sub-agent, a delegated session, a
//! workflow agent, an advisor or reviewer child, or any session an
//! orchestrator or another agent launches must run something else. The user
//! can still pick these models for their own sessions and btw side chats.
//!
//! A family matches as a whole word of the model's spec or name, case
//! insensitive, with a version or suffix allowed after it:
//! `anthropic/claude-fable-5`, `claude-fable5`, `gpt-6-astra-mini`,
//! `Claude Fable 7` — not `astral-7b`, `fables-model` or `pastra`. Pi keeps
//! the same rule in `agent/extensions/lib/reserved-models.ts`.

pub const RESERVED_FAMILIES: [&str; 2] = ["fable", "astra"];

/// The reserved family `model` belongs to, if any: the family is preceded by
/// the start or a non-letter and not followed by a letter.
pub fn reserved_family(model: &str) -> Option<&'static str> {
    let lower = model.to_ascii_lowercase();
    let bytes = lower.as_bytes();
    RESERVED_FAMILIES.into_iter().find(|family| {
        lower.match_indices(family).any(|(at, _)| {
            let before = at.checked_sub(1).map(|i| bytes[i]);
            let after = bytes.get(at + family.len());
            !before.is_some_and(|b| b.is_ascii_alphabetic())
                && !after.is_some_and(|b| b.is_ascii_alphabetic())
        })
    })
}

/// Why `surface` cannot start a session on `model`.
pub fn refusal(model: &str, surface: &str) -> String {
    format!(
        "{surface} cannot start on {model}: Claude Fable and GPT Astra class models are reserved for sessions the user starts, never for a child an agent launches. Pass a different model, e.g. an Opus, Sonnet or GPT model."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_family_matches_as_a_whole_word_with_a_version_after_it() {
        for (model, family) in [
            ("anthropic/claude-fable-5", "fable"),
            ("claude-fable5", "fable"),
            ("fable", "fable"),
            ("gpt-6-astra-mini", "astra"),
            ("Claude Fable 7", "fable"),
            ("astra2", "astra"),
            ("OPENAI/GPT-9-ASTRA", "astra"),
        ] {
            assert_eq!(reserved_family(model), Some(family), "{model}");
        }
    }

    #[test]
    fn other_models_and_lookalikes_are_not_reserved() {
        for model in [
            "anthropic/claude-opus-5",
            "astral-7b",
            "fables-model",
            "pastra",
            "openai/gpt-5.5",
            "",
        ] {
            assert_eq!(reserved_family(model), None, "{model}");
        }
    }

    #[test]
    fn a_later_whole_word_match_counts_after_a_lookalike() {
        assert_eq!(reserved_family("astral/fable-2"), Some("fable"));
        assert_eq!(reserved_family("fables/astra-1"), Some("astra"));
    }

    #[test]
    fn the_refusal_names_the_surface_and_the_model() {
        let text = refusal("anthropic/claude-fable-5", "The advisor");
        assert!(text.starts_with("The advisor cannot start on anthropic/claude-fable-5: "));
        assert!(text.ends_with("Pass a different model, e.g. an Opus, Sonnet or GPT model."));
    }
}
