//! YAML frontmatter parsing for compiled `./wiki/` concept pages (per the
//! design doc's OKF v0.2 concept-page template, Q5).

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceRef {
    pub resource: String,
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WikiFrontmatter {
    /// Per OKF v0.2 §12, `okf_version` belongs only in the bundle-root
    /// `wiki/index.md`'s frontmatter, not on individual pages — no longer
    /// written by the compiler, but still accepted (and preserved) here for
    /// pages compiled before this change.
    #[serde(default)]
    pub okf_version: Option<String>,
    pub r#type: String,
    /// No longer written by the compiler — an OKF document's ID is just its
    /// own file path minus `.md` (see `core::vault_resolver::wiki_content_dirs`
    /// consumers' filename-stem fallback), not a frontmatter field. Still
    /// accepted for pages compiled before this change.
    #[serde(default)]
    pub id: Option<String>,
    pub title: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub sources: Vec<SourceRef>,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub timestamp: Option<String>,
}

pub struct ParsedPage {
    pub frontmatter: WikiFrontmatter,
    pub body: String,
}

/// Splits `---\n<yaml>\n---\n\n<body>` and parses the YAML half. Returns an
/// error (not a default/empty frontmatter) when the delimiters or the YAML
/// itself are malformed — a compiled page missing valid frontmatter is
/// exactly the kind of structural break `okf-mcp lint` exists to catch.
pub fn parse_wiki_page(content: &str) -> anyhow::Result<ParsedPage> {
    let after_open = content
        .strip_prefix("---\n")
        .ok_or_else(|| anyhow::anyhow!("missing opening '---' frontmatter delimiter"))?;
    let close_at = after_open
        .find("\n---\n")
        .ok_or_else(|| anyhow::anyhow!("missing closing '---' frontmatter delimiter"))?;

    let yaml = &after_open[..close_at];
    let body = &after_open[close_at + "\n---\n".len()..];

    let frontmatter: WikiFrontmatter = serde_yaml::from_str(yaml)
        .map_err(|err| anyhow::anyhow!("invalid frontmatter YAML: {err}"))?;

    Ok(ParsedPage {
        frontmatter,
        body: body.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // NOT joined with `\`-newline continuations: those strip leading
    // whitespace from each continued line, which would silently flatten
    // the `sources:` list's indentation into invalid (duplicate-`id`) YAML.
    // Concatenating separate single-line literals keeps each line's
    // leading spaces intact.
    const EXAMPLE: &str = concat!(
        "---\n",
        "okf_version: \"0.2\"\n",
        "type: concept\n",
        "id: concept_microservices\n",
        "title: \"Microservices Architecture\"\n",
        "description: \"A short summary.\"\n",
        "sources:\n",
        "  - resource: \"/raw/raw_a1f94d.md\"\n",
        "    id: \"raw_a1f94d\"\n",
        "    title: \"Original doc\"\n",
        "tags: [architecture, backend]\n",
        "timestamp: \"2026-07-30T18:52:00Z\"\n",
        "---\n",
        "\n",
        "# Microservices Architecture\n",
        "\n",
        "Relates to [[api-gateway]].\n",
    );

    #[test]
    fn parses_a_well_formed_concept_page() {
        let parsed = parse_wiki_page(EXAMPLE).unwrap();
        assert_eq!(
            parsed.frontmatter.id.as_deref(),
            Some("concept_microservices")
        );
        assert_eq!(parsed.frontmatter.sources.len(), 1);
        assert_eq!(parsed.frontmatter.sources[0].resource, "/raw/raw_a1f94d.md");
        assert!(parsed.body.contains("[[api-gateway]]"));
    }

    #[test]
    fn parses_a_page_with_neither_okf_version_nor_id_present() {
        // The current, no-longer-optional-in-name-only shape the compiler
        // actually writes since OKF v0.2 compliance: no per-page
        // `okf_version` (that belongs only in the bundle-root index.md) and
        // no `id:` (a document's ID is just its own file path).
        let page = concat!(
            "---\n",
            "type: Person\n",
            "title: \"Ada Lovelace\"\n",
            "---\n",
            "\n",
            "# Ada Lovelace\n",
        );
        let parsed = parse_wiki_page(page).unwrap();
        assert_eq!(parsed.frontmatter.okf_version, None);
        assert_eq!(parsed.frontmatter.id, None);
        assert_eq!(parsed.frontmatter.r#type, "Person");
    }

    #[test]
    fn errors_when_the_opening_delimiter_is_missing() {
        assert!(parse_wiki_page("no frontmatter here").is_err());
    }

    #[test]
    fn errors_when_the_closing_delimiter_is_missing() {
        assert!(parse_wiki_page("---\nokf_version: \"0.2\"\n").is_err());
    }

    #[test]
    fn errors_on_malformed_yaml() {
        assert!(parse_wiki_page("---\n[not: valid: yaml:\n---\nbody").is_err());
    }
}
