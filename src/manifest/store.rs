//! Load/save `<vault_root>/.okf/manifest.json`.

use std::path::Path;

use super::model::Manifest;

const MANIFEST_RELATIVE_PATH: &str = ".okf/manifest.json";

/// Returns an empty `Manifest` when the file doesn't exist yet (a vault's
/// very first `ingest`), rather than erroring — callers don't need to
/// special-case "no manifest yet" vs. "empty manifest".
pub fn load(vault_root: &Path) -> anyhow::Result<Manifest> {
    let path = vault_root.join(MANIFEST_RELATIVE_PATH);
    match std::fs::read_to_string(&path) {
        Ok(contents) => Ok(serde_json::from_str(&contents)?),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Manifest::default()),
        Err(err) => Err(err.into()),
    }
}

/// Write-tmp-then-rename so a crash or concurrent read mid-write never
/// observes a truncated/partial `manifest.json`.
pub fn save(vault_root: &Path, manifest: &Manifest) -> anyhow::Result<()> {
    let path = vault_root.join(MANIFEST_RELATIVE_PATH);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let tmp_path = path.with_extension("json.tmp");
    let contents = serde_json::to_string_pretty(manifest)?;
    std::fs::write(&tmp_path, contents)?;
    std::fs::rename(&tmp_path, &path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loading_a_vault_with_no_manifest_yet_returns_an_empty_one() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = load(dir.path()).unwrap();
        assert!(manifest.sources.is_empty());
    }

    #[test]
    fn save_then_load_round_trips_manifest_state() {
        let dir = tempfile::tempdir().unwrap();
        let mut manifest = Manifest::default();
        manifest.record_ingest("uri", "sha256:aaa", "raw_aaa", "t0");

        save(dir.path(), &manifest).unwrap();
        assert!(dir.path().join(MANIFEST_RELATIVE_PATH).is_file());

        let loaded = load(dir.path()).unwrap();
        assert_eq!(loaded.get_active_raw_id("uri"), Some("raw_aaa"));
    }

    #[test]
    fn save_creates_the_dot_okf_directory_if_missing() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!dir.path().join(".okf").exists());
        save(dir.path(), &Manifest::default()).unwrap();
        assert!(dir.path().join(".okf").is_dir());
    }
}
