// Standalone `okf-mcp rebuild` command.

use okf_mcp::compiler::{self, CompileOptions};
use okf_mcp::core::output::Output;
use okf_mcp::core::vault_resolver::resolve_vault;

use super::compile::report_and_commit;

pub async fn run(model: Option<&str>, force: bool, vault: Option<&str>) -> anyhow::Result<()> {
    let vault_root = resolve_vault(vault)?;
    let model_spec = compiler::resolve_model_spec(&vault_root, model)?;

    // `--force`: recompile every active source, not just unprocessed ones.
    // Without it, `rebuild` behaves like `compile` (diff-only) — the same
    // underlying `compiler::compile` call, just under this command's name.
    let diff_only = !force;
    let output = Output::cli();
    let report = compiler::compile(
        &vault_root,
        &model_spec,
        diff_only,
        &CompileOptions::default(),
        Some(&output),
    )
    .await?;
    report_and_commit(&vault_root, &report, "okf-mcp rebuild")
}
