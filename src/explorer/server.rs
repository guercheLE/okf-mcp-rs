//! The explorer's local HTTP server: one embedded HTML page, the vendored
//! 3D graph library, and a small JSON API over `explorer::graph` /
//! `explorer::render` / `search::hybrid_search`.
//!
//! Binds `127.0.0.1` only by default (`port` 0 = let the OS pick), never
//! authenticates — it's a local viewer for a local vault, on the same trust
//! footing as opening the files in an editor. Not to be confused with
//! `http::server`, the authenticated MCP transport.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::Router;
use axum::extract::{Query, Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{Html, IntoResponse, Json, Response};
use axum::routing::{get, post};
use serde::Deserialize;
use serde_json::json;
use tokio::sync::RwLock;
use tokio::task::JoinHandle;

use crate::search;

use super::graph::{Graph, build_graph};
use super::render::render_page;

const EXPLORER_HTML: &str = include_str!("../../assets/explorer.html");
const FORCE_GRAPH_JS: &str = include_str!("../../assets/vendor/3d-force-graph.min.js");
const MERMAID_JS: &str = include_str!("../../assets/vendor/mermaid.min.js");
const VENDOR_JS_ROUTE: &str = "/vendor/3d-force-graph.min.js";
const MERMAID_JS_ROUTE: &str = "/vendor/mermaid.min.js";

/// One running explorer. Dropping the handle does *not* stop the server —
/// abort `task` for that (the CLI just awaits it until Ctrl-C; the MCP tool
/// keeps it alive for the session).
pub struct ExplorerHandle {
    pub addr: SocketAddr,
    pub url: String,
    pub task: JoinHandle<()>,
}

struct ExplorerState {
    vault_root: PathBuf,
    graph: RwLock<Arc<Graph>>,
}

type AppState = Arc<ExplorerState>;

/// Rejects requests whose `Host` header names anything other than the
/// address this server was bound at (or `localhost` on the same port).
/// Binding to loopback keeps other machines out, but not a web page the
/// user has open elsewhere: a DNS-rebinding page can point its own hostname
/// at 127.0.0.1 and read the vault through the browser — its requests carry
/// *its* hostname in `Host`, which this check refuses. Same idea as the MCP
/// transport's `with_allowed_hosts`.
async fn require_expected_host(
    State(allowed): State<Arc<Vec<String>>>,
    request: Request,
    next: Next,
) -> Response {
    let host = request
        .headers()
        .get(header::HOST)
        .and_then(|h| h.to_str().ok())
        .map(str::trim)
        .unwrap_or_default();
    if allowed.iter().any(|a| a.eq_ignore_ascii_case(host)) {
        next.run(request).await
    } else {
        error_json(
            StatusCode::FORBIDDEN,
            format!("unexpected Host header '{host}'"),
        )
    }
}

/// The `Host` values `require_expected_host` accepts for a server bound at
/// `addr` (plus `host` as the user typed it, for `--host my-hostname`).
fn allowed_hosts(addr: SocketAddr, host: &str) -> Vec<String> {
    let port = addr.port();
    let mut allowed = vec![
        addr.to_string(),
        format!("localhost:{port}"),
        format!("127.0.0.1:{port}"),
        format!("[::1]:{port}"),
        format!("{host}:{port}"),
    ];
    allowed.sort();
    allowed.dedup();
    allowed
}

/// Builds the router for `vault_root`, computing the initial graph now so
/// the first page load is instant. `allowed_hosts` is the `Host`-header
/// allowlist (see `require_expected_host`); `None` disables the check (the
/// in-process API tests, which send no `Host`). `serve` is the entry point
/// everything else uses.
pub fn build_router(
    vault_root: &Path,
    allowed_hosts: Option<Vec<String>>,
) -> anyhow::Result<Router> {
    let graph = build_graph(vault_root)?;
    let state: AppState = Arc::new(ExplorerState {
        vault_root: vault_root.to_path_buf(),
        graph: RwLock::new(Arc::new(graph)),
    });
    let router = Router::new()
        .route("/", get(index_html))
        .route(VENDOR_JS_ROUTE, get(vendor_js))
        .route(MERMAID_JS_ROUTE, get(mermaid_js))
        .route("/api/info", get(api_info))
        .route("/api/graph", get(api_graph))
        .route("/api/page/{*id}", get(api_page))
        .route("/api/search", get(api_search))
        .route("/api/refresh", post(api_refresh))
        .with_state(state);
    Ok(match allowed_hosts {
        Some(allowed) => router.layer(middleware::from_fn_with_state(
            Arc::new(allowed),
            require_expected_host,
        )),
        None => router,
    })
}

/// Binds and starts serving in a background task; returns immediately with
/// the bound address. `open_browser` launches the user's default browser at
/// the URL (failure to do so is logged, not fatal — the URL is still
/// returned/printed for manual use).
pub async fn serve(
    vault_root: &Path,
    host: &str,
    port: u16,
    open_browser: bool,
) -> anyhow::Result<ExplorerHandle> {
    let addr: SocketAddr = format!("{host}:{port}")
        .parse()
        .map_err(|err| anyhow::anyhow!("invalid explorer bind address '{host}:{port}': {err}"))?;
    let listener = tokio::net::TcpListener::bind(addr).await?;
    let addr = listener.local_addr()?;
    let url = format!("http://{addr}/");
    let app = build_router(vault_root, Some(allowed_hosts(addr, host)))?;

    let task = tokio::spawn(async move {
        if let Err(err) = axum::serve(listener, app).await {
            tracing::error!(error = %err, "explorer server exited with error");
        }
    });

    // Warm the embedding model off the request path so the first search
    // doesn't pay for the model load (or, on first ever use, its download).
    // A detached OS thread, not `spawn_blocking`: the tokio runtime joins its
    // blocking pool on shutdown, so a Ctrl-C during a first-run model
    // download would otherwise hang the CLI until the download finished.
    // `catch_unwind`: `embed` panics if the model can't be loaded (offline,
    // no cache) — that must not take the explorer down.
    std::thread::Builder::new()
        .name("okf-embedding-warmup".into())
        .spawn(|| {
            match std::panic::catch_unwind(|| crate::services::embedding_service::embed("warm-up")) {
                Ok(Ok(_)) => {}
                Ok(Err(err)) => {
                    tracing::debug!(error = %err, "embedding warm-up failed; search will retry lazily")
                }
                Err(_) => tracing::debug!("embedding warm-up panicked; search will retry lazily"),
            }
        })
        .ok();

    if open_browser && let Err(err) = open::that_detached(&url) {
        tracing::warn!(error = %err, url = %url, "could not open a browser; open the URL manually");
    }

    Ok(ExplorerHandle { addr, url, task })
}

async fn index_html() -> Response {
    // The page is versioned with the binary; never let a browser keep a
    // stale copy across upgrades (the vendored JS below is immutable, this
    // isn't).
    ([(header::CACHE_CONTROL, "no-cache")], Html(EXPLORER_HTML)).into_response()
}

fn static_js(body: &'static str) -> Response {
    (
        [
            (
                header::CONTENT_TYPE,
                "application/javascript; charset=utf-8",
            ),
            (header::CACHE_CONTROL, "public, max-age=31536000, immutable"),
        ],
        body,
    )
        .into_response()
}

async fn vendor_js() -> Response {
    static_js(FORCE_GRAPH_JS)
}

async fn mermaid_js() -> Response {
    static_js(MERMAID_JS)
}

fn error_json(status: StatusCode, err: impl std::fmt::Display) -> Response {
    (status, Json(json!({ "error": err.to_string() }))).into_response()
}

async fn api_info(State(state): State<AppState>) -> Response {
    let graph = state.graph.read().await.clone();
    let name = state
        .vault_root
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| state.vault_root.display().to_string());
    let mut kinds = std::collections::BTreeMap::<&str, usize>::new();
    for node in &graph.nodes {
        *kinds.entry(node.kind).or_default() += 1;
    }
    Json(json!({
        "vault_root": state.vault_root.display().to_string(),
        "vault_name": name,
        "version": env!("CARGO_PKG_VERSION"),
        "nodes": graph.nodes.len(),
        "links": graph.links.len(),
        "kinds": kinds,
        "unparsed_pages": graph.unparsed_pages,
    }))
    .into_response()
}

async fn api_graph(State(state): State<AppState>) -> Response {
    let graph = state.graph.read().await.clone();
    Json(serde_json::to_value(&*graph).unwrap_or(json!({}))).into_response()
}

async fn api_page(
    State(state): State<AppState>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Response {
    let graph = state.graph.read().await.clone();
    match render_page(&state.vault_root, &graph, &id) {
        Ok(view) => Json(serde_json::to_value(view).unwrap_or(json!({}))).into_response(),
        Err(err) if graph.node(&id).is_none() => error_json(StatusCode::NOT_FOUND, err),
        Err(err) => error_json(StatusCode::BAD_REQUEST, err),
    }
}

#[derive(Deserialize)]
struct SearchParams {
    q: String,
    #[serde(default)]
    limit: Option<usize>,
}

/// Maps a search hit's vault-relative path onto the graph node id the
/// frontend can highlight: `wiki/**/<slug>.md` → `<slug>`,
/// `raw/<stem>.md` → `raw:<stem>`.
fn node_id_for_path(path: &str) -> String {
    let stem = Path::new(path)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(path);
    if path.starts_with("raw/") {
        format!("raw:{stem}")
    } else {
        stem.to_string()
    }
}

async fn api_search(State(state): State<AppState>, Query(params): Query<SearchParams>) -> Response {
    let query = params.q.trim().to_string();
    if query.is_empty() {
        return Json(json!({ "query": "", "results": [] })).into_response();
    }
    let limit = params.limit.unwrap_or(20).clamp(1, 200);
    // Checked *before* searching: `hybrid_search` creates an empty index
    // directory on first use, so afterwards it always exists.
    let had_index = state.vault_root.join(".okf/index.db/tantivy").is_dir();
    let vault_root = state.vault_root.clone();
    let q = query.clone();
    let outcome =
        tokio::task::spawn_blocking(move || search::hybrid_search(&vault_root, &q, limit)).await;
    match outcome {
        Ok(Ok(results)) => {
            let graph = state.graph.read().await.clone();
            let results: Vec<serde_json::Value> = results
                .into_iter()
                .map(|r| {
                    let id = node_id_for_path(&r.path);
                    let node = graph.node(&id);
                    // BM25-only hits carry no snippet (only vector hits do);
                    // the page's own description is the next best summary.
                    let snippet = if r.snippet.is_empty() {
                        node.and_then(|n| n.description.clone()).unwrap_or_default()
                    } else {
                        r.snippet
                    };
                    json!({
                        "id": id,
                        "in_graph": node.is_some(),
                        "path": r.path,
                        "title": r.title,
                        "snippet": snippet,
                        "score": r.score,
                    })
                })
                .collect();
            // An empty result set against a vault that was never indexed is
            // far more likely "no index" than "no match" — say so.
            let hint = (results.is_empty() && !had_index).then_some(
                "no search index yet — run `okf-mcp reindex --embeddings` for this vault",
            );
            Json(json!({ "query": query, "results": results, "hint": hint })).into_response()
        }
        Ok(Err(err)) => error_json(
            StatusCode::SERVICE_UNAVAILABLE,
            format!(
                "search failed: {err} — is the index built? try `okf-mcp reindex --embeddings`"
            ),
        ),
        Err(err) => error_json(StatusCode::INTERNAL_SERVER_ERROR, err),
    }
}

async fn api_refresh(State(state): State<AppState>) -> Response {
    let vault_root = state.vault_root.clone();
    match tokio::task::spawn_blocking(move || build_graph(&vault_root)).await {
        Ok(Ok(graph)) => {
            let nodes = graph.nodes.len();
            let links = graph.links.len();
            *state.graph.write().await = Arc::new(graph);
            Json(json!({ "nodes": nodes, "links": links })).into_response()
        }
        Ok(Err(err)) => error_json(StatusCode::INTERNAL_SERVER_ERROR, err),
        Err(err) => error_json(StatusCode::INTERNAL_SERVER_ERROR, err),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn node_id_mapping() {
        assert_eq!(node_id_for_path("wiki/concepts/foo.md"), "foo");
        assert_eq!(node_id_for_path("wiki/entities/bar.md"), "bar");
        assert_eq!(node_id_for_path("raw/raw_abc.md"), "raw:raw_abc");
    }

    #[test]
    fn allowed_hosts_cover_bound_addr_localhost_and_user_host() {
        let addr: SocketAddr = "127.0.0.1:4321".parse().unwrap();
        let allowed = allowed_hosts(addr, "127.0.0.1");
        for h in ["127.0.0.1:4321", "localhost:4321", "[::1]:4321"] {
            assert!(
                allowed.contains(&h.to_string()),
                "{h} missing from {allowed:?}"
            );
        }
        assert!(!allowed.iter().any(|h| h.starts_with("evil")));
        let allowed = allowed_hosts(addr, "my-box");
        assert!(allowed.contains(&"my-box:4321".to_string()));
    }

    #[test]
    fn embedded_assets_are_present() {
        assert!(EXPLORER_HTML.contains("<title>"));
        assert!(EXPLORER_HTML.contains(VENDOR_JS_ROUTE));
        assert!(EXPLORER_HTML.contains(MERMAID_JS_ROUTE));
        assert!(FORCE_GRAPH_JS.contains("ForceGraph3D"));
        assert!(MERMAID_JS.contains("mermaid"));
    }
}
