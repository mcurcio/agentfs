//! A small worker pool for LFS transfers (design §7): up to
//! `lfs.concurrenttransfers` objects move at once, each on a scoped
//! thread, so a transfer borrows its cache and client without `'static`
//! bounds.
//!
//! Workers take the next item from a shared index. Once an item fails
//! with an error the caller says should stop the run (a server that
//! cannot be reached, refused credentials), no new item is started; the
//! items already in flight finish, so their progress counts (an uploaded
//! object is not sent again, design §7), and the items never started come
//! back as `None`. Results are in input order.

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use super::LfsError;

/// Runs `f` over `items` on up to `workers` threads (at least one). An
/// error for which `stop` is true starts no further item. Each result is
/// at its item's index; `None` for items never started.
pub fn run<I, T, F, S>(
    items: &[I],
    workers: usize,
    stop: S,
    f: F,
) -> Vec<Option<Result<T, LfsError>>>
where
    I: Sync,
    T: Send,
    F: Fn(&I) -> Result<T, LfsError> + Sync,
    S: Fn(&LfsError) -> bool + Sync,
{
    let mut results: Vec<Option<Result<T, LfsError>>> = Vec::with_capacity(items.len());
    results.resize_with(items.len(), || None);
    if items.is_empty() {
        return results;
    }
    let workers = workers.clamp(1, items.len());
    if workers == 1 {
        // No threads for the common single-object or sequential case.
        for (slot, item) in results.iter_mut().zip(items) {
            let result = f(item);
            let halt = matches!(&result, Err(e) if stop(e));
            *slot = Some(result);
            if halt {
                break;
            }
        }
        return results;
    }
    let next = AtomicUsize::new(0);
    let halted = AtomicBool::new(false);
    let slots: Mutex<&mut Vec<Option<Result<T, LfsError>>>> = Mutex::new(&mut results);
    std::thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(|| {
                loop {
                    if halted.load(Ordering::SeqCst) {
                        return;
                    }
                    let index = next.fetch_add(1, Ordering::SeqCst);
                    let Some(item) = items.get(index) else {
                        return;
                    };
                    let result = f(item);
                    if matches!(&result, Err(e) if stop(e)) {
                        halted.store(true, Ordering::SeqCst);
                    }
                    slots.lock().unwrap_or_else(|e| e.into_inner())[index] = Some(result);
                }
            });
        }
    });
    results
}

/// The first error among `results`, in input order.
pub fn first_error<T>(results: &[Option<Result<T, LfsError>>]) -> Option<&LfsError> {
    results.iter().find_map(|r| match r {
        Some(Err(e)) => Some(e),
        _ => None,
    })
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::events::PushFailureKind;

    #[test]
    fn at_most_n_items_are_in_flight_and_results_keep_their_order() {
        for workers in [1, 3, 8] {
            let in_flight = AtomicUsize::new(0);
            let max = AtomicUsize::new(0);
            let items: Vec<u32> = (0..20).collect();
            let results = run(
                &items,
                workers,
                |_| true,
                |i| {
                    let now = in_flight.fetch_add(1, Ordering::SeqCst) + 1;
                    max.fetch_max(now, Ordering::SeqCst);
                    std::thread::sleep(Duration::from_millis(5));
                    in_flight.fetch_sub(1, Ordering::SeqCst);
                    Ok(i * 2)
                },
            );
            let got: Vec<u32> = results.into_iter().map(|r| r.unwrap().unwrap()).collect();
            assert_eq!(got, items.iter().map(|i| i * 2).collect::<Vec<_>>());
            let max = max.load(Ordering::SeqCst);
            assert!(max <= workers, "{max} in flight with {workers} workers");
            if workers > 1 {
                assert!(max > 1, "never concurrent with {workers} workers");
            }
        }
    }

    #[test]
    fn a_stopping_error_starts_nothing_new_but_lets_in_flight_items_finish() {
        let items: Vec<u32> = (0..50).collect();
        let finished = AtomicUsize::new(0);
        let results = run(
            &items,
            4,
            |e| e.kind == PushFailureKind::Unreachable,
            |&i| {
                if i == 2 {
                    std::thread::sleep(Duration::from_millis(20));
                    return Err(LfsError::new(PushFailureKind::Unreachable, "down"));
                }
                std::thread::sleep(Duration::from_millis(30));
                finished.fetch_add(1, Ordering::SeqCst);
                Ok(i)
            },
        );
        assert_eq!(first_error(&results).unwrap().message, "down");
        // Items 0, 1 and 3 were in flight when 2 failed: they finished.
        for i in [0, 1, 3] {
            assert!(matches!(results[i], Some(Ok(_))), "{i}: {:?}", results[i]);
        }
        let not_started = results.iter().filter(|r| r.is_none()).count();
        assert!(not_started > 40, "{not_started} not started");
        assert_eq!(
            finished.load(Ordering::SeqCst),
            results.iter().filter(|r| matches!(r, Some(Ok(_)))).count()
        );
        // Sequentially too.
        let results = run(
            &items,
            1,
            |_| true,
            |&i| {
                if i == 1 {
                    Err(LfsError::local("x"))
                } else {
                    Ok(i)
                }
            },
        );
        assert!(matches!(results[0], Some(Ok(0))));
        assert!(results[1].as_ref().unwrap().is_err());
        assert!(results[2..].iter().all(Option::is_none));
    }

    #[test]
    fn other_errors_do_not_stop_the_run() {
        let items: Vec<u32> = (0..10).collect();
        let results = run(
            &items,
            3,
            |_| false,
            |&i| {
                if i % 2 == 0 {
                    Err(LfsError::local("even"))
                } else {
                    Ok(i)
                }
            },
        );
        assert!(results.iter().all(Option::is_some));
        assert_eq!(
            results.iter().filter(|r| matches!(r, Some(Ok(_)))).count(),
            5
        );
        assert!(run(&[] as &[u32], 4, |_| true, |&i| Ok(i)).is_empty());
    }
}
