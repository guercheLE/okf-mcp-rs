//! `.okf/config.toml`: per-vault compiler/provider settings (design doc
//! Q4). Deliberately a separate, small TOML loader rather than folded into
//! `config_manager`'s existing YAML env/CLI/file cascade — that cascade is
//! process-level config (which target API, which transport); this is
//! vault-scoped data that travels with the vault itself.

use std::collections::HashMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

const VAULT_CONFIG_RELATIVE_PATH: &str = ".okf/config.toml";

fn default_temperature() -> f32 {
    0.2
}
fn default_max_tokens() -> u32 {
    4096
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompilerConfig {
    #[serde(default)]
    pub default_model: Option<String>,
    #[serde(default = "default_temperature")]
    pub temperature: f32,
    #[serde(default = "default_max_tokens")]
    pub max_tokens: u32,
}

impl Default for CompilerConfig {
    fn default() -> Self {
        Self {
            default_model: None,
            temperature: default_temperature(),
            max_tokens: default_max_tokens(),
        }
    }
}

/// Where to find a provider's credential/endpoint override — never the
/// secret value itself, only the env var *name* to read it from (or a
/// literal base URL, which isn't secret).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProviderConfig {
    #[serde(default)]
    pub api_key_env: Option<String>,
    #[serde(default)]
    pub base_url: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct OkfVaultConfig {
    #[serde(default)]
    pub compiler: CompilerConfig,
    #[serde(default)]
    pub providers: HashMap<String, ProviderConfig>,
}

/// Returns the default (empty) config when `.okf/config.toml` doesn't exist
/// yet — a vault with no compiler config configured is a normal state
/// (`--model` on every call), not an error.
pub fn load_vault_config(vault_root: &Path) -> anyhow::Result<OkfVaultConfig> {
    let path = vault_root.join(VAULT_CONFIG_RELATIVE_PATH);
    match std::fs::read_to_string(&path) {
        Ok(contents) => Ok(toml::from_str(&contents)?),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(OkfVaultConfig::default()),
        Err(err) => Err(err.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_vault_with_no_config_file_yet_loads_defaults() {
        let vault = tempfile::tempdir().unwrap();
        let config = load_vault_config(vault.path()).unwrap();
        assert!(config.compiler.default_model.is_none());
        assert_eq!(config.compiler.temperature, 0.2);
    }

    #[test]
    fn parses_the_design_docs_example_config() {
        let vault = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(vault.path().join(".okf")).unwrap();
        std::fs::write(
            vault.path().join(".okf/config.toml"),
            r#"
[compiler]
default_model = "anthropic/claude-3-5-sonnet"
temperature = 0.2
max_tokens = 4096

[providers.anthropic]
api_key_env = "ANTHROPIC_API_KEY"

[providers.ollama]
base_url = "http://localhost:11434"
"#,
        )
        .unwrap();

        let config = load_vault_config(vault.path()).unwrap();
        assert_eq!(
            config.compiler.default_model.as_deref(),
            Some("anthropic/claude-3-5-sonnet")
        );
        assert_eq!(
            config.providers["anthropic"].api_key_env.as_deref(),
            Some("ANTHROPIC_API_KEY")
        );
        assert_eq!(
            config.providers["ollama"].base_url.as_deref(),
            Some("http://localhost:11434")
        );
    }

    #[test]
    fn malformed_toml_is_a_clear_error() {
        let vault = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(vault.path().join(".okf")).unwrap();
        std::fs::write(vault.path().join(".okf/config.toml"), "not = [valid").unwrap();
        assert!(load_vault_config(vault.path()).is_err());
    }
}
