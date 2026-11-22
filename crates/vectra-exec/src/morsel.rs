use std::sync::atomic::{AtomicBool, Ordering};
use vectra_types::{Result, VectraError};

/// Morsel-Driven Parallel Scheduler (Leis et al., SIGMOD 2014).
///
/// Distributes independent work morsels via a lock-free `crossbeam_deque::Injector` and
/// per-worker `Worker`/`Stealer` deques inside `std::thread::scope`. Supports cooperative
/// cancellation and falls back cleanly to zero-overhead sequential execution when `threads <= 1`
/// or on `wasm32`.
pub fn execute_morsels<T, R, F>(
    morsels: Vec<T>,
    threads: usize,
    cancelled: &AtomicBool,
    worker_fn: F,
) -> Result<Vec<R>>
where
    T: Send,
    R: Send,
    F: Fn(usize, T) -> Result<R> + Sync,
{
    let n = morsels.len();
    if n == 0 {
        return Ok(Vec::new());
    }
    if cancelled.load(Ordering::Relaxed) {
        return Err(VectraError::Cancelled);
    }

    #[cfg(target_arch = "wasm32")]
    let effective_threads = 1usize;
    #[cfg(not(target_arch = "wasm32"))]
    let effective_threads = threads.clamp(1, n);

    if effective_threads <= 1 {
        let mut out = Vec::with_capacity(n);
        for (idx, item) in morsels.into_iter().enumerate() {
            if cancelled.load(Ordering::Relaxed) {
                return Err(VectraError::Cancelled);
            }
            out.push(worker_fn(idx, item)?);
        }
        return Ok(out);
    }

    #[cfg(not(target_arch = "wasm32"))]
    {
        use crossbeam_deque::{Injector, Steal, Worker};
        use parking_lot::Mutex;

        let injector: Injector<(usize, T)> = Injector::new();
        for (idx, item) in morsels.into_iter().enumerate() {
            injector.push((idx, item));
        }

        let workers: Vec<Worker<(usize, T)>> =
            (0..effective_threads).map(|_| Worker::new_fifo()).collect();
        let stealers: Vec<_> = workers.iter().map(Worker::stealer).collect();

        let results: Mutex<Vec<Option<R>>> = Mutex::new((0..n).map(|_| None).collect());
        let first_err: Mutex<Option<VectraError>> = Mutex::new(None);
        let aborted = AtomicBool::new(false);

        std::thread::scope(|scope| {
            for (tid, local) in workers.into_iter().enumerate() {
                let injector = &injector;
                let stealers = &stealers;
                let results = &results;
                let first_err = &first_err;
                let aborted = &aborted;
                let worker_fn = &worker_fn;

                scope.spawn(move || loop {
                    if aborted.load(Ordering::Relaxed) || cancelled.load(Ordering::Relaxed) {
                        break;
                    }
                    let task = local.pop().or_else(|| {
                        std::iter::repeat_with(|| {
                            injector.steal_batch_and_pop(&local).or_else(|| {
                                stealers
                                    .iter()
                                    .enumerate()
                                    .filter(|&(i, _)| i != tid)
                                    .map(|(_, s)| s.steal())
                                    .collect()
                            })
                        })
                        .find(|s| !s.is_retry())
                        .and_then(Steal::success)
                    });

                    let Some((idx, item)) = task else {
                        break;
                    };

                    match worker_fn(idx, item) {
                        Ok(res) => {
                            let mut guard = results.lock();
                            guard[idx] = Some(res);
                        }
                        Err(e) => {
                            aborted.store(true, Ordering::Release);
                            let mut eg = first_err.lock();
                            if eg.is_none() {
                                *eg = Some(e);
                            }
                            break;
                        }
                    }
                });
            }
        });

        if cancelled.load(Ordering::Relaxed) {
            return Err(VectraError::Cancelled);
        }
        if let Some(err) = first_err.into_inner() {
            return Err(err);
        }
        let mut out = Vec::with_capacity(n);
        out.extend(results.into_inner().into_iter().flatten());
        Ok(out)
    }
}
