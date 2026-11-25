//! Regenerating `wiki/index.md` (and scaffolding `wiki/schema.md`) — split
//! out of `driver.rs` so that file is just the orchestration (`compile`),
//! not every stage's own logic.

use std::path::Path;

use crate::core::vault_resolver::wiki_content_dirs;
use crate::storage::fs_ops;
use crate::validator::frontmatter::parse_wiki_page;
use crate::validator::rules::markdown_files_in;

/// Static, hand-maintained documentation of this vault's own conventions
/// (content-dir list, frontmatter shape, wikilink syntax) — framed as this
/// project's own convention, not an OKF-spec requirement, since the spec
/// itself doesn't define this file (only a `type:` field is required).
const WIKI_SCHEMA_MD: &str = include_str!("../../../assets/wiki-schema.md");

/// Writes `wiki/schema.md` if (and only if) it doesn't already exist yet.
///
/// `cli::vault::create` scaffolds it eagerly for brand-new vaults, but
/// `create` refuses to run against a non-empty directory, so an
/// already-existing vault would otherwise never get one. Called from
/// [`regenerate_index`] instead — every finalization path that regenerates
/// the index already runs through there, so any pre-existing vault picks
/// this file up on its very next compile/rebuild/synthesize-submit, with no
/// separate migration step required.
pub fn ensure_wiki_schema(vault_root: &Path) -> anyhow::Result<()> {
    if fs_ops::exists(vault_root, "wiki/schema.md") {
        return Ok(());
    }
    fs_ops::write(vault_root, "wiki/schema.md", WIKI_SCHEMA_MD)?;
    Ok(())
}

/// Human-readable section heading for a `wiki_content_dirs` "kind" —
/// `regenerate_index`'s only use of the kind label.
fn index_heading_for_kind(kind: &str) -> String {
    match kind {
        "concept" => "Concepts".to_string(),
        "entity" => "Entities".to_string(),
        "synthesis" => "Syntheses".to_string(),
        "comparison" => "Comparisons".to_string(),
        "decision" => "Decisions".to_string(),
        "question" => "Questions".to_string(),
        other => other.to_string(),
    }
}

/// Deterministic table-of-contents regeneration: one section per
/// `wiki_content_dirs` entry (in that fixed order), each sorted by title,
/// so re-running `compile` without any actual content change doesn't
/// produce git diff noise from incidental ordering. With six content dirs
/// instead of the original two, grouping by content type keeps the index
/// legible rather than interleaving concepts, entities, syntheses,
/// comparisons, decisions, and questions into one flat list. A dir with no
/// pages yet is omitted entirely rather than printing an empty heading. Per
/// OKF v0.2 §12, the bundle-root `index.md` is the *one* place
/// `okf_version` belongs — no other key is added here.
///
/// Also ensures `wiki/schema.md` exists (see [`ensure_wiki_schema`]) —
/// every finalization path that regenerates the index runs through here,
/// so this is the one place a pre-existing vault is guaranteed to pick the
/// schema file up.
pub fn regenerate_index(vault_root: &Path) -> anyhow::Result<()> {
    ensure_wiki_schema(vault_root)?;

    let mut markdown = String::from("---\nokf_version: \"0.2\"\n---\n\n# Wiki Index\n");

    for (dir, kind) in wiki_content_dirs(vault_root) {
        let mut entries = Vec::new();
        for path in markdown_files_in(&dir)? {
            let content = std::fs::read_to_string(&path)?;
            if let Ok(parsed) = parse_wiki_page(&content) {
                let relative = path
                    .strip_prefix(vault_root)
                    .unwrap_or(&path)
                    .to_string_lossy()
                    .replace('\\', "/");
                entries.push((
                    parsed.frontmatter.title,
                    relative,
                    parsed.frontmatter.description.unwrap_or_default(),
                ));
            }
        }
        if entries.is_empty() {
            continue;
        }
        entries.sort();

        markdown.push_str(&format!("\n## {}\n\n", index_heading_for_kind(kind)));
        for (title, path, description) in entries {
            if description.is_empty() {
                markdown.push_str(&format!("- [{title}]({path})\n"));
            } else {
                markdown.push_str(&format!("- [{title}]({path}) — {description}\n"));
            }
        }
    }
    fs_ops::write(vault_root, "wiki/index.md", &markdown)?;
    Ok(())
}
