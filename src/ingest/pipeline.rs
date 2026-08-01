//! Orchestrates a single `okf-mcp ingest <URL|FILE>` / `okf_ingest` call:
//! fetch or parse -> hash -> manifest CAS update -> conditional write of
//! `./raw/<raw_id>.md`. Shared by both the CLI command and the MCP tool, so
//! there's exactly one place this logic lives.

use std::path::{Path, PathBuf};

use crate::auth::auth_manager::AuthManager;
use crate::auth::request_credentials::RequestCredentials;
use crate::core::config_schema::Config;
use crate::manifest::{self, IngestOutcome};

use super::{frontmatter, local, web};

#[derive(Debug)]
pub struct IngestReport {
    pub source_uri: String,
    pub outcome: IngestOutcome,
    pub raw_path: Option<PathBuf>,
}

/// A local path is keyed in the manifest as `file://<canonical path>`, so
/// two different relative spellings of the same file (`./a.md` vs.
/// `a.md`, run from different directories) resolve to the same manifest
/// entry — matching the design doc's Q3 example (`file:///docs/legacy-auth.md`).
fn normalize_local_uri(path: &Path) -> anyhow::Result<String> {
    let canonical = path
        .canonicalize()
        .map_err(|err| anyhow::anyhow!("cannot read '{}': {err}", path.display()))?;
    Ok(format!("file://{}", canonical.display()))
}

pub async fn process_ingest(
    source: &str,
    tag: Option<&str>,
    vault_root: &Path,
    config: &Config,
    auth_manager: &mut AuthManager,
    request_override: Option<&RequestCredentials>,
) -> anyhow::Result<IngestReport> {
    let (source_uri, content) = if frontmatter::is_url(source) {
        let content =
            web::fetch_and_clean_url(source, config, auth_manager, request_override).await?;
        (source.to_string(), content)
    } else {
        let path = Path::new(source);
        let content = local::parse_local_doc(path)?;
        (normalize_local_uri(path)?, content)
    };

    let hash = frontmatter::hash_content(&content);
    let raw_id = frontmatter::raw_id_for(&hash);
    let ingested_at = chrono::Utc::now().to_rfc3339();

    let mut manifest = manifest::store::load(vault_root)?;
    let outcome = manifest.record_ingest(&source_uri, &hash, &raw_id, &ingested_at);

    let raw_path = match &outcome {
        IngestOutcome::NoOp => None,
        IngestOutcome::New { raw_id } | IngestOutcome::Superseded { raw_id, .. } => {
            Some(frontmatter::write_raw_blob(
                vault_root,
                raw_id,
                &source_uri,
                tag,
                &hash,
                &ingested_at,
                &content,
            )?)
        }
    };

    manifest::store::save(vault_root, &manifest)?;

    Ok(IngestReport {
        source_uri,
        outcome,
        raw_path,
    })
}

/// Same URI shape as `normalize_local_uri`, but tolerant of the file
/// already being gone from disk (common for `delete`, unlike `ingest`,
/// which always reads the file it's normalizing a path for) — falls back
/// to a non-canonicalized absolute path (resolved against the current
/// directory) when `canonicalize` fails because the file doesn't exist.
/// This won't match `ingest`'s own key if a symlink was involved, but that
/// same edge case is already unresolvable once the file is gone.
fn normalize_local_uri_best_effort(path: &Path) -> anyhow::Result<String> {
    match path.canonicalize() {
        Ok(canonical) => Ok(format!("file://{}", canonical.display())),
        Err(_) => {
            let absolute = if path.is_absolute() {
                path.to_path_buf()
            } else {
                std::env::current_dir()?.join(path)
            };
            Ok(format!("file://{}", absolute.display()))
        }
    }
}

#[derive(Debug)]
pub enum DeleteOutcome {
    /// Soft delete: the manifest entry is marked `TOMBSTONED`; raw blobs
    /// stay on disk.
    Tombstoned,
    /// Hard delete: the manifest entry and every one of its raw blobs are
    /// removed.
    Purged { removed_raw_ids: Vec<String> },
}

/// `okf-mcp delete <URL|FILE> [--purge]` / `okf_delete`'s business logic.
/// Soft delete (`purge: false`) by default; `purge: true` additionally
/// unlinks every raw blob `uri` ever had, per the design's GDPR/secrets
/// hard-delete path.
pub fn delete_source(
    vault_root: &Path,
    source: &str,
    purge: bool,
    reason: &str,
) -> anyhow::Result<DeleteOutcome> {
    let source_uri = if frontmatter::is_url(source) {
        source.to_string()
    } else {
        normalize_local_uri_best_effort(Path::new(source))?
    };

    let mut manifest = manifest::store::load(vault_root)?;
    let now = chrono::Utc::now().to_rfc3339();

    if purge {
        let removed = manifest
            .purge(&source_uri)
            .ok_or_else(|| anyhow::anyhow!("no ingested source found for '{source_uri}'"))?;
        let mut removed_raw_ids = Vec::with_capacity(removed.history.len());
        for version in removed.history {
            let _ = crate::storage::fs_ops::remove_file(
                vault_root,
                &format!("raw/{}.md", version.raw_id),
            );
            removed_raw_ids.push(version.raw_id);
        }
        manifest::store::save(vault_root, &manifest)?;
        Ok(DeleteOutcome::Purged { removed_raw_ids })
    } else {
        manifest.tombstone(&source_uri, reason, &now)?;
        manifest::store::save(vault_root, &manifest)?;
        Ok(DeleteOutcome::Tombstoned)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::auth_strategy::Credentials;
    use crate::core::config_schema::AuthMethod;

    fn config() -> Config {
        serde_json::from_value(serde_json::json!({
            "url": "http://unused.invalid",
            "auth_method": "pat",
        }))
        .unwrap()
    }

    fn auth_manager() -> AuthManager {
        let mut manager = AuthManager::new(AuthMethod::Pat);
        manager.set_credentials(Credentials::from([(
            "token".to_string(),
            "s3cr3t".to_string(),
        )]));
        manager
    }

    #[tokio::test]
    async fn ingesting_a_local_file_writes_a_raw_blob_and_updates_the_manifest() {
        let vault = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(vault.path().join(".okf")).unwrap();
        let source = vault.path().join("source.md");
        std::fs::write(&source, "# Hello").unwrap();

        let report = process_ingest(
            source.to_str().unwrap(),
            Some("architecture"),
            vault.path(),
            &config(),
            &mut auth_manager(),
            None,
        )
        .await
        .unwrap();

        assert!(matches!(report.outcome, IngestOutcome::New { .. }));
        let raw_path = report.raw_path.unwrap();
        assert!(raw_path.is_file());
        let contents = std::fs::read_to_string(&raw_path).unwrap();
        assert!(contents.contains("# Hello"));

        let manifest = manifest::store::load(vault.path()).unwrap();
        assert!(manifest.get_active_raw_id(&report.source_uri).is_some());
    }

    #[tokio::test]
    async fn reingesting_unchanged_local_content_is_a_no_op_and_writes_nothing_new() {
        let vault = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(vault.path().join(".okf")).unwrap();
        let source = vault.path().join("source.md");
        std::fs::write(&source, "unchanged").unwrap();

        let first = process_ingest(
            source.to_str().unwrap(),
            None,
            vault.path(),
            &config(),
            &mut auth_manager(),
            None,
        )
        .await
        .unwrap();
        assert!(matches!(first.outcome, IngestOutcome::New { .. }));

        let second = process_ingest(
            source.to_str().unwrap(),
            None,
            vault.path(),
            &config(),
            &mut auth_manager(),
            None,
        )
        .await
        .unwrap();
        assert_eq!(second.outcome, IngestOutcome::NoOp);
        assert!(second.raw_path.is_none());
    }

    #[tokio::test]
    async fn reingesting_changed_local_content_supersedes_the_previous_raw_blob() {
        let vault = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(vault.path().join(".okf")).unwrap();
        let source = vault.path().join("source.md");
        std::fs::write(&source, "version one").unwrap();

        process_ingest(
            source.to_str().unwrap(),
            None,
            vault.path(),
            &config(),
            &mut auth_manager(),
            None,
        )
        .await
        .unwrap();

        std::fs::write(&source, "version two").unwrap();
        let second = process_ingest(
            source.to_str().unwrap(),
            None,
            vault.path(),
            &config(),
            &mut auth_manager(),
            None,
        )
        .await
        .unwrap();

        assert!(matches!(second.outcome, IngestOutcome::Superseded { .. }));
        let raw_path = second.raw_path.unwrap();
        assert!(
            std::fs::read_to_string(&raw_path)
                .unwrap()
                .contains("version two")
        );

        // The superseded blob is still on disk — append-only.
        let raw_dir = vault.path().join("raw");
        let entries: Vec<_> = std::fs::read_dir(&raw_dir).unwrap().collect();
        assert_eq!(entries.len(), 2);
    }

    #[tokio::test]
    async fn ingesting_a_missing_local_file_is_a_clear_error() {
        let vault = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(vault.path().join(".okf")).unwrap();

        let result = process_ingest(
            "/does/not/exist.md",
            None,
            vault.path(),
            &config(),
            &mut auth_manager(),
            None,
        )
        .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn delete_soft_tombstones_without_removing_the_raw_blob() {
        let vault = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(vault.path().join(".okf")).unwrap();
        let source = vault.path().join("source.md");
        std::fs::write(&source, "content").unwrap();

        let report = process_ingest(
            source.to_str().unwrap(),
            None,
            vault.path(),
            &config(),
            &mut auth_manager(),
            None,
        )
        .await
        .unwrap();
        let raw_path = report.raw_path.unwrap();
        assert!(raw_path.is_file());

        let outcome = delete_source(
            vault.path(),
            source.to_str().unwrap(),
            false,
            "no longer needed",
        )
        .unwrap();
        assert!(matches!(outcome, DeleteOutcome::Tombstoned));
        assert!(raw_path.is_file());

        let manifest = manifest::store::load(vault.path()).unwrap();
        assert_eq!(manifest.get_active_raw_id(&report.source_uri), None);
    }

    #[tokio::test]
    async fn delete_purge_removes_the_raw_blob_and_the_manifest_entry() {
        let vault = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(vault.path().join(".okf")).unwrap();
        let source = vault.path().join("source.md");
        std::fs::write(&source, "content").unwrap();

        let report = process_ingest(
            source.to_str().unwrap(),
            None,
            vault.path(),
            &config(),
            &mut auth_manager(),
            None,
        )
        .await
        .unwrap();
        let raw_path = report.raw_path.unwrap();

        let outcome =
            delete_source(vault.path(), source.to_str().unwrap(), true, "gdpr request").unwrap();
        assert!(matches!(outcome, DeleteOutcome::Purged { .. }));
        assert!(!raw_path.exists());

        let manifest = manifest::store::load(vault.path()).unwrap();
        assert!(!manifest.sources.contains_key(&report.source_uri));
    }

    #[test]
    fn deleting_a_uri_that_was_never_ingested_is_an_error() {
        let vault = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(vault.path().join(".okf")).unwrap();
        assert!(
            delete_source(
                vault.path(),
                "https://example.com/never-ingested",
                false,
                "n/a"
            )
            .is_err()
        );
        assert!(
            delete_source(
                vault.path(),
                "https://example.com/never-ingested",
                true,
                "n/a"
            )
            .is_err()
        );
    }
}
