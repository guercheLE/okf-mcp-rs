// `okf-mcp search`: queries a vault's `.okf/index.db` (hybrid BM25 +
// dense-vector, merged via RRF — see `okf_mcp::search::hybrid_search`).

use okf_mcp::core::output::Output;
use okf_mcp::core::vault_registry::VaultRegistry;
use okf_mcp::core::vault_resolver::resolve_vault;
use okf_mcp::search::{SearchResult, hybrid_search};

pub fn run(
    query: &str,
    limit: usize,
    json: bool,
    all_vaults: bool,
    vault: Option<&str>,
) -> anyhow::Result<()> {
    if all_vaults {
        let results = search_all_vaults(query, limit)?;
        print_tagged_results(&results, json)
    } else {
        let vault_root = resolve_vault(vault)?;
        let results = hybrid_search(&vault_root, query, limit)?;
        print_results(&results, json)
    }
}

fn search_all_vaults(query: &str, limit: usize) -> anyhow::Result<Vec<(String, SearchResult)>> {
    let registry = VaultRegistry::load()?;
    let mut tagged = Vec::new();
    for (name, entry) in &registry.vaults {
        if let Ok(results) = hybrid_search(&entry.path, query, limit) {
            tagged.extend(results.into_iter().map(|result| (name.clone(), result)));
        }
    }
    tagged.sort_by(|(_, a), (_, b)| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    tagged.truncate(limit);
    Ok(tagged)
}

fn print_results(results: &[SearchResult], json: bool) -> anyhow::Result<()> {
    let output = Output::cli();
    if json {
        output.line(&serde_json::to_string_pretty(results)?);
        return Ok(());
    }
    if results.is_empty() {
        output.line("No results.");
        return Ok(());
    }
    for result in results {
        output.line(&format!(
            "{:.4}  {}  — {}",
            result.score, result.path, result.title
        ));
        if !result.snippet.is_empty() {
            output.line(&format!("        {}", result.snippet));
        }
    }
    Ok(())
}

fn print_tagged_results(results: &[(String, SearchResult)], json: bool) -> anyhow::Result<()> {
    let output = Output::cli();
    if json {
        let value: Vec<serde_json::Value> = results
            .iter()
            .map(|(vault, result)| {
                let mut value = serde_json::to_value(result).unwrap_or(serde_json::Value::Null);
                if let Some(object) = value.as_object_mut() {
                    object.insert("vault".to_string(), serde_json::json!(vault));
                }
                value
            })
            .collect();
        output.line(&serde_json::to_string_pretty(&value)?);
        return Ok(());
    }
    if results.is_empty() {
        output.line("No results.");
        return Ok(());
    }
    for (vault, result) in results {
        output.line(&format!(
            "{:.4}  [{vault}] {}  — {}",
            result.score, result.path, result.title
        ));
        if !result.snippet.is_empty() {
            output.line(&format!("        {}", result.snippet));
        }
    }
    Ok(())
}
