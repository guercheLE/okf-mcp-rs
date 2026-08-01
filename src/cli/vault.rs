// `okf-mcp vault list|add|create|remove|delete|default` — manages
// `~/.config/okf/vaults.toml`. Also reachable as `okf-mcp kb ...` (see
// `main.rs`'s `Command::Vault` alias) — same commands, two names.

use std::path::PathBuf;

use okf_mcp::core::vault_registry::{VaultEntry, VaultRegistry};
use okf_mcp::manifest::{self, Manifest};

pub fn list() -> anyhow::Result<()> {
    let registry = VaultRegistry::load()?;
    if registry.vaults.is_empty() {
        println!("No vaults registered. Use `okf-mcp vault add <path> --name <name>`.");
        return Ok(());
    }
    let mut names: Vec<&String> = registry.vaults.keys().collect();
    names.sort();
    for name in names {
        let entry = &registry.vaults[name];
        let marker = if registry.default.as_deref() == Some(name.as_str()) {
            " (default)"
        } else {
            ""
        };
        match &entry.description {
            Some(description) => {
                println!("{name}{marker}: {} — {description}", entry.path.display())
            }
            None => println!("{name}{marker}: {}", entry.path.display()),
        }
    }
    Ok(())
}

pub fn add(path: &str, name: &str, description: Option<&str>) -> anyhow::Result<()> {
    let mut registry = VaultRegistry::load()?;
    registry.vaults.insert(
        name.to_string(),
        VaultEntry {
            path: PathBuf::from(path),
            description: description.map(str::to_string),
        },
    );
    registry.save()?;
    println!("Registered vault '{name}' -> {path}");
    Ok(())
}

/// Scaffolds a brand-new vault directory (`.okf/`, `wiki/concepts/`,
/// `raw/`) and registers it — distinct from `add`, which only registers
/// an existing directory. Refuses to touch a `path` that already exists
/// and is non-empty, to avoid silently adopting/overwriting content that
/// wasn't necessarily meant to become a vault.
pub fn create(path: &str, name: &str, description: Option<&str>) -> anyhow::Result<()> {
    let root = PathBuf::from(path);
    let non_empty = std::fs::read_dir(&root)
        .map(|mut entries| entries.next().is_some())
        .unwrap_or(false);
    if non_empty {
        anyhow::bail!(
            "'{path}' already exists and is not empty — use `okf-mcp vault add {path} --name {name}` \
             to register an existing vault instead"
        );
    }

    std::fs::create_dir_all(root.join("wiki/concepts"))?;
    std::fs::create_dir_all(root.join("raw"))?;
    // Creates `.okf/` and writes an empty manifest.json.
    manifest::store::save(&root, &Manifest::default())?;

    add(path, name, description)?;
    println!("Created vault '{name}' at {path}");
    Ok(())
}

pub fn remove(name: &str) -> anyhow::Result<()> {
    let mut registry = VaultRegistry::load()?;
    if registry.vaults.remove(name).is_none() {
        anyhow::bail!("no vault named '{name}' is registered");
    }
    if registry.default.as_deref() == Some(name) {
        registry.default = None;
    }
    registry.save()?;
    println!("Removed vault '{name}' from registry.");
    Ok(())
}

/// Unregisters `name` AND permanently deletes its directory tree.
/// Destructive — requires `--force` (surfaced here as `force: bool`, not
/// an interactive confirmation, since this is a scriptable CLI) or bails
/// with a clear message. Deletes the directory *before* unregistering: if
/// `remove_dir_all` fails partway (permissions, a locked file), the vault
/// stays registered and the delete is safely retryable, rather than
/// leaving a registered entry with no backing directory.
pub fn delete(name: &str, force: bool) -> anyhow::Result<()> {
    let registry = VaultRegistry::load()?;
    let entry = registry
        .vaults
        .get(name)
        .ok_or_else(|| anyhow::anyhow!("no vault named '{name}' is registered"))?;
    let path = entry.path.clone();

    if !force {
        anyhow::bail!(
            "refusing to delete vault '{name}' ({}) without --force — this permanently deletes \
             the directory and everything in it",
            path.display()
        );
    }

    println!("Deleting {} ...", path.display());
    if path.is_dir() {
        std::fs::remove_dir_all(&path)?;
    }
    remove(name)?;
    println!("Deleted vault '{name}' and its directory.");
    Ok(())
}

pub fn default(name: &str) -> anyhow::Result<()> {
    let mut registry = VaultRegistry::load()?;
    if !registry.vaults.contains_key(name) {
        anyhow::bail!("no vault named '{name}' is registered — run `okf-mcp vault add` first");
    }
    registry.default = Some(name.to_string());
    registry.save()?;
    println!("Default vault set to '{name}'.");
    Ok(())
}
