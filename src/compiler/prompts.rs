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

Core Mission: read raw source documents from ./raw/ and synthesize them into clean, atomic, cross-linked Markdown documents inside ./wiki/concepts/ (abstract ideas, processes, patterns, metrics) or ./wiki/entities/ (concrete subjects: people, organizations, places, tools, technologies) — whichever folder actually fits the subject.

Compilation Rules:
1. Source of Truth: read content exclusively from the active raw source(s) provided below.
2. Atomicity: one subject per file. If a document introduces multiple major concepts or entities, compile multiple distinct markdown files.
3. Entities vs. Concepts: a concrete subject (a specific person, organization, place, tool, or technology) belongs in ./wiki/entities/<slug>.md; an abstract idea, process, pattern, or metric belongs in ./wiki/concepts/<slug>.md.
4. Wikilinking Topology: use [[slug]] notation for cross-references. Every link's slug must match a target file's slug (its filename without the .md extension) in EITHER ./wiki/concepts/ or ./wiki/entities/ — either an existing one, or a slug for a new file you are creating in this same response. A reader following a link should never need to know or care which of the two folders its target lives in.
5. Open Type Vocabulary: choose a `type:` value that specifically describes this document's subject — it is NOT a fixed enum. Examples for entities: Person, Organization, Place, Tool, Technology. Examples for concepts: Process, Pattern, Metric, Event, Reference. These are illustrative, not exhaustive — pick whatever value is most descriptive and self-explanatory for the actual content.
6. Strict Provenance: every output file's YAML frontmatter MUST declare its active raw sources under a `sources:` array, each entry shaped `{resource: "/raw/<raw_id>.md", id: "<short stable key, e.g. s1>", title: "<short label>"}`. Where it aids traceability, cite specific claims in the body with a markdown footnote keyed to that source's `id` (e.g. `[^s1]`) — not required for every sentence, only where it meaningfully helps a reader verify a claim.
7. Conflict Resolution: if a new source contradicts an existing wiki page, update that page with the newest state and record the conflict under a `## Contradictions & Evolutions` section, dated.

Output Format Requirement: respond with exactly one valid JSON object, no prose before or after it, no markdown code fences:
{"operations": [
  {"action": "CREATE_OR_UPDATE", "path": "wiki/concepts/<slug>.md", "content": "<full markdown file content, including frontmatter>"},
  {"action": "CREATE_OR_UPDATE", "path": "wiki/entities/<slug>.md", "content": "<full markdown file content, including frontmatter>"},
  {"action": "DELETE", "path": "wiki/concepts/<slug>.md", "reason": "<why this file no longer has any active source>"}
]}

Page template (same shape for both ./wiki/concepts/ and ./wiki/entities/ — only the folder and `type:` value differ):
---
type: <descriptive type — see rule 5>
title: "<Human Readable Title>"
description: "<one sentence, used as an index/search preview>"
sources:
  - resource: "/raw/<raw_id>.md"
    id: "<short stable key>"
    title: "<short label>"
tags: [<tag1>, <tag2>]
generated: { by: "okf-mcp-compiler", at: "<ISO8601>" }
---

# <Title>

<Executive summary>

## Key Details
* Relates to [[other-slug]] for ...

## Contradictions & Evolutions
* **<ISO8601_DATE>**: Superseded [[previous-slug]] based on source `/raw/<new_raw_id>.md`.

Do NOT include an `okf_version:` or `id:` field — those don't belong on individual pages (okf_version is declared once, at the bundle root; a page's ID is just its own file path). Only add `status: draft` if this synthesis is genuinely uncertain or incomplete (omit it otherwise — stable is the default). Only add `stale_after: <YYYY-MM-DD>` if this content has an obvious, concrete expiry — most content won't, so omit it by default rather than guessing.
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

/// Builds the user prompt for `--fix`'s LLM-assisted broken-link repair:
/// unlike `build_compile_user_prompt` (driven by one new/changed raw
/// source), this job starts from a *missing* concept slug that existing
/// pages already link to, grounded only in the raw sources those
/// referencing pages themselves cite — never invented from nothing.
pub fn build_link_fix_user_prompt(
    missing_slug: &str,
    referencing_pages: &[WikiPageRef],
    cited_raw_sources: &[RawBlob],
) -> String {
    let mut prompt = String::new();

    prompt.push_str("## 1. MISSING CONCEPT TO SYNTHESIZE\n");
    prompt.push_str(&format!(
        "The wiki graph contains one or more `[[{missing_slug}]]` wikilinks, but \
         neither `wiki/concepts/{missing_slug}.md` nor `wiki/entities/{missing_slug}.md` \
         exists. Create exactly that one file, in whichever of the two folders fits the \
         subject (see the entities-vs-concepts rule above) \
         (and, only if genuinely warranted by the source material, closely related \
         files it should link to) — do not invent unrelated content.\n\n"
    ));

    prompt.push_str("## 2. PAGES THAT LINK TO IT\n");
    for page in referencing_pages {
        prompt.push_str(&format!(
            "Path: {}\n```markdown\n{}\n```\n\n",
            page.path, page.content
        ));
    }

    prompt.push_str("## 3. RAW SOURCES CITED BY THOSE PAGES\n");
    for raw in cited_raw_sources {
        prompt.push_str(&format!(
            "Raw ID: {}\nContent:\n```markdown\n{}\n```\n\n",
            raw.id, raw.content
        ));
    }

    prompt.push_str(&format!(
        "Synthesize {missing_slug}.md (in wiki/concepts/ or wiki/entities/, whichever fits) \
         strictly from the raw sources above — the same provenance and atomicity rules as \
         any other compile. Emit the JSON payload of operations."
    ));
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
            COMPILER_SYSTEM_PROMPT.contains("[[slug]]")
                || COMPILER_SYSTEM_PROMPT.contains("[[other-slug]]")
        );
    }

    #[test]
    fn the_system_prompt_routes_entities_and_concepts_to_separate_folders() {
        assert!(COMPILER_SYSTEM_PROMPT.contains("wiki/entities/"));
        assert!(COMPILER_SYSTEM_PROMPT.contains("wiki/concepts/"));
        assert!(COMPILER_SYSTEM_PROMPT.contains("Entities vs. Concepts"));
    }

    #[test]
    fn the_system_prompt_declares_an_open_type_vocabulary_not_a_fixed_concept_value() {
        assert!(COMPILER_SYSTEM_PROMPT.contains("Open Type Vocabulary"));
        assert!(COMPILER_SYSTEM_PROMPT.contains("NOT a fixed enum"));
        // The template no longer hardcodes `type: concept` or a per-page
        // `okf_version`/`id` — both moved out (okf_version to the
        // bundle-root index.md; id is derived from the file path).
        assert!(!COMPILER_SYSTEM_PROMPT.contains("type: concept"));
        assert!(COMPILER_SYSTEM_PROMPT.contains("Do NOT include an `okf_version:` or `id:` field"));
    }

    #[test]
    fn the_link_fix_prompt_names_the_missing_slug_and_both_candidate_paths() {
        let prompt = build_link_fix_user_prompt("missing-concept", &[], &[]);
        assert!(prompt.contains("[[missing-concept]]"));
        assert!(prompt.contains("wiki/concepts/missing-concept.md"));
        assert!(prompt.contains("wiki/entities/missing-concept.md"));
    }

    #[test]
    fn the_link_fix_prompt_includes_referencing_pages_and_cited_raw_sources() {
        let prompt = build_link_fix_user_prompt(
            "missing-concept",
            &[WikiPageRef {
                path: "wiki/concepts/a.md".to_string(),
                content: "See [[missing-concept]].".to_string(),
            }],
            &[RawBlob {
                id: "raw_aaa".to_string(),
                content: "source content".to_string(),
            }],
        );
        assert!(prompt.contains("PAGES THAT LINK TO IT"));
        assert!(prompt.contains("wiki/concepts/a.md"));
        assert!(prompt.contains("RAW SOURCES CITED BY THOSE PAGES"));
        assert!(prompt.contains("raw_aaa"));
    }
}
