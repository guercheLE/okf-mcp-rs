//! LLM-assisted synthesis of missing concept pages for broken wikilinks —
//! the `--fix` companion to plain compile: instead of processing a raw
//! source, each distinct missing link target becomes its own synthesis
//! job, grounded in the raw sources already cited by the pages that
//! reference it. A missing target with no such grounding is skipped
//! rather than left to the model's own invention (see `validator::fix`
//! for the mechanical, LLM-free counterpart that handles missing-source
//! path extensions).

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

use crate::core::output::{Output, ProgressEvent};
use crate::storage::fs_ops;
use crate::validator::frontmatter::parse_wiki_page;
use crate::validator::lint_bundle;

use super::driver::{CompileOptions, read_raw_body};
use super::operations::{apply_operations, parse_compile_payload};
use super::prompts::{COMPILER_SYSTEM_PROMPT, RawBlob, WikiPageRef, build_link_fix_user_prompt};
use super::provider::LLMCompilerDriver;

#[derive(Debug)]
pub enum LinkFixStatus {
    Synthesized,
    /// None of the referencing pages cite any raw source — nothing to
    /// ground synthesis in, so the slug is left for a human/content
    /// decision rather than guessed at.
    SkippedNoSourceContext,
    Failed(String),
}

#[derive(Debug)]
pub struct LinkFixOutcome {
    pub slug: String,
    pub status: LinkFixStatus,
}

#[derive(Debug, Default)]
pub struct LinkFixReport {
    pub outcomes: Vec<LinkFixOutcome>,
    pub touched_paths: Vec<PathBuf>,
}

impl LinkFixReport {
    pub fn synthesized_slugs(&self) -> Vec<&str> {
        self.outcomes
            .iter()
            .filter(|o| matches!(o.status, LinkFixStatus::Synthesized))
            .map(|o| o.slug.as_str())
            .collect()
    }
}

fn raw_ids_cited_by(pages: &[WikiPageRef]) -> HashSet<String> {
    let mut ids = HashSet::new();
    for page in pages {
        if let Ok(parsed) = parse_wiki_page(&page.content) {
            for source in &parsed.frontmatter.sources {
                if let Some(stem) = Path::new(&source.resource)
                    .file_stem()
                    .and_then(|stem| stem.to_str())
                {
                    ids.insert(stem.to_string());
                }
            }
        }
    }
    ids
}

/// Finds every distinct broken-link target in the current lint report and
/// attempts to synthesize a concept page for each, grounded only in the
/// raw sources already cited by the pages that link to it.
pub async fn fix_broken_links(
    vault_root: &Path,
    model_spec: &str,
    options: &CompileOptions,
    on_progress: Option<&Output>,
) -> anyhow::Result<LinkFixReport> {
    let lint = lint_bundle(vault_root)?;

    let mut by_slug: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (page, slug) in &lint.broken_links {
        by_slug.entry(slug.clone()).or_default().push(page.clone());
    }

    let driver = LLMCompilerDriver::new();
    let total = by_slug.len();
    let mut report = LinkFixReport::default();

    for (index, (slug, referencing_paths)) in by_slug.into_iter().enumerate() {
        let position = index + 1;
        if let Some(output) = on_progress {
            output.line(
                &ProgressEvent::Started {
                    index: position,
                    total,
                    label: format!("synthesizing [[{slug}]]"),
                }
                .to_string(),
            );
        }

        let mut referencing_pages = Vec::new();
        for path in &referencing_paths {
            if let Ok(content) = fs_ops::read_to_string(vault_root, path) {
                referencing_pages.push(WikiPageRef {
                    path: path.clone(),
                    content,
                });
            }
        }

        let raw_ids = raw_ids_cited_by(&referencing_pages);
        if raw_ids.is_empty() {
            report.outcomes.push(LinkFixOutcome {
                slug: slug.clone(),
                status: LinkFixStatus::SkippedNoSourceContext,
            });
            if let Some(output) = on_progress {
                output.line(
                    &ProgressEvent::Finished {
                        index: position,
                        total,
                        label: format!("synthesizing [[{slug}]]"),
                        error: Some("no raw-source context available — skipped".to_string()),
                    }
                    .to_string(),
                );
            }
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

        let user_prompt = build_link_fix_user_prompt(&slug, &referencing_pages, &cited_raw_sources);

        let result = driver
            .execute_compile_prompt(
                model_spec,
                COMPILER_SYSTEM_PROMPT,
                &user_prompt,
                options.temperature,
                options.max_tokens,
                options.base_url_override.as_deref(),
                options.api_key_env_override.as_deref(),
            )
            .await
            .and_then(|response| parse_compile_payload(&response))
            .and_then(|payload| apply_operations(vault_root, &payload));

        match result {
            Ok(mut paths) => {
                report.touched_paths.append(&mut paths);
                report.outcomes.push(LinkFixOutcome {
                    slug: slug.clone(),
                    status: LinkFixStatus::Synthesized,
                });
                if let Some(output) = on_progress {
                    output.line(
                        &ProgressEvent::Finished {
                            index: position,
                            total,
                            label: format!("synthesizing [[{slug}]]"),
                            error: None,
                        }
                        .to_string(),
                    );
                }
            }
            Err(err) => {
                report.outcomes.push(LinkFixOutcome {
                    slug: slug.clone(),
                    status: LinkFixStatus::Failed(err.to_string()),
                });
                if let Some(output) = on_progress {
                    output.line(
                        &ProgressEvent::Finished {
                            index: position,
                            total,
                            label: format!("synthesizing [[{slug}]]"),
                            error: Some(err.to_string()),
                        }
                        .to_string(),
                    );
                }
            }
        }
    }

    Ok(report)
}

pub fn summary_line(report: &LinkFixReport) -> String {
    if report.outcomes.is_empty() {
        return "No broken links to synthesize.".to_string();
    }
    let mut lines = Vec::new();
    let synthesized: Vec<&str> = report.synthesized_slugs();
    if !synthesized.is_empty() {
        lines.push(format!(
            "Synthesized {} concept page(s) via LLM: {}",
            synthesized.len(),
            synthesized
                .iter()
                .map(|slug| format!("[[{slug}]]"))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    let skipped: Vec<&str> = report
        .outcomes
        .iter()
        .filter(|o| matches!(o.status, LinkFixStatus::SkippedNoSourceContext))
        .map(|o| o.slug.as_str())
        .collect();
    if !skipped.is_empty() {
        lines.push(format!(
            "Skipped {} (no raw-source context available): {}",
            skipped.len(),
            skipped
                .iter()
                .map(|slug| format!("[[{slug}]]"))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    for outcome in &report.outcomes {
        if let LinkFixStatus::Failed(err) = &outcome.status {
            lines.push(format!("Failed to synthesize [[{}]]: {err}", outcome.slug));
        }
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

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
            "---\nokf_version: \"0.2\"\ntype: concept\nid: concept_{slug}\ntitle: \"{slug}\"\n{sources_yaml}---\n\n# {slug}\n\n{body_extra}\n"
        );
        std::fs::write(dir.join(format!("{slug}.md")), content).unwrap();
    }

    #[test]
    fn raw_ids_cited_by_collects_and_dedups_across_pages() {
        let a = WikiPageRef {
            path: "wiki/concepts/a.md".to_string(),
            content: "---\nokf_version: \"0.2\"\ntype: concept\nid: concept_a\ntitle: \"a\"\nsources:\n  - resource: \"/raw/raw_aaa.md\"\n---\n\n# a\n".to_string(),
        };
        let b = WikiPageRef {
            path: "wiki/concepts/b.md".to_string(),
            content: "---\nokf_version: \"0.2\"\ntype: concept\nid: concept_b\ntitle: \"b\"\nsources:\n  - resource: \"/raw/raw_aaa.md\"\n  - resource: \"/raw/raw_bbb.md\"\n---\n\n# b\n".to_string(),
        };
        let ids = raw_ids_cited_by(&[a, b]);
        assert_eq!(ids.len(), 2);
        assert!(ids.contains("raw_aaa"));
        assert!(ids.contains("raw_bbb"));
    }

    #[tokio::test]
    async fn a_slug_with_no_broken_links_produces_an_empty_report() {
        let vault = tempfile::tempdir().unwrap();
        write_concept(vault.path(), "a", &[], "");

        let report = fix_broken_links(
            vault.path(),
            "anthropic/claude-3-5-sonnet",
            &CompileOptions::default(),
            None,
        )
        .await
        .unwrap();
        assert!(report.outcomes.is_empty());
        assert!(report.touched_paths.is_empty());
    }

    #[tokio::test]
    async fn a_broken_link_with_no_cited_raw_sources_is_skipped_without_calling_the_llm() {
        let vault = tempfile::tempdir().unwrap();
        write_concept(vault.path(), "a", &[], "See [[missing]].");

        let report = fix_broken_links(
            vault.path(),
            "anthropic/claude-3-5-sonnet",
            &CompileOptions::default(),
            None,
        )
        .await
        .unwrap();
        assert_eq!(report.outcomes.len(), 1);
        assert!(matches!(
            report.outcomes[0].status,
            LinkFixStatus::SkippedNoSourceContext
        ));
        assert!(report.touched_paths.is_empty());
    }

    /// Minimal HTTP/1.1 server returning a fixed response to the first
    /// request it receives — same pattern as
    /// `provider::tests::mock_server`, but taking an owned `String` body so
    /// the caller can build a dynamic (not `&'static`) JSON response.
    async fn mock_chat_server(body: String) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0u8; 8192];
            let _ = stream.read(&mut request).await.unwrap();
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(response.as_bytes()).await.unwrap();
        });
        format!("http://{address}")
    }

    /// An OpenAI-shaped chat-completion response whose `message.content` is
    /// itself a `CompilePayload` JSON string synthesizing `slug`'s concept
    /// page — the exact shape a real OpenAI-compatible provider (or genai's
    /// `AdapterKind::OpenAI`, which the `"custom"` provider reuses) returns.
    fn openai_chat_response_synthesizing(slug: &str) -> String {
        let concept_page = format!(
            "---\nokf_version: \"0.2\"\ntype: concept\nid: concept_{slug}\ntitle: \"{slug}\"\ndescription: \"synthesized\"\nsources:\n  - resource: \"/raw/raw_aaa.md\"\ntags: []\ntimestamp: \"2024-01-01T00:00:00Z\"\n---\n\n# {slug}\n\nSynthesized from raw_aaa.\n"
        );
        let compile_payload = serde_json::json!({
            "operations": [
                {
                    "action": "CREATE_OR_UPDATE",
                    "path": format!("wiki/concepts/{slug}.md"),
                    "content": concept_page,
                }
            ]
        })
        .to_string();

        serde_json::json!({
            "id": "chatcmpl-test",
            "object": "chat.completion",
            "model": "some-model",
            "choices": [
                {
                    "index": 0,
                    "message": { "role": "assistant", "content": compile_payload },
                    "finish_reason": "stop",
                }
            ],
            "usage": { "prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2 },
        })
        .to_string()
    }

    #[tokio::test]
    async fn fix_broken_links_synthesizes_a_missing_concept_via_a_mocked_openai_compatible_provider()
     {
        // SAFETY: test-only env mutation; this var name is unique to this
        // test so it can't race with other tests touching real provider
        // vars — see `resolve_provider_target`'s auth resolution, which
        // requires this to be set (to anything) for the OpenAI adapter to
        // build an `Authorization` header at all.
        unsafe {
            std::env::set_var("OKF_TEST_LINK_FIX_API_KEY", "test-key");
        }

        let vault = tempfile::tempdir().unwrap();
        write_concept(vault.path(), "a", &["/raw/raw_aaa.md"], "See [[missing]].");
        std::fs::create_dir_all(vault.path().join("raw")).unwrap();
        std::fs::write(
            vault.path().join("raw/raw_aaa.md"),
            "# Raw Source\n\nSome raw content about the missing concept.\n",
        )
        .unwrap();

        let base_url = mock_chat_server(openai_chat_response_synthesizing("missing")).await;
        let options = CompileOptions {
            base_url_override: Some(base_url),
            api_key_env_override: Some("OKF_TEST_LINK_FIX_API_KEY".to_string()),
            ..CompileOptions::default()
        };

        let report = fix_broken_links(vault.path(), "custom/some-model", &options, None)
            .await
            .unwrap();

        assert_eq!(report.outcomes.len(), 1);
        assert!(matches!(
            report.outcomes[0].status,
            LinkFixStatus::Synthesized
        ));
        assert_eq!(report.synthesized_slugs(), vec!["missing"]);
        assert_eq!(report.touched_paths.len(), 1);
        assert!(
            std::fs::read_to_string(vault.path().join("wiki/concepts/missing.md"))
                .unwrap()
                .contains("Synthesized from raw_aaa")
        );

        unsafe {
            std::env::remove_var("OKF_TEST_LINK_FIX_API_KEY");
        }
    }

    #[test]
    fn summary_line_reports_no_broken_links_when_the_report_is_empty() {
        assert_eq!(
            summary_line(&LinkFixReport::default()),
            "No broken links to synthesize."
        );
    }

    #[test]
    fn summary_line_lists_synthesized_and_skipped_slugs() {
        let report = LinkFixReport {
            outcomes: vec![
                LinkFixOutcome {
                    slug: "a".to_string(),
                    status: LinkFixStatus::Synthesized,
                },
                LinkFixOutcome {
                    slug: "b".to_string(),
                    status: LinkFixStatus::SkippedNoSourceContext,
                },
            ],
            touched_paths: vec![],
        };
        let text = summary_line(&report);
        assert!(text.contains("Synthesized 1 concept page(s)"));
        assert!(text.contains("[[a]]"));
        assert!(text.contains("Skipped 1"));
        assert!(text.contains("[[b]]"));
    }
}
