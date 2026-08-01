// Standalone `okf-mcp lint` command.

use okf_mcp::core::output::Output;
use okf_mcp::core::vault_resolver::resolve_vault;
use okf_mcp::validator::{fix, lint_bundle, report};

pub fn run(strict: bool, json: bool, fix: bool, vault: Option<&str>) -> anyhow::Result<()> {
    let vault_root = resolve_vault(vault)?;
    let output = Output::cli();

    let (fix_report, lint_report) = if fix {
        let (fix_report, post_fix) = okf_mcp::validator::fix_bundle(&vault_root)?;
        (Some(fix_report), post_fix)
    } else {
        (None, lint_bundle(&vault_root)?)
    };

    if json {
        let mut value = serde_json::to_value(&lint_report)?;
        if let (Some(object), Some(fix_report)) = (value.as_object_mut(), &fix_report) {
            object.insert(
                "fix".to_string(),
                serde_json::json!({
                    "fixed_count": fix_report.fixed_sources.len(),
                    "fixed": fix_report.fixed_sources,
                }),
            );
        }
        output.line(&serde_json::to_string_pretty(&value)?);
    } else {
        if let Some(fix_report) = &fix_report {
            output.line(&fix::summary_line(fix_report));
        }
        output.line(&report::to_text(&lint_report));
    }

    let clean = !lint_report.has_errors() && (!strict || lint_report.orphan_pages.is_empty());
    if !clean {
        anyhow::bail!("lint found blocking issues");
    }
    Ok(())
}
