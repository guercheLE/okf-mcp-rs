// Standalone `okf-mcp compile` command.

use okf_mcp::compiler::{self, CompileOptions};
use okf_mcp::core::output::Output;
use okf_mcp::core::vault_resolver::resolve_vault;
use okf_mcp::storage::{bundle, git};

pub async fn run(model: Option<&str>, diff: bool, vault: Option<&str>) -> anyhow::Result<()> {
    let vault_root = resolve_vault(vault)?;
    let model_spec = compiler::resolve_model_spec(&vault_root, model)?;
    let output = Output::cli();

    if diff {
        // `--diff`: show what would be compiled without calling the LLM.
        let manifest = okf_mcp::manifest::store::load(&vault_root)?;
        for (uri, _) in manifest.active_entries() {
            output.line(uri);
        }
        return Ok(());
    }

    let report = compiler::compile(
        &vault_root,
        &model_spec,
        true,
        &CompileOptions::default(),
        Some(&output),
    )
    .await?;
    report_and_commit(&vault_root, &report, "okf-mcp compile")
}

/// Shared by `cli::rebuild` — same "print outcome, write the bundle, commit
/// touched paths" tail for both commands.
///
/// If any source failed or lint reports errors, this returns `Err` *before*
/// writing `okf.json` or committing anything — a partial/broken run's
/// inconsistent wiki state must never land in `okf.json` or git history.
/// `./raw/` blobs and each source's own manifest ingest-history/
/// `compiled_hash` entry are untouched either way: those are written
/// during `ingest`/per-source in `compiler::compile`, entirely outside
/// this function, and `manifest.json` is never part of the git-staged
/// path list below — so a source that itself succeeded within an
/// otherwise-failed run stays resumable on the next `compile` (see
/// `select_sources`), it just doesn't get bundled/committed until a
/// subsequent clean run.
pub(crate) fn report_and_commit(
    vault_root: &std::path::Path,
    report: &okf_mcp::compiler::CompileReport,
    commit_summary: &str,
) -> anyhow::Result<()> {
    let output = Output::cli();
    output.line(&format!(
        "Compiled {} source(s), {} failed.",
        report.sources_processed(),
        report.sources_failed()
    ));
    for source in &report.sources {
        if let Some(error) = &source.error {
            output.line(&format!("  {} failed: {error}", source.uri));
        }
    }
    if report.lint_report.has_errors() {
        output.line(&okf_mcp::validator::report::to_text(&report.lint_report));
    }

    if report.sources_failed() > 0 || report.lint_report.has_errors() {
        output.line(&format!(
            "{} source(s) failed / lint found errors — not committing; fix and re-run compile.",
            report.sources_failed()
        ));
        anyhow::bail!("compile finished with errors");
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
            Ok(outcome) if outcome.committed => output.line("Committed changes."),
            Ok(_) => output.line("Nothing to commit."),
            Err(err) => output.line(&format!("git commit skipped: {err}")),
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::process::Command;

    use okf_mcp::compiler::CompileReport;

    use super::*;

    fn run_git(dir: &Path, args: &[&str]) -> std::process::Output {
        Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .unwrap()
    }

    fn init_repo(dir: &Path) {
        assert!(run_git(dir, &["init", "--quiet"]).status.success());
        assert!(
            run_git(dir, &["config", "user.email", "test@example.com"])
                .status
                .success()
        );
        assert!(
            run_git(dir, &["config", "user.name", "Test"])
                .status
                .success()
        );
    }

    fn failing_report() -> CompileReport {
        CompileReport {
            sources: vec![okf_mcp::compiler::driver::SourceOutcome {
                uri: "https://example.com/a".to_string(),
                raw_id: "raw_aaa".to_string(),
                error: Some("LLM call failed".to_string()),
            }],
            touched_paths: Vec::new(),
            lint_report: Default::default(),
        }
    }

    fn clean_report() -> CompileReport {
        CompileReport {
            sources: vec![okf_mcp::compiler::driver::SourceOutcome {
                uri: "https://example.com/a".to_string(),
                raw_id: "raw_aaa".to_string(),
                error: None,
            }],
            touched_paths: Vec::new(),
            lint_report: Default::default(),
        }
    }

    #[test]
    fn a_failed_source_skips_the_bundle_and_the_commit() {
        let vault = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(vault.path().join(".okf")).unwrap();

        let result = report_and_commit(vault.path(), &failing_report(), "okf-mcp compile");

        assert!(result.is_err());
        assert!(!vault.path().join("okf.json").exists());
    }

    #[test]
    fn lint_errors_skip_the_bundle_and_the_commit_even_with_no_failed_sources() {
        let vault = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(vault.path().join(".okf")).unwrap();

        let mut report = clean_report();
        report.sources[0].error = None;
        report.lint_report.broken_links =
            vec![("wiki/concepts/a.md".to_string(), "missing".to_string())];

        let result = report_and_commit(vault.path(), &report, "okf-mcp compile");

        assert!(result.is_err());
        assert!(!vault.path().join("okf.json").exists());
    }

    #[test]
    fn a_clean_report_writes_the_bundle_and_commits() {
        let vault = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(vault.path().join(".okf")).unwrap();
        init_repo(vault.path());
        // In the real `compiler::compile` flow, `regenerate_index` writes
        // this before `report_and_commit` ever runs.
        std::fs::create_dir_all(vault.path().join("wiki")).unwrap();
        std::fs::write(vault.path().join("wiki/index.md"), "# Wiki Index\n").unwrap();

        let result = report_and_commit(vault.path(), &clean_report(), "okf-mcp compile");

        assert!(result.is_ok(), "{result:?}");
        assert!(vault.path().join("okf.json").exists());
        let log = run_git(vault.path(), &["log", "--oneline"]);
        assert!(!String::from_utf8_lossy(&log.stdout).trim().is_empty());
    }
}
