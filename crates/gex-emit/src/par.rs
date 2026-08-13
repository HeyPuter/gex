//! Spread a pure map over the machine.
//!
//! `f` must be pure: results come back in input order, never completion order,
//! because the callers produce wasm whose bytes key a content-addressed cache,
//! and a translation that varied with thread scheduling would never hit it.

/// How wide to go, honouring `GEX_LIFT_THREADS`.
#[cfg(feature = "parallel")]
pub fn threads() -> usize {
    match std::env::var("GEX_LIFT_THREADS")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
    {
        Some(n) if n >= 1 => n,
        _ => std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1),
    }
}

/// Apply `f` to every item, on as many cores as there are, in order.
#[cfg(feature = "parallel")]
pub fn map<T, R, F>(items: &[T], f: &F) -> Vec<R>
where
    T: Sync,
    R: Send,
    F: Fn(&T) -> R + Sync,
{
    // Below this the threads cost more than the work does.
    const MIN_PARALLEL: usize = 64;
    let threads = threads();
    if threads < 2 || items.len() < MIN_PARALLEL {
        return items.iter().map(f).collect();
    }

    // Several chunks per thread rather than one each: the items are far from
    // equal, so one chunk per thread leaves the machine waiting on whichever
    // slice drew the expensive ones.
    let chunk = items.len().div_ceil(threads * 8).max(1);
    let chunks: Vec<&[T]> = items.chunks(chunk).collect();
    let next = std::sync::atomic::AtomicUsize::new(0);
    let results: Vec<std::sync::Mutex<Vec<R>>> = (0..chunks.len())
        .map(|_| std::sync::Mutex::new(Vec::new()))
        .collect();

    std::thread::scope(|scope| {
        let (next, chunks, results) = (&next, &chunks, &results);
        let handles: Vec<_> = (0..threads.min(chunks.len()))
            .map(|_| {
                scope.spawn(move || loop {
                    let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    let Some(part) = chunks.get(i) else { return };
                    let done: Vec<R> = part.iter().map(f).collect();
                    *results[i].lock().unwrap_or_else(|e| e.into_inner()) = done;
                })
            })
            .collect();
        for h in handles {
            // A panic in the lifter or the emitter is a bug: let it out rather
            // than quietly translating less than was asked for.
            if let Err(e) = h.join() {
                std::panic::resume_unwind(e);
            }
        }
    });

    results
        .into_iter()
        .flat_map(|m| m.into_inner().unwrap_or_else(|e| e.into_inner()))
        .collect()
}

/// Rewrite every item in place, on as many cores as there are.
///
/// The index comes with the item because the callers key their own
/// precomputed state by position -- liveness per block, say -- and there is
/// no way to recover it from a `&mut T` alone.
#[cfg(feature = "parallel")]
pub fn map_mut<T, R, F>(items: &mut [T], f: &F) -> Vec<R>
where
    T: Send,
    R: Send + Default + Clone,
    F: Fn(usize, &mut T) -> R + Sync,
{
    const MIN_PARALLEL: usize = 64;
    let threads = threads();
    if threads < 2 || items.len() < MIN_PARALLEL {
        return items.iter_mut().enumerate().map(|(i, t)| f(i, t)).collect();
    }

    let chunk = items.len().div_ceil(threads * 8).max(1);
    let mut out: Vec<R> = vec![R::default(); items.len()];
    // Chunks of both, taken in step, handed out to whichever thread is free.
    // A `&mut` slice cannot be shared, so the queue owns them and a thread
    // takes one out rather than borrowing it.
    let queue: std::sync::Mutex<Vec<(usize, &mut [T], &mut [R])>> = std::sync::Mutex::new(
        items
            .chunks_mut(chunk)
            .zip(out.chunks_mut(chunk))
            .enumerate()
            .map(|(n, (a, b))| (n * chunk, a, b))
            .rev()
            .collect(),
    );

    std::thread::scope(|scope| {
        let queue = &queue;
        let handles: Vec<_> = (0..threads)
            .map(|_| {
                scope.spawn(move || loop {
                    let Some((base, part, dst)) =
                        queue.lock().unwrap_or_else(|e| e.into_inner()).pop()
                    else {
                        return;
                    };
                    for (k, item) in part.iter_mut().enumerate() {
                        dst[k] = f(base + k, item);
                    }
                })
            })
            .collect();
        for h in handles {
            if let Err(e) = h.join() {
                std::panic::resume_unwind(e);
            }
        }
    });
    out
}

/// The single-threaded map, for a build with no threads to spread over --
/// the browser shim, which reaches this code compiled to wasm.
#[cfg(not(feature = "parallel"))]
pub fn map<T, R, F>(items: &[T], f: &F) -> Vec<R>
where
    F: Fn(&T) -> R,
{
    items.iter().map(f).collect()
}

/// The single-threaded in-place rewrite. See [`map`].
#[cfg(not(feature = "parallel"))]
pub fn map_mut<T, R, F>(items: &mut [T], f: &F) -> Vec<R>
where
    F: Fn(usize, &mut T) -> R,
{
    items.iter_mut().enumerate().map(|(i, t)| f(i, t)).collect()
}

#[cfg(test)]
mod tests {
    /// Order follows the input, not whichever thread finished first.
    #[test]
    fn results_come_back_in_order() {
        let items: Vec<usize> = (0..5000).collect();
        let out = super::map(&items, &|n: &usize| n * 2);
        assert_eq!(out.len(), items.len());
        for (i, v) in out.iter().enumerate() {
            assert_eq!(*v, i * 2, "item {i} came back out of order");
        }
    }

    /// A list too short to be worth threads still comes back complete.
    #[test]
    fn a_short_list_is_still_mapped() {
        let items: Vec<usize> = (0..3).collect();
        assert_eq!(super::map(&items, &|n: &usize| n + 1), vec![1, 2, 3]);
    }
}

/// A stopwatch that exists only where there is a clock.
///
/// `std::time::Instant::now()` *panics* on wasm32-unknown-unknown ("time not
/// implemented on this platform"), and a wasm panic is a bare `unreachable`
/// with no file, line or message -- so a stray `Instant::now` in shared code
/// takes the browser down. Hence the target-gated field.
pub struct Stopwatch(#[cfg(not(target_family = "wasm"))] std::time::Instant);

impl Stopwatch {
    pub fn start() -> Stopwatch {
        Stopwatch(
            #[cfg(not(target_family = "wasm"))]
            std::time::Instant::now(),
        )
    }

    /// Seconds since `start`, and zero where there is no clock to ask.
    pub fn secs(&self) -> f64 {
        #[cfg(not(target_family = "wasm"))]
        {
            self.0.elapsed().as_secs_f64()
        }
        #[cfg(target_family = "wasm")]
        {
            0.0
        }
    }
}
