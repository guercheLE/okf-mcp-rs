//! Writing this run's results out: the OS keychain (one key at a time),
//! `.env`/`config.yml`, and the final printed MCP client config — split
//! out of `setup_wizard.rs` so that file is just the `Prompts` seam and
//! the top-level orchestration (`run_setup_wizard[_with]`).

use std::collections::{HashMap, HashSet};

use okf_mcp::core::config_manager;
use okf_mcp::core::config_schema::{AuthMethod, Transport};
use okf_mcp::core::credential_storage::save_credential;

use super::logic::ProviderKeyResult;

pub(super) fn save_new_key(name: &str, key: &ProviderKeyResult) -> anyhow::Result<()> {
    if key.freshly_entered
        && let Some(value) = &key.value
    {
        save_credential(&format!("llm-{name}"), value)?;
        println!("Saved {name} API key to your OS keychain (or the encrypted-file fallback).");
    }
    Ok(())
}

/// Reads an existing `.env` file's `KEY=VALUE` lines (blank/`#`-comment
/// lines skipped) — used to merge this run's output on top instead of
/// clobbering unrelated hand-added entries.
fn read_dotenv_if_exists(path: &std::path::Path) -> HashMap<String, String> {
    let Ok(contents) = std::fs::read_to_string(path) else {
        return HashMap::new();
    };
    contents
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                return None;
            }
            line.split_once('=')
                .map(|(key, value)| (key.trim().to_string(), value.trim().to_string()))
        })
        .collect()
}

/// Persists the non-secret config fields (`firecrawl_base_url`/
/// `api_version`/`transport`/`custom_providers`) as YAML matching exactly
/// what `config_manager::load_config` reads back — never credentials,
/// which stay in the OS keychain/encrypted-file fallback via
/// `save_credential`. Merges with the file's existing contents (rather
/// than overwriting) so a hand-edited field or a previously-saved custom
/// provider entry isn't silently dropped; `custom_providers` specifically
/// merges by provider name rather than replacing the whole map.
pub(super) async fn write_config_yaml(
    path: &std::path::Path,
    config_fields: &serde_json::Value,
) -> anyhow::Result<()> {
    let mut base = config_manager::read_yaml_if_exists(path);

    if let serde_json::Value::Object(new_fields) = config_fields {
        for (key, value) in new_fields {
            if key == "custom_providers" {
                let mut merged_providers = match base.get("custom_providers") {
                    Some(serde_json::Value::Object(existing)) => existing.clone(),
                    _ => serde_json::Map::new(),
                };
                if let serde_json::Value::Object(new_providers) = value {
                    for (name, entry) in new_providers {
                        merged_providers.insert(name.clone(), entry.clone());
                    }
                }
                base.insert(
                    "custom_providers".to_string(),
                    serde_json::Value::Object(merged_providers),
                );
            } else {
                base.insert(key.clone(), value.clone());
            }
        }
    }

    let yaml = serde_yaml::to_string(&serde_json::Value::Object(base))?;
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, yaml)?;
    println!("Wrote {}", path.display());
    println!(
        "Credentials are never written to this file — they're stored via the OS \
         keychain/encrypted-file fallback from the credential prompts above."
    );
    Ok(())
}

pub(super) async fn persist_selection(
    selection: &str,
    env: &HashMap<String, String>,
    config_fields: &serde_json::Value,
    secret_keys: &HashSet<String>,
) -> anyhow::Result<()> {
    let local_dir = std::env::current_dir()?;
    let global_config =
        okf_mcp::core::credential_storage::resolve_home_dir().join(".okf-mcp/config.yml");
    persist_selection_at(
        selection,
        env,
        config_fields,
        secret_keys,
        &local_dir,
        &global_config,
    )
    .await
}

pub(super) async fn persist_selection_at(
    selection: &str,
    env: &HashMap<String, String>,
    config_fields: &serde_json::Value,
    secret_keys: &HashSet<String>,
    local_dir: &std::path::Path,
    global_config: &std::path::Path,
) -> anyhow::Result<()> {
    match selection {
        "Write a .env file" => {
            let path = local_dir.join(".env");
            // Strip this run's secret keys first, then re-add only the
            // ones just opted into — otherwise declining to reveal a
            // secret on a re-run leaves a *previous* run's plaintext
            // value stale on disk instead of removing it.
            let mut merged = read_dotenv_if_exists(&path);
            merged.retain(|key, _| !secret_keys.contains(key));
            merged.extend(env.clone());
            let contents = merged
                .iter()
                .map(|(key, value)| format!("{key}={value}"))
                .collect::<Vec<_>>()
                .join("\n");
            std::fs::write(&path, format!("{contents}\n"))?;
            println!("Wrote .env");
            println!(
                "Note: okf-mcp does not auto-load .env files — source it yourself before \
                 running `okf-mcp start`/`http` (e.g. `set -a; source .env; set +a`)."
            );
        }
        s if s.starts_with("Write a config.yml file (global") => {
            write_config_yaml(global_config, config_fields).await?;
        }
        s if s.starts_with("Write a config.yml file (local") => {
            write_config_yaml(&local_dir.join("okf-mcp.config.yml"), config_fields).await?;
        }
        _ => {
            // `okf-mcp start`/`http` take no CLI flags for these settings
            // (only the global `--vault`) — `export`ing them is the only
            // real, working non-file way to apply this run's settings.
            let mut lines: Vec<String> = env
                .iter()
                .map(|(key, value)| format!("export {key}={value:?}"))
                .collect();
            lines.sort();
            println!("{}", lines.join("\n"));
            println!("okf-mcp start");
        }
    }
    Ok(())
}

/// Prints a ready-to-use MCP client config (the `mcpServers` block a host
/// like Claude Code/Desktop expects) for whichever transport was selected
/// — the stdio and http shapes are mutually exclusive, never both, since
/// they're two different ways of running this same server and a client
/// only ever picks one. HTTP transport shows credentials as headers only
/// (never env/CLI args, matching `AuthManager::apply_auth_headers`'s
/// HTTP-only-uses-per-request-credentials behavior); stdio shows the `env`
/// block only (no fabricated CLI-flags line — `okf-mcp start` takes none).
pub(super) fn print_mcp_client_config(
    transport: Transport,
    auth_method: AuthMethod,
    env: &HashMap<String, String>,
) {
    match transport {
        Transport::Stdio => {
            let config = serde_json::json!({
                "mcpServers": {
                    "okf-mcp": {
                        "command": "okf-mcp",
                        "args": ["start"],
                        "env": env,
                    }
                }
            });
            println!("\nMCP client config (stdio):");
            println!(
                "{}",
                serde_json::to_string_pretty(&config)
                    .expect("built from only strings/maps/arrays; always serializes")
            );
        }
        Transport::Http => {
            let (_, header_name) = okf_mcp::auth::auth_manager::header_location_for(auth_method);
            let mut headers = serde_json::Map::new();
            headers.insert(
                header_name.to_string(),
                serde_json::Value::String("<credential value here>".to_string()),
            );
            let config = serde_json::json!({
                "mcpServers": {
                    "okf-mcp": {
                        "url": "http://127.0.0.1:3000/mcp",
                        "headers": headers,
                    }
                }
            });
            println!(
                "\nMCP client config (http) — adjust the host/port to match how you run \
                 `okf-mcp http`:"
            );
            println!(
                "{}",
                serde_json::to_string_pretty(&config)
                    .expect("built from only strings/maps/arrays; always serializes")
            );
            println!(
                "Credentials are never read from local config/env over HTTP transport — \
                 every request must carry its own \"{header_name}\" header."
            );
        }
    }
}
