//! Every "ask the operator something" step — split out of
//! `setup_wizard.rs` so that file is just the `Prompts` seam and the
//! top-level orchestration (`run_setup_wizard[_with]`). Each function here
//! is plain, deterministic logic given a `&dyn Prompts`, same as before
//! the split — see `setup_wizard.rs`'s module doc comment.

use std::collections::HashMap;
use std::time::Duration;

use okf_mcp::compiler::provider::{self, KNOWN_PROVIDERS};
use okf_mcp::core::config_schema::{AuthMethod, CustomProviderEntry, Transport};
use okf_mcp::core::credential_storage::{credential_exists, load_credential};

use super::Prompts;
use super::logic::{
    CustomProviderChoice, KnownProviderChoice, ProviderKeyResult, provider_default_indices,
    provider_key_from_input, validate_custom_provider_name,
};
use super::persistence::persist_selection;

pub(super) async fn prompt_base_url(
    prompts: &dyn Prompts,
    default: &str,
) -> anyhow::Result<String> {
    let url = prompts
        .text("Firecrawl base URL:".to_string(), Some(default.to_string()))
        .await?;

    let client = reqwest::Client::new();
    match client
        .head(&url)
        .timeout(Duration::from_secs(5))
        .send()
        .await
    {
        Ok(_) => println!("reachable"),
        Err(_) => println!(
            "could not reach that URL yet — continuing anyway (fix it later with `test-connection`)"
        ),
    }

    Ok(url)
}

pub(super) async fn prompt_auth_method() -> anyhow::Result<AuthMethod> {
    // Exactly one scheme was discovered — nothing to choose between.
    Ok(AuthMethod::Pat)
}

/// Outcome of asking whether to keep an already-saved secret or replace it.
pub(super) enum SecretDecision {
    KeepExisting,
    New(String),
}

/// If `account` already has a saved credential, asks (default: keep)
/// whether to keep it — if kept, nothing is re-prompted or re-saved. If
/// nothing is saved yet, or the operator chooses to overwrite, prompts for
/// a new value via `Password`.
pub(super) async fn prompt_secret_decision(
    prompts: &dyn Prompts,
    label: &str,
    account: &str,
) -> anyhow::Result<SecretDecision> {
    if credential_exists(account)? {
        let keep = prompts
            .confirm(format!("Keep the saved {label}?"), true)
            .await?;
        if keep {
            return Ok(SecretDecision::KeepExisting);
        }
    }
    let value = prompts.password(format!("{label}:")).await?;
    Ok(SecretDecision::New(value))
}

/// Resolves the credential fields to use this run — either reloaded from
/// the keychain (if the operator chose to keep the existing value) or
/// freshly entered — plus whether a new value needs saving.
pub(super) async fn prompt_firecrawl_credentials(
    prompts: &dyn Prompts,
    auth_method: AuthMethod,
) -> anyhow::Result<(HashMap<String, String>, bool)> {
    match auth_method {
        AuthMethod::Pat => {
            match prompt_secret_decision(prompts, "Firecrawl personal access token", "firecrawl")
                .await?
            {
                SecretDecision::KeepExisting => {
                    let existing = load_credential("firecrawl")?.ok_or_else(|| {
                        anyhow::anyhow!("expected an existing Firecrawl credential")
                    })?;
                    let credentials: HashMap<String, String> = serde_json::from_str(&existing)?;
                    Ok((credentials, false))
                }
                SecretDecision::New(value) => {
                    let mut credentials = HashMap::new();
                    credentials.insert("token".to_string(), value);
                    Ok((credentials, true))
                }
            }
        }
    }
}

// mcpify:versions:begin
pub(super) async fn prompt_api_version() -> anyhow::Result<String> {
    Ok("default".to_string())
}
// mcpify:versions:end

pub(super) async fn prompt_transport(
    prompts: &dyn Prompts,
    default: Transport,
) -> anyhow::Result<Transport> {
    let choices = vec![
        "stdio (spawned as a subprocess by the MCP client)",
        "http (a standalone server the MCP client connects to over the network)",
    ];
    let starting_cursor = match default {
        Transport::Stdio => 0,
        Transport::Http => 1,
    };
    let selection = prompts
        .select(
            "Which transport will this deployment use?".to_string(),
            choices,
            starting_cursor,
        )
        .await?;

    Ok(if selection.starts_with("stdio") {
        Transport::Stdio
    } else {
        Transport::Http
    })
}

/// Prompts for one provider's API key, offering the same keep/overwrite
/// choice as `prompt_secret_decision` when a value is already saved. A
/// blank/whitespace-only answer means "this provider doesn't need a key"
/// — skipped, not an error. `key_prompt_message` is only used the first
/// time (no saved value yet), which is where a provider-specific hint
/// (e.g. "leave blank if not needed") matters most.
pub(super) async fn prompt_provider_api_key(
    prompts: &dyn Prompts,
    label: &str,
    account: &str,
    key_prompt_message: &str,
) -> anyhow::Result<ProviderKeyResult> {
    if credential_exists(account)? {
        return Ok(
            match prompt_secret_decision(prompts, label, account).await? {
                SecretDecision::KeepExisting => {
                    let existing = load_credential(account)?.unwrap_or_default();
                    ProviderKeyResult {
                        value: Some(existing),
                        freshly_entered: false,
                    }
                }
                SecretDecision::New(value) => provider_key_from_input(value),
            },
        );
    }
    let value = prompts.password(key_prompt_message.to_string()).await?;
    Ok(provider_key_from_input(value))
}

/// Prompts for a new custom provider's name, retrying (with an error
/// message) until `validate_custom_provider_name` accepts it — validated
/// against collision with `KNOWN_PROVIDERS` (including `"custom"` itself)
/// and with any custom provider already added earlier in this same run.
pub(super) async fn prompt_custom_provider_name(
    prompts: &dyn Prompts,
    custom_so_far: &[CustomProviderChoice],
) -> anyhow::Result<String> {
    let taken: Vec<String> = KNOWN_PROVIDERS
        .iter()
        .map(|s| s.to_string())
        .chain(custom_so_far.iter().map(|c| c.name.clone()))
        .collect();
    loop {
        let input = prompts
            .text(
                "Custom provider name (used as <name> in --model <name>/<model>):".to_string(),
                None,
            )
            .await?;
        match validate_custom_provider_name(&input, &taken) {
            Ok(()) => return Ok(input.trim().to_string()),
            Err(message) => println!("{message} — try again."),
        }
    }
}

/// Optional: known-provider API keys (multi-select) plus zero, one, or
/// many named custom providers, for `okf-mcp compile`/`rebuild`.
/// `existing_custom_providers` (from `logic::existing_config_or_defaults`)
/// lets a re-run reuse a previously-configured custom provider's endpoint
/// silently, without re-asking for it.
pub(super) async fn prompt_llm_providers(
    prompts: &dyn Prompts,
    existing_custom_providers: &HashMap<String, CustomProviderEntry>,
) -> anyhow::Result<(Vec<KnownProviderChoice>, Vec<CustomProviderChoice>)> {
    let configure = prompts
        .confirm(
            "Configure LLM provider(s) now, for `okf-mcp compile`/`rebuild`? (optional)"
                .to_string(),
            true,
        )
        .await?;
    if !configure {
        return Ok((Vec::new(), Vec::new()));
    }

    let known_names: Vec<&str> = KNOWN_PROVIDERS
        .iter()
        .copied()
        .filter(|p| *p != "custom")
        .collect();
    let default_indices = provider_default_indices(&known_names, |name| {
        credential_exists(&format!("llm-{name}")).unwrap_or(false)
    });

    let selected = prompts
        .multi_select(
            "Known LLM providers to configure:".to_string(),
            known_names,
            default_indices,
        )
        .await?;

    let mut known = Vec::new();
    for name in selected {
        let name = name.to_string();
        let spec = provider::provider_spec(&name)?;
        let key = if spec.api_key_env.is_none() {
            println!("{name} needs no API key.");
            ProviderKeyResult {
                value: None,
                freshly_entered: false,
            }
        } else {
            prompt_provider_api_key(
                prompts,
                &name,
                &format!("llm-{name}"),
                &format!("{name} API key:"),
            )
            .await?
        };
        known.push(KnownProviderChoice { name, key });
    }

    let mut custom = Vec::new();
    let mut add_another = prompts
        .confirm(
            "Add a custom OpenAI-compatible provider (self-hosted vLLM, etc.)?".to_string(),
            false,
        )
        .await?;
    while add_another {
        let name = prompt_custom_provider_name(prompts, &custom).await?;
        let base_url = if let Some(entry) = existing_custom_providers.get(&name) {
            println!("Reusing saved endpoint for {name}: {}", entry.base_url);
            entry.base_url.clone()
        } else {
            prompts
                .text(format!("Endpoint (base URL) for {name}:"), None)
                .await?
        };
        let key = prompt_provider_api_key(
            prompts,
            &name,
            &format!("llm-{name}"),
            "API key (if this provider does not require one, leave blank or enter anything):",
        )
        .await?;
        custom.push(CustomProviderChoice {
            name,
            base_url,
            key,
        });

        add_another = prompts
            .confirm("Add another custom provider?".to_string(), false)
            .await?;
    }

    Ok((known, custom))
}

/// One `Confirm` per secret, default `false` — the sole gate on whether a
/// secret's real value ever leaves the OS keychain/encrypted-file store.
pub(super) async fn prompt_secret_reveal(
    prompts: &dyn Prompts,
    label: &str,
) -> anyhow::Result<bool> {
    prompts
        .confirm(
            format!(
                "Include {label}'s real value in the MCP client config printed/saved at the end?"
            ),
            false,
        )
        .await
}

pub(super) async fn prompt_persistence(
    prompts: &dyn Prompts,
    env: &HashMap<String, String>,
    config_fields: &serde_json::Value,
    secret_keys: &std::collections::HashSet<String>,
) -> anyhow::Result<()> {
    let choices = vec![
        "Write a .env file",
        "Write a config.yml file (global — ~/.okf-mcp/config.yml, read on every invocation on this machine)",
        "Write a config.yml file (local — ./okf-mcp.config.yml, read only from this directory)",
        "Print a ready-to-run CLI invocation (nothing written to disk)",
    ];
    let selection = prompts
        .select(
            "How should these settings be saved?".to_string(),
            choices,
            0,
        )
        .await?;

    persist_selection(selection, env, config_fields, secret_keys).await
}
