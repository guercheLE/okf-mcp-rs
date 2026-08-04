//! Generic bounded-concurrency async runner.
//!
//! This module is intentionally decoupled from anything compile/LLM specific
//! (no `genai`, no `LLMCompilerDriver`, no vault/provider types) so it can be
//! unit-tested with fast synthetic async closures instead of real network or
//! LLM calls.
//!
//! [`run_bounded`] runs a `Vec<T>` of items through an async `fn(T) -> R`,
//! capping the number of concurrently in-flight tasks at `limit` (enforced
//! via a [`tokio::sync::Semaphore`] permit per task, tracked with a
//! [`tokio::task::JoinSet`]). Results are delivered to the caller as they
//! complete via an `on_result` callback, so the caller can persist state
//! incrementally instead of waiting for every task to finish. The full set
//! of results is also returned, in original input order, for convenience.
//!
//! Concurrency is achieved with a "sliding window": at most `limit` tasks
//! are ever spawned at once. A fresh task is spawned only once an
//! in-flight one completes and frees its semaphore permit. This has an
//! important, load-bearing consequence: when `limit == 1` there is never
//! more than one task in flight, so results are yielded in strict input
//! order — the next task cannot even start, let alone finish, before the
//! previous one does. Callers that rely on today's sequential behavior can
//! pass `limit: 1` and get byte-for-byte identical ordering.
//!
//! `on_result` returns `bool`: `false` tells the sliding window to stop
//! spawning any *not-yet-started* item (a fail-fast escape hatch for
//! callers like `compiler::driver::run_compile_sources`, whose manifest
//! writes must stop being attempted once one of them fails). Items already
//! spawned before that point are never cancelled — they run to completion
//! and still flow through `on_result` — so at `limit == 1` this stops
//! dispatch immediately (byte-for-byte matching a `for` loop's `return
//! Err` on the first failure), while at `limit > 1` up to `limit - 1`
//! already in-flight siblings may still complete afterward.

use std::future::Future;
use std::sync::Arc;

use tokio::sync::Semaphore;
use tokio::task::JoinSet;

/// Runs `items` through `f`, at most `limit` of them concurrently.
///
/// - `items`: the work items, consumed in order.
/// - `limit`: the maximum number of concurrently in-flight tasks. A value
///   of `0` is treated as `1` (fully sequential) rather than deadlocking.
/// - `f`: an async function/closure applied to each item. It is shared
///   across tasks via `Arc`, so it must be `Fn` (not `FnMut`/`FnOnce`),
///   `Send`, and `Sync`.
/// - `on_result`: invoked once per item, as soon as that item's task
///   completes, with `(original_index, &result)`. When `limit == 1` these
///   calls happen in input order (index `0`, then `1`, ...). When
///   `limit > 1` they may happen in any completion order. This callback
///   runs synchronously in the caller's task (never concurrently with
///   itself), so it can freely mutate captured state without extra
///   synchronization. Its return value controls further dispatch: `true`
///   keeps the sliding window going (today's only behavior before this
///   flag existed); `false` stops any *not-yet-spawned* item from ever
///   starting — see this module's doc comment for exactly what "stops"
///   means under `limit > 1`.
///
/// Returns a `Vec<R>` containing the result of every item that was actually
/// dispatched, in original input order (i.e. still ordered like `items`,
/// just possibly shorter than `items` if `on_result` returned `false`
/// before every item got a chance to run).
///
/// # Panics
///
/// Panics if a spawned task itself panics (the panic is propagated via
/// [`JoinSet::join_next`]).
pub async fn run_bounded<T, R, Fut, F, C>(
    items: Vec<T>,
    limit: usize,
    f: F,
    mut on_result: C,
) -> Vec<R>
where
    T: Send + 'static,
    R: Send + 'static,
    Fut: Future<Output = R> + Send + 'static,
    F: Fn(T) -> Fut + Send + Sync + 'static,
    C: FnMut(usize, &R) -> bool,
{
    let limit = limit.max(1);
    let semaphore = Arc::new(Semaphore::new(limit));
    let f = Arc::new(f);

    let mut join_set: JoinSet<(usize, R)> = JoinSet::new();
    let mut slots: Vec<Option<R>> = (0..items.len()).map(|_| None).collect();
    let mut pending = items.into_iter().enumerate();
    let mut keep_dispatching = true;

    // Prime the sliding window with up to `limit` tasks.
    for (idx, item) in pending.by_ref().take(limit) {
        spawn_one(&mut join_set, &semaphore, &f, idx, item).await;
    }

    while let Some(joined) = join_set.join_next().await {
        let (idx, result) = joined.expect("run_bounded: spawned task panicked");
        keep_dispatching &= on_result(idx, &result);
        slots[idx] = Some(result);

        // A permit just freed up (the completed task's future already
        // dropped its permit before returning); keep the window full by
        // spawning the next pending item, if any — unless `on_result` has
        // asked us to stop dispatching anything further.
        if keep_dispatching && let Some((idx, item)) = pending.next() {
            spawn_one(&mut join_set, &semaphore, &f, idx, item).await;
        }
    }

    // Items that were never dispatched (because `on_result` returned
    // `false` before their turn came up) simply have no slot filled —
    // `flatten` drops those `None`s while keeping everything else in
    // original input order.
    slots.into_iter().flatten().collect()
}

/// Acquires a semaphore permit and spawns a single task onto `join_set`.
///
/// Acquiring the permit before spawning is what makes concurrency
/// deterministic at `limit == 1`: the caller (the sliding-window loop in
/// [`run_bounded`]) never spawns item `n + 1` until item `n` has both
/// completed *and* released its permit, so with only one permit available
/// there is never more than one task alive at a time.
async fn spawn_one<T, R, Fut, F>(
    join_set: &mut JoinSet<(usize, R)>,
    semaphore: &Arc<Semaphore>,
    f: &Arc<F>,
    idx: usize,
    item: T,
) where
    T: Send + 'static,
    R: Send + 'static,
    Fut: Future<Output = R> + Send + 'static,
    F: Fn(T) -> Fut + Send + Sync + 'static,
{
    let permit = Arc::clone(semaphore)
        .acquire_owned()
        .await
        .expect("run_bounded: semaphore should never be closed");
    let f = Arc::clone(f);
    join_set.spawn(async move {
        let result = f(item).await;
        drop(permit);
        (idx, result)
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;
    use tokio::sync::Mutex as AsyncMutex;
    use tokio::time::sleep;

    /// (1) limit == 1: completion order must equal input order.
    ///
    /// `on_result` runs synchronously in the driver (never concurrently
    /// with itself), so a plain captured `Vec` is enough to record order —
    /// no locking required.
    #[tokio::test]
    async fn limit_one_preserves_input_order() {
        let items: Vec<u32> = vec![10, 20, 30, 40, 50];
        let mut observed_order = Vec::new();

        let results = run_bounded(
            items,
            1,
            |n| async move { n * 2 },
            |idx, _result: &u32| {
                observed_order.push(idx);
                true
            },
        )
        .await;

        assert_eq!(
            observed_order,
            vec![0, 1, 2, 3, 4],
            "limit == 1 must yield results in strict input order"
        );
        assert_eq!(results, vec![20, 40, 60, 80, 100]);
    }

    /// (2) limit > 1 with a fast/slow mix: completion order is not
    /// required to match input order, and this test proves it actually
    /// doesn't for a case designed to complete out of order — i.e.
    /// concurrency is real, not accidentally serialized.
    #[tokio::test]
    async fn limit_greater_than_one_completes_out_of_order() {
        // Item 0 is slow, items 1..4 are fast. With real concurrency
        // (limit >= 2, and enough room for all 5 to run at once) the fast
        // items finish first even though item 0 was started first.
        let delays_ms: Vec<u64> = vec![120, 5, 5, 5, 5];
        let mut observed_order = Vec::new();

        let _results = run_bounded(
            delays_ms,
            4,
            |delay_ms| async move {
                sleep(Duration::from_millis(delay_ms)).await;
                delay_ms
            },
            |idx, _result: &u64| {
                observed_order.push(idx);
                true
            },
        )
        .await;

        assert_ne!(
            observed_order,
            vec![0, 1, 2, 3, 4],
            "with real concurrency the slow item (index 0) should not finish first"
        );
        assert_eq!(
            observed_order[0], 1,
            "the fastest item (index 1) should complete before the slow item"
        );
        assert_eq!(
            *observed_order.last().unwrap(),
            0,
            "the slow item (index 0) should complete last"
        );
    }

    /// (3) Race-detection: N tasks each increment a shared counter under a
    /// tokio Mutex from inside the spawned async work itself (i.e. under
    /// real cross-task concurrency); the final count must exactly equal N
    /// with no lost updates.
    #[tokio::test]
    async fn shared_counter_has_no_lost_updates_under_concurrency() {
        const N: usize = 200;
        let items: Vec<usize> = (0..N).collect();
        let counter = Arc::new(AsyncMutex::new(0usize));

        let counter_for_task = Arc::clone(&counter);
        let _results = run_bounded(
            items,
            16,
            move |_item| {
                let counter = Arc::clone(&counter_for_task);
                async move {
                    let mut guard = counter.lock().await;
                    *guard += 1;
                }
            },
            |_idx, _result: &()| true,
        )
        .await;

        assert_eq!(*counter.lock().await, N, "no updates should be lost");
    }

    /// (4) The runner must never exceed the configured concurrency limit
    /// at any instant, tracked via a shared "currently in flight" counter
    /// updated from inside the spawned async work.
    #[tokio::test]
    async fn never_exceeds_configured_concurrency_limit() {
        const LIMIT: usize = 5;
        let items: Vec<u32> = (0..40).collect();

        let in_flight = Arc::new(AtomicUsize::new(0));
        let max_observed = Arc::new(AtomicUsize::new(0));

        let in_flight_task = Arc::clone(&in_flight);
        let max_observed_task = Arc::clone(&max_observed);
        let _results = run_bounded(
            items,
            LIMIT,
            move |_item| {
                let in_flight = Arc::clone(&in_flight_task);
                let max_observed = Arc::clone(&max_observed_task);
                async move {
                    let now = in_flight.fetch_add(1, Ordering::SeqCst) + 1;
                    max_observed.fetch_max(now, Ordering::SeqCst);
                    sleep(Duration::from_millis(10)).await;
                    in_flight.fetch_sub(1, Ordering::SeqCst);
                }
            },
            |_idx, _result: &()| true,
        )
        .await;

        let observed_max = max_observed.load(Ordering::SeqCst);
        assert!(
            observed_max <= LIMIT,
            "observed concurrency {observed_max} exceeded configured limit {LIMIT}"
        );
        // Sanity: with 40 items and real concurrency, we should have
        // actually reached the configured limit at some point, otherwise
        // this test would trivially pass for a fully serial (broken)
        // implementation too.
        assert_eq!(observed_max, LIMIT);
        assert_eq!(in_flight.load(Ordering::SeqCst), 0);
    }

    /// (5a) All results are returned (none dropped) for limit == 1.
    #[tokio::test]
    async fn all_results_returned_limit_one() {
        let items: Vec<i32> = (0..25).collect();
        let expected: Vec<i32> = items.iter().map(|n| n * n).collect();

        let results = run_bounded(items, 1, |n| async move { n * n }, |_, _| true).await;

        assert_eq!(results.len(), expected.len());
        assert_eq!(results, expected);
    }

    /// (5b) All results are returned (none dropped) for limit > 1.
    #[tokio::test]
    async fn all_results_returned_limit_greater_than_one() {
        let items: Vec<i32> = (0..25).collect();
        let expected: Vec<i32> = items.iter().map(|n| n * n).collect();

        let results = run_bounded(items, 7, |n| async move { n * n }, |_, _| true).await;

        assert_eq!(results.len(), expected.len());
        assert_eq!(results, expected);
    }

    /// A limit of 0 must not deadlock: it is treated as 1 (sequential).
    #[tokio::test]
    async fn limit_zero_is_treated_as_one_and_does_not_deadlock() {
        let items: Vec<u32> = vec![1, 2, 3];
        let results = run_bounded(items, 0, |n| async move { n + 1 }, |_, _| true).await;
        assert_eq!(results, vec![2, 3, 4]);
    }

    /// (6) `on_result` returning `false` stops any further, not-yet-spawned
    /// item from ever starting — the fail-fast escape hatch
    /// `run_compile_sources` uses once a manifest save fails. At
    /// `limit == 1` this must produce a strict prefix of the input, exactly
    /// matching a `for` loop's `return Err` on the first failure.
    #[tokio::test]
    async fn on_result_returning_false_stops_dispatch_at_limit_one() {
        let items: Vec<u32> = vec![1, 2, 3, 4, 5];
        let executed = Arc::new(AsyncMutex::new(Vec::new()));

        let executed_task = Arc::clone(&executed);
        let results = run_bounded(
            items,
            1,
            move |n| {
                let executed = Arc::clone(&executed_task);
                async move {
                    executed.lock().await.push(n);
                    n
                }
            },
            |_idx, _result: &u32| false, // stop after the very first result
        )
        .await;

        assert_eq!(
            results,
            vec![1],
            "only the first item should have been dispatched"
        );
        assert_eq!(
            *executed.lock().await,
            vec![1],
            "items after the stop signal must never run at all"
        );
    }

    /// (7) Same escape hatch under real concurrency (`limit > 1`): stopping
    /// dispatch never cancels items already in flight — they still run to
    /// completion and still flow through `on_result` — but no item that
    /// hadn't started yet is ever spawned once the flag flips.
    #[tokio::test]
    async fn on_result_returning_false_stops_further_dispatch_but_not_in_flight_work() {
        const N: usize = 20;
        let items: Vec<usize> = (0..N).collect();
        let executed = Arc::new(AsyncMutex::new(Vec::new()));
        let stop_after = Arc::new(AtomicUsize::new(0));

        let executed_task = Arc::clone(&executed);
        let results = run_bounded(
            items,
            4,
            move |n| {
                let executed = Arc::clone(&executed_task);
                async move {
                    executed.lock().await.push(n);
                    n
                }
            },
            move |_idx, _result: &usize| {
                // Stop as soon as any 3 items have completed — well before
                // all N have had a chance to be dispatched.
                stop_after.fetch_add(1, Ordering::SeqCst) < 3
            },
        )
        .await;

        assert!(
            results.len() < N,
            "dispatch must stop before every item runs, got {} of {N}",
            results.len()
        );
        assert_eq!(
            executed.lock().await.len(),
            results.len(),
            "every dispatched item must have actually executed and reported a result"
        );
    }
}
