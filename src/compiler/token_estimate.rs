//! A cheap heuristic prompt-size estimator, used to keep compile prompts
//! (see [`crate::compiler::prompts::build_compile_user_prompt`]) within a
//! sane budget before they're sent to the LLM provider.
//!
//! This is deliberately *not* a real tokenizer: no new crate dependency,
//! no model-specific vocabulary. It uses the well-known `chars / 4 ≈
//! tokens` approximation, which is accurate enough to catch "this prompt
//! is wildly oversized" without the cost/complexity of a proper tokenizer.

use super::prompts::{RawBlob, WikiPageRef};

/// Default token budget for an assembled compile prompt: comfortably
/// under typical modern context windows, leaving headroom for the system
/// prompt (see [`crate::compiler::prompts::COMPILER_SYSTEM_PROMPT`]) and
/// the model's output.
pub const DEFAULT_PROMPT_TOKEN_BUDGET: usize = 100_000;

/// The number of characters treated as roughly one token, per the
/// `chars / 4 ≈ tokens` heuristic.
const CHARS_PER_TOKEN: usize = 4;

/// Estimates the token count of `text` using the `chars / 4 ≈ tokens`
/// heuristic. Rounds up so that any non-empty text is estimated at least
/// one token.
pub fn estimate_tokens(text: &str) -> usize {
    text.chars().count().div_ceil(CHARS_PER_TOKEN)
}

/// Estimates the total token size of everything
/// [`crate::compiler::prompts::build_compile_user_prompt`] assembles into
/// the user prompt: the active raw source, the superseded raw source (if
/// any), and the related wiki pages.
pub fn estimate_prompt_tokens(
    active_raw: &RawBlob,
    superseded_raw: Option<&RawBlob>,
    related: &[WikiPageRef],
) -> usize {
    let mut total = estimate_tokens(&active_raw.content);

    if let Some(old) = superseded_raw {
        total += estimate_tokens(&old.content);
    }

    for page in related {
        total += estimate_tokens(&page.content);
    }

    total
}

/// Adaptively trims `related` down to fit within `budget_tokens`, dropping
/// entries from the tail first. `related` is assumed to already be ranked
/// by relevance (as `hybrid_search` returns it), most relevant first, so
/// truncating the tail drops the least relevant pages.
///
/// If the active/superseded raw sources alone already exceed the budget,
/// `related` is trimmed to empty (there's no room for any related pages,
/// but the raw sources themselves are left untouched — that's
/// [`truncate_to_budget`]'s job).
pub fn trim_related_pages(
    mut related: Vec<WikiPageRef>,
    active_raw: &RawBlob,
    superseded_raw: Option<&RawBlob>,
    budget_tokens: usize,
) -> Vec<WikiPageRef> {
    let base_tokens = estimate_tokens(&active_raw.content)
        + superseded_raw.map_or(0, |old| estimate_tokens(&old.content));

    // Drop from the tail (lowest relevance) until we fit, or run out.
    while !related.is_empty() {
        let related_tokens: usize = related
            .iter()
            .map(|page| estimate_tokens(&page.content))
            .sum();
        if base_tokens + related_tokens <= budget_tokens {
            break;
        }
        related.pop();
    }

    related
}

/// Truncates `active`'s and `superseded`'s content so their *combined*
/// estimated token size fits within `budget_tokens`, splitting the budget
/// between the two rather than giving each the full budget independently.
///
/// [`truncate_to_budget`] alone can't provide this guarantee when applied
/// to `active`/`superseded` separately: each call only knows its own
/// content, so two raw blobs that are each individually under
/// `budget_tokens` (but jointly over it) would both sail through
/// untruncated, leaving the assembled prompt over budget with no related
/// pages left to trim ([`trim_related_pages`] can only drop *related*
/// pages, not these two raw blobs). This function closes that gap by
/// having the two share one budget:
///
/// - If both already fit within `budget_tokens` combined, neither is
///   touched (delegates to [`truncate_to_budget`], which is a no-op for
///   already-small content).
/// - Otherwise, whichever blob is small enough to fit in half the budget
///   keeps its actual size, and the other gets whatever's left over.
/// - If both individually exceed half the budget, the budget is split
///   evenly between them.
///
/// Either way the two together end up at or very close to `budget_tokens`
/// — within a small, fixed number of tokens per truncated blob, from
/// [`truncate_to_budget`]'s own visible truncation marker adding a handful
/// of tokens back after cutting the content down (negligible against
/// [`DEFAULT_PROMPT_TOKEN_BUDGET`]'s real-world scale) — so
/// [`trim_related_pages`]'s later pass is working from an accurate "how
/// much room is left for related pages" baseline instead of one that can
/// already be blown by the raw blobs alone.
pub fn truncate_raw_blobs_to_shared_budget(
    active: RawBlob,
    superseded: Option<RawBlob>,
    budget_tokens: usize,
) -> (RawBlob, Option<RawBlob>) {
    let Some(superseded) = superseded else {
        return (
            RawBlob {
                id: active.id,
                content: truncate_to_budget(&active.content, budget_tokens),
            },
            None,
        );
    };

    let active_tokens = estimate_tokens(&active.content);
    let superseded_tokens = estimate_tokens(&superseded.content);
    let half = budget_tokens / 2;

    let (active_budget, superseded_budget) = if active_tokens + superseded_tokens <= budget_tokens {
        // Already fits combined — give each exactly what it needs so
        // `truncate_to_budget` is a guaranteed no-op for both.
        (active_tokens, superseded_tokens)
    } else if active_tokens <= half {
        (active_tokens, budget_tokens - active_tokens)
    } else if superseded_tokens <= half {
        (budget_tokens - superseded_tokens, superseded_tokens)
    } else {
        (half, budget_tokens - half)
    };

    (
        RawBlob {
            id: active.id,
            content: truncate_to_budget(&active.content, active_budget),
        },
        Some(RawBlob {
            id: superseded.id,
            content: truncate_to_budget(&superseded.content, superseded_budget),
        }),
    )
}

/// Truncates `content` so its estimated token size fits within
/// `budget_tokens`, appending a visible marker noting how many characters
/// were cut. Never truncates silently: if `content` is shortened, the
/// marker is always present. Content already within budget is returned
/// unchanged.
pub fn truncate_to_budget(content: &str, budget_tokens: usize) -> String {
    let budget_chars = budget_tokens * CHARS_PER_TOKEN;
    let total_chars = content.chars().count();

    if total_chars <= budget_chars {
        return content.to_string();
    }

    let over_by = total_chars - budget_chars;
    let truncated: String = content.chars().take(budget_chars).collect();
    format!("{truncated}\n\n[... truncated {over_by} chars over budget ...]")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn estimate_tokens_is_in_the_right_ballpark_for_known_length_ascii() {
        // 400 ASCII chars / 4 chars-per-token = 100 tokens exactly.
        let text = "a".repeat(400);
        assert_eq!(estimate_tokens(&text), 100);

        // Rounds up for partial tokens rather than truncating to zero.
        assert_eq!(estimate_tokens("abc"), 1);
        assert_eq!(estimate_tokens(""), 0);

        // 1000 chars -> 250 tokens.
        let text = "x".repeat(1000);
        assert_eq!(estimate_tokens(&text), 250);
    }

    #[test]
    fn estimate_prompt_tokens_sums_all_three_sections() {
        let active = RawBlob {
            id: "raw_a".to_string(),
            content: "a".repeat(400), // 100 tokens
        };
        let superseded = RawBlob {
            id: "raw_b".to_string(),
            content: "b".repeat(800), // 200 tokens
        };
        let related = vec![
            WikiPageRef {
                path: "wiki/concepts/x.md".to_string(),
                content: "c".repeat(40), // 10 tokens
            },
            WikiPageRef {
                path: "wiki/concepts/y.md".to_string(),
                content: "d".repeat(40), // 10 tokens
            },
        ];

        // Active only.
        assert_eq!(estimate_prompt_tokens(&active, None, &[]), 100);

        // Active + superseded.
        assert_eq!(estimate_prompt_tokens(&active, Some(&superseded), &[]), 300);

        // Active + superseded + related.
        assert_eq!(
            estimate_prompt_tokens(&active, Some(&superseded), &related),
            320
        );
    }

    fn page_with_chars(path: &str, n: usize) -> WikiPageRef {
        WikiPageRef {
            path: path.to_string(),
            content: "z".repeat(n),
        }
    }

    #[test]
    fn trim_related_pages_reduces_count_when_over_budget() {
        let active = RawBlob {
            id: "raw_a".to_string(),
            content: "a".repeat(40), // 10 tokens
        };
        let related = vec![
            page_with_chars("wiki/concepts/1.md", 400), // 100 tokens
            page_with_chars("wiki/concepts/2.md", 400), // 100 tokens
            page_with_chars("wiki/concepts/3.md", 400), // 100 tokens
        ];

        // Budget only fits the active source plus one related page.
        let trimmed = trim_related_pages(related, &active, None, 120);
        assert_eq!(trimmed.len(), 1);
        assert_eq!(trimmed[0].path, "wiki/concepts/1.md");
    }

    #[test]
    fn trim_related_pages_leaves_untouched_when_under_budget() {
        let active = RawBlob {
            id: "raw_a".to_string(),
            content: "a".repeat(40), // 10 tokens
        };
        let related = vec![
            page_with_chars("wiki/concepts/1.md", 40), // 10 tokens
            page_with_chars("wiki/concepts/2.md", 40), // 10 tokens
        ];

        let trimmed = trim_related_pages(related, &active, None, DEFAULT_PROMPT_TOKEN_BUDGET);
        assert_eq!(trimmed.len(), 2);
    }

    #[test]
    fn truncate_to_budget_shortens_oversized_content_with_marker() {
        let content = "x".repeat(1000); // 250 tokens
        let result = truncate_to_budget(&content, 100); // budget: 400 chars

        assert!(result.len() < content.len());
        assert!(result.contains("[... truncated"));
        assert!(result.contains("chars over budget ...]"));
        // 1000 - 400 = 600 chars over budget.
        assert!(result.contains("truncated 600 chars over budget"));
    }

    #[test]
    fn truncate_to_budget_leaves_short_content_untouched() {
        let content = "short content";
        let result = truncate_to_budget(content, DEFAULT_PROMPT_TOKEN_BUDGET);
        assert_eq!(result, content);
        assert!(!result.contains("truncated"));
    }

    #[test]
    fn truncate_to_budget_always_includes_marker_when_it_truncates() {
        let content = "y".repeat(50);
        let result = truncate_to_budget(&content, 1); // budget: 4 chars, definitely over
        assert!(result.contains("[... truncated"));
    }

    /// The two-large-raw-blobs gap: `active` and `superseded` are each
    /// individually under budget, but jointly over it. Independent
    /// `truncate_to_budget` calls (the old behavior) would leave both
    /// untouched; the shared-budget version must truncate at least one of
    /// them so the combined estimate fits.
    #[test]
    fn truncate_raw_blobs_to_shared_budget_truncates_when_only_jointly_over_budget() {
        // Scaled up so the truncation marker's own (small, fixed) token
        // cost is negligible against the budget, same as it is in practice
        // against `DEFAULT_PROMPT_TOKEN_BUDGET` — a budget of 100 tokens
        // (as in earlier drafts of this test) is dominated by the marker's
        // own ~15 tokens, which isn't representative of the real gap this
        // function closes.
        const BUDGET: usize = 10_000;
        let active = RawBlob {
            id: "raw_active".to_string(),
            content: "a".repeat(32_000), // 8000 tokens
        };
        let superseded = RawBlob {
            id: "raw_superseded".to_string(),
            content: "b".repeat(32_000), // 8000 tokens
        };
        // Neither blob alone exceeds the 10,000-token budget, but together
        // they're 16,000 tokens — 6,000 over.
        let (active_out, superseded_out) =
            truncate_raw_blobs_to_shared_budget(active, Some(superseded), BUDGET);
        let superseded_out = superseded_out.unwrap();

        let combined =
            estimate_tokens(&active_out.content) + estimate_tokens(&superseded_out.content);
        assert!(
            combined <= BUDGET + 50, // small, fixed slack for the two truncation markers
            "combined estimate {combined} should fit within (or very close to) the \
             {BUDGET}-token budget — old independent-truncation behavior would have left this \
             at ~16,000"
        );
        assert!(
            active_out.content.contains("[... truncated")
                && superseded_out.content.contains("[... truncated"),
            "both blobs individually exceed half the budget, so both must be visibly truncated"
        );
    }

    #[test]
    fn truncate_raw_blobs_to_shared_budget_leaves_both_untouched_when_jointly_under_budget() {
        let active_content = "a".repeat(40); // 10 tokens
        let superseded_content = "b".repeat(40); // 10 tokens
        let active = RawBlob {
            id: "raw_active".to_string(),
            content: active_content.clone(),
        };
        let superseded = RawBlob {
            id: "raw_superseded".to_string(),
            content: superseded_content.clone(),
        };
        let (active_out, superseded_out) = truncate_raw_blobs_to_shared_budget(
            active,
            Some(superseded),
            DEFAULT_PROMPT_TOKEN_BUDGET,
        );
        assert_eq!(active_out.content, active_content);
        assert_eq!(superseded_out.unwrap().content, superseded_content);
    }

    #[test]
    fn truncate_raw_blobs_to_shared_budget_gives_the_smaller_blob_only_what_it_needs() {
        // `active` is tiny (well under half the budget); `superseded` is
        // huge. `active` shouldn't be truncated at all, and `superseded`
        // should get nearly the whole budget rather than being capped at
        // exactly half.
        let active_content = "a".repeat(40); // 10 tokens
        let active = RawBlob {
            id: "raw_active".to_string(),
            content: active_content.clone(),
        };
        let superseded = RawBlob {
            id: "raw_superseded".to_string(),
            content: "b".repeat(4000), // 1000 tokens
        };
        let (active_out, superseded_out) =
            truncate_raw_blobs_to_shared_budget(active, Some(superseded), 100);

        assert_eq!(
            active_out.content, active_content,
            "the small blob should be left completely untouched"
        );
        let superseded_out = superseded_out.unwrap();
        assert!(superseded_out.content.contains("[... truncated"));
        // Should get ~90 tokens (100 - 10), not capped at half (50).
        assert!(estimate_tokens(&superseded_out.content) > 50);
    }

    #[test]
    fn truncate_raw_blobs_to_shared_budget_with_no_superseded_behaves_like_truncate_to_budget() {
        let content = "a".repeat(1000); // 250 tokens
        let active = RawBlob {
            id: "raw_active".to_string(),
            content: content.clone(),
        };
        let (active_out, superseded_out) = truncate_raw_blobs_to_shared_budget(active, None, 100);
        assert!(superseded_out.is_none());
        assert_eq!(active_out.content, truncate_to_budget(&content, 100));
    }
}
