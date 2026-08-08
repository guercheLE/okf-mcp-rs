//! Job-material assembly for the client-driven `okf-synthesize-next` /
//! `okf-synthesize-submit` MCP tool chain: gathers exactly the same raw
//! material and builds the same prompts `compile()`/`fix_broken_links()` do,
//! but stops short of calling an LLM — the calling MCP client's own model
//! does that step, in its own turn, then calls `okf-synthesize-submit` with
//! its structured result. See `core::mcp_server`'s `okf-synthesize-next`/
//! `okf-synthesize-submit` tool bodies for how a `NextBatch`'s jobs are
//! turned into a JSON response and how a submitted result is looked up
//! again via `StoredJob`.
//!
//! Reuses rather than duplicates: `driver::select_sources` (which raw
//! sources are pending), `driver::{read_raw_body, superseded_raw_id,
//! related_wiki_pages}` plus `token_estimate` (the same prompt-size
//! trimming `compile_one_source` applies), `prompts::build_compile_user_prompt`,
//! and `link_fix::{group_broken_links_by_slug, raw_ids_cited_by}` plus
//! `prompts::build_link_fix_user_prompt` for the fix phase.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::Instant;

use crate::manifest;
use crate::storage::fs_ops;
use crate::validator::lint_bundle;

use super::driver::{read_raw_body, related_wiki_pages, select_sources, superseded_raw_id};
use super::link_fix::{group_broken_links_by_slug, raw_ids_cited_by};
use super::prompts::{RawBlob, WikiPageRef, build_compile_user_prompt, build_link_fix_user_prompt};
use super::token_estimate::{self, DEFAULT_PROMPT_TOKEN_BUDGET};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobKind {
    Compile,
    Fix,
}

impl JobKind {
    pub fn as_str(self) -> &'static str {
        match self {
            JobKind::Compile => "compile",
            JobKind::Fix => "fix",
        }
    }
}

/// One unit of synthesis work handed to the MCP client.
pub struct SynthesizeJob {
    pub job_id: String,
    pub kind: JobKind,
    /// The exact user-prompt text `compile_one_source`/`fix_broken_links`
    /// would have sent to an LLM provider — handed to the client instead.
    pub prompt: String,
}

/// What a job's later `okf-synthesize-submit` call needs to apply its
/// result — stashed server-side (see `core::mcp_server::OkfServer`'s
/// `synthesize_jobs` map), keyed by `SynthesizeJob::job_id`. `Clone` so a
/// lookup can release the map's lock before doing any file I/O, instead of
/// holding it across an `.await`.
#[derive(Clone)]
pub enum StoredJobKind {
    Compile { uri: String },
    Fix { slug: String },
}

#[derive(Clone)]
pub struct StoredJob {
    pub vault_root: PathBuf,
    pub kind: StoredJobKind,
    pub created_at: Instant,
}

/// A slug with no cited source context in any page linking to it — nothing
/// to ground synthesis in, so it's surfaced for visibility rather than
/// turned into a job (same "never invent content with no source" guarantee
/// `link_fix::LinkFixStatus::SkippedNoSourceContext` gives the provider-
/// driven flow).
pub struct SkippedFix {
    pub slug: String,
    pub reason: &'static str,
}

pub struct NextBatch {
    /// "compile" while raw sources are still pending, "fix" once compiling
    /// is done but broken links remain, "done" once neither is true.
    pub phase: &'static str,
    pub jobs: Vec<(SynthesizeJob, StoredJob)>,
    pub skipped: Vec<SkippedFix>,
    /// How much more work is estimated to be left after this batch, in the
    /// current phase only (a phase transition on the next call isn't
    /// reflected here).
    pub remaining_estimate: usize,
}

/// Mirrors `driver::compile_one_source`'s material-gathering steps exactly,
/// minus the LLM call itself: same token-budget trimming
/// (`truncate_raw_blobs_to_shared_budget` caps active+superseded against one
/// *shared* budget, `trim_related_pages` drops least-relevant related pages
/// first), same `build_compile_user_prompt` call.
async fn build_compile_prompt(
    vault_root: &Path,
    superseded_id: Option<&str>,
    raw_id: &str,
) -> anyhow::Result<String> {
    let raw_content = read_raw_body(vault_root, raw_id)?;
    let superseded_full = match superseded_id {
        Some(prev_id) => read_raw_body(vault_root, prev_id)
            .ok()
            .map(|content| RawBlob {
                id: prev_id.to_string(),
                content,
            }),
        None => None,
    };
    let active_full = RawBlob {
        id: raw_id.to_string(),
        content: raw_content,
    };
    let related_full = related_wiki_pages(vault_root, &active_full.content).await?;

    let (active_raw, superseded) = token_estimate::truncate_raw_blobs_to_shared_budget(
        active_full,
        superseded_full,
        DEFAULT_PROMPT_TOKEN_BUDGET,
    );
    let related = token_estimate::trim_related_pages(
        related_full,
        &active_raw,
        superseded.as_ref(),
        DEFAULT_PROMPT_TOKEN_BUDGET,
    );

    Ok(build_compile_user_prompt(
        &active_raw,
        superseded.as_ref(),
        &related,
    ))
}

/// Builds up to `batch_size` synthesis jobs: pending raw sources first
/// (`diff_only` matches `okf-compile`'s own `diff` argument — `false` means
/// every active source, matching `rebuild --force`), then — only once none
/// remain — missing-wikilink-target jobs, then `phase: "done"`.
///
/// `next_job_id` is called once per job to mint its ID; `core::mcp_server`
/// backs it with a per-session atomic counter (job IDs are pure in-memory
/// correlation handles for this one session, not security tokens, so a
/// simple counter is sufficient — no need for random IDs or an extra
/// dependency).
pub async fn next_batch(
    vault_root: &Path,
    batch_size: usize,
    diff_only: bool,
    mut next_job_id: impl FnMut() -> String,
) -> anyhow::Result<NextBatch> {
    let manifest = manifest::store::load(vault_root)?;
    let pending_sources = select_sources(vault_root, &manifest, diff_only)?;

    if !pending_sources.is_empty() {
        let take = pending_sources.len().min(batch_size.max(1));
        let mut jobs = Vec::with_capacity(take);
        for (uri, raw_id) in &pending_sources[..take] {
            let superseded_id = superseded_raw_id(&manifest, uri);
            let prompt = build_compile_prompt(vault_root, superseded_id.as_deref(), raw_id).await?;
            let job_id = next_job_id();
            jobs.push((
                SynthesizeJob {
                    job_id: job_id.clone(),
                    kind: JobKind::Compile,
                    prompt,
                },
                StoredJob {
                    vault_root: vault_root.to_path_buf(),
                    kind: StoredJobKind::Compile { uri: uri.clone() },
                    created_at: Instant::now(),
                },
            ));
        }
        return Ok(NextBatch {
            phase: "compile",
            jobs,
            skipped: Vec::new(),
            remaining_estimate: pending_sources.len() - take,
        });
    }

    let lint = lint_bundle(vault_root)?;
    let by_slug = group_broken_links_by_slug(&lint);
    if by_slug.is_empty() {
        return Ok(NextBatch {
            phase: "done",
            jobs: Vec::new(),
            skipped: Vec::new(),
            remaining_estimate: 0,
        });
    }

    let take = by_slug.len().min(batch_size.max(1));
    let mut jobs = Vec::new();
    let mut skipped = Vec::new();
    for (slug, referencing_paths) in by_slug.iter().take(take) {
        let mut referencing_pages = Vec::new();
        for path in referencing_paths {
            if let Ok(content) = fs_ops::read_to_string(vault_root, path) {
                referencing_pages.push(WikiPageRef {
                    path: path.clone(),
                    content,
                });
            }
        }

        let raw_ids: HashSet<String> = raw_ids_cited_by(&referencing_pages);
        if raw_ids.is_empty() {
            skipped.push(SkippedFix {
                slug: slug.clone(),
                reason: "no_source_context",
            });
            continue;
        }

        let mut cited_raw_sources = Vec::new();
        for raw_id in raw_ids {
            if let Ok(content) = read_raw_body(vault_root, &raw_id) {
                cited_raw_sources.push(RawBlob {
                    id: raw_id,
                    content,
                });
            }
        }

        let prompt = build_link_fix_user_prompt(slug, &referencing_pages, &cited_raw_sources);
        let job_id = next_job_id();
        jobs.push((
            SynthesizeJob {
                job_id: job_id.clone(),
                kind: JobKind::Fix,
                prompt,
            },
            StoredJob {
                vault_root: vault_root.to_path_buf(),
                kind: StoredJobKind::Fix { slug: slug.clone() },
                created_at: Instant::now(),
            },
        ));
    }

    Ok(NextBatch {
        phase: "fix",
        jobs,
        skipped,
        remaining_estimate: by_slug.len() - take,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::Manifest;

    fn write_raw(vault_root: &Path, raw_id: &str, content: &str) {
        let dir = vault_root.join("raw");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(format!("{raw_id}.md")), content).unwrap();
    }

    fn write_concept(vault_root: &Path, slug: &str, sources: &[&str], body_extra: &str) {
        let dir = vault_root.join("wiki/concepts");
        std::fs::create_dir_all(&dir).unwrap();
        let sources_yaml = if sources.is_empty() {
            String::new()
        } else {
            let entries: String = sources
                .iter()
                .map(|s| format!("  - resource: \"{s}\"\n"))
                .collect();
            format!("sources:\n{entries}")
        };
        let content = format!(
            "---\ntype: concept\ntitle: \"{slug}\"\n{sources_yaml}---\n\n# {slug}\n\n{body_extra}\n"
        );
        std::fs::write(dir.join(format!("{slug}.md")), content).unwrap();
    }

    fn counter(start: usize) -> impl FnMut() -> String {
        let mut next = start;
        move || {
            let id = format!("job-{next}");
            next += 1;
            id
        }
    }

    #[tokio::test]
    async fn returns_compile_jobs_for_pending_raw_sources_first() {
        let vault = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(vault.path().join(".okf")).unwrap();
        write_raw(vault.path(), "raw_aaa", "Some raw content.");
        let mut manifest = Manifest::default();
        manifest.record_ingest("uri-a", "sha256:aaa", "raw_aaa", "t0");
        manifest::store::save(vault.path(), &manifest).unwrap();

        let batch = next_batch(vault.path(), 10, true, counter(1))
            .await
            .unwrap();
        assert_eq!(batch.phase, "compile");
        assert_eq!(batch.jobs.len(), 1);
        assert_eq!(batch.jobs[0].0.kind, JobKind::Compile);
        assert!(batch.jobs[0].0.prompt.contains("Some raw content."));
        assert_eq!(batch.remaining_estimate, 0);
        match &batch.jobs[0].1.kind {
            StoredJobKind::Compile { uri } => assert_eq!(uri, "uri-a"),
            _ => panic!("expected a Compile job"),
        }
    }

    #[tokio::test]
    async fn batch_size_caps_how_many_compile_jobs_come_back_at_once() {
        let vault = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(vault.path().join(".okf")).unwrap();
        let mut manifest = Manifest::default();
        for i in 0..5 {
            let raw_id = format!("raw_{i}");
            write_raw(vault.path(), &raw_id, "content");
            manifest.record_ingest(&format!("uri-{i}"), &format!("sha256:{i}"), &raw_id, "t0");
        }
        manifest::store::save(vault.path(), &manifest).unwrap();

        let batch = next_batch(vault.path(), 2, true, counter(1)).await.unwrap();
        assert_eq!(batch.jobs.len(), 2);
        assert_eq!(batch.remaining_estimate, 3);
    }

    #[tokio::test]
    async fn transitions_to_fix_phase_once_nothing_is_pending_to_compile() {
        let vault = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(vault.path().join(".okf")).unwrap();
        write_raw(vault.path(), "raw_aaa", "content");
        write_concept(vault.path(), "a", &["/raw/raw_aaa.md"], "See [[missing]].");

        let batch = next_batch(vault.path(), 10, true, counter(1))
            .await
            .unwrap();
        assert_eq!(batch.phase, "fix");
        assert_eq!(batch.jobs.len(), 1);
        assert_eq!(batch.jobs[0].0.kind, JobKind::Fix);
        match &batch.jobs[0].1.kind {
            StoredJobKind::Fix { slug } => assert_eq!(slug, "missing"),
            _ => panic!("expected a Fix job"),
        }
    }

    #[tokio::test]
    async fn a_broken_link_with_no_cited_source_context_is_skipped_not_a_job() {
        let vault = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(vault.path().join(".okf")).unwrap();
        // No `sources:` on this page at all — nothing to ground synthesis in.
        write_concept(vault.path(), "a", &[], "See [[missing]].");

        let batch = next_batch(vault.path(), 10, true, counter(1))
            .await
            .unwrap();
        assert_eq!(batch.phase, "fix");
        assert!(batch.jobs.is_empty());
        assert_eq!(batch.skipped.len(), 1);
        assert_eq!(batch.skipped[0].slug, "missing");
        assert_eq!(batch.skipped[0].reason, "no_source_context");
    }

    #[tokio::test]
    async fn reports_done_once_nothing_is_pending_or_broken() {
        let vault = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(vault.path().join(".okf")).unwrap();

        let batch = next_batch(vault.path(), 10, true, counter(1))
            .await
            .unwrap();
        assert_eq!(batch.phase, "done");
        assert!(batch.jobs.is_empty());
    }

    #[tokio::test]
    async fn each_job_gets_a_distinct_id_from_the_supplied_generator() {
        let vault = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(vault.path().join(".okf")).unwrap();
        let mut manifest = Manifest::default();
        for i in 0..3 {
            let raw_id = format!("raw_{i}");
            write_raw(vault.path(), &raw_id, "content");
            manifest.record_ingest(&format!("uri-{i}"), &format!("sha256:{i}"), &raw_id, "t0");
        }
        manifest::store::save(vault.path(), &manifest).unwrap();

        let batch = next_batch(vault.path(), 10, true, counter(1))
            .await
            .unwrap();
        let ids: HashSet<&str> = batch.jobs.iter().map(|(j, _)| j.job_id.as_str()).collect();
        assert_eq!(ids.len(), 3);
    }
}
