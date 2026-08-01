// `okf-mcp run <URL|FILE>`: ingest, compile, lint, and commit as one step.

use okf_mcp::auth::auth_manager::AuthManager;
use okf_mcp::compiler::{self, CompileOptions};
use okf_mcp::core::config_manager::load_config;
use okf_mcp::core::vault_resolver::resolve_vault;
use okf_mcp::ingest::process_ingest;

use super::compile::report_and_commit;

pub async fn run(
    source: &str,
    tag: Option<&str>,
    model: Option<&str>,
    vault: Option<&str>,
) -> anyhow::Result<()> {
    let vault_root = resolve_vault(vault)?;
    let config = load_config(serde_json::Map::new())?;
    let mut auth_manager = AuthManager::new(config.auth_method);

    let ingest_report =
        process_ingest(source, tag, &vault_root, &config, &mut auth_manager, None).await?;
    println!(
        "Ingested '{}' ({:?}).",
        ingest_report.source_uri, ingest_report.outcome
    );

    let model_spec = compiler::resolve_model_spec(&vault_root, model)?;
    let compile_report =
        compiler::compile(&vault_root, &model_spec, true, &CompileOptions::default()).await?;
    report_and_commit(&vault_root, &compile_report, "okf-mcp run")
}
