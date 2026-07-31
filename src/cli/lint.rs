// Standalone `okf-mcp lint` command.

use okf_mcp::core::vault_resolver::resolve_vault;
use okf_mcp::validator::{lint_bundle, report};

pub fn run(strict: bool, json: bool, vault: Option<&str>) -> anyhow::Result<()> {
    let vault_root = resolve_vault(vault)?;
    let lint_report = lint_bundle(&vault_root)?;

    if json {
        println!("{}", report::to_json(&lint_report)?);
    } else {
        println!("{}", report::to_text(&lint_report));
    }

    let clean = !lint_report.has_errors() && (!strict || lint_report.orphan_pages.is_empty());
    if !clean {
        anyhow::bail!("lint found blocking issues");
    }
    Ok(())
}
