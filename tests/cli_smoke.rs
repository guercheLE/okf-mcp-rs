//! Black-box smoke tests: spawns the compiled `okf-mcp` binary and asserts
//! on its actual stdout/stderr/exit code, covering the CLI surface
//! end-to-end the way a real user invokes it (unlike the unit tests inside
//! `src/`, which call business logic directly in-process).

use std::path::Path;
use std::process::{Command, Output};

/// Isolates every invocation from this machine's real `~/.okf-mcp`,
/// `~/.config/okf`, and any real credentials — without this, a developer
/// who has actually run `okf-mcp setup` for real use would get spurious
/// failures/successes here depending on their own machine's state.
fn okf_mcp(args: &[&str], home: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_okf-mcp"))
        .args(args)
        .env("HOME", home)
        .output()
        .expect("failed to spawn okf-mcp")
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn make_vault(dir: &Path) {
    std::fs::create_dir_all(dir.join(".okf")).unwrap();
}

#[test]
fn version_help_and_config_are_available() {
    let home = tempfile::tempdir().unwrap();

    let version = okf_mcp(&["version"], home.path());
    assert!(version.status.success());
    assert!(!stdout(&version).trim().is_empty());

    let help = okf_mcp(&["--help"], home.path());
    assert!(help.status.success());
    let help_text = stdout(&help);
    for command in [
        "ingest", "compile", "rebuild", "lint", "reindex", "search", "delete", "run", "vault",
        "start", "http",
    ] {
        assert!(
            help_text.contains(command),
            "--help should mention '{command}'"
        );
    }
    // The old generic-catalog commands must be gone.
    assert!(!help_text.contains("versions"));

    let config = okf_mcp(&["config"], home.path());
    assert!(config.status.success());
    let config_json: serde_json::Value = serde_json::from_str(&stdout(&config)).unwrap();
    assert_eq!(
        config_json["resolved"]["firecrawl_base_url"],
        "https://api.firecrawl.dev"
    );
}

#[test]
fn config_groups_output_by_source_and_never_leaks_credential_values() {
    let home = tempfile::tempdir().unwrap();

    let config = okf_mcp(&["config"], home.path());
    assert!(config.status.success(), "stderr: {}", stderr(&config));
    let config_json: serde_json::Value = serde_json::from_str(&stdout(&config)).unwrap();

    for key in [
        "global_config_file",
        "local_config_file",
        "env_vars",
        "vault_config",
        "credentials",
        "resolved",
    ] {
        assert!(
            config_json.get(key).is_some(),
            "expected top-level '{key}' in config output: {config_json}"
        );
    }

    // No vault resolves from an empty $HOME with no .okf/ anywhere — the
    // section should be present but null, not an error.
    assert!(config_json["vault_config"].is_null());

    // Existence flags only, never a raw secret value anywhere in the tree.
    let rendered = serde_json::to_string(&config_json).unwrap();
    assert!(!rendered.to_lowercase().contains("s3cr3t"));
    for credential in config_json["credentials"].as_array().unwrap() {
        assert!(credential.get("account").is_some());
        assert!(credential.get("saved").is_some());
        assert!(credential.get("value").is_none());
    }
}

#[test]
fn config_shows_vault_config_toml_when_a_vault_resolves() {
    let home = tempfile::tempdir().unwrap();
    let vault_dir = tempfile::tempdir().unwrap();
    make_vault(vault_dir.path());
    std::fs::write(
        vault_dir.path().join(".okf/config.toml"),
        "[compiler]\ndefault_model = \"anthropic/claude-3-5-sonnet\"\n",
    )
    .unwrap();

    let config = okf_mcp(
        &["--vault", vault_dir.path().to_str().unwrap(), "config"],
        home.path(),
    );
    assert!(config.status.success(), "stderr: {}", stderr(&config));
    let config_json: serde_json::Value = serde_json::from_str(&stdout(&config)).unwrap();

    assert_eq!(config_json["vault_config"]["exists"], true);
    assert_eq!(
        config_json["vault_config"]["contents"]["compiler"]["default_model"],
        "anthropic/claude-3-5-sonnet"
    );
}

#[test]
fn vault_add_list_and_default_round_trip() {
    let home = tempfile::tempdir().unwrap();
    let vault_dir = tempfile::tempdir().unwrap();
    make_vault(vault_dir.path());

    let add = okf_mcp(
        &[
            "vault",
            "add",
            vault_dir.path().to_str().unwrap(),
            "--name",
            "test-vault",
            "--description",
            "a smoke-test vault",
        ],
        home.path(),
    );
    assert!(add.status.success(), "stderr: {}", stderr(&add));

    let list = okf_mcp(&["vault", "list"], home.path());
    assert!(list.status.success());
    assert!(stdout(&list).contains("test-vault"));

    let default = okf_mcp(&["vault", "default", "test-vault"], home.path());
    assert!(default.status.success(), "stderr: {}", stderr(&default));

    let list_after_default = okf_mcp(&["vault", "list"], home.path());
    assert!(stdout(&list_after_default).contains("(default)"));

    let unknown_default = okf_mcp(&["vault", "default", "does-not-exist"], home.path());
    assert!(!unknown_default.status.success());

    let remove = okf_mcp(&["vault", "remove", "test-vault"], home.path());
    assert!(remove.status.success(), "stderr: {}", stderr(&remove));

    let list_after_remove = okf_mcp(&["vault", "list"], home.path());
    assert!(!stdout(&list_after_remove).contains("test-vault"));

    let remove_unknown = okf_mcp(&["vault", "rm", "does-not-exist"], home.path());
    assert!(!remove_unknown.status.success());
}

#[test]
fn vault_create_scaffolds_a_new_vault_and_registers_it() {
    let home = tempfile::tempdir().unwrap();
    let new_vault = tempfile::tempdir().unwrap();

    let create = okf_mcp(
        &[
            "vault",
            "create",
            new_vault.path().to_str().unwrap(),
            "--name",
            "created-vault",
        ],
        home.path(),
    );
    assert!(create.status.success(), "stderr: {}", stderr(&create));
    assert!(new_vault.path().join(".okf").is_dir());
    assert!(new_vault.path().join("wiki/concepts").is_dir());
    assert!(new_vault.path().join("raw").is_dir());

    let list = okf_mcp(&["vault", "list"], home.path());
    assert!(stdout(&list).contains("created-vault"));

    // Re-running `create` against the now-non-empty path fails, pointing
    // at `vault add` for adopting an existing directory instead.
    let create_again = okf_mcp(
        &[
            "vault",
            "create",
            new_vault.path().to_str().unwrap(),
            "--name",
            "created-vault-again",
        ],
        home.path(),
    );
    assert!(!create_again.status.success());
    assert!(stderr(&create_again).contains("vault add"));
}

#[test]
fn vault_delete_requires_force_and_removes_the_directory() {
    let home = tempfile::tempdir().unwrap();
    let vault_dir = tempfile::tempdir().unwrap();
    make_vault(vault_dir.path());

    let add = okf_mcp(
        &[
            "vault",
            "add",
            vault_dir.path().to_str().unwrap(),
            "--name",
            "delete-me",
        ],
        home.path(),
    );
    assert!(add.status.success(), "stderr: {}", stderr(&add));

    let delete_without_force = okf_mcp(&["vault", "delete", "delete-me"], home.path());
    assert!(!delete_without_force.status.success());
    assert!(vault_dir.path().is_dir());

    let delete_with_force = okf_mcp(&["vault", "delete", "delete-me", "--force"], home.path());
    assert!(
        delete_with_force.status.success(),
        "stderr: {}",
        stderr(&delete_with_force)
    );
    assert!(!vault_dir.path().exists());

    let list_after_delete = okf_mcp(&["vault", "list"], home.path());
    assert!(!stdout(&list_after_delete).contains("delete-me"));
}

#[test]
fn kb_is_a_full_alias_for_vault() {
    let home = tempfile::tempdir().unwrap();
    let vault_dir = tempfile::tempdir().unwrap();
    make_vault(vault_dir.path());

    let add_via_kb = okf_mcp(
        &[
            "kb",
            "add",
            vault_dir.path().to_str().unwrap(),
            "--name",
            "kb-alias-vault",
        ],
        home.path(),
    );
    assert!(
        add_via_kb.status.success(),
        "stderr: {}",
        stderr(&add_via_kb)
    );

    let kb_list = okf_mcp(&["kb", "list"], home.path());
    let vault_list = okf_mcp(&["vault", "list"], home.path());
    assert_eq!(stdout(&kb_list), stdout(&vault_list));
    assert!(stdout(&kb_list).contains("kb-alias-vault"));
}

#[test]
fn ingest_lint_reindex_search_and_delete_round_trip_a_local_file() {
    let home = tempfile::tempdir().unwrap();
    let vault_dir = tempfile::tempdir().unwrap();
    make_vault(vault_dir.path());
    let vault_arg = vault_dir.path().to_str().unwrap();

    let source = vault_dir.path().join("source.md");
    std::fs::write(
        &source,
        "# Rate Limits\n\nHow do we handle API rate limits and retries?",
    )
    .unwrap();

    let ingest = okf_mcp(
        &[
            "--vault",
            vault_arg,
            "ingest",
            source.to_str().unwrap(),
            "--tag",
            "smoke",
        ],
        home.path(),
    );
    assert!(ingest.status.success(), "stderr: {}", stderr(&ingest));
    assert!(stdout(&ingest).contains("Ingested"));

    // Re-ingesting unchanged content is a no-op.
    let reingest = okf_mcp(
        &["--vault", vault_arg, "ingest", source.to_str().unwrap()],
        home.path(),
    );
    assert!(reingest.status.success());
    assert!(stdout(&reingest).contains("No changes"));

    let lint = okf_mcp(&["--vault", vault_arg, "lint"], home.path());
    assert!(lint.status.success(), "stderr: {}", stderr(&lint));
    assert!(stdout(&lint).contains("OK"));

    let reindex = okf_mcp(&["--vault", vault_arg, "reindex"], home.path());
    assert!(reindex.status.success(), "stderr: {}", stderr(&reindex));
    assert!(stdout(&reindex).contains("Indexed"));

    let search = okf_mcp(
        &["--vault", vault_arg, "search", "rate limits"],
        home.path(),
    );
    assert!(search.status.success(), "stderr: {}", stderr(&search));
    // Raw blobs are stored under a content-hashed name, not the original
    // filename — assert on the extracted title (from the source's `# `
    // heading) and the `raw/` path prefix instead of "source.md" literally.
    let search_stdout = stdout(&search);
    assert!(search_stdout.contains("raw/raw_"));
    assert!(search_stdout.contains("Rate Limits"));

    let search_json = okf_mcp(
        &["--vault", vault_arg, "search", "rate limits", "--json"],
        home.path(),
    );
    assert!(search_json.status.success());
    let results: serde_json::Value = serde_json::from_str(&stdout(&search_json)).unwrap();
    assert!(!results.as_array().unwrap().is_empty());

    let delete = okf_mcp(
        &["--vault", vault_arg, "delete", source.to_str().unwrap()],
        home.path(),
    );
    assert!(delete.status.success(), "stderr: {}", stderr(&delete));
    assert!(stdout(&delete).contains("Tombstoned"));
}

#[test]
fn ingesting_a_missing_local_file_fails_clearly() {
    let home = tempfile::tempdir().unwrap();
    let vault_dir = tempfile::tempdir().unwrap();
    make_vault(vault_dir.path());

    let ingest = okf_mcp(
        &[
            "--vault",
            vault_dir.path().to_str().unwrap(),
            "ingest",
            "/definitely/does/not/exist.md",
        ],
        home.path(),
    );
    assert!(!ingest.status.success());
}

#[test]
fn lint_on_a_broken_wiki_reports_errors_and_exits_nonzero() {
    let home = tempfile::tempdir().unwrap();
    let vault_dir = tempfile::tempdir().unwrap();
    make_vault(vault_dir.path());
    let concepts_dir = vault_dir.path().join("wiki/concepts");
    std::fs::create_dir_all(&concepts_dir).unwrap();
    std::fs::write(
        concepts_dir.join("broken.md"),
        "---\nokf_version: \"0.2\"\ntype: concept\nid: concept_broken\ntitle: \"Broken\"\n---\n\nSee [[nonexistent]].\n",
    )
    .unwrap();

    let lint = okf_mcp(
        &["--vault", vault_dir.path().to_str().unwrap(), "lint"],
        home.path(),
    );
    assert!(!lint.status.success());
    assert!(stdout(&lint).contains("Broken links") || stdout(&lint).contains("FAILED"));

    let lint_json = okf_mcp(
        &[
            "--vault",
            vault_dir.path().to_str().unwrap(),
            "lint",
            "--json",
        ],
        home.path(),
    );
    let report: serde_json::Value = serde_json::from_str(&stdout(&lint_json)).unwrap();
    assert_eq!(report["broken_links"].as_array().unwrap().len(), 1);
}

#[test]
fn compile_without_a_configured_model_fails_with_a_clear_error() {
    let home = tempfile::tempdir().unwrap();
    let vault_dir = tempfile::tempdir().unwrap();
    make_vault(vault_dir.path());

    let compile = okf_mcp(
        &["--vault", vault_dir.path().to_str().unwrap(), "compile"],
        home.path(),
    );
    assert!(!compile.status.success());
    assert!(stderr(&compile).contains("no model specified"));
}

#[test]
fn compile_diff_dry_run_lists_active_sources_without_calling_an_llm() {
    let home = tempfile::tempdir().unwrap();
    let vault_dir = tempfile::tempdir().unwrap();
    make_vault(vault_dir.path());
    let vault_arg = vault_dir.path().to_str().unwrap();
    let source = vault_dir.path().join("source.md");
    std::fs::write(&source, "content").unwrap();

    okf_mcp(
        &["--vault", vault_arg, "ingest", source.to_str().unwrap()],
        home.path(),
    );

    let dry_run = okf_mcp(
        &[
            "--vault",
            vault_arg,
            "compile",
            "--model",
            "anthropic/claude-3-5-sonnet",
            "--diff",
        ],
        home.path(),
    );
    assert!(dry_run.status.success(), "stderr: {}", stderr(&dry_run));
    assert!(stdout(&dry_run).contains("file://"));
}

#[test]
fn http_command_serves_health_and_shuts_down_cleanly() {
    let home = tempfile::tempdir().unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_okf-mcp"))
        .args(["http", "--host", "127.0.0.1", "--port", "18765"])
        .env("HOME", home.path())
        .spawn()
        .expect("failed to spawn okf-mcp http");

    let curl_available = std::process::Command::new("curl")
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success());

    // Poll instead of a single fixed sleep: under parallel test load (many
    // subprocesses spawning at once), a fixed short sleep is flaky — the
    // server may not have bound yet by the time a single probe fires. 10s
    // total budget is generous enough that a still-failing probe after
    // that means something real, not just scheduling noise.
    let mut health = None;
    if curl_available {
        for _ in 0..50 {
            let attempt = std::process::Command::new("curl")
                .args(["-sf", "http://127.0.0.1:18765/healthz"])
                .output();
            if let Ok(output) = attempt
                && output.status.success()
            {
                health = Some(output);
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(200));
        }
    }

    child.kill().ok();
    child.wait().ok();

    if curl_available {
        let health = health.expect("/healthz never became reachable within 10s");
        let body = String::from_utf8_lossy(&health.stdout);
        assert!(body.contains("Healthy"));
    }
    // If `curl` isn't available in this environment at all, at least
    // confirm the process started and could be killed cleanly (no panic
    // above) — this test's main purpose is exercising clean startup/
    // shutdown, and the reachability check is a bonus when possible.
}
