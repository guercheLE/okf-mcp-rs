//! Source selection and the actual per-source/bounded-concurrency compile
//! run — split out of `driver.rs` so that file is just the orchestration
//! (`compile`), not every stage's own logic.

use std::collections::HashSet;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::compiler::concurrency::run_bounded;
use crate::compiler::operations::{apply_operations, parse_compile_payload};
use crate::compiler::prompts::{
    COMPILER_SYSTEM_PROMPT, RawBlob, WikiPageRef, build_compile_user_prompt,
};
use crate::compiler::provider::LLMCompilerDriver;
use crate::compiler::token_estimate::{self, DEFAULT_PROMPT_TOKEN_BUDGET};
use crate::core::output::{Output, ProgressEvent};
use crate::core::vault_resolver::wiki_content_dirs;
use crate::manifest::{self, Manifest};
use crate::search::hybrid_search;
use crate::storage::fs_ops;
use crate::validator::frontmatter::parse_wiki_page;
use crate::validator::rules::markdown_files_in;

use super::{CompileOptions, SourceOutcome};

fn referenced_raw_ids(vault_root: &Path) -> anyhow::Result<HashSet<String>> {
    let mut ids = HashSet::new();
    let mut content_paths = Vec::new();
    for (dir, _kind) in wiki_content_dirs(vault_root) {
        content_paths.extend(markdown_files_in(&dir)?);
    }
    for path in content_paths {
        let content = std::fs::read_to_string(&path)?;
        if let Ok(parsed) = parse_wiki_page(&content) {
            for source in &parsed.frontmatter.sources {
                if let Some(stem) = Path::new(&source.resource)
                    .file_stem()
                    .and_then(|stem| stem.to_str())
                {
                    ids.insert(stem.to_string());
                }
            }
        }
    }
    Ok(ids)
}

/// `diff_only` = every `ACTIVE` manifest entry that's neither (a) already
/// referenced by some wiki page's `sources:` — a safety net for vaults
/// compiled before `compiled_hash` existed, where the wiki already
/// reflects a source but the manifest doesn't know it yet — nor (b)
/// marked `compiled_hash == active_hash` in the manifest, the
/// authoritative "fully compiled at its current content" signal
/// (`Manifest::is_compiled_at_current_hash`). Otherwise (a `rebuild
/// --force`) every `ACTIVE` entry, regardless of either check.
pub(crate) fn select_sources(
    vault_root: &Path,
    manifest: &Manifest,
    diff_only: bool,
) -> anyhow::Result<Vec<(String, String)>> {
    let active: Vec<(String, String)> = manifest
        .active_entries()
        .map(|(uri, version)| (uri.to_string(), version.raw_id.clone()))
        .collect();
    if !diff_only {
        return Ok(active);
    }
    let referenced = referenced_raw_ids(vault_root)?;
    Ok(active
        .into_iter()
        .filter(|(uri, raw_id)| {
            !referenced.contains(raw_id) && !manifest.is_compiled_at_current_hash(uri)
        })
        .collect())
}

pub(crate) fn read_raw_body(vault_root: &Path, raw_id: &str) -> anyhow::Result<String> {
    let path = crate::ingest::frontmatter::resolve_raw_path(vault_root, raw_id)?;
    let content = std::fs::read_to_string(&path)
        .map_err(|err| anyhow::anyhow!("cannot read raw file for '{raw_id}': {err}"))?;
    let (_, body) = crate::search::query::raw_title_and_body(&content);
    Ok(body)
}

/// The `SourceVersion` immediately before the currently-active one in
/// `uri`'s history, if any — the "superseded previous source" the design's
/// diff-synthesis flow (Q5) compares against.
pub(crate) fn superseded_raw_id(manifest: &Manifest, uri: &str) -> Option<String> {
    let history = &manifest.sources.get(uri)?.history;
    if history.len() < 2 {
        return None;
    }
    Some(history[history.len() - 2].raw_id.clone())
}

pub(crate) async fn related_wiki_pages(
    vault_root: &Path,
    raw_content: &str,
) -> anyhow::Result<Vec<WikiPageRef>> {
    // Capped: this is a search *query*, not the full document — a huge raw
    // page would otherwise dominate the query with irrelevant tail content.
    let query: String = raw_content.chars().take(500).collect();
    let results = hybrid_search(vault_root, &query, 5)?;
    let mut pages = Vec::new();
    for result in results {
        if let Ok(content) = fs_ops::read_to_string(vault_root, &result.path) {
            pages.push(WikiPageRef {
                path: result.path,
                content,
            });
        }
    }
    Ok(pages)
}

/// Fetches the active/superseded raw bodies and related wiki pages for one
/// source, then hands them to `token_estimate` to keep the assembled prompt
/// within [`DEFAULT_PROMPT_TOKEN_BUDGET`] before it's ever sent to the LLM:
/// [`token_estimate::trim_related_pages`] drops the least-relevant related
/// pages first, and
/// [`token_estimate::truncate_raw_blobs_to_shared_budget`] caps the active
/// and superseded raw bodies (with a visible truncation marker on whichever
/// side gets cut) against one *shared* budget — not each independently
/// against the full budget — so two raw blobs that are each individually
/// under budget but jointly over it still can't sneak an oversized prompt
/// past this function. `position`/`total`/`on_progress` are purely for the
/// "this source's prompt is large" progress line below — they play no part
/// in which source gets processed.
#[allow(clippy::too_many_arguments)]
pub(super) async fn compile_one_source(
    vault_root: &Path,
    driver: &LLMCompilerDriver,
    model_spec: &str,
    options: &CompileOptions,
    superseded_raw_id: Option<&str>,
    uri: &str,
    raw_id: &str,
    on_progress: Option<&Output>,
    position: usize,
    total: usize,
) -> anyhow::Result<Vec<PathBuf>> {
    let raw_content = read_raw_body(vault_root, raw_id)?;
    let superseded_full = match superseded_raw_id {
        Some(prev_id) => read_raw_body(vault_root, prev_id)
            .ok()
            .map(|content| RawBlob {
                id: prev_id.to_string(),
                content,
            }),
        None => None,
    };
    let active_full = RawBlob {
        id: raw_id.to_string(),
        content: raw_content,
    };

    let related_full = related_wiki_pages(vault_root, &active_full.content).await?;

    // Checked against the *untrimmed* sizes, before any of the capping
    // below — post-cap the estimate is always <= budget by construction,
    // which would make this check meaningless. This is what surfaces which
    // sources are expensive across a long multi-thousand-file run.
    let estimated_tokens = token_estimate::estimate_prompt_tokens(
        &active_full,
        superseded_full.as_ref(),
        &related_full,
    );
    if estimated_tokens > DEFAULT_PROMPT_TOKEN_BUDGET
        && let Some(output) = on_progress
    {
        output.line(&format!(
            "[{position}/{total}] {uri}: prompt is large (~{estimated_tokens} tokens, budget \
             {DEFAULT_PROMPT_TOKEN_BUDGET}) — trimming related pages / truncating body"
        ));
    }

    let (active_raw, superseded) = token_estimate::truncate_raw_blobs_to_shared_budget(
        active_full,
        superseded_full,
        DEFAULT_PROMPT_TOKEN_BUDGET,
    );
    let related = token_estimate::trim_related_pages(
        related_full,
        &active_raw,
        superseded.as_ref(),
        DEFAULT_PROMPT_TOKEN_BUDGET,
    );

    let user_prompt = build_compile_user_prompt(&active_raw, superseded.as_ref(), &related);

    let response = driver
        .execute_compile_prompt(
            model_spec,
            COMPILER_SYSTEM_PROMPT,
            &user_prompt,
            options.temperature,
            options.max_tokens,
            options.base_url_override.as_deref(),
            options.api_key_env_override.as_deref(),
        )
        .await?;
    let payload = parse_compile_payload(&response)?;
    apply_operations(vault_root, &payload)
}

/// One source's position/uri/raw_id, carried through
/// [`run_compile_sources`]'s bounded-concurrency dispatch so the `on_result`
/// callback can report progress and persist the manifest without needing to
/// look anything back up by index.
struct SourceJob {
    position: usize,
    uri: String,
    raw_id: String,
}

/// A completed [`SourceJob`]'s outcome — `outcome`'s error is a `String`
/// (not `anyhow::Error`) purely so this type stays `Send + 'static` for
/// `run_bounded`'s `R` bound without needing `anyhow::Error: Sync` to hold.
struct SourceJobResult {
    position: usize,
    uri: String,
    raw_id: String,
    outcome: Result<Vec<PathBuf>, String>,
}

/// Runs every `(uri, raw_id)` in `sources` through `compile_fn`
/// (`(position, total, uri, raw_id) -> Result<touched_paths>`), at up to
/// `concurrency` sources in flight at once, via
/// [`crate::compiler::concurrency::run_bounded`]. See
/// [`super::compile`]'s doc comment for the concurrency/cross-linking
/// trade-off this enables.
///
/// Fires `ProgressEvent::Started` the moment a source is dispatched (its
/// concurrency-limit permit acquired, right before `compile_fn` starts) and
/// `ProgressEvent::Finished` when it completes. At `concurrency == 1` this
/// produces byte-for-byte the same event sequence, in the same order, as a
/// plain sequential loop — `run_bounded`'s own "sliding window" guarantees
/// strict input-order dispatch-then-completion at `limit == 1` (the next
/// source's task isn't even created until the previous one has both
/// finished *and* released its permit).
///
/// Persists each successful source's manifest entry (`mark_compiled` +
/// `manifest::store::save`) immediately as its result arrives, and strictly
/// one at a time no matter `concurrency`: both calls only ever run inside
/// `run_bounded`'s `on_result` callback, which `run_bounded` drives
/// synchronously from the single task awaiting `join_set.join_next()` —
/// never from inside a spawned per-source task — so two sources' manifest
/// writes can never interleave. This is the invariant crash-resumability
/// depends on (see [`super::compile`]'s doc comment).
///
/// Fails fast on the first `manifest::store::save` error, matching the old
/// sequential loop's `manifest::store::save(...)?` inline in its `for`
/// loop: once a save fails, `on_result` returns `false`, which tells
/// `run_bounded` to stop dispatching any source that hasn't started yet —
/// no further LLM calls, no further `apply_operations` file writes. At
/// `concurrency == 1` this is byte-for-byte identical to the old loop
/// returning `Err` immediately (zero further sources run). At
/// `concurrency > 1`, any sources already in flight at the moment the save
/// failed can't be un-started — they still run to completion, and their
/// successful `apply_operations` writes still land on disk — but they are
/// the last ones that ever will; every source that hadn't yet been
/// dispatched is skipped entirely.
///
/// Generic over `compile_fn` purely so this function's
/// concurrency/manifest-serialization/progress-ordering behavior can be
/// exercised in tests with a fast synthetic fake: `LLMCompilerDriver` calls
/// `genai::Client` directly with no trait seam to mock, so [`super::compile`]
/// (the only real caller) passes a closure that calls
/// [`compile_one_source`].
pub(super) async fn run_compile_sources<F, Fut>(
    vault_root: &Path,
    mut manifest: Manifest,
    sources: Vec<(String, String)>,
    concurrency: usize,
    on_progress: Option<&Output>,
    compile_fn: F,
) -> anyhow::Result<(Manifest, Vec<SourceOutcome>, Vec<PathBuf>)>
where
    F: Fn(usize, usize, String, String) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = anyhow::Result<Vec<PathBuf>>> + Send + 'static,
{
    let total = sources.len();
    let jobs: Vec<SourceJob> = sources
        .into_iter()
        .enumerate()
        .map(|(index, (uri, raw_id))| SourceJob {
            position: index + 1,
            uri,
            raw_id,
        })
        .collect();

    // `Output` is `Copy`, so a plain copy (not a borrow) is all a `'static`
    // worker closure needs — no lifetime gymnastics required.
    let output = on_progress.copied();
    let compile_fn = Arc::new(compile_fn);

    let mut outcomes = Vec::with_capacity(total);
    let mut touched_paths = Vec::new();
    let mut manifest_save_error: Option<anyhow::Error> = None;

    let _ = run_bounded(
        jobs,
        concurrency,
        move |job| {
            let compile_fn = Arc::clone(&compile_fn);
            async move {
                if let Some(output) = output {
                    output.line(
                        &ProgressEvent::Started {
                            index: job.position,
                            total,
                            label: format!("compiling {}", job.uri),
                        }
                        .to_string(),
                    );
                }
                let outcome = compile_fn(job.position, total, job.uri.clone(), job.raw_id.clone())
                    .await
                    .map_err(|err| err.to_string());
                SourceJobResult {
                    position: job.position,
                    uri: job.uri,
                    raw_id: job.raw_id,
                    outcome,
                }
            }
        },
        |_idx, result: &SourceJobResult| {
            if let Some(output) = output {
                output.line(
                    &ProgressEvent::Finished {
                        index: result.position,
                        total,
                        label: format!("compiling {}", result.uri),
                        error: result.outcome.as_ref().err().cloned(),
                    }
                    .to_string(),
                );
            }

            if let Ok(paths) = &result.outcome {
                // Marked and saved immediately, per source — not batched
                // until every source has completed — so a crash/kill
                // partway through a run leaves already-succeeded sources
                // durably resumable on the next `compile`. Independent of
                // `report_and_commit`'s later pass/fail gate: manifest.json
                // is never part of the git-staged path list, so it must
                // keep persisting regardless of whether the overall run
                // later gets treated as failed. Serialized across every
                // concurrency level — see this function's doc comment.
                if manifest_save_error.is_none() {
                    manifest.mark_compiled(&result.uri);
                    if let Err(err) = manifest::store::save(vault_root, &manifest) {
                        manifest_save_error = Some(err);
                    }
                }
                touched_paths.extend(paths.iter().cloned());
            }

            outcomes.push(SourceOutcome {
                uri: result.uri.clone(),
                raw_id: result.raw_id.clone(),
                error: result.outcome.as_ref().err().cloned(),
            });

            // Once a manifest save has failed, tell `run_bounded` to stop
            // dispatching any source that hasn't started yet — see this
            // function's doc comment.
            manifest_save_error.is_none()
        },
    )
    .await;

    if let Some(err) = manifest_save_error {
        return Err(err);
    }

    Ok((manifest, outcomes, touched_paths))
}
