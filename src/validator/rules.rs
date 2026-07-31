//! `okf-mcp lint`: checks `./wiki/concepts/*.md` for dangling wikilinks,
//! missing raw-source provenance, and orphan pages, per the design doc's
//! Q1/Q2 validation table. (The design's fourth check, "Contradiction
//! Flag," is something the *compiler* writes into a page's own
//! `## Contradictions & Evolutions` section when it detects one — not a
//! separate static check this module can perform on its own.)

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use serde::Serialize;

use super::frontmatter::parse_wiki_page;
use super::wikilink::{WikiLink, extract_wikilinks};

#[derive(Debug, Default, Serialize)]
pub struct LintReport {
    /// (wiki page path, missing target slug)
    pub broken_links: Vec<(String, String)>,
    /// concept pages under `./wiki/concepts/` that no other page links to
    pub orphan_pages: Vec<String>,
    /// (wiki page path, `sources:` entry whose raw file doesn't exist)
    pub missing_sources: Vec<(String, String)>,
    /// (wiki page path, "vault::concept") — informational, not a failure
    pub cross_vault_links: Vec<(String, String)>,
}

impl LintReport {
    /// Whether this report should fail a CI/CD gate or `okf-mcp lint`'s own
    /// exit code — orphans and cross-vault references are surfaced but
    /// don't block, matching the design's "Warning" (not "Compiler Error")
    /// severity for orphan detection, and cross-vault links being a
    /// deliberate, allowed feature that's merely flagged for visibility.
    pub fn has_errors(&self) -> bool {
        !self.broken_links.is_empty() || !self.missing_sources.is_empty()
    }
}

struct ParsedConceptPage {
    /// Vault-relative path, e.g. `wiki/concepts/microservices.md`.
    relative_path: String,
    slug: String,
    sources: Vec<String>,
    links: Vec<WikiLink>,
}

/// Shared with `search::query`'s document collection — both need "every
/// `.md` file under this directory, recursively."
pub(crate) fn markdown_files_in(dir: &Path) -> anyhow::Result<Vec<PathBuf>> {
    if !dir.is_dir() {
        return Ok(Vec::new());
    }
    let mut files = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            files.extend(markdown_files_in(&path)?);
        } else if path.extension().and_then(|ext| ext.to_str()) == Some("md") {
            files.push(path);
        }
    }
    Ok(files)
}

pub fn lint_bundle(vault_root: &Path) -> anyhow::Result<LintReport> {
    let concepts_dir = vault_root.join("wiki/concepts");
    let mut report = LintReport::default();

    let mut pages = Vec::new();
    let mut existing_slugs: HashSet<String> = HashSet::new();

    for path in markdown_files_in(&concepts_dir)? {
        let relative_path = path
            .strip_prefix(vault_root)
            .unwrap_or(&path)
            .to_string_lossy()
            .replace('\\', "/");
        let slug = path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .unwrap_or_default()
            .to_string();

        let content = std::fs::read_to_string(&path)?;
        let parsed = parse_wiki_page(&content)
            .map_err(|err| anyhow::anyhow!("{relative_path}: {err}"))?;

        existing_slugs.insert(slug.clone());
        pages.push(ParsedConceptPage {
            relative_path,
            slug,
            sources: parsed
                .frontmatter
                .sources
                .iter()
                .map(|source| source.resource.clone())
                .collect(),
            links: extract_wikilinks(&parsed.body),
        });
    }

    let mut linked_slugs: HashSet<String> = HashSet::new();

    for page in &pages {
        for link in &page.links {
            match link {
                WikiLink::Local(slug) => {
                    linked_slugs.insert(slug.clone());
                    if !existing_slugs.contains(slug) {
                        report
                            .broken_links
                            .push((page.relative_path.clone(), slug.clone()));
                    }
                }
                WikiLink::CrossVault { vault, concept } => {
                    report.cross_vault_links.push((
                        page.relative_path.clone(),
                        format!("{vault}::{concept}"),
                    ));
                }
            }
        }

        for source in &page.sources {
            let relative = source.trim_start_matches('/');
            if !vault_root.join(relative).is_file() {
                report
                    .missing_sources
                    .push((page.relative_path.clone(), source.clone()));
            }
        }
    }

    for page in &pages {
        if !linked_slugs.contains(&page.slug) {
            report.orphan_pages.push(page.relative_path.clone());
        }
    }

    report.broken_links.sort();
    report.orphan_pages.sort();
    report.missing_sources.sort();
    report.cross_vault_links.sort();

    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_concept(vault_root: &Path, slug: &str, sources: &[&str], body_extra: &str) {
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
            "---\nokf_version: \"0.2\"\ntype: concept\nid: concept_{slug}\ntitle: \"{slug}\"\n{sources_yaml}---\n\n# {slug}\n\n{body_extra}\n"
        );
        std::fs::write(dir.join(format!("{slug}.md")), content).unwrap();
    }

    #[test]
    fn an_empty_vault_lints_clean() {
        let vault = tempfile::tempdir().unwrap();
        let report = lint_bundle(vault.path()).unwrap();
        assert!(!report.has_errors());
        assert!(report.orphan_pages.is_empty());
    }

    #[test]
    fn detects_a_dangling_link() {
        let vault = tempfile::tempdir().unwrap();
        write_concept(vault.path(), "a", &[], "See [[nonexistent]].");

        let report = lint_bundle(vault.path()).unwrap();
        assert!(report.has_errors());
        assert_eq!(report.broken_links.len(), 1);
        assert_eq!(report.broken_links[0].1, "nonexistent");
    }

    #[test]
    fn a_link_to_an_existing_concept_is_not_broken() {
        let vault = tempfile::tempdir().unwrap();
        write_concept(vault.path(), "a", &[], "See [[b]].");
        write_concept(vault.path(), "b", &[], "");

        let report = lint_bundle(vault.path()).unwrap();
        assert!(report.broken_links.is_empty());
    }

    #[test]
    fn detects_a_missing_provenance_source() {
        let vault = tempfile::tempdir().unwrap();
        write_concept(vault.path(), "a", &["/raw/raw_missing.md"], "");

        let report = lint_bundle(vault.path()).unwrap();
        assert!(report.has_errors());
        assert_eq!(report.missing_sources.len(), 1);
        assert_eq!(report.missing_sources[0].1, "/raw/raw_missing.md");
    }

    #[test]
    fn a_source_that_exists_on_disk_passes_provenance_audit() {
        let vault = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(vault.path().join("raw")).unwrap();
        std::fs::write(vault.path().join("raw/raw_ok.md"), "content").unwrap();
        write_concept(vault.path(), "a", &["/raw/raw_ok.md"], "");

        let report = lint_bundle(vault.path()).unwrap();
        assert!(report.missing_sources.is_empty());
    }

    #[test]
    fn detects_an_orphan_page() {
        // "linked" is referenced by "linker", so it's not an orphan. Every
        // other page here is never the *target* of a link — "lonely"
        // obviously, but "linker" too (it only links out, nothing links to
        // it), which the design's "linked by at least one other page" rule
        // correctly counts as orphaned as well.
        let vault = tempfile::tempdir().unwrap();
        write_concept(vault.path(), "linked", &[], "");
        write_concept(vault.path(), "lonely", &[], "");
        write_concept(vault.path(), "linker", &[], "See [[linked]].");

        let report = lint_bundle(vault.path()).unwrap();
        assert_eq!(
            report.orphan_pages,
            vec![
                "wiki/concepts/linker.md".to_string(),
                "wiki/concepts/lonely.md".to_string(),
            ]
        );
        assert!(!report.orphan_pages.contains(&"wiki/concepts/linked.md".to_string()));
        // Orphans are a warning, not a hard error.
        assert!(!report.has_errors());
    }

    #[test]
    fn flags_cross_vault_links_without_treating_them_as_broken() {
        let vault = tempfile::tempdir().unwrap();
        write_concept(vault.path(), "a", &[], "See [[personal::journal-entry]].");

        let report = lint_bundle(vault.path()).unwrap();
        assert!(report.broken_links.is_empty());
        assert_eq!(report.cross_vault_links.len(), 1);
        assert_eq!(report.cross_vault_links[0].1, "personal::journal-entry");
    }

    #[test]
    fn malformed_frontmatter_surfaces_as_an_error_not_a_silent_skip() {
        let vault = tempfile::tempdir().unwrap();
        let dir = vault.path().join("wiki/concepts");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("broken.md"), "no frontmatter at all").unwrap();

        assert!(lint_bundle(vault.path()).is_err());
    }
}
