// `okf-mcp vault list|add|default` — manages `~/.config/okf/vaults.toml`.

use std::path::PathBuf;

use okf_mcp::core::vault_registry::{VaultEntry, VaultRegistry};

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
            Some(description) => println!("{name}{marker}: {} — {description}", entry.path.display()),
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
