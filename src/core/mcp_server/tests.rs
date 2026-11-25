//! Unit tests for the `okf-mcp` MCP tool surface — moved out of
//! `mcp_server.rs` verbatim (just de-indented one level, since this is
//! now the module body rather than an inline `mod tests { ... }`).
//! Private to `mcp_server`, so this still reaches `OkfServer`'s private
//! fields/methods and every `args`/`helpers` item via `super::*`.

use std::path::Path;
use std::time::{Duration, Instant};

use super::helpers::{SYNTHESIZE_JOB_CAP, SYNTHESIZE_JOB_TTL};
use super::*;
use crate::compiler::operations::CompileOperation;
use crate::compiler::{CompileOptions, StoredJobKind};
use crate::core::config_schema::AuthMethod;
use crate::manifest;
use crate::storage::fs_ops;
use rmcp::model::CallToolRequestParams;

#[derive(Debug, Clone, Default)]
struct TestClient;

impl rmcp::ClientHandler for TestClient {}

fn server() -> OkfServer {
    let config: Config = serde_json::from_value(serde_json::json!({
        "auth_method": "pat"
    }))
    .unwrap();
    OkfServer::new(
        config,
        Arc::new(Mutex::new(AuthManager::new(AuthMethod::Pat))),
    )
}

fn tool_json(result: &CallToolResult) -> serde_json::Value {
    let text = &result.content[0].as_text().unwrap().text;
    serde_json::from_str(text).unwrap()
}

fn write_raw(vault_root: &Path, raw_id: &str, content: &str) {
    let dir = vault_root.join("raw");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join(format!("{raw_id}.md")), content).unwrap();
}

/// Writes a real raw blob (proper `RawFrontmatter`, via `write_raw_blob`)
/// and registers it in `manifest` — `record_ingest` plus the
/// `record_raw_path` step `ingest::pipeline::process_ingest` normally
/// does right after writing — so `okf-trace-provenance`'s manifest-status
/// join has something real to find. Callers still need to
/// `manifest::store::save` the manifest themselves once done.
fn ingest_raw(
    vault_root: &Path,
    manifest: &mut manifest::Manifest,
    uri: &str,
    raw_id: &str,
    content: &str,
) {
    let hash = format!("sha256:{raw_id}");
    manifest.record_ingest(uri, &hash, raw_id, "t0");
    let path = crate::ingest::frontmatter::write_raw_blob(
        vault_root,
        raw_id,
        uri,
        &[],
        &hash,
        "t0",
        content,
    )
    .unwrap();
    // Built from the file name, not `path.strip_prefix(vault_root)` —
    // `write_raw_blob` canonicalizes the vault root internally
    // (`sandbox_path`), which can silently fail to strip on a host
    // where the vault lives under a symlink (e.g. macOS's `/var` ->
    // `/private/var`); `ingest::pipeline::process_ingest` uses the same
    // approach for the same reason.
    let raw_path = path
        .file_name()
        .map(|name| format!("raw/{}", name.to_string_lossy()))
        .unwrap_or_else(|| format!("raw/{raw_id}.md"));
    manifest.record_raw_path(uri, raw_id, raw_path);
}

fn write_wiki_page_citing(vault_root: &Path, slug: &str, extra_frontmatter: &str, resource: &str) {
    let dir = vault_root.join("wiki/concepts");
    std::fs::create_dir_all(&dir).unwrap();
    let content = format!(
        "---\ntype: concept\ntitle: \"{slug}\"\n{extra_frontmatter}sources:\n  - resource: \"{resource}\"\n---\n\n# {slug}\n"
    );
    std::fs::write(dir.join(format!("{slug}.md")), content).unwrap();
}

#[test]
fn evict_stale_and_excess_synthesize_jobs_removes_entries_past_the_ttl() {
    let mut jobs = HashMap::new();
    jobs.insert(
        "stale".to_string(),
        StoredJob {
            vault_root: PathBuf::from("/vault"),
            kind: StoredJobKind::Fix {
                slug: "x".to_string(),
            },
            created_at: Instant::now() - SYNTHESIZE_JOB_TTL - Duration::from_secs(1),
        },
    );
    jobs.insert(
        "fresh".to_string(),
        StoredJob {
            vault_root: PathBuf::from("/vault"),
            kind: StoredJobKind::Fix {
                slug: "y".to_string(),
            },
            created_at: Instant::now(),
        },
    );

    evict_stale_and_excess_synthesize_jobs(&mut jobs);

    assert!(!jobs.contains_key("stale"));
    assert!(jobs.contains_key("fresh"));
}

#[test]
fn evict_stale_and_excess_synthesize_jobs_caps_total_count_evicting_oldest_first() {
    let mut jobs = HashMap::new();
    let now = Instant::now();
    for i in 0..(SYNTHESIZE_JOB_CAP + 3) {
        jobs.insert(
            format!("job-{i}"),
            StoredJob {
                vault_root: PathBuf::from("/vault"),
                kind: StoredJobKind::Fix {
                    slug: "x".to_string(),
                },
                // Earlier index = older (created further in the past).
                created_at: now - Duration::from_secs((SYNTHESIZE_JOB_CAP + 3 - i) as u64),
            },
        );
    }

    evict_stale_and_excess_synthesize_jobs(&mut jobs);

    assert_eq!(jobs.len(), SYNTHESIZE_JOB_CAP);
    // The 3 oldest (lowest index) must be the ones evicted.
    assert!(!jobs.contains_key("job-0"));
    assert!(!jobs.contains_key("job-1"));
    assert!(!jobs.contains_key("job-2"));
    assert!(jobs.contains_key(&format!("job-{}", SYNTHESIZE_JOB_CAP + 2)));
}

#[tokio::test]
async fn synthesize_next_then_submit_compiles_a_pending_source_and_marks_it_compiled() {
    let vault = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(vault.path().join(".okf")).unwrap();
    write_raw(vault.path(), "raw_aaa", "Some content about widgets.");
    let mut manifest = manifest::Manifest::default();
    manifest.record_ingest("uri-a", "sha256:aaa", "raw_aaa", "t0");
    manifest::store::save(vault.path(), &manifest).unwrap();

    let server = server();
    let vault_path = vault.path().to_str().unwrap().to_string();

    let next = server
        .synthesize_next(Parameters(SynthesizeNextArgs {
            batch_size: 10,
            diff: None,
            vault: Some(vault_path.clone()),
        }))
        .await
        .unwrap();
    assert_eq!(next.is_error, Some(false));
    let next_body = tool_json(&next);
    assert_eq!(next_body["phase"], "compile");
    assert!(
        next_body["instructions"]
            .as_str()
            .unwrap()
            .contains("Content Routing")
    );
    let jobs = next_body["jobs"].as_array().unwrap();
    assert_eq!(jobs.len(), 1);
    let job_id = jobs[0]["job_id"].as_str().unwrap().to_string();
    assert_eq!(jobs[0]["kind"], "compile");
    assert!(jobs[0]["prompt"].as_str().unwrap().contains("widgets"));

    let submit = server
        .synthesize_submit(Parameters(SynthesizeSubmitArgs {
            results: vec![SynthesizeSubmitResult {
                job_id,
                operations: vec![CompileOperation::CreateOrUpdate {
                    path: "wiki/concepts/widgets.md".to_string(),
                    content: "---\ntype: Pattern\ntitle: \"Widgets\"\nsources:\n  - resource: \"/raw/raw_aaa.md\"\n---\n\n# Widgets\n"
                        .to_string(),
                }],
            }],
        }))
        .await
        .unwrap();
    assert_eq!(submit.is_error, Some(false));
    let submit_body = tool_json(&submit);
    let results = submit_body["results"].as_array().unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0]["status"], "applied");

    assert!(vault.path().join("wiki/concepts/widgets.md").is_file());
    let saved_manifest = manifest::store::load(vault.path()).unwrap();
    assert!(saved_manifest.is_compiled_at_current_hash("uri-a"));

    // A successful submit must regenerate wiki/index.md — the same
    // step `compiler::compile` runs at the end of its own pipeline,
    // which okf-synthesize-submit has to trigger itself since it never
    // goes through `compile()`.
    let index = fs_ops::read_to_string(vault.path(), "wiki/index.md").unwrap();
    assert!(index.contains("okf_version"));
    assert!(index.contains("Widgets"));

    // Also logged — one line per vault touched by this batch.
    let log = fs_ops::read_to_string(vault.path(), "wiki/log.md").unwrap();
    assert!(log.contains("[okf-synthesize-submit] ok: 1 job(s) applied, 0 failed"));

    // A second next call has nothing left to compile and no broken
    // links to fix — straight to done.
    let done = server
        .synthesize_next(Parameters(SynthesizeNextArgs {
            batch_size: 10,
            diff: None,
            vault: Some(vault_path),
        }))
        .await
        .unwrap();
    assert_eq!(tool_json(&done)["phase"], "done");
}

#[tokio::test]
async fn synthesize_submit_isolates_an_unknown_job_id_from_the_rest_of_the_batch() {
    let vault = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(vault.path().join(".okf")).unwrap();
    write_raw(vault.path(), "raw_aaa", "content");
    let mut manifest = manifest::Manifest::default();
    manifest.record_ingest("uri-a", "sha256:aaa", "raw_aaa", "t0");
    manifest::store::save(vault.path(), &manifest).unwrap();

    let server = server();
    let next = server
        .synthesize_next(Parameters(SynthesizeNextArgs {
            batch_size: 10,
            diff: None,
            vault: Some(vault.path().to_str().unwrap().to_string()),
        }))
        .await
        .unwrap();
    let job_id = tool_json(&next)["jobs"][0]["job_id"]
        .as_str()
        .unwrap()
        .to_string();

    let submit = server
        .synthesize_submit(Parameters(SynthesizeSubmitArgs {
            results: vec![
                SynthesizeSubmitResult {
                    job_id: "does-not-exist".to_string(),
                    operations: vec![],
                },
                SynthesizeSubmitResult {
                    job_id,
                    operations: vec![CompileOperation::CreateOrUpdate {
                        path: "wiki/concepts/a.md".to_string(),
                        content: "---\ntype: concept\ntitle: \"a\"\n---\n\n# a\n".to_string(),
                    }],
                },
            ],
        }))
        .await
        .unwrap();
    assert_eq!(submit.is_error, Some(false));
    let results = tool_json(&submit)["results"].clone();
    let results = results.as_array().unwrap();
    assert_eq!(results.len(), 2);
    assert_eq!(results[0]["status"], "error");
    assert!(
        results[0]["error"]
            .as_str()
            .unwrap()
            .contains("not found or expired")
    );
    assert_eq!(results[1]["status"], "applied");
}

#[test]
fn apply_arg_overrides_passes_vault_options_through_unchanged_except_concurrency_defaults_to_one() {
    let vault_options = CompileOptions {
        temperature: Some(0.2),
        base_url_override: Some("https://vault.example/v1".to_string()),
        api_key_env_override: Some("VAULT_KEY_ENV".to_string()),
        max_tokens: Some(4096),
        concurrency: 7, // not what vault_provider_options ever sets, but
                        // proves this field is always overwritten below
    };
    let options = apply_arg_overrides(vault_options, None, None, None);
    assert_eq!(options.temperature, Some(0.2));
    assert_eq!(
        options.base_url_override.as_deref(),
        Some("https://vault.example/v1")
    );
    assert_eq!(
        options.api_key_env_override.as_deref(),
        Some("VAULT_KEY_ENV")
    );
    assert_eq!(options.max_tokens, Some(4096));
    assert_eq!(options.concurrency, 1);
}

#[test]
fn apply_arg_overrides_lets_an_explicit_temperature_win_over_the_vault_derived_value() {
    let vault_options = CompileOptions {
        temperature: Some(0.2),
        ..CompileOptions::default()
    };
    let options = apply_arg_overrides(vault_options, Some(0.9), None, None);
    assert_eq!(options.temperature, Some(0.9));
}

#[test]
fn apply_arg_overrides_lets_an_explicit_base_url_override_win() {
    let vault_options = CompileOptions {
        base_url_override: Some("https://vault.example/v1".to_string()),
        ..CompileOptions::default()
    };
    let options = apply_arg_overrides(
        vault_options,
        None,
        Some("https://arg.example/v1".to_string()),
        None,
    );
    assert_eq!(
        options.base_url_override.as_deref(),
        Some("https://arg.example/v1")
    );
}

#[test]
fn apply_arg_overrides_uses_an_explicit_concurrency_value_instead_of_the_default() {
    let options = apply_arg_overrides(CompileOptions::default(), None, None, Some(5));
    assert_eq!(options.concurrency, 5);
}

#[test]
fn server_info_advertises_the_fixed_tool_set_not_a_discovery_layer() {
    let info = server().get_info();
    assert_eq!(info.protocol_version, ProtocolVersion::V_2024_11_05);
    assert!(info.capabilities.tools.is_some());
    let instructions = info.instructions.unwrap();
    assert!(instructions.contains("okf-search"));
    assert!(!instructions.to_lowercase().contains("semantic database"));
}

#[tokio::test]
async fn run_tool_formats_successes_and_failures_consistently() {
    let server = server();
    let success = server
        .run_tool("coverage", async { Ok(serde_json::json!({ "ok": true })) })
        .await
        .unwrap();
    assert_eq!(success.is_error, Some(false));
    let failure = server
        .run_tool("coverage", async { anyhow::bail!("coverage failure") })
        .await
        .unwrap();
    assert_eq!(failure.is_error, Some(true));
}

#[tokio::test]
async fn lint_and_search_and_list_vaults_work_against_an_empty_vault() {
    let vault = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(vault.path().join(".okf")).unwrap();
    let vault_path = vault.path().to_str().unwrap().to_string();

    let server = server();
    let lint = server
        .lint(Parameters(LintArgs {
            strict: None,
            fix: None,
            vault: Some(vault_path.clone()),
        }))
        .await
        .unwrap();
    assert_eq!(lint.is_error, Some(false));

    let search = server
        .search(Parameters(SearchArgs {
            query: "anything".to_string(),
            limit: 5,
            vault: Some(vault_path),
        }))
        .await
        .unwrap();
    assert_eq!(search.is_error, Some(false));

    let vaults = server
        .list_vaults(Parameters(ListVaultsArgs {}))
        .await
        .unwrap();
    assert_eq!(vaults.is_error, Some(false));
}

#[tokio::test]
async fn read_index_errors_clearly_when_nothing_has_been_compiled_yet() {
    let vault = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(vault.path().join(".okf")).unwrap();
    let server = server();
    let result = server
        .read_index(Parameters(ReadIndexArgs {
            vault: Some(vault.path().to_str().unwrap().to_string()),
        }))
        .await
        .unwrap();
    assert_eq!(result.is_error, Some(true));
}

#[tokio::test]
async fn read_concept_resolves_an_entity_page_by_slug_with_no_id_field() {
    // New-shape page: no `id:`/`okf_version:` frontmatter, lives under
    // wiki/entities/ (not wiki/concepts/) — proves find_concept_path's
    // filename-stem match reaches the second content directory.
    let vault = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(vault.path().join(".okf")).unwrap();
    let dir = vault.path().join("wiki/entities");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("ada-lovelace.md"),
        "---\ntype: Person\ntitle: \"Ada Lovelace\"\n---\n\n# Ada Lovelace\n",
    )
    .unwrap();

    let server = server();
    let result = server
        .read_concept(Parameters(ReadConceptArgs {
            id_or_path: "ada-lovelace".to_string(),
            vault: Some(vault.path().to_str().unwrap().to_string()),
        }))
        .await
        .unwrap();
    assert_eq!(result.is_error, Some(false));
}

#[tokio::test]
async fn trace_provenance_reports_an_active_source_and_the_pages_own_lifecycle_fields() {
    let vault = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(vault.path().join(".okf")).unwrap();

    let mut manifest = manifest::Manifest::default();
    ingest_raw(
        vault.path(),
        &mut manifest,
        "https://example.com/a",
        "raw_aaa",
        "# Doc\n\nBody.",
    );
    manifest::store::save(vault.path(), &manifest).unwrap();

    write_wiki_page_citing(
        vault.path(),
        "widgets",
        "status: draft\nstale_after: \"2026-12-31\"\ngenerated: { by: \"okf-mcp-compiler\", at: \"t0\" }\n",
        "/raw/raw_aaa.md",
    );

    let server = server();
    let result = server
        .trace_provenance(Parameters(TraceProvenanceArgs {
            id_or_path: "widgets".to_string(),
            vault: Some(vault.path().to_str().unwrap().to_string()),
        }))
        .await
        .unwrap();
    assert_eq!(result.is_error, Some(false));

    let body = tool_json(&result);
    assert_eq!(body["page"]["path"], "wiki/concepts/widgets.md");
    assert_eq!(body["page"]["status"], "draft");
    assert_eq!(body["page"]["stale_after"], "2026-12-31");
    assert_eq!(body["page"]["generated"]["by"], "okf-mcp-compiler");

    let sources = body["sources"].as_array().unwrap();
    assert_eq!(sources.len(), 1);
    assert_eq!(sources[0]["raw_id"], "raw_aaa");
    assert_eq!(sources[0]["source_url"], "https://example.com/a");
    assert_eq!(sources[0]["checksum"], "sha256:raw_aaa");
    assert_eq!(sources[0]["manifest_status"]["status"], "ACTIVE");
}

#[tokio::test]
async fn trace_provenance_reports_a_superseded_sources_replacement() {
    let vault = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(vault.path().join(".okf")).unwrap();

    let mut manifest = manifest::Manifest::default();
    ingest_raw(
        vault.path(),
        &mut manifest,
        "uri",
        "raw_aaa",
        "# Old\n\nBody.",
    );
    ingest_raw(
        vault.path(),
        &mut manifest,
        "uri",
        "raw_bbb",
        "# New\n\nBody.",
    );
    manifest::store::save(vault.path(), &manifest).unwrap();

    // Cites the now-superseded version on purpose — this is exactly the
    // case a bare raw blob's own frontmatter can't tell you about.
    write_wiki_page_citing(vault.path(), "stale-fact", "", "/raw/raw_aaa.md");

    let server = server();
    let result = server
        .trace_provenance(Parameters(TraceProvenanceArgs {
            id_or_path: "stale-fact".to_string(),
            vault: Some(vault.path().to_str().unwrap().to_string()),
        }))
        .await
        .unwrap();
    assert_eq!(result.is_error, Some(false));

    let body = tool_json(&result);
    let sources = body["sources"].as_array().unwrap();
    assert_eq!(sources[0]["manifest_status"]["status"], "SUPERSEDED");
    assert_eq!(sources[0]["manifest_status"]["by_raw_id"], "raw_bbb");
}

#[tokio::test]
async fn trace_provenance_reports_tombstoned_and_the_reason() {
    let vault = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(vault.path().join(".okf")).unwrap();

    let mut manifest = manifest::Manifest::default();
    ingest_raw(
        vault.path(),
        &mut manifest,
        "uri",
        "raw_aaa",
        "# Old\n\nBody.",
    );
    manifest.tombstone("uri", "deprecated", "t1").unwrap();
    manifest::store::save(vault.path(), &manifest).unwrap();

    write_wiki_page_citing(vault.path(), "gone", "", "/raw/raw_aaa.md");

    let server = server();
    let result = server
        .trace_provenance(Parameters(TraceProvenanceArgs {
            id_or_path: "gone".to_string(),
            vault: Some(vault.path().to_str().unwrap().to_string()),
        }))
        .await
        .unwrap();
    let body = tool_json(&result);
    let sources = body["sources"].as_array().unwrap();
    assert_eq!(sources[0]["manifest_status"]["status"], "TOMBSTONED");
    assert_eq!(sources[0]["manifest_status"]["reason"], "deprecated");
}

#[tokio::test]
async fn trace_provenance_reports_untracked_for_a_raw_file_never_ingested_via_the_manifest() {
    let vault = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(vault.path().join(".okf")).unwrap();

    // A raw blob written directly, never recorded in the manifest (e.g.
    // dropped in by hand) — must not be misreported as ACTIVE.
    crate::ingest::frontmatter::write_raw_blob(
        vault.path(),
        "raw_aaa",
        "https://example.com/a",
        &[],
        "sha256:aaa",
        "t0",
        "# Doc\n\nBody.",
    )
    .unwrap();
    write_wiki_page_citing(vault.path(), "untracked", "", "/raw/raw_aaa.md");

    let server = server();
    let result = server
        .trace_provenance(Parameters(TraceProvenanceArgs {
            id_or_path: "untracked".to_string(),
            vault: Some(vault.path().to_str().unwrap().to_string()),
        }))
        .await
        .unwrap();
    let body = tool_json(&result);
    let sources = body["sources"].as_array().unwrap();
    assert_eq!(sources[0]["manifest_status"]["status"], "UNTRACKED");
}

#[tokio::test]
async fn trace_provenance_reports_unresolvable_for_a_source_with_no_matching_raw_file() {
    let vault = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(vault.path().join(".okf")).unwrap();
    write_wiki_page_citing(vault.path(), "dangling", "", "/raw/raw_missing.md");

    let server = server();
    let result = server
        .trace_provenance(Parameters(TraceProvenanceArgs {
            id_or_path: "dangling".to_string(),
            vault: Some(vault.path().to_str().unwrap().to_string()),
        }))
        .await
        .unwrap();
    assert_eq!(result.is_error, Some(false));

    let body = tool_json(&result);
    let sources = body["sources"].as_array().unwrap();
    assert_eq!(sources[0]["raw_id"], "raw_missing");
    assert_eq!(sources[0]["manifest_status"]["status"], "UNRESOLVABLE");
}

#[tokio::test]
async fn trace_provenance_errors_clearly_for_an_unknown_page() {
    let vault = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(vault.path().join(".okf")).unwrap();

    let server = server();
    let result = server
        .trace_provenance(Parameters(TraceProvenanceArgs {
            id_or_path: "does-not-exist".to_string(),
            vault: Some(vault.path().to_str().unwrap().to_string()),
        }))
        .await
        .unwrap();
    assert_eq!(result.is_error, Some(true));
}

#[tokio::test]
async fn delete_of_a_never_ingested_source_is_a_tool_error_not_a_panic() {
    let vault = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(vault.path().join(".okf")).unwrap();
    let server = server();
    let result = server
        .delete(Parameters(DeleteArgs {
            source: "https://example.com/never-ingested".to_string(),
            purge: None,
            vault: Some(vault.path().to_str().unwrap().to_string()),
        }))
        .await
        .unwrap();
    assert_eq!(result.is_error, Some(true));
}

// Wrapped in an overall timeout: a hang anywhere below (e.g. the
// in-process server task panicking mid-request, which leaves the
// client awaiting a response that will never arrive) would otherwise
// run until CI's own job-level timeout killed it, hours later.
#[tokio::test]
async fn mcp_protocol_exposes_the_full_hyphenated_tool_set() {
    tokio::time::timeout(
        std::time::Duration::from_secs(30),
        mcp_protocol_exposes_the_full_hyphenated_tool_set_inner(),
    )
    .await
    .expect("mcp protocol test timed out after 30s — the server task likely panicked mid-request");
}

async fn mcp_protocol_exposes_the_full_hyphenated_tool_set_inner() {
    let (server_transport, client_transport) = tokio::io::duplex(64 * 1024);
    let server_task = tokio::spawn(async move {
        server().serve(server_transport).await?.waiting().await?;
        anyhow::Ok(())
    });
    let client = TestClient.serve(client_transport).await.unwrap();

    let mut tools: Vec<String> = client
        .list_all_tools()
        .await
        .unwrap()
        .iter()
        .map(|tool| tool.name.to_string())
        .collect();
    tools.sort();
    assert_eq!(
        tools,
        vec![
            "okf-compile",
            "okf-delete",
            "okf-explore",
            "okf-ingest",
            "okf-lint",
            "okf-list-models",
            "okf-list-vaults",
            "okf-read-concept",
            "okf-read-index",
            "okf-rebuild",
            "okf-reindex",
            "okf-search",
            "okf-synthesize-next",
            "okf-synthesize-submit",
            "okf-trace-provenance",
        ]
    );

    let vault = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(vault.path().join(".okf")).unwrap();

    let lint = client
        .call_tool(
            CallToolRequestParams::new("okf-lint").with_arguments(
                serde_json::json!({ "vault": vault.path().to_str().unwrap() })
                    .as_object()
                    .unwrap()
                    .clone(),
            ),
        )
        .await
        .unwrap();
    assert_eq!(lint.is_error, Some(false));

    drop(client);
    tokio::time::timeout(std::time::Duration::from_secs(2), server_task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}
