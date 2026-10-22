//! Credential bytes are hashed locally and never included in diagnostics or disk catalogs.
use crate::{HarnessError, ModelContext};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use zeron_proto::HarnessId;

pub(crate) fn root(variable: &str, fallback: PathBuf) -> PathBuf {
    std::env::var_os(variable)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or(fallback)
}

pub(crate) fn context(
    id: HarnessId,
    binary: &Path,
    extra: &[PathBuf],
) -> Result<ModelContext, HarnessError> {
    let home = crate::executable::home_or_current_dir();
    let files = match id {
        HarnessId::Codex => vec![root("CODEX_HOME", home.join(".codex")).join("auth.json")],
        HarnessId::ClaudeCode => {
            let root = root("CLAUDE_CONFIG_DIR", home.join(".claude"));
            vec![root.join("settings.json"), root.join(".credentials.json")]
        }
        HarnessId::Cursor => vec![home.join(".cursor/sdk/auth.json")],
        HarnessId::Opencode => {
            let data = root("XDG_DATA_HOME", home.join(".local/share"));
            let config = root("XDG_CONFIG_HOME", home.join(".config"));
            let cwd = std::env::current_dir()?;
            let mut files = vec![
                data.join("opencode/auth.json"),
                config.join("opencode/opencode.json"),
                config.join("opencode/opencode.jsonc"),
            ];
            for dir in cwd.ancestors() {
                for name in [
                    "opencode.json",
                    "opencode.jsonc",
                    ".opencode/opencode.json",
                    ".opencode/opencode.jsonc",
                ] {
                    files.push(dir.join(name));
                }
            }
            if let Some(path) = std::env::var_os("OPENCODE_CONFIG") {
                files.push(path.into());
            }
            files
        }
        HarnessId::Grok => {
            let root = root("GROK_HOME", home.join(".grok"));
            vec![root.join("auth.json"), root.join("config.toml")]
        }
        HarnessId::Hermes => {
            let root = root("HERMES_HOME", home.join(".hermes"));
            vec![
                root.join("auth.json"),
                root.join(".env"),
                root.join("config.yaml"),
            ]
        }
        HarnessId::Pi => {
            vec![root("PI_CODING_AGENT_DIR", home.join(".pi/agent")).join("auth.json")]
        }
        HarnessId::Devin => {
            let data = root("XDG_DATA_HOME", home.join(".local/share"));
            let mut files = vec![data.join("devin/credentials.toml")];
            if cfg!(target_os = "macos") {
                files.push(home.join("Library/Application Support/devin/credentials.toml"));
            }
            if cfg!(windows) {
                files.push(
                    root("APPDATA", home.join("AppData/Roaming")).join("devin/credentials.toml"),
                );
            }
            files
        }
        _ => vec![],
    };
    let binary = binary
        .canonicalize()
        .unwrap_or_else(|_| binary.to_path_buf());
    let version = crate::executable::binary_version(&binary).map(|v| v.to_string());
    let mut hash = Sha256::new();
    field(&mut hash, binary.as_os_str().as_encoded_bytes());
    field(
        &mut hash,
        version.as_deref().unwrap_or("unknown").as_bytes(),
    );
    // Unknown-version executables must still invalidate on replacement.
    if let Ok(metadata) = binary.metadata() {
        field(
            &mut hash,
            format!("{:?}:{}", metadata.modified().ok(), metadata.len()).as_bytes(),
        );
    }
    hash_files(&mut hash, files.iter().chain(extra))?;
    let prefixes: &[&str] = match id {
        HarnessId::Codex => &["CODEX_", "OPENAI_"],
        HarnessId::ClaudeCode => &["CLAUDE_", "ANTHROPIC_", "AWS_"],
        HarnessId::Opencode => &["OPENCODE_", "OPENAI_", "ANTHROPIC_", "GOOGLE_"],
        HarnessId::Grok => &["GROK_", "XAI_"],
        HarnessId::Hermes => &["HERMES_", "OPENAI_", "ANTHROPIC_"],
        HarnessId::Pi => &["PI_", "OPENAI_", "ANTHROPIC_"],
        HarnessId::Devin => &["DEVIN_"],
        HarnessId::Antigravity => &["GEMINI_", "GOOGLE_"],
        HarnessId::Cursor => &["CURSOR_"],
        _ => &[],
    };
    let mut env: Vec<_> = std::env::vars_os()
        .filter(|(key, _)| {
            prefixes
                .iter()
                .any(|prefix| key.to_string_lossy().starts_with(prefix))
        })
        .collect();
    env.sort();
    for (key, value) in env {
        field(&mut hash, key.as_encoded_bytes());
        field(&mut hash, value.as_encoded_bytes());
    }
    Ok(ModelContext {
        hash: format!("{:x}", hash.finalize()),
        binary_path: binary,
        binary_version: version,
    })
}

fn field(hash: &mut Sha256, bytes: &[u8]) {
    hash.update((bytes.len() as u64).to_le_bytes());
    hash.update(bytes);
}

/// Fields that change without a different account: OAuth access/refresh
/// tokens and their expiry. An API key (`key`, `apiKey`) is deliberately not
/// here — replacing it is an account change that must invalidate the
/// catalog. A refreshed token is the same account, and hashing it minted a
/// new disk catalog (and a new file) on every rotation.
const VOLATILE_FIELDS: &[&str] = &[
    "access",
    "refresh",
    "accesstoken",
    "refreshtoken",
    "idtoken",
    "expires",
    "expiresat",
    "expiresin",
    "expiry",
];

fn volatile_field(key: &str) -> bool {
    let key: String = key
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_lowercase())
        .collect();
    VOLATILE_FIELDS.contains(&key.as_str())
}

/// A JSON store with its rotating OAuth fields replaced by a placeholder.
/// Anything that is not a JSON object/array is returned unchanged, and a
/// non-JSON file is hashed whole.
fn stable_json(bytes: &[u8]) -> Option<Vec<u8>> {
    let mut value: serde_json::Value = serde_json::from_slice(bytes).ok()?;
    redact_volatile(&mut value, "");
    serde_json::to_vec(&value).ok()
}

fn redact_volatile(value: &mut serde_json::Value, key: &str) {
    if volatile_field(key) {
        *value = serde_json::Value::String("[volatile]".into());
        return;
    }
    match value {
        serde_json::Value::Object(map) => {
            for (name, entry) in map.iter_mut() {
                redact_volatile(entry, name);
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                redact_volatile(item, key);
            }
        }
        _ => {}
    }
}

fn hash_files<'a>(
    hash: &mut Sha256,
    files: impl Iterator<Item = &'a PathBuf>,
) -> Result<(), HarnessError> {
    for path in files {
        field(hash, path.as_os_str().as_encoded_bytes());
        match std::fs::read(path) {
            Ok(bytes) => {
                hash.update([1]);
                field(hash, stable_json(&bytes).as_deref().unwrap_or(&bytes));
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => hash.update([0]),
            // A credential file another process is rewriting (a Windows
            // sharing violation, a lock) must not hide the whole model list:
            // hash it as one stable "unreadable" marker, distinct from
            // "missing", so the same state hashes the same on every poll
            // instead of churning the catalog on each error variant.
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "model context file unreadable");
                hash.update([2]);
            }
        }
    }
    Ok(())
}

impl ModelContext {
    pub(crate) fn key(&self) -> [u8; 32] {
        Sha256::digest(self.hash.as_bytes()).into()
    }
    pub(crate) fn log(&self) {
        tracing::info!(binary_path = %self.binary_path.display(), binary_version = self.binary_version.as_deref().unwrap_or("unknown"), "Model discovery binary");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn content_changes_and_missing_files_invalidate_without_exposing_secrets() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("auth.json");
        let key = || {
            let mut hash = Sha256::new();
            hash_files(&mut hash, std::iter::once(&file)).unwrap();
            format!("{:x}", hash.finalize())
        };
        let missing = key();
        std::fs::write(&file, "account-one").unwrap();
        let first = key();
        std::fs::write(&file, "account-two").unwrap();
        assert_ne!(first, key());
        assert_ne!(missing, first);
        assert!(!first.contains("account"));
        std::fs::remove_file(&file).unwrap();
        assert_eq!(key(), missing);
    }

    #[test]
    fn a_rotated_oauth_token_keeps_the_context_while_an_account_change_does_not() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("auth.json");
        let key = || {
            let mut hash = Sha256::new();
            hash_files(&mut hash, std::iter::once(&file)).unwrap();
            format!("{:x}", hash.finalize())
        };
        let entry = |token: &str, email: &str| {
            format!(
                r#"{{"anthropic":{{"type":"oauth","access":"{token}","refresh":"r-{token}","expires":1}},"email":"{email}"}}"#
            )
        };
        std::fs::write(&file, entry("tok-1", "a@example.com")).unwrap();
        let first = key();
        std::fs::write(&file, entry("tok-2", "a@example.com")).unwrap();
        assert_eq!(first, key(), "a refreshed token is the same account");
        std::fs::write(&file, entry("tok-2", "other@example.com")).unwrap();
        assert_ne!(first, key(), "a different account invalidates");
        // An API key is an account, not a rotating token.
        let api =
            |secret: &str| format!(r#"{{"anthropic":{{"type":"api_key","key":"{secret}"}}}}"#);
        std::fs::write(&file, api("sk-one")).unwrap();
        let keyed = key();
        std::fs::write(&file, api("sk-two")).unwrap();
        assert_ne!(keyed, key(), "a replaced API key invalidates");
    }

    #[test]
    fn a_rotated_token_keeps_the_whole_pi_context_while_an_account_change_does_not() {
        let dir = tempfile::tempdir().unwrap();
        let auth = dir.path().join("auth.json");
        // The binary does not exist, so it contributes only its path; the temp
        // `auth.json` rides the `extra` files, so no environment is touched.
        let binary = dir.path().join("pi");
        let context_hash = || {
            context(
                zeron_proto::HarnessId::Pi,
                &binary,
                std::slice::from_ref(&auth),
            )
            .unwrap()
            .hash
        };
        // `access`, `accessToken`, `refresh` and `expires` rotate together on
        // refresh and are all masked; `email` is the account.
        let entry = |token: &str, email: &str| {
            format!(
                r#"{{"anthropic":{{"type":"oauth","access":"{token}","accessToken":"{token}","refresh":"r-{token}","expires":1}},"email":"{email}"}}"#
            )
        };
        std::fs::write(&auth, entry("tok-1", "a@example.com")).unwrap();
        let first = context_hash();
        std::fs::write(&auth, entry("tok-2", "a@example.com")).unwrap();
        assert_eq!(first, context_hash(), "a refreshed token is the same account");
        std::fs::write(&auth, entry("tok-2", "other@example.com")).unwrap();
        assert_ne!(first, context_hash(), "a different account invalidates");
        // A non-JSON credential file falls back to raw bytes, never to a
        // shared empty hash.
        std::fs::write(&auth, "not json").unwrap();
        let raw = context_hash();
        std::fs::write(&auth, "not json either").unwrap();
        assert_ne!(raw, context_hash(), "a non-JSON file is hashed whole");
    }
}
