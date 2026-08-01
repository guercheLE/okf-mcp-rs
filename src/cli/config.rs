// `okf-mcp config`: prints the resolved configuration, grouped by where
// each value actually comes from — global file, local file, env vars,
// vault-level config, credential storage — since a single flat dump of
// the merged result (the old behavior) gives no way to tell which layer
// is actually responsible for a given value.

use okf_mcp::compiler::provider;
use okf_mcp::core::config_manager::{
    env_overrides, global_config_path, load_config, local_config_path, read_yaml_if_exists,
};
use okf_mcp::core::credential_storage::load_credential;
use okf_mcp::core::output::Output;
use okf_mcp::core::sanitizer::sanitize;
use okf_mcp::core::vault_config::load_vault_config;
use okf_mcp::core::vault_resolver::resolve_vault;

fn config_file_status(path: std::path::PathBuf) -> serde_json::Value {
    let exists = path.is_file();
    let contents = if exists {
        sanitize(&serde_json::Value::Object(read_yaml_if_exists(&path)))
    } else {
        serde_json::Value::Null
    };
    serde_json::json!({
        "path": path.display().to_string(),
        "exists": exists,
        "contents": contents,
    })
}

pub fn run(vault: Option<&str>) -> anyhow::Result<()> {
    let global = config_file_status(global_config_path());
    let local = config_file_status(local_config_path());

    let env_vars = sanitize(&serde_json::to_value(env_overrides())?);
    let provider_env_presence: serde_json::Map<String, serde_json::Value> =
        provider::KNOWN_PROVIDERS
            .iter()
            .filter_map(|name| provider::provider_spec(name).ok()?.api_key_env)
            .map(|env_name| {
                (
                    env_name.to_string(),
                    serde_json::json!(provider::env_var_is_meaningfully_set(env_name)),
                )
            })
            .collect();
    let dotenv_present = std::path::Path::new(".env").is_file();

    let vault_config = resolve_vault(vault).ok().map(|root| {
        let path = root.join(".okf/config.toml");
        let exists = path.is_file();
        let contents = if exists {
            load_vault_config(&root)
                .ok()
                .and_then(|config| serde_json::to_value(config).ok())
                .map(|value| sanitize(&value))
                .unwrap_or(serde_json::Value::Null)
        } else {
            serde_json::Value::Null
        };
        serde_json::json!({
            "path": path.display().to_string(),
            "exists": exists,
            "contents": contents,
        })
    });

    // Existence only — never the value itself. Firecrawl plus every
    // key-bearing LLM provider (Ollama needs none, so it's never saved).
    let mut credentials = Vec::new();
    for account in std::iter::once("firecrawl".to_string()).chain(
        provider::KNOWN_PROVIDERS
            .iter()
            .filter(|name| **name != "ollama")
            .map(|name| format!("llm-{name}")),
    ) {
        let saved = load_credential(&account)?.is_some();
        credentials.push(serde_json::json!({ "account": account, "saved": saved }));
    }

    let resolved = sanitize(&serde_json::to_value(load_config(serde_json::Map::new())?)?);

    Output::cli().line(&serde_json::to_string_pretty(&serde_json::json!({
        "global_config_file": global,
        "local_config_file": local,
        "env_vars": {
            "okf_mcp": env_vars,
            "llm_provider_keys_present": provider_env_presence,
            "dotenv_file_present_but_not_auto_loaded": dotenv_present,
        },
        "vault_config": vault_config,
        "credentials": credentials,
        "resolved": resolved,
    }))?);
    Ok(())
}
