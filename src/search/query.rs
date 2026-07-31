//! `search::reindex`/`search::hybrid_search` — the two entry points the CLI
//! and MCP tool layer actually call. Combines `search::index` (BM25) and
//! `search::vectors` (dense) via `search::rrf`.

use std::path::Path;

use serde::Serialize;

use crate::ingest::frontmatter::hash_content;
use crate::manifest;
use crate::services::embedding_service::embed;
use crate::validator::rules::markdown_files_in;

use super::index::{self, TextDocument};
use super::rrf::rrf_merge;
use super::vectors;

struct CollectedDocument {
    path: String,
    title: String,
    body: String,
    doc_type: &'static str,
}

/// Shared with `compiler::driver`, which also needs "the body of a raw
/// blob, frontmatter stripped" when building the compile prompt.
pub(crate) fn raw_title_and_body(content: &str) -> (String, String) {
    let body = match content
        .strip_prefix("---\n")
        .and_then(|rest| rest.find("\n---\n").map(|end| &rest[end + "\n---\n".len()..]))
    {
        Some(body) => body.to_string(),
        None => content.to_string(),
    };
    let title = body
        .lines()
        .find_map(|line| line.strip_prefix("# "))
        .unwrap_or_default()
        .trim()
        .to_string();
    (title, body)
}

/// Every document `reindex`/`hybrid_search` should consider: active raw
/// sources (per the manifest — tombstoned/superseded raw is excluded, per
/// the design's "source of truth = ACTIVE entries only" rule) plus every
/// compiled wiki concept page.
fn collect_documents(vault_root: &Path) -> anyhow::Result<Vec<CollectedDocument>> {
    let mut documents = Vec::new();

    let manifest = manifest::store::load(vault_root)?;
    let active_raw_ids: std::collections::HashSet<String> = manifest
        .active_entries()
        .map(|(_, version)| version.raw_id.clone())
        .collect();

    let raw_dir = vault_root.join("raw");
    if raw_dir.is_dir() {
        for entry in std::fs::read_dir(&raw_dir)? {
            let path = entry?.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("md") {
                continue;
            }
            let stem = path
                .file_stem()
                .and_then(|stem| stem.to_str())
                .unwrap_or_default();
            if !active_raw_ids.contains(stem) {
                continue;
            }
            let content = std::fs::read_to_string(&path)?;
            let (title, body) = raw_title_and_body(&content);
            documents.push(CollectedDocument {
                path: format!("raw/{stem}.md"),
                title: if title.is_empty() { stem.to_string() } else { title },
                body,
                doc_type: "raw",
            });
        }
    }

    for path in markdown_files_in(&vault_root.join("wiki/concepts"))? {
        let content = std::fs::read_to_string(&path)?;
        let Ok(parsed) = crate::validator::frontmatter::parse_wiki_page(&content) else {
            // A page that fails `okf-mcp lint`'s own frontmatter check is
            // skipped here rather than failing the whole reindex — lint is
            // the tool responsible for surfacing that as an error.
            continue;
        };
        let relative = path
            .strip_prefix(vault_root)
            .unwrap_or(&path)
            .to_string_lossy()
            .replace('\\', "/");
        documents.push(CollectedDocument {
            path: relative,
            title: parsed.frontmatter.title,
            body: parsed.body,
            doc_type: "concept",
        });
    }

    Ok(documents)
}

#[derive(Debug, Default, Serialize)]
pub struct ReindexReport {
    pub text_documents_indexed: usize,
    pub vectors_embedded: usize,
    pub vectors_skipped_unchanged: usize,
}

/// Rebuilds the BM25 text index unconditionally (cheap enough to always do
/// in full); embeddings are only (re)computed for documents whose content
/// hash changed since the last `--embeddings` reindex, unless `embeddings`
/// itself is false, in which case the vector half isn't touched at all.
pub fn reindex(vault_root: &Path, embeddings: bool) -> anyhow::Result<ReindexReport> {
    let documents = collect_documents(vault_root)?;

    let (tantivy_index, schema) = index::open_or_create(vault_root)?;
    let text_documents: Vec<TextDocument<'_>> = documents
        .iter()
        .map(|document| TextDocument {
            path: &document.path,
            title: &document.title,
            body: &document.body,
            doc_type: document.doc_type,
        })
        .collect();
    let text_documents_indexed = index::write_documents(&tantivy_index, &schema, &text_documents)?;

    let mut report = ReindexReport {
        text_documents_indexed,
        ..Default::default()
    };

    if embeddings {
        let conn = vectors::open_or_create(vault_root)?;
        for document in &documents {
            let content_hash = hash_content(&document.body);
            // Must check *before* upserting metadata below — that upsert
            // overwrites the stored hash with `content_hash`, so checking
            // afterward would always see them as equal and never re-embed
            // anything.
            let needs_reembedding =
                vectors::needs_reembedding(&conn, &document.path, &content_hash)?;

            vectors::upsert_document_metadata(
                &conn,
                &document.path,
                document.doc_type,
                &document.title,
                &content_hash,
                &chrono::Utc::now().to_rfc3339(),
            )?;

            if needs_reembedding {
                vectors::embed_and_store_sections(&conn, &document.path, &document.body)?;
                report.vectors_embedded += 1;
            } else {
                report.vectors_skipped_unchanged += 1;
            }
        }
    }

    Ok(report)
}

#[derive(Debug, Clone, Serialize)]
pub struct SearchResult {
    pub path: String,
    pub title: String,
    pub snippet: String,
    pub score: f64,
}

/// Search ingested raw sources and compiled wiki concepts. Internally a
/// hybrid of BM25 keyword matching and dense-vector similarity, merged via
/// Reciprocal Rank Fusion — deliberately not described to callers (CLI help
/// text, MCP tool description) as "semantic search": these are fixed,
/// well-defined tools, not a discovery layer over an unknown surface.
pub fn hybrid_search(vault_root: &Path, query: &str, limit: usize) -> anyhow::Result<Vec<SearchResult>> {
    let fetch_limit = (limit * 2).max(limit);

    let (tantivy_index, schema) = index::open_or_create(vault_root)?;
    let text_hits = index::search_text(&tantivy_index, &schema, query, fetch_limit)?;
    let bm25_paths: Vec<String> = text_hits.into_iter().map(|hit| hit.path).collect();

    let vectors_conn = vectors::open_or_create(vault_root)?;
    let query_embedding = embed(query)?;
    let vector_hits = vectors::search_vectors(&vectors_conn, &query_embedding, fetch_limit)?;

    // Multiple sections of the same file can each appear in the vector
    // hit list; collapse to that file's single best (lowest-distance) hit
    // before ranking, and remember its snippet for the final result.
    let mut best_vector_hit_by_path: std::collections::HashMap<String, super::vectors::VectorHit> =
        std::collections::HashMap::new();
    for hit in vector_hits {
        best_vector_hit_by_path
            .entry(hit.path.clone())
            .and_modify(|existing| {
                if hit.distance < existing.distance {
                    *existing = hit.clone();
                }
            })
            .or_insert(hit);
    }
    let mut vector_paths: Vec<(String, f64)> = best_vector_hit_by_path
        .iter()
        .map(|(path, hit)| (path.clone(), hit.distance))
        .collect();
    vector_paths.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
    let vector_paths: Vec<String> = vector_paths.into_iter().map(|(path, _)| path).collect();

    let merged = rrf_merge(&bm25_paths, &vector_paths, 60.0);

    let mut results = Vec::with_capacity(limit.min(merged.len()));
    for (path, score) in merged.into_iter().take(limit) {
        let title = document_title(vault_root, &path).unwrap_or_default();
        let snippet = best_vector_hit_by_path
            .get(&path)
            .map(|hit| hit.snippet.clone())
            .unwrap_or_default();
        results.push(SearchResult {
            path,
            title,
            snippet,
            score,
        });
    }
    Ok(results)
}

fn document_title(vault_root: &Path, relative_path: &str) -> Option<String> {
    let full_path = vault_root.join(relative_path);
    let content = std::fs::read_to_string(&full_path).ok()?;
    if relative_path.starts_with("raw/") {
        let (title, _) = raw_title_and_body(&content);
        (!title.is_empty()).then_some(title)
    } else {
        crate::validator::frontmatter::parse_wiki_page(&content)
            .ok()
            .map(|parsed| parsed.frontmatter.title)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_wiki_page(vault_root: &Path, slug: &str, title: &str, body: &str) {
        let dir = vault_root.join("wiki/concepts");
        std::fs::create_dir_all(&dir).unwrap();
        let content = format!(
            "---\nokf_version: \"0.2\"\ntype: concept\nid: concept_{slug}\ntitle: \"{title}\"\n---\n\n{body}\n"
        );
        std::fs::write(dir.join(format!("{slug}.md")), content).unwrap();
    }

    #[test]
    fn reindex_without_embeddings_only_touches_the_text_index() {
        let vault = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(vault.path().join(".okf")).unwrap();
        write_wiki_page(vault.path(), "a", "Alpha", "Content about alpha.");

        let report = reindex(vault.path(), false).unwrap();
        assert_eq!(report.text_documents_indexed, 1);
        assert_eq!(report.vectors_embedded, 0);
        assert!(!vault.path().join(".okf/index.db/vectors.sqlite").exists());
    }

    #[test]
    fn reindex_with_embeddings_skips_unchanged_documents_on_a_second_run() {
        let vault = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(vault.path().join(".okf")).unwrap();
        write_wiki_page(vault.path(), "a", "Alpha", "Content about alpha.");

        let first = reindex(vault.path(), true).unwrap();
        assert_eq!(first.vectors_embedded, 1);
        assert_eq!(first.vectors_skipped_unchanged, 0);

        let second = reindex(vault.path(), true).unwrap();
        assert_eq!(second.vectors_embedded, 0);
        assert_eq!(second.vectors_skipped_unchanged, 1);
    }

    #[test]
    fn hybrid_search_finds_a_keyword_match_via_bm25_alone() {
        let vault = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(vault.path().join(".okf")).unwrap();
        write_wiki_page(
            vault.path(),
            "resiliency",
            "Resiliency Patterns",
            "Covers circuit breakers and rate limiting for API calls.",
        );
        write_wiki_page(vault.path(), "colors", "Colors", "A page about paint colors.");

        reindex(vault.path(), false).unwrap();

        let results = hybrid_search(vault.path(), "rate limiting", 5).unwrap();
        assert!(!results.is_empty());
        assert_eq!(results[0].path, "wiki/concepts/resiliency.md");
        assert_eq!(results[0].title, "Resiliency Patterns");
    }

    #[test]
    fn hybrid_search_over_an_empty_vault_returns_no_results_without_erroring() {
        let vault = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(vault.path().join(".okf")).unwrap();
        reindex(vault.path(), false).unwrap();

        let results = hybrid_search(vault.path(), "anything", 5).unwrap();
        assert!(results.is_empty());
    }

    /// The design doc's own canonical example: a query with no literal
    /// keyword overlap should still surface the semantically related page,
    /// via the vector half of the hybrid merge. Uses the real embedding
    /// model (already cached locally — see `search::vectors`' own note on
    /// this), so it's slower than the BM25-only test above but exercises
    /// genuine end-to-end behavior rather than synthetic vectors.
    #[test]
    fn hybrid_search_finds_a_semantically_related_page_with_no_keyword_overlap() {
        let vault = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(vault.path().join(".okf")).unwrap();
        write_wiki_page(
            vault.path(),
            "resiliency-patterns",
            "Resiliency Patterns",
            "Circuit breakers, exponential backoff, and throttling protect a service from being overwhelmed by too many requests in a short window.",
        );
        write_wiki_page(
            vault.path(),
            "colors",
            "Colors",
            "A page about paint colors and interior design choices.",
        );

        reindex(vault.path(), true).unwrap();

        let results = hybrid_search(vault.path(), "How do we handle API rate limits?", 5).unwrap();
        assert!(!results.is_empty());
        assert_eq!(results[0].path, "wiki/concepts/resiliency-patterns.md");
    }
}
