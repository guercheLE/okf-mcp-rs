//! Resolving which model/provider (and its options) a vault wants to use —
//! split out of `driver.rs` so that file is just the orchestration
//! (`compile`), not every stage's own logic.

use std::path::Path;

use crate::compiler::provider::{LLMCompilerDriver, parse_model_spec};
use crate::core::vault_config::load_vault_config;

use super::CompileOptions;

/// `--model`/MCP `model` -> `.okf/config.toml`'s `[compiler].default_model`
/// -> error. No "guess from whichever env var is set" fallback (see
/// `compiler::provider`'s top comment for why).
pub fn resolve_model_spec(vault_root: &Path, explicit: Option<&str>) -> anyhow::Result<String> {
    if let Some(spec) = explicit {
        return Ok(spec.to_string());
    }
    let config = load_vault_config(vault_root)?;
    config.compiler.default_model.ok_or_else(|| {
        anyhow::anyhow!(
            "no model specified — pass --model <provider>/<model>, or set \
             [compiler].default_model in .okf/config.toml"
        )
    })
}

/// Resolves `.okf/config.toml`'s `[providers.<provider>]` entry (if any)
/// for `model_spec`'s provider into a `CompileOptions` override — the
/// vault-level counterpart to `--model`/MCP `base_url_override`/etc.
/// `provider_spec`'s own hardcoded env-var names/defaults still apply
/// whenever the vault has no matching `[providers.<name>]` entry (returns
/// `CompileOptions::default()` in that case, today's exact behavior).
pub fn vault_provider_options(
    vault_root: &Path,
    model_spec: &str,
) -> anyhow::Result<CompileOptions> {
    let (provider, _) = parse_model_spec(model_spec)?;
    vault_provider_options_for_provider(vault_root, provider)
}

/// Same lookup as `vault_provider_options`, but keyed directly on a
/// provider name rather than a full `<provider>/<model>` spec — for
/// callers (e.g. `okf-mcp models <provider>`) that don't have a model name
/// on hand yet, since listing available models is exactly how a user picks
/// one.
///
/// Falls back to the process-level `custom_providers` map (populated by
/// `okf-mcp setup`'s `.env`/config.yml persistence — see
/// `config_manager::env_overrides`/`Config::custom_providers`) when the
/// vault's own `.okf/config.toml` has no matching `[providers.<name>]`
/// entry — vault-level config stays higher precedence (more specific),
/// matching this codebase's existing CLI > env > local > global pattern.
pub fn vault_provider_options_for_provider(
    vault_root: &Path,
    provider: &str,
) -> anyhow::Result<CompileOptions> {
    let vault_config = load_vault_config(vault_root)?;
    let provider_config = vault_config.providers.get(provider);

    let mut base_url_override = provider_config.and_then(|p| p.base_url.clone());
    let api_key_env_override = provider_config.and_then(|p| p.api_key_env.clone());

    if base_url_override.is_none() {
        // Lenient: a malformed process config.yml shouldn't block a
        // resolution that's otherwise fully specified by the vault.
        base_url_override = crate::core::config_manager::load_config(serde_json::Map::new())
            .ok()
            .and_then(|config| {
                config
                    .custom_providers
                    .get(provider)
                    .map(|e| e.base_url.clone())
            });
    }

    // `default_max_tokens` (serde `#[serde(default = ...)]`) guarantees
    // `config.compiler.max_tokens` is always present, even for a vault with
    // no `.okf/config.toml` at all (`OkfVaultConfig::default()`) or one
    // whose `[compiler]` table omits `max_tokens` — so this is always
    // `Some`, never a "no config" gap the way `temperature: None` above is
    // (see this function's doc comment: that gap is a separate, pre-existing
    // issue, out of scope here).
    let max_tokens = Some(vault_config.compiler.max_tokens);

    Ok(CompileOptions {
        temperature: None,
        base_url_override,
        api_key_env_override,
        max_tokens,
        ..CompileOptions::default()
    })
}

/// Lists a provider's available model names — the same lookup
/// `execute_compile_prompt`'s model-not-found hint uses, exposed directly
/// as its own operation for `okf-mcp models <provider>`.
pub async fn list_models(
    provider: &str,
    base_url_override: Option<&str>,
    api_key_env_override: Option<&str>,
) -> anyhow::Result<Vec<String>> {
    LLMCompilerDriver::new()
        .list_models(provider, base_url_override, api_key_env_override)
        .await
}
