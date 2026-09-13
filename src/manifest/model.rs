//! Content-addressable-storage manifest (`.okf/manifest.json`): the
//! append-only `./raw/` tree stays immutable, and updates/deletes are
//! recorded here as state transitions (`Active` -> `Superseded`/`Tombstoned`)
//! instead of mutating or removing raw blobs. Keyed by the ingested source's
//! URI or local path, not by raw_id, so `record_ingest` can look up "have we
//! seen this source before" in one lookup.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status")]
pub enum SourceStatus {
    #[serde(rename = "ACTIVE")]
    Active,
    #[serde(rename = "SUPERSEDED")]
    Superseded { by_raw_id: String },
    #[serde(rename = "TOMBSTONED")]
    Tombstoned {
        reason: String,
        tombstoned_at: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceVersion {
    pub raw_id: String,
    pub hash: String,
    pub ingested_at: String,
    /// Vault-relative on-disk path of this version's raw blob
    /// (`raw/<raw_id>.md` today; a slugged `raw/<raw_id>--<slug>.md` once
    /// phase 2 of the raw-filename change lands), populated at write time
    /// by `ingest::pipeline::process_ingest` once the blob is actually
    /// written — `record_ingest` itself runs *before* that write, so it
    /// can't populate this field directly (see `Manifest::record_raw_path`).
    /// `#[serde(default)]` so entries recorded before this field existed
    /// deserialize with `None` rather than failing — `resolve_raw_path`
    /// falls back to a directory scan for those, no migration needed,
    /// matching the existing `compiled_hash`-style backward-compat pattern.
    #[serde(default)]
    pub raw_path: Option<String>,
    #[serde(flatten)]
    pub status: SourceStatus,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SourceEntry {
    pub active_hash: Option<String>,
    /// The `active_hash` value at which this source last *fully* compiled
    /// without error, or `None` if it never has. Compared against
    /// `active_hash` (not stored as a boolean) so a re-ingest — which
    /// changes `active_hash` — automatically invalidates it without a
    /// separate "clear" step. `#[serde(default)]` so vaults whose
    /// `manifest.json` predates this field deserialize with `None` rather
    /// than failing.
    #[serde(default)]
    pub compiled_hash: Option<String>,
    pub history: Vec<SourceVersion>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Manifest {
    #[serde(default)]
    pub sources: HashMap<String, SourceEntry>,
}

/// What `record_ingest` actually did, so callers (the ingest pipeline) know
/// whether a new raw blob needs writing to disk at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IngestOutcome {
    /// The hash matched the currently active version — nothing written.
    NoOp,
    /// First time this URI/path has ever been ingested.
    New { raw_id: String },
    /// A new snapshot replaced a previously active one.
    Superseded {
        raw_id: String,
        previous_raw_id: String,
    },
}

impl Manifest {
    pub fn get_active_raw_id(&self, uri: &str) -> Option<&str> {
        self.sources
            .get(uri)?
            .history
            .iter()
            .find(|version| version.status == SourceStatus::Active)
            .map(|version| version.raw_id.as_str())
    }

    /// Records an ingest attempt for `uri`. No-ops (and writes nothing) when
    /// `hash` matches the already-active snapshot; otherwise marks any
    /// currently-active version `Superseded` and appends a new `Active` one.
    /// Never mutates or removes an existing `SourceVersion` in place beyond
    /// that status flip — history is append-only, matching the append-only
    /// guarantee `./raw/` itself gives callers.
    pub fn record_ingest(
        &mut self,
        uri: &str,
        hash: &str,
        raw_id: &str,
        ingested_at: &str,
    ) -> IngestOutcome {
        let entry = self.sources.entry(uri.to_string()).or_default();

        if entry.active_hash.as_deref() == Some(hash) {
            return IngestOutcome::NoOp;
        }

        let previous_raw_id = entry
            .history
            .iter_mut()
            .find(|version| version.status == SourceStatus::Active)
            .map(|version| {
                version.status = SourceStatus::Superseded {
                    by_raw_id: raw_id.to_string(),
                };
                version.raw_id.clone()
            });

        entry.history.push(SourceVersion {
            raw_id: raw_id.to_string(),
            hash: hash.to_string(),
            ingested_at: ingested_at.to_string(),
            raw_path: None,
            status: SourceStatus::Active,
        });
        entry.active_hash = Some(hash.to_string());

        match previous_raw_id {
            Some(previous_raw_id) => IngestOutcome::Superseded {
                raw_id: raw_id.to_string(),
                previous_raw_id,
            },
            None => IngestOutcome::New {
                raw_id: raw_id.to_string(),
            },
        }
    }

    /// Soft delete: marks the active version (if any) `Tombstoned` and
    /// clears `active_hash`, but leaves the raw blob on disk and the history
    /// entry in place — `okf-mcp delete --purge` (`purge`, below) is the
    /// only operation that actually removes anything.
    pub fn tombstone(&mut self, uri: &str, reason: &str, at: &str) -> anyhow::Result<()> {
        let entry = self
            .sources
            .get_mut(uri)
            .ok_or_else(|| anyhow::anyhow!("no ingested source found for '{uri}'"))?;

        let active = entry
            .history
            .iter_mut()
            .find(|version| version.status == SourceStatus::Active)
            .ok_or_else(|| anyhow::anyhow!("'{uri}' has no active version to tombstone"))?;

        active.status = SourceStatus::Tombstoned {
            reason: reason.to_string(),
            tombstoned_at: at.to_string(),
        };
        entry.active_hash = None;
        Ok(())
    }

    /// Hard delete: removes `uri`'s entire manifest entry. Deleting the raw
    /// blob(s) on disk is the caller's responsibility (`manifest::store`
    /// only tracks state, not files) — returns the removed entry so the
    /// caller knows which `raw_id`s to unlink.
    pub fn purge(&mut self, uri: &str) -> Option<SourceEntry> {
        self.sources.remove(uri)
    }

    /// Marks `uri`'s currently-active hash as successfully, fully compiled
    /// — call only after every operation for that source's compile batch
    /// has applied without error. A no-op if `uri` has no active entry.
    pub fn mark_compiled(&mut self, uri: &str) {
        if let Some(entry) = self.sources.get_mut(uri) {
            entry.compiled_hash = entry.active_hash.clone();
        }
    }

    /// Whether `uri` has already been fully compiled at its *current*
    /// active hash — the authoritative "does this source need
    /// (re)compiling" check `compiler::driver::select_sources` uses.
    pub fn is_compiled_at_current_hash(&self, uri: &str) -> bool {
        self.sources.get(uri).is_some_and(|entry| {
            entry.compiled_hash.is_some() && entry.compiled_hash == entry.active_hash
        })
    }

    /// Populates `raw_path` on the `SourceVersion` identified by `raw_id`
    /// within `uri`'s history, once its physical file has actually been
    /// written (`record_ingest` runs first and can't know the path yet —
    /// see `ingest::pipeline::process_ingest`, the only real caller). A
    /// no-op if `uri`/`raw_id` aren't found, which should never happen
    /// given the caller's own invariant of calling this right after
    /// `record_ingest` returned that exact `raw_id`.
    pub fn record_raw_path(&mut self, uri: &str, raw_id: &str, raw_path: String) {
        if let Some(entry) = self.sources.get_mut(uri)
            && let Some(version) = entry.history.iter_mut().find(|v| v.raw_id == raw_id)
        {
            version.raw_path = Some(raw_path);
        }
    }

    /// The recorded `raw_path` for `raw_id`, searched across every source's
    /// entire history (not just active entries) — `resolve_raw_path` needs
    /// to find a raw_id's physical file even after it's been superseded or
    /// tombstoned, since the blob itself stays on disk; only its manifest
    /// status changes. `None` when never recorded (pre-this-field entries,
    /// or a `raw_id` the manifest doesn't know at all).
    pub fn raw_path_for(&self, raw_id: &str) -> Option<&str> {
        self.sources.values().find_map(|entry| {
            entry
                .history
                .iter()
                .find(|version| version.raw_id == raw_id)
                .and_then(|version| version.raw_path.as_deref())
        })
    }

    /// Finds the manifest's version record for `raw_id`, searched across
    /// every source's entire history (not just active entries) — mirrors
    /// `raw_path_for`'s search, but returns the full `SourceStatus` (and the
    /// owning source's URI) rather than just the recorded path. Backs
    /// `okf-trace-provenance`'s manifest-status join: a raw blob's own
    /// frontmatter has no way of knowing it was later superseded or
    /// tombstoned, only the manifest does. `None` when `raw_id` isn't
    /// tracked by this manifest at all (e.g. a raw file dropped in by hand).
    pub fn find_version(&self, raw_id: &str) -> Option<(&str, &SourceVersion)> {
        self.sources.iter().find_map(|(uri, entry)| {
            entry
                .history
                .iter()
                .find(|version| version.raw_id == raw_id)
                .map(|version| (uri.as_str(), version))
        })
    }

    /// Every `(uri, active SourceVersion)` pair — the set `compile`/`reindex`
    /// should actually read, per the design's "source of truth = ACTIVE
    /// manifest entries only" rule.
    pub fn active_entries(&self) -> impl Iterator<Item = (&str, &SourceVersion)> {
        self.sources.iter().filter_map(|(uri, entry)| {
            entry
                .history
                .iter()
                .find(|version| version.status == SourceStatus::Active)
                .map(|version| (uri.as_str(), version))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_ingest_of_a_uri_is_new_and_active() {
        let mut manifest = Manifest::default();
        let outcome =
            manifest.record_ingest("https://example.com/a", "sha256:aaa", "raw_aaa", "t0");
        assert_eq!(
            outcome,
            IngestOutcome::New {
                raw_id: "raw_aaa".to_string()
            }
        );
        assert_eq!(
            manifest.get_active_raw_id("https://example.com/a"),
            Some("raw_aaa")
        );
    }

    #[test]
    fn reingesting_the_same_hash_is_a_no_op() {
        let mut manifest = Manifest::default();
        manifest.record_ingest("uri", "sha256:aaa", "raw_aaa", "t0");
        let outcome = manifest.record_ingest("uri", "sha256:aaa", "raw_should_not_be_used", "t1");
        assert_eq!(outcome, IngestOutcome::NoOp);
        assert_eq!(manifest.sources["uri"].history.len(), 1);
    }

    #[test]
    fn reingesting_a_different_hash_supersedes_the_old_active_version() {
        let mut manifest = Manifest::default();
        manifest.record_ingest("uri", "sha256:aaa", "raw_aaa", "t0");
        let outcome = manifest.record_ingest("uri", "sha256:bbb", "raw_bbb", "t1");

        assert_eq!(
            outcome,
            IngestOutcome::Superseded {
                raw_id: "raw_bbb".to_string(),
                previous_raw_id: "raw_aaa".to_string(),
            }
        );
        assert_eq!(manifest.get_active_raw_id("uri"), Some("raw_bbb"));

        let history = &manifest.sources["uri"].history;
        assert_eq!(history.len(), 2);
        assert_eq!(
            history[0].status,
            SourceStatus::Superseded {
                by_raw_id: "raw_bbb".to_string()
            }
        );
        assert_eq!(history[1].status, SourceStatus::Active);
    }

    #[test]
    fn tombstone_clears_active_hash_and_records_the_reason() {
        let mut manifest = Manifest::default();
        manifest.record_ingest("uri", "sha256:aaa", "raw_aaa", "t0");
        manifest.tombstone("uri", "deprecated", "t1").unwrap();

        assert_eq!(manifest.get_active_raw_id("uri"), None);
        assert_eq!(manifest.sources["uri"].active_hash, None);
        assert_eq!(
            manifest.sources["uri"].history[0].status,
            SourceStatus::Tombstoned {
                reason: "deprecated".to_string(),
                tombstoned_at: "t1".to_string(),
            }
        );
    }

    #[test]
    fn tombstoning_an_unknown_uri_is_an_error() {
        let mut manifest = Manifest::default();
        assert!(manifest.tombstone("nope", "reason", "t0").is_err());
    }

    #[test]
    fn tombstoning_a_uri_with_no_active_version_is_an_error() {
        let mut manifest = Manifest::default();
        manifest.record_ingest("uri", "sha256:aaa", "raw_aaa", "t0");
        manifest.tombstone("uri", "first", "t1").unwrap();
        assert!(manifest.tombstone("uri", "second", "t2").is_err());
    }

    #[test]
    fn purge_removes_the_entire_entry_and_returns_it() {
        let mut manifest = Manifest::default();
        manifest.record_ingest("uri", "sha256:aaa", "raw_aaa", "t0");
        let removed = manifest.purge("uri").unwrap();
        assert_eq!(removed.history.len(), 1);
        assert!(!manifest.sources.contains_key("uri"));
        assert!(manifest.purge("uri").is_none());
    }

    #[test]
    fn active_entries_only_yields_currently_active_versions() {
        let mut manifest = Manifest::default();
        manifest.record_ingest("a", "sha256:1", "raw_1", "t0");
        manifest.record_ingest("b", "sha256:2", "raw_2", "t0");
        manifest.tombstone("b", "gone", "t1").unwrap();

        let active: Vec<_> = manifest.active_entries().collect();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].0, "a");
        assert_eq!(active[0].1.raw_id, "raw_1");
    }

    #[test]
    fn manifest_round_trips_through_json() {
        let mut manifest = Manifest::default();
        manifest.record_ingest("uri", "sha256:aaa", "raw_aaa", "t0");
        manifest.record_ingest("uri", "sha256:bbb", "raw_bbb", "t1");

        let json = serde_json::to_string(&manifest).unwrap();
        let restored: Manifest = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.get_active_raw_id("uri"), Some("raw_bbb"));
        assert_eq!(restored.sources["uri"].history.len(), 2);
    }

    #[test]
    fn a_manifest_json_without_the_compiled_hash_field_still_deserializes() {
        let json = r#"{"sources":{"uri":{"active_hash":"sha256:aaa","history":[]}}}"#;
        let manifest: Manifest = serde_json::from_str(json).unwrap();
        assert_eq!(manifest.sources["uri"].compiled_hash, None);
    }

    #[test]
    fn a_source_version_json_without_the_raw_path_field_still_deserializes() {
        let json = r#"{"sources":{"uri":{"active_hash":"sha256:aaa","history":[
            {"raw_id":"raw_aaa","hash":"sha256:aaa","ingested_at":"t0","status":"ACTIVE"}
        ]}}}"#;
        let manifest: Manifest = serde_json::from_str(json).unwrap();
        assert_eq!(manifest.sources["uri"].history[0].raw_path, None);
    }

    #[test]
    fn record_raw_path_sets_it_on_the_matching_history_entry() {
        let mut manifest = Manifest::default();
        manifest.record_ingest("uri", "sha256:aaa", "raw_aaa", "t0");
        manifest.record_raw_path("uri", "raw_aaa", "raw/raw_aaa.md".to_string());

        assert_eq!(
            manifest.sources["uri"].history[0].raw_path.as_deref(),
            Some("raw/raw_aaa.md")
        );
        assert_eq!(manifest.raw_path_for("raw_aaa"), Some("raw/raw_aaa.md"));
    }

    #[test]
    fn record_raw_path_on_an_unknown_uri_or_raw_id_is_a_no_op() {
        let mut manifest = Manifest::default();
        manifest.record_ingest("uri", "sha256:aaa", "raw_aaa", "t0");
        manifest.record_raw_path("nope", "raw_aaa", "raw/raw_aaa.md".to_string());
        manifest.record_raw_path("uri", "raw_zzz", "raw/raw_zzz.md".to_string());

        assert_eq!(manifest.sources["uri"].history[0].raw_path, None);
    }

    #[test]
    fn raw_path_for_finds_a_superseded_versions_path_too() {
        let mut manifest = Manifest::default();
        manifest.record_ingest("uri", "sha256:aaa", "raw_aaa", "t0");
        manifest.record_raw_path("uri", "raw_aaa", "raw/raw_aaa.md".to_string());
        manifest.record_ingest("uri", "sha256:bbb", "raw_bbb", "t1");

        // raw_aaa is now Superseded, but its blob is still on disk and its
        // recorded path must still be findable.
        assert_eq!(manifest.raw_path_for("raw_aaa"), Some("raw/raw_aaa.md"));
        assert_eq!(manifest.raw_path_for("raw_bbb"), None);
        assert_eq!(manifest.raw_path_for("raw_nonexistent"), None);
    }

    #[test]
    fn find_version_returns_the_uri_and_status_for_an_active_raw_id() {
        let mut manifest = Manifest::default();
        manifest.record_ingest("uri", "sha256:aaa", "raw_aaa", "t0");

        let (uri, version) = manifest.find_version("raw_aaa").unwrap();
        assert_eq!(uri, "uri");
        assert_eq!(version.status, SourceStatus::Active);
    }

    #[test]
    fn find_version_finds_a_superseded_or_tombstoned_raw_id_too() {
        let mut manifest = Manifest::default();
        manifest.record_ingest("uri", "sha256:aaa", "raw_aaa", "t0");
        manifest.record_ingest("uri", "sha256:bbb", "raw_bbb", "t1");

        let (uri, version) = manifest.find_version("raw_aaa").unwrap();
        assert_eq!(uri, "uri");
        assert_eq!(
            version.status,
            SourceStatus::Superseded {
                by_raw_id: "raw_bbb".to_string()
            }
        );
    }

    #[test]
    fn find_version_is_none_for_an_untracked_raw_id() {
        let manifest = Manifest::default();
        assert!(manifest.find_version("raw_nonexistent").is_none());
    }

    #[test]
    fn mark_compiled_sets_compiled_hash_to_the_current_active_hash() {
        let mut manifest = Manifest::default();
        manifest.record_ingest("uri", "sha256:aaa", "raw_aaa", "t0");
        manifest.mark_compiled("uri");
        assert_eq!(
            manifest.sources["uri"].compiled_hash.as_deref(),
            Some("sha256:aaa")
        );
    }

    #[test]
    fn mark_compiled_on_an_unknown_uri_is_a_no_op() {
        let mut manifest = Manifest::default();
        manifest.mark_compiled("nope");
        assert!(!manifest.sources.contains_key("nope"));
    }

    #[test]
    fn is_compiled_at_current_hash_is_false_until_marked() {
        let mut manifest = Manifest::default();
        manifest.record_ingest("uri", "sha256:aaa", "raw_aaa", "t0");
        assert!(!manifest.is_compiled_at_current_hash("uri"));
        manifest.mark_compiled("uri");
        assert!(manifest.is_compiled_at_current_hash("uri"));
    }

    #[test]
    fn is_compiled_at_current_hash_is_false_for_an_unknown_uri() {
        let manifest = Manifest::default();
        assert!(!manifest.is_compiled_at_current_hash("nope"));
    }

    #[test]
    fn re_ingesting_after_a_compile_invalidates_the_compiled_flag() {
        let mut manifest = Manifest::default();
        manifest.record_ingest("uri", "sha256:aaa", "raw_aaa", "t0");
        manifest.mark_compiled("uri");
        assert!(manifest.is_compiled_at_current_hash("uri"));

        manifest.record_ingest("uri", "sha256:bbb", "raw_bbb", "t1");
        assert!(!manifest.is_compiled_at_current_hash("uri"));
        // The stale compiled_hash from the previous content is still on
        // disk (not cleared), it's just no longer equal to active_hash.
        assert_eq!(
            manifest.sources["uri"].compiled_hash.as_deref(),
            Some("sha256:aaa")
        );
    }
}
