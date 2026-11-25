//! Library crate shared by every `[[bin]]` target in this project
//! (`okf-mcp`, `okf-mcp-healthcheck`) — each `[[bin]]` is its own separate
//! crate, so anything they need in common has to live here rather than as
//! plain `mod` declarations in `main.rs`, which only `main.rs` itself could
//! see. `cli` is deliberately not declared here: it's `okf-mcp`'s own
//! entry-point wiring, used by no other binary.

// `not(test)`, rather than a bare `warn(...)`, so this only fires on
// production code: `cfg(test)` is set for the *entire* crate when compiled
// as the unit-test binary, so this attribute is simply absent from that
// build — leaving every test's `.unwrap()` (the overwhelming majority of
// this crate's `.unwrap()` calls) alone, while still catching any new
// production one under CI's `cargo clippy -- -D warnings`.
#![cfg_attr(not(test), warn(clippy::unwrap_used))]

pub mod auth;
pub mod compiler;
pub mod core;
pub mod explorer;
pub mod http;
pub mod ingest;
pub mod manifest;
pub mod search;
pub mod services;
pub mod storage;
pub mod validator;
