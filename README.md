# okf-mcp

A Git-native, offline-first **OKF (Open Knowledge Format) pipeline**: ingest web pages (via Firecrawl) or local files into an immutable `./raw/` archive, compile them into a linked `./wiki/` of concept pages with an LLM, lint the result, and search it — hybrid full-text + dense-vector, merged with Reciprocal Rank Fusion. One binary, usable as a CLI (`okf-mcp <command>`) and as an MCP server (`okf-mcp start` / `okf-mcp http`) for AI coding agents.

[![Sponsor](https://img.shields.io/github/sponsors/guerchele?label=Sponsor&logo=github&color=EA4AAA)](https://github.com/sponsors/guerchele)

Building and maintaining this took real ideation, time, design effort, and compute (including LLM usage) to get right. If it's useful to you, consider [sponsoring its development](https://github.com/sponsors/guerchele) — any amount helps keep it going. 💛

Ten fixed, well-defined tools — `okf-ingest`, `okf-compile`, `okf-rebuild`, `okf-lint`, `okf-reindex`, `okf-search`, `okf-delete`, `okf-read-index`, `okf-read-concept`, `okf-list-vaults` — not a discovery layer over an unknown API surface. Each does exactly one named thing; `okf-search`'s hybrid ranking is an internal implementation detail, not something you need to know about to use it.

## Pipeline

```
[Web/File] --Firecrawl/local parse--> ./raw (immutable, hashed) --LLM compile--> ./wiki --lint/validate--> git commit
                                                                                      |
                                                                             okf-reindex / okf-search
```

- **`./raw/`** — append-only. Every ingested source is hashed (SHA-256) and tracked in a content-addressable manifest (`.okf/manifest.json`): re-ingesting unchanged content is a no-op, changed content supersedes the old version (never overwritten), and deletion is a soft tombstone by default (`--purge` for a hard delete).
- **`./wiki/concepts/`** — LLM-compiled, atomic, cross-linked (`[[concept-slug]]`) Markdown pages, one concept per file, each declaring its raw-source provenance in frontmatter.
- **`okf-mcp lint`** — checks for dangling links, missing provenance, and orphan pages before you commit.
- **`okf-mcp reindex` / `okf-mcp search`** — a local Tantivy (BM25) + `sqlite-vec` (dense embeddings) index, merged via RRF; no external service.

See [docs/okf-pipeline-design.md](docs/okf-pipeline-design.md) for the full design rationale and [docs/okf-mcp-implementation-plan.md](docs/okf-mcp-implementation-plan.md) for how it was built.

## Install

```bash
cargo build --release
```

Builds two binaries into `target/release/`: `okf-mcp` (the CLI/server below) and `okf-mcp-healthcheck` (used by the Dockerfile's `HEALTHCHECK`). Run `cargo install --path .` instead if you want `okf-mcp` on your `PATH`.

## Quick start

```bash
okf-mcp setup                                          # Firecrawl API key + an LLM provider key
mkdir -p my-vault/.okf && cd my-vault                   # any directory with a .okf/ marker is a vault
okf-mcp ingest https://example.com/some-article
okf-mcp compile --model anthropic/claude-3-5-sonnet
okf-mcp lint
okf-mcp reindex --embeddings
okf-mcp search "what did that article say about X"
```

`okf-mcp run <URL|FILE>` does ingest → compile → lint → commit as one step.

## Vaults

A **vault** is any directory with a `.okf/` marker — the CLI resolves which one to use, in order: an explicit `--vault <name|path>` flag (global, works on every subcommand) → walking up from the current directory looking for `.okf/` → the registry's default vault.

```bash
okf-mcp vault add ~/Vaults/Personal --name personal --description "Personal notes"
okf-mcp vault default personal
okf-mcp vault list
okf-mcp search "kubernetes" --all-vaults          # federated search across every registered vault
```

The registry lives at `~/.config/okf/vaults.toml` — deliberately separate from `~/.okf-mcp/` (this binary's own settings), since it's a convention other OKF-format tools could reasonably also read. Every operation is sandboxed to its resolved vault root; an MCP tool call scoped to one vault cannot read or write another's files.

An OKF vault happens to also be a valid Obsidian vault (`.obsidian/` and `.okf/` coexist without collision), so `./wiki/` renders natively with working `[[wikilinks]]` if you open the same directory in Obsidian.

## CLI reference

| Command | Args | Description |
| --- | --- | --- |
| `ingest` | `<URL\|FILE> [--tag]` | Fetch a URL (via Firecrawl) or read a local file and add it to the vault's raw sources |
| `compile` | `[--model] [--diff]` | Compile newly-ingested raw sources into wiki concept pages |
| `rebuild` | `[--model] [--force]` | Recompile the wiki from all active raw sources |
| `lint` | `[--strict] [--json]` | Check wikilinks, frontmatter, and source provenance in the wiki |
| `reindex` | `[--embeddings]` | Rebuild the local text and vector index used by search |
| `search` | `<query> [-l/--limit] [--json] [--all-vaults]` | Search ingested raw sources and compiled wiki concepts |
| `delete` | `<URL\|FILE> [--purge]` | Remove a source from the vault (soft by default; `--purge` hard-deletes) |
| `run` | `<URL\|FILE> [--tag] [--model]` | Ingest, compile, lint, and commit as one step |
| `vault list` / `vault add` / `vault default` | — | Manage the vault registry |
| `setup` | — | Configure the Firecrawl API key and an LLM provider key |
| `test-connection` | — | Verify the configured Firecrawl API key is reachable |
| `config` | — | Print the resolved configuration (secrets redacted) |
| `version` | — | Print the installed version |
| `start` / `http` | `[--host] [--port] [--cors-allow]` | Start the MCP server over stdio / HTTP |

Every content-touching subcommand also accepts the global `--vault <name|path>` flag.

## MCP tools

| Tool | Args | Description |
| --- | --- | --- |
| `okf-ingest` | `source, tag?, vault?` | Fetch a URL or read a local file and add it to the vault's raw sources |
| `okf-compile` | `diff?, model?, temperature?, base_url_override?, vault?` | Compile newly-ingested raw sources into wiki concept pages |
| `okf-rebuild` | `force?, model?, temperature?, base_url_override?, vault?` | Recompile the wiki from all active raw sources |
| `okf-lint` | `strict?, vault?` | Check wikilinks, frontmatter, and source provenance in the wiki |
| `okf-reindex` | `embeddings?, vault?` | Rebuild the local text and vector index used by search |
| `okf-search` | `query, limit?, vault?` | Search ingested raw sources and compiled wiki concepts |
| `okf-delete` | `source, purge?, vault?` | Remove a source from the vault |
| `okf-read-index` | `vault?` | Read the wiki's table-of-contents page |
| `okf-read-concept` | `id_or_path, vault?` | Read a single compiled wiki concept page |
| `okf-list-vaults` | — | List every vault registered on this machine |

### Connect an MCP client

**stdio:**

```json
{
  "mcpServers": {
    "okf-mcp": {
      "command": "okf-mcp",
      "args": ["start"]
    }
  }
}
```

Use the absolute executable path if `okf-mcp` is not on the MCP host's `PATH`.

**HTTP:** every request must carry its own Firecrawl `Authorization` header — HTTP transport intentionally never falls back to server-side config for the `okf-ingest` tool's outbound Firecrawl call:

```json
{
  "mcpServers": {
    "okf-mcp": {
      "url": "http://127.0.0.1:3000/mcp",
      "headers": {
        "Authorization": "Bearer <your Firecrawl API key>"
      }
    }
  }
}
```

Keep the listener on localhost unless you've added appropriate network access controls and TLS in front of it. `okf-compile`/`okf-rebuild`'s LLM provider keys are configured server-side (via `okf-mcp setup` or env vars) regardless of transport — they aren't part of this per-request credential model.

## Configuration

| Env var | Purpose |
| --- | --- |
| `OKF_MCP_FIRECRAWL_BASE_URL` | Firecrawl API base URL (default `https://api.firecrawl.dev`) |
| `OKF_MCP_TOKEN` / `OKF_MCP_API_KEY` | Firecrawl API key — checked before the OS keychain/encrypted-file fallback `okf-mcp setup` writes to |
| `ANTHROPIC_API_KEY` / `OPENAI_API_KEY` / `GROQ_API_KEY` | LLM provider keys, read by whichever `<provider>/<model>` you pass to `--model` |
| `OLLAMA_HOST` | Overrides Ollama's default `http://localhost:11434` |
| `CUSTOM_LLM_API_KEY` / `CUSTOM_LLM_BASE_URL` | For `custom/<model>` — any other OpenAI-compatible endpoint |
| `OKF_MCP_LOG_LEVEL` | Log verbosity (`trace`/`debug`/`info`/`warn`/`error`) |

See `.env.example` for the full list. `--model` always requires an explicit `<provider>/<model_name>` prefix (e.g. `anthropic/claude-3-5-sonnet`, `ollama/llama3.2`) — no "guess the provider" fallback, since that's ambiguous whenever more than one provider's key happens to be set. Falls back to a vault's `.okf/config.toml` `[compiler].default_model` when `--model` is omitted.

```toml
# <vault>/.okf/config.toml
[compiler]
default_model = "anthropic/claude-3-5-sonnet"
temperature = 0.2
max_tokens = 4096
```

## Docker

```bash
# Stdio: the MCP client launches this one-off process and owns its stdin/stdout pipes
docker compose run --rm -T okf-mcp

# HTTP: a long-running network endpoint published on http://localhost:3000
docker compose up okf-mcp-http
```

Both services read configuration from a local `.env` file (copy `.env.example`), and mount `~/.okf-mcp` (credentials), `~/.config/okf` (vault registry), and `~/okf-vaults` (vault data, at `/vaults` in the container) from the host.

For iterative local development against your working tree instead of a pinned image, use `docker-compose.dev.yml`:

```bash
docker compose -f docker-compose.dev.yml up --build
```

## Observability & Resilience

### Logging

Structured logs go to **stderr** (never stdout, which is reserved for MCP JSON-RPC frames on stdio transport): JSON by default, pretty-printed automatically when stderr is an interactive TTY. Level is controlled by `OKF_MCP_LOG_LEVEL` (default `info`), passed straight through to `tracing_subscriber::EnvFilter`, so directive syntax works too:

```bash
OKF_MCP_LOG_LEVEL="okf_mcp=debug,warn" okf-mcp start
```

### OpenTelemetry tracing

An OTLP/HTTP trace exporter is built at startup; if it fails to build, tracing export is silently skipped. Point it at a collector with the OTLP SDK's own standard env vars:

```bash
OTEL_EXPORTER_OTLP_ENDPOINT=http://otel-collector:4318 okf-mcp start
```

Defaults to `http://localhost:4318` if unset.

### Metrics

`GET /metrics` (HTTP transport only) serves a minimal hand-rolled Prometheus-text counter store — currently `http_requests_total`.

### Circuit breaker, retries, and rate limiting

Outbound Firecrawl calls (`services/api_client.rs`) pass through a rate limiter, then a circuit breaker, then a retry loop:

| Behavior | Configurable? | Knob | Default |
| --- | --- | --- | --- |
| Request timeout | Yes | `timeout_ms` | 30000 ms |
| Retry attempts | Yes | `retry_attempts` | 3 (immediate retry, no backoff) |
| Rate limit | Partially | `rate_limit` | 100 calls; 1s window, not configurable |
| Circuit breaker | No | — | opens after 5 consecutive failures, 30s half-open trial |

### Health checks

`GET /healthz` (HTTP transport only) reports process health — `Healthy` by default, since (unlike the old generic-catalog scaffold this project started as) there's no single compiled-in resource to check; each tool call resolves and validates its own vault per-call. `okf-mcp-healthcheck` (the Dockerfile's `HEALTHCHECK`) probes `/healthz` when the configured transport is HTTP, or checks the resolved vault's `.okf/` directory is readable for stdio deployments, which have no HTTP endpoint to probe.

### Credential storage

`okf-mcp setup` writes credentials to the OS-native secret store via the `keyring` crate (macOS Keychain / Windows Credential Manager / Linux Secret Service), under service `okf-mcp` — the Firecrawl key under account `firecrawl`, each LLM provider key under `llm-<provider>`. Falls back automatically to an AES-256-GCM-encrypted file at `~/.okf-mcp/credentials.enc` if no OS keychain backend is available.

## Testing

```bash
cargo test
```

### Pre-commit hook

A checked-in hook runs `cargo fmt --check` and `cargo clippy --all-targets -- -D warnings` before each commit — the same two gates CI checks first — so formatting/lint drift never reaches a commit in the first place. Enable it once per clone:

```bash
git config core.hooksPath .githooks
```

## Coverage

```bash
bash scripts/coverage.sh
```

Requires Python 3, `cargo-llvm-cov`, and the `llvm-tools-preview` Rust component.

## Profiling

```bash
bash scripts/profile.sh        # CPU profiling via samply, over search::hybrid_search
bash scripts/profile-heap.sh   # steady-state heap profiling via dhat-rs
```

Both run against a small fixture vault the profiling harness (`examples/profile_search.rs`) builds itself, so no real vault or Firecrawl/LLM credentials are needed. Requires [samply](https://github.com/mstange/samply) (`cargo install samply`) for CPU profiling.

## License

MIT — see [LICENSE](LICENSE).
