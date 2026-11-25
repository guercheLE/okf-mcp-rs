//! `okf-ingest`'s body, split out of the thin `#[tool]` dispatch in
//! `mcp_server.rs`.

use rmcp::RoleServer;
use rmcp::model::CallToolResult;
use rmcp::service::RequestContext;
use rmcp::{ErrorData as McpError, handler::server::wrapper::Parameters};

use crate::core::mcp_server::OkfServer;
use crate::core::mcp_server::args::IngestArgs;
use crate::core::mcp_server::helpers::extract_http_request_credentials;
use crate::core::vault_resolver::resolve_vault;
use crate::ingest;

pub(crate) async fn ingest(
    server: &OkfServer,
    Parameters(args): Parameters<IngestArgs>,
    context: RequestContext<RoleServer>,
) -> Result<CallToolResult, McpError> {
    let config = server.config.clone();
    let auth_manager = server.auth_manager.clone();
    let request_credentials = extract_http_request_credentials(&context, &config);

    server
        .run_tool("okf-ingest", async move {
            let vault_root = resolve_vault(args.vault.as_deref())?;
            let mut auth_manager = auth_manager.lock().await;
            let report = ingest::process_ingest(
                &args.source,
                &args.tags,
                &vault_root,
                &config,
                &mut auth_manager,
                request_credentials.as_ref(),
            )
            .await?;
            Ok(serde_json::json!({
                "source_uri": report.source_uri,
                "outcome": format!("{:?}", report.outcome),
                "raw_path": report.raw_path,
            }))
        })
        .await
}
