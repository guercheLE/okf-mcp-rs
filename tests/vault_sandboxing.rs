//! Integration-level coverage of vault resolution + path sandboxing acting
//! together through the public `okf_mcp::core` API (see
//! `src/core/vault_resolver.rs`'s own unit tests for the pure-function
//! cases in isolation).

use okf_mcp::core::vault_resolver::sandbox_path;

#[test]
fn a_two_vault_setup_never_lets_one_vaults_resolved_paths_land_in_the_other() {
    let vault_a = tempfile::tempdir().unwrap();
    let vault_b = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(vault_a.path().join("raw")).unwrap();
    std::fs::create_dir_all(vault_b.path().join("raw")).unwrap();

    let path_in_a = sandbox_path(vault_a.path(), "raw/doc.md").unwrap();
    let path_in_b = sandbox_path(vault_b.path(), "raw/doc.md").unwrap();

    assert!(path_in_a.starts_with(vault_a.path().canonicalize().unwrap()));
    assert!(!path_in_a.starts_with(vault_b.path().canonicalize().unwrap()));
    assert!(path_in_b.starts_with(vault_b.path().canonicalize().unwrap()));
    assert!(!path_in_b.starts_with(vault_a.path().canonicalize().unwrap()));
}

#[test]
fn an_attempt_to_traverse_from_one_vault_into_a_sibling_directory_is_rejected() {
    let parent = tempfile::tempdir().unwrap();
    let vault = parent.path().join("vault");
    let sibling_secret = parent.path().join("sibling-secret");
    std::fs::create_dir_all(&vault).unwrap();
    std::fs::create_dir_all(&sibling_secret).unwrap();
    std::fs::write(sibling_secret.join("data.txt"), "not yours").unwrap();

    let escape_attempt = sandbox_path(&vault, "../sibling-secret/data.txt");
    assert!(escape_attempt.is_err());
}

#[test]
fn nested_wiki_and_raw_paths_within_the_vault_resolve_normally() {
    let vault = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(vault.path().join("wiki/concepts")).unwrap();

    let resolved = sandbox_path(vault.path(), "wiki/concepts/microservices.md").unwrap();
    assert_eq!(
        resolved,
        vault
            .path()
            .canonicalize()
            .unwrap()
            .join("wiki/concepts/microservices.md")
    );
}
