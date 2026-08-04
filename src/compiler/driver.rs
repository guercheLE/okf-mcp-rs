//! Orchestrates `okf-mcp compile`/`rebuild`: select raw sources -> build
//! prompt -> call the LLM -> apply operations -> lint -> regenerate index.

use std::collections::HashSet;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::core::output::{Output, ProgressEvent};
use crate::core::vault_config::load_vault_config;
use crate::manifest::{self, Manifest};
use crate::search::hybrid_search;
use crate::storage::fs_ops;
use crate::validator::frontmatter::parse_wiki_page;
use crate::validator::rules::markdown_files_in;
use crate::validator::{LintReport, lint_bundle};

use super::concurrency::run_bounded;
use super::operations::{apply_operations, parse_compile_payload};
use super::prompts::{COMPILER_SYSTEM_PROMPT, RawBlob, WikiPageRef, build_compile_user_prompt};
use super::provider::LLMCompilerDriver;
use super::token_estimate::{self, DEFAULT_PROMPT_TOKEN_BUDGET};

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

/// `--model`/MCP `model` -> `.okf/config.toml`'s `[compiler].default_model`
/// -> error. No "guess from whichever env var is set" fallback (see
/// `compiler::provider`'s top comment for why).
pub fn resolve_model_spec(vault_root: &Path, explicit: Option<&str>) -> anyhow::Result<String> {
    if let Some(spec) = explicit {
        return Ok(spec.to_string());
    }
    let config = load_vault_config(vault_root)?;
    config.compiler.default_model.ok_or_else(|| {
        anyhow::anyhow!(
            "no model specified — pass --model <provider>/<model>, or set \
             [compiler].default_model in .okf/config.toml"
        )
    })
}

/// Resolves `.okf/config.toml`'s `[providers.<provider>]` entry (if any)
/// for `model_spec`'s provider into a `CompileOptions` override — the
/// vault-level counterpart to `--model`/MCP `base_url_override`/etc.
/// `provider_spec`'s own hardcoded env-var names/defaults still apply
/// whenever the vault has no matching `[providers.<name>]` entry (returns
/// `CompileOptions::default()` in that case, today's exact behavior).
pub fn vault_provider_options(
    vault_root: &Path,
    model_spec: &str,
) -> anyhow::Result<CompileOptions> {
    let (provider, _) = super::provider::parse_model_spec(model_spec)?;
    vault_provider_options_for_provider(vault_root, provider)
}

/// Same lookup as `vault_provider_options`, but keyed directly on a
/// provider name rather than a full `<provider>/<model>` spec — for
/// callers (e.g. `okf-mcp models <provider>`) that don't have a model name
/// on hand yet, since listing available models is exactly how a user picks
/// one.
///
/// Falls back to the process-level `custom_providers` map (populated by
/// `okf-mcp setup`'s `.env`/config.yml persistence — see
/// `config_manager::env_overrides`/`Config::custom_providers`) when the
/// vault's own `.okf/config.toml` has no matching `[providers.<name>]`
/// entry — vault-level config stays higher precedence (more specific),
/// matching this codebase's existing CLI > env > local > global pattern.
pub fn vault_provider_options_for_provider(
    vault_root: &Path,
    provider: &str,
) -> anyhow::Result<CompileOptions> {
    let vault_config = load_vault_config(vault_root)?;
    let provider_config = vault_config.providers.get(provider);

    let mut base_url_override = provider_config.and_then(|p| p.base_url.clone());
    let api_key_env_override = provider_config.and_then(|p| p.api_key_env.clone());

    if base_url_override.is_none() {
        // Lenient: a malformed process config.yml shouldn't block a
        // resolution that's otherwise fully specified by the vault.
        base_url_override = crate::core::config_manager::load_config(serde_json::Map::new())
            .ok()
            .and_then(|config| {
                config
                    .custom_providers
                    .get(provider)
                    .map(|e| e.base_url.clone())
            });
    }

    // `default_max_tokens` (serde `#[serde(default = ...)]`) guarantees
    // `config.compiler.max_tokens` is always present, even for a vault with
    // no `.okf/config.toml` at all (`OkfVaultConfig::default()`) or one
    // whose `[compiler]` table omits `max_tokens` — so this is always
    // `Some`, never a "no config" gap the way `temperature: None` above is
    // (see this function's doc comment: that gap is a separate, pre-existing
    // issue, out of scope here).
    let max_tokens = Some(vault_config.compiler.max_tokens);

    Ok(CompileOptions {
        temperature: None,
        base_url_override,
        api_key_env_override,
        max_tokens,
        ..CompileOptions::default()
    })
}

/// Lists a provider's available model names — the same lookup
/// `execute_compile_prompt`'s model-not-found hint uses, exposed directly
/// as its own operation for `okf-mcp models <provider>`.
pub async fn list_models(
    provider: &str,
    base_url_override: Option<&str>,
    api_key_env_override: Option<&str>,
) -> anyhow::Result<Vec<String>> {
    super::provider::LLMCompilerDriver::new()
        .list_models(provider, base_url_override, api_key_env_override)
        .await
}

fn referenced_raw_ids(vault_root: &Path) -> anyhow::Result<HashSet<String>> {
    let mut ids = HashSet::new();
    for path in markdown_files_in(&vault_root.join("wiki/concepts"))? {
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
fn select_sources(
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
    let content = fs_ops::read_to_string(vault_root, &format!("raw/{raw_id}.md"))?;
    let (_, body) = crate::search::query::raw_title_and_body(&content);
    Ok(body)
}

/// The `SourceVersion` immediately before the currently-active one in
/// `uri`'s history, if any — the "superseded previous source" the design's
/// diff-synthesis flow (Q5) compares against.
fn superseded_raw_id(manifest: &Manifest, uri: &str) -> Option<String> {
    let history = &manifest.sources.get(uri)?.history;
    if history.len() < 2 {
        return None;
    }
    Some(history[history.len() - 2].raw_id.clone())
}

async fn related_wiki_pages(
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
async fn compile_one_source(
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
/// [`super::concurrency::run_bounded`]. See [`compile`]'s doc comment for
/// the concurrency/cross-linking trade-off this enables.
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
/// depends on (see [`compile`]'s doc comment).
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
/// `genai::Client` directly with no trait seam to mock, so [`compile`] (the
/// only real caller) passes a closure that calls [`compile_one_source`].
async fn run_compile_sources<F, Fut>(
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

/// Deterministic table-of-contents regeneration: sorted by title, so
/// re-running `compile` without any actual content change doesn't produce
/// git diff noise from incidental ordering.
fn regenerate_index(vault_root: &Path) -> anyhow::Result<()> {
    let mut entries = Vec::new();
    for path in markdown_files_in(&vault_root.join("wiki/concepts"))? {
        let content = std::fs::read_to_string(&path)?;
        if let Ok(parsed) = parse_wiki_page(&content) {
            let relative = path
                .strip_prefix(vault_root)
                .unwrap_or(&path)
                .to_string_lossy()
                .replace('\\', "/");
            entries.push((
                parsed.frontmatter.title,
                relative,
                parsed.frontmatter.description.unwrap_or_default(),
            ));
        }
    }
    entries.sort();

    let mut markdown = String::from("# Wiki Index\n\n");
    for (title, path, description) in entries {
        if description.is_empty() {
            markdown.push_str(&format!("- [{title}]({path})\n"));
        } else {
            markdown.push_str(&format!("- [{title}]({path}) — {description}\n"));
        }
    }
    fs_ops::write(vault_root, "wiki/index.md", &markdown)?;
    Ok(())
}

/// `diff_only=true` is `okf-mcp compile`; `diff_only=false` is `okf-mcp
/// rebuild --force`. Applies each source's operations as soon as that
/// source's LLM call succeeds (per-source apply-and-continue) — one bad
/// response doesn't block the rest of the run; the post-compile `lint`
/// pass (and, upstream of this function, a git commit) are the safety net
/// that surfaces any resulting inconsistency.
///
/// `options.concurrency` (default `1`, fully sequential) controls how many
/// sources are compiled at once, via [`run_compile_sources`]'s bounded
/// runner. Trade-off worth knowing before raising it: each source's
/// [`related_wiki_pages`] search runs against whatever's already on disk at
/// the moment that source is dispatched — for sources compiled in the same
/// concurrent wave (`concurrency > 1`), that snapshot doesn't include any
/// other in-flight source's not-yet-applied pages, so a source in wave N
/// can miss a cross-link to a concept another source in the *same* wave is
/// about to create. Sequential (`concurrency == 1`) compiles don't have
/// this gap, since each source's own writes land before the next source's
/// search runs. Either way, this is the same class of imperfection this
/// function already tolerates for a single bad LLM response: the post-loop
/// `lint` pass below (and `--fix`'s LLM-assisted broken-link repair) is the
/// safety net that catches and repairs whatever cross-links a wave missed,
/// rather than this function trying to guarantee a perfect graph in one
/// pass.
pub async fn compile(
    vault_root: &Path,
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

    Ok(CompileReport {
        sources: outcomes,
        touched_paths,
        lint_report,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::credential_storage::HOME_ENV_TEST_LOCK;

    /// Runs `f` with `HOME` redirected to a fresh temp dir, so
    /// `vault_provider_options_for_provider`'s process-level
    /// `custom_providers` fallback (which reads `~/.okf-mcp/config.yml`)
    /// can't pick up whatever real global config happens to exist on the
    /// machine running these tests. Serialized via the same
    /// `HOME_ENV_TEST_LOCK` `config_manager.rs`'s and
    /// `credential_storage.rs`'s own `HOME`-mutating tests already share.
    fn with_isolated_home<T>(f: impl FnOnce(&std::path::Path) -> T) -> T {
        let _guard = HOME_ENV_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let prev_home = std::env::var_os("HOME");
        // SAFETY: test-only env mutation, serialized by HOME_ENV_TEST_LOCK.
        unsafe {
            std::env::set_var("HOME", dir.path());
        }
        let result = f(dir.path());
        // SAFETY: same guard as above.
        unsafe {
            match prev_home {
                Some(prev) => std::env::set_var("HOME", prev),
                None => std::env::remove_var("HOME"),
            }
        }
        result
    }

    #[test]
    fn resolve_model_spec_prefers_the_explicit_flag() {
        let vault = tempfile::tempdir().unwrap();
        let spec = resolve_model_spec(vault.path(), Some("anthropic/claude-3-5-sonnet")).unwrap();
        assert_eq!(spec, "anthropic/claude-3-5-sonnet");
    }

    #[test]
    fn resolve_model_spec_falls_back_to_vault_config() {
        let vault = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(vault.path().join(".okf")).unwrap();
        std::fs::write(
            vault.path().join(".okf/config.toml"),
            "[compiler]\ndefault_model = \"groq/llama-3.3-70b\"\n",
        )
        .unwrap();

        let spec = resolve_model_spec(vault.path(), None).unwrap();
        assert_eq!(spec, "groq/llama-3.3-70b");
    }

    #[test]
    fn resolve_model_spec_errors_when_nothing_is_configured() {
        let vault = tempfile::tempdir().unwrap();
        assert!(resolve_model_spec(vault.path(), None).is_err());
    }

    #[test]
    fn vault_provider_options_reads_the_matching_providers_base_url_and_api_key_env() {
        let vault = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(vault.path().join(".okf")).unwrap();
        std::fs::write(
            vault.path().join(".okf/config.toml"),
            "[providers.custom]\napi_key_env = \"MY_CUSTOM_KEY\"\nbase_url = \"http://localhost:9999\"\n",
        )
        .unwrap();

        let options = vault_provider_options(vault.path(), "custom/some-model").unwrap();
        assert_eq!(
            options.base_url_override.as_deref(),
            Some("http://localhost:9999")
        );
        assert_eq!(
            options.api_key_env_override.as_deref(),
            Some("MY_CUSTOM_KEY")
        );
        assert_eq!(options.temperature, None);
        // `.okf/config.toml` has no `[compiler]` table at all here — serde's
        // `default_max_tokens` still fills it in, so this is always `Some`.
        assert_eq!(options.max_tokens, Some(4096));
        assert_eq!(options.concurrency, 1);
    }

    #[test]
    fn vault_provider_options_reads_a_configured_max_tokens_value() {
        let vault = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(vault.path().join(".okf")).unwrap();
        std::fs::write(
            vault.path().join(".okf/config.toml"),
            "[compiler]\nmax_tokens = 8192\n",
        )
        .unwrap();

        let options = vault_provider_options(vault.path(), "anthropic/claude-3-5-sonnet").unwrap();
        assert_eq!(options.max_tokens, Some(8192));
    }

    #[test]
    fn vault_provider_options_is_empty_for_a_provider_with_no_matching_entry() {
        with_isolated_home(|_home| {
            let vault = tempfile::tempdir().unwrap();
            std::fs::create_dir_all(vault.path().join(".okf")).unwrap();
            std::fs::write(
                vault.path().join(".okf/config.toml"),
                "[providers.custom]\napi_key_env = \"MY_CUSTOM_KEY\"\n",
            )
            .unwrap();

            let options =
                vault_provider_options(vault.path(), "anthropic/claude-3-5-sonnet").unwrap();
            assert_eq!(options.base_url_override, None);
            assert_eq!(options.api_key_env_override, None);
        });
    }

    #[test]
    fn vault_provider_options_is_empty_for_a_vault_with_no_config_at_all() {
        with_isolated_home(|_home| {
            let vault = tempfile::tempdir().unwrap();
            let options =
                vault_provider_options(vault.path(), "anthropic/claude-3-5-sonnet").unwrap();
            assert_eq!(options.base_url_override, None);
            assert_eq!(options.api_key_env_override, None);
        });
    }

    #[test]
    fn vault_provider_options_for_provider_falls_back_to_the_process_level_custom_providers_map() {
        with_isolated_home(|home| {
            std::fs::create_dir_all(home.join(".okf-mcp")).unwrap();
            std::fs::write(
                home.join(".okf-mcp/config.yml"),
                "custom_providers:\n  myvllm:\n    base_url: https://global.example/v1\n",
            )
            .unwrap();

            // A vault with no `.okf/config.toml` at all — nothing to
            // shadow the process-level fallback.
            let vault = tempfile::tempdir().unwrap();
            let options = vault_provider_options_for_provider(vault.path(), "myvllm").unwrap();
            assert_eq!(
                options.base_url_override.as_deref(),
                Some("https://global.example/v1")
            );
        });
    }

    #[test]
    fn vault_provider_options_for_provider_prefers_the_vault_entry_over_the_process_level_fallback()
    {
        with_isolated_home(|home| {
            std::fs::create_dir_all(home.join(".okf-mcp")).unwrap();
            std::fs::write(
                home.join(".okf-mcp/config.yml"),
                "custom_providers:\n  myvllm:\n    base_url: https://global.example/v1\n",
            )
            .unwrap();

            let vault = tempfile::tempdir().unwrap();
            std::fs::create_dir_all(vault.path().join(".okf")).unwrap();
            std::fs::write(
                vault.path().join(".okf/config.toml"),
                "[providers.myvllm]\nbase_url = \"https://vault.example/v1\"\n",
            )
            .unwrap();

            let options = vault_provider_options_for_provider(vault.path(), "myvllm").unwrap();
            assert_eq!(
                options.base_url_override.as_deref(),
                Some("https://vault.example/v1")
            );
        });
    }

    #[test]
    fn vault_provider_options_for_provider_reads_the_same_entry_by_bare_provider_name() {
        // The `okf-mcp models <provider>` path doesn't have a model name to
        // extract a provider from — it calls this directly instead of
        // going through `vault_provider_options`'s `<provider>/<model>`
        // split, and must resolve to the same override.
        let vault = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(vault.path().join(".okf")).unwrap();
        std::fs::write(
            vault.path().join(".okf/config.toml"),
            "[providers.custom]\napi_key_env = \"MY_CUSTOM_KEY\"\nbase_url = \"http://localhost:9999\"\n",
        )
        .unwrap();

        let options = vault_provider_options_for_provider(vault.path(), "custom").unwrap();
        assert_eq!(
            options.base_url_override.as_deref(),
            Some("http://localhost:9999")
        );
        assert_eq!(
            options.api_key_env_override.as_deref(),
            Some("MY_CUSTOM_KEY")
        );
    }

    fn write_concept(vault_root: &Path, slug: &str, sources: &[&str]) {
        let dir = vault_root.join("wiki/concepts");
        std::fs::create_dir_all(&dir).unwrap();
        let sources_yaml = if sources.is_empty() {
            String::new()
        } else {
            let entries: String = sources
                .iter()
                .map(|s| format!("  - resource: \"{s}\"\n"))
                .collect();
            format!("sources:\n{entries}")
        };
        let content = format!(
            "---\nokf_version: \"0.2\"\ntype: concept\nid: concept_{slug}\ntitle: \"{slug}\"\ndescription: \"about {slug}\"\n{sources_yaml}---\n\n# {slug}\n"
        );
        std::fs::write(dir.join(format!("{slug}.md")), content).unwrap();
    }

    #[test]
    fn select_sources_diff_only_skips_already_referenced_raw_ids() {
        let vault = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(vault.path().join(".okf")).unwrap();
        let mut manifest = Manifest::default();
        manifest.record_ingest("uri-a", "sha256:aaa", "raw_aaa", "t0");
        manifest.record_ingest("uri-b", "sha256:bbb", "raw_bbb", "t0");

        write_concept(vault.path(), "already-compiled", &["/raw/raw_aaa.md"]);

        let selected = select_sources(vault.path(), &manifest, true).unwrap();
        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].1, "raw_bbb");
    }

    #[test]
    fn select_sources_diff_only_also_skips_sources_already_compiled_at_the_current_hash() {
        let vault = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(vault.path().join(".okf")).unwrap();
        let mut manifest = Manifest::default();
        manifest.record_ingest("uri-a", "sha256:aaa", "raw_aaa", "t0");
        manifest.record_ingest("uri-b", "sha256:bbb", "raw_bbb", "t0");
        // Marked compiled but with NO wiki page written for it — only the
        // new compiled_hash check (not the older referenced-raw-ids scan)
        // can catch this one.
        manifest.mark_compiled("uri-a");

        let selected = select_sources(vault.path(), &manifest, true).unwrap();
        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].1, "raw_bbb");
    }

    #[test]
    fn select_sources_without_diff_only_returns_every_active_entry() {
        let vault = tempfile::tempdir().unwrap();
        let mut manifest = Manifest::default();
        manifest.record_ingest("uri-a", "sha256:aaa", "raw_aaa", "t0");
        manifest.record_ingest("uri-b", "sha256:bbb", "raw_bbb", "t0");
        write_concept(vault.path(), "already-compiled", &["/raw/raw_aaa.md"]);

        let selected = select_sources(vault.path(), &manifest, false).unwrap();
        assert_eq!(selected.len(), 2);
    }

    #[test]
    fn select_sources_excludes_tombstoned_entries() {
        let vault = tempfile::tempdir().unwrap();
        let mut manifest = Manifest::default();
        manifest.record_ingest("uri-a", "sha256:aaa", "raw_aaa", "t0");
        manifest.tombstone("uri-a", "gone", "t1").unwrap();

        let selected = select_sources(vault.path(), &manifest, false).unwrap();
        assert!(selected.is_empty());
    }

    #[test]
    fn superseded_raw_id_finds_the_entry_before_the_active_one() {
        let mut manifest = Manifest::default();
        manifest.record_ingest("uri", "sha256:aaa", "raw_aaa", "t0");
        manifest.record_ingest("uri", "sha256:bbb", "raw_bbb", "t1");

        assert_eq!(
            superseded_raw_id(&manifest, "uri").as_deref(),
            Some("raw_aaa")
        );
    }

    #[test]
    fn superseded_raw_id_is_none_for_a_first_time_ingest() {
        let mut manifest = Manifest::default();
        manifest.record_ingest("uri", "sha256:aaa", "raw_aaa", "t0");
        assert_eq!(superseded_raw_id(&manifest, "uri"), None);
    }

    #[test]
    fn regenerate_index_lists_pages_sorted_by_title_with_descriptions() {
        let vault = tempfile::tempdir().unwrap();
        write_concept(vault.path(), "zebra", &[]);
        write_concept(vault.path(), "apple", &[]);

        regenerate_index(vault.path()).unwrap();

        let index = fs_ops::read_to_string(vault.path(), "wiki/index.md").unwrap();
        let apple_pos = index.find("apple").unwrap();
        let zebra_pos = index.find("zebra").unwrap();
        assert!(apple_pos < zebra_pos);
        assert!(index.contains("about apple"));
    }

    #[test]
    fn regenerate_index_on_an_empty_wiki_produces_a_header_only_file() {
        let vault = tempfile::tempdir().unwrap();
        regenerate_index(vault.path()).unwrap();
        let index = fs_ops::read_to_string(vault.path(), "wiki/index.md").unwrap();
        assert_eq!(index.trim(), "# Wiki Index");
    }

    #[test]
    fn regenerate_index_omits_the_dash_separator_when_description_is_empty() {
        // `write_concept` above always sets a description — this covers the
        // other arm of `regenerate_index`'s `if description.is_empty()`
        // branch, which drops the "— description" suffix entirely rather
        // than rendering a dangling "— ".
        let vault = tempfile::tempdir().unwrap();
        let dir = vault.path().join("wiki/concepts");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("bare.md"),
            "---\nokf_version: \"0.2\"\ntype: concept\nid: concept_bare\ntitle: \"bare\"\ndescription: \"\"\n---\n\n# bare\n",
        )
        .unwrap();

        regenerate_index(vault.path()).unwrap();

        let index = fs_ops::read_to_string(vault.path(), "wiki/index.md").unwrap();
        assert_eq!(
            index.trim(),
            "# Wiki Index\n\n- [bare](wiki/concepts/bare.md)".trim()
        );
        assert!(!index.contains("—"));
    }

    #[test]
    fn compile_options_default_concurrency_is_one_not_zero() {
        // `usize::default()` is 0, which would starve `run_bounded`'s
        // sliding window of any permits and never compile anything — the
        // manual `Default` impl must force this to 1.
        assert_eq!(CompileOptions::default().concurrency, 1);
    }

    /// (1) `run_compile_sources` at `concurrency == 1` must dispatch AND
    /// complete every source in strict input order — the hard regression
    /// requirement that raising `CompileOptions::concurrency` never changes
    /// today's default (sequential) behavior. The fake `compile_fn` records
    /// the `(position, total, uri)` it's called with, which happens right
    /// after this function's `ProgressEvent::Started` fires for that same
    /// source — so this also pins that Started/Finished pairs never
    /// interleave across sources at `concurrency == 1`.
    #[tokio::test]
    async fn run_compile_sources_at_concurrency_one_dispatches_and_completes_in_input_order() {
        let vault = tempfile::tempdir().unwrap();

        let mut manifest = Manifest::default();
        manifest.record_ingest("uri-a", "sha256:aaa", "raw_aaa", "t0");
        manifest.record_ingest("uri-b", "sha256:bbb", "raw_bbb", "t0");
        manifest.record_ingest("uri-c", "sha256:ccc", "raw_ccc", "t0");

        let sources = vec![
            ("uri-a".to_string(), "raw_aaa".to_string()),
            ("uri-b".to_string(), "raw_bbb".to_string()),
            ("uri-c".to_string(), "raw_ccc".to_string()),
        ];

        let observed = Arc::new(std::sync::Mutex::new(Vec::new()));
        let observed_for_fn = Arc::clone(&observed);
        let compile_fn = move |position: usize, total: usize, uri: String, _raw_id: String| {
            let observed = Arc::clone(&observed_for_fn);
            async move {
                observed.lock().unwrap().push((position, total, uri));
                Ok(Vec::new())
            }
        };

        let (manifest, outcomes, _touched) =
            run_compile_sources(vault.path(), manifest, sources, 1, None, compile_fn)
                .await
                .unwrap();

        assert_eq!(
            *observed.lock().unwrap(),
            vec![
                (1, 3, "uri-a".to_string()),
                (2, 3, "uri-b".to_string()),
                (3, 3, "uri-c".to_string()),
            ],
            "concurrency == 1 must dispatch/complete in strict input order, matching the plain \
             sequential loop it replaced"
        );
        assert_eq!(outcomes.len(), 3);
        assert!(outcomes.iter().all(|o| o.error.is_none()));
        for uri in ["uri-a", "uri-b", "uri-c"] {
            assert!(manifest.is_compiled_at_current_hash(uri));
        }
    }

    /// (2) Under real concurrency (`concurrency > 1`), every source must
    /// still end up marked in the manifest exactly once — none dropped,
    /// none double-processed — proving `run_compile_sources`'s manifest
    /// writes stay serialized (see its doc comment) even when its
    /// `compile_fn` calls genuinely overlap in time.
    #[tokio::test]
    async fn run_compile_sources_under_concurrency_marks_every_source_exactly_once() {
        let vault = tempfile::tempdir().unwrap();

        const N: usize = 20;
        let mut manifest = Manifest::default();
        let mut sources = Vec::with_capacity(N);
        for i in 0..N {
            let uri = format!("uri-{i}");
            let raw_id = format!("raw_{i}");
            manifest.record_ingest(&uri, &format!("sha256:{i}"), &raw_id, "t0");
            sources.push((uri, raw_id));
        }

        let call_counts: Arc<std::sync::Mutex<std::collections::HashMap<String, usize>>> =
            Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));
        let call_counts_for_fn = Arc::clone(&call_counts);
        let compile_fn = move |_position: usize, _total: usize, uri: String, _raw_id: String| {
            let call_counts = Arc::clone(&call_counts_for_fn);
            async move {
                // Yield so tasks genuinely interleave under real
                // concurrency, rather than trivially completing
                // synchronously in dispatch order.
                tokio::task::yield_now().await;
                *call_counts.lock().unwrap().entry(uri).or_insert(0) += 1;
                Ok(Vec::new())
            }
        };

        let (manifest, outcomes, _touched) =
            run_compile_sources(vault.path(), manifest, sources, 6, None, compile_fn)
                .await
                .unwrap();

        assert_eq!(
            outcomes.len(),
            N,
            "every source must produce exactly one outcome — none dropped"
        );
        let counts = call_counts.lock().unwrap();
        assert_eq!(counts.len(), N, "no source should be dropped");
        for (uri, count) in counts.iter() {
            assert_eq!(
                *count, 1,
                "{uri} must be compiled exactly once, not {count}"
            );
        }
        for i in 0..N {
            let uri = format!("uri-{i}");
            assert!(
                manifest.is_compiled_at_current_hash(&uri),
                "{uri} must be marked compiled in the manifest"
            );
        }
    }

    /// A failing source doesn't stop the rest of the run, and is reported
    /// with its error rather than silently dropped — same contract the old
    /// sequential loop had.
    #[tokio::test]
    async fn run_compile_sources_reports_a_failure_without_dropping_other_sources() {
        let vault = tempfile::tempdir().unwrap();

        let mut manifest = Manifest::default();
        manifest.record_ingest("uri-ok", "sha256:aaa", "raw_aaa", "t0");
        manifest.record_ingest("uri-bad", "sha256:bbb", "raw_bbb", "t0");

        let sources = vec![
            ("uri-ok".to_string(), "raw_aaa".to_string()),
            ("uri-bad".to_string(), "raw_bbb".to_string()),
        ];

        let compile_fn = |_position: usize, _total: usize, uri: String, _raw_id: String| async move {
            if uri == "uri-bad" {
                anyhow::bail!("synthetic failure");
            }
            Ok(Vec::new())
        };

        let (manifest, outcomes, _touched) =
            run_compile_sources(vault.path(), manifest, sources, 1, None, compile_fn)
                .await
                .unwrap();

        assert_eq!(outcomes.len(), 2);
        let ok = outcomes.iter().find(|o| o.uri == "uri-ok").unwrap();
        let bad = outcomes.iter().find(|o| o.uri == "uri-bad").unwrap();
        assert!(ok.error.is_none());
        assert_eq!(bad.error.as_deref(), Some("synthetic failure"));
        assert!(manifest.is_compiled_at_current_hash("uri-ok"));
        assert!(!manifest.is_compiled_at_current_hash("uri-bad"));
    }

    /// A `manifest::store::save` failure must stop dispatch of every
    /// not-yet-started source, matching the old sequential loop's
    /// `manifest::store::save(...)?` returning immediately on the first
    /// save failure — no further LLM calls (here: no further `compile_fn`
    /// calls) after the failure. Forces the save to fail deterministically
    /// by making `.okf` a regular file instead of a directory, so
    /// `manifest::store::save`'s `create_dir_all(".okf")` errors.
    #[tokio::test]
    async fn run_compile_sources_stops_dispatching_after_a_manifest_save_failure() {
        let vault = tempfile::tempdir().unwrap();
        // `.okf` is a *file*, not a directory — `manifest::store::save`'s
        // `create_dir_all` will fail on every call for this vault.
        std::fs::write(vault.path().join(".okf"), "not a directory").unwrap();

        let mut manifest = Manifest::default();
        manifest.record_ingest("uri-a", "sha256:aaa", "raw_aaa", "t0");
        manifest.record_ingest("uri-b", "sha256:bbb", "raw_bbb", "t0");
        manifest.record_ingest("uri-c", "sha256:ccc", "raw_ccc", "t0");

        let sources = vec![
            ("uri-a".to_string(), "raw_aaa".to_string()),
            ("uri-b".to_string(), "raw_bbb".to_string()),
            ("uri-c".to_string(), "raw_ccc".to_string()),
        ];

        let dispatched = Arc::new(std::sync::Mutex::new(Vec::new()));
        let dispatched_for_fn = Arc::clone(&dispatched);
        let compile_fn = move |_position: usize, _total: usize, uri: String, _raw_id: String| {
            let dispatched = Arc::clone(&dispatched_for_fn);
            async move {
                dispatched.lock().unwrap().push(uri);
                Ok(Vec::new())
            }
        };

        let result =
            run_compile_sources(vault.path(), manifest, sources, 1, None, compile_fn).await;

        assert!(
            result.is_err(),
            "a manifest save failure must surface as an error, same as the old loop"
        );
        assert_eq!(
            *dispatched.lock().unwrap(),
            vec!["uri-a".to_string()],
            "only the first source should ever have been dispatched — the save failure on \
             uri-a must stop uri-b/uri-c from being compiled at all, matching the old \
             sequential loop's fail-fast behavior at concurrency == 1"
        );
    }

    /// Finds the first occurrence of `needle` in `haystack`, byte-wise —
    /// used to locate the end of an HTTP request's headers (`\r\n\r\n`) in
    /// [`read_http_request`] below.
    fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
        haystack.windows(needle.len()).position(|w| w == needle)
    }

    /// Reads one full HTTP/1.1 request (headers + `Content-Length` body)
    /// off `stream`, looping until every declared body byte has arrived —
    /// a single fixed-size `read()` (as `compiler::provider`'s own mock
    /// server helper uses for its small, fixed test payloads) isn't
    /// reliable here since some of the tests below deliberately send a
    /// multi-hundred-KB prompt body that TCP may deliver across several
    /// reads.
    async fn read_http_request(stream: &mut tokio::net::TcpStream) -> String {
        use tokio::io::AsyncReadExt;

        let mut buf: Vec<u8> = Vec::new();
        let mut chunk = [0u8; 65_536];
        loop {
            match stream.read(&mut chunk).await {
                Ok(0) => break,
                Ok(n) => {
                    buf.extend_from_slice(&chunk[..n]);
                    if let Some(header_end) = find_subslice(&buf, b"\r\n\r\n") {
                        let header_str = String::from_utf8_lossy(&buf[..header_end]);
                        let content_length: usize = header_str
                            .lines()
                            .find_map(|line| {
                                let (name, value) = line.split_once(':')?;
                                name.trim()
                                    .eq_ignore_ascii_case("content-length")
                                    .then(|| value.trim().parse().ok())
                                    .flatten()
                            })
                            .unwrap_or(0);
                        let body_have = buf.len() - (header_end + 4);
                        if body_have >= content_length {
                            break;
                        }
                    }
                }
                Err(_) => break,
            }
        }
        String::from_utf8_lossy(&buf).to_string()
    }

    /// Minimal HTTP/1.1 server that answers every request it receives
    /// (looping `accept()` rather than the single-shot pattern
    /// `compiler::provider`'s own mock server uses, since the tests below
    /// drive it through the real `LLMCompilerDriver` — an OpenAI-compatible
    /// `chat/completions` call per compiled source) with a fixed
    /// `response_body`. When `captured` is `Some`, each request's raw text
    /// (headers + body) is appended to it, in arrival order, so a test can
    /// assert on exactly what prompt `compile_one_source`/`compile` sent.
    async fn mock_llm_server(
        response_body: String,
        captured: Option<Arc<std::sync::Mutex<Vec<String>>>>,
    ) -> String {
        use tokio::io::AsyncWriteExt;
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
                let request = read_http_request(&mut stream).await;
                if let Some(sink) = &captured {
                    sink.lock().unwrap().push(request);
                }
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    response_body.len(),
                    response_body
                );
                let _ = stream.write_all(response.as_bytes()).await;
            }
        });
        format!("http://{address}")
    }

    /// An OpenAI-compatible `chat/completions` response body whose
    /// `choices[0].message.content` is `payload` — `execute_compile_prompt`
    /// pulls its return value straight out of that field via
    /// `response.first_text()`.
    fn chat_completion_response(payload: &str) -> String {
        serde_json::json!({
            "id": "chatcmpl-test",
            "object": "chat.completion",
            "created": 0,
            "model": "test-model",
            "choices": [{
                "index": 0,
                "message": {"role": "assistant", "content": payload},
                "finish_reason": "stop",
            }],
        })
        .to_string()
    }

    /// A `{"operations": [...]}` `CompilePayload` JSON string that writes
    /// one concept page — `operations::parse_compile_payload`'s expected
    /// shape.
    fn compile_payload_creating(path: &str, content: &str) -> String {
        serde_json::json!({
            "operations": [{
                "action": "CREATE_OR_UPDATE",
                "path": path,
                "content": content,
            }],
        })
        .to_string()
    }

    /// Sets an env var to a dummy, meaningfully-non-blank value before
    /// `f` runs, then removes it — used to seed the synthetic/overridden
    /// API-key env var these tests' ad-hoc OpenAI-compatible provider
    /// resolves to, so `resolve_provider_target`'s
    /// `seed_env_from_credential_storage` short-circuits on
    /// `env_var_is_meaningfully_set` and never touches the real OS
    /// keychain (the exact hang this task's `--skip` flags route around
    /// for other, pre-existing tests).
    ///
    /// Takes the not-yet-polled future itself (constructing an `async`
    /// block/fn call never runs its body — only polling, i.e. `.await`,
    /// does) rather than a closure returning one: awaiting `fut` here,
    /// before removing the env var, is what guarantees the var is still
    /// set for the whole call, including the `.await` points inside it —
    /// a closure returning the future without this function itself
    /// awaiting it would remove the var before the future is ever polled.
    async fn with_dummy_api_key<T>(env_var: &str, fut: impl Future<Output = T>) -> T {
        // SAFETY: test-only env mutation; each caller below uses a unique
        // env var name, so this can't race with another test's env var.
        unsafe {
            std::env::set_var(env_var, "dummy-test-key");
        }
        let result = fut.await;
        unsafe {
            std::env::remove_var(env_var);
        }
        result
    }

    /// (1) The "happy path" through `compile_one_source`: a superseded raw
    /// source is present (exercises `superseded_raw_id`'s `Some` arm), a
    /// related wiki page genuinely matches via a real `hybrid_search` call
    /// (exercises `related_wiki_pages`'s result loop, not just its empty
    /// case), the assembled prompt stays under budget (no truncation), and
    /// the mocked LLM's response is parsed and applied to disk. Captures
    /// the literal HTTP request `compile_one_source` sent so this also
    /// pins that the superseded source's content and the related page's
    /// content actually made it into the prompt — not just that some
    /// prompt was sent.
    #[tokio::test]
    async fn compile_one_source_includes_superseded_source_and_related_pages_then_applies_the_response()
     {
        let vault = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(vault.path().join("raw")).unwrap();
        std::fs::write(
            vault.path().join("raw/raw_active.md"),
            "# Active Gizmo\n\nThe active gizmothingamajig revision spins twice as fast.\n",
        )
        .unwrap();
        std::fs::write(
            vault.path().join("raw/raw_prev.md"),
            "# Previous Gizmo\n\nSUPERSEDED_MARKER_TEXT gizmothingamajig revision one.\n",
        )
        .unwrap();
        write_concept(vault.path(), "gizmothingamajig", &[]);
        // BM25-index the wiki page above so `hybrid_search` (called by
        // `related_wiki_pages`) can actually find it — a fresh vault has
        // no index at all until something builds one.
        crate::search::reindex(vault.path(), false, None).unwrap();

        let response_content = compile_payload_creating(
            "wiki/concepts/gizmothingamajig.md",
            "---\nokf_version: \"0.2\"\ntype: concept\nid: concept_gizmothingamajig\ntitle: \"gizmothingamajig\"\ndescription: \"about gizmothingamajig\"\nsources:\n  - resource: \"/raw/raw_active.md\"\n---\n\n# gizmothingamajig\n",
        );
        let captured = Arc::new(std::sync::Mutex::new(Vec::new()));
        let base_url = mock_llm_server(
            chat_completion_response(&response_content),
            Some(Arc::clone(&captured)),
        )
        .await;

        let env_var = "OKF_TEST_DRIVER_KEY_HAPPY_PATH";
        let options = CompileOptions {
            api_key_env_override: Some(env_var.to_string()),
            base_url_override: Some(base_url),
            ..CompileOptions::default()
        };

        let driver = LLMCompilerDriver::new();
        let touched = with_dummy_api_key(
            env_var,
            compile_one_source(
                vault.path(),
                &driver,
                "driver-test-happy/some-model",
                &options,
                Some("raw_prev"),
                "https://example.com/gizmo",
                "raw_active",
                None,
                1,
                1,
            ),
        )
        .await
        .unwrap();

        // Compared with `ends_with` rather than an exact `PathBuf` equality
        // against `vault.path().join(...)`: on macOS `/tmp` is itself a
        // symlink to `/private/tmp`, and `sandbox_path` canonicalizes the
        // vault root, so the two otherwise-equivalent paths can differ in
        // that non-canonical prefix.
        assert_eq!(touched.len(), 1);
        assert!(touched[0].ends_with("wiki/concepts/gizmothingamajig.md"));
        assert!(
            vault
                .path()
                .join("wiki/concepts/gizmothingamajig.md")
                .is_file()
        );

        let requests = captured.lock().unwrap();
        assert_eq!(requests.len(), 1, "exactly one LLM call for one source");
        assert!(
            requests[0].contains("gizmothingamajig revision spins"),
            "the active source's own content must be in the prompt"
        );
        assert!(
            requests[0].contains("SUPERSEDED_MARKER_TEXT"),
            "the superseded source's content must be in the prompt too"
        );
        assert!(
            requests[0].contains("about gizmothingamajig")
                || requests[0].contains("# gizmothingamajig"),
            "the related wiki page hybrid_search found must be included in the prompt"
        );
    }

    /// (2) An active source whose content alone is bigger than
    /// `DEFAULT_PROMPT_TOKEN_BUDGET` must (a) still succeed end-to-end
    /// rather than sending an unbounded prompt, (b) emit the "prompt is
    /// large" progress line (`on_progress: Some`), and (c) actually arrive
    /// at the LLM truncated — not just estimated as oversized locally.
    /// Also exercises `superseded_raw_id`'s `None` arm (no superseded
    /// source at all), the opposite of test (1) above.
    #[tokio::test]
    async fn compile_one_source_truncates_an_oversized_active_source_and_still_succeeds() {
        let vault = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(vault.path().join("raw")).unwrap();
        // Comfortably over `DEFAULT_PROMPT_TOKEN_BUDGET` (100_000 tokens ~=
        // 400_000 chars at the `chars/4` heuristic) on its own.
        let huge_body = "x".repeat(450_000);
        std::fs::write(
            vault.path().join("raw/raw_huge.md"),
            format!("# Huge\n\n{huge_body}\n"),
        )
        .unwrap();

        let response_content = compile_payload_creating(
            "wiki/concepts/huge.md",
            "---\nokf_version: \"0.2\"\ntype: concept\nid: concept_huge\ntitle: \"huge\"\ndescription: \"about huge\"\nsources:\n  - resource: \"/raw/raw_huge.md\"\n---\n\n# huge\n",
        );
        let captured = Arc::new(std::sync::Mutex::new(Vec::new()));
        let base_url = mock_llm_server(
            chat_completion_response(&response_content),
            Some(Arc::clone(&captured)),
        )
        .await;

        let env_var = "OKF_TEST_DRIVER_KEY_OVERSIZED";
        let options = CompileOptions {
            api_key_env_override: Some(env_var.to_string()),
            base_url_override: Some(base_url),
            ..CompileOptions::default()
        };

        let driver = LLMCompilerDriver::new();
        let output = Output::cli();
        let touched = with_dummy_api_key(
            env_var,
            compile_one_source(
                vault.path(),
                &driver,
                "driver-test-oversized/some-model",
                &options,
                None,
                "https://example.com/huge",
                "raw_huge",
                Some(&output),
                1,
                1,
            ),
        )
        .await
        .unwrap();

        // See the sibling test above for why this is `ends_with` rather
        // than an exact `PathBuf` equality (macOS `/tmp` symlink
        // canonicalization).
        assert_eq!(touched.len(), 1);
        assert!(touched[0].ends_with("wiki/concepts/huge.md"));

        let requests = captured.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert!(
            requests[0].contains("truncated") && requests[0].contains("chars over budget"),
            "the oversized active source must have been truncated before it ever reached the \
             LLM, with a visible truncation marker"
        );
        assert!(
            requests[0].len() < huge_body.len(),
            "the request actually sent must be meaningfully smaller than the untruncated body"
        );
    }

    /// (3) `compile()` itself — the one real caller of
    /// `run_compile_sources`/`compile_one_source` — reaches an LLM (here,
    /// the mocked OpenAI-compatible endpoint) and applies its response,
    /// with `on_progress: Some` so this also exercises
    /// `run_compile_sources`'s `ProgressEvent::Started`/`Finished` output
    /// branches (every other test in this module passes `on_progress:
    /// None`).
    #[tokio::test]
    async fn compile_end_to_end_reaches_a_mocked_llm_and_applies_its_operations() {
        let vault = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(vault.path().join("raw")).unwrap();
        std::fs::write(
            vault.path().join("raw/raw_widget.md"),
            "# Widget\n\nThe widget spins.\n",
        )
        .unwrap();

        let mut manifest = Manifest::default();
        manifest.record_ingest(
            "https://example.com/widget",
            "sha256:aaa",
            "raw_widget",
            "t0",
        );
        manifest::store::save(vault.path(), &manifest).unwrap();

        let response_content = compile_payload_creating(
            "wiki/concepts/widget.md",
            "---\nokf_version: \"0.2\"\ntype: concept\nid: concept_widget\ntitle: \"widget\"\ndescription: \"about widget\"\nsources:\n  - resource: \"/raw/raw_widget.md\"\n---\n\n# widget\n",
        );
        let base_url = mock_llm_server(chat_completion_response(&response_content), None).await;

        let env_var = "OKF_TEST_DRIVER_KEY_COMPILE_E2E";
        let options = CompileOptions {
            api_key_env_override: Some(env_var.to_string()),
            base_url_override: Some(base_url),
            ..CompileOptions::default()
        };

        let output = Output::cli();
        let report = with_dummy_api_key(
            env_var,
            compile(
                vault.path(),
                "driver-test-e2e/some-model",
                false,
                &options,
                Some(&output),
            ),
        )
        .await
        .unwrap();

        assert_eq!(report.sources_processed(), 1);
        assert_eq!(report.sources_failed(), 0);
        assert!(vault.path().join("wiki/concepts/widget.md").is_file());
        assert!(
            report
                .touched_paths
                .iter()
                .any(|p| p.ends_with("wiki/concepts/widget.md"))
        );
        assert!(vault.path().join("wiki/index.md").is_file());

        let saved_manifest = manifest::store::load(vault.path()).unwrap();
        assert!(saved_manifest.is_compiled_at_current_hash("https://example.com/widget"));
    }
}
