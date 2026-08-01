//! The `okf-mcp` MCP tool surface: 10 fixed, well-defined tools (ingest,
//! compile, rebuild, lint, reindex, search, delete, read-index,
//! read-concept, list-vaults) over a Git-native OKF knowledge vault.
//!
//! Deliberately not framed as a discovery layer over an unknown API
//! surface (contrast the old mcpify-generated `search`/`get`/`call` trio
//! this replaced, which explicitly advertised itself as "backed by an
//! embedded semantic database" for exactly that reason) — every tool here
//! does one specific, named thing. `okf-search`'s hybrid BM25+vector
//! ranking is an internal implementation detail, not part of how the tool
//! is described to a caller.

use std::path::{Path, PathBuf};
use std::sync::Arc;

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
use crate::compiler::{self, CompileOptions};
use crate::core::config_schema::Config;
use crate::core::output::Output;
use crate::core::vault_registry::VaultRegistry;
use crate::core::vault_resolver::{resolve_vault, sandbox_path};
use crate::http::auth_extractor::extract_request_credentials;
use crate::ingest::{self, DeleteOutcome};
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
    #[serde(default)]
    pub vault: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ModelsArgs {
    /// anthropic, openai, groq, gemini, openrouter, deepseek, xai, together,
    /// ollama-cloud, moonshot, ollama, or custom
    pub provider: String,
    #[serde(default)]
    pub vault: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct LintArgs {
    /// Also fail on warnings (orphan pages), not just broken links/sources
    #[serde(default)]
    pub strict: Option<bool>,
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
    /// A concept's frontmatter `id`, or a vault-relative wiki path
    pub id_or_path: String,
    #[serde(default)]
    pub vault: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ListVaultsArgs {}

fn find_concept_path(vault_root: &Path, id_or_path: &str) -> anyhow::Result<PathBuf> {
    if let Ok(candidate) = sandbox_path(vault_root, id_or_path)
        && candidate.is_file()
    {
        return Ok(candidate);
    }
    for path in markdown_files_in(&vault_root.join("wiki/concepts"))? {
        let content = std::fs::read_to_string(&path)?;
        if let Ok(parsed) = parse_wiki_page(&content)
            && (parsed.frontmatter.id == id_or_path
                || parsed.frontmatter.id == format!("concept_{id_or_path}"))
        {
            return Ok(path);
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
}

#[tool_router]
impl OkfServer {
    pub fn new(config: Config, auth_manager: Arc<Mutex<AuthManager>>) -> Self {
        Self {
            config,
            auth_manager,
            tool_router: Self::tool_router(),
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
        description = "Compile newly-ingested raw sources into wiki concept pages."
    )]
    async fn compile(
        &self,
        Parameters(args): Parameters<CompileArgs>,
    ) -> Result<CallToolResult, McpError> {
        let transport = self.config.transport;
        self.run_tool("okf-compile", async move {
            let vault_root = resolve_vault(args.vault.as_deref())?;
            let model_spec = compiler::resolve_model_spec(&vault_root, args.model.as_deref())?;
            let options = CompileOptions {
                temperature: args.temperature,
                base_url_override: args.base_url_override,
                ..Default::default()
            };
            let diff_only = args.diff.unwrap_or(true);
            let output = Output::for_transport(transport);
            let report =
                compiler::compile(&vault_root, &model_spec, diff_only, &options, Some(&output))
                    .await?;
            compile_report_to_json(&report)
        })
        .await
    }

    #[tool(
        name = "okf-rebuild",
        description = "Recompile the wiki from all active raw sources."
    )]
    async fn rebuild(
        &self,
        Parameters(args): Parameters<RebuildArgs>,
    ) -> Result<CallToolResult, McpError> {
        let transport = self.config.transport;
        self.run_tool("okf-rebuild", async move {
            let vault_root = resolve_vault(args.vault.as_deref())?;
            let model_spec = compiler::resolve_model_spec(&vault_root, args.model.as_deref())?;
            let options = CompileOptions {
                temperature: args.temperature,
                base_url_override: args.base_url_override,
                ..Default::default()
            };
            let diff_only = !args.force.unwrap_or(false);
            let output = Output::for_transport(transport);
            let report =
                compiler::compile(&vault_root, &model_spec, diff_only, &options, Some(&output))
                    .await?;
            compile_report_to_json(&report)
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
            let report = validator::lint_bundle(&vault_root)?;
            let strict = args.strict.unwrap_or(false);
            let clean = !report.has_errors() && (!strict || report.orphan_pages.is_empty());
            let mut value = serde_json::to_value(&report)?;
            if let Some(object) = value.as_object_mut() {
                object.insert("clean".to_string(), serde_json::json!(clean));
                object.insert(
                    "summary".to_string(),
                    serde_json::json!(lint_report_format::to_text(&report)),
                );
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

fn compile_report_to_json(report: &compiler::CompileReport) -> anyhow::Result<serde_json::Value> {
    Ok(serde_json::json!({
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
    }))
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
                 lint the result, and search it. Ten fixed tools: okf-ingest, okf-compile, \
                 okf-rebuild, okf-lint, okf-reindex, okf-search, okf-delete, okf-read-index, \
                 okf-read-concept, okf-list-vaults."
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
