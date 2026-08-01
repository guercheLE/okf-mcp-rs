// `okf-mcp run <URL|FILE>`: ingest, compile, lint, and commit as one step.

use okf_mcp::auth::auth_manager::AuthManager;
use okf_mcp::compiler;
use okf_mcp::core::config_manager::load_config;
use okf_mcp::core::output::Output;
use okf_mcp::core::vault_resolver::resolve_vault;
use okf_mcp::ingest::process_ingest;

use super::compile::report_and_commit;

pub async fn run(
    source: &str,
    tags: &[String],
    model: Option<&str>,
    vault: Option<&str>,
) -> anyhow::Result<()> {
    let vault_root = resolve_vault(vault)?;
    let config = load_config(serde_json::Map::new())?;
    let mut auth_manager = AuthManager::new(config.auth_method);
    let output = Output::cli();

    output.line(&format!("Fetching '{source}'..."));
    let ingest_report =
        process_ingest(source, tags, &vault_root, &config, &mut auth_manager, None).await?;
    output.line(&format!(
        "Ingested '{}' ({:?}).",
        ingest_report.source_uri, ingest_report.outcome
    ));

    let model_spec = compiler::resolve_model_spec(&vault_root, model)?;
    let options = compiler::vault_provider_options(&vault_root, &model_spec)?;
    let compile_report =
        compiler::compile(&vault_root, &model_spec, true, &options, Some(&output)).await?;
    report_and_commit(&vault_root, &compile_report, "okf-mcp run")
}
