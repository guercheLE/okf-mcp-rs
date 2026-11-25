//! Each `okf-mcp` tool's actual logic, grouped by stage — the thin
//! `#[tool(...)]` methods in `mcp_server.rs` just destructure what each of
//! these needs from `OkfServer` and call straight through.

pub(crate) mod compile;
pub(crate) mod ingest;
pub(crate) mod search;
pub(crate) mod vault;
