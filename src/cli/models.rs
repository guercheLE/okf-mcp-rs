// `okf-mcp models <provider>`: list a provider's available model names —
// the same lookup `compile`'s model-not-found hint already used internally,
// exposed directly as its own command so a user can check what's available
// *before* picking a `--model` value, not just after guessing wrong.

use okf_mcp::compiler;
use okf_mcp::core::output::Output;
use okf_mcp::core::vault_resolver::resolve_vault;

pub async fn run(provider: &str, vault: Option<&str>) -> anyhow::Result<()> {
    // Vault-level `[providers.<name>]` overrides apply if a vault resolves
    // (matching `compile`'s own routing), but resolving one isn't required
    // — listing models for a provider with a real default base URL
    // (anthropic, openai, groq, ...) works from anywhere.
    let options = match resolve_vault(vault) {
        Ok(vault_root) => compiler::vault_provider_options_for_provider(&vault_root, provider)?,
        Err(_) => compiler::CompileOptions::default(),
    };

    let models = compiler::list_models(
        provider,
        options.base_url_override.as_deref(),
        options.api_key_env_override.as_deref(),
    )
    .await?;

    let output = Output::cli();
    if models.is_empty() {
        output.line(&format!("no models reported for provider '{provider}'"));
    } else {
        for model in &models {
            output.line(model);
        }
    }
    Ok(())
}
