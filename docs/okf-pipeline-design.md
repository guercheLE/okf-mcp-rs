# Rust OKF Pipeline for AI Agents — design conversation

> Source: Gemini conversation, shared at https://share.gemini.google/sVticNi0koy9 (redirects to
> https://gemini.google.com/share/4c501559807e). Created with 3.6 Flash on 2026-07-30. Saved
> verbatim (Q&A content only; page chrome/navigation omitted) as the design reference for the
> `okf-mcp` implementation. See `docs/okf-mcp-implementation-plan.md` for the implementation plan
> derived from this conversation.

---

## Q1. If you were to "Build a Complete Pipeline OKF pipeline" in 2026 [mcp server + cli in rust], what it would be?

`([Web/URL || file] ──> markdown ──> [./raw] ──LLM Compiler──> [./wiki] ──Validate──> [Validated OKF Bundle])`

Building an Open Knowledge Format (OKF) pipeline in Rust provides an offline-first, Git-native memory engine for AI agents.

Combining Andrej Karpathy's LLM Wiki pattern with Google's OKF standard, this architecture uses `./raw` as an immutable source of truth and `./wiki` as a compiled, strongly typed knowledge graph with automated linting.

### System Architecture Overview

```
[ Web / Local File ]
          │
          ▼
   ┌──────────────┐
   │ Ingest Stage │  (Rust HTML/Doc-to-Markdown, Hash & Metadata tagger)
   └──────┬───────┘
          ▼
      [ ./raw ]      (Immutable markdown source-of-truth)
          │
          ▼
   ┌──────────────┐
   │ LLM Compiler │  (Extracts concepts, builds [[links]], surfaces diffs)
   └──────┬───────┘
          ▼
      [ ./wiki ]     (Atomic OKF entity & concept pages + index.md)
          │
          ▼
   ┌──────────────┐
   │  Validator   │  (Rust Link-checker, Provenance Auditor, Schema Linter)
   └──────┬───────┘
          ▼
[ Validated OKF Bundle ] (Immutable Git commit + manifest bundle)
```

### Complete Pipeline Breakdown

**1. Ingestion Layer (Web/File ──> markdown ──> ./raw)**

Role: Accepts URLs or raw documents (`.pdf`, `.docx`, `.md`, `.txt`), converts them into normalized Markdown, and appends them to `./raw`.

Rust Implementation:
- `reqwest` + `readability`/`htmd` for web extraction.
- `pdf-extract` for document processing.
- Writes to `./raw/<slug_or_hash>.md` with an immutable frontmatter header:

```yaml
---
okf_version: "1.0"
type: raw_source
id: raw_2026_07_30_a1f9
source_url: "https://example.com/docs/arch"
checksum: "sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
ingested_at: "2026-07-30T18:50:00Z"
---
```

Rule: Files in `./raw/` are append-only and never modified by the LLM or pipeline after writing. *(Revisited in Q3 — see the CAS/manifest design below.)*

**2. LLM Compiler Layer (./raw ──> ./wiki)**

Role: Compiles raw documents into atomic, cross-linked concept pages inside `./wiki/`.

Compilation Rules:
- Source of Truth: The compiler reads from `./raw/` to generate concept pages. It does not compile from existing wiki text, to prevent knowledge drift.
- Wikilinking: Every key entity uses typed `[[concept-name]]` wikilinks.
- Strict Provenance: Every output page must declare its source in frontmatter.

Generated `./wiki/concepts/microservices.md` example:

```yaml
---
okf_version: "1.0"
type: concept
id: concept_microservices
title: Microservices Architecture
sources:
  - "raw/raw_2026_07_30_a1f9.md"
last_updated: "2026-07-30T18:52:00Z"
confidence: 0.95
---

# Microservices Architecture

Microservices partition applications into small, independently deployable services organized around business capabilities.

## Key Relationships
* Interacts with [[api-gateway]] for routing.
* Utilizes [[distributed-tracing]] for observability.
```

**3. Validation & Linting Layer (./wiki ──> Validated OKF Bundle)**

Written purely in Rust for rapid execution before emitting a release or git commit.

| Check Type | Validation Rule | Action on Failure |
|---|---|---|
| Dangling Links | Verify every `[[link]]` points to an existing `./wiki/` target. | Compiler Error |
| Provenance Audit | Verify all items in `sources:` exist inside `./raw/`. | Compiler Error |
| Orphan Detection | Ensure non-index pages are linked by at least one other page. | Warning / Error |
| Contradiction Flag | Flag conflicting statements between newly ingested source and existing pages. | Triggers Human Diff Approval |

Once validated, `okf` creates a deterministic `okf.json` bundle manifest and executes an automated Git commit.

### Rust CLI & MCP Server Design

The system is packaged as a single binary crate providing both a command-line interface (`okf`) and a Model Context Protocol server (`okf-mcp`).

Cargo Project Layout:

```
okf/
├── Cargo.toml
├── src/
│   ├── main.rs            # Entry point (CLI + MCP dispatch)
│   ├── ingest/            # Web scraper, file parser, raw frontmatter generator
│   ├── compiler/          # LLM orchestration, prompt templates, diff resolver
│   ├── validator/         # AST parser for [[wikilinks]], YAML schema validation
│   ├── mcp/               # FastMCP JSON-RPC server handler
│   └── storage/           # Git wrapper, file system ops, index generator
```

CLI Subcommands (`okf`):

```bash
# Ingest web page or local file into ./raw
okf ingest https://example.com/spec.pdf --tag "architecture"

# Compile raw sources into wiki concept pages
okf compile --model anthropic/claude-3-5-sonnet

# Validate broken links, orphan pages, schema consistency
okf lint

# Full end-to-end pipeline run (Ingest -> Compile -> Validate -> Commit)
okf run --url https://example.com/spec.pdf
```

MCP Server (`okf-mcp`) Tools — exposed to AI coding agents (Claude Code, Cursor, Pi, Windsurf) over stdio:
- `okf_read_index`: Retrieves root knowledge index (`./wiki/index.md`) to follow query-first discipline.
- `okf_read_concept`: Reads a targeted OKF wiki page.
- `okf_ingest_source`: Submits a new raw document or URL into the raw queue.
- `okf_validate`: Runs the Rust linter and returns structural errors.
- `okf_search`: Fast local BM25/hybrid search over raw and wiki documents.

### Why This Approach Fits 2026 Workflows

- **Deterministic & Auditable**: Eliminates vector database hallucinations by enforcing strict Markdown link topologies and explicit file sources.
- **Git Native**: Everything sits inside your repository as human-readable Markdown. Every compilation step produces auditable Git commits.
- **Rust Speed**: Parsing frontmatter, verifying thousands of `[[wikilinks]]`, and calculating source diffs runs in under a millisecond.

---

## Q2. It should allow for ingestion, lint/reindex/rebuild, search...

Expanding on the lifecycle, here is the architecture and CLI/MCP implementation for Ingestion, Linting / Reindexing / Rebuilding, and Hybrid Search.

### Expanded Lifecycle & Subcommand Flow

```
                     ┌────────────────────────┐
                     │ okf ingest <URL|File>  │
                     └───────────┬────────────┘
                                 │
                                 ▼
                          [ ./raw/*.md ]
                                 │
                     ┌───────────┴────────────┐
                     │ okf compile / rebuild  │
                     └───────────┬────────────┘
                                 │
                                 ▼
                          [ ./wiki/*.md ]
                                 │
         ┌───────────────────────┼───────────────────────┐
         ▼                       ▼                       ▼
┌─────────────────┐     ┌─────────────────┐     ┌─────────────────┐
│    okf lint     │     │   okf reindex   │     │   okf search    │
│ (OKF Spec v0.2) │     │ (Vector / BM25) │     │ (Hybrid Engine) │
└─────────────────┘     └─────────────────┘     └─────────────────┘
```

**1. Ingestion Layer**

Handles raw extraction from documents/URLs, calculates cryptographic hashes, and appends the source to `./raw` with strict metadata.

```rust
// src/ingest/mod.rs
use sha2::{Sha256, Digest};
use serde::{Serialize, Deserialize};

#[derive(Serialize, Deserialize)]
pub struct RawFrontmatter {
    pub okf_version: String,
    pub r#type: String, // "raw_source"
    pub id: String,
    pub source_url: Option<String>,
    pub checksum: String,
    pub ingested_at: String,
}

pub async fn process_ingest(source: &str) -> anyhow::Result<String> {
    let content = match source {
        s if s.starts_with("http") => fetch_and_clean_url(s).await?,
        file_path => parse_local_doc(file_path)?,
    };

    let hash = format!("{:x}", Sha256::digest(content.as_bytes()));
    let raw_id = format!("raw_{}", &hash[..10]);

    let frontmatter = RawFrontmatter {
        okf_version: "0.2".into(),
        r#type: "raw_source".into(),
        id: raw_id.clone(),
        source_url: if source.starts_with("http") { Some(source.to_string()) } else { None },
        checksum: format!("sha256:{}", hash),
        ingested_at: chrono::Utc::now().to_rfc3339(),
    };

    let target_path = format!("./raw/{}.md", raw_id);
    let full_md = format!("---\n{}\n---\n\n{}", serde_yaml::to_string(&frontmatter)?, content);

    tokio::fs::write(&target_path, full_md).await?;
    Ok(target_path)
}
```

**2. Linting, Reindexing & Rebuilding**

*A. Linting (`okf lint`)* — applies static analysis rules to enforce Google OKF v0.2 bundle conformance.

```rust
// src/validator/mod.rs
pub struct LintReport {
    pub broken_links: Vec<(String, String)>, // (Source file, missing target)
    pub orphan_pages: Vec<String>,
    pub missing_sources: Vec<(String, String)>, // (Wiki page, missing raw source)
}

pub fn lint_bundle() -> anyhow::Result<LintReport> {
    // 1. AST parse all markdown files in ./wiki
    // 2. Extract [[wikilinks]] and verify targets exist on disk
    // 3. Verify YAML frontmatter fields (type, title, sources)
    // 4. Assert all items in 'sources:' point to valid ./raw files
    todo!()
}
```

*B. Rebuilding (`okf rebuild`)* — re-evaluates `./raw/` files against LLM prompts to update concept pages in `./wiki/`, updating `./wiki/index.md` and `./wiki/log.md`.

```bash
# Force a clean rebuild of the entire graph from raw sources
okf rebuild --force --model anthropic/claude-3-5-sonnet
```

*C. Reindexing (`okf reindex`)* — generates or updates local search artifacts (`.okf/index.db`) using Tantivy (for BM25 text search) and FastEmbed-rs (for local dense vector embeddings).

**3. Search Architecture (`okf search`)**

Uses a hybrid search paradigm to serve both human developers via CLI and AI agents via MCP:
- **BM25 Keyword Search (Tantivy)**: Catches exact concept terms, YAML tags, and specific `[[links]]`.
- **Dense Vector Search (Local ONNX embeddings)**: Resolves semantic queries (e.g., searching "How do we handle API rate limits?" hits `[[resiliency-patterns]]`).
- **RRF (Reciprocal Rank Fusion)**: Merges sparse and dense search results into a unified ranking list.

```rust
// src/search/hybrid.rs
pub struct SearchResult {
    pub path: String,
    pub title: String,
    pub snippet: String,
    pub score: f32,
}

pub fn hybrid_search(query: &str, limit: usize) -> anyhow::Result<Vec<SearchResult>> {
    let bm25_results = search_tantivy(query, limit * 2)?;
    let vector_results = search_embeddings(query, limit * 2)?;

    // Reciprocal Rank Fusion (RRF) algorithm
    let merged = rrf_merge(bm25_results, vector_results, k=60);
    Ok(merged.into_iter().take(limit).collect())
}
```

### Complete CLI & MCP Matrix

**Command Line Interface**

| Command | Args | Description |
|---|---|---|
| `okf ingest` | `<URL\|File> --tag <tag>` | Ingest source into `./raw` with hash headers |
| `okf compile` | `--diff` | Compile unprocessed `./raw` sources into `./wiki` concepts |
| `okf rebuild` | `--force` | Perform a full rebuild of the wiki graph from source |
| `okf lint` | `--strict` | Run OKF v0.2 spec conformance checks |
| `okf reindex` | `--embeddings` | Rebuild local BM25 + ONNX vector index |
| `okf search` | `"<query>" --json` | Hybrid search across `./wiki` and `./raw` |

**MCP Server Protocol Matrix (`okf-mcp`)** — exposed natively over stdio for agent tool-calling:

```json
{
  "tools": [
    {
      "name": "okf_ingest",
      "description": "Fetch URL or parse local file and save to raw source-of-truth.",
      "parameters": { "type": "object", "properties": { "source": { "type": "string" } } }
    },
    {
      "name": "okf_lint",
      "description": "Validate the integrity of the OKF wiki graph (dangling links, frontmatter schema errors).",
      "parameters": { "type": "object", "properties": {} }
    },
    {
      "name": "okf_reindex",
      "description": "Rebuild the local search indices after structural wiki updates.",
      "parameters": { "type": "object", "properties": { "force": { "type": "boolean" } } }
    },
    {
      "name": "okf_search",
      "description": "Perform hybrid (sparse BM25 + dense semantic) search over OKF concepts.",
      "parameters": { "type": "object", "properties": { "query": { "type": "string" }, "limit": { "type": "integer" } } }
    }
  ]
}
```

> **Implementation note (this repo):** the tool/command descriptions above ("Perform hybrid... search", "semantic database", etc.) are the *design source's* framing. The actual `okf-mcp` implementation deliberately does **not** market its tools around "semantic search/discovery" — see `docs/okf-mcp-implementation-plan.md`'s constraint #4. The tools are described plainly by what they do; hybrid BM25+vector search is an internal implementation detail of `okf-search`, not part of its public description.

---

## Q3. "Rule: Files in ./raw/ are append-only and never modified by the LLM or pipeline after writing.": What happens with one want ingest updated web / local file or delete its content from ./raw/? There should be a mapping web / local file -> hash [to allow to replace or delete files with hashs in name in ./raw/, maybe in a . (dot) [ignored] folder?

To enforce the append-only rule without breaking document updates or deletions, you need a Content-Addressable Storage (CAS) model paired with an Inverted Manifest Index (stored in `.okf/manifest.json`).

Instead of mutating or deleting raw source files, you record state transitions (e.g., `SUPERSEDED_BY` or `TOMBSTONED`) in `.okf/manifest.json`. This gives you auditability while allowing `./raw` and `./wiki` to stay in sync.

### The Architecture: Hash-to-URI Tracking

```
.
├── .okf/                       <-- Hidden Pipeline System Folder
│   ├── manifest.json           <-- URI <-> Hash state tracking & tombstones
│   └── index.db                <-- Tantivy / FastEmbed search index
├── raw/                        <-- Immutable Raw Blobs
│   ├── raw_a1f94d.md           <-- Version 1 (Preserved)
│   ├── raw_b882e3.md           <-- Version 2 (Newer content for same URL)
│   └── raw_c30111.md           <-- Tombstoned (Marked inactive in manifest)
└── wiki/                       <-- Compiled OKF Entities
    ├── index.md
    └── concepts/
```

### Manifest Design (.okf/manifest.json)

The manifest acts as the source map between external targets (URLs or local file paths), content hashes, and state tracking.

```json
{
  "sources": {
    "https://example.com/api-spec.pdf": {
      "active_hash": "sha256:b882e3...",
      "history": [
        {
          "raw_id": "raw_a1f94d",
          "hash": "sha256:a1f94d...",
          "ingested_at": "2026-06-15T10:00:00Z",
          "status": "SUPERSEDED_BY_b882e3"
        },
        {
          "raw_id": "raw_b882e3",
          "hash": "sha256:b882e3...",
          "ingested_at": "2026-07-30T18:50:00Z",
          "status": "ACTIVE"
        }
      ]
    },
    "file:///docs/legacy-auth.md": {
      "active_hash": null,
      "history": [
        {
          "raw_id": "raw_c30111",
          "hash": "sha256:c30111...",
          "ingested_at": "2026-05-01T12:00:00Z",
          "status": "TOMBSTONED",
          "tombstoned_at": "2026-07-30T19:00:00Z",
          "reason": "Deprecation requested by user via okf delete"
        }
      ]
    }
  }
}
```

### Lifecycle Operations

**1. Update Scenario (`okf ingest <URL>`)** — when you re-ingest an existing URL or local file path:
1. Hash Check: Compute the SHA256 of the fetched content.
   - If `hash == manifest[URI].active_hash` → No-Op (Skip writing, no LLM work needed).
   - If `hash != manifest[URI].active_hash` → New Snapshot Flow.
2. Append-Only Write: Write the new blob to `./raw/raw_<new_hash_prefix>.md`.
3. Manifest State Update: set old version status to `"SUPERSEDED_BY_<new_raw_id>"`; set new version status to `"ACTIVE"`.
4. Compilation Phase (`okf compile`): The LLM compiler reads only `ACTIVE` source blobs from `.okf/manifest.json`. It diffs `raw_<old>` against `raw_<new>` and applies updates to impacted `./wiki/` pages.

**2. Deletion Scenario (`okf delete <URL|File>`)** — to remove a document without violating append-only guarantees:

```bash
# Soft delete (Default: Marks tombstone in manifest)
okf delete https://example.com/api-spec.pdf

# Hard delete (Prunes old raw file from disk and Git history)
okf delete https://example.com/api-spec.pdf --purge
```

*Soft Delete*: the raw blob (`raw_c30111.md`) remains in `./raw`, but `.okf/manifest.json` sets its status to `"TOMBSTONED"`. The search engine (`okf reindex`) removes the document from `.okf/index.db`. Running `okf compile` triggers a wiki clean-up pass:
- Orphan Cleanup: If a concept page in `./wiki/` lost all its `ACTIVE` sources, it is removed or moved to `./wiki/deprecated/`.
- Reference Pruning: Wikilinks to pruned concepts are updated or flagged by `okf lint`.

*Hard Prune (`--purge`)*: if regulatory compliance (GDPR/secrets) requires physical deletion: delete `./raw/raw_c30111.md` from disk, remove entry from `.okf/manifest.json`, run `okf compile --force` to regenerate concept pages without that raw input.

### Rust Implementation Detail (src/ingest/manifest.rs)

```rust
use std::collections::HashMap;
use serde::{Serialize, Deserialize};

#[derive(Serialize, Deserialize, PartialEq, Debug)]
pub enum SourceStatus {
    Active,
    Superseded { by_raw_id: String },
    Tombstoned { reason: String, at: String },
}

#[derive(Serialize, Deserialize)]
pub struct SourceVersion {
    pub raw_id: String,
    pub hash: String,
    pub ingested_at: String,
    pub status: SourceStatus,
}

#[derive(Serialize, Deserialize, Default)]
pub struct Manifest {
    pub sources: HashMap<String, Vec<SourceVersion>>, // Key: URI or local path
}

impl Manifest {
    pub fn get_active_raw_id(&self, uri: &str) -> Option<&str> {
        self.sources.get(uri)?.iter().find_map(|v| match &v.status {
            SourceStatus::Active => Some(v.raw_id.as_str()),
            _ => None,
        })
    }
}
```

---

## Q4. In "okf compile --model anthropic/claude-3-5-sonnet", how would I inform/setup which provider to use (openai, anthropic, groq, ollama)?

In Rust-based AI tools, handling model routing (like `okf compile --model anthropic/claude-3-5-sonnet`) is typically implemented using Provider Prefixes paired with a Hierarchical Configuration Hierarchy (CLI flags > Env Vars > Config File).

The Rust crate `genai` is commonly used in this architecture because it natively supports 25+ providers (OpenAI, Anthropic, Groq, Ollama, Gemini, DeepSeek, local endpoints) behind a single unified Rust async interface.

### 1. Provider Resolution & Prefixing Scheme

The `--model` flag uses a `<provider>/<model_name>` syntax. The pipeline splits this string in Rust to dynamically pick the execution driver and endpoint.

```
okf compile --model anthropic/claude-3-5-sonnet
okf compile --model openai/gpt-4o
okf compile --model groq/llama-3.3-70b-versatile
okf compile --model ollama/llama3.2
okf compile --model custom/my-vllm-endpoint
```

Fallback Rules:
- Explicit Prefix: If the model string contains a `/` (e.g., `groq/llama-3.3-70b`), okf routes explicitly to that provider's driver.
- Implicit Prefix: If no prefix is supplied (e.g., `--model gpt-4o`), okf checks: (a) config file default (`.okf/config.toml` or `~/.config/okf/config.toml`), then (b) active environment variable (e.g., if `ANTHROPIC_API_KEY` is present, assume Anthropic).

### 2. API Key & Endpoint Configuration

`okf` looks up environment variables or runtime configuration depending on the selected provider.

| Provider Prefix | Environment Variable(s) Checked | Base URL (Default) |
|---|---|---|
| `anthropic/` | `ANTHROPIC_API_KEY` | https://api.anthropic.com |
| `openai/` | `OPENAI_API_KEY`, `OPENAI_ORG_ID` | https://api.openai.com/v1 |
| `groq/` | `GROQ_API_KEY` | https://api.groq.com/openai/v1 |
| `ollama/` | `OLLAMA_HOST` (Optional) | http://localhost:11434 (No API key required) |
| `custom/` | `CUSTOM_LLM_API_KEY`, `CUSTOM_LLM_BASE_URL` | Configurable OpenAI-compatible endpoint |

### 3. Persistent Configuration (.okf/config.toml)

Users can define defaults locally or globally in `.okf/config.toml` so they don't have to pass CLI parameters repeatedly:

```toml
# .okf/config.toml
[compiler]
# Default provider/model when --model is omitted
default_model = "anthropic/claude-3-5-sonnet"
temperature = 0.2
max_tokens = 4096

[providers.anthropic]
# api_key can be hardcoded, but referencing ENV is safer
api_key_env = "ANTHROPIC_API_KEY"

[providers.ollama]
base_url = "http://localhost:11434"

[providers.groq]
api_key_env = "GROQ_API_KEY"

[providers.custom_vllm]
base_url = "http://192.168.1.50:8000/v1"
api_key_env = "LOCAL_VLLM_KEY"
```

### 4. Rust Backend Driver (src/compiler/provider.rs)

Using Rust's enum-dispatch pattern with `genai` or custom HTTP adapters:

```rust
use genai::client::Client;
use genai::chat::{ChatRequest, ChatMessage};
use anyhow::{Result, bail};

pub struct LLMCompilerDriver {
    client: Client,
}

impl LLMCompilerDriver {
    pub fn new() -> Self {
        Self {
            // genai automatically resolves system env vars for major providers
            client: Client::default(),
        }
    }

    pub async fn execute_compile_prompt(
        &self,
        full_model_spec: &str, // e.g., "anthropic/claude-3-5-sonnet" or "ollama/llama3.2"
        prompt: &str
    ) -> Result<String> {
        let (provider_prefix, model_name) = parse_model_spec(full_model_spec)?;

        let chat_req = ChatRequest::new(vec![
            ChatMessage::system("You are an OKF Knowledge Graph Compiler."),
            ChatMessage::user(prompt),
        ]);

        // Route execution dynamically based on standard genai model naming or custom client
        let response = self.client
            .exec_chat(model_name, chat_req, None)
            .await?;

        Ok(response.content_text_into().unwrap_or_default())
    }
}

fn parse_model_spec(spec: &str) -> Result<(&str, &str)> {
    if let Some((provider, model)) = spec.split_once('/') {
        Ok((provider, model))
    } else {
        bail!("Invalid model format. Must be '<provider>/<model_name>', got: '{}'", spec)
    }
}
```

### 5. Overrides via MCP (okf-mcp)

When an AI agent calls the `okf_compile` tool over MCP, it can override provider options dynamically per execution:

```json
{
  "jsonrpc": "2.0",
  "method": "tools/call",
  "params": {
    "name": "okf_compile",
    "arguments": {
      "model": "groq/llama-3.3-70b-versatile",
      "options": {
        "temperature": 0.1,
        "base_url_override": "http://localhost:11434"
      }
    }
  },
  "id": 1
}
```

> **Implementation note (this repo):** the "implicit prefix" auto-detection-from-env fallback above is explicitly **dropped** in the actual `okf-mcp` implementation — `--model`/MCP `model` always requires an explicit `<provider>/<model>` prefix, or falls back to `.okf/config.toml`'s `default_model`. See `docs/okf-mcp-implementation-plan.md`.

---

## Q5. Sure, I want to see [the compiler system prompt]

Here is the complete LLM Compiler System Prompt and Compilation Protocol designed for `okf compile`. It adheres to the OKF v0.2 Specification and implements Andrej Karpathy's LLM Wiki synthesis pattern. It handles incremental compilation, entity extraction, diff synthesis from superseded sources, and link maintenance.

### Compiler System Prompt Template (src/compiler/prompts.rs)

```
You are the **OKF LLM Compiler Engine**, an expert knowledge-graph synthesis process written to maintain a local Open Knowledge Format (OKF v0.2) wiki repository.

### Core Mission
Your job is to read raw, unstructured, or updated source documents from `./raw/` and synthesize them into clean, atomic, cross-linked Markdown concept documents inside `./wiki/`.

### Compilation Rules
1. **Source of Truth**: Read content exclusively from active files listed in `.okf/manifest.json`.
2. **Atomicity**: One concept per file. If a document introduces 3 major concepts (e.g., "OAuth2", "JWT", "PKCE"), compile 3 distinct markdown files inside `./wiki/concepts/`.
3. **Wikilinking Topology**: Use `[[concept-slug]]` notation for all entities, technologies, or terms. All links must match target file slugs inside `./wiki/`.
4. **Strict Provenance**: Every output file MUST declare its active raw sources in YAML frontmatter under the `sources:` array.
5. **Conflict Resolution**: If a new source directly contradicts an existing wiki concept, update the page with the newest active state and record the conflict under a `## Contradictions & Evolutions` section.

---

### Output Format Requirement
You MUST respond with a single, valid JSON payload containing an array of file operations. DO NOT wrap the output in conversational text.

#### JSON Output Schema
```json
{
  "operations": [
    {
      "action": "CREATE_OR_UPDATE",
      "path": "wiki/concepts/distributed-tracing.md",
      "content": "--- STRING_ESCAPED_FULL_MARKDOWN_FILE_CONTENT ---"
    },
    {
      "action": "DELETE",
      "path": "wiki/concepts/obsolete-concept.md",
      "reason": "Source raw_a1f94d was tombstoned and concept has no active sources."
    }
  ]
}
```
```

### OKF v0.2 Concept Document Template

When creating or updating concept pages, use the following Markdown structure:

```markdown
---
okf_version: "0.2"
type: concept
id: concept_<slug>
title: "<Human Readable Title>"
description: "<One and for indexes preview search sentence summary used>"
sources:
  - resource: "/raw/<raw_id>.md"
    id: "<raw_id>"
    title: "<Original Source Title>"
tags: [<tag1>, <tag2>]
timestamp: "<ISO8601_TIMESTAMP>"
---

# <Human Readable Title>

<Executive & Core Definition Summary>

## Key Concepts & Architecture
* Relates to [[other-concept-1]] for authentication.
* Leverages [[other-concept-2]] for storage.

## Technical Details
<Structured Markdown blocks, code content: definitions mathematical or tables,>

## Contradictions & Historical Diffs
* **<ISO8601_DATE>**: Superseded [[previous-implementation]] based on source `/raw/<new_raw_id>.md`.
```

### User Prompt Construction (Diff Synthesis Flow)

When `okf compile` runs, the Rust compiler engine builds a context payload comparing the **Active Raw Source** against any **Existing Wiki State** and **Superseded Blobs**:

```rust
// src/compiler/prompt_builder.rs

pub fn build_compile_user_prompt(
    active_raw: &RawBlob,
    superseded_raw: Option<&RawBlob>,
    existing_related_wiki_pages: &[WikiPage],
) -> String {
    let mut prompt = String::new();

    prompt.push_str("## 1. ACTIVE RAW SOURCE TO PROCESS\n");
    prompt.push_str(&format!("Raw ID: {}\n", active_raw.id));
    prompt.push_str(&format!("Content:\n```markdown\n{}\n```\n\n", active_raw.content));

    if let Some(old) = superseded_raw {
        prompt.push_str("## 2. SUPERSEDED PREVIOUS SOURCE (Diff Reference)\n");
        prompt.push_str(&format!("Old Raw ID: {}\n", old.id));
        prompt.push_str(&format!("Old Content:\n```markdown\n{}\n```\n\n", old.content));
        prompt.push_str("INSTRUCTION: Identify what changed or evolved between the superseded source and active source, then update existing concept pages accordingly.\n\n");
    }

    if !existing_related_wiki_pages.is_empty() {
        prompt.push_str("## 3. CURRENT EXISTING WIKI CONTEXT (To Update / Link Against)\n");
        for page in existing_related_wiki_pages {
            prompt.push_str(&format!("Path: {}\n```markdown\n{}\n```\n\n", page.path, page.content));
        }
    }

    prompt.push_str("Synthesize the active raw source into OKF v0.2 concept documents. Emit the JSON payload of operations.");
    prompt
}
```

### How the Rust Compiler Handles the Payload

1. Invoke Model: Passes the System Prompt and User Prompt via the selected provider (`genai` / HTTP client).
2. Parse JSON Output: Extracts the `operations` array from the LLM's response.
3. Atomic Disk Operations: for `CREATE_OR_UPDATE`, write/overwrite `./wiki/concepts/<slug>.md`; for `DELETE`, remove obsolete concept files.
4. Trigger Validation (`okf lint`): instantly runs the Rust AST parser to confirm no broken `[[wikilinks]]` were created during LLM output, and that frontmatter matches OKF v0.2 YAML schemas.
5. Update Index (`./wiki/index.md`): regenerates the master table-of-contents catalog.

---

## Q6. How would the cli and mcp server behave in a computer with many okf [obsidian vaults]?

When running `okf` on a machine with multiple Obsidian vaults (or multiple OKF knowledge bundles), the CLI and MCP server transition from operating on a single implicitly scoped directory to an auto-discovery, multi-tenant registry model.

Since an Obsidian vault is just a directory containing Markdown files, `okf` leverages root-marker detection (finding `.okf/` or `.obsidian/` folders) to manage scoping seamlessly.

### 1. How the CLI Behaves Across Multiple Vaults

The CLI provides flexible execution: automatic contextual discovery, explicit target passing, and global registry management.

```
       [ Terminal User in /Users/dev/vaults/work/project-a ]
                                │
                                ▼
                       okf ingest <URL>
                                │
     ┌──────────────────────────┴──────────────────────────┐
     ▼                                                     ▼
[ Path Check ]                                   [ Global Registry ]
Finds .okf/ in current working dir            Falls back to default registered vault
--> Ingests to current local vault             if executed outside any vault root
```

**Contextual Resolution Strategy** — when you run a command like `okf compile` or `okf search`:
1. Current Working Directory (CWD) Detection: `okf` searches upward from the current shell path for a `.okf/` marker directory. If found, it scopes all operations to that specific vault.
2. Flag Target Override (`--vault`/`-V`): Override CWD detection from anywhere on your system:
   ```bash
   okf search "auth flow" --vault ~/Vaults/Personal
   okf compile --vault ~/Vaults/Work
   ```
3. Global Registry (`~/.config/okf/vaults.toml`): When executed in a non-vault directory (e.g., your home folder), `okf` uses the default vault designated in its global registry.

```toml
# ~/.config/okf/vaults.toml
default = "work"

[vaults.work]
path = "/Users/dev/Vaults/Work"
description = "Main corporate knowledge base"

[vaults.personal]
path = "/Users/dev/Vaults/Personal"
description = "Personal notes and technical research"

[vaults.project_a]
path = "/Users/dev/Projects/ProjectA/docs"
```

**Workspace Commands**

```bash
# List all discovered/registered OKF vaults on your system
okf vault list

# Register a new vault directory manually
okf vault add ~/Vaults/Research --name research

# Set the active global default
okf vault default research

# Cross-vault federated search
okf search "kubernetes" --all-vaults
```

### 2. How the MCP Server Behaves (okf-mcp)

AI client environments (like Claude Desktop, Cursor, or Windsurf) need to access multiple vaults without requiring a separate MCP process for each directory. `okf-mcp` handles this using two main patterns:

**Option A: Dynamic Multi-Vault MCP (Recommended)** — the MCP server starts globally and discovers available vaults dynamically. It exposes vault parameters across all tools while maintaining path sandboxing.

MCP Tools Schema Response:

```json
{
  "tools": [
    {
      "name": "okf_list_vaults",
      "description": "List all available OKF / Obsidian vaults registered on the host system."
    },
    {
      "name": "okf_search",
      "description": "Hybrid search across an OKF vault.",
      "parameters": {
        "type": "object", "properties": {
          "query": { "type": "string" },
          "vault": {
            "type": "string",
            "description": "Target vault name or path (optional; defaults to active context)."
          }
        }
      }
    }
  ]
}
```

MCP Execution Flow:

```
┌──────────────┐         1. okf_list_vaults          ┌───────────────────┐
│              ├────────────────────────────────────►│                   │
│   AI Agent   │◄────────────────────────────────────┤     okf-mcp       │
│ (Claude Code │         2. ["work", "personal"]     │  Global Protocol  │
│  / Cursor)   │                                     │     Daemon        │
│              │   3. okf_search("auth", vault="work")│                   │
│              ├────────────────────────────────────►│                   │
└──────────────┘                                     └─────────┬─────────┘
                                                               │
                                                ┌──────────────┴──────────────┐
                                                ▼                             ▼
                                      [ Vault: /work ]              [ Vault: /personal ]
                                      (.okf/index.db)               (.okf/index.db)
```

**Option B: Isolated Per-Vault MCP Config** — if you prefer hard boundaries between AI agents and specific vaults, you can configure individual MCP server instances in your client configuration file:

```json
// claude_desktop_config.json
{
  "mcpServers": {
    "okf-work": {
      "command": "okf-mcp",
      "env": {
        "OKF_VAULT_PATH": "/Users/dev/Vaults/Work"
      }
    },
    "okf-personal": {
      "command": "okf-mcp",
      "env": {
        "OKF_VAULT_PATH": "/Users/dev/Vaults/Personal"
      }
    }
  }
}
```

### 3. Obsidian Compatibility Layer

Since Obsidian stores vault settings inside `.obsidian/` and OKF stores compilation/CAS data inside `.okf/`, both tools co-exist in the same folder without collision:

```
/Users/dev/Vaults/Work/
├── .obsidian/               <-- Obsidian UI settings, plugins, workspace layout
├── .okf/                    <-- OKF manifests, CAS states, SQLite index
│   ├── manifest.json
│   └── index.db
├── raw/                     <-- OKF Immutable raw source inputs
├── wiki/                    <-- OKF Compiled concept graph
│   ├── index.md             <-- Rendered as standard Markdown index in Obsidian
│   └── concepts/             <-- Rendered natively in Obsidian with [[wikilinks]]
```

**Key Multi-Vault Safeguards**
- Strict Path Sandboxing: The MCP server prevents path traversal (`../../`) across vault boundaries. An operation aimed at `vault="personal"` cannot access files in `vault="work"`.
- Isolated SQLite Search Indexes: Each vault maintains its own `.okf/index.db` index. Running an `okf reindex` inside one vault updates only its local search state.
- Cross-Vault Wikilinks: Standard `[[concept]]` links resolve inside their local vault. Cross-vault references use standard URI schemes or explicit prefixes (e.g., `[[personal::journal-entry]]`), which `okf lint` flags appropriately during validation.

> **Implementation note (this repo):** "Option A: Dynamic Multi-Vault MCP" is the one adopted. See `docs/okf-mcp-implementation-plan.md`'s Multi-vault plan section for the concrete `sandbox_path`/`vault_resolver` design.

---

## Q7. Would the ingestion phase benefit from having merkle tree built-in in the app?

Yes, but with an important caveat: standard file-level SHA-256 flat hashes handle single document updates fine, but introducing a built-in Merkle Tree during the ingestion phase transforms `okf` into a distributed, incremental knowledge network. Whether it's worth the added complexity depends on the scale and how your pipeline handles content chunking.

### Where a Merkle Tree Provides Massive Benefits

**1. Granular / Chunk-Level Deduplication (LLM Cost Optimization)**

If you chunk documents before writing them to `./raw` (e.g., breaking a 200-page manual into paragraphs, sections, or markdown headers), a Merkle Tree becomes a huge advantage: leaf nodes are hashes of individual chunks/sections; the root hash represents the entire document's state. When a 50-page document changes by only 2 sentences:
- Flat Hash: entire file SHA-256 changes → LLM compiler must process the entire file again ($$$ spent on tokens).
- Merkle Tree: the Merkle Proof pinpoints the exact chunk that changed in O(log N) time. The LLM compiler re-processes only the dirty chunk and its immediate context.

**2. Multi-Vault & Peer-to-Peer Synchronization (Distributed OKF)**

If two engineers or agents are syncing independent vault replicas over a network, S3, or peer-to-peer (IPFS-style): replicas exchange top-level Merkle roots (O(1)); if roots match, vaults are 100% synchronized; if they differ, nodes traverse down tree branches in O(log N) steps to find missing or modified raw blobs without sending the full dataset over the network.

**3. Chunk-to-Wiki Lineage & Provenance**

Instead of saying `wiki/concepts/oauth.md` depends on `/raw/raw_a1f94d.md`, frontmatter can reference the exact Merkle leaf hash:

```yaml
sources:
  - resource: "/raw/raw_a1f94d.md"
    merkle_leaf: "sha256:7f8a92..." # References paragraph #12
```

### Where a Merkle Tree is Overkill

If your unit of ingestion is strictly atomic, whole files (e.g., fetching a web page as a single `.md` file), Git already acts as a built-in Merkle Tree (Commit → Tree → Blob). Storing `.okf/manifest.json` as a flat map of `URI -> File SHA256` gives you O(1) local change detection without custom tree data structure maintenance.

### Architecture Comparison

| Feature | Standard Flat Hash (Current Manifest) | Ingestion-Phase Merkle Tree |
|---|---|---|
| Change Detection Level | Whole File / URL | Paragraph / Chunk / Section |
| LLM Token Efficiency | High consumption on minor edits | Surgical edits (Only process dirty sub-trees) |
| P2P / Multi-Agent Sync | Must diff entire manifest lists | Fast branch traversal (O(log N) sync) |
| Complexity | Simple (`HashMap<URI, SHA256>`) | High (Tree balancing, chunk boundary rules) |

### Recommendation for the okf Rust Engine

Start with File-Level Hashes in `.okf/manifest.json` and defer to Git as your repository's primary Merkle tree. However, if you plan to introduce Section-Based Ingestion (or your OKF bundles grow into gigabytes of raw data across hundreds of vaults), implement a Merkle Tree for Chunked Sources:

```rust
// src/ingest/merkle.rs
pub struct MerkleChunk {
    pub leaf_hash: String,
    pub byte_range: (usize, usize),
    pub content: String,
}

pub struct MerkleDocumentTree {
    pub root_hash: String,
    pub leaves: Vec<MerkleChunk>,
}
```

This keeps the LLM compilation phase extremely fast and cost-effective when processing large file updates.

> **Implementation note (this repo):** per explicit decision, **no Merkle tree is implemented** — flat per-file SHA-256 in `.okf/manifest.json` (Q3's design) is the change-detection mechanism, and Git's own commit→tree→blob structure is treated as the durable Merkle-structured history. Chunk-level Merkle trees remain noted here as an explicit future-work option only, tied to section/chunk-level ingestion or P2P vault sync — neither of which is in scope. See `docs/okf-mcp-implementation-plan.md`.
