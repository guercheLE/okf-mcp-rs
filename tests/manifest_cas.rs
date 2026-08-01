//! Integration-level coverage of the manifest CAS lifecycle through the
//! public `okf_mcp::manifest` API — persisted across separate `load`/`save`
//! calls, the way the real ingest/delete pipeline will use it, rather than
//! just in-memory (see `src/manifest/model.rs`'s own unit tests for the pure
//! state-machine cases).

use okf_mcp::manifest::{self, IngestOutcome, SourceStatus};

#[test]
fn ingest_update_and_soft_delete_survive_a_save_load_round_trip() {
    let vault = tempfile::tempdir().unwrap();

    let mut manifest = manifest::store::load(vault.path()).unwrap();
    let outcome = manifest.record_ingest("https://example.com/spec", "sha256:aaa", "raw_aaa", "t0");
    assert_eq!(
        outcome,
        IngestOutcome::New {
            raw_id: "raw_aaa".to_string()
        }
    );
    manifest::store::save(vault.path(), &manifest).unwrap();

    // A fresh process re-ingesting the same URL: load from disk, not the
    // in-memory struct above.
    let mut reloaded = manifest::store::load(vault.path()).unwrap();
    let outcome = reloaded.record_ingest("https://example.com/spec", "sha256:bbb", "raw_bbb", "t1");
    assert_eq!(
        outcome,
        IngestOutcome::Superseded {
            raw_id: "raw_bbb".to_string(),
            previous_raw_id: "raw_aaa".to_string(),
        }
    );
    manifest::store::save(vault.path(), &reloaded).unwrap();

    let mut final_state = manifest::store::load(vault.path()).unwrap();
    assert_eq!(
        final_state.get_active_raw_id("https://example.com/spec"),
        Some("raw_bbb")
    );

    final_state
        .tombstone("https://example.com/spec", "user requested delete", "t2")
        .unwrap();
    manifest::store::save(vault.path(), &final_state).unwrap();

    let after_delete = manifest::store::load(vault.path()).unwrap();
    assert_eq!(
        after_delete.get_active_raw_id("https://example.com/spec"),
        None
    );
    let history = &after_delete.sources["https://example.com/spec"].history;
    assert_eq!(history.len(), 2);
    assert!(matches!(history[1].status, SourceStatus::Tombstoned { .. }));
    // Soft delete never drops history — hard delete (`purge`) is the only
    // operation that does, and it's the caller's job to also unlink the raw
    // blobs it names.
}

#[test]
fn purge_removes_the_manifest_entry_entirely() {
    let vault = tempfile::tempdir().unwrap();

    let mut manifest = manifest::store::load(vault.path()).unwrap();
    manifest.record_ingest("file:///docs/legacy.md", "sha256:ccc", "raw_ccc", "t0");
    manifest::store::save(vault.path(), &manifest).unwrap();

    let mut reloaded = manifest::store::load(vault.path()).unwrap();
    let removed = reloaded.purge("file:///docs/legacy.md").unwrap();
    assert_eq!(removed.history[0].raw_id, "raw_ccc");
    manifest::store::save(vault.path(), &reloaded).unwrap();

    let after_purge = manifest::store::load(vault.path()).unwrap();
    assert!(!after_purge.sources.contains_key("file:///docs/legacy.md"));
}
