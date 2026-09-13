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
    /// Omitted from the written YAML entirely when empty, rather than
    /// serialized as `tags: []` — matches the existing "absent, not an
    /// empty placeholder" convention `source_url: null` already sets.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
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

/// Extracts the `raw_id` embedded in a wiki page's `sources[].resource`
/// value (`/raw/raw_a1f448945f.md` today; the slugged
/// `raw_a1f448945f--my-title.md` shape phase 2 of the raw-filename change
/// introduces) — the **path → identity** direction every consumer that
/// used to string-match a literal resource path goes through instead, so a
/// physical filename gaining a slug doesn't break identity lookups.
///
/// `resource` is LLM/user-written frontmatter and never validated before
/// this runs, so it never panics on odd input: it strips a leading path, a
/// trailing `.md`, and anything after the first `--`, then hands back
/// whatever's left as the best-effort identity token — without checking
/// that it actually looks like a real `raw_id` (`resolve_raw_path` is the
/// place that answers "does this identity actually exist"). `None` only
/// for input with nothing left to extract (empty, or a path ending in a
/// separator).
pub fn raw_id_from_resource(resource: &str) -> Option<String> {
    let trimmed = resource.trim();
    let basename = trimmed.rsplit(['/', '\\']).next().unwrap_or(trimmed);
    let stem = basename.strip_suffix(".md").unwrap_or(basename);
    let candidate = stem.split_once("--").map_or(stem, |(id, _)| id).trim();
    (!candidate.is_empty()).then(|| candidate.to_string())
}

/// Same extraction as `raw_id_from_resource`, but starting from a physical
/// filename under `./raw/` rather than a `sources[].resource` string — the
/// **on-disk path → identity** direction `search::query::collect_documents`
/// and `explorer::server::node_id_for_path` need so a raw node's graph id
/// stays `raw:<raw_id>` regardless of which of the two ways it was derived.
pub fn raw_id_from_filename(path: &Path) -> Option<String> {
    let file_name = path.file_name()?.to_str()?;
    raw_id_from_resource(file_name)
}

/// Resolves a `raw_id` to its actual on-disk file under `<vault_root>/raw/`
/// — the **identity → path** direction every consumer that used to
/// construct `raw/{raw_id}.md` literally now goes through instead, so the
/// physical filename can carry a human-readable slug (phase 2 of the
/// raw-filename change) without every call site needing to know the
/// on-disk shape.
///
/// Tries the manifest's `raw_path` field first — an O(1) lookup, populated
/// at write time by `ingest::pipeline::process_ingest` once a raw blob is
/// actually written — then falls back to scanning `./raw/` for a filename
/// `raw_id_from_filename` recognizes as this `raw_id`, for manifest
/// entries recorded before that field existed (no migration needed,
/// matching the project's existing `compiled_hash`-style backward-compat
/// pattern).
///
/// The returned path is always built by joining `vault_root` directly
/// (never `vault_root.canonicalize()`-ing it first, the way `sandbox_path`
/// does internally) — `sandbox_path` is still used to *validate* the
/// manifest's recorded `raw_path` can't escape the vault, but callers that
/// go on to `strip_prefix(vault_root)` this result to recover a
/// vault-relative string need it to actually start with the exact
/// `vault_root` they passed in, which a canonicalized path isn't guaranteed
/// to do (e.g. a macOS temp dir under a `/var` -> `/private/var` symlink).
pub fn resolve_raw_path(vault_root: &Path, raw_id: &str) -> anyhow::Result<PathBuf> {
    let manifest = crate::manifest::store::load(vault_root)?;
    if let Some(raw_path) = manifest.raw_path_for(raw_id)
        && sandbox_path(vault_root, raw_path).is_ok()
    {
        let candidate = vault_root.join(raw_path);
        if candidate.is_file() {
            return Ok(candidate);
        }
    }

    let raw_dir = vault_root.join("raw");
    if raw_dir.is_dir() {
        // `filter_map(Result::ok)`, not `?`: a concurrent purge could
        // remove a file mid-scan, which isn't this function's error to
        // report — that entry just isn't a match.
        for entry in std::fs::read_dir(&raw_dir)?.filter_map(Result::ok) {
            let path = entry.path();
            if raw_id_from_filename(&path).as_deref() == Some(raw_id) {
                return Ok(path);
            }
        }
    }

    anyhow::bail!(
        "no raw file found for '{raw_id}' under '{}'",
        vault_root.display()
    )
}

/// Writes `./raw/<raw_id>.md`: YAML frontmatter (per `RawFrontmatter`)
/// followed by the raw content, sandboxed under `vault_root`. Raw blobs are
/// content-addressed and never overwritten in place — the manifest (not
/// this function) decides whether a write is even needed (see
/// `manifest::model::Manifest::record_ingest`'s no-op case).
///
/// `source` is always already a normalized URI by the time it reaches
/// here — either a real `http(s)://` URL, or (for local files) the
/// `file://<canonical absolute path>` string `ingest::pipeline::
/// normalize_local_uri` already computes and uses as the manifest key —
/// so `source_url` is populated unconditionally, verbatim, rather than
/// only for `http(s)://` sources: one identifier, not two that could
/// silently diverge.
pub fn write_raw_blob(
    vault_root: &Path,
    raw_id: &str,
    source: &str,
    tags: &[String],
    checksum: &str,
    ingested_at: &str,
    content: &str,
) -> anyhow::Result<PathBuf> {
    let frontmatter = RawFrontmatter {
        okf_version: OKF_SCHEMA_VERSION.to_string(),
        r#type: "raw_source".to_string(),
        id: raw_id.to_string(),
        source_url: Some(source.to_string()),
        checksum: checksum.to_string(),
        ingested_at: ingested_at.to_string(),
        tags: tags.to_vec(),
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
            &["architecture".to_string()],
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
        assert!(contents.contains("tags:"));
        assert!(contents.contains("- architecture"));
        assert!(contents.ends_with("# Hello\n\nBody text."));
    }

    #[test]
    fn write_raw_blob_writes_multiple_tags_as_a_list() {
        let vault = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(vault.path().join(".okf")).unwrap();

        let path = write_raw_blob(
            vault.path(),
            "raw_ccc",
            "https://example.com/docs",
            &[
                "github".to_string(),
                "repository".to_string(),
                "mcpify".to_string(),
            ],
            "sha256:ccc",
            "t0",
            "content",
        )
        .unwrap();

        let contents = std::fs::read_to_string(&path).unwrap();
        let parsed: RawFrontmatter = serde_yaml::from_str(
            contents
                .trim_start_matches("---\n")
                .split("---")
                .next()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(parsed.tags, vec!["github", "repository", "mcpify"]);
    }

    #[test]
    fn write_raw_blob_omits_tags_entirely_when_none_given() {
        let vault = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(vault.path().join(".okf")).unwrap();

        let path = write_raw_blob(
            vault.path(),
            "raw_ddd",
            "https://example.com/docs",
            &[],
            "sha256:ddd",
            "t0",
            "content",
        )
        .unwrap();

        let contents = std::fs::read_to_string(&path).unwrap();
        assert!(!contents.contains("tags:"));
    }

    #[test]
    fn raw_id_from_resource_extracts_the_bare_pre_slug_shape() {
        assert_eq!(
            raw_id_from_resource("/raw/raw_a1f448945f.md"),
            Some("raw_a1f448945f".to_string())
        );
    }

    #[test]
    fn raw_id_from_resource_extracts_the_slugged_post_slug_shape() {
        assert_eq!(
            raw_id_from_resource("/raw/raw_a1f448945f--my-cool-title.md"),
            Some("raw_a1f448945f".to_string())
        );
    }

    #[test]
    fn raw_id_from_resource_tolerates_a_missing_extension() {
        assert_eq!(
            raw_id_from_resource("/raw/raw_aaa"),
            Some("raw_aaa".to_string())
        );
    }

    #[test]
    fn raw_id_from_resource_tolerates_a_leading_dot_slash_and_no_slash_at_all() {
        assert_eq!(
            raw_id_from_resource("./raw/raw_aaa.md"),
            Some("raw_aaa".to_string())
        );
        assert_eq!(
            raw_id_from_resource("raw_aaa.md"),
            Some("raw_aaa".to_string())
        );
    }

    #[test]
    fn raw_id_from_resource_never_panics_on_malformed_input_it_just_extracts_a_basename() {
        assert_eq!(raw_id_from_resource(""), None);
        assert_eq!(raw_id_from_resource("/"), None);
        assert_eq!(
            raw_id_from_resource("/../../../etc/passwd"),
            Some("passwd".to_string())
        );
    }

    #[test]
    fn raw_id_from_filename_matches_raw_id_from_resource_on_the_same_shapes() {
        assert_eq!(
            raw_id_from_filename(Path::new("raw_aaa.md")),
            Some("raw_aaa".to_string())
        );
        assert_eq!(
            raw_id_from_filename(Path::new("/vault/raw/raw_aaa--slug.md")),
            Some("raw_aaa".to_string())
        );
    }

    #[test]
    fn resolve_raw_path_finds_a_bare_shaped_file_via_directory_scan_fallback() {
        // No manifest raw_path recorded (the pre-this-field/pre-slug case)
        // — resolution must fall back to scanning `raw/`.
        let vault = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(vault.path().join(".okf")).unwrap();
        std::fs::create_dir_all(vault.path().join("raw")).unwrap();
        std::fs::write(vault.path().join("raw/raw_aaa.md"), "content").unwrap();

        let resolved = resolve_raw_path(vault.path(), "raw_aaa").unwrap();
        assert_eq!(resolved, vault.path().join("raw/raw_aaa.md"));
    }

    #[test]
    fn resolve_raw_path_prefers_the_manifests_recorded_raw_path() {
        let vault = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(vault.path().join("raw")).unwrap();
        std::fs::write(vault.path().join("raw/raw_aaa.md"), "content").unwrap();

        let mut manifest = crate::manifest::Manifest::default();
        manifest.record_ingest("uri", "sha256:aaa", "raw_aaa", "t0");
        manifest.record_raw_path("uri", "raw_aaa", "raw/raw_aaa.md".to_string());
        crate::manifest::store::save(vault.path(), &manifest).unwrap();

        let resolved = resolve_raw_path(vault.path(), "raw_aaa").unwrap();
        assert_eq!(resolved, vault.path().join("raw/raw_aaa.md"));
    }

    #[test]
    fn resolve_raw_path_errors_when_the_raw_id_is_not_found_anywhere() {
        let vault = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(vault.path().join(".okf")).unwrap();
        assert!(resolve_raw_path(vault.path(), "raw_nonexistent").is_err());
    }

    #[test]
    fn write_raw_blob_populates_source_url_for_local_file_uris_too() {
        // `source` here is exactly the form `ingest::pipeline::
        // normalize_local_uri` produces and uses as the manifest key —
        // source_url should match it verbatim, not stay null.
        let vault = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(vault.path().join(".okf")).unwrap();

        let path = write_raw_blob(
            vault.path(),
            "raw_bbb",
            "file:///local/legacy.md",
            &[],
            "sha256:bbb",
            "t0",
            "content",
        )
        .unwrap();
        let frontmatter_yaml = std::fs::read_to_string(&path).unwrap();
        assert!(frontmatter_yaml.contains("source_url: file:///local/legacy.md"));
    }
}
