//! The `okf-mcp` MCP tool surface: 12 fixed, well-defined tools (ingest,
//! compile, rebuild, synthesize-next, synthesize-submit, lint, reindex,
//! search, delete, read-index, read-concept, list-vaults) over a
//! Git-native OKF knowledge vault.
//!
//! Deliberately not framed as a discovery layer over an unknown API
//! surface (contrast the old mcpify-generated `search`/`get`/`call` trio
//! this replaced, which explicitly advertised itself as "backed by an
//! embedded semantic database" for exactly that reason) — every tool here
//! does one specific, named thing. `okf-search`'s hybrid BM25+vector
//! ranking is an internal implementation detail, not part of how the tool
//! is described to a caller.
//!
//! `okf-compile`/`okf-rebuild` drive an LLM *provider* (`compiler::provider`)
//! themselves — useful with a real API key, but unreliable for bulk
//! automation through a subscription-CLI-backed provider, which may have
//! its own agentic identity and decline a system-prompt-driven synthesis
//! request. `okf-synthesize-next`/`okf-synthesize-submit` invert that: they
//! hand raw material to the calling MCP client's own LLM (no separate
//! provider/API key needed) and let it submit the result back as a normal,
//! user-authorized tool call — the preferred path whenever the client
//! already has a model in the loop.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{
    CallToolResult, ContentBlock, Implementation, ProtocolVersion, ServerCapabilities, ServerInfo,
};
use rmcp::service::RequestContext;
use rmcp::transport::stdio;
use rmcp::{
    ErrorData as McpError, RoleServer, ServerHandler, ServiceExt, schemars, tool, tool_handler,
    tool_router,
};
use serde::Deserialize;
use tokio::sync::Mutex;

use crate::auth::auth_manager::{AuthManager, header_location_for};
use crate::compiler::operations::{CompileOperation, CompilePayload, apply_operations};
use crate::compiler::{self, CompileOptions, StoredJob, StoredJobKind};
use crate::core::config_schema::Config;
use crate::core::output::Output;
use crate::core::vault_registry::VaultRegistry;
use crate::core::vault_resolver::{resolve_vault, sandbox_path, wiki_content_dirs};
use crate::http::auth_extractor::extract_request_credentials;
use crate::ingest::{self, DeleteOutcome};
use crate::manifest;
use crate::search;
use crate::storage::fs_ops;
use crate::validator::frontmatter::parse_wiki_page;
use crate::validator::rules::markdown_files_in;
use crate::validator::{self, report as lint_report_format};

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct IngestArgs {
    /// URL to fetch (via Firecrawl) or local file path to read
    pub source: String,
    /// Repeatable in effect: pass a list of tags to attach
    #[serde(default)]
    pub tags: Vec<String>,
    /// Vault name (from the registry) or path; defaults to the active context
    #[serde(default)]
    pub vault: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct CompileArgs {
    /// Only compile unprocessed/changed sources (default true)
    #[serde(default)]
    pub diff: Option<bool>,
    /// "<provider>/<model_name>", e.g. "anthropic/claude-3-5-sonnet"
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub temperature: Option<f32>,
    #[serde(default)]
    pub base_url_override: Option<String>,
    /// Repair lint issues after compiling: mechanically (missing `.md`
    /// extensions on `sources:` entries, `tid:`/`id:` frontmatter typos)
    /// and, for broken links, via the LLM — synthesizing a concept page for
    /// a missing link target, grounded only in the raw sources the
    /// referencing pages already cite
    #[serde(default)]
    pub fix: Option<bool>,
    /// Compile this many sources concurrently (default 1, sequential) — see
    /// `compiler::driver::compile`'s doc comment for the cross-linking
    /// trade-off of raising it
    #[serde(default)]
    pub concurrency: Option<usize>,
    #[serde(default)]
    pub vault: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct RebuildArgs {
    /// Recompile every active source, not just unprocessed ones
    #[serde(default)]
    pub force: Option<bool>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub temperature: Option<f32>,
    #[serde(default)]
    pub base_url_override: Option<String>,
    /// Same as `CompileArgs::fix` — see its description for details
    #[serde(default)]
    pub fix: Option<bool>,
    /// Same as `CompileArgs::concurrency` — see its description for details
    #[serde(default)]
    pub concurrency: Option<usize>,
    #[serde(default)]
    pub vault: Option<String>,
}

fn default_synthesize_batch_size() -> usize {
    10
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SynthesizeNextArgs {
    /// How many jobs to return in this call (default 10, capped at 50).
    /// Batched by default, not just when asked — fewer round trips and
    /// less token overhead for the calling client
    #[serde(default = "default_synthesize_batch_size")]
    pub batch_size: usize,
    /// Only pending/changed sources (default true) — same meaning as
    /// `CompileArgs::diff`
    #[serde(default)]
    pub diff: Option<bool>,
    #[serde(default)]
    pub vault: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SynthesizeSubmitResult {
    /// A `job_id` returned by a prior `okf-synthesize-next` call
    pub job_id: String,
    pub operations: Vec<CompileOperation>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SynthesizeSubmitArgs {
    /// One entry per job being resolved in this call — a batch, mirroring
    /// `okf-synthesize-next`'s own batching. One entry failing (an expired
    /// job_id, an invalid path) does not fail the others
    pub results: Vec<SynthesizeSubmitResult>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ModelsArgs {
    /// anthropic, openai, groq, gemini, openrouter, deepseek, xai, together,
    /// ollama-cloud, moonshot, ollama, foundry-local, claude-max-api-proxy,
    /// or custom
    pub provider: String,
    #[serde(default)]
    pub vault: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct LintArgs {
    /// Also fail on warnings (orphan pages), not just broken links/sources
    #[serde(default)]
    pub strict: Option<bool>,
    /// Mechanically repair auto-fixable issues (missing `.md` extensions
    /// on `sources:` entries, `tid:`/`id:` frontmatter typos) before
    /// reporting. Never calls an LLM — see `okf-compile`/`okf-rebuild`'s
    /// `fix` for LLM-assisted broken-link repair
    #[serde(default)]
    pub fix: Option<bool>,
    #[serde(default)]
    pub vault: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ReindexArgs {
    /// Also (re)compute dense-vector embeddings, not just the text index
    #[serde(default)]
    pub embeddings: Option<bool>,
    #[serde(default)]
    pub vault: Option<String>,
}

fn default_search_limit() -> usize {
    5
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SearchArgs {
    pub query: String,
    #[serde(default = "default_search_limit")]
    pub limit: usize,
    #[serde(default)]
    pub vault: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct DeleteArgs {
    pub source: String,
    /// Hard delete: also unlinks every raw blob this source ever had
    #[serde(default)]
    pub purge: Option<bool>,
    #[serde(default)]
    pub vault: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ReadIndexArgs {
    #[serde(default)]
    pub vault: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ReadConceptArgs {
    /// A wiki page's slug (its filename without `.md`), a legacy `id:`
    /// frontmatter value, or a vault-relative wiki path
    pub id_or_path: String,
    #[serde(default)]
    pub vault: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ListVaultsArgs {}

/// A pending job never submitted within this long is dropped — a safety
/// net against an abandoned client session leaking memory forever, not
/// something normal use should ever hit.
const SYNTHESIZE_JOB_TTL: Duration = Duration::from_secs(30 * 60);
/// If the map still exceeds this many entries after the TTL sweep, the
/// oldest are evicted down to this count — bounds worst-case memory even
/// under a pathological client that never submits anything.
const SYNTHESIZE_JOB_CAP: usize = 500;

/// Checked lazily at the top of both `okf-synthesize-next` and
/// `okf-synthesize-submit` — no background task/timer needed, since a
/// session that never calls either tool again has nothing left to evict
/// anyway.
fn evict_stale_and_excess_synthesize_jobs(jobs: &mut HashMap<String, StoredJob>) {
    let now = Instant::now();
    jobs.retain(|_, job| now.duration_since(job.created_at) < SYNTHESIZE_JOB_TTL);

    if jobs.len() > SYNTHESIZE_JOB_CAP {
        let mut by_age: Vec<(String, Instant)> = jobs
            .iter()
            .map(|(id, job)| (id.clone(), job.created_at))
            .collect();
        by_age.sort_by_key(|(_, created_at)| *created_at);
        let excess = jobs.len() - SYNTHESIZE_JOB_CAP;
        for (id, _) in by_age.into_iter().take(excess) {
            jobs.remove(&id);
        }
    }
}

/// Resolves `id_or_path` against `wiki/concepts/` and `wiki/entities/`.
/// Tries, in order: (1) a direct sandboxed path hit; (2) a filename-stem
/// match — the spec-correct behavior, since an OKF document's ID is just
/// its own file path minus `.md`, no frontmatter field needed; (3) the
/// legacy `id:` frontmatter field (now optional and no longer generated),
/// for content compiled before this repo stopped writing it. (2) never
/// needs to read a file's content, so it's tried across every candidate
/// before falling back to (3), which does.
fn find_concept_path(vault_root: &Path, id_or_path: &str) -> anyhow::Result<PathBuf> {
    if let Ok(candidate) = sandbox_path(vault_root, id_or_path)
        && candidate.is_file()
    {
        return Ok(candidate);
    }

    let mut content_paths = Vec::new();
    for dir in wiki_content_dirs(vault_root) {
        content_paths.extend(markdown_files_in(&dir)?);
    }

    for path in &content_paths {
        if path.file_stem().and_then(|stem| stem.to_str()) == Some(id_or_path) {
            return Ok(path.clone());
        }
    }

    for path in &content_paths {
        let content = std::fs::read_to_string(path)?;
        if let Ok(parsed) = parse_wiki_page(&content)
            && let Some(id) = parsed.frontmatter.id.as_deref()
            && (id == id_or_path || id == format!("concept_{id_or_path}"))
        {
            return Ok(path.clone());
        }
    }

    anyhow::bail!("no wiki concept found for '{id_or_path}'")
}

/// Shared state every tool method needs. `Clone` because rmcp constructs
/// one instance per session (see `http::server::start_http_server`'s
/// service factory).
#[derive(Clone)]
pub struct OkfServer {
    config: Config,
    auth_manager: Arc<Mutex<AuthManager>>,
    tool_router: ToolRouter<OkfServer>,
    /// Pending `okf-synthesize-next` jobs awaiting their
    /// `okf-synthesize-submit`, keyed by job ID — scoped to this one
    /// session (matches `auth_manager`'s own per-session lifetime, per
    /// `OkfServer::new` being called once per connection). Evicted lazily
    /// (see `evict_stale_and_excess_synthesize_jobs`) rather than on a
    /// background timer, so an abandoned session can't leak memory forever
    /// without needing any extra task/thread.
    synthesize_jobs: Arc<Mutex<HashMap<String, StoredJob>>>,
    /// Backs `okf-synthesize-next`'s job IDs. A plain counter, not a random
    /// token: job IDs are pure in-memory correlation handles for this one
    /// session, never a security boundary, so there's no reason to pull in
    /// an RNG dependency for them.
    synthesize_job_counter: Arc<AtomicU64>,
}

#[tool_router]
impl OkfServer {
    pub fn new(config: Config, auth_manager: Arc<Mutex<AuthManager>>) -> Self {
        Self {
            config,
            auth_manager,
            tool_router: Self::tool_router(),
            synthesize_jobs: Arc::new(Mutex::new(HashMap::new())),
            synthesize_job_counter: Arc::new(AtomicU64::new(1)),
        }
    }

    #[tool(
        name = "okf-ingest",
        description = "Fetch a URL or read a local file and add it to the vault's raw sources."
    )]
    async fn ingest(
        &self,
        Parameters(args): Parameters<IngestArgs>,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        let config = self.config.clone();
        let auth_manager = self.auth_manager.clone();
        let request_credentials = extract_http_request_credentials(&context, &config);

        self.run_tool("okf-ingest", async move {
            let vault_root = resolve_vault(args.vault.as_deref())?;
            let mut auth_manager = auth_manager.lock().await;
            let report = ingest::process_ingest(
                &args.source,
                &args.tags,
                &vault_root,
                &config,
                &mut auth_manager,
                request_credentials.as_ref(),
            )
            .await?;
            Ok(serde_json::json!({
                "source_uri": report.source_uri,
                "outcome": format!("{:?}", report.outcome),
                "raw_path": report.raw_path,
            }))
        })
        .await
    }

    #[tool(
        name = "okf-compile",
        description = "Compile newly-ingested raw sources into wiki concept/entity pages. Requires a configured LLM provider (see okf-mcp setup). If you (the calling MCP client) have your own LLM and would rather not configure a separate provider, use okf-synthesize-next/okf-synthesize-submit instead."
    )]
    async fn compile(
        &self,
        Parameters(args): Parameters<CompileArgs>,
    ) -> Result<CallToolResult, McpError> {
        let transport = self.config.transport;
        self.run_tool("okf-compile", async move {
            let vault_root = resolve_vault(args.vault.as_deref())?;
            let model_spec = compiler::resolve_model_spec(&vault_root, args.model.as_deref())?;
            // Start from the vault's `.okf/config.toml`-derived options (this
            // is what actually populates `max_tokens`/`api_key_env_override`
            // — see `compiler::vault_provider_options`), then let any
            // explicit MCP args override individual fields. Mirrors the CLI
            // path (`cli::compile::run`/`cli::rebuild::run`), which already
            // does the same thing — without this, `.okf/config.toml`'s
            // `[compiler].max_tokens` never reached MCP-triggered compiles.
            let vault_options = compiler::vault_provider_options(&vault_root, &model_spec)?;
            let options = apply_arg_overrides(
                vault_options,
                args.temperature,
                args.base_url_override,
                args.concurrency,
            );
            let diff_only = args.diff.unwrap_or(true);
            let output = Output::for_transport(transport);
            let mut report =
                compiler::compile(&vault_root, &model_spec, diff_only, &options, Some(&output))
                    .await?;
            let (mechanical_fix, link_fix) = apply_fix_if_requested(
                &vault_root,
                &model_spec,
                &options,
                &output,
                args.fix.unwrap_or(false),
                &mut report,
            )
            .await?;
            compile_report_to_json(&report, mechanical_fix.as_ref(), link_fix.as_ref())
        })
        .await
    }

    #[tool(
        name = "okf-rebuild",
        description = "Recompile the wiki from all active raw sources. Requires a configured LLM provider (see okf-mcp setup). If you (the calling MCP client) have your own LLM and would rather not configure a separate provider, use okf-synthesize-next/okf-synthesize-submit instead."
    )]
    async fn rebuild(
        &self,
        Parameters(args): Parameters<RebuildArgs>,
    ) -> Result<CallToolResult, McpError> {
        let transport = self.config.transport;
        self.run_tool("okf-rebuild", async move {
            let vault_root = resolve_vault(args.vault.as_deref())?;
            let model_spec = compiler::resolve_model_spec(&vault_root, args.model.as_deref())?;
            // See the matching comment in `compile` above — same
            // vault-config-first, args-override-second precedence as the
            // CLI's `cli::rebuild::run`.
            let vault_options = compiler::vault_provider_options(&vault_root, &model_spec)?;
            let options = apply_arg_overrides(
                vault_options,
                args.temperature,
                args.base_url_override,
                args.concurrency,
            );
            let diff_only = !args.force.unwrap_or(false);
            let output = Output::for_transport(transport);
            let mut report =
                compiler::compile(&vault_root, &model_spec, diff_only, &options, Some(&output))
                    .await?;
            let (mechanical_fix, link_fix) = apply_fix_if_requested(
                &vault_root,
                &model_spec,
                &options,
                &output,
                args.fix.unwrap_or(false),
                &mut report,
            )
            .await?;
            compile_report_to_json(&report, mechanical_fix.as_ref(), link_fix.as_ref())
        })
        .await
    }

    #[tool(
        name = "okf-synthesize-next",
        description = "Preferred way to compile/fix the vault when you (the calling MCP client) have your own LLM — no separate provider or API key needed. Returns a batch of synthesis jobs (pending raw sources to compile, then — once none remain — missing wikilink targets to fix) together with the exact compiler instructions and material to work from. Synthesize each job's `{\"operations\": [...]}` payload yourself, following `instructions`, then call okf-synthesize-submit with the results. Call repeatedly until `phase` is \"done\"."
    )]
    async fn synthesize_next(
        &self,
        Parameters(args): Parameters<SynthesizeNextArgs>,
    ) -> Result<CallToolResult, McpError> {
        let jobs_map = self.synthesize_jobs.clone();
        let counter = self.synthesize_job_counter.clone();
        self.run_tool("okf-synthesize-next", async move {
            let vault_root = resolve_vault(args.vault.as_deref())?;
            let batch_size = args.batch_size.clamp(1, 50);
            let diff_only = args.diff.unwrap_or(true);

            let next_job_id = move || {
                let id = counter.fetch_add(1, Ordering::Relaxed);
                format!("job-{id}")
            };

            let batch =
                compiler::next_batch(&vault_root, batch_size, diff_only, next_job_id).await?;

            let jobs_json: Vec<serde_json::Value> = batch
                .jobs
                .iter()
                .map(|(job, _)| {
                    serde_json::json!({
                        "job_id": job.job_id,
                        "kind": job.kind.as_str(),
                        "prompt": job.prompt,
                    })
                })
                .collect();
            let skipped_json: Vec<serde_json::Value> = batch
                .skipped
                .iter()
                .map(|s| serde_json::json!({ "slug": s.slug, "reason": s.reason }))
                .collect();

            {
                let mut jobs_map = jobs_map.lock().await;
                evict_stale_and_excess_synthesize_jobs(&mut jobs_map);
                for (job, stored) in batch.jobs {
                    jobs_map.insert(job.job_id, stored);
                }
            }

            Ok(serde_json::json!({
                "phase": batch.phase,
                "instructions": compiler::prompts::COMPILER_SYSTEM_PROMPT,
                "jobs": jobs_json,
                "skipped": skipped_json,
                "remaining_estimate": batch.remaining_estimate,
            }))
        })
        .await
    }

    #[tool(
        name = "okf-synthesize-submit",
        description = "Submit synthesized results for jobs returned by okf-synthesize-next. Accepts a batch — one entry failing (an expired job_id, an invalid path) does not fail the others. Call okf-synthesize-next again afterward for more work."
    )]
    async fn synthesize_submit(
        &self,
        Parameters(args): Parameters<SynthesizeSubmitArgs>,
    ) -> Result<CallToolResult, McpError> {
        let jobs_map = self.synthesize_jobs.clone();
        self.run_tool("okf-synthesize-submit", async move {
            let mut results_json = Vec::with_capacity(args.results.len());
            // Regenerating per-item would rewrite wiki/index.md up to N
            // times for an N-item batch — once per distinct vault touched,
            // after the loop, is enough (mirrors `compiler::compile`'s own
            // "regenerate once after all sources are applied" ordering).
            let mut vaults_to_reindex: std::collections::HashSet<PathBuf> =
                std::collections::HashSet::new();

            for result in args.results {
                let stored = {
                    let mut jobs_map = jobs_map.lock().await;
                    evict_stale_and_excess_synthesize_jobs(&mut jobs_map);
                    jobs_map.get(&result.job_id).cloned()
                };

                let Some(stored) = stored else {
                    results_json.push(serde_json::json!({
                        "job_id": result.job_id,
                        "status": "error",
                        "error": "job not found or expired",
                    }));
                    continue;
                };

                // Scoped so `?` stays local to this one job — one item's
                // failure must not abort the rest of the batch.
                let outcome: anyhow::Result<Vec<PathBuf>> = (|| {
                    let payload = CompilePayload {
                        operations: result.operations,
                    };
                    let touched = apply_operations(&stored.vault_root, &payload)?;
                    if let StoredJobKind::Compile { uri } = &stored.kind {
                        let mut manifest = manifest::store::load(&stored.vault_root)?;
                        manifest.mark_compiled(uri);
                        manifest::store::save(&stored.vault_root, &manifest)?;
                    }
                    Ok(touched)
                })();

                match outcome {
                    Ok(touched_paths) => {
                        jobs_map.lock().await.remove(&result.job_id);
                        vaults_to_reindex.insert(stored.vault_root.clone());
                        results_json.push(serde_json::json!({
                            "job_id": result.job_id,
                            "status": "applied",
                            "touched_paths": touched_paths
                                .iter()
                                .map(|p| p.to_string_lossy().to_string())
                                .collect::<Vec<_>>(),
                        }));
                    }
                    Err(err) => {
                        // Left in the map (not removed) so the client can
                        // retry with corrected output, bounded by the same
                        // TTL/cap eviction as any other pending job.
                        results_json.push(serde_json::json!({
                            "job_id": result.job_id,
                            "status": "error",
                            "error": err.to_string(),
                        }));
                    }
                }
            }

            // Best-effort: a page landed on disk either way even if this
            // fails, and the next successful submit/compile against this
            // vault regenerates the index anyway — not worth failing an
            // otherwise-successful batch over.
            for vault_root in &vaults_to_reindex {
                let _ = compiler::regenerate_index(vault_root);
            }

            Ok(serde_json::json!({ "results": results_json }))
        })
        .await
    }

    #[tool(
        name = "okf-list-models",
        description = "List a provider's available models (e.g. before picking a model for okf-compile)."
    )]
    async fn list_models(
        &self,
        Parameters(args): Parameters<ModelsArgs>,
    ) -> Result<CallToolResult, McpError> {
        self.run_tool("okf-list-models", async move {
            let options = match resolve_vault(args.vault.as_deref()) {
                Ok(vault_root) => {
                    compiler::vault_provider_options_for_provider(&vault_root, &args.provider)?
                }
                Err(_) => CompileOptions::default(),
            };
            let models = compiler::list_models(
                &args.provider,
                options.base_url_override.as_deref(),
                options.api_key_env_override.as_deref(),
            )
            .await?;
            Ok(serde_json::json!({ "provider": args.provider, "models": models }))
        })
        .await
    }

    #[tool(
        name = "okf-lint",
        description = "Check wikilinks, frontmatter, and source provenance in the wiki."
    )]
    async fn lint(
        &self,
        Parameters(args): Parameters<LintArgs>,
    ) -> Result<CallToolResult, McpError> {
        self.run_tool("okf-lint", async move {
            let vault_root = resolve_vault(args.vault.as_deref())?;
            let fix = args.fix.unwrap_or(false);
            let (fix_report, report) = if fix {
                let (fix_report, after_fix) = validator::fix_bundle(&vault_root)?;
                (Some(fix_report), after_fix)
            } else {
                (None, validator::lint_bundle(&vault_root)?)
            };
            let strict = args.strict.unwrap_or(false);
            let clean = !report.has_errors() && (!strict || report.orphan_pages.is_empty());
            let mut value = serde_json::to_value(&report)?;
            if let Some(object) = value.as_object_mut() {
                object.insert("clean".to_string(), serde_json::json!(clean));
                object.insert(
                    "summary".to_string(),
                    serde_json::json!(lint_report_format::to_text(&report)),
                );
                if let Some(fix_report) = &fix_report {
                    object.insert(
                        "fix".to_string(),
                        serde_json::json!({
                            "fixed_frontmatter_typos": fix_report.fixed_frontmatter_typos,
                            "fixed_sources": fix_report.fixed_sources,
                        }),
                    );
                }
            }
            Ok(value)
        })
        .await
    }

    #[tool(
        name = "okf-reindex",
        description = "Rebuild the local text and vector index used by search."
    )]
    async fn reindex(
        &self,
        Parameters(args): Parameters<ReindexArgs>,
    ) -> Result<CallToolResult, McpError> {
        let transport = self.config.transport;
        self.run_tool("okf-reindex", async move {
            let vault_root = resolve_vault(args.vault.as_deref())?;
            let embeddings = args.embeddings.unwrap_or(false);
            let output = Output::for_transport(transport);
            let report = search::reindex(&vault_root, embeddings, Some(&output))?;
            Ok(serde_json::to_value(report)?)
        })
        .await
    }

    #[tool(
        name = "okf-search",
        description = "Search ingested raw sources and compiled wiki concepts."
    )]
    async fn search(
        &self,
        Parameters(args): Parameters<SearchArgs>,
    ) -> Result<CallToolResult, McpError> {
        self.run_tool("okf-search", async move {
            let vault_root = resolve_vault(args.vault.as_deref())?;
            let results = search::hybrid_search(&vault_root, &args.query, args.limit)?;
            Ok(serde_json::to_value(results)?)
        })
        .await
    }

    #[tool(
        name = "okf-delete",
        description = "Remove a source from the vault (soft by default; purge hard-deletes)."
    )]
    async fn delete(
        &self,
        Parameters(args): Parameters<DeleteArgs>,
    ) -> Result<CallToolResult, McpError> {
        self.run_tool("okf-delete", async move {
            let vault_root = resolve_vault(args.vault.as_deref())?;
            let purge = args.purge.unwrap_or(false);
            let outcome =
                ingest::delete_source(&vault_root, &args.source, purge, "deleted via okf-delete")?;
            Ok(match outcome {
                DeleteOutcome::Tombstoned => serde_json::json!({ "outcome": "tombstoned" }),
                DeleteOutcome::Purged { removed_raw_ids } => serde_json::json!({
                    "outcome": "purged",
                    "removed_raw_ids": removed_raw_ids,
                }),
            })
        })
        .await
    }

    #[tool(
        name = "okf-read-index",
        description = "Read the wiki's table-of-contents page."
    )]
    async fn read_index(
        &self,
        Parameters(args): Parameters<ReadIndexArgs>,
    ) -> Result<CallToolResult, McpError> {
        self.run_tool("okf-read-index", async move {
            let vault_root = resolve_vault(args.vault.as_deref())?;
            let content = fs_ops::read_to_string(&vault_root, "wiki/index.md")
                .map_err(|_| anyhow::anyhow!("no wiki/index.md yet — run okf-compile first"))?;
            Ok(serde_json::json!({ "content": content }))
        })
        .await
    }

    #[tool(
        name = "okf-read-concept",
        description = "Read a single compiled wiki concept page."
    )]
    async fn read_concept(
        &self,
        Parameters(args): Parameters<ReadConceptArgs>,
    ) -> Result<CallToolResult, McpError> {
        self.run_tool("okf-read-concept", async move {
            let vault_root = resolve_vault(args.vault.as_deref())?;
            let path = find_concept_path(&vault_root, &args.id_or_path)?;
            let content = std::fs::read_to_string(&path)?;
            let relative = path
                .strip_prefix(&vault_root)
                .unwrap_or(&path)
                .to_string_lossy()
                .replace('\\', "/");
            Ok(serde_json::json!({ "path": relative, "content": content }))
        })
        .await
    }

    #[tool(
        name = "okf-list-vaults",
        description = "List every vault registered on this machine."
    )]
    async fn list_vaults(
        &self,
        Parameters(_args): Parameters<ListVaultsArgs>,
    ) -> Result<CallToolResult, McpError> {
        self.run_tool("okf-list-vaults", async move {
            let registry = VaultRegistry::load()?;
            let vaults: Vec<serde_json::Value> = registry
                .vaults
                .iter()
                .map(|(name, entry)| {
                    serde_json::json!({
                        "name": name,
                        "path": entry.path,
                        "description": entry.description,
                        "default": registry.default.as_deref() == Some(name.as_str()),
                    })
                })
                .collect();
            Ok(serde_json::json!({ "vaults": vaults }))
        })
        .await
    }
}

/// `--fix`'s MCP-side counterpart to `cli::compile::report_and_commit`'s
/// fix pass — same two tiers (mechanical, then LLM-assisted for any
/// remaining broken links), folded into `report.lint_report` so the
/// returned JSON reflects post-fix state. No git commit here: MCP
/// `okf-compile`/`okf-rebuild` never commit (that's CLI-only), so there's
/// no LLM-content-review gate to apply — the caller sees exactly what was
/// written and decides what to do with it.
async fn apply_fix_if_requested(
    vault_root: &Path,
    model_spec: &str,
    options: &CompileOptions,
    output: &Output,
    fix: bool,
    report: &mut compiler::CompileReport,
) -> anyhow::Result<(
    Option<validator::FixReport>,
    Option<compiler::LinkFixReport>,
)> {
    if !fix {
        return Ok((None, None));
    }

    let (mechanical, after_mechanical) = validator::fix_bundle(vault_root)?;
    report.lint_report = after_mechanical;

    let mut link_fix_report = None;
    if !report.lint_report.broken_links.is_empty() {
        let link_fix =
            compiler::fix_broken_links(vault_root, model_spec, options, Some(output)).await?;
        report.lint_report = validator::lint_bundle(vault_root)?;
        link_fix_report = Some(link_fix);
    }

    Ok((Some(mechanical), link_fix_report))
}

/// Merges an MCP tool call's explicit `temperature`/`base_url_override`/
/// `concurrency` args on top of the vault-config-derived `options` — shared
/// by `compile` and `rebuild`, which otherwise duplicated this exact
/// precedence (vault config first, an explicit `Some` arg wins,
/// `concurrency` defaults to `1` when not given). Pure and synchronous so
/// the precedence itself can be unit-tested without a vault/LLM.
fn apply_arg_overrides(
    mut options: CompileOptions,
    temperature: Option<f32>,
    base_url_override: Option<String>,
    concurrency: Option<usize>,
) -> CompileOptions {
    if temperature.is_some() {
        options.temperature = temperature;
    }
    if base_url_override.is_some() {
        options.base_url_override = base_url_override;
    }
    options.concurrency = concurrency.unwrap_or(1);
    options
}

fn compile_report_to_json(
    report: &compiler::CompileReport,
    mechanical_fix: Option<&validator::FixReport>,
    link_fix: Option<&compiler::LinkFixReport>,
) -> anyhow::Result<serde_json::Value> {
    let mut value = serde_json::json!({
        "sources_processed": report.sources_processed(),
        "sources_failed": report.sources_failed(),
        "sources": report.sources.iter().map(|s| serde_json::json!({
            "uri": s.uri,
            "raw_id": s.raw_id,
            "error": s.error,
        })).collect::<Vec<_>>(),
        "touched_paths": report.touched_paths,
        "lint": {
            "clean": !report.lint_report.has_errors(),
            "broken_links": report.lint_report.broken_links.len(),
            "missing_sources": report.lint_report.missing_sources.len(),
            "orphan_pages": report.lint_report.orphan_pages.len(),
        },
    });

    if let Some(object) = value.as_object_mut() {
        if let Some(mechanical) = mechanical_fix {
            object.insert(
                "fix".to_string(),
                serde_json::json!({
                    "fixed_frontmatter_typos": mechanical.fixed_frontmatter_typos,
                    "fixed_sources": mechanical.fixed_sources,
                }),
            );
        }
        if let Some(link_fix) = link_fix {
            let outcomes: Vec<serde_json::Value> = link_fix
                .outcomes
                .iter()
                .map(|outcome| {
                    let status = match &outcome.status {
                        compiler::LinkFixStatus::Synthesized => "synthesized".to_string(),
                        compiler::LinkFixStatus::SkippedNoSourceContext => {
                            "skipped_no_source_context".to_string()
                        }
                        compiler::LinkFixStatus::Failed(err) => format!("failed: {err}"),
                    };
                    serde_json::json!({ "slug": outcome.slug, "status": status })
                })
                .collect();
            object.insert(
                "link_fix".to_string(),
                serde_json::json!({
                    "synthesized": link_fix.synthesized_slugs(),
                    "outcomes": outcomes,
                }),
            );
        }
    }

    Ok(value)
}

/// HTTP transport only: extracts this call's own per-request credentials
/// for Firecrawl (rmcp injects the originating `http::request::Parts` into
/// `context.extensions` per JSON-RPC message — see
/// `http::server::auth_gate`'s doc comment for why this, not a
/// `tokio::task_local!`, is the mechanism that actually works here).
/// `None` on stdio, where no such extension is ever inserted — stdio calls
/// fall back to local config/keychain inside `AuthManager` itself.
fn extract_http_request_credentials(
    context: &RequestContext<RoleServer>,
    config: &Config,
) -> Option<crate::auth::request_credentials::RequestCredentials> {
    context
        .extensions
        .get::<axum::http::request::Parts>()
        .and_then(|parts| {
            let (header_location, header_name) = header_location_for(config.auth_method);
            extract_request_credentials(&parts.headers, header_location, header_name).ok()
        })
}

impl OkfServer {
    /// Wraps a tool's core logic with consistent MCP response formatting
    /// and error handling, so each tool method only implements its own
    /// business logic, not the MCP content-envelope boilerplate.
    async fn run_tool<F>(&self, tool_name: &str, fut: F) -> Result<CallToolResult, McpError>
    where
        F: std::future::Future<Output = anyhow::Result<serde_json::Value>>,
    {
        match fut.await {
            Ok(value) => {
                let text =
                    serde_json::to_string_pretty(&value).unwrap_or_else(|_| value.to_string());
                Ok(CallToolResult::success(vec![ContentBlock::text(text)]))
            }
            Err(err) => {
                tracing::error!(tool = tool_name, error = %err, "tool execution failed");
                Ok(CallToolResult::error(vec![ContentBlock::text(
                    err.to_string(),
                )]))
            }
        }
    }
}

// `router = self.tool_router.clone()`: without it, `#[tool_handler]`
// defaults to calling `Self::tool_router()` fresh on every `list_tools`/
// `call_tool` request, rebuilding the router instead of reusing the one
// `new()` already built into this instance's `tool_router` field.
#[tool_handler(router = self.tool_router.clone())]
impl ServerHandler for OkfServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::from_build_env())
            .with_protocol_version(ProtocolVersion::V_2024_11_05)
            .with_instructions(
                "A Git-native OKF (Open Knowledge Format) knowledge vault: ingest web pages \
                 (via Firecrawl) or local files, compile them into a linked wiki with an LLM, \
                 lint the result, and search it. Twelve fixed tools: okf-ingest, okf-compile, \
                 okf-rebuild, okf-synthesize-next, okf-synthesize-submit, okf-lint, \
                 okf-reindex, okf-search, okf-delete, okf-read-index, okf-read-concept, \
                 okf-list-vaults. Prefer okf-synthesize-next/okf-synthesize-submit over \
                 okf-compile/okf-rebuild when you (the calling client) have your own LLM — \
                 they use it directly instead of requiring a separately configured provider."
                    .to_string(),
            )
    }
}

/// Runs `server` over the stdio transport until the client disconnects.
pub async fn connect_stdio<S>(server: S) -> anyhow::Result<()>
where
    S: rmcp::ServerHandler,
{
    let running = server.serve(stdio()).await?;
    tracing::info!("MCP server connected over stdio");
    running.waiting().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::config_schema::AuthMethod;
    use rmcp::model::CallToolRequestParams;

    #[derive(Debug, Clone, Default)]
    struct TestClient;

    impl rmcp::ClientHandler for TestClient {}

    fn server() -> OkfServer {
        let config: Config = serde_json::from_value(serde_json::json!({
            "auth_method": "pat"
        }))
        .unwrap();
        OkfServer::new(
            config,
            Arc::new(Mutex::new(AuthManager::new(AuthMethod::Pat))),
        )
    }

    fn tool_json(result: &CallToolResult) -> serde_json::Value {
        let text = &result.content[0].as_text().unwrap().text;
        serde_json::from_str(text).unwrap()
    }

    fn write_raw(vault_root: &Path, raw_id: &str, content: &str) {
        let dir = vault_root.join("raw");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(format!("{raw_id}.md")), content).unwrap();
    }

    #[test]
    fn evict_stale_and_excess_synthesize_jobs_removes_entries_past_the_ttl() {
        let mut jobs = HashMap::new();
        jobs.insert(
            "stale".to_string(),
            StoredJob {
                vault_root: PathBuf::from("/vault"),
                kind: StoredJobKind::Fix {
                    slug: "x".to_string(),
                },
                created_at: Instant::now() - SYNTHESIZE_JOB_TTL - Duration::from_secs(1),
            },
        );
        jobs.insert(
            "fresh".to_string(),
            StoredJob {
                vault_root: PathBuf::from("/vault"),
                kind: StoredJobKind::Fix {
                    slug: "y".to_string(),
                },
                created_at: Instant::now(),
            },
        );

        evict_stale_and_excess_synthesize_jobs(&mut jobs);

        assert!(!jobs.contains_key("stale"));
        assert!(jobs.contains_key("fresh"));
    }

    #[test]
    fn evict_stale_and_excess_synthesize_jobs_caps_total_count_evicting_oldest_first() {
        let mut jobs = HashMap::new();
        let now = Instant::now();
        for i in 0..(SYNTHESIZE_JOB_CAP + 3) {
            jobs.insert(
                format!("job-{i}"),
                StoredJob {
                    vault_root: PathBuf::from("/vault"),
                    kind: StoredJobKind::Fix {
                        slug: "x".to_string(),
                    },
                    // Earlier index = older (created further in the past).
                    created_at: now - Duration::from_secs((SYNTHESIZE_JOB_CAP + 3 - i) as u64),
                },
            );
        }

        evict_stale_and_excess_synthesize_jobs(&mut jobs);

        assert_eq!(jobs.len(), SYNTHESIZE_JOB_CAP);
        // The 3 oldest (lowest index) must be the ones evicted.
        assert!(!jobs.contains_key("job-0"));
        assert!(!jobs.contains_key("job-1"));
        assert!(!jobs.contains_key("job-2"));
        assert!(jobs.contains_key(&format!("job-{}", SYNTHESIZE_JOB_CAP + 2)));
    }

    #[tokio::test]
    async fn synthesize_next_then_submit_compiles_a_pending_source_and_marks_it_compiled() {
        let vault = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(vault.path().join(".okf")).unwrap();
        write_raw(vault.path(), "raw_aaa", "Some content about widgets.");
        let mut manifest = manifest::Manifest::default();
        manifest.record_ingest("uri-a", "sha256:aaa", "raw_aaa", "t0");
        manifest::store::save(vault.path(), &manifest).unwrap();

        let server = server();
        let vault_path = vault.path().to_str().unwrap().to_string();

        let next = server
            .synthesize_next(Parameters(SynthesizeNextArgs {
                batch_size: 10,
                diff: None,
                vault: Some(vault_path.clone()),
            }))
            .await
            .unwrap();
        assert_eq!(next.is_error, Some(false));
        let next_body = tool_json(&next);
        assert_eq!(next_body["phase"], "compile");
        assert!(
            next_body["instructions"]
                .as_str()
                .unwrap()
                .contains("Entities vs. Concepts")
        );
        let jobs = next_body["jobs"].as_array().unwrap();
        assert_eq!(jobs.len(), 1);
        let job_id = jobs[0]["job_id"].as_str().unwrap().to_string();
        assert_eq!(jobs[0]["kind"], "compile");
        assert!(jobs[0]["prompt"].as_str().unwrap().contains("widgets"));

        let submit = server
            .synthesize_submit(Parameters(SynthesizeSubmitArgs {
                results: vec![SynthesizeSubmitResult {
                    job_id,
                    operations: vec![CompileOperation::CreateOrUpdate {
                        path: "wiki/concepts/widgets.md".to_string(),
                        content: "---\ntype: Pattern\ntitle: \"Widgets\"\nsources:\n  - resource: \"/raw/raw_aaa.md\"\n---\n\n# Widgets\n"
                            .to_string(),
                    }],
                }],
            }))
            .await
            .unwrap();
        assert_eq!(submit.is_error, Some(false));
        let submit_body = tool_json(&submit);
        let results = submit_body["results"].as_array().unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0]["status"], "applied");

        assert!(vault.path().join("wiki/concepts/widgets.md").is_file());
        let saved_manifest = manifest::store::load(vault.path()).unwrap();
        assert!(saved_manifest.is_compiled_at_current_hash("uri-a"));

        // A successful submit must regenerate wiki/index.md — the same
        // step `compiler::compile` runs at the end of its own pipeline,
        // which okf-synthesize-submit has to trigger itself since it never
        // goes through `compile()`.
        let index = fs_ops::read_to_string(vault.path(), "wiki/index.md").unwrap();
        assert!(index.contains("okf_version"));
        assert!(index.contains("Widgets"));

        // A second next call has nothing left to compile and no broken
        // links to fix — straight to done.
        let done = server
            .synthesize_next(Parameters(SynthesizeNextArgs {
                batch_size: 10,
                diff: None,
                vault: Some(vault_path),
            }))
            .await
            .unwrap();
        assert_eq!(tool_json(&done)["phase"], "done");
    }

    #[tokio::test]
    async fn synthesize_submit_isolates_an_unknown_job_id_from_the_rest_of_the_batch() {
        let vault = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(vault.path().join(".okf")).unwrap();
        write_raw(vault.path(), "raw_aaa", "content");
        let mut manifest = manifest::Manifest::default();
        manifest.record_ingest("uri-a", "sha256:aaa", "raw_aaa", "t0");
        manifest::store::save(vault.path(), &manifest).unwrap();

        let server = server();
        let next = server
            .synthesize_next(Parameters(SynthesizeNextArgs {
                batch_size: 10,
                diff: None,
                vault: Some(vault.path().to_str().unwrap().to_string()),
            }))
            .await
            .unwrap();
        let job_id = tool_json(&next)["jobs"][0]["job_id"]
            .as_str()
            .unwrap()
            .to_string();

        let submit = server
            .synthesize_submit(Parameters(SynthesizeSubmitArgs {
                results: vec![
                    SynthesizeSubmitResult {
                        job_id: "does-not-exist".to_string(),
                        operations: vec![],
                    },
                    SynthesizeSubmitResult {
                        job_id,
                        operations: vec![CompileOperation::CreateOrUpdate {
                            path: "wiki/concepts/a.md".to_string(),
                            content: "---\ntype: concept\ntitle: \"a\"\n---\n\n# a\n".to_string(),
                        }],
                    },
                ],
            }))
            .await
            .unwrap();
        assert_eq!(submit.is_error, Some(false));
        let results = tool_json(&submit)["results"].clone();
        let results = results.as_array().unwrap();
        assert_eq!(results.len(), 2);
        assert_eq!(results[0]["status"], "error");
        assert!(
            results[0]["error"]
                .as_str()
                .unwrap()
                .contains("not found or expired")
        );
        assert_eq!(results[1]["status"], "applied");
    }

    #[test]
    fn apply_arg_overrides_passes_vault_options_through_unchanged_except_concurrency_defaults_to_one()
     {
        let vault_options = CompileOptions {
            temperature: Some(0.2),
            base_url_override: Some("https://vault.example/v1".to_string()),
            api_key_env_override: Some("VAULT_KEY_ENV".to_string()),
            max_tokens: Some(4096),
            concurrency: 7, // not what vault_provider_options ever sets, but
                            // proves this field is always overwritten below
        };
        let options = apply_arg_overrides(vault_options, None, None, None);
        assert_eq!(options.temperature, Some(0.2));
        assert_eq!(
            options.base_url_override.as_deref(),
            Some("https://vault.example/v1")
        );
        assert_eq!(
            options.api_key_env_override.as_deref(),
            Some("VAULT_KEY_ENV")
        );
        assert_eq!(options.max_tokens, Some(4096));
        assert_eq!(options.concurrency, 1);
    }

    #[test]
    fn apply_arg_overrides_lets_an_explicit_temperature_win_over_the_vault_derived_value() {
        let vault_options = CompileOptions {
            temperature: Some(0.2),
            ..CompileOptions::default()
        };
        let options = apply_arg_overrides(vault_options, Some(0.9), None, None);
        assert_eq!(options.temperature, Some(0.9));
    }

    #[test]
    fn apply_arg_overrides_lets_an_explicit_base_url_override_win() {
        let vault_options = CompileOptions {
            base_url_override: Some("https://vault.example/v1".to_string()),
            ..CompileOptions::default()
        };
        let options = apply_arg_overrides(
            vault_options,
            None,
            Some("https://arg.example/v1".to_string()),
            None,
        );
        assert_eq!(
            options.base_url_override.as_deref(),
            Some("https://arg.example/v1")
        );
    }

    #[test]
    fn apply_arg_overrides_uses_an_explicit_concurrency_value_instead_of_the_default() {
        let options = apply_arg_overrides(CompileOptions::default(), None, None, Some(5));
        assert_eq!(options.concurrency, 5);
    }

    #[test]
    fn server_info_advertises_the_fixed_tool_set_not_a_discovery_layer() {
        let info = server().get_info();
        assert_eq!(info.protocol_version, ProtocolVersion::V_2024_11_05);
        assert!(info.capabilities.tools.is_some());
        let instructions = info.instructions.unwrap();
        assert!(instructions.contains("okf-search"));
        assert!(!instructions.to_lowercase().contains("semantic database"));
    }

    #[tokio::test]
    async fn run_tool_formats_successes_and_failures_consistently() {
        let server = server();
        let success = server
            .run_tool("coverage", async { Ok(serde_json::json!({ "ok": true })) })
            .await
            .unwrap();
        assert_eq!(success.is_error, Some(false));
        let failure = server
            .run_tool("coverage", async { anyhow::bail!("coverage failure") })
            .await
            .unwrap();
        assert_eq!(failure.is_error, Some(true));
    }

    #[tokio::test]
    async fn lint_and_search_and_list_vaults_work_against_an_empty_vault() {
        let vault = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(vault.path().join(".okf")).unwrap();
        let vault_path = vault.path().to_str().unwrap().to_string();

        let server = server();
        let lint = server
            .lint(Parameters(LintArgs {
                strict: None,
                fix: None,
                vault: Some(vault_path.clone()),
            }))
            .await
            .unwrap();
        assert_eq!(lint.is_error, Some(false));

        let search = server
            .search(Parameters(SearchArgs {
                query: "anything".to_string(),
                limit: 5,
                vault: Some(vault_path),
            }))
            .await
            .unwrap();
        assert_eq!(search.is_error, Some(false));

        let vaults = server
            .list_vaults(Parameters(ListVaultsArgs {}))
            .await
            .unwrap();
        assert_eq!(vaults.is_error, Some(false));
    }

    #[tokio::test]
    async fn read_index_errors_clearly_when_nothing_has_been_compiled_yet() {
        let vault = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(vault.path().join(".okf")).unwrap();
        let server = server();
        let result = server
            .read_index(Parameters(ReadIndexArgs {
                vault: Some(vault.path().to_str().unwrap().to_string()),
            }))
            .await
            .unwrap();
        assert_eq!(result.is_error, Some(true));
    }

    #[tokio::test]
    async fn read_concept_resolves_an_entity_page_by_slug_with_no_id_field() {
        // New-shape page: no `id:`/`okf_version:` frontmatter, lives under
        // wiki/entities/ (not wiki/concepts/) — proves find_concept_path's
        // filename-stem match reaches the second content directory.
        let vault = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(vault.path().join(".okf")).unwrap();
        let dir = vault.path().join("wiki/entities");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("ada-lovelace.md"),
            "---\ntype: Person\ntitle: \"Ada Lovelace\"\n---\n\n# Ada Lovelace\n",
        )
        .unwrap();

        let server = server();
        let result = server
            .read_concept(Parameters(ReadConceptArgs {
                id_or_path: "ada-lovelace".to_string(),
                vault: Some(vault.path().to_str().unwrap().to_string()),
            }))
            .await
            .unwrap();
        assert_eq!(result.is_error, Some(false));
    }

    #[tokio::test]
    async fn delete_of_a_never_ingested_source_is_a_tool_error_not_a_panic() {
        let vault = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(vault.path().join(".okf")).unwrap();
        let server = server();
        let result = server
            .delete(Parameters(DeleteArgs {
                source: "https://example.com/never-ingested".to_string(),
                purge: None,
                vault: Some(vault.path().to_str().unwrap().to_string()),
            }))
            .await
            .unwrap();
        assert_eq!(result.is_error, Some(true));
    }

    // Wrapped in an overall timeout: a hang anywhere below (e.g. the
    // in-process server task panicking mid-request, which leaves the
    // client awaiting a response that will never arrive) would otherwise
    // run until CI's own job-level timeout killed it, hours later.
    #[tokio::test]
    async fn mcp_protocol_exposes_the_full_hyphenated_tool_set() {
        tokio::time::timeout(
            std::time::Duration::from_secs(30),
            mcp_protocol_exposes_the_full_hyphenated_tool_set_inner(),
        )
        .await
        .expect(
            "mcp protocol test timed out after 30s — the server task likely panicked mid-request",
        );
    }

    async fn mcp_protocol_exposes_the_full_hyphenated_tool_set_inner() {
        let (server_transport, client_transport) = tokio::io::duplex(64 * 1024);
        let server_task = tokio::spawn(async move {
            server().serve(server_transport).await?.waiting().await?;
            anyhow::Ok(())
        });
        let client = TestClient.serve(client_transport).await.unwrap();

        let mut tools: Vec<String> = client
            .list_all_tools()
            .await
            .unwrap()
            .iter()
            .map(|tool| tool.name.to_string())
            .collect();
        tools.sort();
        assert_eq!(
            tools,
            vec![
                "okf-compile",
                "okf-delete",
                "okf-ingest",
                "okf-lint",
                "okf-list-models",
                "okf-list-vaults",
                "okf-read-concept",
                "okf-read-index",
                "okf-rebuild",
                "okf-reindex",
                "okf-search",
                "okf-synthesize-next",
                "okf-synthesize-submit",
            ]
        );

        let vault = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(vault.path().join(".okf")).unwrap();

        let lint = client
            .call_tool(
                CallToolRequestParams::new("okf-lint").with_arguments(
                    serde_json::json!({ "vault": vault.path().to_str().unwrap() })
                        .as_object()
                        .unwrap()
                        .clone(),
                ),
            )
            .await
            .unwrap();
        assert_eq!(lint.is_error, Some(false));

        drop(client);
        tokio::time::timeout(std::time::Duration::from_secs(2), server_task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    }
}
