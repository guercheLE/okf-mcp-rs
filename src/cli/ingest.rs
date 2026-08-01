// Standalone `okf-mcp ingest` command, independently testable ahead of the
// full CLI/MCP rewiring (see docs/okf-mcp-implementation-plan.md's Wave 4).

use okf_mcp::auth::auth_manager::AuthManager;
use okf_mcp::core::config_manager::load_config;
use okf_mcp::core::output::Output;
use okf_mcp::core::vault_resolver::resolve_vault;
use okf_mcp::ingest::process_ingest;
use okf_mcp::manifest::IngestOutcome;

pub async fn run(source: &str, tag: Option<&str>, vault: Option<&str>) -> anyhow::Result<()> {
    let vault_root = resolve_vault(vault)?;
    let config = load_config(serde_json::Map::new())?;
    let mut auth_manager = AuthManager::new(config.auth_method);
    let output = Output::cli();

    output.line(&format!("Fetching '{source}'..."));
    let report = process_ingest(source, tag, &vault_root, &config, &mut auth_manager, None).await?;

    match &report.outcome {
        IngestOutcome::NoOp => {
            output.line(&format!(
                "No changes: '{}' already ingested with this content.",
                report.source_uri
            ));
        }
        IngestOutcome::New { raw_id } => {
            output.line(&format!(
                "Ingested '{}' -> {raw_id} ({})",
                report.source_uri,
                report.raw_path.as_ref().unwrap().display()
            ));
        }
        IngestOutcome::Superseded {
            raw_id,
            previous_raw_id,
        } => {
            output.line(&format!(
                "Updated '{}' -> {raw_id} (superseded {previous_raw_id}) ({})",
                report.source_uri,
                report.raw_path.as_ref().unwrap().display()
            ));
        }
    }
    Ok(())
}
