//! Library crate shared by every `[[bin]]` target in this project
//! (`okf-mcp`, `okf-mcp-healthcheck`) — each `[[bin]]` is its own separate
//! crate, so anything they need in common has to live here rather than as
//! plain `mod` declarations in `main.rs`, which only `main.rs` itself could
//! see. `cli` is deliberately not declared here: it's `okf-mcp`'s own
//! entry-point wiring, used by no other binary.

pub mod auth;
pub mod compiler;
pub mod core;
pub mod http;
pub mod ingest;
pub mod manifest;
pub mod search;
pub mod services;
pub mod storage;
pub mod validator;
