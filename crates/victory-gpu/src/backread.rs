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
//! the access run. Blocks not touched within [`HORIZON`] frames are counted
//! as such and given back. At most [`SLOTS`] blocks are followed at once.
//!
//! What this cannot see: GPU reads of title memory used in place; kernel
//! accesses (a system call touching a guarded page fails once and clears
//! the guard: counted as "lost" when found at expiry); reads through an
//! address of the page the runtime did not tell us of; anything finer than
//! a 64 KiB block (the copy-back and write-watch granularity).

use std::collections::HashSet;
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

struct Slot {
    state: AtomicU8,
    kind: AtomicU8,
    block: AtomicU64,
    aliases: [AtomicU64; ALIASES],
    frame: AtomicU64,
    us: AtomicU64,
}

static SLOT: [Slot; SLOTS] = [const {
    Slot {
        state: AtomicU8::new(FREE),
        kind: AtomicU8::new(0),
        block: AtomicU64::new(0),
        aliases: [const { AtomicU64::new(0) }; ALIASES],
        frame: AtomicU64::new(0),
        us: AtomicU64::new(0),
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

/// `len` bytes at `at` of title memory were just copied back from the GPU.
pub(crate) fn copied(at: usize, len: usize, kind: Kind) {
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
    let n = rate();
    if n == 0 {
        return;
    }
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
    if SLOT.iter().any(|s| s.block.load(Ordering::Acquire) == block && s.state.load(Ordering::Acquire) != FREE) {
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
    slot.us.store(start().elapsed().as_micros() as u64, Ordering::Relaxed);
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
                FIRST[k][who as usize][write].fetch_add(1, Ordering::Relaxed);
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
                {
                    let frames = crate::queue::presented().saturating_sub(s.frame.load(Ordering::Relaxed));
                    let f = match frames {
                        0 => 0,
                        1 => 1,
                        2..=3 => 2,
                        4..=10 => 3,
                        _ => 4,
                    };
                    AFTER_FRAMES[k][who as usize][f].fetch_add(1, Ordering::Relaxed);
                    let us = start().elapsed().as_micros() as u64 - s.us.load(Ordering::Relaxed).min(start().elapsed().as_micros() as u64);
                    let t = match us {
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
                free(s);
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

/// Gives back blocks followed for more than [`HORIZON`] frames (counted as
/// untouched, or lost if their guard is gone).
fn sweep() {
    let now = crate::queue::presented();
    for s in &SLOT {
        if s.state.load(Ordering::Acquire) != ARMED || now.saturating_sub(s.frame.load(Ordering::Relaxed)) <= HORIZON {
            continue;
        }
        if s.state.compare_exchange(ARMED, CLAIMED, Ordering::AcqRel, Ordering::Relaxed).is_err() {
            continue;
        }
        let k = usize::from(s.kind.load(Ordering::Relaxed));
        let mut any = false;
        for b in std::iter::once(s.block.load(Ordering::Acquire))
            .chain(s.aliases.iter().map(|a| a.load(Ordering::Acquire)).filter(|&a| a != 0))
        {
            any |= unguard(b);
        }
        if any {
            EXPIRED[k].fetch_add(1, Ordering::Relaxed);
        } else {
            LOST[k].fetch_add(1, Ordering::Relaxed);
        }
        free(s);
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
    if rate() == 0 {
        if !SAID.swap(true, Ordering::Relaxed) {
            text += "(VICTORY_BACK_READS=<n> follows their first CPU access)";
        }
        return text;
    }
    sweep();
    let pending = SLOT.iter().filter(|s| s.state.load(Ordering::Relaxed) == ARMED).count();
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
        "followed now {pending}; distinct blocks copied back {} followed {}; other addresses [0,1,2-3,4-7,8-15,16+] {:?};          not followed: slots full {}, many views {}, not guardable {}; late faults {}; cost {:.1} ms in {} faults so far",
        distinct(&RUN_BLOCKS),
        distinct(&RUN_FOLLOWED),
        ALIAS_COUNTS.each_ref().map(n),
        n(&SKIPPED[0]),
        n(&SKIPPED[1]),
        n(&SKIPPED[2]),
        n(&LATE),
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

    fn n_ours() -> u64 {
        FIRST[0][Who::Ours as usize][0].load(Ordering::Relaxed)
    }
}
