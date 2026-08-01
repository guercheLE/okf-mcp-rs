//! Resolves which vault a CLI invocation or MCP tool call operates on, and
//! sandboxes every path derived from that resolution.
//!
//! Resolution order (matches the design doc's Q6): explicit `--vault`/`vault`
//! param -> walk up from the current directory looking for a `.okf/` marker
//! -> the registry's default vault.

use std::path::{Component, Path, PathBuf};

use super::vault_registry::VaultRegistry;

const VAULT_MARKER_DIR: &str = ".okf";

pub fn resolve_vault(explicit: Option<&str>) -> anyhow::Result<PathBuf> {
    resolve_vault_with(explicit, &std::env::current_dir()?, &VaultRegistry::load()?)
}

/// Takes the registry as a plain value rather than loading it internally —
/// lets tests exercise the CWD-walk-up/registry-fallback logic against an
/// in-memory `VaultRegistry` instead of needing to mutate the real process
/// `HOME` (which `VaultRegistry::load()` reads), avoiding a cross-test race
/// on that global env var.
fn resolve_vault_with(
    explicit: Option<&str>,
    start_dir: &Path,
    registry: &VaultRegistry,
) -> anyhow::Result<PathBuf> {
    if let Some(name_or_path) = explicit {
        return Ok(registry.resolve(name_or_path));
    }

    let mut dir = start_dir.to_path_buf();
    loop {
        if dir.join(VAULT_MARKER_DIR).is_dir() {
            return Ok(dir);
        }
        if !dir.pop() {
            break;
        }
    }

    registry.default_path()
}

/// The single chokepoint every module that touches vault content (`ingest`,
/// `manifest`, `compiler::operations`, `validator`, `search::index`) must
/// call before reading or writing a vault-relative path. Rejects absolute
/// paths and any `..` that would climb above `vault_root`.
///
/// Deliberately a pure, filesystem-side-effect-free lexical normalization
/// (resolve `.`/`..` components against the canonicalized root) rather than
/// `Path::canonicalize`-ing the candidate itself: canonicalize requires the
/// path to already exist, which would force this security check to also be
/// responsible for creating directories — a surprising side effect for a
/// function whose entire job is "is this allowed". `vault_root` itself must
/// already exist (every vault has a `.okf/` marker by construction), so only
/// it needs canonicalizing.
pub fn sandbox_path(vault_root: &Path, relative: &str) -> anyhow::Result<PathBuf> {
    let canonical_root = vault_root.canonicalize().map_err(|err| {
        anyhow::anyhow!(
            "vault root '{}' is not accessible: {err}",
            vault_root.display()
        )
    })?;

    let mut resolved = canonical_root.clone();
    for component in Path::new(relative).components() {
        match component {
            Component::Normal(part) => resolved.push(part),
            Component::CurDir => {}
            Component::ParentDir => {
                if !resolved.pop() || !resolved.starts_with(&canonical_root) {
                    anyhow::bail!(
                        "path '{relative}' escapes vault root '{}': not permitted",
                        vault_root.display()
                    );
                }
            }
            Component::RootDir | Component::Prefix(_) => {
                anyhow::bail!("path '{relative}' must be relative to the vault root, not absolute");
            }
        }
    }

    if !resolved.starts_with(&canonical_root) {
        anyhow::bail!(
            "path '{relative}' escapes vault root '{}': not permitted",
            vault_root.display()
        );
    }
    Ok(resolved)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::vault_registry::VaultEntry;

    fn make_vault(root: &Path) {
        std::fs::create_dir_all(root.join(VAULT_MARKER_DIR)).unwrap();
    }

    #[test]
    fn explicit_vault_wins_even_from_outside_any_vault_directory() {
        let dir = tempfile::tempdir().unwrap();
        let resolved = resolve_vault_with(
            Some(dir.path().to_str().unwrap()),
            Path::new("/tmp"),
            &VaultRegistry::default(),
        )
        .unwrap();
        assert_eq!(resolved, dir.path());
    }

    #[test]
    fn cwd_walk_up_finds_a_dot_okf_marker_in_a_parent_directory() {
        let dir = tempfile::tempdir().unwrap();
        make_vault(dir.path());
        let nested = dir.path().join("a/b/c");
        std::fs::create_dir_all(&nested).unwrap();

        let resolved = resolve_vault_with(None, &nested, &VaultRegistry::default()).unwrap();
        assert_eq!(resolved, dir.path());
    }

    #[test]
    fn falls_back_to_the_registry_default_when_no_marker_is_found() {
        let mut registry = VaultRegistry {
            default: Some("work".to_string()),
            ..Default::default()
        };
        let vault_dir = tempfile::tempdir().unwrap();
        registry.vaults.insert(
            "work".to_string(),
            VaultEntry {
                path: vault_dir.path().to_path_buf(),
                description: None,
            },
        );

        let outside = tempfile::tempdir().unwrap();
        let resolved = resolve_vault_with(None, outside.path(), &registry).unwrap();
        assert_eq!(resolved, vault_dir.path());
    }

    #[test]
    fn sandbox_path_accepts_a_legitimate_relative_path() {
        let dir = tempfile::tempdir().unwrap();
        let resolved = sandbox_path(dir.path(), "raw/x.md").unwrap();
        assert_eq!(
            resolved,
            dir.path().canonicalize().unwrap().join("raw/x.md")
        );
    }

    #[test]
    fn sandbox_path_rejects_a_parent_dir_escape() {
        let dir = tempfile::tempdir().unwrap();
        assert!(sandbox_path(dir.path(), "../../etc/passwd").is_err());
    }

    #[test]
    fn sandbox_path_rejects_an_absolute_path() {
        let dir = tempfile::tempdir().unwrap();
        assert!(sandbox_path(dir.path(), "/etc/passwd").is_err());
    }

    #[test]
    fn sandbox_path_allows_dot_dot_that_stays_inside_the_root() {
        let dir = tempfile::tempdir().unwrap();
        let resolved = sandbox_path(dir.path(), "raw/../wiki/index.md").unwrap();
        assert_eq!(
            resolved,
            dir.path().canonicalize().unwrap().join("wiki/index.md")
        );
    }

    #[test]
    fn two_vaults_never_resolve_into_each_others_roots() {
        let vault_a = tempfile::tempdir().unwrap();
        let vault_b = tempfile::tempdir().unwrap();

        let resolved_a = sandbox_path(vault_a.path(), "raw/x.md").unwrap();
        let resolved_b = sandbox_path(vault_b.path(), "raw/x.md").unwrap();

        assert!(resolved_a.starts_with(vault_a.path().canonicalize().unwrap()));
        assert!(resolved_b.starts_with(vault_b.path().canonicalize().unwrap()));
        assert_ne!(resolved_a, resolved_b);
    }
}
