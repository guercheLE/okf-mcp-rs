// Single source of truth for embedding computation. `search::vectors`
// (reindex time, embedding wiki/raw content) and `search::query` (query
// time, embedding the search string) both call `embed()` from here, so the
// two are structurally guaranteed to share the same model and vector
// space. `all-mpnet-base-v2` at its native 768 dimensions, matching
// `search::vectors::EMBEDDING_DIM` and the `document_vectors` vec0 table's
// `FLOAT[768]` column exactly.

use std::sync::{Mutex, OnceLock};

use fastembed::{EmbeddingModel, TextEmbedding, TextInitOptions};

fn model() -> &'static Mutex<TextEmbedding> {
    static MODEL: OnceLock<Mutex<TextEmbedding>> = OnceLock::new();
    MODEL.get_or_init(|| {
        // Downloads on first use and caches locally afterward (no network
        // needed once cached) — mirrors `@xenova/transformers`' own
        // caching UX. `.expect()` here matches the TS target's own
        // failure mode: an unrecoverable startup error either way if the
        // model can't be fetched/loaded.
        Mutex::new(
            TextEmbedding::try_new(TextInitOptions::new(EmbeddingModel::AllMpnetBaseV2))
                .expect("failed to load the all-mpnet-base-v2 embedding model"),
        )
    })
}

/// Computes a 768-dim embedding vector for `text`, mean-pooled and
/// normalized (fastembed's default behavior for this model, replicating
/// the sentence-transformers reference implementation).
pub fn embed(text: &str) -> anyhow::Result<Vec<f32>> {
    let model = model();
    let mut model = model.lock().unwrap();
    let mut embeddings = model.embed(vec![text], None)?;
    embeddings
        .pop()
        .ok_or_else(|| anyhow::anyhow!("embedding model returned no output for the given text"))
}
