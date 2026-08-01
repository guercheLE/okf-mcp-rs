# Changelog

All notable changes to this project are documented in this file, reconstructed retrospectively from git history in [Keep a Changelog](https://keepachangelog.com/en/1.1.0/) format. This project follows [Semantic Versioning](https://semver.org/).

## [0.6.0] - 2026-08-01

### Added
- `lint --fix`: mechanically repairs auto-fixable lint findings — missing `.md` extensions on `sources:` entries and `tid:`/`id:` frontmatter typos — before reporting. Never calls an LLM, matching `lint`'s own design intent.
- `compile`/`rebuild`/`run --fix`: the same mechanical repairs, plus LLM-assisted synthesis of a missing concept page for any broken link still remaining afterward, grounded only in the raw sources the referencing pages already cite. New `--yes`/`-y` skips the confirmation prompt required before committing LLM-synthesized changes; a non-interactive session never auto-commits without it.
- MCP tool parity: `fix` argument on `okf-lint`/`okf-compile`/`okf-rebuild`.

### Fixed
- Wikilink parsing had no support for Obsidian's piped-alias syntax (`[[target|display text]]`) — every such link was reported as broken regardless of whether its target existed, since the entire `target|display text` string was used as the lookup slug. `[[target|display text]]` now resolves on `target` only.

## [0.5.0] - 2026-08-01

### Added
- `okf-mcp models <provider>` (and MCP tool `okf-list-models`): lists a provider's available models directly, instead of only surfacing them as a side effect of an invalid `--model` error.

## [0.4.1] - 2026-08-01

### Fixed
- `compile`/`rebuild`/`run` could hang forever on a stalled or never-responding LLM provider (e.g. a rate-limited/queued free-tier OpenRouter model) — `genai::Client::default()` builds its HTTP client with no request timeout at all. Now bounded to 300s per request.

## [0.4.0] - 2026-08-01

### Added
- Seven more LLM providers alongside Anthropic/OpenAI/Groq/Ollama: Gemini, OpenRouter, DeepSeek, xAI, Together, Ollama Cloud, and Moonshot, each with genai-matched default base URLs and documented API key env vars.
- `--tag` is now repeatable on `ingest`/`run` (`--tag github --tag repository --tag mcpify`), stored as a list in `raw/*.md` frontmatter instead of accepting only one value.

### Fixed
- Anthropic's hardcoded default base URL was missing the `/v1` path segment (`https://api.anthropic.com` instead of `https://api.anthropic.com/v1/`), producing a silent 404 with an empty body on every `compile`/`rebuild`/`run` call — the same "trailing path segment" bug class as the already-documented Ollama gap, just on the other end of the URL.
- `source_url` in `raw/*.md` frontmatter was left `null` for local files even though `ingest::pipeline::normalize_local_uri` already computes a stable `file://<canonical path>` URI and uses it as the manifest key — now reused verbatim instead of leaving a second, potentially-diverging identifier unset.

## [0.3.1] - 2026-08-01

### Changed
- README's Pipeline section now renders as a Mermaid diagram instead of an ASCII block that overflowed horizontally in GitHub's rendered view.
- `docs/design-gaps.md` now spans the project's full release history (each entry carries its own "Fixed in" version instead of being grouped under one), and adds the `v0.2.1` Ollama base-URL gap.

## [0.3.0] - 2026-08-01

A batch of fixes and small features from real-world usage feedback: vault lifecycle, compile resumability/commit-safety, provider auth, transport-aware output, and layered config/connection diagnostics. See [docs/plan-2026-08-fixes.md](docs/plan-2026-08-fixes.md) for the full investigation and design rationale, and [docs/design-gaps.md](docs/design-gaps.md) for what the original planning docs missed that let each of these ship in the first place.

### Added
- `vault create <path> --name <name>`: scaffold a brand-new vault directory (`.okf/`, `wiki/concepts/`, `raw/`) and register it, distinct from `vault add` (which only registers an existing directory).
- `vault delete <name> --force`: unregister a vault AND permanently delete its directory tree. Requires `--force`; deletes the directory before unregistering so a failed delete stays retryable.
- `vault remove` / `vault rm`: unregister a vault from the registry without touching its files.
- `okf-mcp kb ...`: full alias for `okf-mcp vault ...` — same commands under two names, for Obsidian-oriented and OKF-oriented workflows respectively.
- Per-source compile resumability: `.okf/manifest.json` now tracks `compiled_hash` per source, set only once that source's full operation batch applies without error. A crash/kill mid-`compile` leaves already-succeeded sources durably resumable on the next run.
- `okf-mcp lint` now surfaces a "Pending compile" section first, listing active sources ingested but not yet fully compiled — the likely root cause of downstream broken-link/missing-source symptoms.
- Transport-aware progress reporting for `compile`/`rebuild`/`reindex --embeddings`/`ingest`: per-source/document `[i/N] .../done/failed` lines, routed to stdout for CLI and MCP-over-HTTP, stderr for MCP-over-stdio (where stdout is the JSON-RPC channel).
- `.okf/config.toml`'s `[providers.<name>]` table (`base_url`, `api_key_env`) now actually takes effect for CLI `compile`/`rebuild`/`run` — previously parsed but never consulted.
- On a model-not-found error, `compile`/`rebuild`/`run` now append the provider's available-model list to the error message.
- `test-connection` now probes every configured LLM provider (not just Firecrawl), reporting per-provider credential presence and reachability.
- `config` now groups output by source: global config file, local config file, env vars actually set, vault-level config, and per-provider credential-storage presence — instead of a single flat merged dump.
- `--help` text for `compile`/`rebuild`/`run`'s `--model` flag now recommends an 8B-9B instruct model as the best local speed/quality balance.
- A checked-in pre-commit hook (`.githooks/pre-commit`) runs `cargo fmt --check` and `cargo clippy -- -D warnings` before each commit.

### Fixed
- `raw/*.md` blobs hardcoded `okf_version: "1.0"` while every other schema location used `"0.2"` — a leftover from an earlier illustrative doc example. Now a single shared `OKF_SCHEMA_VERSION` constant.
- LLM provider API keys saved via `okf-mcp setup` could be silently shadowed by a blank/whitespace-only environment variable of the same name (`std::env::var(name).is_ok()` is true even for `""`), producing a 401 that looked like a setup problem when it wasn't. Also now trims whitespace from keys at save and load time.
- `compile`/`rebuild`/`run` no longer commit or write `okf.json` when any source failed or lint reports errors — previously the broken/partial wiki state was committed *before* the failure was even reported.
- The embedding model cache (`fastembed`, ~415 MiB) now resolves to a stable, absolute directory (`FASTEMBED_CACHE_DIR`, then `<home>/.okf-mcp/models`, then a directory next to the executable) instead of a relative path resolved against whatever the process's CWD happened to be — it was re-downloading on every run where CWD differed from a prior one.
- `okf-mcp setup`'s "How should these settings be saved?" prompt now makes explicit that it only applies to Firecrawl settings — the LLM provider key is always saved to credential storage regardless of that choice, which was previously unstated and easy to misread.
- `cargo fmt --check` had been failing on `main` since at least the `v0.2.0`/`v0.2.1` release commits, aborting CI before clippy/tests/coverage ever ran.
- A latent test-isolation race: `credential_storage`'s and `config_manager`'s tests both mutated the process-global `HOME` env var without synchronization, which could interleave under `cargo test`'s default parallel execution and break decryption non-deterministically. Now serialized by a shared lock.

## [0.2.1] - 2026-07-31

### Fixed
- Ollama base URL normalization: missing trailing slash/scheme now handled gracefully instead of producing malformed request URLs.

## [0.2.0] - 2026-07-31

### Changed
- **Breaking**: rebuilt `okf-mcp` from a generic Firecrawl proxy into a full OKF (Open Knowledge Format) pipeline engine — ingest, compile, lint, reindex, and search a Git-native knowledge base, as both an MCP server and a CLI.

## [Unreleased] - 2026-07-30

### Added
- Initial commit.

[0.6.0]: https://github.com/guercheLE/okf-mcp-rs/compare/v0.5.0...v0.6.0
[0.5.0]: https://github.com/guercheLE/okf-mcp-rs/compare/v0.4.1...v0.5.0
[0.4.1]: https://github.com/guercheLE/okf-mcp-rs/compare/v0.4.0...v0.4.1
[0.4.0]: https://github.com/guercheLE/okf-mcp-rs/compare/v0.3.1...v0.4.0
[0.3.1]: https://github.com/guercheLE/okf-mcp-rs/compare/v0.3.0...v0.3.1
[0.3.0]: https://github.com/guercheLE/okf-mcp-rs/compare/v0.2.1...v0.3.0
[0.2.1]: https://github.com/guercheLE/okf-mcp-rs/compare/v0.2.0...v0.2.1
[0.2.0]: https://github.com/guercheLE/okf-mcp-rs/compare/4d16659...v0.2.0
