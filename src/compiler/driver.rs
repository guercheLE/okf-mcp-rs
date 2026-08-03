//! Orchestrates `okf-mcp compile`/`rebuild`: select raw sources -> build
//! prompt -> call the LLM -> apply operations -> lint -> regenerate index.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use crate::core::output::{Output, ProgressEvent};
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
    pub api_key_env_override: Option<String>,
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

/// Resolves `.okf/config.toml`'s `[providers.<provider>]` entry (if any)
/// for `model_spec`'s provider into a `CompileOptions` override — the
/// vault-level counterpart to `--model`/MCP `base_url_override`/etc.
/// `provider_spec`'s own hardcoded env-var names/defaults still apply
/// whenever the vault has no matching `[providers.<name>]` entry (returns
/// `CompileOptions::default()` in that case, today's exact behavior).
pub fn vault_provider_options(
    vault_root: &Path,
    model_spec: &str,
) -> anyhow::Result<CompileOptions> {
    let (provider, _) = super::provider::parse_model_spec(model_spec)?;
    vault_provider_options_for_provider(vault_root, provider)
}

/// Same lookup as `vault_provider_options`, but keyed directly on a
/// provider name rather than a full `<provider>/<model>` spec — for
/// callers (e.g. `okf-mcp models <provider>`) that don't have a model name
/// on hand yet, since listing available models is exactly how a user picks
/// one.
///
/// Falls back to the process-level `custom_providers` map (populated by
/// `okf-mcp setup`'s `.env`/config.yml persistence — see
/// `config_manager::env_overrides`/`Config::custom_providers`) when the
/// vault's own `.okf/config.toml` has no matching `[providers.<name>]`
/// entry — vault-level config stays higher precedence (more specific),
/// matching this codebase's existing CLI > env > local > global pattern.
pub fn vault_provider_options_for_provider(
    vault_root: &Path,
    provider: &str,
) -> anyhow::Result<CompileOptions> {
    let vault_config = load_vault_config(vault_root)?;
    let provider_config = vault_config.providers.get(provider);

    let mut base_url_override = provider_config.and_then(|p| p.base_url.clone());
    let api_key_env_override = provider_config.and_then(|p| p.api_key_env.clone());

    if base_url_override.is_none() {
        // Lenient: a malformed process config.yml shouldn't block a
        // resolution that's otherwise fully specified by the vault.
        base_url_override = crate::core::config_manager::load_config(serde_json::Map::new())
            .ok()
            .and_then(|config| {
                config
                    .custom_providers
                    .get(provider)
                    .map(|e| e.base_url.clone())
            });
    }

    Ok(CompileOptions {
        temperature: None,
        base_url_override,
        api_key_env_override,
    })
}

/// Lists a provider's available model names — the same lookup
/// `execute_compile_prompt`'s model-not-found hint uses, exposed directly
/// as its own operation for `okf-mcp models <provider>`.
pub async fn list_models(
    provider: &str,
    base_url_override: Option<&str>,
    api_key_env_override: Option<&str>,
) -> anyhow::Result<Vec<String>> {
    super::provider::LLMCompilerDriver::new()
        .list_models(provider, base_url_override, api_key_env_override)
        .await
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

/// `diff_only` = every `ACTIVE` manifest entry that's neither (a) already
/// referenced by some wiki page's `sources:` — a safety net for vaults
/// compiled before `compiled_hash` existed, where the wiki already
/// reflects a source but the manifest doesn't know it yet — nor (b)
/// marked `compiled_hash == active_hash` in the manifest, the
/// authoritative "fully compiled at its current content" signal
/// (`Manifest::is_compiled_at_current_hash`). Otherwise (a `rebuild
/// --force`) every `ACTIVE` entry, regardless of either check.
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
        .filter(|(uri, raw_id)| {
            !referenced.contains(raw_id) && !manifest.is_compiled_at_current_hash(uri)
        })
        .collect())
}

pub(crate) fn read_raw_body(vault_root: &Path, raw_id: &str) -> anyhow::Result<String> {
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

async fn related_wiki_pages(
    vault_root: &Path,
    raw_content: &str,
) -> anyhow::Result<Vec<WikiPageRef>> {
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
            .map(|content| RawBlob {
                id: prev_id,
                content,
            })
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
            options.api_key_env_override.as_deref(),
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
    on_progress: Option<&Output>,
) -> anyhow::Result<CompileReport> {
    let mut manifest = manifest::store::load(vault_root)?;
    let sources = select_sources(vault_root, &manifest, diff_only)?;

    let driver = LLMCompilerDriver::new();
    let total = sources.len();
    let mut outcomes = Vec::with_capacity(total);
    let mut touched_paths = Vec::new();

    for (index, (uri, raw_id)) in sources.into_iter().enumerate() {
        let position = index + 1;
        if let Some(output) = on_progress {
            output.line(
                &ProgressEvent::Started {
                    index: position,
                    total,
                    label: format!("compiling {uri}"),
                }
                .to_string(),
            );
        }
        match compile_one_source(
            vault_root, &driver, model_spec, options, &manifest, &uri, &raw_id,
        )
        .await
        {
            Ok(mut paths) => {
                touched_paths.append(&mut paths);
                // Marked and saved immediately, per source — not batched
                // until the end of the loop — so a crash/kill partway
                // through a run leaves already-succeeded sources durably
                // resumable on the next `compile`. Independent of
                // `report_and_commit`'s later pass/fail gate: manifest.json
                // is never part of the git-staged path list, so it must
                // keep persisting regardless of whether the overall run
                // later gets treated as failed.
                manifest.mark_compiled(&uri);
                manifest::store::save(vault_root, &manifest)?;
                if let Some(output) = on_progress {
                    output.line(
                        &ProgressEvent::Finished {
                            index: position,
                            total,
                            label: format!("compiling {uri}"),
                            error: None,
                        }
                        .to_string(),
                    );
                }
                outcomes.push(SourceOutcome {
                    uri,
                    raw_id,
                    error: None,
                });
            }
            Err(err) => {
                if let Some(output) = on_progress {
                    output.line(
                        &ProgressEvent::Finished {
                            index: position,
                            total,
                            label: format!("compiling {uri}"),
                            error: Some(err.to_string()),
                        }
                        .to_string(),
                    );
                }
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
    use crate::core::credential_storage::HOME_ENV_TEST_LOCK;

    /// Runs `f` with `HOME` redirected to a fresh temp dir, so
    /// `vault_provider_options_for_provider`'s process-level
    /// `custom_providers` fallback (which reads `~/.okf-mcp/config.yml`)
    /// can't pick up whatever real global config happens to exist on the
    /// machine running these tests. Serialized via the same
    /// `HOME_ENV_TEST_LOCK` `config_manager.rs`'s and
    /// `credential_storage.rs`'s own `HOME`-mutating tests already share.
    fn with_isolated_home<T>(f: impl FnOnce(&std::path::Path) -> T) -> T {
        let _guard = HOME_ENV_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let prev_home = std::env::var_os("HOME");
        // SAFETY: test-only env mutation, serialized by HOME_ENV_TEST_LOCK.
        unsafe {
            std::env::set_var("HOME", dir.path());
        }
        let result = f(dir.path());
        // SAFETY: same guard as above.
        unsafe {
            match prev_home {
                Some(prev) => std::env::set_var("HOME", prev),
                None => std::env::remove_var("HOME"),
            }
        }
        result
    }

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

    #[test]
    fn vault_provider_options_reads_the_matching_providers_base_url_and_api_key_env() {
        let vault = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(vault.path().join(".okf")).unwrap();
        std::fs::write(
            vault.path().join(".okf/config.toml"),
            "[providers.custom]\napi_key_env = \"MY_CUSTOM_KEY\"\nbase_url = \"http://localhost:9999\"\n",
        )
        .unwrap();

        let options = vault_provider_options(vault.path(), "custom/some-model").unwrap();
        assert_eq!(
            options.base_url_override.as_deref(),
            Some("http://localhost:9999")
        );
        assert_eq!(
            options.api_key_env_override.as_deref(),
            Some("MY_CUSTOM_KEY")
        );
        assert_eq!(options.temperature, None);
    }

    #[test]
    fn vault_provider_options_is_empty_for_a_provider_with_no_matching_entry() {
        with_isolated_home(|_home| {
            let vault = tempfile::tempdir().unwrap();
            std::fs::create_dir_all(vault.path().join(".okf")).unwrap();
            std::fs::write(
                vault.path().join(".okf/config.toml"),
                "[providers.custom]\napi_key_env = \"MY_CUSTOM_KEY\"\n",
            )
            .unwrap();

            let options =
                vault_provider_options(vault.path(), "anthropic/claude-3-5-sonnet").unwrap();
            assert_eq!(options.base_url_override, None);
            assert_eq!(options.api_key_env_override, None);
        });
    }

    #[test]
    fn vault_provider_options_is_empty_for_a_vault_with_no_config_at_all() {
        with_isolated_home(|_home| {
            let vault = tempfile::tempdir().unwrap();
            let options =
                vault_provider_options(vault.path(), "anthropic/claude-3-5-sonnet").unwrap();
            assert_eq!(options.base_url_override, None);
            assert_eq!(options.api_key_env_override, None);
        });
    }

    #[test]
    fn vault_provider_options_for_provider_falls_back_to_the_process_level_custom_providers_map() {
        with_isolated_home(|home| {
            std::fs::create_dir_all(home.join(".okf-mcp")).unwrap();
            std::fs::write(
                home.join(".okf-mcp/config.yml"),
                "custom_providers:\n  myvllm:\n    base_url: https://global.example/v1\n",
            )
            .unwrap();

            // A vault with no `.okf/config.toml` at all — nothing to
            // shadow the process-level fallback.
            let vault = tempfile::tempdir().unwrap();
            let options = vault_provider_options_for_provider(vault.path(), "myvllm").unwrap();
            assert_eq!(
                options.base_url_override.as_deref(),
                Some("https://global.example/v1")
            );
        });
    }

    #[test]
    fn vault_provider_options_for_provider_prefers_the_vault_entry_over_the_process_level_fallback()
    {
        with_isolated_home(|home| {
            std::fs::create_dir_all(home.join(".okf-mcp")).unwrap();
            std::fs::write(
                home.join(".okf-mcp/config.yml"),
                "custom_providers:\n  myvllm:\n    base_url: https://global.example/v1\n",
            )
            .unwrap();

            let vault = tempfile::tempdir().unwrap();
            std::fs::create_dir_all(vault.path().join(".okf")).unwrap();
            std::fs::write(
                vault.path().join(".okf/config.toml"),
                "[providers.myvllm]\nbase_url = \"https://vault.example/v1\"\n",
            )
            .unwrap();

            let options = vault_provider_options_for_provider(vault.path(), "myvllm").unwrap();
            assert_eq!(
                options.base_url_override.as_deref(),
                Some("https://vault.example/v1")
            );
        });
    }

    #[test]
    fn vault_provider_options_for_provider_reads_the_same_entry_by_bare_provider_name() {
        // The `okf-mcp models <provider>` path doesn't have a model name to
        // extract a provider from — it calls this directly instead of
        // going through `vault_provider_options`'s `<provider>/<model>`
        // split, and must resolve to the same override.
        let vault = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(vault.path().join(".okf")).unwrap();
        std::fs::write(
            vault.path().join(".okf/config.toml"),
            "[providers.custom]\napi_key_env = \"MY_CUSTOM_KEY\"\nbase_url = \"http://localhost:9999\"\n",
        )
        .unwrap();

        let options = vault_provider_options_for_provider(vault.path(), "custom").unwrap();
        assert_eq!(
            options.base_url_override.as_deref(),
            Some("http://localhost:9999")
        );
        assert_eq!(
            options.api_key_env_override.as_deref(),
            Some("MY_CUSTOM_KEY")
        );
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
    fn select_sources_diff_only_also_skips_sources_already_compiled_at_the_current_hash() {
        let vault = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(vault.path().join(".okf")).unwrap();
        let mut manifest = Manifest::default();
        manifest.record_ingest("uri-a", "sha256:aaa", "raw_aaa", "t0");
        manifest.record_ingest("uri-b", "sha256:bbb", "raw_bbb", "t0");
        // Marked compiled but with NO wiki page written for it — only the
        // new compiled_hash check (not the older referenced-raw-ids scan)
        // can catch this one.
        manifest.mark_compiled("uri-a");

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

        assert_eq!(
            superseded_raw_id(&manifest, "uri").as_deref(),
            Some("raw_aaa")
        );
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
