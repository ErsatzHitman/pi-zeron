//! Who serves a Pi model: the provider the model picker groups by.
//!
//! Pi's catalog spans providers and names each model's provider in
//! [`Model::description`]: a new engine sends Pi's own display name ("OpenCode
//! Go"), an older one the provider id ("opencode-go"); the model id carries
//! the id too (`opencode-go/glm-5.3`). [`resolve`] folds both onto one key,
//! wherever the provider was signed in (Pi's `/login`, Settings → API keys,
//! an environment variable). A provider not listed here still groups under
//! its own id and name, so one added or signed in later needs no change.

use crate::Model;

/// A model's provider: `key` groups, `name` is shown.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ModelProvider {
    pub key: String,
    pub name: String,
}

/// Pi's built-in providers, as pi-ai registers them
/// (`createProvider({id, name})`). Order is irrelevant.
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
/// by id or name; anything else keys on its slug and keeps its own name (an
/// id-shaped one made readable). `None` for blank text.
pub fn resolve(text: &str) -> Option<ModelProvider> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    let lower = text.to_lowercase();
    let known = KNOWN
        .iter()
        .find(|(id, name)| *id == lower || name.eq_ignore_ascii_case(text));
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

/// The provider serving a Pi model, `None` for Pi's "default" stand-in. A
/// model without a description (an older or hand-made list) still names it
/// as its id's `provider/` prefix.
pub fn of_model(model: &Model) -> Option<ModelProvider> {
    if model.id == "default" {
        return None;
    }
    model
        .description
        .as_deref()
        .and_then(resolve)
        .or_else(|| of_id(&model.id))
}

/// A `provider/model` id's provider.
pub fn of_id(id: &str) -> Option<ModelProvider> {
    id.split_once('/')
        .and_then(|(provider, _)| resolve(provider))
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
    fn ids_and_names_fold_onto_one_provider() {
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
    fn a_pi_model_names_its_provider() {
        let pi = |id, d| key(of_model(&model(id, d)));
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
    }
}
