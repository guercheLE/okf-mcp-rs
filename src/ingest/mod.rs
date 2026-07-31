pub mod frontmatter;
pub mod local;
pub mod pipeline;
pub mod web;

pub use pipeline::{DeleteOutcome, delete_source, process_ingest};
