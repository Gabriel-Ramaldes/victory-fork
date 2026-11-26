//! Diagnostics: is what the GPU wrote, once copied back into title memory,
//! read by the CPU afterwards?
//!
//! Always (diagnostics builds): bytes and calls copied back, and the
//! distinct 64 KiB blocks they touch per report, by origin (a host copy's
//! `copy_back`/`sparse_back`, or a readback the title asked for).
//!
//! With `VICTORY_BACK_READS=<n>`: one block in `n` (chosen by its address,
//! so the same blocks are followed) gets `PAGE_GUARD` on all its pages, at
//! every address the runtime told us it is shown at, right after a copy
//! back. The first access by any CPU code raises a one-time guard
//! exception; our handler notes who touched it (title code, our code, or a
//! system DLL called by either), read or write, and after how many frames
//! and microseconds, takes the guard off the block's other pages, and lets
//! the access run. A title access ends the following; after one of ours
//! (or a write announced to `watch::writing`, which kernel writes need) the
//! guards come back [`REARM_US`] later, so title accesses after ours are
//! seen too. Blocks followed for [`HORIZON`] frames are counted (untouched,
//! or touched by us only) and given back. At most [`SLOTS`] blocks are
//! followed at once.
//!
//! Always too: bytes host copies take again from title memory, split by
//! whether the same copy had just copied the block back (the echo of our
//! own write), another had, or none; with the variable set, sampled echo
//! blocks are compared with the copy first (bytes that differ).
//!
//! What this cannot see: GPU reads of title memory used in place; kernel
//! accesses (a system call touching a guarded page fails once and clears
//! the guard: counted as "lost" when found at expiry); reads through an
//! address of the page the runtime did not tell us of; anything finer than
//! a 64 KiB block (the copy-back and write-watch granularity).

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

use crate::watch::PAGE;

/// Where the bytes copied back came from.
#[derive(Clone, Copy)]
pub(crate) enum Kind {
    /// A host copy of title memory (`copy_back`, `sparse_back`, card back).
    Shadow = 0,
    /// A readback the title recorded (a copy into its memory).
    Readback = 1,
    /// A texture shaders wrote, tiled back (`texback`).
    Texture = 2,
}

/// Variants of [`Kind`].
const KINDS: usize = 3;
const NAMES: [&str; KINDS] = ["copies", "readbacks", "textures"];

/// Blocks followed at once.
const SLOTS: usize = 256;
/// Frames after which a followed block not touched is given back.
const HORIZON: u64 = 30;
/// Alias addresses followed per block (besides its own).
const ALIASES: usize = 15;
/// Frames of a first access by us or a system DLL kept (for its stack).
const DEPTH: usize = 6;
const KERNEL_PAGE: usize = 4096;
const GUARD_VIOLATION: i32 = 0x8000_0001_u32 as i32;

const FREE: u8 = 0;
const ARMING: u8 = 1;
const ARMED: u8 = 2;
const CLAIMED: u8 = 3;
/// Touched by us (or a write announced): guards off, put back by [`rearm_due`].
const PENDING: u8 = 4;
/// Time a block touched by us stays without guards before [`rearm_due`]
/// puts them back (our copy of it has ended by then).
const REARM_US: u64 = 2_000;

struct Slot {
    state: AtomicU8,
    kind: AtomicU8,
    block: AtomicU64,
    aliases: [AtomicU64; ALIASES],
    /// Frame of the first arming (for the horizon).
    frame: AtomicU64,
    /// Frame and time of the last copy back into it.
    last_frame: AtomicU64,
    us: AtomicU64,
    /// Accesses by us (and announced writes) so far, and when the last one
    /// took the guards off.
    touches: AtomicU64,
    pending_us: AtomicU64,
}

static SLOT: [Slot; SLOTS] = [const {
    Slot {
        state: AtomicU8::new(FREE),
        kind: AtomicU8::new(0),
        block: AtomicU64::new(0),
        aliases: [const { AtomicU64::new(0) }; ALIASES],
        frame: AtomicU64::new(0),
        last_frame: AtomicU64::new(0),
        us: AtomicU64::new(0),
        touches: AtomicU64::new(0),
        pending_us: AtomicU64::new(0),
    }
}; SLOTS];

/// Blocks ever guarded (a ring): a late fault on one of them (a guard set
/// while its slot was being given back) is taken, not passed on.
static EVER: [AtomicU64; 1024] = [const { AtomicU64::new(0) }; 1024];
static EVER_AT: AtomicUsize = AtomicUsize::new(0);

/// Copied back since the last report, by [`Kind`]: bytes, calls.
static BYTES: [AtomicU64; KINDS] = [const { AtomicU64::new(0) }; KINDS];
static CALLS: [AtomicU64; KINDS] = [const { AtomicU64::new(0) }; KINDS];
/// Distinct blocks copied back since the last report, by [`Kind`].
static BLOCKS: [Mutex<Option<HashSet<u64>>>; KINDS] = [const { Mutex::new(None) }; KINDS];

/// First accesses by [`Kind`] and [`Who`] x (read, write).
static FIRST: [[[AtomicU64; 2]; 3]; KINDS] =
    [const { [const { [const { AtomicU64::new(0) }; 2] }; 3] }; KINDS];
/// First accesses by [`Kind`], [`Who`] and frames after the copy: same
/// frame, 1, 2-3, 4-10, 11 and more.
static AFTER_FRAMES: [[[AtomicU64; 5]; 3]; KINDS] =
    [const { [const { [const { AtomicU64::new(0) }; 5] }; 3] }; KINDS];
/// First accesses by [`Kind`], [`Who`] and time after the copy: <1 ms,
/// <10 ms, <100 ms, <1 s, 1 s and more.
static AFTER_TIME: [[[AtomicU64; 5]; 3]; KINDS] =
    [const { [const { [const { AtomicU64::new(0) }; 5] }; 3] }; KINDS];
/// Blocks considered for following, by how many other addresses show them:
/// 0, 1, 2-3, 4-7, 8-15, 16 and more.
static ALIAS_COUNTS: [AtomicU64; 6] = [const { AtomicU64::new(0) }; 6];
/// Distinct blocks copied back, and followed, since the start (copies).
static RUN_BLOCKS: Mutex<Option<HashSet<u64>>> = Mutex::new(None);
static RUN_FOLLOWED: Mutex<Option<HashSet<u64>>> = Mutex::new(None);
/// Stacks of first accesses not by the title, with counts.
static STACKS: Mutex<Vec<([u64; DEPTH], u64)>> = Mutex::new(Vec::new());
/// Given back untouched after [`HORIZON`] frames, and found without their
/// guard then (lost), by [`Kind`].
static EXPIRED: [AtomicU64; KINDS] = [const { AtomicU64::new(0) }; KINDS];
static LOST: [AtomicU64; KINDS] = [const { AtomicU64::new(0) }; KINDS];
/// Followed, by [`Kind`].
static ARMED_N: [AtomicU64; KINDS] = [const { AtomicU64::new(0) }; KINDS];
/// Not followed: slots full, more aliases than followed, not guardable.
static SKIPPED: [AtomicU64; 3] = [const { AtomicU64::new(0) }; 3];
/// Title accesses at any time within the horizon, by [`Kind`] x (read,
/// write), and those of them after one of ours.
static TITLE_ANY: [[AtomicU64; 2]; KINDS] = [const { [const { AtomicU64::new(0) }; 2] }; KINDS];
static TITLE_AFTER_OURS: [AtomicU64; KINDS] = [const { AtomicU64::new(0) }; KINDS];
/// Followed to the horizon with accesses by us only (none by the title).
static EXPIRED_OURS: [AtomicU64; KINDS] = [const { AtomicU64::new(0) }; KINDS];
/// Guards put back after an access of ours; guards taken off before a
/// write announced to `watch::writing` (our copies, the title's I/O).
static REARMS: AtomicU64 = AtomicU64::new(0);
static ANNOUNCED: AtomicU64 = AtomicU64::new(0);
static LAST_REARM: AtomicU64 = AtomicU64::new(0);
/// Blocks copied back and not taken again since, with the host copy's
/// address of the block (usize::MAX: none, a readback or a texture).
static BACKED: Mutex<Option<HashMap<u64, usize>>> = Mutex::new(None);
/// Bytes host copies took again from title memory since the last report:
/// blocks the same copy had just copied back (echo), blocks another copy
/// or a readback had (other), the rest.
static RETAKEN: [AtomicU64; 3] = [const { AtomicU64::new(0) }; 3];
/// Echo bytes compared with what the copy holds (sampled blocks), and those
/// that differed.
static ECHO_DIFF: [AtomicU64; 2] = [const { AtomicU64::new(0) }; 2];
/// Faults on blocks no longer followed.
static LATE: AtomicU64 = AtomicU64::new(0);
/// Microseconds spent guarding and in the handler (the measurement's own
/// cost), and handler calls.
static COST_US: AtomicU64 = AtomicU64::new(0);
static FAULTS: AtomicU64 = AtomicU64::new(0);

/// Who touched a followed block first.
#[derive(Clone, Copy)]
enum Who {
    Title = 0,
    Ours = 1,
    System = 2,
}

/// `VICTORY_BACK_READS`: one block in this many is followed; 0 = off.
fn rate() -> u64 {
    static RATE: OnceLock<u64> = OnceLock::new();
    *RATE.get_or_init(|| {
        let n = std::env::var("VICTORY_BACK_READS").ok().and_then(|v| v.trim().parse().ok()).unwrap_or(0);
        if n > 0 {
            tracing::warn!(one_in = n, horizon_frames = HORIZON, "VICTORY_BACK_READS: blocks copied back are followed with guard pages");
        }
        n
    })
}

fn start() -> &'static Instant {
    static START: OnceLock<Instant> = OnceLock::new();
    START.get_or_init(Instant::now)
}

/// `len` bytes at `at` of title memory were just copied back from the GPU;
/// `host` is where they are in the host copy they came from (0 if none).
pub(crate) fn copied(at: usize, len: usize, kind: Kind, host: usize) {
    if len == 0 {
        return;
    }
    let k = kind as usize;
    BYTES[k].fetch_add(len as u64, Ordering::Relaxed);
    CALLS[k].fetch_add(1, Ordering::Relaxed);
    let (first, last) = ((at / PAGE) as u64, (at + len).div_ceil(PAGE) as u64);
    {
        let mut set = BLOCKS[k].lock().unwrap_or_else(|e| e.into_inner());
        let set = set.get_or_insert_with(HashSet::new);
        set.extend(first..last);
    }
    if let Kind::Shadow = kind {
        let mut set = RUN_BLOCKS.lock().unwrap_or_else(|e| e.into_inner());
        set.get_or_insert_with(HashSet::new).extend(first..last);
    }
    {
        let mut backed = BACKED.lock().unwrap_or_else(|e| e.into_inner());
        let backed = backed.get_or_insert_with(HashMap::new);
        for block in first..last {
            let base = if host == 0 { usize::MAX } else { host.wrapping_add(block as usize * PAGE).wrapping_sub(at) };
            backed.insert(block, base);
        }
    }
    let n = rate();
    if n == 0 {
        return;
    }
    rearm_due();
    for block in first..last {
        if (block.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 32) % n == 0 {
            arm(block, kind);
        }
    }
}

/// Guards `block` (and its other addresses) until its first access.
fn arm(block: u64, kind: Kind) {
    let started = Instant::now();
    ranges();
    install();
    if let Some(s) = SLOT.iter().find(|s| s.block.load(Ordering::Acquire) == block && s.state.load(Ordering::Acquire) != FREE) {
        // Followed already: the title's next access is timed from this copy.
        s.last_frame.store(crate::queue::presented(), Ordering::Relaxed);
        s.us.store(start().elapsed().as_micros() as u64, Ordering::Relaxed);
        return;
    }
    let aliases: Vec<u64> = crate::watch::aliases(block as usize * PAGE)
        .into_iter()
        .map(|a| (a / PAGE) as u64)
        .filter(|&a| a != block)
        .collect();
    let bucket = match aliases.len() {
        0 => 0,
        1 => 1,
        2..=3 => 2,
        4..=7 => 3,
        8..=15 => 4,
        _ => 5,
    };
    ALIAS_COUNTS[bucket].fetch_add(1, Ordering::Relaxed);
    if aliases.len() > ALIASES {
        SKIPPED[1].fetch_add(1, Ordering::Relaxed);
        return;
    }
    let Some(slot) = SLOT
        .iter()
        .find(|s| s.state.compare_exchange(FREE, ARMING, Ordering::AcqRel, Ordering::Relaxed).is_ok())
    else {
        SKIPPED[0].fetch_add(1, Ordering::Relaxed);
        return;
    };
    slot.block.store(block, Ordering::Release);
    for (k, a) in slot.aliases.iter().enumerate() {
        a.store(aliases.get(k).copied().unwrap_or(0), Ordering::Release);
    }
    slot.kind.store(kind as u8, Ordering::Relaxed);
    slot.frame.store(crate::queue::presented(), Ordering::Relaxed);
    slot.last_frame.store(crate::queue::presented(), Ordering::Relaxed);
    slot.us.store(start().elapsed().as_micros() as u64, Ordering::Relaxed);
    slot.touches.store(0, Ordering::Relaxed);
    let mut guarded = false;
    for b in std::iter::once(block).chain(aliases.iter().copied()) {
        guarded |= guard(b);
        let i = EVER_AT.fetch_add(1, Ordering::Relaxed) % EVER.len();
        EVER[i].store(b, Ordering::Relaxed);
    }
    if guarded {
        ARMED_N[kind as usize].fetch_add(1, Ordering::Relaxed);
        if let Kind::Shadow = kind {
            let mut set = RUN_FOLLOWED.lock().unwrap_or_else(|e| e.into_inner());
            set.get_or_insert_with(HashSet::new).insert(block);
        }
        // A fault while arming already claimed it: leave it as it is.
        let _ = slot.state.compare_exchange(ARMING, ARMED, Ordering::AcqRel, Ordering::Relaxed);
    } else {
        SKIPPED[2].fetch_add(1, Ordering::Relaxed);
        free(slot);
    }
    COST_US.fetch_add(started.elapsed().as_micros() as u64, Ordering::Relaxed);
}

fn free(slot: &Slot) {
    slot.block.store(0, Ordering::Release);
    for a in &slot.aliases {
        a.store(0, Ordering::Release);
    }
    slot.state.store(FREE, Ordering::Release);
}

/// Adds `PAGE_GUARD` to the committed, accessible pages of `block`; false
/// if none could be.
fn guard(block: u64) -> bool {
    use windows_sys::Win32::System::Memory::{MEM_COMMIT, PAGE_GUARD, PAGE_NOACCESS, VirtualProtect};
    let base = block as usize * PAGE;
    let mut any = false;
    for page in (base..base + PAGE).step_by(KERNEL_PAGE) {
        let Some((state, protect)) = query(page) else { continue };
        if state != MEM_COMMIT || protect == 0 || protect & (PAGE_NOACCESS | PAGE_GUARD) != 0 {
            continue;
        }
        let mut old = 0;
        // SAFETY: only the protection of committed title memory changes.
        any |= unsafe { VirtualProtect(page as *const _, KERNEL_PAGE, protect | PAGE_GUARD, &mut old) } != 0;
    }
    any
}

/// Takes `PAGE_GUARD` off the pages of `block` that still have it; true if
/// any had.
fn unguard(block: u64) -> bool {
    use windows_sys::Win32::System::Memory::{PAGE_GUARD, VirtualProtect};
    let base = block as usize * PAGE;
    let mut any = false;
    for page in (base..base + PAGE).step_by(KERNEL_PAGE) {
        let Some((_, protect)) = query(page) else { continue };
        if protect & PAGE_GUARD == 0 {
            continue;
        }
        any = true;
        let mut old = 0;
        // SAFETY: as in `guard`, the guard we set is taken off.
        unsafe { VirtualProtect(page as *const _, KERNEL_PAGE, protect & !PAGE_GUARD, &mut old) };
    }
    any
}

/// (state, protection) of the page at `at`.
fn query(at: usize) -> Option<(u32, u32)> {
    use windows_sys::Win32::System::Memory::{MEMORY_BASIC_INFORMATION, VirtualQuery};
    let mut info = MEMORY_BASIC_INFORMATION::default();
    // SAFETY: describes an address without touching it.
    let n = unsafe { VirtualQuery(at as *const _, &mut info, size_of_val(&info)) };
    (n != 0).then_some((info.State, info.Protect))
}

/// Code ranges: ours (`victory.exe`) and system DLLs a copy may run in.
struct Ranges {
    ours: (u64, u64),
    system: Vec<(u64, u64)>,
}

fn ranges() -> &'static Ranges {
    static RANGES: OnceLock<Ranges> = OnceLock::new();
    RANGES.get_or_init(|| {
        use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
        use windows_sys::Win32::System::ProcessStatus::{GetModuleInformation, MODULEINFO};
        use windows_sys::Win32::System::Threading::GetCurrentProcess;
        let range = |module: windows_sys::Win32::Foundation::HMODULE| -> Option<(u64, u64)> {
            if module.is_null() {
                return None;
            }
            let mut info = MODULEINFO { lpBaseOfDll: std::ptr::null_mut(), SizeOfImage: 0, EntryPoint: std::ptr::null_mut() };
            // SAFETY: a module of this process, a structure of the size given.
            let ok = unsafe { GetModuleInformation(GetCurrentProcess(), module, &mut info, size_of::<MODULEINFO>() as u32) };
            (ok != 0).then(|| (info.lpBaseOfDll as u64, info.lpBaseOfDll as u64 + u64::from(info.SizeOfImage)))
        };
        // SAFETY: the null name is this executable.
        let ours = range(unsafe { GetModuleHandleW(std::ptr::null()) }).unwrap_or((0, 0));
        let system = ["ntdll.dll", "kernelbase.dll", "kernel32.dll", "vcruntime140.dll", "ucrtbase.dll", "msvcp140.dll"]
            .iter()
            .filter_map(|name| {
                let wide: Vec<u16> = name.encode_utf16().chain([0]).collect();
                // SAFETY: a nul-terminated name; no load, no reference taken.
                range(unsafe { GetModuleHandleW(wide.as_ptr()) })
            })
            .collect();
        Ranges { ours, system }
    })
}

/// The faulting code address and up to [`DEPTH`] - 1 callers.
fn walk(ctx: &windows_sys::Win32::System::Diagnostics::Debug::CONTEXT) -> [u64; DEPTH] {
    use windows_sys::Win32::System::Diagnostics::Debug::{RtlLookupFunctionEntry, RtlVirtualUnwind, UNW_FLAG_NHANDLER};
    let mut frames = [0u64; DEPTH];
    let mut c = *ctx;
    frames[0] = c.Rip;
    for (i, f) in frames.iter_mut().enumerate().skip(1) {
        let mut base = 0u64;
        // SAFETY: looks up the unwind data of a code address.
        let entry = unsafe { RtlLookupFunctionEntry(c.Rip, &mut base, std::ptr::null_mut()) };
        if entry.is_null() {
            if i > 1 {
                break;
            }
            // The faulting function is a leaf (a copy loop): its return
            // address is at the stack pointer.
            // SAFETY: the faulting thread's stack, live while we run on it.
            c.Rip = unsafe { *(c.Rsp as *const u64) };
            c.Rsp += 8;
        } else {
            let mut data = std::ptr::null_mut();
            let mut frame = 0u64;
            // SAFETY: unwinds a copy of the context by the function's own data.
            unsafe { RtlVirtualUnwind(UNW_FLAG_NHANDLER, base, c.Rip, entry, &mut c, &mut data, &mut frame, std::ptr::null_mut()) };
        }
        if c.Rip == 0 {
            break;
        }
        *f = c.Rip;
    }
    frames
}

/// Who runs the first of `frames` that is not in a system DLL.
fn who(frames: &[u64; DEPTH]) -> Who {
    let Some(r) = RANGES_READY.get() else { return Who::System };
    let inside = |pc: u64, (a, b): (u64, u64)| (a..b).contains(&pc);
    for &pc in frames.iter().take_while(|&&pc| pc != 0) {
        if inside(pc, r.ours) {
            return Who::Ours;
        }
        if !r.system.iter().any(|&s| inside(pc, s)) {
            return Who::Title;
        }
    }
    Who::System
}

/// [`ranges`] once made (the handler does not make it).
static RANGES_READY: OnceLock<&'static Ranges> = OnceLock::new();

fn install() {
    use windows_sys::Win32::System::Diagnostics::Debug::{
        AddVectoredExceptionHandler, EXCEPTION_CONTINUE_EXECUTION, EXCEPTION_CONTINUE_SEARCH, EXCEPTION_POINTERS,
    };
    unsafe extern "system" fn on_guard(info: *mut EXCEPTION_POINTERS) -> i32 {
        // SAFETY: the system passes the exception being dispatched.
        let r = unsafe { &*(*info).ExceptionRecord };
        if r.ExceptionCode != GUARD_VIOLATION || r.NumberParameters < 2 {
            return EXCEPTION_CONTINUE_SEARCH;
        }
        let started = Instant::now();
        let block = (r.ExceptionInformation[1] / PAGE) as u64;
        let write = usize::from(r.ExceptionInformation[0] == 1);
        let slot = SLOT.iter().find(|s| {
            let st = s.state.load(Ordering::Acquire);
            (st == ARMED || st == ARMING)
                && (s.block.load(Ordering::Acquire) == block
                    || s.aliases.iter().any(|a| a.load(Ordering::Acquire) == block))
        });
        let taken = match slot {
            Some(s) if [ARMED, ARMING].iter().any(|&st| s.state.compare_exchange(st, CLAIMED, Ordering::AcqRel, Ordering::Relaxed).is_ok()) => {
                // SAFETY: the context of the faulting thread.
                let stack = walk(unsafe { &*(*info).ContextRecord });
                let who = who(&stack);
                let k = usize::from(s.kind.load(Ordering::Relaxed));
                let touches = s.touches.load(Ordering::Relaxed);
                if touches == 0 {
                    FIRST[k][who as usize][write].fetch_add(1, Ordering::Relaxed);
                }
                if !matches!(who, Who::Title)
                    && let Ok(mut stacks) = STACKS.try_lock()
                {
                    // No allocation here: room for 64 was made at install.
                    if let Some(i) = stacks.iter().position(|(st, _)| *st == stack) {
                        stacks[i].1 += 1;
                    } else if stacks.len() < stacks.capacity() {
                        stacks.push((stack, 1));
                    }
                }
                let title = matches!(who, Who::Title);
                if title {
                    TITLE_ANY[k][write].fetch_add(1, Ordering::Relaxed);
                    if touches > 0 {
                        TITLE_AFTER_OURS[k].fetch_add(1, Ordering::Relaxed);
                    }
                }
                // Timed from the last copy back: the first access of all, and
                // every title access.
                if touches == 0 || title {
                    let frames = crate::queue::presented().saturating_sub(s.last_frame.load(Ordering::Relaxed));
                    let f = match frames {
                        0 => 0,
                        1 => 1,
                        2..=3 => 2,
                        4..=10 => 3,
                        _ => 4,
                    };
                    AFTER_FRAMES[k][who as usize][f].fetch_add(1, Ordering::Relaxed);
                    let now = start().elapsed().as_micros() as u64;
                    let t = match now.saturating_sub(s.us.load(Ordering::Relaxed)) {
                        0..1_000 => 0,
                        1_000..10_000 => 1,
                        10_000..100_000 => 2,
                        100_000..1_000_000 => 3,
                        _ => 4,
                    };
                    AFTER_TIME[k][who as usize][t].fetch_add(1, Ordering::Relaxed);
                }
                for b in std::iter::once(s.block.load(Ordering::Acquire))
                    .chain(s.aliases.iter().map(|a| a.load(Ordering::Acquire)).filter(|&a| a != 0))
                {
                    unguard(b);
                }
                if title {
                    free(s);
                } else {
                    // Ours: followed on, its guards put back once our access
                    // has ended.
                    s.touches.fetch_add(1, Ordering::Relaxed);
                    s.pending_us.store(start().elapsed().as_micros() as u64, Ordering::Relaxed);
                    s.state.store(PENDING, Ordering::Release);
                }
                true
            }
            _ => {
                if EVER.iter().any(|e| e.load(Ordering::Relaxed) == block) {
                    unguard(block);
                    LATE.fetch_add(1, Ordering::Relaxed);
                    true
                } else {
                    false
                }
            }
        };
        if !taken {
            return EXCEPTION_CONTINUE_SEARCH;
        }
        FAULTS.fetch_add(1, Ordering::Relaxed);
        COST_US.fetch_add(started.elapsed().as_micros() as u64, Ordering::Relaxed);
        // The system took the guard off the page touched; the access runs.
        EXCEPTION_CONTINUE_EXECUTION
    }
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let _ = RANGES_READY.set(ranges());
        STACKS.lock().unwrap_or_else(|e| e.into_inner()).reserve_exact(64);
        // SAFETY: the handler only reads its own tables and changes guards it set.
        unsafe { AddVectoredExceptionHandler(1, Some(on_guard)) };
    });
}

/// Gives back blocks followed for more than [`HORIZON`] frames: untouched
/// (or lost if their guard is gone), or touched by us only.
fn sweep() {
    let now = crate::queue::presented();
    for s in &SLOT {
        let st = s.state.load(Ordering::Acquire);
        if !(st == ARMED || st == PENDING) || now.saturating_sub(s.frame.load(Ordering::Relaxed)) <= HORIZON {
            continue;
        }
        if s.state.compare_exchange(st, CLAIMED, Ordering::AcqRel, Ordering::Relaxed).is_err() {
            continue;
        }
        let k = usize::from(s.kind.load(Ordering::Relaxed));
        let mut any = false;
        for b in std::iter::once(s.block.load(Ordering::Acquire))
            .chain(s.aliases.iter().map(|a| a.load(Ordering::Acquire)).filter(|&a| a != 0))
        {
            any |= unguard(b);
        }
        if s.touches.load(Ordering::Relaxed) > 0 {
            EXPIRED_OURS[k].fetch_add(1, Ordering::Relaxed);
        } else if any {
            EXPIRED[k].fetch_add(1, Ordering::Relaxed);
        } else {
            LOST[k].fetch_add(1, Ordering::Relaxed);
        }
        free(s);
    }
}

/// Puts the guards back on blocks touched by us at least [`REARM_US`] ago
/// (looked at no more than once a millisecond).
fn rearm_due() {
    let now = start().elapsed().as_micros() as u64;
    let last = LAST_REARM.load(Ordering::Relaxed);
    if now < last + 1_000 || LAST_REARM.compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed).is_err() {
        return;
    }
    for s in &SLOT {
        if s.state.load(Ordering::Acquire) != PENDING || now < s.pending_us.load(Ordering::Relaxed) + REARM_US {
            continue;
        }
        if s.state.compare_exchange(PENDING, ARMING, Ordering::AcqRel, Ordering::Relaxed).is_err() {
            continue;
        }
        let started = Instant::now();
        let mut guarded = false;
        for b in std::iter::once(s.block.load(Ordering::Acquire))
            .chain(s.aliases.iter().map(|a| a.load(Ordering::Acquire)).filter(|&a| a != 0))
        {
            guarded |= guard(b);
        }
        if guarded {
            REARMS.fetch_add(1, Ordering::Relaxed);
            // A fault while arming already claimed it: leave it as it is.
            let _ = s.state.compare_exchange(ARMING, ARMED, Ordering::AcqRel, Ordering::Relaxed);
        } else if s.state.compare_exchange(ARMING, CLAIMED, Ordering::AcqRel, Ordering::Relaxed).is_ok() {
            let k = usize::from(s.kind.load(Ordering::Relaxed));
            EXPIRED_OURS[k].fetch_add(1, Ordering::Relaxed);
            free(s);
        }
        COST_US.fetch_add(started.elapsed().as_micros() as u64, Ordering::Relaxed);
    }
}

/// A write into title memory `[at, at + len)` is announced (`watch::writing`:
/// our copies back, our kernel copies, the title's file and DirectStorage
/// reads): followed blocks there lose their guards first, or a kernel write
/// would fail on them. Counted as a touch by us; the guards come back later.
pub(crate) fn announced(at: usize, len: usize) {
    if rate() == 0 || len == 0 {
        return;
    }
    let (first, last) = ((at / PAGE) as u64, (at + len).div_ceil(PAGE) as u64);
    for s in &SLOT {
        if s.state.load(Ordering::Acquire) != ARMED {
            continue;
        }
        let blocks = || {
            std::iter::once(s.block.load(Ordering::Acquire))
                .chain(s.aliases.iter().map(|a| a.load(Ordering::Acquire)).filter(|&a| a != 0))
        };
        if !blocks().any(|b| (first..last).contains(&b)) {
            continue;
        }
        if s.state.compare_exchange(ARMED, CLAIMED, Ordering::AcqRel, Ordering::Relaxed).is_err() {
            continue;
        }
        for b in blocks() {
            unguard(b);
        }
        ANNOUNCED.fetch_add(1, Ordering::Relaxed);
        s.touches.fetch_add(1, Ordering::Relaxed);
        s.pending_us.store(start().elapsed().as_micros() as u64, Ordering::Relaxed);
        s.state.store(PENDING, Ordering::Release);
    }
}

/// A host copy takes `[at, at + len)` of title memory again, into `host`.
/// Counts the bytes by whether the block was just copied back from that
/// same place (an echo), from elsewhere, or not; for sampled echo blocks
/// (`VICTORY_BACK_READS`), compares title memory with what the copy holds.
pub(crate) fn retaken(at: usize, len: usize, host: *const u8) {
    if len == 0 {
        return;
    }
    let n = rate();
    let (first, last) = ((at / PAGE) as u64, (at + len).div_ceil(PAGE) as u64);
    let mut compare = Vec::new();
    {
        let mut backed = BACKED.lock().unwrap_or_else(|e| e.into_inner());
        let backed = backed.get_or_insert_with(HashMap::new);
        for block in first..last {
            let lo = (block as usize * PAGE).max(at);
            let hi = ((block as usize + 1) * PAGE).min(at + len);
            let bytes = (hi - lo) as u64;
            let here = (host as usize).wrapping_add(block as usize * PAGE).wrapping_sub(at);
            match backed.remove(&block) {
                Some(base) if base == here => {
                    RETAKEN[0].fetch_add(bytes, Ordering::Relaxed);
                    if n > 0 && (block.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 32) % n == 0 {
                        compare.push((lo, hi));
                    }
                }
                Some(_) => {
                    RETAKEN[1].fetch_add(bytes, Ordering::Relaxed);
                }
                None => {
                    RETAKEN[2].fetch_add(bytes, Ordering::Relaxed);
                }
            }
        }
    }
    for (lo, hi) in compare {
        let mut title = vec![0u8; hi - lo];
        if !crate::host::read_into(lo as *const u8, &mut title) {
            continue;
        }
        // SAFETY: the host copy's bytes for `[lo, hi)`, inside `[at, at + len)`.
        let held = unsafe { std::slice::from_raw_parts(host.add(lo - at), hi - lo) };
        let differ = title.iter().zip(held).filter(|(a, b)| a != b).count();
        ECHO_DIFF[0].fetch_add((hi - lo) as u64, Ordering::Relaxed);
        ECHO_DIFF[1].fetch_add(differ as u64, Ordering::Relaxed);
    }
}

/// The periodic line: copied back since the last one, and (when followed)
/// the first accesses so far.
pub(crate) fn report_text() -> String {
    static SAID: AtomicBool = AtomicBool::new(false);
    let mut text = String::new();
    for (k, name) in NAMES.iter().enumerate() {
        let bytes = BYTES[k].swap(0, Ordering::Relaxed);
        let calls = CALLS[k].swap(0, Ordering::Relaxed);
        let blocks = BLOCKS[k].lock().unwrap_or_else(|e| e.into_inner()).take().map_or(0, |s| s.len());
        text += &format!("{name} back since: {:.1} MB in {calls} writes, {blocks} blocks of 64 KiB; ", bytes as f64 / 1e6);
    }
    let mb = |a: &AtomicU64| a.swap(0, Ordering::Relaxed) as f64 / 1e6;
    text += &format!(
        "taken again since: echo {:.1} MB, copied back by another {:.1} MB, other {:.1} MB; ",
        mb(&RETAKEN[0]),
        mb(&RETAKEN[1]),
        mb(&RETAKEN[2]),
    );
    if rate() == 0 {
        if !SAID.swap(true, Ordering::Relaxed) {
            text += "(VICTORY_BACK_READS=<n> follows their first CPU access)";
        }
        return text;
    }
    sweep();
    rearm_due();
    let pending = SLOT.iter().filter(|s| matches!(s.state.load(Ordering::Relaxed), ARMED | PENDING)).count();
    let n = |a: &AtomicU64| a.load(Ordering::Relaxed);
    for (k, name) in NAMES.iter().enumerate() {
        if n(&ARMED_N[k]) == 0 {
            continue;
        }
        let f = |w: usize, rw: usize| n(&FIRST[k][w][rw]);
        text += &format!(
            "{name}: followed {}, first access title read {} write {}, ours read {} write {}, system {};              untouched in {HORIZON} frames {}, lost {}; ",
            n(&ARMED_N[k]),
            f(0, 0),
            f(0, 1),
            f(1, 0),
            f(1, 1),
            f(2, 0) + f(2, 1),
            n(&EXPIRED[k]),
            n(&LOST[k]),
        );
        text += &format!(
            "title any time read {} write {} (after ours {}); to the horizon with ours only {}; ",
            n(&TITLE_ANY[k][0]),
            n(&TITLE_ANY[k][1]),
            n(&TITLE_AFTER_OURS[k]),
            n(&EXPIRED_OURS[k]),
        );
        for (w, who) in ["title", "ours", "system"].iter().enumerate() {
            if f(w, 0) + f(w, 1) == 0 {
                continue;
            }
            text += &format!(
                "{who} after frames [0,1,2-3,4-10,11+] {:?} ms [<1,<10,<100,<1000,more] {:?}; ",
                AFTER_FRAMES[k][w].each_ref().map(n),
                AFTER_TIME[k][w].each_ref().map(n),
            );
        }
    }
    let distinct = |set: &Mutex<Option<HashSet<u64>>>| set.lock().unwrap_or_else(|e| e.into_inner()).as_ref().map_or(0, HashSet::len);
    text += &format!(
        "followed now {pending}; distinct blocks copied back {} followed {}; other addresses [0,1,2-3,4-7,8-15,16+] {:?};          not followed: slots full {}, many views {}, not guardable {}; late faults {}; rearmed {}, announced writes {}; echo compared {:.1} MB, differing {} bytes; \
         cost {:.1} ms in {} faults so far",
        distinct(&RUN_BLOCKS),
        distinct(&RUN_FOLLOWED),
        ALIAS_COUNTS.each_ref().map(n),
        n(&SKIPPED[0]),
        n(&SKIPPED[1]),
        n(&SKIPPED[2]),
        n(&LATE),
        n(&REARMS),
        n(&ANNOUNCED),
        n(&ECHO_DIFF[0]) as f64 / 1e6,
        n(&ECHO_DIFF[1]),
        n(&COST_US) as f64 / 1e3,
        n(&FAULTS),
    );
    text
}

/// The most frequent stacks of first accesses not by the title, as
/// `victory.exe+offset` (named offline against its PDB, so that no symbol
/// engine runs in the process) or absolute addresses; empty when not
/// following.
pub(crate) fn stacks_text() -> String {
    if rate() == 0 {
        return String::new();
    }
    let mut stacks = STACKS.lock().unwrap_or_else(|e| e.into_inner()).clone();
    if stacks.is_empty() {
        return String::new();
    }
    stacks.sort_by(|a, b| b.1.cmp(&a.1));
    let (lo, hi) = ranges().ours;
    let mut text = String::from("first accesses not by the title, by stack:");
    for (stack, count) in stacks.iter().take(6) {
        let names: Vec<String> = stack
            .iter()
            .take_while(|&&pc| pc != 0)
            .map(|&pc| if (lo..hi).contains(&pc) { format!("victory.exe+{:#x}", pc - lo) } else { format!("{pc:#x}") })
            .collect();
        text += &format!("
  {count}x: {}", names.join(" <- "));
    }
    text
}

/// Comparison of copies back, by path (0: sparse copies, 1: other host
/// copies): what [`Tally`] adds up, in [`F`] fields.
static CMP: [[AtomicU64; F]; 2] = [const { [const { AtomicU64::new(0) }; F] }; 2];
/// Frames presented at the last [`compare_text`].
static CMP_FRAMES: AtomicU64 = AtomicU64::new(0);
const F: usize = 25;
const CMP_CALLS: usize = 0;
const TOTAL_NS: usize = 1;
const SAME: usize = 2;
const SAME_BYTES: usize = 3;
const DIFF: usize = 4;
const DIFF_BYTES: usize = 5;
const CMP_NS: usize = 6;
const REST_NS: usize = 7;
const WRITTEN: usize = 8;
const UNREAD: usize = 9;
/// Tiles by size: <4 KiB, <16 KiB, <64 KiB, 64 KiB; identical, then
/// different.
const SAME_SIZE: usize = 10;
const DIFF_SIZE: usize = 14;
/// Different tiles by bytes written back: none, up to 64, 1 KiB, 4 KiB,
/// 16 KiB, less than 64 KiB, 64 KiB.
const SPAN: usize = 18;

/// One copy-back call's comparisons, added up locally (no shared counter
/// per tile) and added to [`CMP`] when dropped.
pub(crate) struct Tally {
    path: usize,
    start: Instant,
    n: [u64; F],
}

impl Tally {
    pub(crate) fn new(path: usize) -> Self {
        Self { path, start: Instant::now(), n: [0; F] }
    }

    fn size(n: usize) -> usize {
        match n {
            0..4096 => 0,
            4096..16384 => 1,
            16384..65536 => 2,
            _ => 3,
        }
    }

    /// A tile of `n` bytes compared identical (the comparison began at `t0`).
    pub(crate) fn same(&mut self, n: usize, t0: Instant) {
        self.n[CMP_NS] += t0.elapsed().as_nanos() as u64;
        self.n[SAME] += 1;
        self.n[SAME_BYTES] += n as u64;
        self.n[SAME_SIZE + Self::size(n)] += 1;
    }

    /// A tile of `n` bytes compared different; returns when the comparison
    /// ended (the rest of its work is timed from there).
    pub(crate) fn differ(&mut self, n: usize, t0: Instant) -> Instant {
        let t1 = Instant::now();
        self.n[CMP_NS] += (t1 - t0).as_nanos() as u64;
        self.n[DIFF] += 1;
        self.n[DIFF_BYTES] += n as u64;
        self.n[DIFF_SIZE + Self::size(n)] += 1;
        t1
    }

    /// The different tile's title memory could not be read.
    pub(crate) fn unread(&mut self, t1: Instant) {
        self.n[REST_NS] += t1.elapsed().as_nanos() as u64;
        self.n[UNREAD] += 1;
    }

    /// The different tile was read, merged and `written` bytes written back.
    pub(crate) fn rest(&mut self, t1: Instant, written: usize) {
        self.n[REST_NS] += t1.elapsed().as_nanos() as u64;
        self.n[WRITTEN] += written as u64;
        let span = match written {
            0 => 0,
            1..=64 => 1,
            65..=1024 => 2,
            1025..=4096 => 3,
            4097..=16384 => 4,
            16385..65536 => 5,
            _ => 6,
        };
        self.n[SPAN + span] += 1;
    }
}

impl Drop for Tally {
    fn drop(&mut self) {
        self.n[CMP_CALLS] = 1;
        self.n[TOTAL_NS] = self.start.elapsed().as_nanos() as u64;
        for (total, &n) in CMP[self.path].iter().zip(&self.n) {
            if n != 0 {
                total.fetch_add(n, Ordering::Relaxed);
            }
        }
    }
}

/// The comparison of copies back since the last call, per frame.
pub(crate) fn compare_text() -> String {
    let now = crate::queue::presented();
    let frames = now.saturating_sub(CMP_FRAMES.swap(now, Ordering::Relaxed)).max(1) as f64;
    let mut text = String::from("copy-back compare per frame:");
    for (path, name) in ["sparse", "other copies"].iter().enumerate() {
        let n: [u64; F] = CMP[path].each_ref().map(|a| a.swap(0, Ordering::Relaxed));
        if n[CMP_CALLS] == 0 {
            continue;
        }
        let tiles = (n[SAME] + n[DIFF]).max(1) as f64;
        let mb = |b: u64| b as f64 / 1e6 / frames;
        let ms = |ns: u64| ns as f64 / 1e6 / frames;
        text += &format!(
            " {name}: {:.0} calls, {:.0} tiles, {:.1} MB compared; identical {:.0} tiles ({:.1}%, {:.1} MB), \
             different {:.0} tiles ({:.1}%, {:.1} MB: an upper bound of the bytes changed); written back {:.1} MB, \
             unread {}; tiles [<4K,<16K,<64K,64K] identical {:?} different {:?}; \
             written per different tile [0,<=64,<=1K,<=4K,<=16K,<64K,64K] {:?}; \
             thread ms: calls {:.1}, compare {:.1}, read+merge+write {:.1};",
            n[CMP_CALLS] as f64 / frames,
            tiles / frames,
            mb(n[SAME_BYTES] + n[DIFF_BYTES]),
            n[SAME] as f64 / frames,
            100.0 * n[SAME] as f64 / tiles,
            mb(n[SAME_BYTES]),
            n[DIFF] as f64 / frames,
            100.0 * n[DIFF] as f64 / tiles,
            mb(n[DIFF_BYTES]),
            mb(n[WRITTEN]),
            n[UNREAD],
            [0, 1, 2, 3].map(|i| (n[SAME_SIZE + i] as f64 / frames).round() as u64),
            [0, 1, 2, 3].map(|i| (n[DIFF_SIZE + i] as f64 / frames).round() as u64),
            [0, 1, 2, 3, 4, 5, 6].map(|i| (n[SPAN + i] as f64 / frames).round() as u64),
            ms(n[TOTAL_NS]),
            ms(n[CMP_NS]),
            ms(n[REST_NS]),
        );
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A guarded block of our own memory: a read by this code (counted as
    /// ours) clears it and runs; the data is intact.
    #[test]
    fn a_guarded_block_is_given_back_at_its_first_access() {
        use windows_sys::Win32::System::Memory::{MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_GUARD, PAGE_READWRITE, VirtualAlloc, VirtualFree};
        // SAFETY: a private block of our own, freed at the end.
        let base = unsafe { VirtualAlloc(std::ptr::null(), 2 * PAGE, MEM_RESERVE | MEM_COMMIT, PAGE_READWRITE) } as usize;
        assert!(base != 0);
        let at = base.next_multiple_of(PAGE);
        let at = if at + PAGE <= base + 2 * PAGE { at } else { base };
        // SAFETY: committed memory of ours.
        unsafe { (at as *mut u32).write_volatile(0x1234_5678) };
        ranges();
        install();
        let block = (at / PAGE) as u64;
        let before = n_ours();
        arm(block, Kind::Shadow);
        assert!(query(at).is_some_and(|(_, p)| p & PAGE_GUARD != 0));
        // SAFETY: as above; the guard fault is taken by our handler.
        let v = unsafe { (at as *const u32).read_volatile() };
        assert_eq!(v, 0x1234_5678);
        assert_eq!(n_ours(), before + 1);
        // The whole block is given back.
        assert!((at..at + PAGE).step_by(KERNEL_PAGE).all(|p| query(p).is_some_and(|(_, pr)| pr & PAGE_GUARD == 0)));
        // SAFETY: our allocation.
        unsafe { VirtualFree(base as *mut _, 0, MEM_RELEASE) };
    }

    /// Taken again into the copy it came back from: an echo, once; from
    /// another place: copied back by another; never copied back: other.
    /// (Addresses only: nothing is read without `VICTORY_BACK_READS`.)
    #[test]
    fn bytes_taken_again_are_told_apart_by_where_they_came_back_from() {
        let at = 0x7123_4500_0000usize + 0x100;
        let host = 0x5000_0000usize;
        let get = |i: usize| RETAKEN[i].load(Ordering::Relaxed);
        let before = [get(0), get(1), get(2)];
        copied(at, 0x200, Kind::Shadow, host);
        retaken(at, 0x200, host as *const u8);
        assert!(get(0) >= before[0] + 0x200);
        // Its echo is counted once.
        retaken(at, 0x200, host as *const u8);
        assert!(get(2) >= before[2] + 0x200);
        copied(at + PAGE, 0x40, Kind::Shadow, host);
        retaken(at + PAGE, 0x40, (host + 0x1000) as *const u8);
        assert!(get(1) >= before[1] + 0x40);
    }

    /// A call's comparisons reach the shared counters once, when dropped.
    #[test]
    fn a_tally_adds_up_its_tiles_when_dropped() {
        let get = |i: usize| CMP[0][i].load(Ordering::Relaxed);
        let before = [get(SAME), get(DIFF), get(WRITTEN), get(SAME_SIZE + 3), get(DIFF_SIZE), get(SPAN + 2)];
        {
            let mut tally = Tally::new(0);
            tally.same(PAGE, Instant::now());
            let t1 = tally.differ(100, Instant::now());
            tally.rest(t1, 80);
            assert_eq!(get(SAME), before[0]);
        }
        assert!(get(SAME) > before[0] && get(DIFF) > before[1]);
        assert!(get(WRITTEN) >= before[2] + 80);
        assert!(get(SAME_SIZE + 3) > before[3] && get(DIFF_SIZE) > before[4] && get(SPAN + 2) > before[5]);
    }

    fn n_ours() -> u64 {
        FIRST[0][Who::Ours as usize][0].load(Ordering::Relaxed)
    }
}
