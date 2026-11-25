//! `okf-lint`/`okf-reindex`/`okf-search`/`okf-delete`/`okf-read-index`/
//! `okf-read-concept`/`okf-trace-provenance` bodies — querying and
//! maintaining an already-compiled vault — split out of the thin `#[tool]`
//! dispatch in `mcp_server.rs`.

use rmcp::model::CallToolResult;
use rmcp::{ErrorData as McpError, handler::server::wrapper::Parameters};

use crate::core::mcp_server::OkfServer;
use crate::core::mcp_server::args::{
    DeleteArgs, LintArgs, ReadConceptArgs, ReadIndexArgs, ReindexArgs, SearchArgs,
    TraceProvenanceArgs,
};
use crate::core::mcp_server::helpers::{find_concept_path, trace_source};
use crate::core::output::Output;
use crate::core::vault_resolver::resolve_vault;
use crate::ingest::{self, DeleteOutcome};
use crate::manifest;
use crate::search;
use crate::storage::fs_ops;
use crate::validator::frontmatter::parse_wiki_page;
use crate::validator::{self, report as lint_report_format};

pub(crate) async fn lint(
    server: &OkfServer,
    Parameters(args): Parameters<LintArgs>,
) -> Result<CallToolResult, McpError> {
    server
        .run_tool("okf-lint", async move {
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
                        }),
                    );
                }
            }
            Ok(value)
        })
        .await
}

pub(crate) async fn reindex(
    server: &OkfServer,
    Parameters(args): Parameters<ReindexArgs>,
) -> Result<CallToolResult, McpError> {
    let transport = server.config.transport;
    server
        .run_tool("okf-reindex", async move {
            let vault_root = resolve_vault(args.vault.as_deref())?;
            let embeddings = args.embeddings.unwrap_or(false);
            let output = Output::for_transport(transport);
            let report = search::reindex(&vault_root, embeddings, Some(&output))?;
            Ok(serde_json::to_value(report)?)
        })
        .await
}

pub(crate) async fn search(
    server: &OkfServer,
    Parameters(args): Parameters<SearchArgs>,
) -> Result<CallToolResult, McpError> {
    server
        .run_tool("okf-search", async move {
            let vault_root = resolve_vault(args.vault.as_deref())?;
            let results = search::hybrid_search(&vault_root, &args.query, args.limit)?;
            Ok(serde_json::to_value(results)?)
        })
        .await
}

pub(crate) async fn delete(
    server: &OkfServer,
    Parameters(args): Parameters<DeleteArgs>,
) -> Result<CallToolResult, McpError> {
    server
        .run_tool("okf-delete", async move {
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

pub(crate) async fn read_index(
    server: &OkfServer,
    Parameters(args): Parameters<ReadIndexArgs>,
) -> Result<CallToolResult, McpError> {
    server
        .run_tool("okf-read-index", async move {
            let vault_root = resolve_vault(args.vault.as_deref())?;
            let content = fs_ops::read_to_string(&vault_root, "wiki/index.md")
                .map_err(|_| anyhow::anyhow!("no wiki/index.md yet — run okf-compile first"))?;
            Ok(serde_json::json!({ "content": content }))
        })
        .await
}

pub(crate) async fn read_concept(
    server: &OkfServer,
    Parameters(args): Parameters<ReadConceptArgs>,
) -> Result<CallToolResult, McpError> {
    server
        .run_tool("okf-read-concept", async move {
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

pub(crate) async fn trace_provenance(
    server: &OkfServer,
    Parameters(args): Parameters<TraceProvenanceArgs>,
) -> Result<CallToolResult, McpError> {
    server
        .run_tool("okf-trace-provenance", async move {
            let vault_root = resolve_vault(args.vault.as_deref())?;
            let path = find_concept_path(&vault_root, &args.id_or_path)?;
            let content = std::fs::read_to_string(&path)?;
            let parsed = parse_wiki_page(&content)?;
            let relative = path
                .strip_prefix(&vault_root)
                .unwrap_or(&path)
                .to_string_lossy()
                .replace('\\', "/");

            let source_manifest = manifest::store::load(&vault_root)?;
            let sources: Vec<serde_json::Value> = parsed
                .frontmatter
                .sources
                .iter()
                .map(|source| trace_source(&vault_root, &source_manifest, source))
                .collect();

            Ok(serde_json::json!({
                "page": {
                    "path": relative,
                    "type": parsed.frontmatter.r#type,
                    "title": parsed.frontmatter.title,
                    "status": parsed.frontmatter.status,
                    "stale_after": parsed.frontmatter.stale_after,
                    "generated": parsed.frontmatter.generated,
                },
                "sources": sources,
            }))
        })
        .await
}
