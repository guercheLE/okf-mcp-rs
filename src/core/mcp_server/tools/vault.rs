//! `okf-explore`/`okf-list-vaults` bodies, split out of the thin `#[tool]`
//! dispatch in `mcp_server.rs`.

use rmcp::model::CallToolResult;
use rmcp::{ErrorData as McpError, handler::server::wrapper::Parameters};

use crate::core::mcp_server::OkfServer;
use crate::core::mcp_server::args::{ExploreArgs, ListVaultsArgs};
use crate::core::vault_registry::VaultRegistry;
use crate::core::vault_resolver::resolve_vault;

pub(crate) async fn explore(
    server: &OkfServer,
    Parameters(args): Parameters<ExploreArgs>,
) -> Result<CallToolResult, McpError> {
    let explorers = server.explorers.clone();
    server
        .run_tool("okf-explore", async move {
            let vault_root = resolve_vault(args.vault.as_deref())?;
            let key = vault_root.canonicalize().unwrap_or(vault_root.clone());
            let open = args.open.unwrap_or(true);
            let mut explorers = explorers.lock().await;
            if let Some(handle) = explorers.get(&key)
                && !handle.task.is_finished()
            {
                if open && let Err(err) = open::that_detached(&handle.url) {
                    tracing::warn!(error = %err, "could not open a browser");
                }
                return Ok(serde_json::json!({
                    "url": handle.url,
                    "vault_root": vault_root.display().to_string(),
                    "already_running": true,
                }));
            }
            let handle =
                crate::explorer::serve(&vault_root, "127.0.0.1", args.port.unwrap_or(0), open)
                    .await?;
            let url = handle.url.clone();
            explorers.insert(key, handle);
            Ok(serde_json::json!({
                "url": url,
                "vault_root": vault_root.display().to_string(),
                "already_running": false,
            }))
        })
        .await
}

pub(crate) async fn list_vaults(
    server: &OkfServer,
    Parameters(_args): Parameters<ListVaultsArgs>,
) -> Result<CallToolResult, McpError> {
    server
        .run_tool("okf-list-vaults", async move {
            let registry = VaultRegistry::load()?;
            let vaults: Vec<serde_json::Value> = registry
                .vaults
                .iter()
                .map(|(name, entry)| {
                    serde_json::json!({
                        "name": name,
                        "path": entry.path,
                        "description": entry.description,
                        "default": registry.default.as_deref() == Some(name.as_str()),
                    })
                })
                .collect();
            Ok(serde_json::json!({ "vaults": vaults }))
        })
        .await
}
