//! Kept helper threads for [`crate::host::in_parallel`].
//!
//! `in_parallel` used to start its helpers with `std::thread::scope` on
//! every call. The submitting thread calls it several times a submission,
//! and starting and joining threads costs hundreds of microseconds each
//! time on Windows (`ZwCreateThreadEx` showed in the submitting thread's
//! profile). The helpers are now started once and kept.
//!
//! A call offers its work to the kept threads, up to a number of them, and
//! does the work itself as well. Once the caller's own share is done the
//! offer is withdrawn: no thread takes it after that, and the caller waits
//! only for the threads that took it. A call therefore never waits for a
//! thread busy with something else (another call, or a call nested in its
//! own work): it cannot deadlock, and a call no thread helps runs as it
//! would alone. A panic in a helper is caught there and raised again in the
//! caller, as `std::thread::scope` did; the helper goes on.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};

/// How many helper threads are kept.
pub(crate) const HELPERS: usize = 5;

/// The work of one call, with its lifetime erased. The caller of [`run`]
/// does not return before every helper that took it has finished, so it
/// outlives every use.
#[derive(Clone, Copy)]
struct Work(*const (dyn Fn() + Sync));

// SAFETY: the pointee is `Sync`, and it lives until the last helper that
// took it is done (see `run`).
unsafe impl Send for Work {}
// SAFETY: as above; the pointer is only read, and its pointee is `Sync`.
unsafe impl Sync for Work {}

#[derive(Default)]
struct Taken {
    /// Helpers that may still take the offer.
    left: usize,
    /// Helpers running the work now.
    running: usize,
    /// The first panic a helper caught.
    panic: Option<Box<dyn std::any::Any + Send>>,
}

struct Call {
    work: Work,
    taken: Mutex<Taken>,
    finished: Condvar,
}

struct Pool {
    /// Calls on offer. Locked before a call's `taken`, never after.
    open: Mutex<Vec<Arc<Call>>>,
    offered: Condvar,
    /// How many helpers were started.
    started: AtomicUsize,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn pool() -> &'static Pool {
    static POOL: OnceLock<&'static Pool> = OnceLock::new();
    POOL.get_or_init(|| {
        let pool: &'static Pool =
            Box::leak(Box::new(Pool { open: Mutex::new(Vec::new()), offered: Condvar::new(), started: AtomicUsize::new(0) }));
        for i in 0..HELPERS {
            let spawned = std::thread::Builder::new()
                .name(format!("victory-parallel-{i}"))
                .spawn(move || helper(pool));
            match spawned {
                Ok(_) => {
                    pool.started.fetch_add(1, Ordering::Relaxed);
                }
                Err(e) => {
                    tracing::warn!(i, error = %e, "a kept helper thread did not start; calls use the ones that did");
                    break;
                }
            }
        }
        pool
    })
}

/// A kept helper: takes the first call on offer, runs it, and waits for the
/// next one.
fn helper(pool: &'static Pool) {
    loop {
        let call = {
            let mut open = lock(&pool.open);
            loop {
                let found = open.iter().position(|call| {
                    let mut taken = lock(&call.taken);
                    if taken.left == 0 {
                        return false;
                    }
                    taken.left -= 1;
                    taken.running += 1;
                    true
                });
                if let Some(at) = found {
                    let call = open[at].clone();
                    if lock(&call.taken).left == 0 {
                        open.swap_remove(at);
                    }
                    break call;
                }
                open = pool.offered.wait(open).unwrap_or_else(|e| e.into_inner());
            }
        };
        let work = call.work;
        // SAFETY: the caller waits for this helper before its work goes away.
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe { (*work.0)() }));
        let mut taken = lock(&call.taken);
        taken.running -= 1;
        if let Err(panic) = outcome {
            taken.panic.get_or_insert(panic);
        }
        if taken.running == 0 {
            call.finished.notify_all();
        }
    }
}

/// Withdraws a call's offer and waits for the helpers that took it, also
/// when the caller's own share panics.
struct Withdraw<'a> {
    pool: &'static Pool,
    call: &'a Arc<Call>,
}

impl Drop for Withdraw<'_> {
    fn drop(&mut self) {
        {
            let mut open = lock(&self.pool.open);
            open.retain(|c| !Arc::ptr_eq(c, self.call));
            lock(&self.call.taken).left = 0;
        }
        let mut taken = lock(&self.call.taken);
        while taken.running > 0 {
            taken = self.call.finished.wait(taken).unwrap_or_else(|e| e.into_inner());
        }
    }
}

/// Runs `work` on the calling thread and on up to `helpers` kept threads
/// that are free meanwhile; returns once every run of it has ended. A panic
/// in a helper's run is raised here.
pub(crate) fn run(helpers: usize, work: &(dyn Fn() + Sync)) {
    let pool = pool();
    let helpers = helpers.min(pool.started.load(Ordering::Relaxed));
    if helpers == 0 {
        return work();
    }
    // SAFETY: only the lifetime is erased; `Withdraw` waits for every helper
    // that took the work before this function returns or unwinds.
    let erased: *const (dyn Fn() + Sync + 'static) = unsafe { std::mem::transmute(work as *const (dyn Fn() + Sync)) };
    let call = Arc::new(Call {
        work: Work(erased),
        taken: Mutex::new(Taken { left: helpers, ..Taken::default() }),
        finished: Condvar::new(),
    });
    lock(&pool.open).push(call.clone());
    if helpers == 1 {
        pool.offered.notify_one();
    } else {
        pool.offered.notify_all();
    }
    {
        let _withdraw = Withdraw { pool, call: &call };
        work();
    }
    if let Some(panic) = lock(&call.taken).panic.take() {
        std::panic::resume_unwind(panic);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    fn on_helper() -> bool {
        std::thread::current().name().is_some_and(|n| n.starts_with("victory-parallel-"))
    }

    /// Every item of a list is done exactly once.
    #[test]
    fn every_item_is_done_once() {
        let items: Vec<usize> = (0..10_000).collect();
        let done: Vec<AtomicUsize> = items.iter().map(|_| AtomicUsize::new(0)).collect();
        for _ in 0..20 {
            crate::host::in_parallel(&items, 8, |piece| {
                for &i in piece {
                    done[i].fetch_add(1, Ordering::Relaxed);
                }
            });
        }
        assert!(done.iter().all(|d| d.load(Ordering::Relaxed) == 20));
    }

    /// Many calls use the same few threads.
    #[test]
    fn calls_share_the_kept_threads() {
        let names = Mutex::new(BTreeSet::new());
        let items: Vec<usize> = (0..4096).collect();
        for _ in 0..50 {
            crate::host::in_parallel(&items, 8, |_| {
                if on_helper() {
                    lock(&names).insert(std::thread::current().name().unwrap().to_owned());
                }
                std::thread::sleep(std::time::Duration::from_micros(50));
            });
        }
        let names = names.into_inner().unwrap();
        assert!(!names.is_empty(), "no helper took any work");
        assert!(names.len() <= HELPERS, "{names:?}");
    }

    /// A call made inside a call's work ends, with every item done.
    #[test]
    fn a_call_inside_a_call_ends() {
        let outer: Vec<usize> = (0..64).collect();
        let inner: Vec<usize> = (0..256).collect();
        let done = AtomicUsize::new(0);
        crate::host::in_parallel(&outer, 1, |piece| {
            for _ in piece {
                crate::host::in_parallel(&inner, 8, |p| {
                    done.fetch_add(p.len(), Ordering::Relaxed);
                });
            }
        });
        assert_eq!(done.load(Ordering::Relaxed), 64 * 256);
    }

    /// Calls from several threads at once all end, each item done once.
    #[test]
    fn calls_from_several_threads_at_once_end() {
        let items: Vec<usize> = (0..2048).collect();
        let total = AtomicUsize::new(0);
        std::thread::scope(|s| {
            for _ in 0..8 {
                s.spawn(|| {
                    for _ in 0..20 {
                        crate::host::in_parallel(&items, 8, |p| {
                            total.fetch_add(p.len(), Ordering::Relaxed);
                        });
                    }
                });
            }
        });
        assert_eq!(total.load(Ordering::Relaxed), 8 * 20 * 2048);
    }

    /// A panic in a helper reaches the caller, and the helpers go on.
    #[test]
    fn a_panic_in_a_helper_reaches_the_caller() {
        let mut caught = false;
        for _ in 0..50 {
            let outcome = std::panic::catch_unwind(|| {
                run(HELPERS, &|| {
                    if on_helper() {
                        panic!("a helper's panic");
                    }
                    // Long enough for a helper to take the work.
                    std::thread::sleep(std::time::Duration::from_millis(5));
                })
            });
            if outcome.is_err() {
                caught = true;
                break;
            }
        }
        assert!(caught, "no helper took the work in 50 calls");
        let items: Vec<usize> = (0..1024).collect();
        let total = AtomicUsize::new(0);
        crate::host::in_parallel(&items, 8, |p| {
            total.fetch_add(p.len(), Ordering::Relaxed);
        });
        assert_eq!(total.load(Ordering::Relaxed), 1024);
    }

    /// A panic in the caller's own share waits for the helpers first, so
    /// the work they run is still alive.
    #[test]
    fn a_panic_in_the_caller_waits_for_the_helpers() {
        let running = AtomicUsize::new(0);
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            run(HELPERS, &|| {
                if on_helper() {
                    running.fetch_add(1, Ordering::SeqCst);
                    std::thread::sleep(std::time::Duration::from_millis(20));
                    running.fetch_sub(1, Ordering::SeqCst);
                } else {
                    std::thread::sleep(std::time::Duration::from_millis(5));
                    panic!("the caller's panic");
                }
            })
        }));
        assert!(outcome.is_err());
        assert_eq!(running.load(Ordering::SeqCst), 0, "returned while a helper still ran");
    }

    /// What a call costs on kept threads and on threads started for it
    /// (`cargo test --release -p victory-gpu --lib parallel -- --ignored --nocapture`).
    #[test]
    #[ignore]
    fn cost_of_a_call() {
        let calls = 2000;
        let work = || std::hint::black_box(());
        let began = std::time::Instant::now();
        for _ in 0..calls {
            std::thread::scope(|s| {
                for _ in 0..HELPERS {
                    s.spawn(work);
                }
                work();
            });
        }
        let started = began.elapsed().as_secs_f64() * 1e6 / calls as f64;
        run(HELPERS, &work);
        let began = std::time::Instant::now();
        for _ in 0..calls {
            run(HELPERS, &work);
        }
        let kept = began.elapsed().as_secs_f64() * 1e6 / calls as f64;
        println!("per call: threads started {started:.1} us, kept threads {kept:.1} us");
    }

    /// No helpers asked for: the work runs once, on the caller.
    #[test]
    fn no_helpers_runs_on_the_caller() {
        let runs = AtomicUsize::new(0);
        run(0, &|| {
            assert!(!on_helper());
            runs.fetch_add(1, Ordering::Relaxed);
        });
        assert_eq!(runs.load(Ordering::Relaxed), 1);
    }
}
