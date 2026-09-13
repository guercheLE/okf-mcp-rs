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
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::core::vault_resolver::wiki_content_dirs;
use crate::ingest::frontmatter::{raw_id_from_resource, resolve_raw_path};
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
    /// Dense rank by `in_degree`, descending: 1 = the most linked-to node in
    /// the vault (of any kind). Nodes with equal `in_degree` share a rank.
    pub hub_rank: usize,
    /// Number of distinct neighbours, direction ignored (what Obsidian sizes
    /// nodes by).
    pub degree: usize,
    /// Betweenness centrality on the undirected graph, normalised to
    /// `0.0..=1.0` (1.0 = the vault's top bridge). *This* is the measure of
    /// "glue": how many shortest paths between other nodes run through this
    /// one — the nodes whose removal actually splits the graph into islands,
    /// which neither in- nor out-degree reliably identifies (a tag linked
    /// from 40 pages of one cluster has a huge in-degree but bridges
    /// nothing). Sampled (Brandes with pivots) above `BETWEENNESS_EXACT_MAX`
    /// nodes so big vaults stay fast.
    pub betweenness: f64,
    /// Articulation point: removing this node alone disconnects its
    /// component (Tarjan). The strictest kind of hub.
    pub articulation: bool,
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

/// Best-effort title and resolved path for a raw source: its `source_url:`
/// if the raw blob resolves and parses, else `raw_id` itself as the title
/// and `None` for the path. Never fails — a raw node with a plain title
/// beats a missing node. Resolution goes through `resolve_raw_path`
/// (identity, not a literal path built from `sources[].resource`), which
/// also means a `resource` value that escapes the vault (a crafted `../`)
/// can never make this read outside it: only a real `raw_id` match under
/// `./raw/` is ever opened.
pub(crate) fn raw_source_title(vault_root: &Path, raw_id: &str) -> (String, Option<PathBuf>) {
    let Ok(path) = resolve_raw_path(vault_root, raw_id) else {
        return (raw_id.to_string(), None);
    };
    let Ok(content) = std::fs::read_to_string(&path) else {
        return (raw_id.to_string(), Some(path));
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
        .unwrap_or_else(|| raw_id.to_string());
    (title, Some(path))
}

struct PendingNode {
    title: String,
    kind: &'static str,
    r#type: Option<String>,
    description: Option<String>,
    tags: Vec<String>,
    path: Option<String>,
}

/// Above this many nodes betweenness is estimated from a fixed sample of
/// pivot sources instead of every node (Brandes' algorithm is O(V·E) exact).
const BETWEENNESS_EXACT_MAX: usize = 4000;
const BETWEENNESS_PIVOTS: usize = 1500;

#[derive(Debug, Clone, Copy, Default)]
struct StructuralMetrics {
    degree: usize,
    betweenness: f64,
    articulation: bool,
}

/// Undirected degree, normalised betweenness centrality (Brandes 2001) and
/// articulation points (Tarjan) for every node. `ids` fixes the node
/// numbering; `links` are treated as undirected and de-duplicated.
fn structural_metrics<'a>(
    ids: &[&'a str],
    links: &HashSet<GraphLink>,
) -> HashMap<&'a str, StructuralMetrics> {
    let n = ids.len();
    let index: HashMap<&str, usize> = ids.iter().enumerate().map(|(i, id)| (*id, i)).collect();
    let mut adjacency: Vec<Vec<usize>> = vec![Vec::new(); n];
    for link in links {
        let (Some(&a), Some(&b)) = (
            index.get(link.source.as_str()),
            index.get(link.target.as_str()),
        ) else {
            continue;
        };
        if a != b {
            adjacency[a].push(b);
            adjacency[b].push(a);
        }
    }
    for list in &mut adjacency {
        list.sort_unstable();
        list.dedup();
    }

    // Brandes: accumulate pair-dependencies from each source's BFS DAG.
    let mut betweenness = vec![0.0f64; n];
    let sources: Vec<usize> = if n <= BETWEENNESS_EXACT_MAX {
        (0..n).collect()
    } else {
        // Deterministic stride sample — no RNG dependency, reproducible.
        let stride = n.div_ceil(BETWEENNESS_PIVOTS).max(1);
        (0..n).step_by(stride).collect()
    };
    let mut sigma = vec![0.0f64; n];
    let mut dist = vec![usize::MAX; n];
    let mut delta = vec![0.0f64; n];
    let mut predecessors: Vec<Vec<usize>> = vec![Vec::new(); n];
    let mut order: Vec<usize> = Vec::with_capacity(n);
    let mut queue: std::collections::VecDeque<usize> = std::collections::VecDeque::new();
    for &s in &sources {
        for i in 0..n {
            sigma[i] = 0.0;
            dist[i] = usize::MAX;
            delta[i] = 0.0;
            predecessors[i].clear();
        }
        order.clear();
        sigma[s] = 1.0;
        dist[s] = 0;
        queue.push_back(s);
        while let Some(v) = queue.pop_front() {
            order.push(v);
            for &w in &adjacency[v] {
                if dist[w] == usize::MAX {
                    dist[w] = dist[v] + 1;
                    queue.push_back(w);
                }
                if dist[w] == dist[v] + 1 {
                    sigma[w] += sigma[v];
                    predecessors[w].push(v);
                }
            }
        }
        while let Some(w) = order.pop() {
            for &v in &predecessors[w] {
                delta[v] += sigma[v] / sigma[w] * (1.0 + delta[w]);
            }
            if w != s {
                betweenness[w] += delta[w];
            }
        }
    }
    let max = betweenness.iter().copied().fold(0.0f64, f64::max);
    if max > 0.0 {
        for b in &mut betweenness {
            *b /= max;
        }
    }

    // Tarjan articulation points, iterative to be safe on deep graphs.
    let mut articulation = vec![false; n];
    let mut disc = vec![usize::MAX; n];
    let mut low = vec![0usize; n];
    let mut time = 0usize;
    for root in 0..n {
        if disc[root] != usize::MAX {
            continue;
        }
        let mut root_children = 0usize;
        // stack of (node, parent, next-neighbour-index)
        let mut stack: Vec<(usize, usize, usize)> = vec![(root, usize::MAX, 0)];
        disc[root] = time;
        low[root] = time;
        time += 1;
        while let Some(&mut (v, parent, ref mut next)) = stack.last_mut() {
            if *next < adjacency[v].len() {
                let w = adjacency[v][*next];
                *next += 1;
                if disc[w] == usize::MAX {
                    disc[w] = time;
                    low[w] = time;
                    time += 1;
                    stack.push((w, v, 0));
                } else if w != parent {
                    low[v] = low[v].min(disc[w]);
                }
            } else {
                stack.pop();
                if let Some(&(p, _, _)) = stack.last() {
                    low[p] = low[p].min(low[v]);
                    if p == root {
                        root_children += 1;
                    } else if low[v] >= disc[p] {
                        articulation[p] = true;
                    }
                }
            }
        }
        articulation[root] = root_children > 1;
    }

    ids.iter()
        .enumerate()
        .map(|(i, id)| {
            (
                *id,
                StructuralMetrics {
                    degree: adjacency[i].len(),
                    betweenness: betweenness[i],
                    articulation: articulation[i],
                },
            )
        })
        .collect()
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
            let Some(raw_id) = raw_id_from_resource(resource) else {
                continue;
            };
            let id = format!("raw:{raw_id}");
            nodes.entry(id.clone()).or_insert_with(|| {
                let (title, resolved) = raw_source_title(vault_root, &raw_id);
                let relative = resolved.map(|path| {
                    path.strip_prefix(vault_root)
                        .unwrap_or(&path)
                        .to_string_lossy()
                        .replace('\\', "/")
                });
                PendingNode {
                    title,
                    kind: "raw",
                    r#type: None,
                    description: relative.is_none().then(|| "raw file not found".to_string()),
                    tags: Vec::new(),
                    path: relative,
                }
            });
            links.insert(GraphLink {
                source: page.slug.clone(),
                target: id,
                kind: "source",
            });
        }
    }

    // Degrees, hub rank, and the structural measures.
    let structure: HashMap<String, StructuralMetrics> = {
        let ids: Vec<&str> = nodes.keys().map(String::as_str).collect();
        structural_metrics(&ids, &links)
            .into_iter()
            .map(|(id, m)| (id.to_string(), m))
            .collect()
    };
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
            let metrics = structure.get(id.as_str()).copied().unwrap_or_default();
            GraphNode {
                hub_rank: rank_of[&ind],
                in_degree: ind,
                out_degree: out_degree.get(id.as_str()).copied().unwrap_or(0),
                degree: metrics.degree,
                betweenness: metrics.betweenness,
                articulation: metrics.articulation,
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
    fn structural_metrics_find_bridges_not_just_popular_nodes() {
        // Two triangles {a,b,c} and {d,e,f} joined only through `bridge`:
        //   a-b, b-c, c-a, c-bridge, bridge-d, d-e, e-f, f-d
        // plus a "popular" leaf hub `tag` linked from a, b, c (in-degree 3,
        // but bridging nothing).
        let ids = ["a", "b", "c", "bridge", "d", "e", "f", "tag"];
        let mut links = HashSet::new();
        for (s, t) in [
            ("a", "b"),
            ("b", "c"),
            ("c", "a"),
            ("c", "bridge"),
            ("bridge", "d"),
            ("d", "e"),
            ("e", "f"),
            ("f", "d"),
            ("a", "tag"),
            ("b", "tag"),
            ("c", "tag"),
        ] {
            links.insert(GraphLink {
                source: s.to_string(),
                target: t.to_string(),
                kind: "wikilink",
            });
        }
        let m = structural_metrics(&ids, &links);
        assert_eq!(m["tag"].degree, 3);
        assert_eq!(m["c"].degree, 4);
        assert_eq!(m["bridge"].degree, 2);
        // The bridge (and its two anchors) are articulation points; the tag is not.
        assert!(m["bridge"].articulation);
        assert!(m["c"].articulation);
        assert!(m["d"].articulation);
        assert!(!m["tag"].articulation);
        assert!(!m["a"].articulation);
        // Betweenness ranks the bridge/anchors far above the popular tag.
        assert!(m["bridge"].betweenness > 0.9, "{:?}", m["bridge"]);
        assert!(m["c"].betweenness > m["tag"].betweenness);
        assert_eq!(m["tag"].betweenness, 0.0);
        let top = m.values().map(|x| x.betweenness).fold(0.0, f64::max);
        assert!((top - 1.0).abs() < 1e-9);
    }

    #[test]
    fn graph_nodes_carry_structural_metrics() {
        let dir = sample_vault();
        let graph = build_graph(dir.path()).unwrap();
        let alpha = graph.node("alpha").unwrap();
        // alpha ↔ beta, gamma, broken, other::thing, #travel, #ai, raw:raw_abc
        assert_eq!(alpha.degree, 7);
        assert!(alpha.betweenness > 0.99, "{}", alpha.betweenness);
        assert!(alpha.articulation);
        assert!(!graph.node("#ai").unwrap().articulation);
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
