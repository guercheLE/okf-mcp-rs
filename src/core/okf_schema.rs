//! Single source of truth for the OKF frontmatter schema version, so
//! `raw/` blobs and the compiled `wiki/`/`okf.json` bundle can't drift
//! apart the way they used to (raw blobs hardcoded `"1.0"`, everything
//! else hardcoded `"0.2"` — see `docs/okf-pipeline-design.md`'s reference
//! implementation, which uses `"0.2"` throughout).

pub const OKF_SCHEMA_VERSION: &str = "0.2";
