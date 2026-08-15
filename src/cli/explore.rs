// `okf-mcp explore`: open the vault in the browser-based explorer (3D link
// graph, note pane, backlinks, hybrid search) and keep serving until Ctrl-C.

use okf_mcp::core::output::Output;
use okf_mcp::core::vault_resolver::resolve_vault;
use okf_mcp::explorer;

pub async fn run(host: &str, port: u16, no_open: bool, vault: Option<&str>) -> anyhow::Result<()> {
    let vault_root = resolve_vault(vault)?;
    let output = Output::cli();

    let mut handle = explorer::serve(&vault_root, host, port, !no_open).await?;
    output.line(&format!(
        "Exploring {} at {}  (Ctrl-C to stop)",
        vault_root.display(),
        handle.url
    ));
    if no_open {
        output.line("Open the URL above in a browser.");
    }

    tokio::select! {
        _ = tokio::signal::ctrl_c() => {
            output.line("Stopping explorer.");
            handle.task.abort();
        }
        _ = &mut handle.task => {}
    }
    Ok(())
}
