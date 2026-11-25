//! Pure/non-interactive logic the wizard's prompts build on — split out of
//! `setup_wizard.rs` so that file is just the `Prompts` seam and the
//! top-level orchestration (`run_setup_wizard[_with]`).

use std::collections::HashMap;

use okf_mcp::compiler::provider;
use okf_mcp::core::config_manager;
use okf_mcp::core::config_schema::{Config, Transport};

/// Reads whatever's already resolvable via the normal config cascade
/// (env vars/local/global/system/install-dir files/built-in defaults —
/// no CLI flags at this call site) to seed prompt defaults on a re-run.
/// Falls back to `Config`'s own serde defaults (the same path
/// `an_entirely_empty_cascade_loads_successfully_with_defaults` proves
/// works) with a printed warning if an existing file is malformed —
/// never hard-fails setup over a bad existing file.
pub(super) fn existing_config_or_defaults() -> Config {
    match config_manager::load_config(serde_json::Map::new()) {
        Ok(config) => config,
        Err(err) => {
            println!(
                "warning: could not read existing configuration ({err}) — using built-in defaults"
            );
            serde_json::from_value(serde_json::json!({})).expect(
                "Config is constructible from an empty map — every field has a serde default",
            )
        }
    }
}

/// The outcome of prompting for one provider's API key: `value` is `None`
/// when the provider structurally needs no key, or the operator left the
/// prompt blank (meaning "this one doesn't need a key" — never treated as
/// an error). `freshly_entered` is `true` only when a new value was typed
/// this run, so callers know whether it still needs saving.
pub(super) struct ProviderKeyResult {
    pub(super) value: Option<String>,
    pub(super) freshly_entered: bool,
}

pub(super) struct KnownProviderChoice {
    pub(super) name: String,
    pub(super) key: ProviderKeyResult,
}

pub(super) struct CustomProviderChoice {
    pub(super) name: String,
    pub(super) base_url: String,
    pub(super) key: ProviderKeyResult,
}

pub(super) fn provider_key_from_input(value: String) -> ProviderKeyResult {
    let trimmed = value.trim().to_string();
    if trimmed.is_empty() {
        ProviderKeyResult {
            value: None,
            freshly_entered: false,
        }
    } else {
        ProviderKeyResult {
            value: Some(trimmed),
            freshly_entered: true,
        }
    }
}

/// Pure validation for a candidate custom-provider name — non-empty, no
/// `/` (would break `parse_model_spec`'s split), and not already taken by
/// a known provider or one added earlier this run.
pub(super) fn validate_custom_provider_name(
    input: &str,
    taken: &[String],
) -> Result<(), &'static str> {
    let input = input.trim();
    if input.is_empty() {
        Err("name can't be empty")
    } else if input.contains('/') {
        Err("name can't contain '/'")
    } else if taken.iter().any(|t| t == input) {
        Err("name already in use")
    } else {
        Ok(())
    }
}

/// Which indices into `names` should be pre-selected in the known-provider
/// multi-select — those already having a saved credential, per
/// `already_configured`. Takes a closure rather than calling
/// `credential_exists` directly so this decision is testable without
/// touching the real OS keychain.
pub(super) fn provider_default_indices(
    names: &[&str],
    mut already_configured: impl FnMut(&str) -> bool,
) -> Vec<usize> {
    names
        .iter()
        .enumerate()
        .filter(|(_, name)| already_configured(name))
        .map(|(index, _)| index)
        .collect()
}

/// One secret whose real value may, on explicit per-secret opt-in, appear
/// in the `.env` file/printed MCP client config this wizard produces —
/// otherwise it's keychain-only.
pub(super) struct RevealableSecret {
    pub(super) label: String,
    pub(super) env_key: String,
    pub(super) value: String,
}

/// Builds the list of secrets this run knows about (the Firecrawl PAT plus
/// every provider key that resolved to a real value) — pure given
/// already-resolved credentials/provider choices, so testable without any
/// prompting or I/O beyond the (also pure) provider-spec lookups.
pub(super) fn collect_revealable_secrets(
    credentials: &HashMap<String, String>,
    known_providers: &[KnownProviderChoice],
    custom_providers: &[CustomProviderChoice],
) -> anyhow::Result<Vec<RevealableSecret>> {
    let mut secrets = Vec::new();
    if let Some(token) = credentials.get("token") {
        secrets.push(RevealableSecret {
            label: "the Firecrawl personal access token".to_string(),
            env_key: "OKF_MCP_FIRECRAWL_API_PAT".to_string(),
            value: token.clone(),
        });
    }
    for provider in known_providers {
        if let Some(value) = &provider.key.value {
            let env_key = provider::provider_spec(&provider.name)?
                .api_key_env
                .expect("a known provider with a resolved key must declare an api_key_env")
                .to_string();
            secrets.push(RevealableSecret {
                label: format!("the {} API key", provider.name),
                env_key,
                value: value.clone(),
            });
        }
    }
    for provider in custom_providers {
        if let Some(value) = &provider.key.value {
            secrets.push(RevealableSecret {
                label: format!("the {} API key", provider.name),
                env_key: provider::synthetic_api_key_env_name(&provider.name),
                value: value.clone(),
            });
        }
    }
    Ok(secrets)
}

/// Inserts each secret's real value into `env` only if the operator opted
/// into revealing it — everything else stays keychain-only, absent from
/// `env` entirely.
pub(super) fn apply_reveals(
    env: &mut HashMap<String, String>,
    secrets: &[RevealableSecret],
    revealed: &std::collections::HashSet<String>,
) {
    for secret in secrets {
        if revealed.contains(&secret.env_key) {
            env.insert(secret.env_key.clone(), secret.value.clone());
        }
    }
}

/// Merges every custom provider's name+endpoint (never the API key) into
/// `env` (as `OKF_MCP_CUSTOM_PROVIDER_<NAME>_BASE_URL`, for `.env`
/// persistence) and `config_fields` (under `"custom_providers"`, for
/// config.yml persistence) — not secret, no reveal gating needed.
pub(super) fn merge_custom_providers(
    env: &mut HashMap<String, String>,
    config_fields: &mut serde_json::Value,
    custom: &[CustomProviderChoice],
) {
    if custom.is_empty() {
        return;
    }
    let mut providers = serde_json::Map::new();
    for provider in custom {
        env.insert(
            config_manager::custom_provider_base_url_env_name(&provider.name),
            provider.base_url.clone(),
        );
        providers.insert(
            provider.name.clone(),
            serde_json::json!({ "base_url": provider.base_url }),
        );
    }
    if let serde_json::Value::Object(map) = config_fields {
        map.insert(
            "custom_providers".to_string(),
            serde_json::Value::Object(providers),
        );
    }
}

pub(super) fn build_runtime_settings(
    url: &str,
    api_version: &str,
    transport: Transport,
) -> anyhow::Result<(HashMap<String, String>, serde_json::Value)> {
    let mut env = HashMap::new();
    env.insert("OKF_MCP_FIRECRAWL_API_URL".to_string(), url.to_string());
    env.insert("OKF_MCP_API_VERSION".to_string(), api_version.to_string());
    env.insert(
        "OKF_MCP_TRANSPORT".to_string(),
        serde_json::to_value(transport)?
            .as_str()
            .unwrap_or_default()
            .to_string(),
    );

    let config_fields = serde_json::json!({
        "firecrawl_base_url": env.get("OKF_MCP_FIRECRAWL_API_URL"),
        "api_version": env.get("OKF_MCP_API_VERSION"),
        "transport": env.get("OKF_MCP_TRANSPORT"),
    });
    Ok((env, config_fields))
}
