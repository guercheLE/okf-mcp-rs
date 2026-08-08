pub mod concurrency;
pub mod driver;
pub mod link_fix;
pub mod operations;
pub mod prompts;
pub mod provider;
pub mod synthesize;
pub mod token_estimate;

pub use driver::{
    CompileOptions, CompileReport, compile, list_models, regenerate_index, resolve_model_spec,
    vault_provider_options, vault_provider_options_for_provider,
};
pub use link_fix::{LinkFixOutcome, LinkFixReport, LinkFixStatus, fix_broken_links};
pub use synthesize::{JobKind, NextBatch, StoredJob, StoredJobKind, SynthesizeJob, next_batch};
