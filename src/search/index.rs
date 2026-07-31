//! BM25 full-text index over vault content, at `.okf/index.db/tantivy/`.
//! `okf-mcp reindex` does a full rebuild each run (simple and cheap enough
//! at the vault sizes this targets) — the expensive part, dense-vector
//! embedding, is handled separately by `search::vectors` with its own
//! content-hash-based skip logic.

use std::path::Path;

use tantivy::collector::TopDocs;
use tantivy::query::QueryParser;
use tantivy::schema::{Field, STORED, Schema, STRING, TEXT, Value};
use tantivy::{Index, IndexWriter, ReloadPolicy, TantivyDocument, doc};

use crate::core::vault_resolver::sandbox_path;

pub struct SearchSchema {
    pub schema: Schema,
    pub path: Field,
    pub title: Field,
    pub body: Field,
    pub doc_type: Field,
}

fn build_schema() -> SearchSchema {
    let mut builder = Schema::builder();
    let path = builder.add_text_field("path", STRING | STORED);
    let title = builder.add_text_field("title", TEXT | STORED);
    let body = builder.add_text_field("body", TEXT | STORED);
    let doc_type = builder.add_text_field("doc_type", STRING | STORED);
    SearchSchema {
        schema: builder.build(),
        path,
        title,
        body,
        doc_type,
    }
}

/// Opens the vault's Tantivy index, creating it (and the schema) on first
/// use. The index directory is gitignored and fully rebuilt by `reindex` —
/// nothing here assumes it survives being deleted.
pub fn open_or_create(vault_root: &Path) -> anyhow::Result<(Index, SearchSchema)> {
    let dir = sandbox_path(vault_root, ".okf/index.db/tantivy")?;
    std::fs::create_dir_all(&dir)?;
    let schema = build_schema();

    let index = match Index::open_in_dir(&dir) {
        Ok(index) => index,
        Err(_) => Index::create_in_dir(&dir, schema.schema.clone())?,
    };
    Ok((index, schema))
}

pub struct TextDocument<'a> {
    pub path: &'a str,
    pub title: &'a str,
    pub body: &'a str,
    pub doc_type: &'a str,
}

/// Full rebuild: clears every existing document, then indexes `documents`.
pub fn write_documents(
    index: &Index,
    schema: &SearchSchema,
    documents: &[TextDocument<'_>],
) -> anyhow::Result<usize> {
    let mut writer: IndexWriter = index.writer(50_000_000)?;
    writer.delete_all_documents()?;

    for document in documents {
        writer.add_document(doc!(
            schema.path => document.path,
            schema.title => document.title,
            schema.body => document.body,
            schema.doc_type => document.doc_type,
        ))?;
    }
    writer.commit()?;
    Ok(documents.len())
}

#[derive(Debug, Clone)]
pub struct TextHit {
    pub path: String,
    pub score: f32,
}

pub fn search_text(
    index: &Index,
    schema: &SearchSchema,
    query_str: &str,
    limit: usize,
) -> anyhow::Result<Vec<TextHit>> {
    if query_str.trim().is_empty() {
        return Ok(Vec::new());
    }

    let reader = index
        .reader_builder()
        .reload_policy(ReloadPolicy::OnCommitWithDelay)
        .try_into()?;
    let searcher = reader.searcher();
    let query_parser = QueryParser::for_index(index, vec![schema.title, schema.body]);
    // A query containing characters the parser treats as syntax (e.g. a
    // bare `:` or unbalanced quote in a natural-language question) should
    // degrade to "no keyword hits" for this query, not fail the whole
    // hybrid search — the vector half still runs.
    let Ok(query) = query_parser.parse_query(query_str) else {
        return Ok(Vec::new());
    };

    let top_docs = searcher.search(&query, &TopDocs::with_limit(limit).order_by_score())?;
    let mut hits = Vec::new();
    for (score, doc_address) in top_docs {
        let retrieved: TantivyDocument = searcher.doc(doc_address)?;
        if let Some(path) = retrieved
            .get_first(schema.path)
            .and_then(|value| value.as_str())
        {
            hits.push(TextHit {
                path: path.to_string(),
                score,
            });
        }
    }
    Ok(hits)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_then_search_finds_a_matching_document_by_title_or_body() {
        let vault = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(vault.path().join(".okf")).unwrap();
        let (index, schema) = open_or_create(vault.path()).unwrap();

        write_documents(
            &index,
            &schema,
            &[
                TextDocument {
                    path: "wiki/concepts/resiliency-patterns.md",
                    title: "Resiliency Patterns",
                    body: "How do we handle API rate limits and retries.",
                    doc_type: "concept",
                },
                TextDocument {
                    path: "wiki/concepts/unrelated.md",
                    title: "Unrelated",
                    body: "Something about colors and shapes.",
                    doc_type: "concept",
                },
            ],
        )
        .unwrap();

        let hits = search_text(&index, &schema, "rate limits", 5).unwrap();
        assert!(!hits.is_empty());
        assert_eq!(hits[0].path, "wiki/concepts/resiliency-patterns.md");
    }

    #[test]
    fn reindexing_clears_previously_indexed_documents() {
        let vault = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(vault.path().join(".okf")).unwrap();
        let (index, schema) = open_or_create(vault.path()).unwrap();

        write_documents(
            &index,
            &schema,
            &[TextDocument {
                path: "a.md",
                title: "A",
                body: "alpha",
                doc_type: "concept",
            }],
        )
        .unwrap();
        write_documents(&index, &schema, &[]).unwrap();

        let hits = search_text(&index, &schema, "alpha", 5).unwrap();
        assert!(hits.is_empty());
    }

    #[test]
    fn an_empty_query_returns_no_hits_instead_of_matching_everything() {
        let vault = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(vault.path().join(".okf")).unwrap();
        let (index, schema) = open_or_create(vault.path()).unwrap();
        write_documents(
            &index,
            &schema,
            &[TextDocument {
                path: "a.md",
                title: "A",
                body: "alpha",
                doc_type: "concept",
            }],
        )
        .unwrap();

        let hits = search_text(&index, &schema, "   ", 5).unwrap();
        assert!(hits.is_empty());
    }
}
