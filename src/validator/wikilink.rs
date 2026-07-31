//! `[[concept-slug]]` / `[[vault::concept-slug]]` extraction from wiki
//! markdown bodies. Manual `str::find`-based scanning rather than a `regex`
//! dependency — mirrors `services::api_client`'s own manual `{param}`
//! scanner, which this codebase already uses in place of pulling in
//! `regex` for one narrow pattern.

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WikiLink {
    /// `[[concept-slug]]` — resolves within the same vault.
    Local(String),
    /// `[[vault::concept-slug]]` — an explicit cross-vault reference,
    /// per the design doc's Q6: `lint` flags these, nothing auto-resolves
    /// or auto-generates them.
    CrossVault { vault: String, concept: String },
}

/// Scans `body` for every `[[...]]` occurrence. A link spanning a newline
/// (almost certainly two unrelated `[[`/`]]` pairs, e.g. Markdown link
/// reference syntax) is skipped rather than misparsed as one giant link.
pub fn extract_wikilinks(body: &str) -> Vec<WikiLink> {
    let mut links = Vec::new();
    let mut search_from = 0usize;

    while let Some(start_rel) = body[search_from..].find("[[") {
        let start = search_from + start_rel + 2;
        let Some(end_rel) = body[start..].find("]]") else {
            break;
        };
        let end = start + end_rel;
        let inner = &body[start..end];

        if !inner.is_empty() && !inner.contains('\n') {
            links.push(parse_link(inner));
        }
        search_from = end + 2;
    }

    links
}

fn parse_link(inner: &str) -> WikiLink {
    match inner.split_once("::") {
        Some((vault, concept)) => WikiLink::CrossVault {
            vault: vault.to_string(),
            concept: concept.to_string(),
        },
        None => WikiLink::Local(inner.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_a_single_local_link() {
        let links = extract_wikilinks("Relates to [[api-gateway]] for routing.");
        assert_eq!(links, vec![WikiLink::Local("api-gateway".to_string())]);
    }

    #[test]
    fn extracts_multiple_links_in_one_document() {
        let links = extract_wikilinks("[[a]] and [[b]] and [[c]]");
        assert_eq!(
            links,
            vec![
                WikiLink::Local("a".to_string()),
                WikiLink::Local("b".to_string()),
                WikiLink::Local("c".to_string()),
            ]
        );
    }

    #[test]
    fn extracts_a_cross_vault_link() {
        let links = extract_wikilinks("See [[personal::journal-entry]].");
        assert_eq!(
            links,
            vec![WikiLink::CrossVault {
                vault: "personal".to_string(),
                concept: "journal-entry".to_string(),
            }]
        );
    }

    #[test]
    fn ignores_an_unterminated_link() {
        assert_eq!(extract_wikilinks("dangling [[open bracket"), Vec::new());
    }

    #[test]
    fn ignores_an_empty_link() {
        assert_eq!(extract_wikilinks("[[]]"), Vec::new());
    }

    #[test]
    fn a_document_with_no_links_yields_nothing() {
        assert_eq!(extract_wikilinks("just plain text"), Vec::new());
    }
}
