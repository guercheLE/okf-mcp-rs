//! Unit tests for the compile pipeline — moved out of `driver.rs`
//! verbatim (just de-indented one level, since this is now the module
//! body rather than an inline `mod tests { ... }`). Private to
//! `driver`, so this still reaches every stage's items via `super::*`.

use std::path::Path;

use super::*;
use crate::manifest::Manifest;
use crate::storage::fs_ops;

/// Runs `f` with `HOME` redirected to a fresh temp dir, so
/// `vault_provider_options_for_provider`'s process-level
/// `custom_providers` fallback (which reads `~/.okf-mcp/config.yml`)
/// can't pick up whatever real global config happens to exist on the
/// machine running these tests. `temp_env::with_var` serializes this
/// against every other `temp_env` call in the crate — including
/// `config_manager.rs`'s and `credential_storage.rs`'s own
/// `HOME`-mutating tests — for the whole closure.
fn with_isolated_home<T>(f: impl FnOnce(&std::path::Path) -> T) -> T {
    let dir = tempfile::tempdir().unwrap();
    temp_env::with_var("HOME", Some(dir.path()), || f(dir.path()))
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
    // `.okf/config.toml` has no `[compiler]` table at all here — serde's
    // `default_max_tokens` still fills it in, so this is always `Some`.
    assert_eq!(options.max_tokens, Some(4096));
    assert_eq!(options.concurrency, 1);
}

#[test]
fn vault_provider_options_reads_a_configured_max_tokens_value() {
    let vault = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(vault.path().join(".okf")).unwrap();
    std::fs::write(
        vault.path().join(".okf/config.toml"),
        "[compiler]\nmax_tokens = 8192\n",
    )
    .unwrap();

    let options = vault_provider_options(vault.path(), "anthropic/claude-3-5-sonnet").unwrap();
    assert_eq!(options.max_tokens, Some(8192));
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

        let options = vault_provider_options(vault.path(), "anthropic/claude-3-5-sonnet").unwrap();
        assert_eq!(options.base_url_override, None);
        assert_eq!(options.api_key_env_override, None);
    });
}

#[test]
fn vault_provider_options_is_empty_for_a_vault_with_no_config_at_all() {
    with_isolated_home(|_home| {
        let vault = tempfile::tempdir().unwrap();
        let options = vault_provider_options(vault.path(), "anthropic/claude-3-5-sonnet").unwrap();
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
fn vault_provider_options_for_provider_prefers_the_vault_entry_over_the_process_level_fallback() {
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

// New-shape entity page: no `okf_version`/`id`, an open `type:` value,
// under `wiki/entities/` — proves the two-content-directory scan
// actually reads the second directory, not just the legacy one.
fn write_entity(vault_root: &Path, slug: &str, entity_type: &str, sources: &[&str]) {
    let dir = vault_root.join("wiki/entities");
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
        "---\ntype: {entity_type}\ntitle: \"{slug}\"\ndescription: \"about {slug}\"\n{sources_yaml}---\n\n# {slug}\n"
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
fn select_sources_diff_only_also_skips_a_raw_id_referenced_only_by_an_entity_page() {
    // referenced_raw_ids' legacy-vault safety net must check
    // wiki/entities/ too, not just wiki/concepts/ — otherwise a source
    // whose only compiled output is an entity page looks unreferenced
    // and gets needlessly recompiled.
    let vault = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(vault.path().join(".okf")).unwrap();
    let mut manifest = Manifest::default();
    manifest.record_ingest("uri-a", "sha256:aaa", "raw_aaa", "t0");
    manifest.record_ingest("uri-b", "sha256:bbb", "raw_bbb", "t0");

    write_entity(
        vault.path(),
        "already-compiled-entity",
        "Person",
        &["/raw/raw_aaa.md"],
    );

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
fn regenerate_index_lists_pages_sorted_by_title_within_each_content_type_section() {
    let vault = tempfile::tempdir().unwrap();
    write_concept(vault.path(), "zebra", &[]);
    write_concept(vault.path(), "apple", &[]);
    write_entity(vault.path(), "mango", "Person", &[]);

    regenerate_index(vault.path()).unwrap();

    let index = fs_ops::read_to_string(vault.path(), "wiki/index.md").unwrap();
    // Grouped by content type (`wiki_content_dirs`'s fixed order), so
    // the whole "Concepts" section precedes "Entities" — within
    // "Concepts", apple sorts before zebra by title.
    let concepts_heading = index.find("## Concepts").unwrap();
    let entities_heading = index.find("## Entities").unwrap();
    let apple_pos = index.find("apple").unwrap();
    let zebra_pos = index.find("zebra").unwrap();
    let mango_pos = index.find("mango").unwrap();
    assert!(concepts_heading < apple_pos);
    assert!(apple_pos < zebra_pos);
    assert!(zebra_pos < entities_heading);
    assert!(entities_heading < mango_pos);
    assert!(index.contains("about apple"));
    assert!(index.contains("wiki/entities/mango.md"));
}

#[test]
fn regenerate_index_omits_a_content_type_heading_when_that_dir_has_no_pages() {
    let vault = tempfile::tempdir().unwrap();
    write_concept(vault.path(), "solo", &[]);

    regenerate_index(vault.path()).unwrap();

    let index = fs_ops::read_to_string(vault.path(), "wiki/index.md").unwrap();
    assert!(index.contains("## Concepts"));
    assert!(!index.contains("## Entities"));
    assert!(!index.contains("## Syntheses"));
    assert!(!index.contains("## Questions"));
}

#[test]
fn ensure_wiki_schema_writes_the_file_once() {
    let vault = tempfile::tempdir().unwrap();
    ensure_wiki_schema(vault.path()).unwrap();

    let schema = fs_ops::read_to_string(vault.path(), "wiki/schema.md").unwrap();
    assert!(schema.contains("wiki/concepts/"));
    assert!(schema.contains("wiki/questions/"));

    // A second call must not clobber an existing (possibly
    // hand-edited) file — `ensure_wiki_schema` only ever writes when
    // the file is absent.
    fs_ops::write(vault.path(), "wiki/schema.md", "hand-edited content").unwrap();
    ensure_wiki_schema(vault.path()).unwrap();
    let unchanged = fs_ops::read_to_string(vault.path(), "wiki/schema.md").unwrap();
    assert_eq!(unchanged, "hand-edited content");
}

#[test]
fn regenerate_index_also_writes_wiki_schema_when_missing() {
    // Every finalization path that regenerates the index must pick up
    // wiki/schema.md too — this is the one place a pre-existing vault
    // (created before this file existed) is guaranteed to get it,
    // with no separate migration step.
    let vault = tempfile::tempdir().unwrap();
    assert!(!vault.path().join("wiki/schema.md").exists());

    regenerate_index(vault.path()).unwrap();

    assert!(vault.path().join("wiki/schema.md").is_file());
}

#[test]
fn regenerate_index_declares_okf_version_in_its_own_frontmatter() {
    // Per OKF v0.2 §12, the bundle-root index.md is the one place
    // `okf_version` belongs — individual pages no longer carry it.
    let vault = tempfile::tempdir().unwrap();
    regenerate_index(vault.path()).unwrap();
    let index = fs_ops::read_to_string(vault.path(), "wiki/index.md").unwrap();
    assert_eq!(
        index.trim(),
        "---\nokf_version: \"0.2\"\n---\n\n# Wiki Index"
    );
}

#[test]
fn regenerate_index_omits_the_dash_separator_when_description_is_empty() {
    // `write_concept` above always sets a description — this covers the
    // other arm of `regenerate_index`'s `if description.is_empty()`
    // branch, which drops the "— description" suffix entirely rather
    // than rendering a dangling "— ".
    let vault = tempfile::tempdir().unwrap();
    let dir = vault.path().join("wiki/concepts");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("bare.md"),
        "---\nokf_version: \"0.2\"\ntype: concept\nid: concept_bare\ntitle: \"bare\"\ndescription: \"\"\n---\n\n# bare\n",
    )
    .unwrap();

    regenerate_index(vault.path()).unwrap();

    let index = fs_ops::read_to_string(vault.path(), "wiki/index.md").unwrap();
    assert_eq!(
        index.trim(),
        "---\nokf_version: \"0.2\"\n---\n\n# Wiki Index\n\n## Concepts\n\n- [bare](wiki/concepts/bare.md)"
            .trim()
    );
    assert!(!index.contains("—"));
}

#[test]
fn compile_options_default_concurrency_is_one_not_zero() {
    // `usize::default()` is 0, which would starve `run_bounded`'s
    // sliding window of any permits and never compile anything — the
    // manual `Default` impl must force this to 1.
    assert_eq!(CompileOptions::default().concurrency, 1);
}

/// (1) `run_compile_sources` at `concurrency == 1` must dispatch AND
/// complete every source in strict input order — the hard regression
/// requirement that raising `CompileOptions::concurrency` never changes
/// today's default (sequential) behavior. The fake `compile_fn` records
/// the `(position, total, uri)` it's called with, which happens right
/// after this function's `ProgressEvent::Started` fires for that same
/// source — so this also pins that Started/Finished pairs never
/// interleave across sources at `concurrency == 1`.
#[tokio::test]
async fn run_compile_sources_at_concurrency_one_dispatches_and_completes_in_input_order() {
    let vault = tempfile::tempdir().unwrap();

    let mut manifest = Manifest::default();
    manifest.record_ingest("uri-a", "sha256:aaa", "raw_aaa", "t0");
    manifest.record_ingest("uri-b", "sha256:bbb", "raw_bbb", "t0");
    manifest.record_ingest("uri-c", "sha256:ccc", "raw_ccc", "t0");

    let sources = vec![
        ("uri-a".to_string(), "raw_aaa".to_string()),
        ("uri-b".to_string(), "raw_bbb".to_string()),
        ("uri-c".to_string(), "raw_ccc".to_string()),
    ];

    let observed = Arc::new(std::sync::Mutex::new(Vec::new()));
    let observed_for_fn = Arc::clone(&observed);
    let compile_fn = move |position: usize, total: usize, uri: String, _raw_id: String| {
        let observed = Arc::clone(&observed_for_fn);
        async move {
            observed.lock().unwrap().push((position, total, uri));
            Ok(Vec::new())
        }
    };

    let (manifest, outcomes, _touched) =
        run_compile_sources(vault.path(), manifest, sources, 1, None, compile_fn)
            .await
            .unwrap();

    assert_eq!(
        *observed.lock().unwrap(),
        vec![
            (1, 3, "uri-a".to_string()),
            (2, 3, "uri-b".to_string()),
            (3, 3, "uri-c".to_string()),
        ],
        "concurrency == 1 must dispatch/complete in strict input order, matching the plain \
         sequential loop it replaced"
    );
    assert_eq!(outcomes.len(), 3);
    assert!(outcomes.iter().all(|o| o.error.is_none()));
    for uri in ["uri-a", "uri-b", "uri-c"] {
        assert!(manifest.is_compiled_at_current_hash(uri));
    }
}

/// (2) Under real concurrency (`concurrency > 1`), every source must
/// still end up marked in the manifest exactly once — none dropped,
/// none double-processed — proving `run_compile_sources`'s manifest
/// writes stay serialized (see its doc comment) even when its
/// `compile_fn` calls genuinely overlap in time.
#[tokio::test]
async fn run_compile_sources_under_concurrency_marks_every_source_exactly_once() {
    let vault = tempfile::tempdir().unwrap();

    const N: usize = 20;
    let mut manifest = Manifest::default();
    let mut sources = Vec::with_capacity(N);
    for i in 0..N {
        let uri = format!("uri-{i}");
        let raw_id = format!("raw_{i}");
        manifest.record_ingest(&uri, &format!("sha256:{i}"), &raw_id, "t0");
        sources.push((uri, raw_id));
    }

    let call_counts: Arc<std::sync::Mutex<std::collections::HashMap<String, usize>>> =
        Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));
    let call_counts_for_fn = Arc::clone(&call_counts);
    let compile_fn = move |_position: usize, _total: usize, uri: String, _raw_id: String| {
        let call_counts = Arc::clone(&call_counts_for_fn);
        async move {
            // Yield so tasks genuinely interleave under real
            // concurrency, rather than trivially completing
            // synchronously in dispatch order.
            tokio::task::yield_now().await;
            *call_counts.lock().unwrap().entry(uri).or_insert(0) += 1;
            Ok(Vec::new())
        }
    };

    let (manifest, outcomes, _touched) =
        run_compile_sources(vault.path(), manifest, sources, 6, None, compile_fn)
            .await
            .unwrap();

    assert_eq!(
        outcomes.len(),
        N,
        "every source must produce exactly one outcome — none dropped"
    );
    let counts = call_counts.lock().unwrap();
    assert_eq!(counts.len(), N, "no source should be dropped");
    for (uri, count) in counts.iter() {
        assert_eq!(
            *count, 1,
            "{uri} must be compiled exactly once, not {count}"
        );
    }
    for i in 0..N {
        let uri = format!("uri-{i}");
        assert!(
            manifest.is_compiled_at_current_hash(&uri),
            "{uri} must be marked compiled in the manifest"
        );
    }
}

/// A failing source doesn't stop the rest of the run, and is reported
/// with its error rather than silently dropped — same contract the old
/// sequential loop had.
#[tokio::test]
async fn run_compile_sources_reports_a_failure_without_dropping_other_sources() {
    let vault = tempfile::tempdir().unwrap();

    let mut manifest = Manifest::default();
    manifest.record_ingest("uri-ok", "sha256:aaa", "raw_aaa", "t0");
    manifest.record_ingest("uri-bad", "sha256:bbb", "raw_bbb", "t0");

    let sources = vec![
        ("uri-ok".to_string(), "raw_aaa".to_string()),
        ("uri-bad".to_string(), "raw_bbb".to_string()),
    ];

    let compile_fn = |_position: usize, _total: usize, uri: String, _raw_id: String| async move {
        if uri == "uri-bad" {
            anyhow::bail!("synthetic failure");
        }
        Ok(Vec::new())
    };

    let (manifest, outcomes, _touched) =
        run_compile_sources(vault.path(), manifest, sources, 1, None, compile_fn)
            .await
            .unwrap();

    assert_eq!(outcomes.len(), 2);
    let ok = outcomes.iter().find(|o| o.uri == "uri-ok").unwrap();
    let bad = outcomes.iter().find(|o| o.uri == "uri-bad").unwrap();
    assert!(ok.error.is_none());
    assert_eq!(bad.error.as_deref(), Some("synthetic failure"));
    assert!(manifest.is_compiled_at_current_hash("uri-ok"));
    assert!(!manifest.is_compiled_at_current_hash("uri-bad"));
}

/// A `manifest::store::save` failure must stop dispatch of every
/// not-yet-started source, matching the old sequential loop's
/// `manifest::store::save(...)?` returning immediately on the first
/// save failure — no further LLM calls (here: no further `compile_fn`
/// calls) after the failure. Forces the save to fail deterministically
/// by making `.okf` a regular file instead of a directory, so
/// `manifest::store::save`'s `create_dir_all(".okf")` errors.
#[tokio::test]
async fn run_compile_sources_stops_dispatching_after_a_manifest_save_failure() {
    let vault = tempfile::tempdir().unwrap();
    // `.okf` is a *file*, not a directory — `manifest::store::save`'s
    // `create_dir_all` will fail on every call for this vault.
    std::fs::write(vault.path().join(".okf"), "not a directory").unwrap();

    let mut manifest = Manifest::default();
    manifest.record_ingest("uri-a", "sha256:aaa", "raw_aaa", "t0");
    manifest.record_ingest("uri-b", "sha256:bbb", "raw_bbb", "t0");
    manifest.record_ingest("uri-c", "sha256:ccc", "raw_ccc", "t0");

    let sources = vec![
        ("uri-a".to_string(), "raw_aaa".to_string()),
        ("uri-b".to_string(), "raw_bbb".to_string()),
        ("uri-c".to_string(), "raw_ccc".to_string()),
    ];

    let dispatched = Arc::new(std::sync::Mutex::new(Vec::new()));
    let dispatched_for_fn = Arc::clone(&dispatched);
    let compile_fn = move |_position: usize, _total: usize, uri: String, _raw_id: String| {
        let dispatched = Arc::clone(&dispatched_for_fn);
        async move {
            dispatched.lock().unwrap().push(uri);
            Ok(Vec::new())
        }
    };

    let result = run_compile_sources(vault.path(), manifest, sources, 1, None, compile_fn).await;

    assert!(
        result.is_err(),
        "a manifest save failure must surface as an error, same as the old loop"
    );
    assert_eq!(
        *dispatched.lock().unwrap(),
        vec!["uri-a".to_string()],
        "only the first source should ever have been dispatched — the save failure on \
         uri-a must stop uri-b/uri-c from being compiled at all, matching the old \
         sequential loop's fail-fast behavior at concurrency == 1"
    );
}

/// Finds the first occurrence of `needle` in `haystack`, byte-wise —
/// used to locate the end of an HTTP request's headers (`\r\n\r\n`) in
/// [`read_http_request`] below.
fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// Reads one full HTTP/1.1 request (headers + `Content-Length` body)
/// off `stream`, looping until every declared body byte has arrived —
/// a single fixed-size `read()` (as `compiler::provider`'s own mock
/// server helper uses for its small, fixed test payloads) isn't
/// reliable here since some of the tests below deliberately send a
/// multi-hundred-KB prompt body that TCP may deliver across several
/// reads.
async fn read_http_request(stream: &mut tokio::net::TcpStream) -> String {
    use tokio::io::AsyncReadExt;

    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 65_536];
    loop {
        match stream.read(&mut chunk).await {
            Ok(0) => break,
            Ok(n) => {
                buf.extend_from_slice(&chunk[..n]);
                if let Some(header_end) = find_subslice(&buf, b"\r\n\r\n") {
                    let header_str = String::from_utf8_lossy(&buf[..header_end]);
                    let content_length: usize = header_str
                        .lines()
                        .find_map(|line| {
                            let (name, value) = line.split_once(':')?;
                            name.trim()
                                .eq_ignore_ascii_case("content-length")
                                .then(|| value.trim().parse().ok())
                                .flatten()
                        })
                        .unwrap_or(0);
                    let body_have = buf.len() - (header_end + 4);
                    if body_have >= content_length {
                        break;
                    }
                }
            }
            Err(_) => break,
        }
    }
    String::from_utf8_lossy(&buf).to_string()
}

/// Minimal HTTP/1.1 server that answers every request it receives
/// (looping `accept()` rather than the single-shot pattern
/// `compiler::provider`'s own mock server uses, since the tests below
/// drive it through the real `LLMCompilerDriver` — an OpenAI-compatible
/// `chat/completions` call per compiled source) with a fixed
/// `response_body`. When `captured` is `Some`, each request's raw text
/// (headers + body) is appended to it, in arrival order, so a test can
/// assert on exactly what prompt `compile_one_source`/`compile` sent.
async fn mock_llm_server(
    response_body: String,
    captured: Option<Arc<std::sync::Mutex<Vec<String>>>>,
) -> String {
    use tokio::io::AsyncWriteExt;
    use tokio::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                break;
            };
            let request = read_http_request(&mut stream).await;
            if let Some(sink) = &captured {
                sink.lock().unwrap().push(request);
            }
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                response_body.len(),
                response_body
            );
            let _ = stream.write_all(response.as_bytes()).await;
        }
    });
    format!("http://{address}")
}

/// An OpenAI-compatible `chat/completions` response body whose
/// `choices[0].message.content` is `payload` — `execute_compile_prompt`
/// pulls its return value straight out of that field via
/// `response.first_text()`.
fn chat_completion_response(payload: &str) -> String {
    serde_json::json!({
        "id": "chatcmpl-test",
        "object": "chat.completion",
        "created": 0,
        "model": "test-model",
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": payload},
            "finish_reason": "stop",
        }],
    })
    .to_string()
}

/// A `{"operations": [...]}` `CompilePayload` JSON string that writes
/// one concept page — `operations::parse_compile_payload`'s expected
/// shape.
fn compile_payload_creating(path: &str, content: &str) -> String {
    serde_json::json!({
        "operations": [{
            "action": "CREATE_OR_UPDATE",
            "path": path,
            "content": content,
        }],
    })
    .to_string()
}

/// Sets an env var to a dummy, meaningfully-non-blank value for the
/// duration of `fut` — used to seed the synthetic/overridden API-key
/// env var these tests' ad-hoc OpenAI-compatible provider resolves to,
/// so `resolve_provider_target`'s `resolve_api_key` short-circuits on
/// `env_var_is_meaningfully_set` and never touches the real OS
/// keychain (the exact hang this task's `--skip` flags route around
/// for other, pre-existing tests).
///
/// Takes the not-yet-polled future itself (constructing an `async`
/// block/fn call never runs its body — only polling, i.e. `.await`,
/// does) rather than a closure returning one: `async_with_vars`
/// awaiting `fut` itself, before restoring the env var, is what
/// guarantees the var is still set for the whole call, including the
/// `.await` points inside it — a closure returning the future without
/// awaiting it would let the var be restored before the future is
/// ever polled.
async fn with_dummy_api_key<T>(env_var: &str, fut: impl Future<Output = T>) -> T {
    temp_env::async_with_vars([(env_var, Some("dummy-test-key"))], fut).await
}

/// (1) The "happy path" through `compile_one_source`: a superseded raw
/// source is present (exercises `superseded_raw_id`'s `Some` arm), a
/// related wiki page genuinely matches via a real `hybrid_search` call
/// (exercises `related_wiki_pages`'s result loop, not just its empty
/// case), the assembled prompt stays under budget (no truncation), and
/// the mocked LLM's response is parsed and applied to disk. Captures
/// the literal HTTP request `compile_one_source` sent so this also
/// pins that the superseded source's content and the related page's
/// content actually made it into the prompt — not just that some
/// prompt was sent.
#[tokio::test]
async fn compile_one_source_includes_superseded_source_and_related_pages_then_applies_the_response()
{
    let vault = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(vault.path().join("raw")).unwrap();
    std::fs::write(
        vault.path().join("raw/raw_active.md"),
        "# Active Gizmo\n\nThe active gizmothingamajig revision spins twice as fast.\n",
    )
    .unwrap();
    std::fs::write(
        vault.path().join("raw/raw_prev.md"),
        "# Previous Gizmo\n\nSUPERSEDED_MARKER_TEXT gizmothingamajig revision one.\n",
    )
    .unwrap();
    write_concept(vault.path(), "gizmothingamajig", &[]);
    // BM25-index the wiki page above so `hybrid_search` (called by
    // `related_wiki_pages`) can actually find it — a fresh vault has
    // no index at all until something builds one.
    crate::search::reindex(vault.path(), false, None).unwrap();

    let response_content = compile_payload_creating(
        "wiki/concepts/gizmothingamajig.md",
        "---\nokf_version: \"0.2\"\ntype: concept\nid: concept_gizmothingamajig\ntitle: \"gizmothingamajig\"\ndescription: \"about gizmothingamajig\"\nsources:\n  - resource: \"/raw/raw_active.md\"\n---\n\n# gizmothingamajig\n",
    );
    let captured = Arc::new(std::sync::Mutex::new(Vec::new()));
    let base_url = mock_llm_server(
        chat_completion_response(&response_content),
        Some(Arc::clone(&captured)),
    )
    .await;

    let env_var = "OKF_TEST_DRIVER_KEY_HAPPY_PATH";
    let options = CompileOptions {
        api_key_env_override: Some(env_var.to_string()),
        base_url_override: Some(base_url),
        ..CompileOptions::default()
    };

    let driver = LLMCompilerDriver::new();
    let touched = with_dummy_api_key(
        env_var,
        compile_one_source(
            vault.path(),
            &driver,
            "driver-test-happy/some-model",
            &options,
            Some("raw_prev"),
            "https://example.com/gizmo",
            "raw_active",
            None,
            1,
            1,
        ),
    )
    .await
    .unwrap();

    // Compared with `ends_with` rather than an exact `PathBuf` equality
    // against `vault.path().join(...)`: on macOS `/tmp` is itself a
    // symlink to `/private/tmp`, and `sandbox_path` canonicalizes the
    // vault root, so the two otherwise-equivalent paths can differ in
    // that non-canonical prefix.
    assert_eq!(touched.len(), 1);
    assert!(touched[0].ends_with("wiki/concepts/gizmothingamajig.md"));
    assert!(
        vault
            .path()
            .join("wiki/concepts/gizmothingamajig.md")
            .is_file()
    );

    let requests = captured.lock().unwrap();
    assert_eq!(requests.len(), 1, "exactly one LLM call for one source");
    assert!(
        requests[0].contains("gizmothingamajig revision spins"),
        "the active source's own content must be in the prompt"
    );
    assert!(
        requests[0].contains("SUPERSEDED_MARKER_TEXT"),
        "the superseded source's content must be in the prompt too"
    );
    assert!(
        requests[0].contains("about gizmothingamajig")
            || requests[0].contains("# gizmothingamajig"),
        "the related wiki page hybrid_search found must be included in the prompt"
    );
}

/// (2) An active source whose content alone is bigger than
/// `DEFAULT_PROMPT_TOKEN_BUDGET` must (a) still succeed end-to-end
/// rather than sending an unbounded prompt, (b) emit the "prompt is
/// large" progress line (`on_progress: Some`), and (c) actually arrive
/// at the LLM truncated — not just estimated as oversized locally.
/// Also exercises `superseded_raw_id`'s `None` arm (no superseded
/// source at all), the opposite of test (1) above.
#[tokio::test]
async fn compile_one_source_truncates_an_oversized_active_source_and_still_succeeds() {
    let vault = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(vault.path().join("raw")).unwrap();
    // Comfortably over `DEFAULT_PROMPT_TOKEN_BUDGET` (100_000 tokens ~=
    // 400_000 chars at the `chars/4` heuristic) on its own.
    let huge_body = "x".repeat(450_000);
    std::fs::write(
        vault.path().join("raw/raw_huge.md"),
        format!("# Huge\n\n{huge_body}\n"),
    )
    .unwrap();

    let response_content = compile_payload_creating(
        "wiki/concepts/huge.md",
        "---\nokf_version: \"0.2\"\ntype: concept\nid: concept_huge\ntitle: \"huge\"\ndescription: \"about huge\"\nsources:\n  - resource: \"/raw/raw_huge.md\"\n---\n\n# huge\n",
    );
    let captured = Arc::new(std::sync::Mutex::new(Vec::new()));
    let base_url = mock_llm_server(
        chat_completion_response(&response_content),
        Some(Arc::clone(&captured)),
    )
    .await;

    let env_var = "OKF_TEST_DRIVER_KEY_OVERSIZED";
    let options = CompileOptions {
        api_key_env_override: Some(env_var.to_string()),
        base_url_override: Some(base_url),
        ..CompileOptions::default()
    };

    let driver = LLMCompilerDriver::new();
    let output = Output::cli();
    let touched = with_dummy_api_key(
        env_var,
        compile_one_source(
            vault.path(),
            &driver,
            "driver-test-oversized/some-model",
            &options,
            None,
            "https://example.com/huge",
            "raw_huge",
            Some(&output),
            1,
            1,
        ),
    )
    .await
    .unwrap();

    // See the sibling test above for why this is `ends_with` rather
    // than an exact `PathBuf` equality (macOS `/tmp` symlink
    // canonicalization).
    assert_eq!(touched.len(), 1);
    assert!(touched[0].ends_with("wiki/concepts/huge.md"));

    let requests = captured.lock().unwrap();
    assert_eq!(requests.len(), 1);
    assert!(
        requests[0].contains("truncated") && requests[0].contains("chars over budget"),
        "the oversized active source must have been truncated before it ever reached the \
         LLM, with a visible truncation marker"
    );
    assert!(
        requests[0].len() < huge_body.len(),
        "the request actually sent must be meaningfully smaller than the untruncated body"
    );
}

/// (3) `compile()` itself — the one real caller of
/// `run_compile_sources`/`compile_one_source` — reaches an LLM (here,
/// the mocked OpenAI-compatible endpoint) and applies its response,
/// with `on_progress: Some` so this also exercises
/// `run_compile_sources`'s `ProgressEvent::Started`/`Finished` output
/// branches (every other test in this module passes `on_progress:
/// None`).
#[tokio::test]
async fn compile_end_to_end_reaches_a_mocked_llm_and_applies_its_operations() {
    let vault = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(vault.path().join("raw")).unwrap();
    std::fs::write(
        vault.path().join("raw/raw_widget.md"),
        "# Widget\n\nThe widget spins.\n",
    )
    .unwrap();

    let mut manifest = Manifest::default();
    manifest.record_ingest(
        "https://example.com/widget",
        "sha256:aaa",
        "raw_widget",
        "t0",
    );
    manifest::store::save(vault.path(), &manifest).unwrap();

    let response_content = compile_payload_creating(
        "wiki/concepts/widget.md",
        "---\nokf_version: \"0.2\"\ntype: concept\nid: concept_widget\ntitle: \"widget\"\ndescription: \"about widget\"\nsources:\n  - resource: \"/raw/raw_widget.md\"\n---\n\n# widget\n",
    );
    let base_url = mock_llm_server(chat_completion_response(&response_content), None).await;

    let env_var = "OKF_TEST_DRIVER_KEY_COMPILE_E2E";
    let options = CompileOptions {
        api_key_env_override: Some(env_var.to_string()),
        base_url_override: Some(base_url),
        ..CompileOptions::default()
    };

    let output = Output::cli();
    let report = with_dummy_api_key(
        env_var,
        compile(
            vault.path(),
            "driver-test-e2e/some-model",
            false,
            &options,
            Some(&output),
        ),
    )
    .await
    .unwrap();

    assert_eq!(report.sources_processed(), 1);
    assert_eq!(report.sources_failed(), 0);
    assert!(vault.path().join("wiki/concepts/widget.md").is_file());
    assert!(
        report
            .touched_paths
            .iter()
            .any(|p| p.ends_with("wiki/concepts/widget.md"))
    );
    assert!(vault.path().join("wiki/index.md").is_file());
    assert!(vault.path().join("wiki/schema.md").is_file());

    let log = fs_ops::read_to_string(vault.path(), "wiki/log.md").unwrap();
    assert!(log.contains("[compile] ok: 1 source(s) processed, 0 failed"));

    let saved_manifest = manifest::store::load(vault.path()).unwrap();
    assert!(saved_manifest.is_compiled_at_current_hash("https://example.com/widget"));
}
