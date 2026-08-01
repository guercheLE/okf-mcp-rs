// Single source of truth for embedding computation. `search::vectors`
// (reindex time, embedding wiki/raw content) and `search::query` (query
// time, embedding the search string) both call `embed()` from here, so the
// two are structurally guaranteed to share the same model and vector
// space. `all-mpnet-base-v2` at its native 768 dimensions, matching
// `search::vectors::EMBEDDING_DIM` and the `document_vectors` vec0 table's
// `FLOAT[768]` column exactly.

use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

use fastembed::{EmbeddingModel, TextEmbedding, TextInitOptions};

/// Where the ~415 MiB embedding model gets cached, in order:
/// 1. `FASTEMBED_CACHE_DIR`, if the environment already sets one.
/// 2. `<home>/.okf-mcp/models` — a stable, absolute location, consistent
///    with `core::credential_storage`'s own home-dir convention.
/// 3. `<the okf-mcp executable's own directory>/okf-mcp-models` — only
///    reached when neither `HOME` nor `USERPROFILE` resolves, so there's
///    no meaningful "home" to use.
///
/// Deliberately does *not* delegate to `credential_storage::resolve_home_dir()`:
/// that helper's own last resort is the relative path `"."`, which would
/// silently reintroduce the exact CWD-dependent re-download bug this
/// function exists to fix — this needs to detect "no home dir available"
/// and fall through to tier 3 instead of using a relative path.
fn resolve_models_cache_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("FASTEMBED_CACHE_DIR")
        && !dir.trim().is_empty()
    {
        return PathBuf::from(dir);
    }
    if let Some(home) = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")) {
        return PathBuf::from(home).join(".okf-mcp/models");
    }
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.join("okf-mcp-models")))
        .unwrap_or_else(|| PathBuf::from(".fastembed_cache"))
}

fn model() -> &'static Mutex<TextEmbedding> {
    static MODEL: OnceLock<Mutex<TextEmbedding>> = OnceLock::new();
    MODEL.get_or_init(|| {
        // Downloads on first use and caches locally afterward (no network
        // needed once cached) — mirrors `@xenova/transformers`' own
        // caching UX. `.expect()` here matches the TS target's own
        // failure mode: an unrecoverable startup error either way if the
        // model can't be fetched/loaded.
        Mutex::new(
            TextEmbedding::try_new(
                TextInitOptions::new(EmbeddingModel::AllMpnetBaseV2)
                    .with_cache_dir(resolve_models_cache_dir()),
            )
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

#[cfg(test)]
mod tests {
    use super::*;

    // Pure path resolution, no model download/network involved in any case.

    #[test]
    fn resolve_models_cache_dir_prefers_an_explicit_fastembed_cache_dir_env_var() {
        let _guard = crate::core::credential_storage::HOME_ENV_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        // SAFETY: test-only env mutation, serialized by the guard above.
        unsafe {
            std::env::set_var("FASTEMBED_CACHE_DIR", "/explicit/cache/dir");
        }
        assert_eq!(
            resolve_models_cache_dir(),
            PathBuf::from("/explicit/cache/dir")
        );
        unsafe {
            std::env::remove_var("FASTEMBED_CACHE_DIR");
        }
    }

    #[test]
    fn resolve_models_cache_dir_falls_back_to_home_when_the_env_var_is_unset() {
        let _guard = crate::core::credential_storage::HOME_ENV_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let prev_home = std::env::var_os("HOME");
        // SAFETY: test-only env mutation, serialized by the guard above.
        unsafe {
            std::env::remove_var("FASTEMBED_CACHE_DIR");
            std::env::set_var("HOME", dir.path());
        }

        assert_eq!(
            resolve_models_cache_dir(),
            dir.path().join(".okf-mcp/models")
        );

        unsafe {
            match prev_home {
                Some(prev) => std::env::set_var("HOME", prev),
                None => std::env::remove_var("HOME"),
            }
        }
    }

    #[test]
    fn resolve_models_cache_dir_is_absolute_and_not_the_relative_default_when_home_is_unset() {
        let _guard = crate::core::credential_storage::HOME_ENV_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let prev_home = std::env::var_os("HOME");
        let prev_userprofile = std::env::var_os("USERPROFILE");
        // SAFETY: test-only env mutation, serialized by the guard above.
        unsafe {
            std::env::remove_var("FASTEMBED_CACHE_DIR");
            std::env::remove_var("HOME");
            std::env::remove_var("USERPROFILE");
        }

        let resolved = resolve_models_cache_dir();
        assert!(
            resolved.is_absolute(),
            "expected an absolute path, got {resolved:?}"
        );
        assert_ne!(resolved, PathBuf::from(".fastembed_cache"));

        unsafe {
            if let Some(prev) = prev_home {
                std::env::set_var("HOME", prev);
            }
            if let Some(prev) = prev_userprofile {
                std::env::set_var("USERPROFILE", prev);
            }
        }
    }
}
