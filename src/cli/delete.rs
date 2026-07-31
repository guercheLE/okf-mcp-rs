// `okf-mcp delete <URL|FILE> [--purge]`.

use okf_mcp::core::vault_resolver::resolve_vault;
use okf_mcp::ingest::{DeleteOutcome, delete_source};

pub fn run(source: &str, purge: bool, vault: Option<&str>) -> anyhow::Result<()> {
    let vault_root = resolve_vault(vault)?;
    let outcome = delete_source(&vault_root, source, purge, "deleted via okf-mcp delete")?;
    match outcome {
        DeleteOutcome::Tombstoned => println!("Tombstoned '{source}' (raw blob kept on disk)."),
        DeleteOutcome::Purged { removed_raw_ids } => println!(
            "Purged '{source}' ({} raw blob(s) removed).",
            removed_raw_ids.len()
        ),
    }
    Ok(())
}
