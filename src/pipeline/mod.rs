pub(crate) mod describe;
pub(crate) mod improve;
pub(crate) mod review;

use crate::input::Input;
use crate::secretscan;
use anyhow::{anyhow, Context, Result};
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

pub(crate) fn prepare_out(p: &Path) -> Result<PathBuf> {
    std::fs::create_dir_all(p)
        .with_context(|| format!("failed to create output directory: {}", p.display()))?;
    Ok(p.to_path_buf())
}

/// #122: called right after `input::normalize`, before any LLM call. Refuses to proceed (unless
/// `allow_sensitive_input`) when the diff (or, per #137, requirements/conventions — both are
/// sent to the LLM verbatim just like the diff, see `promptctx::shared_context`) contains
/// something that looks like a credential.
pub(crate) fn enforce_secret_scan(inp: &Input, allow_sensitive_input: bool) -> Result<()> {
    let mut hits = secretscan::scan(&inp.diff);
    if let Some(r) = &inp.requirements {
        hits.extend(secretscan::scan_text("requirements", r));
    }
    if let Some(c) = &inp.conventions {
        hits.extend(secretscan::scan_text("conventions", c));
    }
    if hits.is_empty() || allow_sensitive_input {
        return Ok(());
    }
    let mut msg = format!(
        "refusing to send diff to the LLM: found {} value(s) that look like credentials:\n",
        hits.len()
    );
    for h in &hits {
        msg.push_str(&format!(
            "  - {} in {}: {}\n",
            h.pattern, h.file, h.redacted
        ));
    }
    msg.push_str(
        "Remove the secret(s) from the diff, or pass --allow-sensitive-input to send it anyway.",
    );
    Err(anyhow!(msg))
}

/// Runs `f` over `items` on up to `concurrency` worker threads pulling from a shared queue.
/// Collects each item's result as an individual Result (processing continues even if one fails) —
/// the caller decides whether to ignore partial failures as-is or filter out errors and continue.
/// It used to abort everything on the first failure, which is excessive for independent items like lens reviews (would wipe out the other lenses too).
///
/// #166: this used to slice `items` into fixed `concurrency`-sized chunks and join()
/// each chunk fully before starting the next one — a single slow item in a chunk blocked every
/// other worker from picking up the next chunk's work, even if they'd already gone idle. A
/// shared work queue lets a freed-up worker immediately pull the next item instead of waiting
/// for its chunk-mates.
pub(crate) fn par_map<T, R, F>(concurrency: usize, items: Vec<T>, f: F) -> Vec<Result<R>>
where
    T: Send,
    R: Send,
    F: Fn(T) -> Result<R> + Sync,
{
    use std::panic::{catch_unwind, AssertUnwindSafe};

    let item_count = items.len();
    if item_count == 0 {
        return Vec::new();
    }
    let worker_count = concurrency.max(1).min(item_count);
    let jobs: Mutex<VecDeque<(usize, T)>> = Mutex::new(items.into_iter().enumerate().collect());
    let results: Mutex<Vec<(usize, Result<R>)>> = Mutex::new(Vec::with_capacity(item_count));

    std::thread::scope(|s| {
        for _ in 0..worker_count {
            s.spawn(|| loop {
                let job = jobs.lock().unwrap_or_else(|e| e.into_inner()).pop_front();
                let Some((index, item)) = job else { break };
                // A persistent worker thread (unlike the old one-thread-per-item chunk) must not
                // let one item's panic take the whole worker — and with it, every job still left
                // in the queue — down with it.
                let result = catch_unwind(AssertUnwindSafe(|| f(item)))
                    .unwrap_or_else(|_| Err(anyhow!("worker thread panicked")));
                results
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push((index, result));
            });
        }
    });

    let mut results = results.into_inner().unwrap_or_else(|e| e.into_inner());
    // Preserve input order regardless of which worker finished which item when.
    results.sort_by_key(|(index, _)| *index);
    results.into_iter().map(|(_, r)| r).collect()
}

#[cfg(test)]
mod par_map_tests {
    use super::*;

    #[test]
    fn par_map_keeps_successful_results_when_one_item_fails() {
        let items = vec![1, 2, 3];
        let results = par_map(2, items, |i| {
            if i == 2 {
                Err(anyhow!("boom on {i}"))
            } else {
                Ok(i * 10)
            }
        });
        assert_eq!(results.len(), 3);
        let (oks, errs): (Vec<_>, Vec<_>) = results.into_iter().partition(Result::is_ok);
        assert_eq!(oks.len(), 2);
        assert_eq!(errs.len(), 1);
    }

    #[test]
    fn par_map_preserves_all_successes_when_nothing_fails() {
        let items = vec![1, 2, 3, 4, 5];
        let results = par_map(3, items, |i| Ok::<_, anyhow::Error>(i * 2));
        let values: Vec<i32> = results.into_iter().map(|r| r.unwrap()).collect();
        let mut sorted = values.clone();
        sorted.sort();
        assert_eq!(sorted, vec![2, 4, 6, 8, 10]);
    }

    #[test]
    fn par_map_converts_a_genuine_worker_panic_into_an_err_instead_of_propagating_it() {
        // #113: this is what the outer lens_handle/good_things_handle .join() in
        // pipeline/review.rs relies on being true one layer down — an actual panic!(), not
        // just a returned Err, must not take the whole test process down.
        let default_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {})); // suppress the panic backtrace noise in test output
        let items = vec![1, 2, 3];
        let results = par_map(2, items, |i| {
            if i == 2 {
                panic!("boom on {i}");
            }
            Ok::<_, anyhow::Error>(i * 10)
        });
        std::panic::set_hook(default_hook);

        assert_eq!(results.len(), 3);
        let (oks, errs): (Vec<_>, Vec<_>) = results.into_iter().partition(Result::is_ok);
        assert_eq!(oks.len(), 2);
        assert_eq!(errs.len(), 1);
    }

    #[test]
    fn par_map_returns_results_in_input_order_regardless_of_completion_order() {
        // Item 0 deliberately finishes last — a naive "push results as they complete" approach
        // would put it last in the output too, instead of preserving input order.
        use std::time::Duration;
        let items = vec![30u64, 0, 0];
        let results = par_map(3, items, |ms| {
            std::thread::sleep(Duration::from_millis(ms));
            Ok::<_, anyhow::Error>(ms)
        });
        let values: Vec<u64> = results.into_iter().map(|r| r.unwrap()).collect();
        assert_eq!(values, vec![30, 0, 0]);
    }

    #[test]
    fn par_map_lets_a_freed_worker_pick_up_the_next_item_instead_of_waiting_on_a_slow_chunk_mate() {
        // #166: 4 items, concurrency 3 — one slow (60ms) plus three fast (15ms each). The old
        // fixed-chunk barrier would run [60,15,15] together (bounded by the 60ms item), THEN run
        // the 4th 15ms item in a second chunk afterward — roughly 75ms+ total. A work-stealing
        // pool lets whichever fast worker frees up first immediately pick up the 4th item
        // (~30ms for that worker), so the whole call is bounded by the single 60ms item instead.
        use std::time::{Duration, Instant};
        let items = vec![60u64, 15, 15, 15];
        let start = Instant::now();
        let results = par_map(3, items, |ms| {
            std::thread::sleep(Duration::from_millis(ms));
            Ok::<_, anyhow::Error>(ms)
        });
        let elapsed = start.elapsed();
        assert_eq!(results.len(), 4);
        assert!(
            elapsed < Duration::from_millis(70),
            "expected work-stealing to finish near the slowest single item (~60ms), took {elapsed:?} \
             (a fixed-chunk barrier would take ~75ms+)"
        );
    }

    #[test]
    fn par_map_returns_empty_immediately_for_an_empty_input() {
        let results: Vec<Result<i32>> = par_map(4, Vec::new(), Ok);
        assert!(results.is_empty());
    }
}
