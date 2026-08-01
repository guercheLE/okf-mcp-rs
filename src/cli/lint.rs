// Standalone `okf-mcp lint` command.

use okf_mcp::core::output::Output;
use okf_mcp::core::vault_resolver::resolve_vault;
use okf_mcp::validator::{lint_bundle, report};

pub fn run(strict: bool, json: bool, vault: Option<&str>) -> anyhow::Result<()> {
    let vault_root = resolve_vault(vault)?;
    let lint_report = lint_bundle(&vault_root)?;
    let output = Output::cli();

    if json {
        output.line(&report::to_json(&lint_report)?);
    } else {
        output.line(&report::to_text(&lint_report));
    }

    let clean = !lint_report.has_errors() && (!strict || lint_report.orphan_pages.is_empty());
    if !clean {
        anyhow::bail!("lint found blocking issues");
    }
    Ok(())
}
