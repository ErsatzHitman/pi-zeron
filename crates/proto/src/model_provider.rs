//! Who serves a model: the provider the model picker groups by.
//!
//! Pi and OpenCode catalogs span providers and name each model's provider in
//! [`Model::description`]: a new engine sends Pi's own display name ("OpenCode
//! Go"), an older one the provider id ("opencode-go"), and OpenCode its own
//! name ("Anthropic"). [`resolve`] folds all three onto one key, so a
//! provider's models group together whichever harness offers them, wherever
//! the provider was signed in (Pi's `/login`, Settings → API keys, an
//! environment variable). A provider nobody lists here still groups under
//! its own id and name, so one added or signed in later needs no change.
//! Harnesses tied to one provider name it from the harness; the few that
//! span providers without saying which infer it from the model's family.

use crate::{HarnessId, Model};

/// A model's provider: `key` groups, `name` is shown.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ModelProvider {
    pub key: String,
    pub name: String,
}

/// Pi's built-in providers, as pi-ai registers them
/// (`createProvider({id, name})`), plus the providers other harnesses
/// name that Pi lacks. Order is irrelevant.
const KNOWN: &[(&str, &str)] = &[
    ("amazon-bedrock", "Amazon Bedrock"),
    ("ant-ling", "Ant Ling"),
    ("anthropic", "Anthropic"),
    ("azure", "Azure"),
    ("baseten", "Baseten"),
    ("cerebras", "Cerebras"),
    ("cloudflare-ai-gateway", "Cloudflare AI Gateway"),
    ("cloudflare-workers-ai", "Cloudflare Workers AI"),
    ("deepseek", "DeepSeek"),
    ("fireworks", "Fireworks"),
    ("github-copilot", "GitHub Copilot"),
    ("google", "Google"),
    ("google-vertex", "Google Vertex AI"),
    ("groq", "Groq"),
    ("huggingface", "Hugging Face"),
    ("kimi-coding", "Kimi For Coding"),
    ("meta", "Meta"),
    ("minimax", "MiniMax"),
    ("minimax-cn", "MiniMax CN"),
    ("mistral", "Mistral"),
    ("moonshotai", "Moonshot AI"),
    ("moonshotai-cn", "Moonshot AI CN"),
    ("nvidia", "NVIDIA"),
    ("openai", "OpenAI"),
    ("openai-codex", "OpenAI Codex (legacy)"),
    ("opencode", "OpenCode Zen"),
    ("opencode-go", "OpenCode Go"),
    ("openrouter", "OpenRouter"),
    ("qwen", "Qwen"),
    ("qwen-token-plan", "Qwen Token Plan"),
    ("qwen-token-plan-cn", "Qwen Token Plan CN"),
    ("qwen-token-plan-individual", "Qwen Token Plan Individual"),
    ("together", "Together"),
    ("typesafe", "TypeSafe"),
    ("vercel-ai-gateway", "Vercel AI Gateway"),
    ("xai", "xAI"),
    ("xiaomi", "Xiaomi"),
    ("xiaomi-token-plan-ams", "Xiaomi Token Plan AMS"),
    ("xiaomi-token-plan-cn", "Xiaomi Token Plan CN"),
    ("xiaomi-token-plan-sgp", "Xiaomi Token Plan SGP"),
    ("zai", "Z.AI"),
    ("zai-coding-cn", "Z.AI Coding CN"),
];

/// Other spellings of a known provider (OpenCode's and models.dev's names).
const ALIASES: &[(&str, &str)] = &[
    ("opencode zen", "opencode"),
    ("zhipu ai", "zai"),
    ("z.ai", "zai"),
    ("moonshot", "moonshotai"),
    ("x.ai", "xai"),
    ("vertex", "google-vertex"),
    ("google vertex", "google-vertex"),
    ("bedrock", "amazon-bedrock"),
    ("aws bedrock", "amazon-bedrock"),
];

/// The display name for a provider id: Pi's name for a provider it ships,
/// else the id made readable ("my-gateway" → "My Gateway").
pub fn display_name(id: &str) -> String {
    let id = id.trim();
    KNOWN
        .iter()
        .find(|(known, _)| known.eq_ignore_ascii_case(id))
        .map(|(_, name)| (*name).to_owned())
        .unwrap_or_else(|| humanize(id))
}

/// The provider a provider id or display name names. Known providers match
/// by id, name or alias; anything else keys on its slug and keeps its own
/// name (an id-shaped one made readable). `None` for blank text.
pub fn resolve(text: &str) -> Option<ModelProvider> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    let lower = text.to_lowercase();
    let alias = ALIASES
        .iter()
        .find(|(alias, _)| *alias == lower)
        .map(|(_, id)| *id);
    let known = KNOWN
        .iter()
        .find(|(id, name)| alias == Some(*id) || *id == lower || name.eq_ignore_ascii_case(text));
    Some(match known {
        Some((id, name)) => ModelProvider {
            key: (*id).to_owned(),
            name: (*name).to_owned(),
        },
        None => ModelProvider {
            key: slug(text),
            name: if is_id_shaped(text) {
                humanize(text)
            } else {
                text.to_owned()
            },
        },
    })
}

/// Whether a harness's models all come from one provider it is named for.
pub fn single_provider(harness: HarnessId) -> Option<&'static str> {
    match harness {
        HarnessId::ClaudeCode | HarnessId::Mock => Some("anthropic"),
        HarnessId::Codex => Some("openai"),
        HarnessId::Grok => Some("xai"),
        HarnessId::Antigravity => Some("google"),
        _ => None,
    }
}

/// The provider serving `model` on `harness`, `None` when nothing names one
/// (Pi's "default" stand-in, a Cursor model of Cursor's own).
pub fn of_model(harness: HarnessId, model: &Model) -> Option<ModelProvider> {
    if let Some(id) = single_provider(harness) {
        return resolve(id);
    }
    match harness {
        // The catalog names the provider; a row without one (an older or
        // hand-made list) still carries it as its id's `provider/` prefix.
        HarnessId::Pi | HarnessId::Opencode => {
            if harness == HarnessId::Pi && model.id == "default" {
                return None;
            }
            model
                .description
                .as_deref()
                .and_then(resolve)
                .or_else(|| of_id(&model.id))
        }
        _ => from_family(&model.id).or_else(|| from_family(&model.label)),
    }
}

/// A `provider/model` id's provider.
pub fn of_id(id: &str) -> Option<ModelProvider> {
    id.split_once('/')
        .and_then(|(provider, _)| resolve(provider))
}

/// The lab behind a model family, for harnesses that span providers without
/// naming them.
fn from_family(id: &str) -> Option<ModelProvider> {
    let base = id.rsplit('/').next().unwrap_or(id).trim().to_lowercase();
    let starts = |prefix: &str| {
        base.strip_prefix(prefix)
            .is_some_and(|rest| rest.is_empty() || !rest.starts_with(char::is_alphabetic))
    };
    let provider = if ["claude", "opus", "sonnet", "haiku", "fable"]
        .iter()
        .any(|p| starts(*p))
    {
        "anthropic"
    } else if ["gpt", "o1", "o3", "o4", "codex", "chatgpt"]
        .iter()
        .any(|p| starts(*p))
    {
        "openai"
    } else if ["gemini", "gemma"].iter().any(|p| starts(*p)) {
        "google"
    } else if starts("grok") {
        "xai"
    } else if starts("deepseek") {
        "deepseek"
    } else if ["kimi", "moonshot"].iter().any(|p| starts(*p)) {
        "moonshotai"
    } else if starts("glm") {
        "zai"
    } else if ["qwen", "qwq"].iter().any(|p| starts(*p)) {
        "qwen"
    } else if ["mistral", "devstral", "codestral", "magistral", "ministral"]
        .iter()
        .any(|p| starts(*p))
    {
        "mistral"
    } else if starts("llama") {
        "meta"
    } else if starts("minimax") {
        "minimax"
    } else {
        return None;
    };
    resolve(provider)
}

fn is_id_shaped(text: &str) -> bool {
    !text.contains(' ') && !text.chars().any(char::is_uppercase)
}

fn slug(text: &str) -> String {
    text.trim()
        .to_lowercase()
        .split(|c: char| c.is_whitespace() || c == '_')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("-")
}

fn humanize(id: &str) -> String {
    id.split(['-', '_', ' '])
        .filter(|part| !part.is_empty())
        .map(|part| {
            let mut chars = part.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().chain(chars).collect(),
                None => String::new(),
            }
        })
        .collect::<Vec<String>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model(id: &str, description: Option<&str>) -> Model {
        Model {
            id: id.into(),
            label: id.into(),
            description: description.map(Into::into),
            reasoning_levels: vec![],
            options: vec![],
        }
    }

    fn key(provider: Option<ModelProvider>) -> Option<String> {
        provider.map(|p| p.key)
    }

    #[test]
    fn ids_names_and_aliases_fold_onto_one_provider() {
        for text in ["opencode-go", "OpenCode Go", " opencode-go "] {
            assert_eq!(
                resolve(text),
                Some(ModelProvider {
                    key: "opencode-go".into(),
                    name: "OpenCode Go".into()
                })
            );
        }
        assert_eq!(key(resolve("OpenCode Zen")), Some("opencode".into()));
        assert_eq!(key(resolve("Anthropic")), Some("anthropic".into()));
        assert_eq!(key(resolve("Z.AI")), Some("zai".into()));
        assert_eq!(resolve(""), None);
        assert_eq!(display_name("openai"), "OpenAI");
    }

    #[test]
    fn unknown_providers_group_under_their_own_name() {
        assert_eq!(
            resolve("my-gateway"),
            Some(ModelProvider {
                key: "my-gateway".into(),
                name: "My Gateway".into()
            })
        );
        assert_eq!(
            resolve("Acme Cloud"),
            Some(ModelProvider {
                key: "acme-cloud".into(),
                name: "Acme Cloud".into()
            })
        );
        assert_eq!(display_name("clinepass"), "Clinepass");
    }

    #[test]
    fn each_harness_names_its_models_provider() {
        let pi = |id, d| key(of_model(HarnessId::Pi, &model(id, d)));
        assert_eq!(
            pi("anthropic/claude-opus-5-5", Some("Anthropic")),
            Some("anthropic".into())
        );
        assert_eq!(
            pi("opencode-go/glm-5.3", Some("opencode-go")),
            Some("opencode-go".into())
        );
        // No description: the id's prefix names it.
        assert_eq!(pi("openrouter/x", None), Some("openrouter".into()));
        assert_eq!(pi("default", Some("Model configured in Pi")), None);
        let claude = model("opus", Some("Most capable"));
        assert_eq!(
            key(of_model(HarnessId::ClaudeCode, &claude)),
            Some("anthropic".into())
        );
        assert_eq!(
            key(of_model(HarnessId::Codex, &model("gpt-5.5", None))),
            Some("openai".into())
        );
        let cursor = |id| key(of_model(HarnessId::Cursor, &model(id, None)));
        assert_eq!(cursor("claude-4.5-sonnet"), Some("anthropic".into()));
        assert_eq!(cursor("gpt-5"), Some("openai".into()));
        assert_eq!(cursor("o3"), Some("openai".into()));
        assert_eq!(cursor("gemini-2.5-pro"), Some("google".into()));
        assert_eq!(cursor("composer-1"), None);
        assert_eq!(cursor("opusculum"), None);
    }
}
