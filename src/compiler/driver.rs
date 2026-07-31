//! Orchestrates `okf-mcp compile`/`rebuild`: select raw sources -> build
//! prompt -> call the LLM -> apply operations -> lint -> regenerate index.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use crate::core::vault_config::load_vault_config;
use crate::manifest::{self, Manifest};
use crate::search::hybrid_search;
use crate::storage::fs_ops;
use crate::validator::frontmatter::parse_wiki_page;
use crate::validator::rules::markdown_files_in;
use crate::validator::{LintReport, lint_bundle};

use super::operations::{apply_operations, parse_compile_payload};
use super::prompts::{COMPILER_SYSTEM_PROMPT, RawBlob, WikiPageRef, build_compile_user_prompt};
use super::provider::LLMCompilerDriver;

#[derive(Debug, Default, Clone)]
pub struct CompileOptions {
    pub temperature: Option<f32>,
    pub base_url_override: Option<String>,
}

#[derive(Debug)]
pub struct SourceOutcome {
    pub uri: String,
    pub raw_id: String,
    pub error: Option<String>,
}

pub struct CompileReport {
    pub sources: Vec<SourceOutcome>,
    pub touched_paths: Vec<PathBuf>,
    pub lint_report: LintReport,
}

impl CompileReport {
    pub fn sources_processed(&self) -> usize {
        self.sources.iter().filter(|s| s.error.is_none()).count()
    }

    pub fn sources_failed(&self) -> usize {
        self.sources.iter().filter(|s| s.error.is_some()).count()
    }
}

/// `--model`/MCP `model` -> `.okf/config.toml`'s `[compiler].default_model`
/// -> error. No "guess from whichever env var is set" fallback (see
/// `compiler::provider`'s top comment for why).
pub fn resolve_model_spec(vault_root: &Path, explicit: Option<&str>) -> anyhow::Result<String> {
    if let Some(spec) = explicit {
        return Ok(spec.to_string());
    }
    let config = load_vault_config(vault_root)?;
    config.compiler.default_model.ok_or_else(|| {
        anyhow::anyhow!(
            "no model specified — pass --model <provider>/<model>, or set \
             [compiler].default_model in .okf/config.toml"
        )
    })
}

fn referenced_raw_ids(vault_root: &Path) -> anyhow::Result<HashSet<String>> {
    let mut ids = HashSet::new();
    for path in markdown_files_in(&vault_root.join("wiki/concepts"))? {
        let content = std::fs::read_to_string(&path)?;
        if let Ok(parsed) = parse_wiki_page(&content) {
            for source in &parsed.frontmatter.sources {
                if let Some(stem) = Path::new(&source.resource)
                    .file_stem()
                    .and_then(|stem| stem.to_str())
                {
                    ids.insert(stem.to_string());
                }
            }
        }
    }
    Ok(ids)
}

/// `diff_only` = every `ACTIVE` manifest entry not yet referenced by any
/// wiki page's `sources:`; otherwise (a `rebuild --force`) every `ACTIVE`
/// entry, regardless of whether it's already compiled.
fn select_sources(
    vault_root: &Path,
    manifest: &Manifest,
    diff_only: bool,
) -> anyhow::Result<Vec<(String, String)>> {
    let active: Vec<(String, String)> = manifest
        .active_entries()
        .map(|(uri, version)| (uri.to_string(), version.raw_id.clone()))
        .collect();
    if !diff_only {
        return Ok(active);
    }
    let referenced = referenced_raw_ids(vault_root)?;
    Ok(active
        .into_iter()
        .filter(|(_, raw_id)| !referenced.contains(raw_id))
        .collect())
}

fn read_raw_body(vault_root: &Path, raw_id: &str) -> anyhow::Result<String> {
    let content = fs_ops::read_to_string(vault_root, &format!("raw/{raw_id}.md"))?;
    let (_, body) = crate::search::query::raw_title_and_body(&content);
    Ok(body)
}

/// The `SourceVersion` immediately before the currently-active one in
/// `uri`'s history, if any — the "superseded previous source" the design's
/// diff-synthesis flow (Q5) compares against.
fn superseded_raw_id(manifest: &Manifest, uri: &str) -> Option<String> {
    let history = &manifest.sources.get(uri)?.history;
    if history.len() < 2 {
        return None;
    }
    Some(history[history.len() - 2].raw_id.clone())
}

async fn related_wiki_pages(vault_root: &Path, raw_content: &str) -> anyhow::Result<Vec<WikiPageRef>> {
    // Capped: this is a search *query*, not the full document — a huge raw
    // page would otherwise dominate the query with irrelevant tail content.
    let query: String = raw_content.chars().take(500).collect();
    let results = hybrid_search(vault_root, &query, 5)?;
    let mut pages = Vec::new();
    for result in results {
        if let Ok(content) = fs_ops::read_to_string(vault_root, &result.path) {
            pages.push(WikiPageRef {
                path: result.path,
                content,
            });
        }
    }
    Ok(pages)
}

#[allow(clippy::too_many_arguments)]
async fn compile_one_source(
    vault_root: &Path,
    driver: &LLMCompilerDriver,
    model_spec: &str,
    options: &CompileOptions,
    manifest: &Manifest,
    uri: &str,
    raw_id: &str,
) -> anyhow::Result<Vec<PathBuf>> {
    let raw_content = read_raw_body(vault_root, raw_id)?;
    let superseded = superseded_raw_id(manifest, uri).and_then(|prev_id| {
        read_raw_body(vault_root, &prev_id)
            .ok()
            .map(|content| RawBlob { id: prev_id, content })
    });

    let related = related_wiki_pages(vault_root, &raw_content).await?;
    let user_prompt = build_compile_user_prompt(
        &RawBlob {
            id: raw_id.to_string(),
            content: raw_content,
        },
        superseded.as_ref(),
        &related,
    );

    let response = driver
        .execute_compile_prompt(
            model_spec,
            COMPILER_SYSTEM_PROMPT,
            &user_prompt,
            options.temperature,
            options.base_url_override.as_deref(),
        )
        .await?;
    let payload = parse_compile_payload(&response)?;
    apply_operations(vault_root, &payload)
}

/// Deterministic table-of-contents regeneration: sorted by title, so
/// re-running `compile` without any actual content change doesn't produce
/// git diff noise from incidental ordering.
fn regenerate_index(vault_root: &Path) -> anyhow::Result<()> {
    let mut entries = Vec::new();
    for path in markdown_files_in(&vault_root.join("wiki/concepts"))? {
        let content = std::fs::read_to_string(&path)?;
        if let Ok(parsed) = parse_wiki_page(&content) {
            let relative = path
                .strip_prefix(vault_root)
                .unwrap_or(&path)
                .to_string_lossy()
                .replace('\\', "/");
            entries.push((
                parsed.frontmatter.title,
                relative,
                parsed.frontmatter.description.unwrap_or_default(),
            ));
        }
    }
    entries.sort();

    let mut markdown = String::from("# Wiki Index\n\n");
    for (title, path, description) in entries {
        if description.is_empty() {
            markdown.push_str(&format!("- [{title}]({path})\n"));
        } else {
            markdown.push_str(&format!("- [{title}]({path}) — {description}\n"));
        }
    }
    fs_ops::write(vault_root, "wiki/index.md", &markdown)?;
    Ok(())
}

/// `diff_only=true` is `okf-mcp compile`; `diff_only=false` is `okf-mcp
/// rebuild --force`. Applies each source's operations as soon as that
/// source's LLM call succeeds (per-source apply-and-continue) — one bad
/// response doesn't block the rest of the run; the post-compile `lint`
/// pass (and, upstream of this function, a git commit) are the safety net
/// that surfaces any resulting inconsistency.
pub async fn compile(
    vault_root: &Path,
    model_spec: &str,
    diff_only: bool,
    options: &CompileOptions,
) -> anyhow::Result<CompileReport> {
    let manifest = manifest::store::load(vault_root)?;
    let sources = select_sources(vault_root, &manifest, diff_only)?;

    let driver = LLMCompilerDriver::new();
    let mut outcomes = Vec::with_capacity(sources.len());
    let mut touched_paths = Vec::new();

    for (uri, raw_id) in sources {
        match compile_one_source(vault_root, &driver, model_spec, options, &manifest, &uri, &raw_id).await {
            Ok(mut paths) => {
                touched_paths.append(&mut paths);
                outcomes.push(SourceOutcome {
                    uri,
                    raw_id,
                    error: None,
                });
            }
            Err(err) => {
                outcomes.push(SourceOutcome {
                    uri,
                    raw_id,
                    error: Some(err.to_string()),
                });
            }
        }
    }

    regenerate_index(vault_root)?;
    let lint_report = lint_bundle(vault_root)?;

    Ok(CompileReport {
        sources: outcomes,
        touched_paths,
        lint_report,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_model_spec_prefers_the_explicit_flag() {
        let vault = tempfile::tempdir().unwrap();
        let spec = resolve_model_spec(vault.path(), Some("anthropic/claude-3-5-sonnet")).unwrap();
        assert_eq!(spec, "anthropic/claude-3-5-sonnet");
    }

    #[test]
    fn resolve_model_spec_falls_back_to_vault_config() {
        let vault = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(vault.path().join(".okf")).unwrap();
        std::fs::write(
            vault.path().join(".okf/config.toml"),
            "[compiler]\ndefault_model = \"groq/llama-3.3-70b\"\n",
        )
        .unwrap();

        let spec = resolve_model_spec(vault.path(), None).unwrap();
        assert_eq!(spec, "groq/llama-3.3-70b");
    }

    #[test]
    fn resolve_model_spec_errors_when_nothing_is_configured() {
        let vault = tempfile::tempdir().unwrap();
        assert!(resolve_model_spec(vault.path(), None).is_err());
    }

    fn write_concept(vault_root: &Path, slug: &str, sources: &[&str]) {
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
            "---\nokf_version: \"0.2\"\ntype: concept\nid: concept_{slug}\ntitle: \"{slug}\"\ndescription: \"about {slug}\"\n{sources_yaml}---\n\n# {slug}\n"
        );
        std::fs::write(dir.join(format!("{slug}.md")), content).unwrap();
    }

    #[test]
    fn select_sources_diff_only_skips_already_referenced_raw_ids() {
        let vault = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(vault.path().join(".okf")).unwrap();
        let mut manifest = Manifest::default();
        manifest.record_ingest("uri-a", "sha256:aaa", "raw_aaa", "t0");
        manifest.record_ingest("uri-b", "sha256:bbb", "raw_bbb", "t0");

        write_concept(vault.path(), "already-compiled", &["/raw/raw_aaa.md"]);

        let selected = select_sources(vault.path(), &manifest, true).unwrap();
        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].1, "raw_bbb");
    }

    #[test]
    fn select_sources_without_diff_only_returns_every_active_entry() {
        let vault = tempfile::tempdir().unwrap();
        let mut manifest = Manifest::default();
        manifest.record_ingest("uri-a", "sha256:aaa", "raw_aaa", "t0");
        manifest.record_ingest("uri-b", "sha256:bbb", "raw_bbb", "t0");
        write_concept(vault.path(), "already-compiled", &["/raw/raw_aaa.md"]);

        let selected = select_sources(vault.path(), &manifest, false).unwrap();
        assert_eq!(selected.len(), 2);
    }

    #[test]
    fn select_sources_excludes_tombstoned_entries() {
        let vault = tempfile::tempdir().unwrap();
        let mut manifest = Manifest::default();
        manifest.record_ingest("uri-a", "sha256:aaa", "raw_aaa", "t0");
        manifest.tombstone("uri-a", "gone", "t1").unwrap();

        let selected = select_sources(vault.path(), &manifest, false).unwrap();
        assert!(selected.is_empty());
    }

    #[test]
    fn superseded_raw_id_finds_the_entry_before_the_active_one() {
        let mut manifest = Manifest::default();
        manifest.record_ingest("uri", "sha256:aaa", "raw_aaa", "t0");
        manifest.record_ingest("uri", "sha256:bbb", "raw_bbb", "t1");

        assert_eq!(superseded_raw_id(&manifest, "uri").as_deref(), Some("raw_aaa"));
    }

    #[test]
    fn superseded_raw_id_is_none_for_a_first_time_ingest() {
        let mut manifest = Manifest::default();
        manifest.record_ingest("uri", "sha256:aaa", "raw_aaa", "t0");
        assert_eq!(superseded_raw_id(&manifest, "uri"), None);
    }

    #[test]
    fn regenerate_index_lists_pages_sorted_by_title_with_descriptions() {
        let vault = tempfile::tempdir().unwrap();
        write_concept(vault.path(), "zebra", &[]);
        write_concept(vault.path(), "apple", &[]);

        regenerate_index(vault.path()).unwrap();

        let index = fs_ops::read_to_string(vault.path(), "wiki/index.md").unwrap();
        let apple_pos = index.find("apple").unwrap();
        let zebra_pos = index.find("zebra").unwrap();
        assert!(apple_pos < zebra_pos);
        assert!(index.contains("about apple"));
    }

    #[test]
    fn regenerate_index_on_an_empty_wiki_produces_a_header_only_file() {
        let vault = tempfile::tempdir().unwrap();
        regenerate_index(vault.path()).unwrap();
        let index = fs_ops::read_to_string(vault.path(), "wiki/index.md").unwrap();
        assert_eq!(index.trim(), "# Wiki Index");
    }
}
