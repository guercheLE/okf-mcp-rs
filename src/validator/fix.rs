//! Mechanical auto-repair for lint findings that have exactly one correct
//! fix with no judgment call: currently just a `tid:` frontmatter typo (see
//! `fix_id_field_typos`). A `sources[].resource` value missing its `.md`
//! extension *used* to be this module's other fix, back when
//! `missing_sources` resolved a literal filesystem path — now that
//! resolution goes through `raw_id_from_resource`/`resolve_raw_path`
//! (identity, not exact filename), such a resource already resolves on its
//! own and is never reported as missing in the first place, so there's
//! nothing left here to mechanically repair for it. Broken links and orphan
//! pages are never touched here either — creating/renaming a concept page
//! is a content decision, not a mechanical repair (see `compiler::link_fix`
//! for the LLM-assisted counterpart that handles broken links, gated behind
//! `compile`/`rebuild` since it needs a model and `lint` deliberately never
//! does).

use std::path::Path;

use serde::Serialize;

use crate::core::vault_resolver::wiki_content_dirs;

use super::frontmatter::parse_wiki_page;
use super::rules::LintReport;
use super::rules::{lint_bundle, markdown_files_in};

#[derive(Debug, Clone, Default, Serialize)]
pub struct FixReport {
    /// wiki page paths where a `tid:` frontmatter field was renamed to `id:`
    pub fixed_frontmatter_typos: Vec<String>,
}

impl FixReport {
    pub fn is_empty(&self) -> bool {
        self.fixed_frontmatter_typos.is_empty()
    }
}

/// A page whose frontmatter fails to parse makes `lint_bundle` itself
/// return `Err` (see `frontmatter::parse_wiki_page`'s doc comment — a
/// deliberate "surface as an error" design, not a silent skip), which
/// would otherwise prevent this whole fix pass — including the unrelated
/// missing-sources fix below — from running at all over the rest of the
/// vault. This targets one known, observed LLM-compiler typo: `tid:`
/// emitted where `id:` was meant. `id` is now optional
/// (`#[serde(default)]`), so this typo alone no longer breaks parsing on
/// its own — this fix now mostly matters for older vaults compiled before
/// that change, or a page that fails to parse for some other reason
/// alongside the typo. Only touches pages that currently fail to parse,
/// only rewrites the `tid:` line inside the frontmatter block (never the
/// body), and verifies the rewrite actually fixes parsing before keeping
/// it — otherwise the file is left untouched for a human/LLM to look at.
fn fix_id_field_typos(vault_root: &Path) -> anyhow::Result<Vec<String>> {
    let mut fixed_pages = Vec::new();

    let mut content_paths = Vec::new();
    for dir in wiki_content_dirs(vault_root) {
        content_paths.extend(markdown_files_in(&dir)?);
    }

    for path in content_paths {
        let content = std::fs::read_to_string(&path)?;
        if parse_wiki_page(&content).is_ok() {
            continue; // already valid; not this class of bug
        }

        let Some(after_open) = content.strip_prefix("---\n") else {
            continue;
        };
        let Some(close_at) = after_open.find("\n---\n") else {
            continue;
        };
        let yaml = &after_open[..close_at];
        if yaml.lines().any(|line| line.starts_with("id:")) {
            continue; // `id:` already present — the parse failure is something else
        }
        let Some(tid_line) = yaml.lines().find(|line| line.starts_with("tid:")) else {
            continue;
        };

        let fixed_line = format!("id:{}", &tid_line["tid:".len()..]);
        let fixed_yaml = yaml.replacen(tid_line, &fixed_line, 1);
        let body = &after_open[close_at + "\n---\n".len()..];
        let new_content = format!("---\n{fixed_yaml}\n---\n{body}");

        if parse_wiki_page(&new_content).is_ok() {
            std::fs::write(&path, &new_content)?;
            let relative = path
                .strip_prefix(vault_root)
                .unwrap_or(&path)
                .to_string_lossy()
                .replace('\\', "/");
            fixed_pages.push(relative);
        }
        // If the rewrite still doesn't parse, some other problem exists
        // alongside the typo — leave the file untouched.
    }

    Ok(fixed_pages)
}

/// Fixes known frontmatter field typos (currently: `tid:` -> `id:`), then
/// re-lints so the returned `LintReport` is authoritative post-fix state.
pub fn fix_bundle(vault_root: &Path) -> anyhow::Result<(FixReport, LintReport)> {
    let fix_report = FixReport {
        fixed_frontmatter_typos: fix_id_field_typos(vault_root)?,
    };

    let post_fix_report = lint_bundle(vault_root)?;
    Ok((fix_report, post_fix_report))
}

pub fn summary_line(report: &FixReport) -> String {
    if report.is_empty() {
        return "No auto-fixable issues found.".to_string();
    }
    let mut lines = Vec::new();
    if !report.fixed_frontmatter_typos.is_empty() {
        lines.push(format!(
            "Fixed {} frontmatter typo(s) (tid: -> id:):",
            report.fixed_frontmatter_typos.len()
        ));
        for page in &report.fixed_frontmatter_typos {
            lines.push(format!("  {page}"));
        }
    }
    lines.join("\n")
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

    fn write_raw(vault_root: &Path, raw_id: &str) {
        let dir = vault_root.join("raw");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(format!("{raw_id}.md")), "content").unwrap();
    }

    #[test]
    fn a_tid_typo_alone_no_longer_needs_fixing_since_id_is_optional() {
        // Regression guard for the OKF v0.2 compliance change that made
        // `id:` optional: a `tid:` typo (missing the real `id:` field) used
        // to break this page's parsing outright, which is what
        // `fix_id_field_typos` existed to repair. Now the page parses fine
        // on its own — `tid:` is just an ignored extra key — so there's
        // nothing to fix, and the file must be left byte-for-byte untouched.
        let vault = tempfile::tempdir().unwrap();
        let dir = vault.path().join("wiki/concepts");
        std::fs::create_dir_all(&dir).unwrap();
        let original = "---\ntype: concept\ntid: concept_bad\ntitle: \"bad\"\n---\n\n# bad\n";
        std::fs::write(dir.join("bad.md"), original).unwrap();
        write_concept(vault.path(), "a", &[], "");

        let (fix_report, post_fix) = fix_bundle(vault.path()).unwrap();

        assert!(fix_report.fixed_frontmatter_typos.is_empty());
        assert!(!post_fix.has_errors());

        let content = std::fs::read_to_string(dir.join("bad.md")).unwrap();
        assert_eq!(content, original);
    }

    #[test]
    fn a_page_with_a_genuinely_unparseable_id_line_is_left_untouched() {
        let vault = tempfile::tempdir().unwrap();
        let dir = vault.path().join("wiki/concepts");
        std::fs::create_dir_all(&dir).unwrap();
        // `tid:` present, but the rest of the YAML is still broken (missing
        // closing delimiter) — renaming `tid` to `id` alone won't fix it,
        // so the rewrite must not be kept.
        std::fs::write(dir.join("bad.md"), "---\ntid: concept_bad\n").unwrap();

        assert!(fix_bundle(vault.path()).is_err());
        let content = std::fs::read_to_string(dir.join("bad.md")).unwrap();
        assert!(content.contains("tid:"));
    }

    #[test]
    fn a_resource_missing_the_dot_md_extension_already_resolves_so_theres_nothing_to_fix() {
        // `missing_sources` (validator::rules) now resolves `sources:`
        // entries by `raw_id`, not literal filename — a resource missing
        // its `.md` extension already resolves on its own, so this module
        // never sees it as something to mechanically repair.
        let vault = tempfile::tempdir().unwrap();
        write_raw(vault.path(), "raw_aaa");
        write_concept(vault.path(), "a", &["/raw/raw_aaa"], "");

        let (fix_report, post_fix) = fix_bundle(vault.path()).unwrap();

        assert!(fix_report.is_empty());
        assert!(post_fix.missing_sources.is_empty());

        // The file is left byte-for-byte untouched — there was nothing to fix.
        let content = std::fs::read_to_string(vault.path().join("wiki/concepts/a.md")).unwrap();
        assert!(content.contains("resource: \"/raw/raw_aaa\""));
    }

    #[test]
    fn leaves_a_genuinely_missing_raw_file_alone() {
        let vault = tempfile::tempdir().unwrap();
        write_concept(vault.path(), "a", &["/raw/raw_missing"], "");

        let (fix_report, post_fix) = fix_bundle(vault.path()).unwrap();

        assert!(fix_report.is_empty());
        assert_eq!(post_fix.missing_sources.len(), 1);
    }

    #[test]
    fn is_idempotent_when_nothing_is_fixable() {
        let vault = tempfile::tempdir().unwrap();
        write_concept(vault.path(), "a", &[], "");

        let (fix_report, post_fix) = fix_bundle(vault.path()).unwrap();

        assert!(fix_report.is_empty());
        assert!(!post_fix.has_errors());

        // second run is a no-op
        let (fix_report_again, _) = fix_bundle(vault.path()).unwrap();
        assert!(fix_report_again.is_empty());
    }
}
