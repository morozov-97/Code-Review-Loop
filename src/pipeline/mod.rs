pub(crate) mod describe;
pub(crate) mod improve;
pub(crate) mod review;

use crate::secretscan;
use anyhow::{anyhow, Context, Result};
use std::path::{Path, PathBuf};

pub(crate) fn prepare_out(p: &Path) -> Result<PathBuf> {
    std::fs::create_dir_all(p)
        .with_context(|| format!("failed to create output directory: {}", p.display()))?;
    Ok(p.to_path_buf())
}

/// #122: called right after `input::normalize`, before any LLM call. Refuses to proceed (unless
/// `allow_sensitive_input`) when the diff's added lines contain something that looks like a
/// credential — the diff is about to be sent verbatim to an external LLM provider.
pub(crate) fn enforce_secret_scan(diff: &str, allow_sensitive_input: bool) -> Result<()> {
    let hits = secretscan::scan(diff);
    if hits.is_empty() || allow_sensitive_input {
        return Ok(());
    }
    let mut msg = format!(
        "refusing to send diff to the LLM: found {} value(s) that look like credentials in added lines:\n",
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

/// Groups threads by concurrency and runs them in sequence (chunk-wise barrier).
/// Collects each item's result as an individual Result (processing continues even if one fails) —
/// the caller decides whether to ignore partial failures as-is or filter out errors and continue.
/// It used to abort everything on the first failure, which is excessive for independent items like lens reviews (would wipe out the other lenses too).
pub(crate) fn par_map<T, R, F>(concurrency: usize, items: Vec<T>, f: F) -> Vec<Result<R>>
where
    T: Send,
    R: Send,
    F: Fn(T) -> Result<R> + Sync,
{
    let c = concurrency.max(1);
    let mut out: Vec<Result<R>> = Vec::new();
    let mut rest = items;
    while !rest.is_empty() {
        let take = c.min(rest.len());
        let chunk: Vec<T> = rest.drain(..take).collect();
        let mut results: Vec<Result<R>> = std::thread::scope(|s| {
            let handles: Vec<_> = chunk.into_iter().map(|item| s.spawn(|| f(item))).collect();
            handles
                .into_iter()
                .map(|h| {
                    h.join()
                        .map_err(|_| anyhow!("worker thread panicked"))
                        .and_then(|r| r)
                })
                .collect()
        });
        out.append(&mut results);
    }
    out
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
}
