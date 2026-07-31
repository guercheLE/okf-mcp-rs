//! Reciprocal Rank Fusion: merges the BM25 (`search::index`) and dense
//! vector (`search::vectors`) ranked lists into one combined ranking,
//! per the design doc's `hybrid_search` sketch.

use std::collections::HashMap;
use std::hash::Hash;

/// `score(item) = sum over each list it appears in of 1 / (k + rank)`,
/// 1-based rank. `k = 60` is the design doc's own default (a standard RRF
/// constant — large enough that no single list's #1 result dominates the
/// merge outright). Ties broken by `T`'s natural ordering, so results are
/// deterministic regardless of `HashMap` iteration order.
pub fn rrf_merge<T: Eq + Hash + Clone + Ord>(bm25: &[T], vector: &[T], k: f64) -> Vec<(T, f64)> {
    let mut scores: HashMap<T, f64> = HashMap::new();

    for (rank, item) in bm25.iter().enumerate() {
        *scores.entry(item.clone()).or_insert(0.0) += 1.0 / (k + (rank + 1) as f64);
    }
    for (rank, item) in vector.iter().enumerate() {
        *scores.entry(item.clone()).or_insert(0.0) += 1.0 / (k + (rank + 1) as f64);
    }

    let mut merged: Vec<(T, f64)> = scores.into_iter().collect();
    merged.sort_by(|(item_a, score_a), (item_b, score_b)| {
        score_b
            .partial_cmp(score_a)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| item_a.cmp(item_b))
    });
    merged
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_item_ranked_first_in_both_lists_wins() {
        let bm25 = vec!["a".to_string(), "b".to_string()];
        let vector = vec!["a".to_string(), "c".to_string()];
        let merged = rrf_merge(&bm25, &vector, 60.0);
        assert_eq!(merged[0].0, "a");
    }

    #[test]
    fn every_item_from_both_lists_appears_exactly_once() {
        let bm25 = vec!["a".to_string(), "b".to_string()];
        let vector = vec!["c".to_string(), "b".to_string()];
        let merged = rrf_merge(&bm25, &vector, 60.0);
        let items: Vec<_> = merged.iter().map(|(item, _)| item.clone()).collect();
        assert_eq!(items.len(), 3);
        assert!(items.contains(&"a".to_string()));
        assert!(items.contains(&"b".to_string()));
        assert!(items.contains(&"c".to_string()));
    }

    #[test]
    fn an_item_present_in_only_one_list_still_scores_above_zero() {
        let bm25 = vec!["only-here".to_string()];
        let vector: Vec<String> = vec![];
        let merged = rrf_merge(&bm25, &vector, 60.0);
        assert_eq!(merged[0].0, "only-here");
        assert!(merged[0].1 > 0.0);
    }

    #[test]
    fn matches_the_textbook_reciprocal_rank_fusion_formula() {
        let bm25 = vec!["a".to_string()];
        let vector = vec!["a".to_string()];
        let merged = rrf_merge(&bm25, &vector, 60.0);
        let expected = 1.0 / 61.0 + 1.0 / 61.0;
        assert!((merged[0].1 - expected).abs() < 1e-9);
    }

    #[test]
    fn ties_break_deterministically_by_natural_ordering() {
        // "z" ranked #1 in bm25 and "a" ranked #1 in vector both score
        // 1/(60+1) — a genuine tie, resolved by "a" < "z".
        let bm25 = vec!["z".to_string()];
        let vector = vec!["a".to_string()];
        let merged = rrf_merge(&bm25, &vector, 60.0);
        assert_eq!(merged[0].0, "a");
        assert_eq!(merged[1].0, "z");
    }
}
