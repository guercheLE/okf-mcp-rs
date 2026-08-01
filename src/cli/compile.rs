// Standalone `okf-mcp compile` command.

use okf_mcp::compiler::{self, CompileOptions};
use okf_mcp::core::vault_resolver::resolve_vault;
use okf_mcp::storage::{bundle, git};

pub async fn run(model: Option<&str>, diff: bool, vault: Option<&str>) -> anyhow::Result<()> {
    let vault_root = resolve_vault(vault)?;
    let model_spec = compiler::resolve_model_spec(&vault_root, model)?;

    if diff {
        // `--diff`: show what would be compiled without calling the LLM.
        let manifest = okf_mcp::manifest::store::load(&vault_root)?;
        for (uri, _) in manifest.active_entries() {
            println!("{uri}");
        }
        return Ok(());
    }

    let report =
        compiler::compile(&vault_root, &model_spec, true, &CompileOptions::default()).await?;
    report_and_commit(&vault_root, &report, "okf-mcp compile")
}

/// Shared by `cli::rebuild` — same "print outcome, write the bundle, commit
/// touched paths" tail for both commands.
pub(crate) fn report_and_commit(
    vault_root: &std::path::Path,
    report: &okf_mcp::compiler::CompileReport,
    commit_summary: &str,
) -> anyhow::Result<()> {
    println!(
        "Compiled {} source(s), {} failed.",
        report.sources_processed(),
        report.sources_failed()
    );
    for source in &report.sources {
        if let Some(error) = &source.error {
            eprintln!("  {} failed: {error}", source.uri);
        }
    }
    if report.lint_report.has_errors() {
        eprintln!(
            "{}",
            okf_mcp::validator::report::to_text(&report.lint_report)
        );
    }

    let bundle_path = bundle::write_bundle(vault_root)?;
    let mut paths: Vec<String> = report
        .touched_paths
        .iter()
        .filter_map(|path| {
            path.strip_prefix(vault_root)
                .ok()
                .map(|relative| relative.to_string_lossy().replace('\\', "/"))
        })
        .collect();
    paths.push("wiki/index.md".to_string());
    if let Ok(relative) = bundle_path.strip_prefix(vault_root) {
        paths.push(relative.to_string_lossy().replace('\\', "/"));
    }
    paths.sort();
    paths.dedup();

    if git::is_git_repository(vault_root) {
        let message = format!(
            "{commit_summary}: {} source(s) compiled",
            report.sources_processed()
        );
        match git::commit(vault_root, &paths, &message) {
            Ok(outcome) if outcome.committed => println!("Committed changes."),
            Ok(_) => println!("Nothing to commit."),
            Err(err) => eprintln!("git commit skipped: {err}"),
        }
    }

    if report.sources_failed() > 0 || report.lint_report.has_errors() {
        anyhow::bail!("compile finished with errors");
    }
    Ok(())
}
