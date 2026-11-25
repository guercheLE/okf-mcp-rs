//! `okf-compile`/`okf-rebuild`/`okf-synthesize-next`/`okf-synthesize-submit`/
//! `okf-list-models` bodies — the LLM-compile-pipeline tools — split out of
//! the thin `#[tool]` dispatch in `mcp_server.rs`.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::Ordering;

use rmcp::model::CallToolResult;
use rmcp::{ErrorData as McpError, handler::server::wrapper::Parameters};

use crate::compiler::operations::{CompilePayload, apply_operations};
use crate::compiler::{self, CompileOptions, StoredJobKind};
use crate::core::mcp_server::OkfServer;
use crate::core::mcp_server::args::{
    CompileArgs, ModelsArgs, RebuildArgs, SynthesizeNextArgs, SynthesizeSubmitArgs,
};
use crate::core::mcp_server::helpers::{
    apply_arg_overrides, apply_fix_if_requested, compile_report_to_json,
    evict_stale_and_excess_synthesize_jobs,
};
use crate::core::output::Output;
use crate::core::vault_resolver::resolve_vault;
use crate::manifest;

pub(crate) async fn compile(
    server: &OkfServer,
    Parameters(args): Parameters<CompileArgs>,
) -> Result<CallToolResult, McpError> {
    let transport = server.config.transport;
    server
        .run_tool("okf-compile", async move {
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

pub(crate) async fn rebuild(
    server: &OkfServer,
    Parameters(args): Parameters<RebuildArgs>,
) -> Result<CallToolResult, McpError> {
    let transport = server.config.transport;
    server
        .run_tool("okf-rebuild", async move {
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

pub(crate) async fn synthesize_next(
    server: &OkfServer,
    Parameters(args): Parameters<SynthesizeNextArgs>,
) -> Result<CallToolResult, McpError> {
    let jobs_map = server.synthesize_jobs.clone();
    let counter = server.synthesize_job_counter.clone();
    server
        .run_tool("okf-synthesize-next", async move {
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

pub(crate) async fn synthesize_submit(
    server: &OkfServer,
    Parameters(args): Parameters<SynthesizeSubmitArgs>,
) -> Result<CallToolResult, McpError> {
    let jobs_map = server.synthesize_jobs.clone();
    server
        .run_tool("okf-synthesize-submit", async move {
            let mut results_json = Vec::with_capacity(args.results.len());
            // Regenerating per-item would rewrite wiki/index.md up to N
            // times for an N-item batch — once per distinct vault touched,
            // after the loop, is enough (mirrors `compiler::compile`'s own
            // "regenerate once after all sources are applied" ordering).
            let mut vaults_to_reindex: std::collections::HashSet<PathBuf> =
                std::collections::HashSet::new();
            // (applied, errored) counts per vault touched by this batch —
            // for this call's own `wiki/log.md` entry below, which (like
            // the reindex above) is written once per vault rather than once
            // per job.
            let mut counts_by_vault: HashMap<PathBuf, (usize, usize)> = HashMap::new();

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
                        counts_by_vault
                            .entry(stored.vault_root.clone())
                            .or_default()
                            .0 += 1;
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
                        counts_by_vault
                            .entry(stored.vault_root.clone())
                            .or_default()
                            .1 += 1;
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
            // Also best-effort, for the same reason — see `wiki_log::append`'s
            // own doc comment. One line per vault touched by this batch,
            // not per job, mirroring the reindex loop just above.
            for (vault_root, (applied, errored)) in &counts_by_vault {
                let _ = crate::storage::wiki_log::append(
                    vault_root,
                    "okf-synthesize-submit",
                    if *errored == 0 { "ok" } else { "error" },
                    &format!("{applied} job(s) applied, {errored} failed"),
                );
            }

            Ok(serde_json::json!({ "results": results_json }))
        })
        .await
}

pub(crate) async fn list_models(
    server: &OkfServer,
    Parameters(args): Parameters<ModelsArgs>,
) -> Result<CallToolResult, McpError> {
    server
        .run_tool("okf-list-models", async move {
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
