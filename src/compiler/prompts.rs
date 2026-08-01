//! The compiler system prompt and user-prompt builder, per the design
//! doc's Q5 (`docs/okf-pipeline-design.md`).

/// A raw source's content, as read from `./raw/<raw_id>.md` (frontmatter
/// stripped — just the body).
pub struct RawBlob {
    pub id: String,
    pub content: String,
}

/// An existing compiled wiki page, for the "related context" section of the
/// prompt.
pub struct WikiPageRef {
    pub path: String,
    pub content: String,
}

pub const COMPILER_SYSTEM_PROMPT: &str = r#"You are the OKF LLM Compiler Engine, a knowledge-graph synthesis process that maintains a local Open Knowledge Format (OKF v0.2) wiki repository.

Core Mission: read raw source documents from ./raw/ and synthesize them into clean, atomic, cross-linked Markdown concept documents inside ./wiki/concepts/.

Compilation Rules:
1. Source of Truth: read content exclusively from the active raw source(s) provided below.
2. Atomicity: one concept per file. If a document introduces multiple major concepts, compile multiple distinct markdown files.
3. Wikilinking Topology: use [[concept-slug]] notation for entities, technologies, or terms. Every link's slug must match a target file's slug (its filename without the .md extension) inside ./wiki/concepts/ — either an existing one, or a slug for a new concept you are creating in this same response.
4. Strict Provenance: every output file's YAML frontmatter MUST declare its active raw sources under a `sources:` array, each entry shaped `{resource: "/raw/<raw_id>.md"}`.
5. Conflict Resolution: if a new source contradicts an existing wiki concept, update that page with the newest state and record the conflict under a `## Contradictions & Evolutions` section, dated.

Output Format Requirement: respond with exactly one valid JSON object, no prose before or after it, no markdown code fences:
{"operations": [
  {"action": "CREATE_OR_UPDATE", "path": "wiki/concepts/<slug>.md", "content": "<full markdown file content, including frontmatter>"},
  {"action": "DELETE", "path": "wiki/concepts/<slug>.md", "reason": "<why this concept no longer has any active source>"}
]}

Concept page template:
---
okf_version: "0.2"
type: concept
id: concept_<slug>
title: "<Human Readable Title>"
description: "<one sentence, used as an index/search preview>"
sources:
  - resource: "/raw/<raw_id>.md"
tags: [<tag1>, <tag2>]
timestamp: "<ISO8601>"
---

# <Title>

<Executive summary>

## Key Concepts & Architecture
* Relates to [[other-concept]] for ...

## Technical Details
<details>

## Contradictions & Historical Diffs
* **<ISO8601_DATE>**: Superseded [[previous-implementation]] based on source `/raw/<new_raw_id>.md`.
"#;

/// Assembles the three labeled sections from the design doc's Q5: the
/// active source to process, the superseded version it replaced (if any,
/// for diff synthesis), and related existing wiki pages to update/link
/// against.
pub fn build_compile_user_prompt(
    active_raw: &RawBlob,
    superseded_raw: Option<&RawBlob>,
    existing_related_wiki_pages: &[WikiPageRef],
) -> String {
    let mut prompt = String::new();

    prompt.push_str("## 1. ACTIVE RAW SOURCE TO PROCESS\n");
    prompt.push_str(&format!("Raw ID: {}\n", active_raw.id));
    prompt.push_str(&format!(
        "Content:\n```markdown\n{}\n```\n\n",
        active_raw.content
    ));

    if let Some(old) = superseded_raw {
        prompt.push_str("## 2. SUPERSEDED PREVIOUS SOURCE (Diff Reference)\n");
        prompt.push_str(&format!("Old Raw ID: {}\n", old.id));
        prompt.push_str(&format!(
            "Old Content:\n```markdown\n{}\n```\n\n",
            old.content
        ));
        prompt.push_str(
            "INSTRUCTION: identify what changed or evolved between the superseded source and \
             the active source, then update existing concept pages accordingly.\n\n",
        );
    }

    if !existing_related_wiki_pages.is_empty() {
        prompt.push_str("## 3. CURRENT EXISTING WIKI CONTEXT (To Update / Link Against)\n");
        for page in existing_related_wiki_pages {
            prompt.push_str(&format!(
                "Path: {}\n```markdown\n{}\n```\n\n",
                page.path, page.content
            ));
        }
    }

    prompt.push_str(
        "Synthesize the active raw source into OKF v0.2 concept documents. Emit the JSON \
         payload of operations.",
    );
    prompt
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_prompt_always_includes_the_active_source() {
        let prompt = build_compile_user_prompt(
            &RawBlob {
                id: "raw_aaa".to_string(),
                content: "content".to_string(),
            },
            None,
            &[],
        );
        assert!(prompt.contains("ACTIVE RAW SOURCE"));
        assert!(prompt.contains("raw_aaa"));
        assert!(!prompt.contains("SUPERSEDED"));
        assert!(!prompt.contains("EXISTING WIKI CONTEXT"));
    }

    #[test]
    fn the_prompt_includes_the_superseded_section_only_when_given_one() {
        let prompt = build_compile_user_prompt(
            &RawBlob {
                id: "raw_bbb".to_string(),
                content: "new".to_string(),
            },
            Some(&RawBlob {
                id: "raw_aaa".to_string(),
                content: "old".to_string(),
            }),
            &[],
        );
        assert!(prompt.contains("SUPERSEDED PREVIOUS SOURCE"));
        assert!(prompt.contains("raw_aaa"));
        assert!(prompt.contains("raw_bbb"));
    }

    #[test]
    fn the_prompt_lists_every_related_wiki_page() {
        let prompt = build_compile_user_prompt(
            &RawBlob {
                id: "raw_aaa".to_string(),
                content: "content".to_string(),
            },
            None,
            &[
                WikiPageRef {
                    path: "wiki/concepts/a.md".to_string(),
                    content: "A content".to_string(),
                },
                WikiPageRef {
                    path: "wiki/concepts/b.md".to_string(),
                    content: "B content".to_string(),
                },
            ],
        );
        assert!(prompt.contains("EXISTING WIKI CONTEXT"));
        assert!(prompt.contains("wiki/concepts/a.md"));
        assert!(prompt.contains("wiki/concepts/b.md"));
    }

    #[test]
    fn the_system_prompt_declares_the_json_only_output_contract() {
        assert!(COMPILER_SYSTEM_PROMPT.contains("CREATE_OR_UPDATE"));
        assert!(COMPILER_SYSTEM_PROMPT.contains("DELETE"));
        assert!(
            COMPILER_SYSTEM_PROMPT.contains("[[concept-slug]]")
                || COMPILER_SYSTEM_PROMPT.contains("[[other-concept]]")
        );
    }
}
