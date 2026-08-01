//! CPU/heap profiling harness for `search::query::hybrid_search`. Two
//! modes selected by argv, matching `scripts/profile.sh`'s and
//! `scripts/profile-heap.sh`'s two different profiling techniques:
//!
//! - `profile_search <ready_file> <warm_seconds>` (samply/CPU mode): builds
//!   a small fixture vault, reindexes it (with embeddings), signals
//!   readiness by creating `ready_file`, then loops `hybrid_search` calls
//!   for `warm_seconds` wall-clock seconds — `samply record --pid` attaches
//!   mid-loop, so the captured profile is steady-state search cost only,
//!   not model/index initialization.
//! - `profile_search` (no args, dhat/heap mode): builds the same fixture
//!   vault, does `PROFILE_HEAP_WARMUPS` warmup calls, starts DHAT (only
//!   when built with `--features profiling`), then does
//!   `PROFILE_HEAP_ITERATIONS` measured calls.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use okf_mcp::search::{hybrid_search, reindex};

#[cfg(feature = "profiling")]
#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

const FIXTURE_CONCEPTS: &[(&str, &str, &str)] = &[
    (
        "resiliency-patterns",
        "Resiliency Patterns",
        "Circuit breakers, exponential backoff, and rate limiting protect a service from being \
         overwhelmed by too many requests in a short window. Related to retries and bulkheads.",
    ),
    (
        "distributed-tracing",
        "Distributed Tracing",
        "Correlates requests across service boundaries using trace and span identifiers, \
         letting an operator reconstruct a single request's path through a distributed system.",
    ),
    (
        "api-gateway",
        "API Gateway",
        "A single entry point that routes, authenticates, and rate-limits requests before they \
         reach backend services.",
    ),
    (
        "event-sourcing",
        "Event Sourcing",
        "Persists state as an ordered sequence of immutable events rather than as a single \
         mutable record, letting the current state be derived by replaying history.",
    ),
    (
        "cqrs",
        "CQRS",
        "Separates the read and write models of an application so each can be scaled and \
         optimized independently.",
    ),
];

fn fixture_vault() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("okf-mcp-profile-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join(".okf")).expect("create .okf marker");
    std::fs::create_dir_all(dir.join("wiki/concepts")).expect("create wiki/concepts");

    for (slug, title, body) in FIXTURE_CONCEPTS {
        let content = format!(
            "---\nokf_version: \"0.2\"\ntype: concept\nid: concept_{slug}\ntitle: \"{title}\"\ndescription: \"{title}.\"\n---\n\n# {title}\n\n{body}\n"
        );
        std::fs::write(dir.join(format!("wiki/concepts/{slug}.md")), content)
            .expect("write fixture concept");
    }
    dir
}

fn run_query(vault_root: &Path, query: &str) -> anyhow::Result<()> {
    hybrid_search(vault_root, query, 5)?;
    Ok(())
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let query = std::env::var("PROFILE_QUERY")
        .unwrap_or_else(|_| "how do we handle API rate limits".to_string());

    let vault_root = fixture_vault();
    reindex(&vault_root, true, None).expect("fixture reindex failed");

    // Warm the embedding model once before any measured work, in both
    // modes — loading it mid-measurement would otherwise dominate whatever
    // is being profiled. Failing loudly here (unlike the steady-state loop
    // below) surfaces a broken setup immediately instead of silently
    // profiling a loop that does nothing.
    run_query(&vault_root, &query).expect("warmup search failed");

    if args.len() >= 3 {
        let ready_file = &args[1];
        let warm_seconds: u64 = args[2].parse().unwrap_or(30);
        std::fs::write(ready_file, b"ready").expect("failed to write ready file");

        let deadline = Instant::now() + Duration::from_secs(warm_seconds);
        while Instant::now() < deadline {
            let _ = run_query(&vault_root, &query);
        }
    } else {
        let warmups: usize = std::env::var("PROFILE_HEAP_WARMUPS")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(1);
        let iterations: usize = std::env::var("PROFILE_HEAP_ITERATIONS")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(5)
            .max(1);

        for _ in 0..warmups {
            let _ = run_query(&vault_root, &query);
        }

        #[cfg(feature = "profiling")]
        let _dhat_profiler = dhat::Profiler::new_heap();

        for _ in 0..iterations {
            let _ = run_query(&vault_root, &query);
        }
    }

    let _ = std::fs::remove_dir_all(&vault_root);
}
