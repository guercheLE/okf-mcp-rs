// `okf-mcp reindex`: rebuilds the vault's `.okf/index.db`.

use okf_mcp::core::vault_resolver::resolve_vault;
use okf_mcp::search::reindex;

pub fn run(embeddings: bool, vault: Option<&str>) -> anyhow::Result<()> {
    let vault_root = resolve_vault(vault)?;
    let report = reindex(&vault_root, embeddings)?;
    println!(
        "Indexed {} document(s). {} embedded, {} unchanged (skipped).",
        report.text_documents_indexed, report.vectors_embedded, report.vectors_skipped_unchanged
    );
    Ok(())
}
