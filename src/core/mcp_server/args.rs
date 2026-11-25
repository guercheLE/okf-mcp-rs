//! `#[tool(...)]` parameter structs for every `okf-mcp` MCP tool — split out
//! of `mcp_server.rs` so that file is the tools' dispatch/wiring, not their
//! schemas. Each struct's doc comments double as the schema description
//! `schemars::JsonSchema` exposes to the calling MCP client.

use rmcp::schemars;
use serde::Deserialize;

use crate::compiler::operations::CompileOperation;

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

pub fn default_synthesize_batch_size() -> usize {
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

pub fn default_search_limit() -> usize {
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
pub struct TraceProvenanceArgs {
    /// Same resolution as `okf-read-concept`'s `id_or_path`: a wiki page's
    /// slug, a legacy `id:` frontmatter value, or a vault-relative wiki path
    pub id_or_path: String,
    #[serde(default)]
    pub vault: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ListVaultsArgs {}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ExploreArgs {
    #[serde(default)]
    pub vault: Option<String>,
    /// Also launch the machine's default browser at the explorer URL
    /// (default: true). The URL is returned either way.
    #[serde(default)]
    pub open: Option<bool>,
    /// Port to listen on; omitted/0 picks a free one
    #[serde(default)]
    pub port: Option<u16>,
}
