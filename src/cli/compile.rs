// Standalone `okf-mcp compile` command.

use std::io::IsTerminal;

use okf_mcp::compiler;
use okf_mcp::core::output::Output;
use okf_mcp::core::vault_resolver::resolve_vault;
use okf_mcp::storage::{bundle, git};
use okf_mcp::validator;

pub async fn run(
    model: Option<&str>,
    diff: bool,
    fix: bool,
    yes: bool,
    concurrency: usize,
    vault: Option<&str>,
) -> anyhow::Result<()> {
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

    let mut options = compiler::vault_provider_options(&vault_root, &model_spec)?;
    options.concurrency = concurrency;
    let report = compiler::compile(&vault_root, &model_spec, true, &options, Some(&output)).await?;
    report_and_commit(
        &vault_root,
        &report,
        "okf-mcp compile",
        fix,
        yes,
        &model_spec,
        &options,
    )
    .await
}

/// Prompts for confirmation before committing `--fix`'s LLM-synthesized
/// changes, unless `assume_yes` (`--yes`) was passed. Mirrors
/// `cli::setup_wizard`'s existing `spawn_blocking` + `inquire` pattern for
/// running a blocking prompt from an async context. Never blocks waiting
/// for input that can't come: a non-interactive session (no TTY on stdin —
/// CI, a pipe, a background job) short-circuits to "don't commit" rather
/// than hanging.
async fn confirm_commit(message: String, assume_yes: bool) -> anyhow::Result<bool> {
    if assume_yes {
        return Ok(true);
    }
    if !std::io::stdin().is_terminal() {
        return Ok(false);
    }
    let confirmed = tokio::task::spawn_blocking(move || {
        inquire::Confirm::new(&message).with_default(false).prompt()
    })
    .await??;
    Ok(confirmed)
}

/// Shared by `cli::rebuild`/`cli::run` — same "print outcome, optionally
/// auto-fix, write the bundle, commit touched paths" tail for all three
/// commands.
///
/// If any source failed or the (possibly post-fix) lint report has errors,
/// this returns `Err` *before* writing `okf.json` or committing anything —
/// a partial/broken run's inconsistent wiki state must never land in
/// `okf.json` or git history. `./raw/` blobs and each source's own
/// manifest ingest-history/`compiled_hash` entry are untouched either way:
/// those are written during `ingest`/per-source in `compiler::compile`,
/// entirely outside this function, and `manifest.json` is never part of
/// the git-staged path list below — so a source that itself succeeded
/// within an otherwise-failed run stays resumable on the next `compile`
/// (see `select_sources`), it just doesn't get bundled/committed until a
/// subsequent clean run.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn report_and_commit(
    vault_root: &std::path::Path,
    report: &okf_mcp::compiler::CompileReport,
    commit_summary: &str,
    fix: bool,
    assume_yes: bool,
    model_spec: &str,
    options: &compiler::CompileOptions,
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

    let mut lint_report = report.lint_report.clone();
    let mut fixed_paths: Vec<String> = Vec::new();
    let mut synthesized_slugs: Vec<String> = Vec::new();

    if fix {
        let (mechanical, after_mechanical) = validator::fix_bundle(vault_root)?;
        if !mechanical.is_empty() {
            output.line(&validator::fix::summary_line(&mechanical));
            fixed_paths.extend(mechanical.fixed_frontmatter_typos.iter().cloned());
        }
        lint_report = after_mechanical;

        if !lint_report.broken_links.is_empty() {
            let link_fix =
                compiler::fix_broken_links(vault_root, model_spec, options, Some(&output)).await?;
            output.line(&compiler::link_fix::summary_line(&link_fix));
            synthesized_slugs = link_fix
                .synthesized_slugs()
                .into_iter()
                .map(str::to_string)
                .collect();
            fixed_paths.extend(link_fix.touched_paths.iter().filter_map(|path| {
                path.strip_prefix(vault_root)
                    .ok()
                    .map(|relative| relative.to_string_lossy().replace('\\', "/"))
            }));
            lint_report = validator::lint_bundle(vault_root)?;
        }
    }

    if lint_report.has_errors() {
        output.line(&okf_mcp::validator::report::to_text(&lint_report));
    }

    if report.sources_failed() > 0 || lint_report.has_errors() {
        output.line(&format!(
            "{} source(s) failed / lint found errors — not committing; fix and re-run compile.",
            report.sources_failed()
        ));
        // Best-effort, same as every other `wiki_log::append` call site —
        // this human-readable summary must never itself turn a real
        // failure into a *different* error (or mask the original one).
        let _ = okf_mcp::storage::wiki_log::append(
            vault_root,
            "report_and_commit",
            "error",
            &format!(
                "{} source(s) failed, lint errors: {}",
                report.sources_failed(),
                lint_report.has_errors()
            ),
        );
        anyhow::bail!("compile finished with errors");
    }

    // Logged before the bundle/commit below so `wiki/log.md`'s own write
    // lands on disk in time to be included in `paths` and committed
    // alongside everything else this run touched.
    let _ = okf_mcp::storage::wiki_log::append(
        vault_root,
        "report_and_commit",
        "ok",
        &format!(
            "{} source(s) compiled, {} path(s) fixed",
            report.sources_processed(),
            fixed_paths.len()
        ),
    );

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
    paths.extend(fixed_paths);
    paths.push("wiki/index.md".to_string());
    paths.push("wiki/schema.md".to_string());
    // `wiki/log.md` is written best-effort above (`let _ = ...append(...)`)
    // and must stay optional here too: only stage it when it actually
    // exists, so a run where the append silently failed (or the file is
    // gitignored) still commits everything else instead of `git commit`
    // bailing on a missing pathspec and this whole successful run going
    // uncommitted.
    if okf_mcp::storage::fs_ops::exists(vault_root, "wiki/log.md") {
        paths.push("wiki/log.md".to_string());
    }
    if let Ok(relative) = bundle_path.strip_prefix(vault_root) {
        paths.push(relative.to_string_lossy().replace('\\', "/"));
    }
    paths.sort();
    paths.dedup();

    if git::is_git_repository(vault_root) {
        if !synthesized_slugs.is_empty() {
            let message = format!(
                "--fix synthesized {} new concept page(s) via LLM: {}. Commit these changes now?",
                synthesized_slugs.len(),
                synthesized_slugs
                    .iter()
                    .map(|slug| format!("[[{slug}]]"))
                    .collect::<Vec<_>>()
                    .join(", ")
            );
            if !confirm_commit(message, assume_yes).await? {
                output.line(
                    "Fix changes written but not committed — review and `git commit` \
                     yourself, or re-run with --yes.",
                );
                return Ok(());
            }
        }

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

    use okf_mcp::compiler::{CompileOptions, CompileReport};

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

    #[tokio::test]
    async fn a_failed_source_skips_the_bundle_and_the_commit() {
        let vault = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(vault.path().join(".okf")).unwrap();

        let result = report_and_commit(
            vault.path(),
            &failing_report(),
            "okf-mcp compile",
            false,
            false,
            "anthropic/claude-3-5-sonnet",
            &CompileOptions::default(),
        )
        .await;

        assert!(result.is_err());
        assert!(!vault.path().join("okf.json").exists());

        // Even a failed run is logged — with an "error" status — not just
        // silently dropped.
        let log_md = std::fs::read_to_string(vault.path().join("wiki/log.md")).unwrap();
        assert!(log_md.contains("[report_and_commit] error:"));
    }

    #[tokio::test]
    async fn lint_errors_skip_the_bundle_and_the_commit_even_with_no_failed_sources() {
        let vault = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(vault.path().join(".okf")).unwrap();

        let mut report = clean_report();
        report.sources[0].error = None;
        report.lint_report.broken_links =
            vec![("wiki/concepts/a.md".to_string(), "missing".to_string())];

        let result = report_and_commit(
            vault.path(),
            &report,
            "okf-mcp compile",
            false,
            false,
            "anthropic/claude-3-5-sonnet",
            &CompileOptions::default(),
        )
        .await;

        assert!(result.is_err());
        assert!(!vault.path().join("okf.json").exists());
    }

    #[tokio::test]
    async fn a_clean_report_writes_the_bundle_and_commits() {
        let vault = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(vault.path().join(".okf")).unwrap();
        init_repo(vault.path());
        // In the real `compiler::compile` flow, `regenerate_index` writes
        // both of these (`wiki/index.md` and, via `ensure_wiki_schema`,
        // `wiki/schema.md`) before `report_and_commit` ever runs.
        std::fs::create_dir_all(vault.path().join("wiki")).unwrap();
        std::fs::write(vault.path().join("wiki/index.md"), "# Wiki Index\n").unwrap();
        std::fs::write(vault.path().join("wiki/schema.md"), "# Wiki Schema\n").unwrap();

        let result = report_and_commit(
            vault.path(),
            &clean_report(),
            "okf-mcp compile",
            false,
            false,
            "anthropic/claude-3-5-sonnet",
            &CompileOptions::default(),
        )
        .await;

        assert!(result.is_ok(), "{result:?}");
        assert!(vault.path().join("okf.json").exists());
        let log = run_git(vault.path(), &["log", "--oneline"]);
        assert!(!String::from_utf8_lossy(&log.stdout).trim().is_empty());

        // wiki/log.md must exist AND be part of the same commit — not just
        // written to disk and forgotten (the whole point of adding it to
        // `paths` explicitly, since `report_and_commit` builds that list by
        // hand rather than `git add -A`).
        let log_md = std::fs::read_to_string(vault.path().join("wiki/log.md")).unwrap();
        assert!(log_md.contains("[report_and_commit] ok:"));
        let show = run_git(vault.path(), &["show", "--stat", "--format=", "HEAD"]);
        let show_text = String::from_utf8_lossy(&show.stdout);
        assert!(show_text.contains("wiki/log.md"));
        // wiki/schema.md is the other file `ensure_wiki_schema` creates
        // outside of `run_compile_sources`'s `touched_paths` tracking — it
        // must be staged and committed by the same hand-built `paths` list,
        // not silently left untracked forever.
        assert!(show_text.contains("wiki/schema.md"));
    }

    #[tokio::test]
    async fn fix_true_with_nothing_fixable_behaves_like_fix_false() {
        let vault = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(vault.path().join(".okf")).unwrap();
        init_repo(vault.path());
        std::fs::create_dir_all(vault.path().join("wiki")).unwrap();
        std::fs::write(vault.path().join("wiki/index.md"), "# Wiki Index\n").unwrap();
        std::fs::write(vault.path().join("wiki/schema.md"), "# Wiki Schema\n").unwrap();

        let result = report_and_commit(
            vault.path(),
            &clean_report(),
            "okf-mcp compile",
            true,
            false,
            "anthropic/claude-3-5-sonnet",
            &CompileOptions::default(),
        )
        .await;

        assert!(result.is_ok(), "{result:?}");
        assert!(vault.path().join("okf.json").exists());
    }

    #[tokio::test]
    async fn fix_true_leaves_a_source_missing_its_dot_md_extension_untouched_since_it_already_resolves()
     {
        // `validator::rules::missing_sources` now resolves `sources:` by
        // `raw_id`, not literal filename, so a resource missing its `.md`
        // extension already resolves and lint reports no error — there's
        // nothing left for `--fix`'s mechanical pass to repair here, and
        // the page must be committed byte-for-byte as written.
        let vault = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(vault.path().join(".okf")).unwrap();
        std::fs::create_dir_all(vault.path().join("raw")).unwrap();
        std::fs::write(vault.path().join("raw/raw_aaa.md"), "content").unwrap();
        std::fs::create_dir_all(vault.path().join("wiki/concepts")).unwrap();
        let original = "---\nokf_version: \"0.2\"\ntype: concept\nid: concept_a\ntitle: \"a\"\nsources:\n  - resource: \"/raw/raw_aaa\"\n---\n\n# a\n";
        std::fs::write(vault.path().join("wiki/concepts/a.md"), original).unwrap();
        std::fs::write(vault.path().join("wiki/index.md"), "# Wiki Index\n").unwrap();
        std::fs::write(vault.path().join("wiki/schema.md"), "# Wiki Schema\n").unwrap();
        init_repo(vault.path());

        let result = report_and_commit(
            vault.path(),
            &clean_report(),
            "okf-mcp compile",
            true,
            false,
            "anthropic/claude-3-5-sonnet",
            &CompileOptions::default(),
        )
        .await;

        assert!(result.is_ok(), "{result:?}");
        let content = std::fs::read_to_string(vault.path().join("wiki/concepts/a.md")).unwrap();
        assert_eq!(content, original);
        let log = run_git(vault.path(), &["log", "--oneline"]);
        assert!(!String::from_utf8_lossy(&log.stdout).trim().is_empty());
    }
}
