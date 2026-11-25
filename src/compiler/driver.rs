//! Orchestrates `okf-mcp compile`/`rebuild`: select raw sources -> build
//! prompt -> call the LLM -> apply operations -> lint -> regenerate index.
//!
//! Each stage lives in its own submodule — `provider_resolution` (which
//! model/provider a vault wants), `pipeline` (source selection and the
//! actual bounded-concurrency compile run), `indexing` (regenerating
//! `wiki/index.md`) — so this file is just [`compile`] itself, the shared
//! types every stage passes around, and the re-exports that keep
//! `compiler::driver::X` resolving exactly where it always has for callers
//! like `compiler::mod.rs`, `cli::compile`, `compiler::synthesize`, and
//! `compiler::link_fix`.

mod indexing;
mod pipeline;
mod provider_resolution;

use std::path::PathBuf;
use std::sync::Arc;

use crate::compiler::provider::LLMCompilerDriver;
use crate::core::output::Output;
use crate::manifest;
use crate::validator::{LintReport, lint_bundle};

pub use indexing::{ensure_wiki_schema, regenerate_index};
pub use provider_resolution::{
    list_models, resolve_model_spec, vault_provider_options, vault_provider_options_for_provider,
};
// `pub(crate)`, not `pub`: used by sibling modules (`synthesize.rs`,
// `link_fix.rs`) via `super::driver::{...}`, but never outside this crate.
pub(crate) use pipeline::{read_raw_body, related_wiki_pages, select_sources, superseded_raw_id};
// Only this file's own `compile()` below calls these — no re-export needed.
use pipeline::{compile_one_source, run_compile_sources};

#[derive(Debug, Clone)]
pub struct CompileOptions {
    pub temperature: Option<f32>,
    pub base_url_override: Option<String>,
    pub api_key_env_override: Option<String>,
    /// Passed straight through to `ChatOptions::with_max_tokens` when
    /// `Some` — see `vault_provider_options_for_provider`, which populates
    /// this from `.okf/config.toml`'s `[compiler].max_tokens`.
    pub max_tokens: Option<u32>,
    /// Maximum number of sources compiled concurrently by `compile()`'s
    /// main loop, via `compiler::concurrency::run_bounded`. Must never be
    /// `0` (that would deadlock/do no work) — `Default` below forces `1`
    /// (today's fully-sequential behavior), and `run_bounded` itself also
    /// treats `0` as `1` as a second line of defense.
    pub concurrency: usize,
}

impl Default for CompileOptions {
    fn default() -> Self {
        Self {
            temperature: None,
            base_url_override: None,
            api_key_env_override: None,
            max_tokens: None,
            // usize::default() is 0, which would starve `run_bounded`'s
            // sliding window of any permits — must be forced to 1 here
            // rather than relying on a derived `Default`.
            concurrency: 1,
        }
    }
}

#[derive(Debug)]
pub struct SourceOutcome {
    pub uri: String,
    pub raw_id: String,
    pub error: Option<String>,
}

pub struct CompileReport {
    pub sources: Vec<SourceOutcome>,
    pub touched_paths: Vec<PathBuf>,
    pub lint_report: LintReport,
}

impl CompileReport {
    pub fn sources_processed(&self) -> usize {
        self.sources.iter().filter(|s| s.error.is_none()).count()
    }

    pub fn sources_failed(&self) -> usize {
        self.sources.iter().filter(|s| s.error.is_some()).count()
    }
}

/// `diff_only=true` is `okf-mcp compile`; `diff_only=false` is `okf-mcp
/// rebuild --force`. Applies each source's operations as soon as that
/// source's LLM call succeeds (per-source apply-and-continue) — one bad
/// response doesn't block the rest of the run; the post-compile `lint`
/// pass (and, upstream of this function, a git commit) are the safety net
/// that surfaces any resulting inconsistency.
///
/// `options.concurrency` (default `1`, fully sequential) controls how many
/// sources are compiled at once, via [`pipeline::run_compile_sources`]'s
/// bounded runner. Trade-off worth knowing before raising it: each
/// source's [`pipeline::related_wiki_pages`] search runs against whatever's
/// already on disk at the moment that source is dispatched — for sources
/// compiled in the same concurrent wave (`concurrency > 1`), that snapshot
/// doesn't include any other in-flight source's not-yet-applied pages, so
/// a source in wave N can miss a cross-link to a concept another source in
/// the *same* wave is about to create. Sequential (`concurrency == 1`)
/// compiles don't have this gap, since each source's own writes land
/// before the next source's search runs. Either way, this is the same
/// class of imperfection this function already tolerates for a single bad
/// LLM response: the post-loop `lint` pass below (and `--fix`'s
/// LLM-assisted broken-link repair) is the safety net that catches and
/// repairs whatever cross-links a wave missed, rather than this function
/// trying to guarantee a perfect graph in one pass.
pub async fn compile(
    vault_root: &std::path::Path,
    model_spec: &str,
    diff_only: bool,
    options: &CompileOptions,
    on_progress: Option<&Output>,
) -> anyhow::Result<CompileReport> {
    let manifest = manifest::store::load(vault_root)?;
    let sources = select_sources(vault_root, &manifest, diff_only)?;

    // Precomputed once, up front, from the manifest snapshot at the start
    // of this run — safe to share read-only across every concurrently
    // dispatched source because `superseded_raw_id` only reads a URI's
    // `history`, which `mark_compiled` (called per-source as this run
    // proceeds, see `run_compile_sources`) never touches. So every source
    // in this run sees the same "previous version" regardless of dispatch/
    // completion order or concurrency.
    let superseded_by_uri: std::collections::HashMap<String, Option<String>> = sources
        .iter()
        .map(|(uri, _)| (uri.clone(), superseded_raw_id(&manifest, uri)))
        .collect();
    let superseded_by_uri = Arc::new(superseded_by_uri);

    let vault_root_owned = Arc::new(vault_root.to_path_buf());
    let driver = Arc::new(LLMCompilerDriver::new());
    let model_spec_owned: Arc<str> = Arc::from(model_spec);
    let options_owned = Arc::new(options.clone());
    // `Output` is `Copy`; a plain copy is all the `'static` `compile_fn`
    // closure below needs to keep emitting the "large prompt" progress line.
    let output = on_progress.copied();

    let compile_fn = move |position: usize, total: usize, uri: String, raw_id: String| {
        let vault_root = Arc::clone(&vault_root_owned);
        let driver = Arc::clone(&driver);
        let model_spec = Arc::clone(&model_spec_owned);
        let options = Arc::clone(&options_owned);
        let superseded_by_uri = Arc::clone(&superseded_by_uri);
        async move {
            let superseded = superseded_by_uri.get(&uri).cloned().flatten();
            compile_one_source(
                &vault_root,
                &driver,
                &model_spec,
                &options,
                superseded.as_deref(),
                &uri,
                &raw_id,
                output.as_ref(),
                position,
                total,
            )
            .await
        }
    };

    let (_manifest, outcomes, touched_paths) = run_compile_sources(
        vault_root,
        manifest,
        sources,
        options.concurrency,
        on_progress,
        compile_fn,
    )
    .await?;

    regenerate_index(vault_root)?;
    let lint_report = lint_bundle(vault_root)?;

    let processed = outcomes.iter().filter(|s| s.error.is_none()).count();
    let failed = outcomes.iter().filter(|s| s.error.is_some()).count();
    // Best-effort: sources were already compiled and written to disk either
    // way — a failure to append this human-readable summary must never
    // fail (or misrepresent) an otherwise-real compile run.
    let _ = crate::storage::wiki_log::append(
        vault_root,
        "compile",
        if failed == 0 { "ok" } else { "error" },
        &format!("{processed} source(s) processed, {failed} failed"),
    );

    Ok(CompileReport {
        sources: outcomes,
        touched_paths,
        lint_report,
    })
}

#[cfg(test)]
mod tests;
