//! The cross-tool vault registry (`~/.config/okf/vaults.toml`): a name ->
//! path lookup for every OKF vault a user has registered on this machine,
//! plus which one is the default. Deliberately lives under
//! `~/.config/okf/`, not this binary's own `~/.okf-mcp/` settings tree —
//! other OKF-format consumers (an Obsidian plugin, another OKF client)
//! might reasonably read the same registry, so it isn't scoped to this
//! binary's own config namespace.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::credential_storage::resolve_home_dir;

const REGISTRY_RELATIVE_PATH: &str = ".config/okf/vaults.toml";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VaultEntry {
    pub path: PathBuf,
    #[serde(default)]
    pub description: Option<String>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct VaultRegistry {
    #[serde(default)]
    pub default: Option<String>,
    #[serde(default)]
    pub vaults: HashMap<String, VaultEntry>,
}

impl VaultRegistry {
    pub fn registry_path() -> PathBuf {
        resolve_home_dir().join(REGISTRY_RELATIVE_PATH)
    }

    pub fn load() -> anyhow::Result<Self> {
        Self::load_from(&Self::registry_path())
    }

    fn load_from(path: &Path) -> anyhow::Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(contents) => Ok(toml::from_str(&contents)?),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(err) => Err(err.into()),
        }
    }

    pub fn save(&self) -> anyhow::Result<()> {
        self.save_to(&Self::registry_path())
    }

    fn save_to(&self, path: &Path) -> anyhow::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, toml::to_string_pretty(self)?)?;
        Ok(())
    }

    /// A registered vault name resolves to its registered path; anything
    /// else is treated as a literal filesystem path, so `--vault
    /// ~/Vaults/Personal` works even for a vault that was never `vault
    /// add`ed.
    pub fn resolve(&self, name_or_path: &str) -> PathBuf {
        self.vaults
            .get(name_or_path)
            .map(|entry| entry.path.clone())
            .unwrap_or_else(|| PathBuf::from(name_or_path))
    }

    pub fn default_path(&self) -> anyhow::Result<PathBuf> {
        let name = self.default.as_deref().ok_or_else(|| {
            anyhow::anyhow!(
                "no vault specified, no `.okf/` directory found in any parent of the \
                 current directory, and no default vault is registered — pass --vault, \
                 run from inside a vault, or run `okf-mcp vault default <name>`"
            )
        })?;
        self.vaults
            .get(name)
            .map(|entry| entry.path.clone())
            .ok_or_else(|| anyhow::anyhow!("default vault '{name}' is not registered"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loading_a_registry_that_does_not_exist_yet_returns_an_empty_one() {
        let dir = tempfile::tempdir().unwrap();
        let registry = VaultRegistry::load_from(&dir.path().join("vaults.toml")).unwrap();
        assert!(registry.default.is_none());
        assert!(registry.vaults.is_empty());
    }

    #[test]
    fn save_then_load_round_trips_registered_vaults() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("vaults.toml");

        let mut registry = VaultRegistry {
            default: Some("work".to_string()),
            ..Default::default()
        };
        registry.vaults.insert(
            "work".to_string(),
            VaultEntry {
                path: PathBuf::from("/Users/dev/Vaults/Work"),
                description: Some("Main corporate knowledge base".to_string()),
            },
        );
        registry.save_to(&path).unwrap();

        let loaded = VaultRegistry::load_from(&path).unwrap();
        assert_eq!(loaded.default.as_deref(), Some("work"));
        assert_eq!(
            loaded.vaults["work"].path,
            PathBuf::from("/Users/dev/Vaults/Work")
        );
    }

    #[test]
    fn resolve_prefers_a_registered_name_over_treating_it_as_a_literal_path() {
        let mut registry = VaultRegistry::default();
        registry.vaults.insert(
            "work".to_string(),
            VaultEntry {
                path: PathBuf::from("/Users/dev/Vaults/Work"),
                description: None,
            },
        );
        assert_eq!(registry.resolve("work"), PathBuf::from("/Users/dev/Vaults/Work"));
        assert_eq!(
            registry.resolve("/some/literal/path"),
            PathBuf::from("/some/literal/path")
        );
    }

    #[test]
    fn default_path_errors_when_no_default_is_configured() {
        let registry = VaultRegistry::default();
        assert!(registry.default_path().is_err());
    }

    #[test]
    fn default_path_errors_when_the_default_name_is_not_actually_registered() {
        let registry = VaultRegistry {
            default: Some("ghost".to_string()),
            ..Default::default()
        };
        assert!(registry.default_path().is_err());
    }

    #[test]
    fn default_path_resolves_the_registered_default_vault() {
        let mut registry = VaultRegistry {
            default: Some("work".to_string()),
            ..Default::default()
        };
        registry.vaults.insert(
            "work".to_string(),
            VaultEntry {
                path: PathBuf::from("/Users/dev/Vaults/Work"),
                description: None,
            },
        );
        assert_eq!(
            registry.default_path().unwrap(),
            PathBuf::from("/Users/dev/Vaults/Work")
        );
    }
}
