//! Thin read/write helpers built on `core::vault_resolver::sandbox_path` —
//! a convenience layer for callers that just want "read/write this
//! vault-relative path safely" without repeating the sandbox-then-touch-fs
//! dance inline.

use std::path::{Path, PathBuf};

use crate::core::vault_resolver::sandbox_path;

pub fn read_to_string(vault_root: &Path, relative: &str) -> anyhow::Result<String> {
    let path = sandbox_path(vault_root, relative)?;
    Ok(std::fs::read_to_string(path)?)
}

/// Creates any missing parent directories before writing.
pub fn write(vault_root: &Path, relative: &str, contents: &str) -> anyhow::Result<PathBuf> {
    let path = sandbox_path(vault_root, relative)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, contents)?;
    Ok(path)
}

/// No-ops (rather than erroring) when the file doesn't exist — matches
/// `manifest::model::Manifest::purge`'s "caller unlinks the raw blob,
/// tolerant of it already being gone" contract.
pub fn remove_file(vault_root: &Path, relative: &str) -> anyhow::Result<()> {
    let path = sandbox_path(vault_root, relative)?;
    if path.is_file() {
        std::fs::remove_file(path)?;
    }
    Ok(())
}

pub fn exists(vault_root: &Path, relative: &str) -> bool {
    sandbox_path(vault_root, relative)
        .map(|path| path.exists())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_creates_parent_directories_and_read_round_trips() {
        let vault = tempfile::tempdir().unwrap();
        let path = write(vault.path(), "wiki/concepts/foo.md", "content").unwrap();
        assert!(path.is_file());
        assert_eq!(
            read_to_string(vault.path(), "wiki/concepts/foo.md").unwrap(),
            "content"
        );
    }

    #[test]
    fn exists_reflects_the_real_filesystem_state() {
        let vault = tempfile::tempdir().unwrap();
        assert!(!exists(vault.path(), "raw/a.md"));
        write(vault.path(), "raw/a.md", "x").unwrap();
        assert!(exists(vault.path(), "raw/a.md"));
    }

    #[test]
    fn remove_file_is_a_no_op_when_the_file_is_already_gone() {
        let vault = tempfile::tempdir().unwrap();
        assert!(remove_file(vault.path(), "raw/missing.md").is_ok());
    }

    #[test]
    fn remove_file_deletes_an_existing_file() {
        let vault = tempfile::tempdir().unwrap();
        write(vault.path(), "raw/a.md", "x").unwrap();
        remove_file(vault.path(), "raw/a.md").unwrap();
        assert!(!exists(vault.path(), "raw/a.md"));
    }

    #[test]
    fn a_path_escaping_the_vault_root_is_rejected() {
        let vault = tempfile::tempdir().unwrap();
        assert!(write(vault.path(), "../escape.md", "x").is_err());
    }
}
