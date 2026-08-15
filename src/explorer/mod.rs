//! `okf-mcp explore`: an Obsidian-style, browser-based explorer for one
//! vault — a 3D force-directed link graph (notes, tags, raw sources,
//! unresolved links), a note pane with rendered markdown + backlinks, and
//! search backed by the vault's own hybrid BM25 + vector index
//! (`search::query::hybrid_search`), not a client-side re-implementation.
//!
//! Served by a small local axum server (`server`) with one embedded,
//! fully-offline HTML page (`assets/explorer.html` + a vendored
//! `3d-force-graph` bundle) and a tiny JSON API. The graph itself is built
//! by `graph` from the same wikilink/frontmatter parsers the linter uses.

pub mod graph;
pub mod render;
pub mod server;

pub use graph::{Graph, GraphLink, GraphNode, build_graph};
pub use render::{PageView, render_page};
pub use server::{ExplorerHandle, serve};
