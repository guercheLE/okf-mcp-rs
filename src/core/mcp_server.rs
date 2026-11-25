//! The `okf-mcp` MCP tool surface: a fixed, well-defined set of tools
//! (ingest, compile, rebuild, synthesize-next, synthesize-submit, lint,
//! reindex, search, delete, read-index, read-concept, trace-provenance,
//! list-vaults, explore) over a Git-native OKF knowledge vault.
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
//!
//! Each tool's own parameter schema lives in `args`, shared supporting
//! logic in `helpers`, and each tool's actual body in `tools` (grouped by
//! stage: ingest, compile, search, vault) — this file is just the
//! `#[tool_router]`'s thin dispatch, `OkfServer` itself, and the
//! `ServerHandler` impl.

pub mod args;
pub(crate) mod helpers;
mod tools;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{
    CallToolResult, ContentBlock, Implementation, ProtocolVersion, ServerCapabilities, ServerConfig,
};
use rmcp::service::RequestContext;
use rmcp::transport::stdio;
use rmcp::{
    ErrorData as McpError, RoleServer, ServerHandler, ServiceExt, tool, tool_handler, tool_router,
};
use tokio::sync::Mutex;

use crate::auth::auth_manager::AuthManager;
use crate::compiler::StoredJob;
use crate::core::config_schema::Config;

use args::{
    CompileArgs, DeleteArgs, ExploreArgs, IngestArgs, LintArgs, ListVaultsArgs, ModelsArgs,
    ReadConceptArgs, ReadIndexArgs, RebuildArgs, ReindexArgs, SearchArgs, SynthesizeNextArgs,
    SynthesizeSubmitArgs, TraceProvenanceArgs,
};
// Re-exported (privately) so `tests.rs`'s `use super::*;` keeps reaching
// these, unchanged from when they all lived directly in this file.
#[cfg(test)]
use args::SynthesizeSubmitResult;
#[cfg(test)]
use helpers::{apply_arg_overrides, evict_stale_and_excess_synthesize_jobs};

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
    /// (see `helpers::evict_stale_and_excess_synthesize_jobs`) rather than
    /// on a background timer, so an abandoned session can't leak memory
    /// forever without needing any extra task/thread.
    synthesize_jobs: Arc<Mutex<HashMap<String, StoredJob>>>,
    /// Backs `okf-synthesize-next`'s job IDs. A plain counter, not a random
    /// token: job IDs are pure in-memory correlation handles for this one
    /// session, never a security boundary, so there's no reason to pull in
    /// an RNG dependency for them.
    synthesize_job_counter: Arc<AtomicU64>,
    /// Running `okf-explore` servers, keyed by canonical vault root, so a
    /// second call for the same vault returns the existing URL instead of
    /// starting another server. Session-scoped like everything else here.
    explorers: Arc<Mutex<HashMap<PathBuf, crate::explorer::ExplorerHandle>>>,
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
            explorers: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    #[tool(
        name = "okf-ingest",
        description = "Fetch a URL or read a local file and add it to the vault's raw sources."
    )]
    async fn ingest(
        &self,
        args: Parameters<IngestArgs>,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        tools::ingest::ingest(self, args, context).await
    }

    #[tool(
        name = "okf-compile",
        description = "Compile newly-ingested raw sources into wiki concept/entity pages. Requires a configured LLM provider (see okf-mcp setup). If you (the calling MCP client) have your own LLM and would rather not configure a separate provider, use okf-synthesize-next/okf-synthesize-submit instead."
    )]
    async fn compile(&self, args: Parameters<CompileArgs>) -> Result<CallToolResult, McpError> {
        tools::compile::compile(self, args).await
    }

    #[tool(
        name = "okf-rebuild",
        description = "Recompile the wiki from all active raw sources. Requires a configured LLM provider (see okf-mcp setup). If you (the calling MCP client) have your own LLM and would rather not configure a separate provider, use okf-synthesize-next/okf-synthesize-submit instead."
    )]
    async fn rebuild(&self, args: Parameters<RebuildArgs>) -> Result<CallToolResult, McpError> {
        tools::compile::rebuild(self, args).await
    }

    #[tool(
        name = "okf-synthesize-next",
        description = "Preferred way to compile/fix the vault when you (the calling MCP client) have your own LLM — no separate provider or API key needed. Returns a batch of synthesis jobs (pending raw sources to compile, then — once none remain — missing wikilink targets to fix) together with the exact compiler instructions and material to work from. Synthesize each job's `{\"operations\": [...]}` payload yourself, following `instructions`, then call okf-synthesize-submit with the results. Call repeatedly until `phase` is \"done\"."
    )]
    async fn synthesize_next(
        &self,
        args: Parameters<SynthesizeNextArgs>,
    ) -> Result<CallToolResult, McpError> {
        tools::compile::synthesize_next(self, args).await
    }

    #[tool(
        name = "okf-synthesize-submit",
        description = "Submit synthesized results for jobs returned by okf-synthesize-next. Accepts a batch — one entry failing (an expired job_id, an invalid path) does not fail the others. Call okf-synthesize-next again afterward for more work."
    )]
    async fn synthesize_submit(
        &self,
        args: Parameters<SynthesizeSubmitArgs>,
    ) -> Result<CallToolResult, McpError> {
        tools::compile::synthesize_submit(self, args).await
    }

    #[tool(
        name = "okf-list-models",
        description = "List a provider's available models (e.g. before picking a model for okf-compile)."
    )]
    async fn list_models(&self, args: Parameters<ModelsArgs>) -> Result<CallToolResult, McpError> {
        tools::compile::list_models(self, args).await
    }

    #[tool(
        name = "okf-lint",
        description = "Check wikilinks, frontmatter, and source provenance in the wiki."
    )]
    async fn lint(&self, args: Parameters<LintArgs>) -> Result<CallToolResult, McpError> {
        tools::search::lint(self, args).await
    }

    #[tool(
        name = "okf-reindex",
        description = "Rebuild the local text and vector index used by search."
    )]
    async fn reindex(&self, args: Parameters<ReindexArgs>) -> Result<CallToolResult, McpError> {
        tools::search::reindex(self, args).await
    }

    #[tool(
        name = "okf-search",
        description = "Search ingested raw sources and compiled wiki concepts."
    )]
    async fn search(&self, args: Parameters<SearchArgs>) -> Result<CallToolResult, McpError> {
        tools::search::search(self, args).await
    }

    #[tool(
        name = "okf-delete",
        description = "Remove a source from the vault (soft by default; purge hard-deletes)."
    )]
    async fn delete(&self, args: Parameters<DeleteArgs>) -> Result<CallToolResult, McpError> {
        tools::search::delete(self, args).await
    }

    #[tool(
        name = "okf-read-index",
        description = "Read the wiki's table-of-contents page."
    )]
    async fn read_index(
        &self,
        args: Parameters<ReadIndexArgs>,
    ) -> Result<CallToolResult, McpError> {
        tools::search::read_index(self, args).await
    }

    #[tool(
        name = "okf-read-concept",
        description = "Read a single compiled wiki concept page."
    )]
    async fn read_concept(
        &self,
        args: Parameters<ReadConceptArgs>,
    ) -> Result<CallToolResult, McpError> {
        tools::search::read_concept(self, args).await
    }

    #[tool(
        name = "okf-trace-provenance",
        description = "Trace a compiled wiki page back to its cited raw sources and each source's current manifest status (ACTIVE/SUPERSEDED/TOMBSTONED/UNTRACKED/UNRESOLVABLE) — \"why does the wiki believe this\"."
    )]
    async fn trace_provenance(
        &self,
        args: Parameters<TraceProvenanceArgs>,
    ) -> Result<CallToolResult, McpError> {
        tools::search::trace_provenance(self, args).await
    }

    #[tool(
        name = "okf-explore",
        description = "Start (or reuse) a local browser-based explorer for the vault — 3D link graph, note pane with backlinks, hub filtering, and search over the vault's index — and return its URL."
    )]
    async fn explore(&self, args: Parameters<ExploreArgs>) -> Result<CallToolResult, McpError> {
        tools::vault::explore(self, args).await
    }

    #[tool(
        name = "okf-list-vaults",
        description = "List every vault registered on this machine."
    )]
    async fn list_vaults(
        &self,
        args: Parameters<ListVaultsArgs>,
    ) -> Result<CallToolResult, McpError> {
        tools::vault::list_vaults(self, args).await
    }
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
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::from_build_env())
            .with_protocol_version(ProtocolVersion::V_2024_11_05)
            .with_instructions(
                "A Git-native OKF (Open Knowledge Format) knowledge vault: ingest web pages \
                 (via Firecrawl) or local files, compile them into a linked wiki with an LLM, \
                 lint the result, and search it. Fixed tools: okf-ingest, okf-compile, \
                 okf-rebuild, okf-synthesize-next, okf-synthesize-submit, okf-lint, \
                 okf-reindex, okf-search, okf-delete, okf-read-index, okf-read-concept, \
                 okf-trace-provenance, okf-list-vaults, okf-explore. Prefer \
                 okf-synthesize-next/okf-synthesize-submit over \
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
mod tests;
