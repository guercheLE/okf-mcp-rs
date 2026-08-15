# Plan: `okf-mcp explore` — Obsidian-like vault explorer with a 3D graph view

_Planned and implemented 2026-08-15 (v0.11.0). Kept as the design record for the `explorer` module; the "as built" notes at the end record where the implementation diverged from the plan._

## Context

The user wants okf-mcp-rs to offer a command **and** an MCP tool that opens an HTML "playground"-style page for a vault with Obsidian-like features: a **3D graph view**, filtering, note navigation, wikilink following, backlinks — and search that reuses the **built-in hybrid search** (tantivy BM25 + sqlite-vec + RRF) instead of a client-side re-implementation.

**Feasibility: yes.** The repo already has every building block except three:
1. a link graph / backlink index (today the only link-derived state is a throwaway `HashSet` in `src/validator/rules.rs:127`),
2. markdown → HTML rendering + a static asset layer (none exists),
3. a browser-launch dependency.

Because search must hit the tantivy/sqlite-vec indexes on disk, the page cannot be a purely static file — it needs a small **local HTTP backend**. So the design is: a local axum server (bound to `127.0.0.1`, random free port by default) serving one self-contained HTML page plus a tiny JSON API; the CLI opens the browser automatically. A static `--export` single-file mode (client-side search only) is a cheap follow-up, noted at the end.

## Design

### New module `src/explorer/` (in the lib, declared in `src/lib.rs`)

| File | Responsibility |
|---|---|
| `mod.rs` | re-exports; `ExplorerState { vault_root: PathBuf, graph: RwLock<Graph> }` |
| `graph.rs` | Build the link graph from the vault. Reuse `core::vault_resolver::wiki_content_dirs` (`src/core/vault_resolver.rs:24`), `validator::rules::markdown_files_in` (`src/validator/rules.rs:61`), `validator::frontmatter::parse_wiki_page` (`src/validator/frontmatter.rs:50`), `validator::wikilink::extract_wikilinks` (`src/validator/wikilink.rs:20`). Output: `Graph { nodes: Vec<Node>, links: Vec<Link> }`; `Node { id (slug), title, r#type, tags, path, kind: "concept"\|"entity"\|"raw"\|"missing", in_degree, out_degree }`; `Link { source, target, cross_vault: bool }`. Also builds `backlinks: HashMap<slug, Vec<slug>>`. Unresolved `[[x]]` become `kind: "missing"` nodes (Obsidian-style dim nodes). **Tags become `kind: "tag"` nodes** (id `#tag`, green like Obsidian) with a `page → #tag` link per frontmatter tag, so tag hubs show up and are subject to the same hub filter. Raw sources (from frontmatter `sources[].resource`) become `kind: "raw"` nodes so the graph shows provenance (toggleable in the UI). |
| `render.rs` | `render_page(vault_root, slug) -> PageView { slug, title, frontmatter: serde_json::Value, html, backlinks: Vec<{slug,title,snippet-line}> , outlinks }`. Markdown → HTML with `pulldown-cmark`; pre-pass rewrites `[[slug]]` / `[[slug\|alias]]` / `[[vault::slug]]` to `<a class="wikilink" data-slug="…">` (missing → `class="wikilink missing"`). Frontmatter is parsed to a generic `serde_yaml::Value` → JSON so the pane shows *all* fields (`generated`, `status`, `stale_after` — which `WikiFrontmatter` currently drops). Path safety through `sandbox_path` (`src/core/vault_resolver.rs:75`). |
| `server.rs` | axum `Router` (axum 0.8 already a dep): `GET /` → embedded HTML; `GET /api/graph`; `GET /api/page/{slug}`; `GET /api/search?q=&limit=` → `search::query::hybrid_search` (`src/search/query.rs:225`) wrapped in `tokio::task::spawn_blocking` (it's sync: tantivy + fastembed); `GET /api/raw/{raw_id}` (render raw source); `POST /api/refresh` (rebuild graph). `serve(vault_root, host, port, open) -> anyhow::Result<SocketAddr>` binds `127.0.0.1:{port}` (port 0 = OS-assigned), prints URL, optionally opens the browser. Warm the embedding model on startup in a background task so the first search isn't slow. Errors → `{ "error": "..." }` with proper status. |
| `assets/explorer.html` | The single-page UI, `include_str!`'d. Inline CSS/JS. Vendored `assets/vendor/3d-force-graph.min.js` (MIT; bundles three.js, ~1 MB) also `include_str!`'d and injected into a `<script>` so the page works fully offline. |

Frontend features (mirroring the Obsidian screenshots):
- **3D graph** via 3d-force-graph: nodes colored by kind/type (concept/entity/raw/missing — legend), **sized by inbound-link count (in-degree)** — bigger node = bigger hub; hover label shows title + in/out counts; click → opens note; double-click → focus/fly-to; drag/orbit/zoom; link particles optional.
- **Hub filter (island/cluster discovery)**: the backend ranks every node by hub importance (`hub_rank` = dense rank by in-degree, plus the raw `in_degree`, both in `/api/graph`). Ranking is **kind-agnostic**: notes (`gemini-apps`, `gemini-apps-activity`), tags (`#travel`, `#gemini`), raw sources and missing nodes are all ranked in the same list purely by in-degree — a heavily-linked note is a hub exactly like a popular tag. The UI exposes a **"Hide hubs" slider/threshold**: hide all nodes whose in-degree ≥ N (or top-K% by hub_rank), regardless of kind, with a live count "hiding 37 hubs (12 notes, 25 tags) / 1 204 nodes". A per-kind checkbox row lets the user restrict the hub filter to a subset of kinds when wanted (default: all kinds). Hidden hubs' edges are removed too, so weakly-connected islands and clusters that hubs normally glue together become visible; the force layout re-settles. Also: "Show only hubs" inverse toggle, and a ranked hub list (top 50) in the panel — click to reveal/hide one hub individually or jump to it. Optionally compute connected components after filtering and color islands distinctly ("Color by cluster" toggle, computed client-side).
- **Filters panel** (like Obsidian's graph settings): text filter (title/tag/type substring), tag multiselect, type multiselect, toggle tag nodes / raw sources / missing nodes / orphans-only (tag nodes on by default, matching Obsidian; hiding them is itself a quick way to break tag-driven hubs), "local graph" depth slider (1–3 hops from selected note), force sliders (link distance, repulsion). All filters compose (AND) with the hub filter.
- **Note pane** (right side): rendered markdown, properties table from frontmatter, `sources` rendered as clickable raw-source links, **backlinks** list ("Menções vinculadas") with count, outgoing links; wikilinks navigate in-pane and re-center the graph; back/forward history (browser `pushState` with `#/note/<slug>`).
- **Search box**: debounced call to `/api/search` (hybrid), results list with snippet + score; selecting highlights hits in the graph (others dimmed) and opens the top hit. This is the "use built-in search" requirement.
- **Sidebar file tree** (concepts / entities / raw), collapsible.
- Keyboard: `/` focus search, `Esc` clear, `←/→` history.

### CLI: `okf-mcp explore`

- `src/main.rs`: add `Command::Explore { port: u16 (default 0), host (default 127.0.0.1), no_open: bool }` following the pattern at `src/main.rs:48-194` + dispatch arm at `:329-413`.
- `src/cli/explore.rs`: `resolve_vault(vault)` → `explorer::server::serve(...)`; blocks until Ctrl-C. Register in `src/cli/mod.rs`.

### MCP tool: `okf-explore`

- `src/core/mcp_server.rs`: new arg struct `ExploreArgs { vault: Option<String>, open: Option<bool> }` (pattern at `:60-228`), tool fn next to the others (pattern `okf-ingest` at `:334-366`). Spawns the explorer server as a background tokio task (idempotent per vault: keep `HashMap<PathBuf, SocketAddr>` in `OkfServer` like the synthesize-job map) and returns `{ url, vault_root, opened }`. Update the `get_info()` instructions string (`:960-978`) and the "twelve fixed tools" doc comment/README list.

### Dependencies (Cargo.toml)

`pulldown-cmark` (markdown), `open` (browser launch), and vendor `3d-force-graph.min.js` under `assets/vendor/` with its LICENSE. No CDN — offline-first. `serde_yaml` (already present) for generic frontmatter.

### Docs

README: new "Explore" section (CLI + MCP tool + screenshots later); CHANGELOG entry; bump tool count.

## Verification

1. `cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test --locked` (CI gates).
2. Unit tests (inline `#[cfg(test)]`, repo convention): `graph.rs` — temp vault with 3 pages + one missing link + one cross-vault link → correct nodes/links/backlinks/degrees; `render.rs` — wikilink rewrite (alias, cross-vault, missing), frontmatter passthrough of `generated`.
3. Integration test `tests/explorer_api.rs` using `tower::ServiceExt::oneshot` (as `src/http/server.rs` tests do): `/`, `/api/graph`, `/api/page/{slug}`, 404 for unknown slug, path-traversal slug rejected, `/api/search` after `reindex` on a temp vault.
4. `tests/cli_smoke.rs`: `okf-mcp explore --help` exits 0.
5. Manual: `cargo run -- explore --vault <path>` → browser opens; graph renders; click node → note pane; search "gemini" → hits highlighted; backlinks count matches Obsidian for a known page (e.g. `gemini-apps` → 65).

## Follow-ups (not in this change)

- `--export out.html`: bake graph JSON + rendered pages into one static file (client-side substring search only).
- 2D graph toggle (`force-graph` sibling lib), theme switch, edit-in-place.

## As built (2026-08-15)

- Module layout matches the plan (`src/explorer/{mod,graph,render,server}.rs`, `assets/explorer.html`, vendored `assets/vendor/3d-force-graph.min.js` v1.80.0 + LICENSE). The vendored bundle is served at `/vendor/3d-force-graph.min.js` (embedded via `include_str!`) rather than inlined into the HTML.
- `GraphNode.kind` ∈ `concept | entity | tag | raw | cross_vault | missing`; `GraphLink.kind` ∈ `wikilink | tag | source | cross_vault`. Node ids: slug, `#tag`, `raw:<stem>`, `<vault>::<slug>`. `hub_rank` is a dense rank by in-degree over *all* kinds.
- `/api/page/{*id}` renders any node with a file (pages and cited raw sources) **and** any uncited `raw:<stem>` (search can surface raw files no page cites yet), always through `sandbox_path`.
- `/api/search` short-circuits empty queries, falls back to the page description when a BM25-only hit has no snippet, and returns a `hint` when the vault has never been indexed.
- The MCP `okf-explore` tool keeps a per-session `HashMap<canonical vault root, ExplorerHandle>` and reuses a live server (`already_running: true`).
- Selection only mildly dims non-neighbors; hover dims strongly (Obsidian-like). Note pane starts collapsed under 1100px viewport width.
- Hardening added after a multi-agent review pass (13 confirmed findings, 3 refuted): Markdown is rendered on pulldown-cmark's event stream with `ENABLE_WIKILINKS` (so `[[..]]` inside code is left alone), raw HTML blocks/inline HTML are escaped to text (ingested web pages can carry `<img onerror>`-style payloads), `javascript:`/`data:` link and image URLs are dropped; `sources[].resource` is resolved through `sandbox_path` in the graph builder; the server enforces a `Host`-header allowlist (bound address + `localhost`, DNS-rebinding defence, mirroring the MCP transport's `with_allowed_hosts`); the embedding warm-up runs on a detached OS thread with `catch_unwind` so Ctrl-C never waits on a model download; frontend: note-pane request sequencing, Esc recomputes a local-depth view and clears the hash, refresh keeps filter state, hub list can un-hide threshold-hidden hubs (`manualShown`), `sources` chip ids match the server's stem rule, and the non-functional "always show labels" toggle was removed.
- Deferred, as planned: `--export` static single-file mode, 2D toggle.
