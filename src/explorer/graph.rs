//! Builds the vault's link graph — the data behind the explorer's 3D view.
//!
//! Nodes are wiki pages (`concept` / `entity`, by which content dir they
//! live in), frontmatter tags (`tag`, id `#name` — Obsidian draws these as
//! their own nodes and they're usually the biggest hubs), raw sources
//! referenced from `sources:` (`raw`), cross-vault references
//! (`cross_vault`), and wikilink targets that don't resolve (`missing`,
//! drawn dim like Obsidian's unresolved links). Links are one per distinct
//! (source, target) pair.
//!
//! Every node carries `in_degree` (how many distinct nodes point at it) and
//! a kind-agnostic `hub_rank` (dense rank by `in_degree`, 1 = biggest hub) so
//! the UI can size nodes by inbound links and hide the top-N hubs of *any*
//! kind — a heavily-linked note is as much a hub as a popular tag — to
//! expose the islands/clusters those hubs otherwise glue together.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;

use serde::Serialize;

use crate::core::vault_resolver::{sandbox_path, wiki_content_dirs};
use crate::validator::frontmatter::parse_wiki_page;
use crate::validator::rules::markdown_files_in;
use crate::validator::wikilink::{WikiLink, extract_wikilinks};

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct GraphNode {
    /// Stable identifier: a page's slug, `#tag`, `raw:<stem>`,
    /// `<vault>::<slug>`, or the unresolved slug itself.
    pub id: String,
    pub title: String,
    /// `concept` | `entity` | `tag` | `raw` | `cross_vault` | `missing`
    pub kind: &'static str,
    /// The page's frontmatter `type:` (Concept, Product, Person, …), when any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub r#type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    /// Vault-relative path (`wiki/concepts/foo.md`, `raw/raw_abc.md`) for
    /// nodes that are files on disk.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    pub in_degree: usize,
    pub out_degree: usize,
    /// Dense rank by `in_degree`, descending: 1 = the biggest hub in the
    /// vault (of any kind). Nodes with equal `in_degree` share a rank.
    pub hub_rank: usize,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq, Hash)]
pub struct GraphLink {
    pub source: String,
    pub target: String,
    /// `wikilink` | `tag` | `source` | `cross_vault`
    pub kind: &'static str,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Graph {
    pub nodes: Vec<GraphNode>,
    pub links: Vec<GraphLink>,
    /// Number of vault pages (concepts + entities) that failed to parse; the
    /// pages are still present as nodes (title = slug) so the graph stays
    /// complete, but the count is surfaced so the UI can say so.
    pub unparsed_pages: usize,
}

impl Graph {
    pub fn node(&self, id: &str) -> Option<&GraphNode> {
        self.nodes.iter().find(|n| n.id == id)
    }

    /// Every node with a link *into* `id`, in stable (node) order.
    pub fn backlinks_of(&self, id: &str) -> Vec<&GraphNode> {
        let sources: HashSet<&str> = self
            .links
            .iter()
            .filter(|l| l.target == id)
            .map(|l| l.source.as_str())
            .collect();
        self.nodes
            .iter()
            .filter(|n| sources.contains(n.id.as_str()))
            .collect()
    }

    /// Every node `id` links *out* to, in stable (node) order.
    pub fn outlinks_of(&self, id: &str) -> Vec<&GraphNode> {
        let targets: HashSet<&str> = self
            .links
            .iter()
            .filter(|l| l.source == id)
            .map(|l| l.target.as_str())
            .collect();
        self.nodes
            .iter()
            .filter(|n| targets.contains(n.id.as_str()))
            .collect()
    }
}

/// `[[slug#Heading]]` / `[[slug^block]]` address a location *inside* a
/// page; the graph only cares about the page. Obsidian does the same.
fn strip_anchor(slug: &str) -> &str {
    let cut = slug.find(['#', '^']).unwrap_or(slug.len());
    slug[..cut].trim()
}

/// `sources[].resource` is written like `/raw/raw_a1f448945f.md`; normalize
/// to a vault-relative path plus its stem so both the node id and the
/// on-disk lookup are stable regardless of a leading slash or `./`.
fn normalize_source_resource(resource: &str) -> (String, String) {
    let relative = resource
        .trim()
        .trim_start_matches("./")
        .trim_start_matches('/')
        .to_string();
    let stem = Path::new(&relative)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(&relative)
        .to_string();
    (relative, stem)
}

/// Best-effort title for a raw source: its `source_url:` if the raw blob
/// parses, else the stem. Never fails — a raw node with a plain title beats
/// a missing node.
pub(crate) fn raw_source_title(vault_root: &Path, relative: &str, stem: &str) -> (String, bool) {
    // `resource` is LLM/user-written frontmatter: resolve it through the
    // sandbox so a `../` in it can't make the graph builder read outside
    // the vault — an escaping resource is simply "not found".
    let Ok(path) = sandbox_path(vault_root, relative) else {
        return (stem.to_string(), false);
    };
    if !path.is_file() {
        return (stem.to_string(), false);
    }
    let Ok(content) = std::fs::read_to_string(&path) else {
        return (stem.to_string(), true);
    };
    let title = content
        .strip_prefix("---\n")
        .and_then(|rest| rest.find("\n---\n").map(|end| &rest[..end]))
        .and_then(|yaml| serde_yaml::from_str::<serde_yaml::Value>(yaml).ok())
        .and_then(|value| {
            value
                .get("source_url")
                .and_then(|v| v.as_str())
                .map(str::to_string)
        })
        .unwrap_or_else(|| stem.to_string());
    (title, true)
}

struct PendingNode {
    title: String,
    kind: &'static str,
    r#type: Option<String>,
    description: Option<String>,
    tags: Vec<String>,
    path: Option<String>,
}

pub fn build_graph(vault_root: &Path) -> anyhow::Result<Graph> {
    // BTreeMap: deterministic node order regardless of directory walk order.
    let mut nodes: BTreeMap<String, PendingNode> = BTreeMap::new();
    let mut links: HashSet<GraphLink> = HashSet::new();
    let mut unparsed_pages = 0usize;

    // Pass 1: every page becomes a node so pass 2 can tell resolved links
    // from missing ones without caring about walk order.
    struct PageLinks {
        slug: String,
        wikilinks: Vec<WikiLink>,
        tags: Vec<String>,
        sources: Vec<String>,
    }
    let mut pages: Vec<PageLinks> = Vec::new();

    for (dir, kind) in wiki_content_dirs(vault_root)
        .into_iter()
        .zip(["concept", "entity"])
    {
        let mut files = markdown_files_in(&dir)?;
        files.sort();
        for path in files {
            let Some(slug) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            let slug = slug.to_string();
            let relative = path
                .strip_prefix(vault_root)
                .unwrap_or(&path)
                .to_string_lossy()
                .replace('\\', "/");
            let content = std::fs::read_to_string(&path)?;

            match parse_wiki_page(&content) {
                Ok(parsed) => {
                    let tags: Vec<String> = parsed
                        .frontmatter
                        .tags
                        .iter()
                        .map(|t| t.trim().trim_start_matches('#').to_string())
                        .filter(|t| !t.is_empty())
                        .collect();
                    nodes.insert(
                        slug.clone(),
                        PendingNode {
                            title: parsed.frontmatter.title.clone(),
                            kind,
                            r#type: Some(parsed.frontmatter.r#type.clone()),
                            description: parsed.frontmatter.description.clone(),
                            tags: tags.clone(),
                            path: Some(relative),
                        },
                    );
                    pages.push(PageLinks {
                        slug,
                        wikilinks: extract_wikilinks(&parsed.body),
                        tags,
                        sources: parsed
                            .frontmatter
                            .sources
                            .iter()
                            .map(|s| s.resource.clone())
                            .collect(),
                    });
                }
                Err(_) => {
                    unparsed_pages += 1;
                    nodes.insert(
                        slug.clone(),
                        PendingNode {
                            title: slug.clone(),
                            kind,
                            r#type: None,
                            description: None,
                            tags: Vec::new(),
                            path: Some(relative),
                        },
                    );
                    pages.push(PageLinks {
                        slug,
                        wikilinks: extract_wikilinks(&content),
                        tags: Vec::new(),
                        sources: Vec::new(),
                    });
                }
            }
        }
    }

    // Pass 2: edges, creating tag / raw / cross-vault / missing nodes on
    // first sight.
    for page in &pages {
        for link in &page.wikilinks {
            match link {
                WikiLink::Local(target) => {
                    let target = strip_anchor(target);
                    if target.is_empty() || target == page.slug {
                        continue;
                    }
                    nodes
                        .entry(target.to_string())
                        .or_insert_with(|| PendingNode {
                            title: target.to_string(),
                            kind: "missing",
                            r#type: None,
                            description: None,
                            tags: Vec::new(),
                            path: None,
                        });
                    links.insert(GraphLink {
                        source: page.slug.clone(),
                        target: target.to_string(),
                        kind: "wikilink",
                    });
                }
                WikiLink::CrossVault { vault, concept } => {
                    let id = format!("{vault}::{}", strip_anchor(concept));
                    nodes.entry(id.clone()).or_insert_with(|| PendingNode {
                        title: id.clone(),
                        kind: "cross_vault",
                        r#type: None,
                        description: Some(format!("in vault '{vault}'")),
                        tags: Vec::new(),
                        path: None,
                    });
                    links.insert(GraphLink {
                        source: page.slug.clone(),
                        target: id,
                        kind: "cross_vault",
                    });
                }
            }
        }
        for tag in &page.tags {
            let id = format!("#{tag}");
            nodes.entry(id.clone()).or_insert_with(|| PendingNode {
                title: id.clone(),
                kind: "tag",
                r#type: None,
                description: None,
                tags: Vec::new(),
                path: None,
            });
            links.insert(GraphLink {
                source: page.slug.clone(),
                target: id,
                kind: "tag",
            });
        }
        for resource in &page.sources {
            let (relative, stem) = normalize_source_resource(resource);
            if stem.is_empty() {
                continue;
            }
            let id = format!("raw:{stem}");
            nodes.entry(id.clone()).or_insert_with(|| {
                let (title, exists) = raw_source_title(vault_root, &relative, &stem);
                PendingNode {
                    title,
                    kind: "raw",
                    r#type: None,
                    description: (!exists).then(|| "raw file not found".to_string()),
                    tags: Vec::new(),
                    path: exists.then_some(relative),
                }
            });
            links.insert(GraphLink {
                source: page.slug.clone(),
                target: id,
                kind: "source",
            });
        }
    }

    // Degrees + hub rank.
    let mut in_degree: HashMap<&str, usize> = HashMap::new();
    let mut out_degree: HashMap<&str, usize> = HashMap::new();
    for link in &links {
        *in_degree.entry(link.target.as_str()).or_default() += 1;
        *out_degree.entry(link.source.as_str()).or_default() += 1;
    }
    let mut distinct_in: Vec<usize> = in_degree.values().copied().collect();
    distinct_in.push(0);
    distinct_in.sort_unstable_by(|a, b| b.cmp(a));
    distinct_in.dedup();
    let rank_of: HashMap<usize, usize> = distinct_in
        .iter()
        .enumerate()
        .map(|(i, d)| (*d, i + 1))
        .collect();

    let nodes: Vec<GraphNode> = nodes
        .into_iter()
        .map(|(id, pending)| {
            let ind = in_degree.get(id.as_str()).copied().unwrap_or(0);
            GraphNode {
                hub_rank: rank_of[&ind],
                in_degree: ind,
                out_degree: out_degree.get(id.as_str()).copied().unwrap_or(0),
                id,
                title: pending.title,
                kind: pending.kind,
                r#type: pending.r#type,
                description: pending.description,
                tags: pending.tags,
                path: pending.path,
            }
        })
        .collect();

    let mut links: Vec<GraphLink> = links.into_iter().collect();
    links.sort_by(|a, b| (&a.source, &a.target).cmp(&(&b.source, &b.target)));

    Ok(Graph {
        nodes,
        links,
        unparsed_pages,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(root: &Path, relative: &str, content: &str) {
        let path = root.join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }

    fn page(title: &str, tags: &[&str], sources: &[&str], body: &str) -> String {
        let tags = tags
            .iter()
            .map(|t| format!("  - {t}"))
            .collect::<Vec<_>>()
            .join("\n");
        let sources = sources
            .iter()
            .map(|s| format!("  - resource: {s}"))
            .collect::<Vec<_>>()
            .join("\n");
        format!(
            "---\ntype: Concept\ntitle: {title}\ndescription: about {title}\ntags:\n{tags}\nsources:\n{sources}\n---\n\n{body}\n"
        )
    }

    fn sample_vault() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join(".okf")).unwrap();
        write(
            root,
            "raw/raw_abc.md",
            "---\ntype: raw_source\nsource_url: https://example.com/a\n---\n\nraw body\n",
        );
        write(
            root,
            "wiki/concepts/alpha.md",
            &page(
                "Alpha",
                &["travel", "ai"],
                &["/raw/raw_abc.md"],
                "Links to [[beta]] and [[beta|Beta alias]] and [[gamma#Section]] and [[other::thing]] and [[alpha]].",
            ),
        );
        write(
            root,
            "wiki/concepts/beta.md",
            &page("Beta", &["travel"], &[], "Back to [[alpha]]."),
        );
        write(
            root,
            "wiki/entities/gamma.md",
            &page(
                "Gamma",
                &[],
                &["/raw/raw_missing.md", "/../../../etc/passwd"],
                "See [[alpha]] and [[nowhere]].",
            ),
        );
        write(root, "wiki/concepts/broken.md", "no frontmatter [[alpha]]");
        dir
    }

    #[test]
    fn builds_nodes_of_every_kind() {
        let dir = sample_vault();
        let graph = build_graph(dir.path()).unwrap();

        let kind = |id: &str| graph.node(id).map(|n| n.kind);
        assert_eq!(kind("alpha"), Some("concept"));
        assert_eq!(kind("gamma"), Some("entity"));
        assert_eq!(kind("#travel"), Some("tag"));
        assert_eq!(kind("raw:raw_abc"), Some("raw"));
        assert_eq!(kind("raw:raw_missing"), Some("raw"));
        assert_eq!(kind("other::thing"), Some("cross_vault"));
        assert_eq!(kind("nowhere"), Some("missing"));
        assert_eq!(kind("broken"), Some("concept"));
        assert_eq!(graph.unparsed_pages, 1);

        let raw = graph.node("raw:raw_abc").unwrap();
        assert_eq!(raw.title, "https://example.com/a");
        assert_eq!(raw.path.as_deref(), Some("raw/raw_abc.md"));
        assert!(graph.node("raw:raw_missing").unwrap().path.is_none());
        // A `sources[].resource` escaping the vault is a "not found" raw
        // node, never a read outside the vault.
        let escaping = graph.node("raw:passwd").unwrap();
        assert!(escaping.path.is_none());
        assert_eq!(escaping.title, "passwd");
        assert_eq!(
            graph.node("alpha").unwrap().path.as_deref(),
            Some("wiki/concepts/alpha.md")
        );
    }

    #[test]
    fn links_are_distinct_and_self_links_and_anchors_are_normalized() {
        let dir = sample_vault();
        let graph = build_graph(dir.path()).unwrap();

        let has = |s: &str, t: &str, k: &str| {
            graph
                .links
                .iter()
                .any(|l| l.source == s && l.target == t && l.kind == k)
        };
        // [[beta]] + [[beta|alias]] collapse to one link
        assert_eq!(
            graph
                .links
                .iter()
                .filter(|l| l.source == "alpha" && l.target == "beta")
                .count(),
            1
        );
        assert!(has("alpha", "gamma", "wikilink")); // anchor stripped
        assert!(has("alpha", "other::thing", "cross_vault"));
        assert!(has("alpha", "#travel", "tag"));
        assert!(has("alpha", "raw:raw_abc", "source"));
        assert!(!has("alpha", "alpha", "wikilink")); // self-link dropped
        assert!(has("broken", "alpha", "wikilink")); // unparsed page still linked
    }

    #[test]
    fn degrees_backlinks_and_hub_rank() {
        let dir = sample_vault();
        let graph = build_graph(dir.path()).unwrap();

        // alpha is linked from beta, gamma, broken → in_degree 3, the top hub.
        let alpha = graph.node("alpha").unwrap();
        assert_eq!(alpha.in_degree, 3);
        assert_eq!(alpha.hub_rank, 1);
        assert_eq!(alpha.out_degree, 6); // beta, gamma, other::thing, #travel, #ai, raw:raw_abc

        let travel = graph.node("#travel").unwrap();
        assert_eq!(travel.in_degree, 2);
        assert_eq!(travel.hub_rank, 2);

        let nowhere = graph.node("nowhere").unwrap();
        assert_eq!(nowhere.in_degree, 1);
        assert_eq!(nowhere.hub_rank, 3);

        // out-only nodes share the last rank
        let broken = graph.node("broken").unwrap();
        assert_eq!(broken.in_degree, 0);
        assert_eq!(broken.hub_rank, 4);

        let mut backlinks: Vec<&str> = graph
            .backlinks_of("alpha")
            .into_iter()
            .map(|n| n.id.as_str())
            .collect();
        backlinks.sort();
        assert_eq!(backlinks, vec!["beta", "broken", "gamma"]);

        let outlinks: Vec<&str> = graph
            .outlinks_of("beta")
            .into_iter()
            .map(|n| n.id.as_str())
            .collect();
        assert_eq!(outlinks, vec!["#travel", "alpha"]);
    }

    #[test]
    fn empty_vault_is_an_empty_graph() {
        let dir = tempfile::tempdir().unwrap();
        let graph = build_graph(dir.path()).unwrap();
        assert!(graph.nodes.is_empty());
        assert!(graph.links.is_empty());
    }

    #[test]
    fn strip_anchor_cases() {
        assert_eq!(strip_anchor("foo#Bar"), "foo");
        assert_eq!(strip_anchor("foo^blk"), "foo");
        assert_eq!(strip_anchor("foo"), "foo");
        assert_eq!(strip_anchor("#only"), "");
    }
}
