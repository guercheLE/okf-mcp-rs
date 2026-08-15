//! In-process tests for `okf-mcp explore`'s HTTP API (`explorer::server`):
//! drives the axum router with `tower::ServiceExt::oneshot` against a temp
//! vault, no port binding or browser involved.

use std::path::Path;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use http_body_util::BodyExt;
use okf_mcp::explorer::server::build_router;
use tower::ServiceExt;

fn write(root: &Path, relative: &str, content: &str) {
    let path = root.join(relative);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, content).unwrap();
}

fn make_vault() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::create_dir_all(root.join(".okf")).unwrap();
    write(
        root,
        "raw/raw_abc.md",
        "---\ntype: raw_source\nsource_url: https://example.com/rate-limits\n---\n\n# Rate Limits\n\nHow API rate limits and retries work.\n",
    );
    write(
        root,
        "wiki/concepts/rate-limiting.md",
        "---\ntype: Concept\ntitle: Rate Limiting\ndescription: Throttling API calls.\ntags:\n  - api\n  - reliability\nsources:\n  - resource: /raw/raw_abc.md\ngenerated:\n  by: okf-mcp-compiler\n  at: 2026-08-08T00:00:00Z\n---\n\n# Rate Limiting\n\nPairs with [[retry-with-backoff]] and [[circuit-breaker]].\n",
    );
    write(
        root,
        "wiki/concepts/retry-with-backoff.md",
        "---\ntype: Concept\ntitle: Retry With Backoff\ntags:\n  - reliability\n---\n\nUsed after [[rate-limiting]] rejects a call.\n",
    );
    dir
}

async fn get(router: axum::Router, uri: &str) -> (StatusCode, serde_json::Value, String) {
    let response = router
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let content_type = response
        .headers()
        .get(header::CONTENT_TYPE)
        .map(|v| v.to_str().unwrap().to_string())
        .unwrap_or_default();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let json = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, json, content_type)
}

#[tokio::test]
async fn index_and_vendor_assets_are_served() {
    let vault = make_vault();
    let router = build_router(vault.path(), None).unwrap();

    let response = router
        .clone()
        .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert!(
        response.headers()[header::CONTENT_TYPE]
            .to_str()
            .unwrap()
            .starts_with("text/html")
    );
    let html = response.into_body().collect().await.unwrap().to_bytes();
    let html = String::from_utf8_lossy(&html);
    assert!(html.contains("OKF Explorer"));
    assert!(html.contains("/vendor/3d-force-graph.min.js"));

    let (status, _, content_type) = get(router, "/vendor/3d-force-graph.min.js").await;
    assert_eq!(status, StatusCode::OK);
    assert!(content_type.starts_with("application/javascript"));
}

#[tokio::test]
async fn info_and_graph_reflect_the_vault() {
    let vault = make_vault();
    let router = build_router(vault.path(), None).unwrap();

    let (status, info, _) = get(router.clone(), "/api/info").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(info["kinds"]["concept"], 2);
    assert_eq!(info["kinds"]["tag"], 2);
    assert_eq!(info["kinds"]["raw"], 1);
    assert_eq!(info["kinds"]["missing"], 1);
    assert_eq!(info["unparsed_pages"], 0);

    let (status, graph, _) = get(router, "/api/graph").await;
    assert_eq!(status, StatusCode::OK);
    let nodes = graph["nodes"].as_array().unwrap();
    let rate = nodes.iter().find(|n| n["id"] == "rate-limiting").unwrap();
    assert_eq!(rate["kind"], "concept");
    assert_eq!(rate["type"], "Concept");
    assert_eq!(rate["in_degree"], 1);
    assert_eq!(rate["out_degree"], 5); // retry, circuit-breaker, #api, #reliability, raw
    let reliability = nodes.iter().find(|n| n["id"] == "#reliability").unwrap();
    assert_eq!(reliability["in_degree"], 2);
    assert_eq!(reliability["hub_rank"], 1);
    let links = graph["links"].as_array().unwrap();
    assert!(links.iter().any(|l| l["source"] == "rate-limiting"
        && l["target"] == "raw:raw_abc"
        && l["kind"] == "source"));
}

#[tokio::test]
async fn page_renders_frontmatter_html_and_backlinks() {
    let vault = make_vault();
    let router = build_router(vault.path(), None).unwrap();

    let (status, page, _) = get(router.clone(), "/api/page/rate-limiting").await;
    assert_eq!(status, StatusCode::OK, "{page}");
    assert_eq!(page["title"], "Rate Limiting");
    assert_eq!(page["path"], "wiki/concepts/rate-limiting.md");
    assert_eq!(page["frontmatter"]["generated"]["by"], "okf-mcp-compiler");
    let html = page["html"].as_str().unwrap();
    assert!(html.contains("<h1>Rate Limiting</h1>"));
    assert!(html.contains(r##"class="wikilink" href="#/note/retry-with-backoff""##));
    assert!(html.contains(r##"class="wikilink missing" href="#/note/circuit-breaker""##));
    assert_eq!(page["backlinks"][0]["id"], "retry-with-backoff");

    let (status, raw, _) = get(router.clone(), "/api/page/raw:raw_abc").await;
    assert_eq!(status, StatusCode::OK, "{raw}");
    assert_eq!(raw["kind"], "raw");
    assert_eq!(
        raw["frontmatter"]["source_url"],
        "https://example.com/rate-limits"
    );
    assert_eq!(raw["backlinks"][0]["id"], "rate-limiting");

    let (status, err, _) = get(router.clone(), "/api/page/does-not-exist").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(err["error"].as_str().unwrap().contains("does-not-exist"));

    // A missing-link node exists in the graph but has no file to render.
    let (status, _, _) = get(router.clone(), "/api/page/circuit-breaker").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // Path traversal through the wildcard segment is not a node → 404, never a file read.
    let (status, _, _) = get(router, "/api/page/../../.okf/manifest.json").await;
    assert_ne!(status, StatusCode::OK);
}

#[tokio::test]
async fn refresh_picks_up_new_pages() {
    let vault = make_vault();
    let router = build_router(vault.path(), None).unwrap();

    let (_, before, _) = get(router.clone(), "/api/info").await;
    assert_eq!(before["nodes"], 6);

    write(
        vault.path(),
        "wiki/concepts/circuit-breaker.md",
        "---\ntype: Concept\ntitle: Circuit Breaker\n---\n\nSee [[rate-limiting]].\n",
    );
    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/refresh")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let (_, after, _) = get(router.clone(), "/api/info").await;
    assert_eq!(after["nodes"], 6); // the missing node became a real page: same count
    assert_eq!(after["kinds"]["missing"], serde_json::Value::Null);
    assert_eq!(after["kinds"]["concept"], 3);
}

#[tokio::test]
async fn search_uses_the_vault_index() {
    let vault = make_vault();
    let router = build_router(vault.path(), None).unwrap();

    // Empty query short-circuits without touching the index.
    let (status, empty, _) = get(router.clone(), "/api/search?q=%20").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(empty["results"].as_array().unwrap().len(), 0);

    // Build the same index `okf-mcp reindex --embeddings` builds, then hit it.
    okf_mcp::search::reindex(vault.path(), true, None).unwrap();
    let (status, hits, _) = get(router, "/api/search?q=rate%20limits&limit=5").await;
    assert_eq!(status, StatusCode::OK, "{hits}");
    let results = hits["results"].as_array().unwrap();
    assert!(!results.is_empty());
    let ids: Vec<&str> = results.iter().map(|r| r["id"].as_str().unwrap()).collect();
    assert!(
        ids.contains(&"rate-limiting") || ids.contains(&"raw:raw_abc"),
        "expected a rate-limit hit, got {ids:?}"
    );
    assert!(results.iter().all(|r| r["in_graph"] == true));
}

#[tokio::test]
async fn host_header_allowlist_blocks_dns_rebinding() {
    let vault = make_vault();
    let router = build_router(
        vault.path(),
        Some(vec![
            "127.0.0.1:4321".to_string(),
            "localhost:4321".to_string(),
        ]),
    )
    .unwrap();

    let with_host = |host: &str| {
        Request::builder()
            .uri("/api/info")
            .header(header::HOST, host)
            .body(Body::empty())
            .unwrap()
    };
    let ok = router
        .clone()
        .oneshot(with_host("localhost:4321"))
        .await
        .unwrap();
    assert_eq!(ok.status(), StatusCode::OK);
    let ok = router
        .clone()
        .oneshot(with_host("127.0.0.1:4321"))
        .await
        .unwrap();
    assert_eq!(ok.status(), StatusCode::OK);

    let rebound = router
        .clone()
        .oneshot(with_host("evil.example:4321"))
        .await
        .unwrap();
    assert_eq!(rebound.status(), StatusCode::FORBIDDEN);
    let no_host = router
        .oneshot(
            Request::builder()
                .uri("/api/info")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(no_host.status(), StatusCode::FORBIDDEN);
}
