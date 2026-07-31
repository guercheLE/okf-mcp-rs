//! Dense-vector half of hybrid search: a fresh `rusqlite` + `sqlite-vec`
//! database at `.okf/index.db/vectors.sqlite`, structurally modeled on
//! `data::store`'s connection-caching/sqlite-vec-registration pattern but a
//! new implementation — that file's schema and compile-time
//! `include_bytes!` embedding don't apply to mutable, per-vault runtime
//! data.
//!
//! Content is embedded **per markdown section** (split on `##` headings,
//! whole-file for short documents with none), not one vector per file: long
//! Firecrawl-scraped raw pages and wiki concept pages both benefit from
//! section-level precision, and it's what lets `hybrid_search` point at a
//! specific matching snippet instead of just "this file, somewhere."

use std::path::Path;
use std::sync::Once;

use rusqlite::{Connection, OptionalExtension, params};

use crate::core::vault_resolver::sandbox_path;
use crate::services::embedding_service::embed;

/// Matches `services::embedding_service`'s `all-mpnet-base-v2` output.
pub const EMBEDDING_DIM: usize = 768;

static REGISTER_VEC_EXTENSION: Once = Once::new();

fn register_vec_extension() {
    REGISTER_VEC_EXTENSION.call_once(|| unsafe {
        #[allow(clippy::missing_transmute_annotations)]
        rusqlite::ffi::sqlite3_auto_extension(Some(std::mem::transmute(
            sqlite_vec::sqlite3_vec_init as *const (),
        )));
    });
}

/// Opens (creating on first use) the vault's vector database.
pub fn open_or_create(vault_root: &Path) -> anyhow::Result<Connection> {
    register_vec_extension();
    let path = sandbox_path(vault_root, ".okf/index.db/vectors.sqlite")?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let conn = Connection::open(&path)?;
    init_schema(&conn)?;
    Ok(conn)
}

fn init_schema(conn: &Connection) -> anyhow::Result<()> {
    conn.execute(
        "CREATE TABLE IF NOT EXISTS documents (
            path TEXT PRIMARY KEY,
            doc_type TEXT NOT NULL,
            title TEXT NOT NULL,
            content_hash TEXT NOT NULL,
            updated_at TEXT NOT NULL
        )",
        [],
    )?;
    conn.execute(
        "CREATE TABLE IF NOT EXISTS sections (
            section_key TEXT PRIMARY KEY,
            path TEXT NOT NULL,
            heading TEXT,
            snippet TEXT NOT NULL
        )",
        [],
    )?;
    conn.execute(
        &format!(
            "CREATE VIRTUAL TABLE IF NOT EXISTS document_vectors USING vec0(
                section_key TEXT PRIMARY KEY,
                embedding FLOAT[{EMBEDDING_DIM}]
            )"
        ),
        [],
    )?;
    Ok(())
}

/// Splits `body` on `## `-prefixed headings. A document with no such
/// headings (the common case for short raw sources or brief concept pages)
/// yields exactly one chunk covering the whole body.
pub fn chunk_by_heading(body: &str) -> Vec<(Option<String>, String)> {
    let mut chunks = Vec::new();
    let mut heading: Option<String> = None;
    let mut text = String::new();

    for line in body.lines() {
        if let Some(rest) = line.strip_prefix("## ") {
            if !text.trim().is_empty() {
                chunks.push((heading.take(), text.trim().to_string()));
            }
            heading = Some(rest.trim().to_string());
            text.clear();
        } else {
            text.push_str(line);
            text.push('\n');
        }
    }
    if !text.trim().is_empty() {
        chunks.push((heading, text.trim().to_string()));
    }
    if chunks.is_empty() {
        chunks.push((None, String::new()));
    }
    chunks
}

/// Whether `path`'s stored `content_hash` differs from `content_hash` (or
/// the document isn't indexed yet at all) — the caller's cue to actually
/// recompute embeddings rather than skip an unchanged document.
pub fn needs_reembedding(conn: &Connection, path: &str, content_hash: &str) -> anyhow::Result<bool> {
    let existing: Option<String> = conn
        .query_row(
            "SELECT content_hash FROM documents WHERE path = ?1",
            [path],
            |row| row.get(0),
        )
        .optional()?;
    Ok(existing.as_deref() != Some(content_hash))
}

pub fn upsert_document_metadata(
    conn: &Connection,
    path: &str,
    doc_type: &str,
    title: &str,
    content_hash: &str,
    updated_at: &str,
) -> anyhow::Result<()> {
    conn.execute(
        "INSERT INTO documents (path, doc_type, title, content_hash, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(path) DO UPDATE SET
             doc_type = excluded.doc_type,
             title = excluded.title,
             content_hash = excluded.content_hash,
             updated_at = excluded.updated_at",
        params![path, doc_type, title, content_hash, updated_at],
    )?;
    Ok(())
}

/// Replaces every section/vector for `path` with `sections` (each already
/// embedded by the caller — this function does no model inference itself,
/// so tests can exercise it with synthetic vectors without the real
/// `embedding_service`).
pub fn replace_sections(
    conn: &Connection,
    path: &str,
    sections: &[(String, Option<String>, String, Vec<f32>)],
) -> anyhow::Result<()> {
    conn.execute(
        "DELETE FROM document_vectors WHERE section_key IN (SELECT section_key FROM sections WHERE path = ?1)",
        [path],
    )?;
    conn.execute("DELETE FROM sections WHERE path = ?1", [path])?;

    for (section_key, heading, snippet, embedding) in sections {
        conn.execute(
            "INSERT INTO sections (section_key, path, heading, snippet) VALUES (?1, ?2, ?3, ?4)",
            params![section_key, path, heading, snippet],
        )?;
        let blob = embedding_to_blob(embedding);
        conn.execute(
            "INSERT INTO document_vectors (section_key, embedding) VALUES (?1, ?2)",
            params![section_key, blob],
        )?;
    }
    Ok(())
}

pub fn remove_document(conn: &Connection, path: &str) -> anyhow::Result<()> {
    conn.execute(
        "DELETE FROM document_vectors WHERE section_key IN (SELECT section_key FROM sections WHERE path = ?1)",
        [path],
    )?;
    conn.execute("DELETE FROM sections WHERE path = ?1", [path])?;
    conn.execute("DELETE FROM documents WHERE path = ?1", [path])?;
    Ok(())
}

fn embedding_to_blob(embedding: &[f32]) -> Vec<u8> {
    embedding.iter().flat_map(|value| value.to_le_bytes()).collect()
}

#[derive(Debug, Clone)]
pub struct VectorHit {
    pub path: String,
    pub section_key: String,
    pub heading: Option<String>,
    pub snippet: String,
    /// L2 distance (smaller = closer) — embeddings are normalized, so
    /// distance ordering matches cosine-similarity ordering even though
    /// this isn't literally a cosine score.
    pub distance: f64,
}

pub fn search_vectors(
    conn: &Connection,
    query_embedding: &[f32],
    limit: usize,
) -> anyhow::Result<Vec<VectorHit>> {
    let blob = embedding_to_blob(query_embedding);
    let mut stmt = conn.prepare(
        "SELECT v.section_key, s.path, s.heading, s.snippet, v.distance
         FROM document_vectors v
         JOIN sections s ON s.section_key = v.section_key
         WHERE v.embedding MATCH ?1 AND k = ?2
         ORDER BY v.distance",
    )?;
    let rows = stmt.query_map(params![blob, limit], |row| {
        Ok(VectorHit {
            section_key: row.get(0)?,
            path: row.get(1)?,
            heading: row.get(2)?,
            snippet: row.get(3)?,
            distance: row.get(4)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// Embeds and stores every section of `path`'s `body` — the one function
/// here that actually calls the (slow, model-backed) `embedding_service`;
/// kept separate from `replace_sections` so tests can exercise storage and
/// search with synthetic vectors instead.
pub fn embed_and_store_sections(conn: &Connection, path: &str, body: &str) -> anyhow::Result<()> {
    let chunks = chunk_by_heading(body);
    let mut sections = Vec::with_capacity(chunks.len());
    for (index, (heading, text)) in chunks.into_iter().enumerate() {
        let embedding = embed(&text)?;
        let snippet: String = text.chars().take(280).collect();
        sections.push((format!("{path}#{index}"), heading, snippet, embedding));
    }
    replace_sections(conn, path, &sections)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn synthetic_vector(seed: f32) -> Vec<f32> {
        let mut v = vec![0.0f32; EMBEDDING_DIM];
        v[0] = seed;
        v[1] = 1.0 - seed.abs().min(1.0);
        v
    }

    #[test]
    fn chunk_by_heading_splits_on_h2_and_keeps_leading_content_as_its_own_chunk() {
        let body = "Intro text.\n\n## First\nBody one.\n\n## Second\nBody two.\n";
        let chunks = chunk_by_heading(body);
        assert_eq!(chunks.len(), 3);
        assert_eq!(chunks[0].0, None);
        assert!(chunks[0].1.contains("Intro text."));
        assert_eq!(chunks[1].0.as_deref(), Some("First"));
        assert!(chunks[1].1.contains("Body one."));
        assert_eq!(chunks[2].0.as_deref(), Some("Second"));
    }

    #[test]
    fn chunk_by_heading_with_no_headings_yields_one_whole_file_chunk() {
        let chunks = chunk_by_heading("Just a short paragraph, no headings.");
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].0, None);
    }

    #[test]
    fn needs_reembedding_is_true_for_a_never_seen_path() {
        let vault = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(vault.path().join(".okf")).unwrap();
        let conn = open_or_create(vault.path()).unwrap();
        assert!(needs_reembedding(&conn, "a.md", "sha256:aaa").unwrap());
    }

    #[test]
    fn needs_reembedding_is_false_once_the_hash_matches_the_stored_one() {
        let vault = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(vault.path().join(".okf")).unwrap();
        let conn = open_or_create(vault.path()).unwrap();
        upsert_document_metadata(&conn, "a.md", "concept", "A", "sha256:aaa", "t0").unwrap();

        assert!(!needs_reembedding(&conn, "a.md", "sha256:aaa").unwrap());
        assert!(needs_reembedding(&conn, "a.md", "sha256:bbb").unwrap());
    }

    #[test]
    fn replace_sections_then_search_finds_the_nearest_section() {
        let vault = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(vault.path().join(".okf")).unwrap();
        let conn = open_or_create(vault.path()).unwrap();

        upsert_document_metadata(&conn, "close.md", "concept", "Close", "sha256:1", "t0").unwrap();
        upsert_document_metadata(&conn, "far.md", "concept", "Far", "sha256:2", "t0").unwrap();

        replace_sections(
            &conn,
            "close.md",
            &[(
                "close.md#0".to_string(),
                None,
                "close snippet".to_string(),
                synthetic_vector(0.9),
            )],
        )
        .unwrap();
        replace_sections(
            &conn,
            "far.md",
            &[(
                "far.md#0".to_string(),
                None,
                "far snippet".to_string(),
                synthetic_vector(-0.9),
            )],
        )
        .unwrap();

        let hits = search_vectors(&conn, &synthetic_vector(0.9), 5).unwrap();
        assert!(!hits.is_empty());
        assert_eq!(hits[0].path, "close.md");
    }

    #[test]
    fn replace_sections_removes_the_previous_sections_for_that_path() {
        let vault = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(vault.path().join(".okf")).unwrap();
        let conn = open_or_create(vault.path()).unwrap();
        upsert_document_metadata(&conn, "a.md", "concept", "A", "sha256:1", "t0").unwrap();

        replace_sections(
            &conn,
            "a.md",
            &[
                ("a.md#0".to_string(), None, "one".to_string(), synthetic_vector(0.1)),
                ("a.md#1".to_string(), None, "two".to_string(), synthetic_vector(0.2)),
            ],
        )
        .unwrap();
        replace_sections(
            &conn,
            "a.md",
            &[("a.md#0".to_string(), None, "only".to_string(), synthetic_vector(0.1))],
        )
        .unwrap();

        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM sections WHERE path = 'a.md'", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn remove_document_deletes_metadata_sections_and_vectors() {
        let vault = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(vault.path().join(".okf")).unwrap();
        let conn = open_or_create(vault.path()).unwrap();
        upsert_document_metadata(&conn, "a.md", "concept", "A", "sha256:1", "t0").unwrap();
        replace_sections(
            &conn,
            "a.md",
            &[("a.md#0".to_string(), None, "snippet".to_string(), synthetic_vector(0.1))],
        )
        .unwrap();

        remove_document(&conn, "a.md").unwrap();

        let doc_count: i64 = conn.query_row("SELECT COUNT(*) FROM documents", [], |row| row.get(0)).unwrap();
        let section_count: i64 = conn.query_row("SELECT COUNT(*) FROM sections", [], |row| row.get(0)).unwrap();
        assert_eq!(doc_count, 0);
        assert_eq!(section_count, 0);
    }

    /// The one test in this module that exercises the real embedding
    /// model — everything else above uses synthetic vectors so the default
    /// `cargo test` run stays fast. `.fastembed_cache/` is already
    /// populated in this repo's checkout, so this doesn't hit the network.
    #[test]
    fn embed_and_store_sections_round_trips_through_the_real_model() {
        let vault = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(vault.path().join(".okf")).unwrap();
        let conn = open_or_create(vault.path()).unwrap();
        upsert_document_metadata(&conn, "a.md", "concept", "A", "sha256:1", "t0").unwrap();

        embed_and_store_sections(&conn, "a.md", "How do we handle API rate limits?").unwrap();

        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM document_vectors", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 1);

        let query_embedding = embed("rate limiting strategy").unwrap();
        let hits = search_vectors(&conn, &query_embedding, 5).unwrap();
        assert_eq!(hits[0].path, "a.md");
    }
}
