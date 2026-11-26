//! Diagnostics experiment (`VICTORY_BACK_COMPARE=<n>`, off by default):
//! `Shadow::sparse_back` compares its tiles (the host copy's bytes with
//! their snapshot) on `n` kept worker threads and its own, before its
//! serial loop, which then takes each tile's result in order instead of
//! comparing it there. Only the comparison moves: reading title memory,
//! merging, `watch::writing` and writing stay on the calling thread, in
//! the same order, as do the tiles whose comparisons are found equal (they
//! are left alone either way).
//!
//! Why the results are the serial ones: every tile has memory and a
//! snapshot of its own, and handling one tile writes only its own (the
//! merge syncs that tile's copy and snapshot), so a tile compares the same
//! before the loop as at its turn in it. The caller holds the copy's
//! `before` lock (which every writer of the snapshots takes) from before
//! the tiles are listed until after the loop, and the card has finished
//! with the copy before a copy back starts.
//!
//! Lifetimes: work is given out as pieces of about [`PIECE`] bytes of one
//! [`Batch`], which an `Arc` keeps; the caller does pieces too and returns
//! only once every piece taken is done, so no worker reads a tile after
//! that. A worker that finds the batch later takes no piece. Workers take
//! no lock of the copies, only the queue's (the caller takes it under
//! `before`; nothing takes them the other way round). A panic in a piece
//! is caught, counted, and that call compares again on its own thread.
//! Workers live as long as the process.

use std::collections::VecDeque;
use std::ops::Range;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::Instant;

/// About this many bytes of tiles to a piece of work.
const PIECE: usize = 1 << 20;
/// Calls with fewer bytes than this compare on their own thread.
const LEAST: usize = 2 * PIECE;

/// `VICTORY_BACK_COMPARE`: kept workers that compare copy-back tiles; 0
/// (the default) leaves the comparison where it was.
pub(crate) fn workers() -> usize {
    static N: OnceLock<usize> = OnceLock::new();
    *N.get_or_init(|| {
        let n = asked(std::env::var("VICTORY_BACK_COMPARE").ok().as_deref());
        if n > 0 {
            tracing::warn!(workers = n, "VICTORY_BACK_COMPARE: copy-back tiles compared on kept threads (experiment)");
        }
        n
    })
}

fn asked(v: Option<&str>) -> usize {
    v.and_then(|v| v.trim().parse::<usize>().ok()).unwrap_or(0).min(8)
}

/// One call's tiles: (copy's bytes, snapshot's bytes, length), the pieces
/// they are cut in, and what was found.
struct Batch {
    tiles: Vec<(usize, usize, usize)>,
    pieces: Vec<Range<usize>>,
    next: AtomicUsize,
    done: AtomicUsize,
    differ: Vec<AtomicBool>,
    failed: AtomicBool,
}

impl Batch {
    /// Takes pieces until none is left; how many it did.
    fn work(&self) -> u64 {
        let mut did = 0;
        loop {
            let p = self.next.fetch_add(1, Ordering::AcqRel);
            if p >= self.pieces.len() {
                return did;
            }
            let compared = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                for t in self.pieces[p].clone() {
                    let (now, old, n) = self.tiles[t];
                    // SAFETY: both stretches can be read and nothing writes
                    // them until every piece is done (module notes).
                    let (a, b) =
                        unsafe { (std::slice::from_raw_parts(now as *const u8, n), std::slice::from_raw_parts(old as *const u8, n)) };
                    self.differ[t].store(a != b, Ordering::Relaxed);
                }
            }));
            if compared.is_err() {
                self.failed.store(true, Ordering::Relaxed);
            }
            self.done.fetch_add(1, Ordering::Release);
            did += 1;
        }
    }

    fn open(&self) -> bool {
        self.next.load(Ordering::Relaxed) < self.pieces.len()
    }
}

static QUEUE: Mutex<VecDeque<Arc<Batch>>> = Mutex::new(VecDeque::new());
static MORE: Condvar = Condvar::new();

/// Since the last [`text`]: calls on the workers, calls on their own
/// thread (small), pieces done by callers and by workers, nanoseconds of
/// the calls on the workers and of their callers waiting on pieces taken
/// by workers, and pieces that panicked.
static STATS: [AtomicU64; 7] = [const { AtomicU64::new(0) }; 7];
const POOLED: usize = 0;
const SMALL: usize = 1;
const BY_CALLER: usize = 2;
const BY_WORKERS: usize = 3;
const CALL_NS: usize = 4;
const WAIT_NS: usize = 5;
const PANICKED: usize = 6;

fn start() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        for i in 0..workers() {
            let spawned = std::thread::Builder::new().name(format!("victory-back-compare-{i}")).spawn(serve);
            if let Err(e) = spawned {
                // The callers do every piece themselves then.
                tracing::warn!(%e, "a copy-back compare worker could not start");
            }
        }
    });
}

fn serve() {
    loop {
        let batch = {
            let mut queue = QUEUE.lock().unwrap_or_else(|e| e.into_inner());
            loop {
                // Batches with every piece taken wait for their callers to
                // take them out.
                if let Some(b) = queue.iter().find(|b| b.open()) {
                    break b.clone();
                }
                queue = MORE.wait(queue).unwrap_or_else(|e| e.into_inner());
            }
        };
        STATS[BY_WORKERS].fetch_add(batch.work(), Ordering::Relaxed);
    }
}

fn serial(tiles: &[(usize, usize, usize)]) -> Vec<bool> {
    tiles
        .iter()
        .map(|&(now, old, n)| {
            // SAFETY: the caller's (see `differ`).
            let (a, b) = unsafe { (std::slice::from_raw_parts(now as *const u8, n), std::slice::from_raw_parts(old as *const u8, n)) };
            a != b
        })
        .collect()
}

/// Whether each of `tiles` (a host copy's bytes, its snapshot's, their
/// length) differs, in order: compared on the workers and this thread, or
/// on this one alone when [`workers`] is 0 or there is little.
///
/// # Safety
/// Both stretches of every tile can be read, and nothing writes them, until
/// this returns.
pub(crate) unsafe fn differ(tiles: Vec<(usize, usize, usize)>) -> Vec<bool> {
    let bytes: usize = tiles.iter().map(|t| t.2).sum();
    if workers() == 0 || bytes < LEAST {
        STATS[SMALL].fetch_add(1, Ordering::Relaxed);
        return serial(&tiles);
    }
    start();
    let began = Instant::now();
    // SAFETY: the caller's.
    let found = unsafe { pooled(tiles, PIECE) };
    STATS[POOLED].fetch_add(1, Ordering::Relaxed);
    STATS[CALL_NS].fetch_add(began.elapsed().as_nanos() as u64, Ordering::Relaxed);
    found
}

/// [`differ`] on the workers and this thread, in pieces of about `piece`
/// bytes (at least one tile each).
///
/// # Safety
/// As [`differ`]'s.
unsafe fn pooled(tiles: Vec<(usize, usize, usize)>, piece: usize) -> Vec<bool> {
    let mut pieces = Vec::new();
    let (mut from, mut taken) = (0, 0);
    for (i, t) in tiles.iter().enumerate() {
        if taken >= piece {
            pieces.push(from..i);
            (from, taken) = (i, 0);
        }
        taken += t.2;
    }
    if from < tiles.len() {
        pieces.push(from..tiles.len());
    }
    let differ = (0..tiles.len()).map(|_| AtomicBool::new(false)).collect();
    let batch = Arc::new(Batch {
        tiles,
        pieces,
        next: AtomicUsize::new(0),
        done: AtomicUsize::new(0),
        differ,
        failed: AtomicBool::new(false),
    });
    QUEUE.lock().unwrap_or_else(|e| e.into_inner()).push_back(batch.clone());
    MORE.notify_all();
    STATS[BY_CALLER].fetch_add(batch.work(), Ordering::Relaxed);
    // Pieces workers took: each a tenth of a millisecond or so.
    let waiting = Instant::now();
    let mut spins = 0u32;
    while batch.done.load(Ordering::Acquire) < batch.pieces.len() {
        spins = spins.wrapping_add(1);
        if spins % 64 == 0 {
            std::thread::yield_now();
        } else {
            std::hint::spin_loop();
        }
    }
    STATS[WAIT_NS].fetch_add(waiting.elapsed().as_nanos() as u64, Ordering::Relaxed);
    QUEUE.lock().unwrap_or_else(|e| e.into_inner()).retain(|b| !Arc::ptr_eq(b, &batch));
    if batch.failed.load(Ordering::Relaxed) {
        STATS[PANICKED].fetch_add(1, Ordering::Relaxed);
        return serial(&batch.tiles);
    }
    batch.differ.iter().map(|d| d.load(Ordering::Relaxed)).collect()
}

/// The workers' line since the last one, per frame; empty when off.
pub(crate) fn text() -> String {
    if workers() == 0 {
        return String::new();
    }
    static FRAMES: AtomicU64 = AtomicU64::new(0);
    let now = crate::queue::presented();
    let frames = now.saturating_sub(FRAMES.swap(now, Ordering::Relaxed)).max(1) as f64;
    let n: [u64; 7] = STATS.each_ref().map(|a| a.swap(0, Ordering::Relaxed));
    format!(
        "copy-back compare on {} workers, per frame: {:.0} calls on them ({:.1} ms, of it {:.1} ms waiting for theirs), \
         {:.0} on their own thread; pieces by callers {:.0}, by workers {:.0}; panicked {}",
        workers(),
        n[POOLED] as f64 / frames,
        n[CALL_NS] as f64 / 1e6 / frames,
        n[WAIT_NS] as f64 / 1e6 / frames,
        n[SMALL] as f64 / frames,
        n[BY_CALLER] as f64 / frames,
        n[BY_WORKERS] as f64 / frames,
        n[PANICKED],
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A little xorshift, so the test needs nothing more.
    fn bytes(seed: &mut u64, n: usize) -> Vec<u8> {
        (0..n)
            .map(|_| {
                *seed ^= *seed << 13;
                *seed ^= *seed >> 7;
                *seed ^= *seed << 17;
                *seed as u8
            })
            .collect()
    }

    /// Workers or not, every tile's answer is the serial comparison's:
    /// whole and partial tiles, a change at the first byte, the last, the
    /// middle, or none, in batches above and below the small limit.
    #[test]
    fn the_answers_are_the_serial_ones() {
        let mut seed = 0x9E37_79B9_7F4A_7C15u64;
        for round in 0..6 {
            let count = [3, 17, 40, 90, 33, 64][round];
            let mut now = Vec::new();
            let mut old = Vec::new();
            for t in 0..count {
                let n = match t % 5 {
                    0 => 65536,
                    1 => 65536 - 4096 * (t % 7 + 1),
                    2 => 3 + t * 13,
                    _ => 65536,
                };
                let a = bytes(&mut seed, n);
                let mut b = a.clone();
                match (t + round) % 4 {
                    0 => {}
                    1 => b[0] ^= 1,
                    2 => b[n - 1] ^= 0x80,
                    _ => b[n / 2] = b[n / 2].wrapping_add(1),
                }
                now.push(a);
                old.push(b);
            }
            let tiles: Vec<(usize, usize, usize)> =
                now.iter().zip(&old).map(|(a, b)| (a.as_ptr() as usize, b.as_ptr() as usize, a.len())).collect();
            let want = serial(&tiles);
            // SAFETY: the vectors live and are not written until it returns.
            let got = unsafe { differ(tiles.clone()) };
            assert_eq!(got, want, "round {round}");
            // On kept workers, whatever `workers()` says here, one tile
            // to a piece so that they take some.
            start_for_test(2);
            // SAFETY: as above.
            let pooled = unsafe { pooled(tiles, 1) };
            assert_eq!(pooled, want, "round {round}, on workers");
        }
        assert_eq!(STATS[PANICKED].load(Ordering::Relaxed), 0);
    }

    /// The setting is a number of workers, at most 8; anything else is 0.
    #[test]
    fn the_setting_is_a_count() {
        assert_eq!(asked(None), 0);
        assert_eq!(asked(Some("")), 0);
        assert_eq!(asked(Some("x")), 0);
        assert_eq!(asked(Some("2")), 2);
        assert_eq!(asked(Some(" 4 ")), 4);
        assert_eq!(asked(Some("99")), 8);
    }

    fn start_for_test(n: usize) {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| {
            for i in 0..n {
                std::thread::Builder::new().name(format!("test-compare-{i}")).spawn(serve).unwrap();
            }
        });
    }
}
