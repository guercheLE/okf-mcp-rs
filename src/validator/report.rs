//! Renders a `LintReport` for `okf-mcp lint`'s CLI/MCP output.

use super::rules::LintReport;

pub fn to_json(report: &LintReport) -> anyhow::Result<String> {
    Ok(serde_json::to_string_pretty(report)?)
}

/// Plain-text summary — deliberately not framed as "AI-powered" anything,
/// just a structural report: what's broken, what's missing, what's unused.
pub fn to_text(report: &LintReport) -> String {
    if !report.has_errors()
        && report.pending_compiles.is_empty()
        && report.orphan_pages.is_empty()
        && report.cross_vault_links.is_empty()
    {
        return "OK: no lint issues found.".to_string();
    }

    let mut lines = Vec::new();

    if !report.pending_compiles.is_empty() {
        lines.push(format!(
            "Pending compile ({} source(s) not yet fully compiled — run 'okf-mcp compile'):",
            report.pending_compiles.len()
        ));
        for uri in &report.pending_compiles {
            lines.push(format!("  {uri}"));
        }
    }

    if !report.broken_links.is_empty() {
        lines.push(format!("Broken links ({}):", report.broken_links.len()));
        for (source, target) in &report.broken_links {
            lines.push(format!(
                "  {source} -> [[{target}]] (target does not exist)"
            ));
        }
    }

    if !report.missing_sources.is_empty() {
        lines.push(format!(
            "Missing provenance sources ({}):",
            report.missing_sources.len()
        ));
        for (page, source) in &report.missing_sources {
            lines.push(format!("  {page}: sources entry '{source}' does not exist"));
        }
    }

    if !report.orphan_pages.is_empty() {
        lines.push(format!(
            "Orphan pages ({}, warning):",
            report.orphan_pages.len()
        ));
        for page in &report.orphan_pages {
            lines.push(format!("  {page} is not linked from any other page"));
        }
    }

    if !report.cross_vault_links.is_empty() {
        lines.push(format!(
            "Cross-vault links ({}, informational):",
            report.cross_vault_links.len()
        ));
        for (source, target) in &report.cross_vault_links {
            lines.push(format!("  {source} -> [[{target}]]"));
        }
    }

    lines.push(if report.has_errors() {
        "FAILED: fix broken links and missing sources above.".to_string()
    } else {
        "OK: no blocking issues (see warnings above).".to_string()
    });

    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_clean_report_renders_a_single_ok_line() {
        assert_eq!(to_text(&LintReport::default()), "OK: no lint issues found.");
    }

    #[test]
    fn a_report_with_errors_renders_failed_and_lists_them() {
        let report = LintReport {
            broken_links: vec![("wiki/concepts/a.md".to_string(), "missing".to_string())],
            ..Default::default()
        };
        let text = to_text(&report);
        assert!(text.contains("Broken links (1)"));
        assert!(text.contains("[[missing]]"));
        assert!(text.contains("FAILED"));
    }

    #[test]
    fn a_report_with_only_warnings_still_reports_ok() {
        let report = LintReport {
            orphan_pages: vec!["wiki/concepts/lonely.md".to_string()],
            ..Default::default()
        };
        let text = to_text(&report);
        assert!(text.contains("Orphan pages (1, warning)"));
        assert!(text.contains("OK: no blocking issues"));
    }

    #[test]
    fn pending_compiles_renders_first_and_does_not_force_a_failed_verdict() {
        let report = LintReport {
            pending_compiles: vec!["https://example.com/a".to_string()],
            ..Default::default()
        };
        let text = to_text(&report);
        assert!(text.contains("Pending compile (1 source(s)"));
        assert!(text.contains("https://example.com/a"));
        assert!(text.contains("OK: no blocking issues"));
        assert!(
            text.find("Pending compile").unwrap() < text.find("OK:").unwrap(),
            "pending compile should render before the final verdict line"
        );
    }

    #[test]
    fn pending_compiles_renders_before_broken_links_when_both_are_present() {
        let report = LintReport {
            pending_compiles: vec!["https://example.com/a".to_string()],
            broken_links: vec![("wiki/concepts/a.md".to_string(), "missing".to_string())],
            ..Default::default()
        };
        let text = to_text(&report);
        assert!(text.find("Pending compile").unwrap() < text.find("Broken links").unwrap());
    }

    #[test]
    fn json_rendering_round_trips_through_serde() {
        let report = LintReport {
            broken_links: vec![("a".to_string(), "b".to_string())],
            ..Default::default()
        };
        let json = to_json(&report).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["broken_links"][0][0], "a");
    }
}
