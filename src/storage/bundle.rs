//! Deterministic `okf.json` bundle manifest — written after a successful
//! `okf-mcp lint` pass, summarizing the vault's compiled state (design doc
//! Q1: "Validated OKF Bundle"). "Deterministic" means unchanged vault
//! content produces byte-identical output across runs (sorted arrays), so
//! committing it doesn't create diff noise from incidental ordering.

use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::core::vault_resolver::sandbox_path;
use crate::manifest;
use crate::validator::frontmatter::parse_wiki_page;
use crate::validator::rules::markdown_files_in;

const BUNDLE_OKF_VERSION: &str = "0.2";
const BUNDLE_RELATIVE_PATH: &str = "okf.json";

#[derive(Debug, Serialize)]
pub struct BundleConcept {
    pub id: String,
    pub path: String,
    pub title: String,
    pub sources: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct BundleRawSource {
    pub uri: String,
    pub raw_id: String,
    pub hash: String,
}

#[derive(Debug, Serialize)]
pub struct Bundle {
    pub okf_version: String,
    pub concepts: Vec<BundleConcept>,
    pub raw_sources: Vec<BundleRawSource>,
}

pub fn build_bundle(vault_root: &Path) -> anyhow::Result<Bundle> {
    let manifest = manifest::store::load(vault_root)?;
    let mut raw_sources: Vec<BundleRawSource> = manifest
        .active_entries()
        .map(|(uri, version)| BundleRawSource {
            uri: uri.to_string(),
            raw_id: version.raw_id.clone(),
            hash: version.hash.clone(),
        })
        .collect();
    raw_sources.sort_by(|a, b| a.uri.cmp(&b.uri));

    let mut concepts = Vec::new();
    for path in markdown_files_in(&vault_root.join("wiki/concepts"))? {
        let content = std::fs::read_to_string(&path)?;
        let Ok(parsed) = parse_wiki_page(&content) else {
            continue;
        };
        let relative = path
            .strip_prefix(vault_root)
            .unwrap_or(&path)
            .to_string_lossy()
            .replace('\\', "/");
        let mut sources: Vec<String> = parsed
            .frontmatter
            .sources
            .iter()
            .map(|source| source.resource.clone())
            .collect();
        sources.sort();
        concepts.push(BundleConcept {
            id: parsed.frontmatter.id,
            path: relative,
            title: parsed.frontmatter.title,
            sources,
        });
    }
    concepts.sort_by(|a, b| a.path.cmp(&b.path));

    Ok(Bundle {
        okf_version: BUNDLE_OKF_VERSION.to_string(),
        concepts,
        raw_sources,
    })
}

/// Writes `<vault_root>/okf.json`, returning its path so callers (e.g.
/// `storage::git::commit`) know what to stage.
pub fn write_bundle(vault_root: &Path) -> anyhow::Result<PathBuf> {
    let bundle = build_bundle(vault_root)?;
    let path = sandbox_path(vault_root, BUNDLE_RELATIVE_PATH)?;
    std::fs::write(&path, serde_json::to_string_pretty(&bundle)?)?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_wiki_page(vault_root: &Path, slug: &str, sources: &[&str]) {
        let dir = vault_root.join("wiki/concepts");
        std::fs::create_dir_all(&dir).unwrap();
        let sources_yaml = if sources.is_empty() {
            String::new()
        } else {
            let entries: String = sources
                .iter()
                .map(|s| format!("  - resource: \"{s}\"\n"))
                .collect();
            format!("sources:\n{entries}")
        };
        let content = format!(
            "---\nokf_version: \"0.2\"\ntype: concept\nid: concept_{slug}\ntitle: \"{slug}\"\n{sources_yaml}---\n\n# {slug}\n"
        );
        std::fs::write(dir.join(format!("{slug}.md")), content).unwrap();
    }

    #[test]
    fn an_empty_vault_produces_an_empty_bundle() {
        let vault = tempfile::tempdir().unwrap();
        let bundle = build_bundle(vault.path()).unwrap();
        assert!(bundle.concepts.is_empty());
        assert!(bundle.raw_sources.is_empty());
    }

    #[test]
    fn the_bundle_lists_active_raw_sources_and_compiled_concepts() {
        let vault = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(vault.path().join(".okf")).unwrap();
        let mut manifest = manifest::store::load(vault.path()).unwrap();
        manifest.record_ingest("https://example.com/spec", "sha256:aaa", "raw_aaa", "t0");
        manifest::store::save(vault.path(), &manifest).unwrap();

        write_wiki_page(vault.path(), "foo", &["/raw/raw_aaa.md"]);

        let bundle = build_bundle(vault.path()).unwrap();
        assert_eq!(bundle.raw_sources.len(), 1);
        assert_eq!(bundle.raw_sources[0].raw_id, "raw_aaa");
        assert_eq!(bundle.concepts.len(), 1);
        assert_eq!(
            bundle.concepts[0].sources,
            vec!["/raw/raw_aaa.md".to_string()]
        );
    }

    #[test]
    fn tombstoned_sources_are_excluded_from_the_bundle() {
        let vault = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(vault.path().join(".okf")).unwrap();
        let mut manifest = manifest::store::load(vault.path()).unwrap();
        manifest.record_ingest("uri", "sha256:aaa", "raw_aaa", "t0");
        manifest.tombstone("uri", "gone", "t1").unwrap();
        manifest::store::save(vault.path(), &manifest).unwrap();

        let bundle = build_bundle(vault.path()).unwrap();
        assert!(bundle.raw_sources.is_empty());
    }

    #[test]
    fn write_bundle_produces_stable_output_across_repeated_calls() {
        let vault = tempfile::tempdir().unwrap();
        write_wiki_page(vault.path(), "a", &[]);
        write_wiki_page(vault.path(), "b", &[]);

        let first_path = write_bundle(vault.path()).unwrap();
        let first = std::fs::read_to_string(&first_path).unwrap();
        let second_path = write_bundle(vault.path()).unwrap();
        let second = std::fs::read_to_string(&second_path).unwrap();

        assert_eq!(first, second);
    }
}
