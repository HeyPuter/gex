//! Spread a pure map over the machine.
//!
//! `f` must be pure: results come back in input order, never completion order,
//! because the callers produce wasm whose bytes key a content-addressed cache,
//! and a translation that varied with thread scheduling would never hit it.

/// How wide to go, honouring `GEX_LIFT_THREADS`. Found once: std asks the
/// host's cgroup files each time (four calls), and every translation batch
/// asks.
#[cfg(feature = "parallel")]
pub fn threads() -> usize {
    static N: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *N.get_or_init(|| {
        match std::env::var("GEX_LIFT_THREADS")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
        {
            Some(n) if n >= 1 => n,
            _ => std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(1),
        }
    })
}

/// Items each thread is given at least: below that a helper costs more to
/// start than its share of the work (a block's lift is microseconds, a
/// thread's start and its allocator arena more), so a small batch runs on
/// fewer threads, and one under twice this on the caller alone.
#[cfg(feature = "parallel")]
const PER_THREAD: usize = 64;

/// How many threads a batch of `n` items runs on, the caller's included.
#[cfg(feature = "parallel")]
fn width(n: usize) -> usize {
    threads().min(n / PER_THREAD).max(1)
}

/// Helper threads running now, across every caller in the process. One
/// budget of [`threads`] for them all: each translating guest thread asked
/// for `threads()` of its own, so a program with 125 threads could ask for
/// some 3000 at once, and a spawn refused under a task limit (`TasksMax`)
/// panicked the guest thread that asked (`scope.spawn`).
#[cfg(feature = "parallel")]
static HELPERS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// The most helpers that have run at once (tests only).
#[cfg(all(feature = "parallel", test))]
static PEAK: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Refuse every spawn, as a host out of tasks does (tests only).
#[cfg(all(feature = "parallel", test))]
static REFUSE_SPAWN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// One helper's place in the budget, given back when it ends.
#[cfg(feature = "parallel")]
struct Helper;

#[cfg(feature = "parallel")]
impl Helper {
    /// A place, if the budget has one left.
    fn take() -> Option<Helper> {
        use std::sync::atomic::Ordering;
        let cap = threads();
        let mut n = HELPERS.load(Ordering::Relaxed);
        loop {
            if n >= cap {
                return None;
            }
            match HELPERS.compare_exchange_weak(n, n + 1, Ordering::AcqRel, Ordering::Relaxed) {
                Ok(_) => {
                    #[cfg(test)]
                    PEAK.fetch_max(n + 1, Ordering::Relaxed);
                    return Some(Helper);
                }
                Err(now) => n = now,
            }
        }
    }
}

#[cfg(feature = "parallel")]
impl Drop for Helper {
    fn drop(&mut self) {
        HELPERS.fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
    }
}

/// Run `work` on the calling thread and on up to `want - 1` helpers, as
/// many as the process-wide budget has room for and the host will start: a
/// refused spawn means less width, never a failure. `work` takes its pieces
/// from a shared queue until it is empty, so which thread did a piece does
/// not matter to the result.
#[cfg(feature = "parallel")]
fn spread(want: usize, work: &(dyn Fn() + Sync)) {
    std::thread::scope(|scope| {
        let mut handles = Vec::new();
        for _ in 1..want {
            let Some(place) = Helper::take() else { break };
            #[cfg(test)]
            if REFUSE_SPAWN.load(std::sync::atomic::Ordering::Relaxed) {
                break;
            }
            match std::thread::Builder::new().name("gex-lift".into()).spawn_scoped(scope, move || {
                let _place = place;
                work();
            }) {
                Ok(h) => handles.push(h),
                // The closure, and the place in it, are dropped with the error.
                Err(_) => break,
            }
        }
        work();
        for h in handles {
            // A panic in the lifter or the emitter is a bug: let it out rather
            // than quietly translating less than was asked for.
            if let Err(e) = h.join() {
                std::panic::resume_unwind(e);
            }
        }
    });
}

/// Apply `f` to every item, on as many cores as there are, in order.
#[cfg(feature = "parallel")]
pub fn map<T, R, F>(items: &[T], f: &F) -> Vec<R>
where
    T: Sync,
    R: Send,
    F: Fn(&T) -> R + Sync,
{
    let threads = width(items.len());
    if threads < 2 {
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
    spread(threads.min(chunks.len()), &|| loop {
        let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let Some(part) = chunks.get(i) else { return };
        let done: Vec<R> = part.iter().map(f).collect();
        *results[i].lock().unwrap_or_else(|e| e.into_inner()) = done;
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
    let threads = width(items.len());
    if threads < 2 {
        return items.iter_mut().enumerate().map(|(i, t)| f(i, t)).collect();
    }

    let chunk = items.len().div_ceil(threads * 8).max(1);
    let mut out: Vec<R> = vec![R::default(); items.len()];
    {
        // Chunks of both, taken in step, handed out to whichever thread is
        // free. A `&mut` slice cannot be shared, so the queue owns them and a
        // thread takes one out rather than borrowing it.
        let queue: std::sync::Mutex<Vec<(usize, &mut [T], &mut [R])>> = std::sync::Mutex::new(
            items
                .chunks_mut(chunk)
                .zip(out.chunks_mut(chunk))
                .enumerate()
                .map(|(n, (a, b))| (n * chunk, a, b))
                .rev()
                .collect(),
        );
        spread(threads, &|| loop {
            let Some((base, part, dst)) = queue.lock().unwrap_or_else(|e| e.into_inner()).pop() else {
                return;
            };
            for (k, item) in part.iter_mut().enumerate() {
                dst[k] = f(base + k, item);
            }
        });
    }
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

    /// Many callers at once share one budget of helpers, never more than
    /// `threads()` running in all; and with every spawn refused (a host out
    /// of tasks) each caller does its own work: the same results, no panic.
    #[cfg(feature = "parallel")]
    #[test]
    fn many_callers_share_one_budget_and_survive_refused_spawns() {
        use std::sync::atomic::Ordering;
        let items: Vec<u64> = (0..20_000).collect();
        let want: Vec<u64> = items.iter().map(|n| n * n + 7).collect();
        let mut_want: Vec<usize> = (0..items.len()).map(|i| i * 3).collect();
        for refuse in [false, true] {
            super::REFUSE_SPAWN.store(refuse, Ordering::Relaxed);
            std::thread::scope(|s| {
                for _ in 0..32 {
                    s.spawn(|| {
                        for _ in 0..4 {
                            assert_eq!(super::map(&items, &|n: &u64| n * n + 7), want);
                            let mut v = items.clone();
                            let r = super::map_mut(&mut v, &|i: usize, x: &mut u64| {
                                *x += 1;
                                i * 3
                            });
                            assert_eq!(r, mut_want);
                            assert!(v.iter().zip(&items).all(|(a, b)| *a == b + 1));
                        }
                    });
                }
            });
        }
        super::REFUSE_SPAWN.store(false, Ordering::Relaxed);
        assert!(super::PEAK.load(Ordering::Relaxed) <= super::threads());
    }

    /// A batch too small to share runs on the caller alone, and one big
    /// enough on every thread there is, never more.
    #[cfg(feature = "parallel")]
    #[test]
    fn a_batch_runs_as_wide_as_its_size_warrants() {
        assert_eq!(super::width(0), 1);
        assert_eq!(super::width(2 * super::PER_THREAD - 1), 1);
        assert_eq!(super::width(1 << 30), super::threads());
        assert!(super::width(4 * super::PER_THREAD) <= 4);
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
