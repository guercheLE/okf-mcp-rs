//! Renders one vault file for the explorer's note pane: markdown → HTML
//! (`pulldown-cmark`), with `[[wikilinks]]` rewritten to in-app anchors the
//! frontend can intercept, plus the page's *complete* frontmatter as JSON
//! (a generic, lenient YAML parse — so any field beyond what
//! `WikiFrontmatter` models, or a page whose frontmatter doesn't parse
//! under the stricter validator at all, still shows up in the properties
//! table) and its backlinks/outlinks from the graph.

use std::collections::HashSet;
use std::path::Path;

use pulldown_cmark::{CowStr, Event, LinkType, Options, Parser, Tag, TagEnd, html};
use serde::Serialize;

use crate::core::vault_resolver::sandbox_path;
use crate::ingest::frontmatter::resolve_raw_path;

use super::graph::Graph;

#[derive(Debug, Clone, Serialize)]
pub struct LinkedNode {
    pub id: String,
    pub title: String,
    pub kind: &'static str,
}

#[derive(Debug, Clone, Serialize)]
pub struct PageView {
    pub id: String,
    pub title: String,
    pub kind: &'static str,
    pub path: String,
    /// The whole frontmatter block as JSON (object), or `null` if the file
    /// has none / it doesn't parse.
    pub frontmatter: serde_json::Value,
    pub html: String,
    pub backlinks: Vec<LinkedNode>,
    pub outlinks: Vec<LinkedNode>,
}

/// Splits `---\n<yaml>\n---\n<body>`. Lenient on purpose (unlike
/// `validator::frontmatter::parse_wiki_page`): the explorer should still
/// *show* a page whose frontmatter is broken, with the raw text as body.
fn split_frontmatter(content: &str) -> (serde_json::Value, &str) {
    let Some(after_open) = content.strip_prefix("---\n") else {
        return (serde_json::Value::Null, content);
    };
    let Some(close_at) = after_open.find("\n---\n") else {
        return (serde_json::Value::Null, content);
    };
    let yaml = &after_open[..close_at];
    let body = &after_open[close_at + "\n---\n".len()..];
    let value = serde_yaml::from_str::<serde_yaml::Value>(yaml)
        .ok()
        .and_then(|v| serde_json::to_value(v).ok())
        .unwrap_or(serde_json::Value::Null);
    (value, body)
}

fn escape_html(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

/// Turns a wikilink destination (`beta`, `beta#Heading`, `beta^block`,
/// `vault::concept#x`) into the graph node id it addresses.
fn wikilink_target(dest: &str) -> String {
    match dest.split_once("::") {
        Some((vault, concept)) => {
            let concept = concept.split(['#', '^']).next().unwrap_or(concept).trim();
            format!("{vault}::{concept}")
        }
        None => dest
            .split(['#', '^'])
            .next()
            .unwrap_or(dest)
            .trim()
            .to_string(),
    }
}

/// Only these URL schemes survive into `href`/`src`; anything else
/// (`javascript:`, `data:`, `vbscript:`) is dropped to `#`. Relative and
/// fragment URLs pass.
fn safe_url(url: &str) -> bool {
    let lower = url.trim().to_ascii_lowercase();
    match lower.split_once(':') {
        None => true,
        Some((scheme, _)) => {
            !scheme.is_empty()
                && scheme
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '+' || c == '-' || c == '.')
                && matches!(scheme, "http" | "https" | "mailto" | "ftp" | "file")
                || lower.starts_with('/')
                || lower.starts_with('#')
                || lower.starts_with('.')
        }
    }
}

/// Markdown → HTML for the note pane. Everything the vault didn't write as
/// Markdown is neutralised: raw HTML blocks/inline HTML become escaped text
/// (ingested web pages routinely carry inline HTML, and the pane renders via
/// `innerHTML`), and link/image URLs with a script-capable scheme are
/// dropped. `[[wikilinks]]` are parsed by pulldown-cmark itself
/// (`ENABLE_WIKILINKS`, so a `[[..]]` inside a code span or fenced block is
/// left alone) and emitted as `<a class="wikilink" href="#/note/<id>"
/// data-slug="<id>">` anchors the frontend intercepts; targets `is_missing`
/// says don't resolve get the extra `missing` class, like Obsidian's
/// unresolved-link styling.
pub fn render_markdown(body: &str, is_missing: &dyn Fn(&str) -> bool) -> String {
    let mut options = Options::empty();
    options.insert(Options::ENABLE_TABLES);
    options.insert(Options::ENABLE_FOOTNOTES);
    options.insert(Options::ENABLE_STRIKETHROUGH);
    options.insert(Options::ENABLE_TASKLISTS);
    options.insert(Options::ENABLE_HEADING_ATTRIBUTES);
    options.insert(Options::ENABLE_WIKILINKS);

    // One entry per currently-open link: `true` when it's a wikilink we
    // opened ourselves and must close ourselves.
    let mut open_links: Vec<bool> = Vec::new();
    let mut events: Vec<Event<'_>> = Vec::new();

    for event in Parser::new_ext(body, options) {
        match event {
            Event::Html(raw) | Event::InlineHtml(raw) => events.push(Event::Text(raw)),
            Event::Start(Tag::Link {
                link_type: LinkType::WikiLink { .. },
                dest_url,
                ..
            }) => {
                let id = wikilink_target(&dest_url);
                let class = if is_missing(&id) {
                    "wikilink missing"
                } else {
                    "wikilink"
                };
                let id = escape_html(&id);
                events.push(Event::Html(
                    format!("<a class=\"{class}\" href=\"#/note/{id}\" data-slug=\"{id}\">").into(),
                ));
                open_links.push(true);
            }
            Event::Start(Tag::Link {
                link_type,
                dest_url,
                title,
                id,
            }) => {
                let dest_url = if safe_url(&dest_url) {
                    dest_url
                } else {
                    CowStr::Borrowed("#")
                };
                events.push(Event::Start(Tag::Link {
                    link_type,
                    dest_url,
                    title,
                    id,
                }));
                open_links.push(false);
            }
            Event::End(TagEnd::Link) => {
                if open_links.pop() == Some(true) {
                    events.push(Event::Html("</a>".into()));
                } else {
                    events.push(Event::End(TagEnd::Link));
                }
            }
            Event::Start(Tag::Image {
                link_type,
                dest_url,
                title,
                id,
            }) => {
                let dest_url = if safe_url(&dest_url) {
                    dest_url
                } else {
                    CowStr::Borrowed("#")
                };
                events.push(Event::Start(Tag::Image {
                    link_type,
                    dest_url,
                    title,
                    id,
                }));
            }
            other => events.push(other),
        }
    }

    let mut out = String::with_capacity(body.len() * 2);
    html::push_html(&mut out, events.into_iter());
    out
}

/// Renders the node `id` (a page slug or `raw:<raw_id>`) from `graph`.
/// Reads the file through `sandbox_path`, so a crafted id can't escape the
/// vault.
///
/// A `raw:<raw_id>` that isn't a graph node (a raw source no page cites
/// yet — search still finds those) resolves via `resolve_raw_path` rather
/// than a literal `raw/<raw_id>.md` path, so every search hit is openable
/// regardless of whether its physical filename carries a slug.
pub fn render_page(vault_root: &Path, graph: &Graph, id: &str) -> anyhow::Result<PageView> {
    let uncited_raw;
    let node = match graph.node(id) {
        Some(node) => node,
        None => {
            let raw_id = id
                .strip_prefix("raw:")
                .filter(|raw_id| !raw_id.is_empty() && !raw_id.contains(['/', '\\']))
                .ok_or_else(|| anyhow::anyhow!("no node '{id}' in this vault"))?;
            let path = resolve_raw_path(vault_root, raw_id)
                .map_err(|_| anyhow::anyhow!("no node '{id}' in this vault"))?;
            let relative = path
                .strip_prefix(vault_root)
                .unwrap_or(&path)
                .to_string_lossy()
                .replace('\\', "/");
            let (title, _) = super::graph::raw_source_title(vault_root, raw_id);
            uncited_raw = super::graph::GraphNode {
                id: id.to_string(),
                title,
                kind: "raw",
                r#type: None,
                description: Some("raw source not cited by any wiki page yet".to_string()),
                tags: Vec::new(),
                path: Some(relative),
                in_degree: 0,
                out_degree: 0,
                hub_rank: 0,
                degree: 0,
                betweenness: 0.0,
                articulation: false,
            };
            &uncited_raw
        }
    };
    let relative = node
        .path
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("'{id}' is a {} node with no file to render", node.kind))?;
    let path = sandbox_path(vault_root, relative)?;
    let content = std::fs::read_to_string(&path)
        .map_err(|err| anyhow::anyhow!("cannot read '{relative}': {err}"))?;

    let (frontmatter, body) = split_frontmatter(&content);
    let known: HashSet<&str> = graph
        .nodes
        .iter()
        .filter(|n| n.kind != "missing")
        .map(|n| n.id.as_str())
        .collect();
    let html = render_markdown(body, &|target| !known.contains(target));

    let to_linked = |n: &super::graph::GraphNode| LinkedNode {
        id: n.id.clone(),
        title: n.title.clone(),
        kind: n.kind,
    };

    Ok(PageView {
        id: node.id.clone(),
        title: node.title.clone(),
        kind: node.kind,
        path: relative.to_string(),
        frontmatter,
        html,
        backlinks: graph.backlinks_of(id).into_iter().map(to_linked).collect(),
        outlinks: graph.outlinks_of(id).into_iter().map(to_linked).collect(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::explorer::graph::build_graph;

    fn render(body: &str, missing: &[&str]) -> String {
        render_markdown(body, &|t| missing.contains(&t))
    }

    #[test]
    fn rewrites_plain_alias_anchor_and_cross_vault_links() {
        let out = render(
            "a [[beta]] b [[beta|Nice]] c [[beta#Head]] d [[other::thing]] e [[ghost]] f [[]] g",
            &["ghost"],
        );
        assert!(
            out.contains(r##"<a class="wikilink" href="#/note/beta" data-slug="beta">beta</a>"##),
            "{out}"
        );
        assert!(out.contains(r##"data-slug="beta">Nice</a>"##), "{out}");
        assert!(out.contains(r##"data-slug="beta">beta#Head</a>"##), "{out}");
        assert!(
            out.contains(r##"data-slug="other::thing">other::thing</a>"##),
            "{out}"
        );
        assert!(
            out.contains(
                r##"<a class="wikilink missing" href="#/note/ghost" data-slug="ghost">ghost</a>"##
            ),
            "{out}"
        );
        assert!(out.contains("f [[]] g"), "empty link left alone: {out}");
    }

    #[test]
    fn wikilinks_inside_code_are_left_alone() {
        let out = render("x `[[a]]` y\n\n```\n[[b]]\n```\n", &[]);
        assert!(!out.contains("wikilink"), "{out}");
        assert!(out.contains("<code>[[a]]</code>"), "{out}");
        assert!(out.contains("[[b]]"), "{out}");
    }

    #[test]
    fn raw_html_and_script_urls_are_neutralised() {
        let out = render(
            "hi\n\n<img src=x onerror=alert(1)>\n\ninline <b onclick=\"x()\">bold</b> [go](javascript:alert(1)) ![i](javascript:alert(2)) [ok](https://example.com)\n",
            &[],
        );
        assert!(!out.contains("<img src=x"), "{out}");
        assert!(!out.contains("<b onclick"), "{out}");
        assert!(!out.contains("javascript:"), "{out}");
        assert!(out.contains("&lt;img src=x onerror=alert(1)&gt;"), "{out}");
        assert!(
            out.contains(r#"<a href="https://example.com">ok</a>"#),
            "{out}"
        );
    }

    #[test]
    fn escapes_html_in_link_targets() {
        let out = render("[[<script>|<b>]]", &[]);
        assert!(!out.contains("<script>"), "{out}");
        assert!(out.contains("&lt;script&gt;"), "{out}");
        assert!(out.contains("&lt;b&gt;"), "{out}");
    }

    #[test]
    fn markdown_renders_footnotes_and_tables() {
        let html = render(
            "# T\n\nx[^1]\n\n| a | b |\n|---|---|\n| 1 | 2 |\n\n[^1]: note\n",
            &[],
        );
        assert!(html.contains("<h1>T</h1>"));
        assert!(html.contains("<table>"));
        assert!(html.contains("footnote"));
    }

    #[test]
    fn mermaid_fences_keep_their_language_class() {
        let out = render("```mermaid\nflowchart LR\n  A --> B\n```\n", &[]);
        assert!(
            out.contains(r#"<pre><code class="language-mermaid">"#),
            "{out}"
        );
        assert!(out.contains("A --&gt; B"), "{out}");
    }

    #[test]
    fn safe_url_cases() {
        assert!(safe_url("https://x"));
        assert!(safe_url("/raw/x.md"));
        assert!(safe_url("#frag"));
        assert!(safe_url("./rel.md"));
        assert!(safe_url("mailto:a@b"));
        assert!(!safe_url("javascript:alert(1)"));
        assert!(!safe_url("JavaScript:alert(1)"));
        assert!(!safe_url("data:text/html,x"));
        assert!(!safe_url("vbscript:x"));
    }

    #[test]
    fn render_page_end_to_end() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("wiki/concepts")).unwrap();
        std::fs::create_dir_all(root.join("raw")).unwrap();
        std::fs::write(
            root.join("wiki/concepts/alpha.md"),
            "---\ntype: Concept\ntitle: Alpha\ngenerated:\n  by: okf-mcp-compiler\n  at: 2026-08-08T00:00:00Z\nsources:\n  - resource: /raw/raw_x.md\n---\n\n# Alpha\n\nSee [[beta]] and [[ghost]].\n",
        )
        .unwrap();
        std::fs::write(
            root.join("wiki/concepts/beta.md"),
            "---\ntype: Concept\ntitle: Beta\n---\n\nBack to [[alpha]].\n",
        )
        .unwrap();
        std::fs::write(
            root.join("raw/raw_x.md"),
            "---\ntype: raw_source\nsource_url: https://x\n---\n\nRaw **body**\n",
        )
        .unwrap();

        let graph = build_graph(root).unwrap();
        let view = render_page(root, &graph, "alpha").unwrap();
        assert_eq!(view.title, "Alpha");
        assert_eq!(view.path, "wiki/concepts/alpha.md");
        assert_eq!(view.frontmatter["generated"]["by"], "okf-mcp-compiler");
        assert!(
            view.html
                .contains(r##"class="wikilink" href="#/note/beta""##)
        );
        assert!(
            view.html
                .contains(r##"class="wikilink missing" href="#/note/ghost""##)
        );
        assert_eq!(view.backlinks.len(), 1);
        assert_eq!(view.backlinks[0].id, "beta");
        let mut out: Vec<&str> = view.outlinks.iter().map(|l| l.id.as_str()).collect();
        out.sort();
        assert_eq!(out, vec!["beta", "ghost", "raw:raw_x"]);

        let raw = render_page(root, &graph, "raw:raw_x").unwrap();
        assert_eq!(raw.kind, "raw");
        assert!(raw.html.contains("<strong>body</strong>"));
        assert_eq!(raw.frontmatter["source_url"], "https://x");
        assert_eq!(raw.backlinks[0].id, "alpha");

        assert!(render_page(root, &graph, "ghost").is_err());
        assert!(render_page(root, &graph, "#tag").is_err());
        assert!(render_page(root, &graph, "../etc/passwd").is_err());

        // A raw file no page cites is not a node, but is still renderable.
        std::fs::write(
            root.join("raw/raw_uncited.md"),
            "---\ntype: raw_source\nsource_url: https://u\n---\n\nlonely\n",
        )
        .unwrap();
        assert!(graph.node("raw:raw_uncited").is_none());
        let lonely = render_page(root, &graph, "raw:raw_uncited").unwrap();
        assert_eq!(lonely.title, "https://u");
        assert!(lonely.html.contains("lonely"));
        assert!(lonely.backlinks.is_empty());
        assert!(render_page(root, &graph, "raw:../wiki/concepts/alpha").is_err());
        assert!(render_page(root, &graph, "raw:nope").is_err());
    }
}
