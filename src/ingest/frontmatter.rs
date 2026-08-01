//! Frontmatter for immutable `./raw/` blobs, and the SHA-256 hashing used to
//! derive `raw_id`s and detect content changes for the manifest CAS.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::core::okf_schema::OKF_SCHEMA_VERSION;
use crate::core::vault_resolver::sandbox_path;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RawFrontmatter {
    pub okf_version: String,
    pub r#type: String,
    pub id: String,
    pub source_url: Option<String>,
    pub checksum: String,
    pub ingested_at: String,
    #[serde(default)]
    pub tag: Option<String>,
}

/// `sha256:<hex>`, matching the checksum format used throughout the
/// manifest and frontmatter. `sha2` 0.11's digest output type doesn't
/// implement `LowerHex` the way 0.10's did, so hex-encode manually — same
/// approach `core::credential_storage::to_hex` already uses.
pub fn hash_content(content: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(content.as_bytes());
    let digest = hasher.finalize();
    let hex: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
    format!("sha256:{hex}")
}

/// `raw_<first 10 hex chars of the hash>`, matching the design doc's
/// `raw_<hash_prefix>` naming (Q1/Q3).
pub fn raw_id_for(hash: &str) -> String {
    let hex = hash.strip_prefix("sha256:").unwrap_or(hash);
    format!("raw_{}", &hex[..10.min(hex.len())])
}

pub fn is_url(source: &str) -> bool {
    source.starts_with("http://") || source.starts_with("https://")
}

/// Writes `./raw/<raw_id>.md`: YAML frontmatter (per `RawFrontmatter`)
/// followed by the raw content, sandboxed under `vault_root`. Raw blobs are
/// content-addressed and never overwritten in place — the manifest (not
/// this function) decides whether a write is even needed (see
/// `manifest::model::Manifest::record_ingest`'s no-op case).
pub fn write_raw_blob(
    vault_root: &Path,
    raw_id: &str,
    source: &str,
    tag: Option<&str>,
    checksum: &str,
    ingested_at: &str,
    content: &str,
) -> anyhow::Result<PathBuf> {
    let frontmatter = RawFrontmatter {
        okf_version: OKF_SCHEMA_VERSION.to_string(),
        r#type: "raw_source".to_string(),
        id: raw_id.to_string(),
        source_url: is_url(source).then(|| source.to_string()),
        checksum: checksum.to_string(),
        ingested_at: ingested_at.to_string(),
        tag: tag.map(str::to_string),
    };

    let yaml = serde_yaml::to_string(&frontmatter)?;
    let full_md = format!("---\n{yaml}---\n\n{content}");

    let path = sandbox_path(vault_root, &format!("raw/{raw_id}.md"))?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, full_md)?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_content_is_deterministic_and_sha256_prefixed() {
        let a = hash_content("hello");
        let b = hash_content("hello");
        assert_eq!(a, b);
        assert!(a.starts_with("sha256:"));
        assert_ne!(a, hash_content("goodbye"));
    }

    #[test]
    fn raw_id_for_takes_the_first_ten_hex_chars_after_the_prefix() {
        let hash = "sha256:abcdef0123456789";
        assert_eq!(raw_id_for(hash), "raw_abcdef0123");
    }

    #[test]
    fn is_url_recognizes_http_and_https_only() {
        assert!(is_url("https://example.com"));
        assert!(is_url("http://example.com"));
        assert!(!is_url("file:///docs/a.md"));
        assert!(!is_url("/local/path.md"));
    }

    #[test]
    fn write_raw_blob_writes_frontmatter_and_content_under_raw() {
        let vault = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(vault.path().join(".okf")).unwrap();

        let path = write_raw_blob(
            vault.path(),
            "raw_aaa",
            "https://example.com/docs",
            Some("architecture"),
            "sha256:aaa",
            "2026-07-30T18:50:00Z",
            "# Hello\n\nBody text.",
        )
        .unwrap();

        let contents = std::fs::read_to_string(&path).unwrap();
        assert!(contents.starts_with("---\n"));
        assert!(
            contents.contains("okf_version: '0.2'") || contents.contains("okf_version: \"0.2\"")
        );
        assert!(contents.contains("source_url: https://example.com/docs"));
        assert!(contents.contains("tag: architecture"));
        assert!(contents.ends_with("# Hello\n\nBody text."));
    }

    #[test]
    fn write_raw_blob_leaves_source_url_null_for_local_files() {
        let vault = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(vault.path().join(".okf")).unwrap();

        let path = write_raw_blob(
            vault.path(),
            "raw_bbb",
            "/local/legacy.md",
            None,
            "sha256:bbb",
            "t0",
            "content",
        )
        .unwrap();
        let frontmatter_yaml = std::fs::read_to_string(&path).unwrap();
        assert!(
            frontmatter_yaml.contains("source_url: null")
                || frontmatter_yaml.contains("source_url: ~")
        );
    }
}
