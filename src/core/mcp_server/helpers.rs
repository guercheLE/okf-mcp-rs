//! Free functions shared by more than one tool handler (or pure enough to
//! unit-test on their own), split out of `mcp_server.rs` so that file is
//! the tools' dispatch/wiring, not their supporting logic.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use rmcp::RoleServer;
use rmcp::service::RequestContext;

use crate::auth::auth_manager::header_location_for;
use crate::compiler::{self, CompileOptions, StoredJob};
use crate::core::config_schema::Config;
use crate::core::output::Output;
use crate::core::vault_resolver::{sandbox_path, wiki_content_dirs};
use crate::http::auth_extractor::extract_request_credentials;
use crate::ingest::frontmatter::{parse_raw_frontmatter, raw_id_from_resource, resolve_raw_path};
use crate::manifest;
use crate::validator;
use crate::validator::frontmatter::{SourceRef, parse_wiki_page};
use crate::validator::rules::markdown_files_in;

/// A pending job never submitted within this long is dropped — a safety
/// net against an abandoned client session leaking memory forever, not
/// something normal use should ever hit.
pub(super) const SYNTHESIZE_JOB_TTL: Duration = Duration::from_secs(30 * 60);
/// If the map still exceeds this many entries after the TTL sweep, the
/// oldest are evicted down to this count — bounds worst-case memory even
/// under a pathological client that never submits anything.
pub(super) const SYNTHESIZE_JOB_CAP: usize = 500;

/// Checked lazily at the top of both `okf-synthesize-next` and
/// `okf-synthesize-submit` — no background task/timer needed, since a
/// session that never calls either tool again has nothing left to evict
/// anyway.
pub(super) fn evict_stale_and_excess_synthesize_jobs(jobs: &mut HashMap<String, StoredJob>) {
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
pub(super) fn find_concept_path(vault_root: &Path, id_or_path: &str) -> anyhow::Result<PathBuf> {
    if let Ok(candidate) = sandbox_path(vault_root, id_or_path)
        && candidate.is_file()
    {
        return Ok(candidate);
    }

    let mut content_paths = Vec::new();
    for (dir, _kind) in wiki_content_dirs(vault_root) {
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

/// Traces one `sources:` entry back to its raw blob and current manifest
/// status, for `okf-trace-provenance`. Never propagates an error: a
/// malformed `resource` value, a `raw_id` whose file can't be found, or a
/// raw file whose own frontmatter fails to parse all report as
/// `"UNRESOLVABLE"` (with a `reason`) rather than failing the whole call —
/// one bad citation on a page shouldn't hide every other source's
/// provenance. A raw file that resolves fine but was never (or is no
/// longer) tracked in this vault's manifest — e.g. dropped in by hand —
/// reports `"UNTRACKED"` rather than a false `"ACTIVE"`.
pub(super) fn trace_source(
    vault_root: &Path,
    manifest: &manifest::Manifest,
    source: &SourceRef,
) -> serde_json::Value {
    let unresolvable = |raw_id: Option<&str>, reason: &str| {
        serde_json::json!({
            "resource": source.resource,
            "raw_id": raw_id,
            "source_url": null,
            "checksum": null,
            "ingested_at": null,
            "manifest_status": { "status": "UNRESOLVABLE", "reason": reason },
        })
    };

    let Some(raw_id) = raw_id_from_resource(&source.resource) else {
        return unresolvable(None, "could not extract a raw_id from this resource value");
    };

    let raw_path = match resolve_raw_path(vault_root, &raw_id) {
        Ok(path) => path,
        Err(_) => return unresolvable(Some(&raw_id), "no raw file found for this raw_id"),
    };

    let raw_frontmatter = match std::fs::read_to_string(&raw_path)
        .map_err(anyhow::Error::from)
        .and_then(|content| parse_raw_frontmatter(&content))
    {
        Ok(frontmatter) => frontmatter,
        Err(_) => {
            return unresolvable(Some(&raw_id), "raw file's frontmatter could not be parsed");
        }
    };

    let manifest_status = match manifest.find_version(&raw_id) {
        Some((_uri, version)) => serde_json::to_value(&version.status)
            .unwrap_or_else(|_| serde_json::json!({ "status": "UNTRACKED" })),
        None => serde_json::json!({ "status": "UNTRACKED" }),
    };

    serde_json::json!({
        "resource": source.resource,
        "raw_id": raw_id,
        "source_url": raw_frontmatter.source_url,
        "checksum": raw_frontmatter.checksum,
        "ingested_at": raw_frontmatter.ingested_at,
        "manifest_status": manifest_status,
    })
}

/// `--fix`'s MCP-side counterpart to `cli::compile::report_and_commit`'s
/// fix pass — same two tiers (mechanical, then LLM-assisted for any
/// remaining broken links), folded into `report.lint_report` so the
/// returned JSON reflects post-fix state. No git commit here: MCP
/// `okf-compile`/`okf-rebuild` never commit (that's CLI-only), so there's
/// no LLM-content-review gate to apply — the caller sees exactly what was
/// written and decides what to do with it.
pub(super) async fn apply_fix_if_requested(
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
pub(super) fn apply_arg_overrides(
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

pub(super) fn compile_report_to_json(
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
pub(super) fn extract_http_request_credentials(
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
