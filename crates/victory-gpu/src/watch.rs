//! Which title memory the title wrote since a host copy took it (decision
//! 0007, "Copies of title memory"). The runtime reserves title GPU memory
//! with write watching (`victory-hle` xmem), so the kernel keeps a dirty bit
//! per 4 KiB page; asking for a range's written pages also clears them. Host
//! copies of the same memory can overlap (nested buffers, textures in
//! buffers), so what one asks is kept here for the others: a journal of
//! each page's last write, numbered by the asking.
//!
//! Physical-page views (xmem's sections) cannot be write-watched. There a
//! copy leaves each 64 KiB block it took read-only, and our vectored handler
//! takes the first write to it: it notes the block as written and gives the
//! write access back. The runtime tells us when it maps or unmaps views
//! ([`title_memory_remapped`]), which resets their protection.
//!
//! The title shows one physical page at several addresses. A write of its
//! own through any of them is seen: the runtime tells us the views it maps
//! ([`title_memory_view`]), guarding a block makes every other view of its
//! page read-only too, and a fault at one of them opens them all. Without
//! this a copy taken at one address stayed as it was for the rest of the
//! run when the title wrote through another (parts of moving vehicles left
//! behind and stretched across the screen, 2026-11-25). The idea is
//! victory-fork #115's (Spexivus, joroyt) as carried by victory-amd; the
//! code here is written anew. `VICTORY_VIEW_GUARD=0` turns it off.
//!
//! shortcut: the runtime's own writes into physical pages through a view of
//! its own (xmem's `write_physical`, DirectStorage's page destinations) are
//! told by address ([`title_memory_written`]), not through this. A page
//! shown at more than [`MANY_VIEWS`] addresses is guarded one view at a
//! time. A system call writing into a guarded block fails (none is known
//! to).

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

/// Per-64-MiB-region epoch table. Incremented for only the region(s) touched
/// whenever a guarded block is opened (VEH guard fault or `writing`),
/// remapped (`title_memory_remapped`), or rewritten (`title_memory_written`).
/// While a span's region epoch stays unchanged, any range already guarded and
/// copied at that epoch cannot have been modified by the title — and
/// `copy_back` on one buffer no longer invalidates clean spans in other regions.
const REGION_SHIFT: usize = 26; // 64 MiB per bucket
const REGION_BUCKETS: usize = 1024;
static REGION_EPOCHS: [AtomicU64; REGION_BUCKETS] =
    [const { AtomicU64::new(1) }; REGION_BUCKETS];

/// Diagnostics (`VICTORY_COPY_COUNTS=1`): epochs as [`REGION_EPOCHS`], by
/// 1 MiB instead of 64 MiB, to measure what a finer epoch would let
/// `clean_spans` skip (`shadow::refresh_counted`).
#[cfg(feature = "diagnostics")]
const FINE_SHIFT: usize = 20;
#[cfg(feature = "diagnostics")]
const FINE_BUCKETS: usize = 1 << 16;
#[cfg(feature = "diagnostics")]
static FINE_EPOCHS: [AtomicU64; FINE_BUCKETS] = [const { AtomicU64::new(1) }; FINE_BUCKETS];

/// [`region_epoch`] by 1 MiB (diagnostics).
#[cfg(feature = "diagnostics")]
pub(crate) fn fine_epoch(start: usize, end: usize) -> u64 {
    if start >= end {
        return 0;
    }
    let (first, last) = (start >> FINE_SHIFT, (end - 1) >> FINE_SHIFT);
    if last - first + 1 >= FINE_BUCKETS {
        return u64::MAX;
    }
    (first..=last).fold(0u64, |hash, r| {
        hash.wrapping_mul(0x9e37_79b9_7f4a_7c15)
            .wrapping_add(FINE_EPOCHS[r & (FINE_BUCKETS - 1)].load(Ordering::Acquire))
    })
}

/// Diagnostics (`VICTORY_COPY_DIFFERS=1`): why each page was last noted
/// written, by page: [`ORIGIN_PIN`] and the others.
#[cfg(feature = "diagnostics")]
static ORIGINS: std::sync::Mutex<Option<std::collections::HashMap<usize, u8>>> = std::sync::Mutex::new(None);

/// A pin held it open (our copies, the kernel, a GPU window).
#[cfg(feature = "diagnostics")]
pub(crate) const ORIGIN_PIN: u8 = 1;
/// Read-only but not clean: something other than a fault said written.
#[cfg(feature = "diagnostics")]
pub(crate) const ORIGIN_UNCLEAN: u8 = 2;
/// Found open: a write fault (or an opening) since it was guarded.
#[cfg(feature = "diagnostics")]
pub(crate) const ORIGIN_OPEN: u8 = 3;
/// The host GPU wrote it in place (`gpu_wrote`, the whole declared range).
#[cfg(feature = "diagnostics")]
pub(crate) const ORIGIN_GPU: u8 = 4;
/// The runtime wrote a streamed file into it.
#[cfg(feature = "diagnostics")]
pub(crate) const ORIGIN_STREAM: u8 = 5;

#[cfg(feature = "diagnostics")]
fn note_origin(first: usize, last: usize, origin: u8) {
    if !crate::backread::counts() || std::env::var_os("VICTORY_COPY_DIFFERS").is_none() {
        return;
    }
    let mut o = ORIGINS.lock().unwrap_or_else(|e| e.into_inner());
    let o = o.get_or_insert_with(Default::default);
    for page in first..last {
        if origin == ORIGIN_UNCLEAN && o.get(&page) == Some(&ORIGIN_GPU) {
            continue;
        }
        o.insert(page, origin);
    }
}

/// Why `page` was last noted written (0: not known).
#[cfg(feature = "diagnostics")]
pub(crate) fn origin_of(page: usize) -> u8 {
    ORIGINS.lock().unwrap_or_else(|e| e.into_inner()).as_ref().and_then(|o| o.get(&page).copied()).unwrap_or(0)
}

#[inline]
fn bump_region_epochs(start: usize, end: usize) {
    if start >= end {
        return;
    }
    #[cfg(feature = "diagnostics")]
    if crate::backread::counts() {
        let (first, last) = (start >> FINE_SHIFT, (end - 1) >> FINE_SHIFT);
        if last - first + 1 >= FINE_BUCKETS {
            FINE_EPOCHS.iter().for_each(|slot| {
                slot.fetch_add(1, Ordering::Release);
            });
        } else {
            for r in first..=last {
                FINE_EPOCHS[r & (FINE_BUCKETS - 1)].fetch_add(1, Ordering::Release);
            }
        }
    }
    let first = start >> REGION_SHIFT;
    let last = (end - 1) >> REGION_SHIFT;
    let count = last - first + 1;
    if count >= REGION_BUCKETS {
        for slot in &REGION_EPOCHS {
            slot.fetch_add(1, Ordering::Release);
        }
    } else {
        for r in first..=last {
            REGION_EPOCHS[r & (REGION_BUCKETS - 1)].fetch_add(1, Ordering::Release);
        }
    }
}

#[inline]
pub(crate) fn region_epoch(start: usize, end: usize) -> u64 {
    if start >= end {
        return 0;
    }
    let first = start >> REGION_SHIFT;
    let last = (end - 1) >> REGION_SHIFT;
    let count = last - first + 1;
    let mut hash = 0u64;
    if count >= REGION_BUCKETS {
        for slot in &REGION_EPOCHS {
            hash = hash
                .wrapping_mul(0x9e37_79b9_7f4a_7c15)
                .wrapping_add(slot.load(Ordering::Acquire));
        }
    } else {
        for r in first..=last {
            hash = hash
                .wrapping_mul(0x9e37_79b9_7f4a_7c15)
                .wrapping_add(REGION_EPOCHS[r & (REGION_BUCKETS - 1)].load(Ordering::Acquire));
        }
    }
    hash
}

/// The unit tracked and copied: 64 KiB, the console's page (the kernel's
/// 4 KiB pages fold into it). Finer units made the checks of the world's
/// big buffers cost more than the copies they saved.
pub(crate) const PAGE: usize = 64 << 10;
/// The kernel's page, the unit of its answers.
const KERNEL_PAGE: usize = 4096;

struct Journal {
    /// Numbers the askings; 0 is "never".
    asked: u64,
    /// Page number → the asking that last found it written.
    written: BTreeMap<usize, u64>,
    /// Room for the kernel's answer.
    found: Vec<usize>,
}

static JOURNAL: Mutex<Journal> = Mutex::new(Journal {
    asked: 0,
    written: BTreeMap::new(),
    found: Vec::new(),
});

/// The pages of `[start, end)` (whole pages) as last written, for pages
/// written since tracking began: `(page number, asking)` in page order, and
/// this asking's number, which is higher than any in the list but those the
/// kernel just reported. `None` if the memory is not write-watched (nor,
/// with `guarded`, view memory, which is then guarded).
pub(crate) fn writes(start: usize, end: usize, guarded: bool) -> Option<(u64, Vec<(usize, u64)>)> {
    use windows_sys::Win32::System::Memory::GetWriteWatch;
    const WRITE_WATCH_FLAG_RESET: u32 = 1;
    let (first, last) = (start / PAGE, end.div_ceil(PAGE));
    let mut j = JOURNAL.lock().unwrap_or_else(|e| e.into_inner());
    let j = &mut *j;
    let mut count = (last - first) * PAGE / KERNEL_PAGE;
    j.found.resize(count.max(j.found.len()), 0);
    let mut granularity = 0;
    #[cfg(feature = "diagnostics")]
    let started = std::time::Instant::now();
    // SAFETY: the answer buffer holds `count` entries.
    let failed = unsafe {
        GetWriteWatch(
            WRITE_WATCH_FLAG_RESET,
            (first * PAGE) as *const _,
            (last - first) * PAGE,
            j.found.as_mut_ptr().cast(),
            &mut count,
            &mut granularity,
        )
    } != 0;
    let guarded = if failed {
        count = 0;
        if !guarded {
            return None;
        }
        guard(first * PAGE, last * PAGE)?
    } else {
        Vec::new()
    };
    #[cfg(feature = "diagnostics")]
    {
        use crate::copystat::{C, add};
        add(C::Watches, 1);
        add(C::WatchUs, started.elapsed().as_micros() as u64);
    }
    j.asked += 1;
    let now = j.asked;
    let pages = j.found[..count].iter().map(|&at| at / PAGE).chain(guarded);
    for page in pages.collect::<Vec<_>>() {
        j.written.insert(page, now);
    }
    Some((
        now,
        j.written
            .range(first..last)
            .map(|(&p, &w)| (p, w))
            .collect(),
    ))
}

/// Same as [`writes`] for memory already known to be guarded `MEM_MAPPED`
/// view memory: skips the `GetWriteWatch` syscall (which always fails on
/// mapped views).
pub(crate) fn writes_guarded(start: usize, end: usize) -> Option<(u64, Vec<(usize, u64)>)> {
    let (first, last) = (start / PAGE, end.div_ceil(PAGE));
    #[cfg(feature = "diagnostics")]
    let started = std::time::Instant::now();
    let guarded = guard(first * PAGE, last * PAGE)?;
    let mut j = JOURNAL.lock().unwrap_or_else(|e| e.into_inner());
    let j = &mut *j;
    #[cfg(feature = "diagnostics")]
    {
        use crate::copystat::{C, add};
        add(C::Watches, 1);
        add(C::WatchUs, started.elapsed().as_micros() as u64);
    }
    j.asked += 1;
    let now = j.asked;
    for page in guarded {
        j.written.insert(page, now);
    }
    Some((
        now,
        j.written
            .range(first..last)
            .map(|(&p, &w)| (p, w))
            .collect(),
    ))
}

/// Guarded blocks of view memory.
const BLOCK: usize = PAGE;

struct Guards {
    /// Blocks we made read-only.
    protected: BTreeSet<usize>,
    /// Of those, the ones not written since a copy took them.
    clean: BTreeSet<usize>,
    /// Unmapped blocks (`VirtualProtect` failed) within guarded ranges;
    /// cleared by [`title_memory_remapped`] when the runtime maps them.
    holes: BTreeSet<usize>,
    /// Blocks whose page was guarded together with its other views (those
    /// views' blocks are in it too): a write to one opens them all.
    aliased: BTreeSet<usize>,
    /// Blocks something is about to write without faulting into our
    /// handler (the kernel, our own copies, the host GPU), from, to, and
    /// until when they are left writable. Until then they are not made
    /// read-only, and every check reports them written.
    pins: Vec<(usize, usize, Until)>,
}

/// Until when a pin holds ([`Guards::pins`]).
#[derive(Clone, Copy, Debug, PartialEq)]
enum Until {
    /// A time ([`writing`]: windows of the host GPU on title memory).
    Time(std::time::Instant),
    /// As long as a [`Writing`] lives: a write that ends when its caller
    /// is done (our copies, synchronous reads of the kernel).
    Held(u64),
    /// Until the asynchronous read whose `OVERLAPPED` is at this address
    /// has completed ([`Writing::until_done`]), with the held pin's number.
    Done(u64, usize),
}

/// An `OVERLAPPED`'s status while its operation is in flight.
const STATUS_PENDING: usize = 0x103;

/// Whether the operation of the `OVERLAPPED` at `at` is still in flight:
/// its status (the first word) is pending. Read so that a page let go of
/// reads as done: the title may free the structure only once it completed.
fn pending(at: usize) -> bool {
    let mut word = [0u8; size_of::<usize>()];
    crate::host::read_into(at as *const u8, &mut word) && usize::from_ne_bytes(word) == STATUS_PENDING
}

impl Until {
    /// Whether the pin still holds at `now`.
    fn holds(&self, now: std::time::Instant) -> bool {
        match *self {
            Until::Time(until) => until > now,
            Until::Held(_) => true,
            Until::Done(_, overlapped) => pending(overlapped),
        }
    }
}

static GUARDS: Mutex<Guards> = Mutex::new(Guards {
    protected: BTreeSet::new(),
    clean: BTreeSet::new(),
    holes: BTreeSet::new(),
    aliased: BTreeSet::new(),
    pins: Vec::new(),
});

/// How long blocks stay writable after [`writing`].
const PIN: std::time::Duration = std::time::Duration::from_secs(2);

impl Guards {
    /// Whether `block` is left writable for a kernel write.
    fn pinned(&self, block: usize) -> bool {
        self.pins.iter().any(|&(from, to, _)| from <= block && block < to)
    }

    /// Lets go of the pins that ended at `now`.
    fn unpin_ended(&mut self, now: std::time::Instant) {
        #[cfg(feature = "diagnostics")]
        let mut done = 0u64;
        self.pins.retain(|(_, _, until)| {
            let holds = until.holds(now);
            #[cfg(feature = "diagnostics")]
            if !holds && matches!(until, Until::Done(..)) {
                done += 1;
            }
            holds
        });
        #[cfg(feature = "diagnostics")]
        if done != 0 {
            crate::copystat::add(crate::copystat::C::PinsAsyncDone, done);
        }
    }
}

/// How many pins hold now (diagnostics).
#[cfg(feature = "diagnostics")]
pub(crate) fn pins_alive() -> usize {
    guards().pins.len()
}

/// Views of one page past which its views are not guarded together (the
/// page the title shows wherever it has nothing has thousands).
pub(crate) const MANY_VIEWS: usize = 64;

/// The views of physical pages the runtime mapped: by address (length,
/// section, offset in it), and the addresses each page of a section is
/// shown at.
#[derive(Default)]
struct Views {
    by_addr: BTreeMap<usize, (usize, usize, u64)>,
    pages: std::collections::HashMap<(usize, u64), Vec<usize>>,
}

static VIEWS: Mutex<Option<Views>> = Mutex::new(None);

/// The run of views holding `[at, end)` whose pages follow one another
/// both here and in one section, within `[low, high)`: its start and end,
/// the section, and the section offset at the start. `None` where no such
/// run holds all of the range (a gap, another section, or pages out of
/// order in it). Only views the runtime reported are known
/// ([`title_memory_view`], so not with `VICTORY_VIEW_GUARD=0`).
pub(crate) fn run_of_views(at: usize, end: usize, low: usize, high: usize) -> Option<(usize, usize, usize, u64)> {
    let views = VIEWS.lock().unwrap_or_else(|e| e.into_inner());
    let views = views.as_ref()?;
    let (&first, &(len, section, offset)) = views.by_addr.range(..=at).next_back()?;
    if first + len <= at || first < low || first + len > high {
        return None;
    }
    // Up from the view holding `at`, then down from it.
    let (mut stop, mut next) = (first + len, offset + len as u64);
    for (&a, &(n, s, o)) in views.by_addr.range(stop..high) {
        if a != stop || s != section || o != next || a + n > high {
            break;
        }
        (stop, next) = (a + n, o + n as u64);
    }
    if stop < end {
        return None;
    }
    let (mut start, mut from) = (first, offset);
    for (&a, &(n, s, o)) in views.by_addr.range(low..first).rev() {
        if a + n != start || s != section || o + n as u64 != from {
            break;
        }
        (start, from) = (a, o);
    }
    Some((start, stop, section, from))
}

/// Whether all the views of a guarded page are guarded together.
fn view_guard() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_VIEW_GUARD").is_none_or(|v| v != "0"))
}

impl Views {
    /// Forgets the views overlapping `[at, at + len)`, whole.
    fn forget(&mut self, at: usize, len: usize) {
        let end = at.saturating_add(len);
        let first = self
            .by_addr
            .range(..=at)
            .next_back()
            .filter(|(a, (n, _, _))| *a + n > at)
            .map(|(a, _)| *a);
        let hit: Vec<usize> = first
            .into_iter()
            .chain(self.by_addr.range(at.saturating_add(1)..end).map(|(a, _)| *a))
            .collect();
        for a in hit {
            let Some((n, section, offset)) = self.by_addr.remove(&a) else {
                continue;
            };
            for k in 0..n / BLOCK {
                let key = (section, offset / BLOCK as u64 + k as u64);
                if let Some(shown) = self.pages.get_mut(&key) {
                    shown.retain(|&x| x != a + k * BLOCK);
                    if shown.is_empty() {
                        self.pages.remove(&key);
                    }
                }
            }
        }
    }

    /// Every address the page shown at `at` (a block's start) is shown at,
    /// `at` included; empty if `at` is in no view we were told of.
    fn shown(&self, at: usize) -> &[usize] {
        let Some((&base, &(n, section, offset))) = self.by_addr.range(..=at).next_back() else {
            return &[];
        };
        if at >= base + n {
            return &[];
        }
        let key = (section, offset / BLOCK as u64 + ((at - base) / BLOCK) as u64);
        self.pages.get(&key).map_or(&[], Vec::as_slice)
    }
}

/// Every address the page of the block at `at` is shown at (see
/// [`Views::shown`]).
pub(crate) fn aliases(at: usize) -> Vec<usize> {
    let views = VIEWS.lock().unwrap_or_else(|e| e.into_inner());
    views.as_ref().map_or(Vec::new(), |v| v.shown(at - at % BLOCK).to_vec())
}

/// Diagnostics (`crate::writeaudit`, `VICTORY_STRETCH_AUDIT`): how
/// `[at, at + len)` is made of the views the runtime was told of. Reads the
/// views only.
#[cfg(feature = "diagnostics")]
#[derive(Default)]
pub(crate) struct AuditLayout {
    /// Views overlapping the range.
    pub(crate) views: usize,
    /// The range's first byte is in memory of a section (`MEM_MAPPED`).
    pub(crate) mapped: bool,
    /// Bytes of the range inside views, and between them.
    pub(crate) covered: usize,
    pub(crate) gaps: usize,
    /// Distinct sections; runs of views that follow one another in address
    /// and in their section's offsets.
    pub(crate) sections: usize,
    pub(crate) runs: usize,
    pub(crate) section: usize,
    pub(crate) first_view: usize,
    pub(crate) first_len: usize,
    pub(crate) first_offset: u64,
    pub(crate) last_view: usize,
    pub(crate) last_len: usize,
    /// Blocks of the range whose page is shown at other addresses too, and
    /// those addresses.
    pub(crate) aliased_blocks: usize,
    pub(crate) alias_addresses: usize,
    /// `VirtualQuery`'s allocation base is the same at both ends.
    pub(crate) same_base: bool,
    /// Each view: address, length, section, offset, whether it follows the
    /// one before in address and section, other addresses of its first block.
    pub(crate) list: Vec<(usize, usize, usize, u64, bool, usize)>,
}

#[cfg(feature = "diagnostics")]
pub(crate) fn audit_layout(at: usize, len: usize) -> AuditLayout {
    use windows_sys::Win32::System::Memory::{MEM_MAPPED, MEMORY_BASIC_INFORMATION, VirtualQuery};
    let query = |a: usize| {
        // SAFETY: plain data filled in by the query.
        let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
        // SAFETY: a valid out pointer of the right size.
        unsafe { VirtualQuery(a as *const _, &mut info, size_of_val(&info)) };
        info
    };
    let (first, last) = (query(at), query(at + len.max(1) - 1));
    let mut out = AuditLayout {
        mapped: first.Type == MEM_MAPPED,
        same_base: first.AllocationBase == last.AllocationBase,
        ..Default::default()
    };
    let views = VIEWS.lock().unwrap_or_else(|e| e.into_inner());
    let Some(views) = views.as_ref() else { return out };
    let end = at.saturating_add(len);
    let from = views
        .by_addr
        .range(..=at)
        .next_back()
        .filter(|(a, (n, _, _))| *a + n > at)
        .map_or(at, |(a, _)| *a);
    let mut sections = std::collections::BTreeSet::new();
    let mut before: Option<(usize, usize, u64)> = None;
    let mut cursor = at;
    let mut aliases = std::collections::BTreeSet::new();
    for (&a, &(n, section, offset)) in views.by_addr.range(from..end) {
        let follows = before.is_some_and(|(e, s, o)| e == a && s == section && o == offset);
        if !follows {
            out.runs += 1;
        }
        if out.views == 0 {
            (out.first_view, out.first_len, out.first_offset, out.section) = (a, n, offset, section);
        }
        (out.last_view, out.last_len) = (a, n);
        out.views += 1;
        sections.insert(section);
        let (lo, hi) = (a.max(at), (a + n).min(end));
        if lo > cursor {
            out.gaps += lo - cursor;
        }
        out.covered += hi.saturating_sub(lo);
        cursor = cursor.max(hi);
        let mut others_first = 0;
        for k in 0..n / BLOCK {
            let block = a + k * BLOCK;
            if block < at || block >= end {
                continue;
            }
            let shown = views.shown(block);
            if shown.len() > 1 {
                out.aliased_blocks += 1;
                aliases.extend(shown.iter().copied().filter(|&x| x != block));
            }
            if k == 0 {
                others_first = shown.len().saturating_sub(1);
            }
        }
        out.list.push((a, n, section, offset, follows, others_first));
        before = Some((a + n, section, offset + n as u64));
    }
    out.sections = sections.len();
    out.alias_addresses = aliases.len();
    out
}

/// The runtime mapped a view at `[at, at + len)` of the bytes from `offset`
/// on of the physical-page section `section`. The view is writable, so the
/// same pages guarded at another view are opened: their copies check them
/// again, which also guards the new view with them.
pub fn title_memory_view(at: usize, len: usize, section: usize, offset: u64) {
    if !view_guard() || len == 0 || !at.is_multiple_of(BLOCK) || !offset.is_multiple_of(BLOCK as u64) {
        return;
    }
    let mut others = Vec::new();
    {
        let mut views = VIEWS.lock().unwrap_or_else(|e| e.into_inner());
        let views = views.get_or_insert_with(Views::default);
        views.forget(at, len);
        views.by_addr.insert(at, (len, section, offset));
        for k in 0..len / BLOCK {
            let shown = views.pages.entry((section, offset / BLOCK as u64 + k as u64)).or_default();
            if shown.len() < MANY_VIEWS {
                others.extend_from_slice(shown);
            }
            shown.push(at + k * BLOCK);
        }
    }
    if !others.is_empty() {
        let mut g = guards();
        for a in others {
            open_here(&mut g, a, a + BLOCK);
        }
    }
}

/// The runtime unmapped the views in `[at, at + len)`.
pub fn title_memory_view_gone(at: usize, len: usize) {
    if let Some(views) = VIEWS.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
        views.forget(at, len);
    }
}

fn guards() -> std::sync::MutexGuard<'static, Guards> {
    GUARDS.lock().unwrap_or_else(|e| e.into_inner())
}

/// For view memory: the pages of `[start, end)`'s blocks written since
/// they were last guarded (all of a block's pages), guarding them again
/// before the caller copies them. `None` if the memory is not a view.
fn guard(start: usize, end: usize) -> Option<Vec<usize>> {
    let (first, last) = (start / BLOCK, end.div_ceil(BLOCK));
    // Fast path: if we already guard or recorded a hole at `first`, we already
    // know this is a mapped view and can skip `QueryVirtualMemoryInformation`.
    {
        let mut g = guards();
        if g.protected.contains(&first) || g.holes.contains(&first) {
            return Some(walk(&mut g, first, last, false));
        }
    }
    // Slow path (first encounter): `start` is itself a mapped view, or if
    // `start` is an unmapped hole (common in 2 GiB sparse shadows), jump past
    // leading holes via `VirtualQuery` to find the first committed page.
    let probe = if mapped(start) {
        start
    } else {
        first_mapped(start, end)?
    };
    let check_each = probe != start;
    install();
    let mut g = guards();
    Some(walk(&mut g, first, last, check_each))
}

/// [`guard`]'s look at blocks `[first, last)`, under the guards' lock: the
/// pages of the blocks written since they were last guarded, guarding the
/// open ones again. A pinned block ([`writing`]) is left writable and
/// reported written, wherever it lies in the range (it was protected again
/// when the range began with a guarded block, before 2026-11-26, and the
/// kernel's write into it then failed). With `check_each`, blocks are
/// looked at only where something is mapped.
fn walk(g: &mut Guards, first: usize, last: usize, check_each: bool) -> Vec<usize> {
    if !g.pins.is_empty() {
        g.unpin_ended(std::time::Instant::now());
    }
    let mut written = Vec::new();
    let mut run: Option<usize> = None;
    for block in first..=last {
        let open = block < last
            && !g.clean.contains(&block)
            && !g.holes.contains(&block)
            && (!check_each || mapped(block * BLOCK));
        if open {
            if g.pinned(block) {
                // The kernel may be writing it: left writable, reported
                // written at every check until the pin ends.
                written.extend(block * BLOCK / PAGE..(block + 1) * BLOCK / PAGE);
                #[cfg(feature = "diagnostics")]
                note_origin(block * BLOCK / PAGE, (block + 1) * BLOCK / PAGE, ORIGIN_PIN);
                // No range holding it is clean meanwhile (`region_epoch`).
                bump_region_epochs(block * BLOCK, (block + 1) * BLOCK);
            } else if g.protected.contains(&block) {
                // Written through the runtime, still read-only.
                g.clean.insert(block);
                written.extend(block * BLOCK / PAGE..(block + 1) * BLOCK / PAGE);
                #[cfg(feature = "diagnostics")]
                note_origin(block * BLOCK / PAGE, (block + 1) * BLOCK / PAGE, ORIGIN_UNCLEAN);
            } else {
                run.get_or_insert(block);
                continue;
            }
        }
        if let Some(from) = run.take() {
            protect(g, from, block, &mut written);
        }
    }
    written
}

/// Scans `[start, end)` jumping over unmapped holes; returns the first
/// committed address if it is a mapped view, or `None` if `[start, end)` has
/// no committed mapped view (or starts with committed non-view memory).
fn first_mapped(start: usize, end: usize) -> Option<usize> {
    use windows_sys::Win32::System::Memory::{
        MEM_COMMIT, MEM_MAPPED, MEMORY_BASIC_INFORMATION, VirtualQuery,
    };
    let mut at = start;
    while at < end {
        let mut info = MEMORY_BASIC_INFORMATION::default();
        // SAFETY: queries our own address space without dereferencing `at`.
        let n = unsafe { VirtualQuery(at as *const _, &mut info, size_of_val(&info)) };
        if n == 0 || info.RegionSize == 0 {
            return None;
        }
        if info.State == MEM_COMMIT {
            return (info.Type == MEM_MAPPED).then_some(at);
        }
        let next = (info.BaseAddress as usize).saturating_add(info.RegionSize);
        if next <= at {
            return None;
        }
        at = next;
    }
    None
}

/// Whether the current allocation at `at` is a mapped view. Asking only for
/// the allocation type avoids enumerating pages with identical protection,
/// which can span an entire large view after its blocks were guarded.
fn mapped(at: usize) -> bool {
    use windows_sys::Win32::System::Memory::{
        MEM_MAPPED, MEMORY_BASIC_INFORMATION, MemoryRegionInfo, QueryVirtualMemoryInformation,
        VirtualQuery, WIN32_MEMORY_REGION_INFORMATION,
    };
    use windows_sys::Win32::System::Threading::GetCurrentProcess;

    let mut region = WIN32_MEMORY_REGION_INFORMATION::default();
    // SAFETY: both queries describe an address without dereferencing it. The
    // output buffers match their information classes.
    let queried = unsafe {
        QueryVirtualMemoryInformation(
            GetCurrentProcess(),
            at as *const _,
            MemoryRegionInfo,
            (&raw mut region).cast(),
            size_of_val(&region),
            std::ptr::null_mut(),
        )
    };
    if queried != 0 {
        // These are the Windows SDK's named bitfields in declaration order.
        // windows-sys exposes only the union's raw bits.
        const PRIVATE: u32 = 1 << 0;
        const MAPPED_DATA_FILE: u32 = 1 << 1;
        const MAPPED_IMAGE: u32 = 1 << 2;
        const MAPPED_PAGE_FILE: u32 = 1 << 3;
        const MAPPED_PHYSICAL: u32 = 1 << 4;
        // SAFETY: this union was filled by the successful query above.
        match unsafe { region.Anonymous.Flags } {
            MAPPED_DATA_FILE | MAPPED_PAGE_FILE | MAPPED_PHYSICAL => return true,
            PRIVATE | MAPPED_IMAGE => return false,
            _ => {}
        }
    }
    // Unknown flag combinations and failed allocation queries keep the
    // VirtualQuery type check, including for a free address.
    let mut info = MEMORY_BASIC_INFORMATION::default();
    // SAFETY: the output buffer has the required layout and size.
    let n = unsafe { VirtualQuery(at as *const _, &mut info, size_of_val(&info)) };
    n != 0 && info.Type == MEM_MAPPED
}

/// Makes blocks `[from, to)` read-only and notes them guarded and clean;
/// block by block if they span views, recording unmapped blocks in `g.holes`
/// so subsequent checks neither retry `VirtualProtect` nor fault copying them.
fn protect(g: &mut Guards, from: usize, to: usize, written: &mut Vec<usize>) {
    use windows_sys::Win32::System::Memory::{PAGE_READONLY, VirtualProtect};
    let set = |from: usize, to: usize| {
        let mut old = 0;
        // SAFETY: title view memory; only its protection changes.
        unsafe {
            VirtualProtect(
                (from * BLOCK) as *const _,
                (to - from) * BLOCK,
                PAGE_READONLY,
                &mut old,
            ) != 0
        }
    };
    let note = |g: &mut Guards, written: &mut Vec<usize>, from: usize, to: usize| {
        g.protected.extend(from..to);
        g.clean.extend(from..to);
        written.extend(from * BLOCK / PAGE..to * BLOCK / PAGE);
        #[cfg(feature = "diagnostics")]
        note_origin(from * BLOCK / PAGE, to * BLOCK / PAGE, ORIGIN_OPEN);
    };
    if set(from, to) {
        note(g, written, from, to);
    } else {
        for block in from..to {
            if set(block, block + 1) {
                note(g, written, block, block + 1);
            } else {
                g.holes.insert(block);
            }
        }
    }
    if !view_guard() {
        return;
    }
    // The same pages at their other views: read-only too, so that a write
    // through one of those faults. They are not noted clean: the copies
    // taken at those views have not taken the pages, and their next check
    // must report them (a block read-only and not clean is reported).
    // One look at the views for the whole run, and no list made for a page
    // with one view (most of them): this runs under the guards' lock, which
    // every copy's check waits for (6 % of the submitting thread's time when
    // it asked block by block, run p1).
    let views = VIEWS.lock().unwrap_or_else(|e| e.into_inner());
    let Some(views) = views.as_ref() else { return };
    for block in from..to {
        let shown = views.shown(block * BLOCK);
        if shown.len() < 2 || shown.len() > MANY_VIEWS || !g.protected.contains(&block) {
            continue;
        }
        for &at in shown {
            let alias = at / BLOCK;
            g.aliased.insert(alias);
            if alias != block && !g.protected.contains(&alias) && !g.pinned(alias) {
                let mut old = 0;
                // SAFETY: title view memory; only its protection changes.
                if unsafe { VirtualProtect(at as *const _, BLOCK, PAGE_READONLY, &mut old) } != 0 {
                    g.protected.insert(alias);
                }
            }
        }
    }
}

/// Gives write access back to the guarded blocks of `[start, end)` and
/// notes them written; true if any was guarded. The same pages at their
/// other views are opened too: every guard over them must copy them again.
fn open(g: &mut Guards, start: usize, end: usize) -> bool {
    let any = open_here(g, start, end);
    if start >= end || g.aliased.is_empty() {
        return any;
    }
    let aliased: Vec<usize> = g.aliased.range(start / BLOCK..end.div_ceil(BLOCK)).copied().collect();
    for block in aliased {
        g.aliased.remove(&block);
        for at in aliases(block * BLOCK) {
            if at / BLOCK != block {
                open_here(g, at, at + BLOCK);
            }
            g.aliased.remove(&(at / BLOCK));
        }
    }
    any
}

/// [`open`] for one range, without the other views of its pages.
fn open_here(g: &mut Guards, start: usize, end: usize) -> bool {
    use windows_sys::Win32::System::Memory::{PAGE_READWRITE, VirtualProtect};
    let blocks: Vec<usize> = g
        .protected
        .range(start / BLOCK..end.div_ceil(BLOCK))
        .copied()
        .collect();
    if blocks.is_empty() {
        return false;
    }
    for &block in &blocks {
        g.protected.remove(&block);
        g.clean.remove(&block);
    }
    // Runs of blocks next to each other in one call (fewer kernel
    // transitions and TLB shootdowns). One call cannot span two views: the
    // title's page mappings are one view a page, and a run over several
    // failed whole, leaving blocks read-only that were no longer noted
    // guarded (a kernel read into them then failed and the title stopped).
    // So a run that fails is given back block by block.
    let give = |from: usize, to: usize| {
        let mut old = 0;
        // SAFETY: blocks we made read-only; only their protection changes.
        let whole =
            unsafe { VirtualProtect((from * BLOCK) as *const _, (to - from) * BLOCK, PAGE_READWRITE, &mut old) } != 0;
        if !whole && to - from > 1 {
            for block in from..to {
                // SAFETY: as above, one block.
                unsafe { VirtualProtect((block * BLOCK) as *const _, BLOCK, PAGE_READWRITE, &mut old) };
            }
        }
    };
    let mut run_start = blocks[0];
    let mut run_end = run_start + 1;
    for &block in &blocks[1..] {
        if block == run_end {
            run_end = block + 1;
        } else {
            give(run_start, run_end);
            run_start = block;
            run_end = block + 1;
        }
    }
    give(run_start, run_end);
    bump_region_epochs(start, end);
    true
}

/// Gives write access back to the guarded blocks of `[at, at + len)` for
/// [`PIN`], for a writer whose writes go on after the call (the host GPU
/// through a window on title memory). The same pages at their other views
/// are opened too: every guard over them must copy them again. A write
/// that ends when its caller is done takes [`writing_held`] instead.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn writing(at: usize, len: usize) {
    if len == 0 {
        return;
    }
    #[cfg(feature = "diagnostics")]
    crate::copystat::add(crate::copystat::C::PinsTimed, 1);
    pin(at, len, Until::Time(std::time::Instant::now() + PIN));
}

/// A write into title memory that does not fault into our handler, in
/// progress ([`writing_held`]): its blocks stay writable, and are reported
/// written at every check, until it is dropped, or for an asynchronous
/// read until that completed ([`Writing::until_done`]).
#[must_use = "the blocks are guarded again when it is dropped"]
pub struct Writing {
    number: u64,
    range: (usize, usize),
    #[cfg(feature = "diagnostics")]
    began: std::time::Instant,
}

/// Before a write into title memory `[at, at + len)` that does not fault
/// into our handler (the kernel reading a file into it, our copies of what
/// the host GPU wrote): opens its guarded blocks, here and at the other
/// views of their pages, and keeps them writable while the result lives.
pub fn writing_held(at: usize, len: usize) -> Writing {
    static NUMBER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    let number = NUMBER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    if len != 0 {
        #[cfg(feature = "diagnostics")]
        crate::copystat::add(crate::copystat::C::PinsHeld, 1);
        pin(at, len, Until::Held(number));
    }
    Writing {
        number,
        range: (at, at + len),
        #[cfg(feature = "diagnostics")]
        began: std::time::Instant::now(),
    }
}

impl Writing {
    /// The write is an asynchronous read that is still in flight, whose
    /// `OVERLAPPED` is at `overlapped`: its blocks stay writable until the
    /// read completed (its status is no longer pending), not when this
    /// returns.
    pub fn until_done(self, overlapped: *const std::ffi::c_void) {
        let this = std::mem::ManuallyDrop::new(self);
        let mut g = guards();
        for pin in g.pins.iter_mut().filter(|p| p.2 == Until::Held(this.number)) {
            pin.2 = Until::Done(this.number, overlapped as usize);
        }
        #[cfg(feature = "diagnostics")]
        crate::copystat::add(crate::copystat::C::PinsAsync, 1);
    }
}

impl Drop for Writing {
    fn drop(&mut self) {
        let (at, end) = self.range;
        if at == end {
            return;
        }
        // The blocks are left open and not clean: their next check guards
        // them again and reports them written. No region epoch is bumped
        // here: a range is noted clean (`Shadow::clean_spans`) only by a
        // check that guarded all its blocks, so opening them bumped it
        // already, and a check while they were pinned bumped it again.
        guards().pins.retain(|p| p.2 != Until::Held(self.number));
        #[cfg(feature = "diagnostics")]
        crate::copystat::add(crate::copystat::C::PinHeldUs, self.began.elapsed().as_micros() as u64);
    }
}

/// Pins the blocks of `[at, at + len)` and the same pages at their other
/// views until `until`, and opens them.
fn pin(at: usize, len: usize, until: Until) {
    #[cfg(feature = "diagnostics")]
    crate::backread::announced(at, len);
    let (first, last) = (at / BLOCK, (at + len).div_ceil(BLOCK));
    let mut g = guards();
    g.pins.push((first, last, until));
    open(&mut g, at, at + len);
    if !view_guard() {
        return;
    }
    // The same pages at their other views: the write lands in them too, so
    // their copies must take them again, now and when it has landed.
    for block in first..last {
        let shown = aliases(block * BLOCK);
        if shown.len() < 2 || shown.len() > MANY_VIEWS {
            continue;
        }
        for alias in shown {
            if alias / BLOCK != block {
                g.pins.push((alias / BLOCK, alias / BLOCK + 1, until));
                open_here(&mut g, alias, alias + BLOCK);
            }
        }
    }
}

/// The runtime mapped or unmapped title memory `[at, at + len)`: what it
/// shows is new and read-write, so it is no longer guarded, and any placed
/// textures backed by that range are evicted.
pub fn title_memory_remapped(at: usize, len: usize) {
    if len == 0 {
        return;
    }
    let end = at.saturating_add(len);
    {
        let mut g = guards();
        let blocks: Vec<usize> = g
            .protected
            .range(at / BLOCK..end.div_ceil(BLOCK))
            .copied()
            .collect();
        let holes: Vec<usize> = g
            .holes
            .range(at / BLOCK..end.div_ceil(BLOCK))
            .copied()
            .collect();
        let changed = !holes.is_empty() || !blocks.is_empty();
        for block in holes {
            g.holes.remove(&block);
        }
        for block in blocks {
            g.protected.remove(&block);
            g.clean.remove(&block);
            // A view mapped a moment ago may already have been made
            // read-only as another view of a guarded page (between the
            // runtime's `title_memory_view` and this call): it is given
            // back, or the title's next write there would fault with no
            // guard to answer for it.
            let mut old = 0;
            // SAFETY: only the protection of title view memory changes; the
            // call fails harmlessly on memory that is no longer a view.
            unsafe {
                windows_sys::Win32::System::Memory::VirtualProtect(
                    (block * BLOCK) as *const _,
                    BLOCK,
                    windows_sys::Win32::System::Memory::PAGE_READWRITE,
                    &mut old,
                )
            };
        }
        let aliased: Vec<usize> = g.aliased.range(at / BLOCK..end.div_ceil(BLOCK)).copied().collect();
        for block in aliased {
            g.aliased.remove(&block);
        }
        if changed {
            bump_region_epochs(at, end);
        }
    }
    crate::shadow::remapped(at as u64, end as u64);
    crate::resource::evict_placed_textures(at as u64, end as u64);
    crate::descriptor::host::bindless_memory_changed(at as u64, end as u64);
}

/// The title committed memory `[at, at + len)` it had reserved.
pub fn title_memory_committed(at: usize, len: usize) {
    if len != 0 {
        crate::shadow::filled(at as u64, (at + len) as u64);
    }
}

/// The runtime wrote new contents into title memory `[at, at + len)` (a
/// streamed file arrived): textures placed there are uploaded again at their
/// next use, not when their turn to be looked at comes.
pub fn title_memory_written(at: usize, len: usize) {
    if len != 0 {
        bump_region_epochs(at, at.saturating_add(len));
        // The runtime writes through a view of its own, which neither the
        // kernel's write watch nor our guards see: the pages are noted
        // written here, or host copies of buffers made before the file
        // arrived kept what the memory held then for good (meshes read as
        // zeros: walls, floors and bodies missing, others each launch).
        {
            let mut j = JOURNAL.lock().unwrap_or_else(|e| e.into_inner());
            j.asked += 1;
            let now = j.asked;
            for page in at / PAGE..(at + len).div_ceil(PAGE) {
                j.written.insert(page, now);
            }
        }
        #[cfg(feature = "diagnostics")]
        note_origin(at / PAGE, (at + len).div_ceil(PAGE), ORIGIN_STREAM);
        crate::resource::textures_rewritten(at as u64, (at + len) as u64);
    }
}

/// The host GPU wrote title memory `[at, at + len)` in place (a window on
/// the title's own pages, not a copy): copies of those pages, here and at
/// their other views, take them again. Nothing's protection changes, as no
/// thread wrote: a guarded block noted not clean is reported written at its
/// next check ([`guard`]) without a kernel call, and the journal and the
/// region epochs tell the copies that are not guarded.
pub(crate) fn gpu_wrote(at: usize, len: usize) {
    if len == 0 {
        return;
    }
    let end = at.saturating_add(len);
    let (first, last) = (at / BLOCK, end.div_ceil(BLOCK));
    let mut blocks: Vec<usize> = (first..last).collect();
    if view_guard()
        && let Some(views) = VIEWS.lock().unwrap_or_else(|e| e.into_inner()).as_ref()
    {
        for block in first..last {
            let shown = views.shown(block * BLOCK);
            if shown.len() < 2 || shown.len() > MANY_VIEWS {
                continue;
            }
            blocks.extend(shown.iter().map(|a| a / BLOCK).filter(|&b| b != block));
        }
    }
    blocks.sort_unstable();
    blocks.dedup();
    {
        let mut g = guards();
        for block in &blocks {
            g.clean.remove(block);
        }
    }
    {
        let mut j = JOURNAL.lock().unwrap_or_else(|e| e.into_inner());
        j.asked += 1;
        let now = j.asked;
        for block in &blocks {
            for page in block * BLOCK / PAGE..(block + 1) * BLOCK / PAGE {
                j.written.insert(page, now);
            }
        }
    }
    #[cfg(feature = "diagnostics")]
    for block in &blocks {
        note_origin(block * BLOCK / PAGE, (block + 1) * BLOCK / PAGE, ORIGIN_GPU);
    }
    // Runs of blocks next to one another, one bump each.
    let mut run: Option<(usize, usize)> = None;
    for &block in &blocks {
        run = match run {
            Some((from, to)) if to == block => Some((from, block + 1)),
            Some((from, to)) => {
                bump_region_epochs(from * BLOCK, to * BLOCK);
                Some((block, block + 1))
            }
            None => Some((block, block + 1)),
        };
    }
    if let Some((from, to)) = run {
        bump_region_epochs(from * BLOCK, to * BLOCK);
    }
}

/// Opens a guarded block at `at` (or returns `true` if another thread already
/// made it read-write). Called both by our own VEH handler and by
/// `host::safe_copy`'s VEH handler on write faults.
pub(crate) fn try_open_guarded(at: usize) -> bool {
    let mut g = guards();
    if open(&mut g, at, at + 1) {
        #[cfg(feature = "diagnostics")]
        crate::copystat::add(crate::copystat::C::GuardFaults, 1);
        return true;
    }
    if writable(at) {
        return true;
    }
    // Read-only, in a view we were told of, and not noted guarded: only we
    // make title views read-only, so it is ours to give back.
    if view_guard() && !aliases(at).is_empty() {
        let mut old = 0;
        let block = at - at % BLOCK;
        // SAFETY: only the protection of title view memory changes.
        let opened = unsafe {
            windows_sys::Win32::System::Memory::VirtualProtect(
                block as *const _,
                BLOCK,
                windows_sys::Win32::System::Memory::PAGE_READWRITE,
                &mut old,
            )
        } != 0;
        if opened {
            bump_region_epochs(block, block + BLOCK);
            tracing::warn!(at = format_args!("{at:#x}"), "a read-only view block that no guard knew was opened");
        }
        return opened;
    }
    false
}

/// Registers the handler that takes writes to guarded blocks, once, ahead
/// of the crash reporter's.
fn install() {
    use windows_sys::Win32::Foundation::{EXCEPTION_ACCESS_VIOLATION, NTSTATUS};
    use windows_sys::Win32::System::Diagnostics::Debug::{
        AddVectoredExceptionHandler, EXCEPTION_CONTINUE_EXECUTION, EXCEPTION_CONTINUE_SEARCH,
        EXCEPTION_POINTERS,
    };
    unsafe extern "system" fn on_write(info: *mut EXCEPTION_POINTERS) -> i32 {
        // SAFETY: the system passes the exception being dispatched.
        let r = unsafe { &*(*info).ExceptionRecord };
        const WRITE: usize = 1;
        if r.ExceptionCode != EXCEPTION_ACCESS_VIOLATION as NTSTATUS
            || r.NumberParameters < 2
            || r.ExceptionInformation[0] != WRITE
        {
            return EXCEPTION_CONTINUE_SEARCH;
        }
        let at = r.ExceptionInformation[1];
        if try_open_guarded(at) {
            EXCEPTION_CONTINUE_EXECUTION
        } else {
            EXCEPTION_CONTINUE_SEARCH
        }
    }
    static ONCE: std::sync::Once = std::sync::Once::new();
    // SAFETY: the handler only touches our guard state and protections.
    ONCE.call_once(|| unsafe {
        AddVectoredExceptionHandler(1, Some(on_write));
    });
}

/// Whether `at` is committed read-write memory now.
fn writable(at: usize) -> bool {
    use windows_sys::Win32::System::Memory::{
        MEM_COMMIT, MEMORY_BASIC_INFORMATION, PAGE_READWRITE, VirtualQuery,
    };
    // SAFETY: plain data filled in by the query.
    let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: a valid out pointer of the right size.
    let n = unsafe { VirtualQuery(at as *const _, &mut info, size_of_val(&info)) };
    n != 0 && info.State == MEM_COMMIT && info.Protect == PAGE_READWRITE
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows_sys::Win32::System::Memory::{
        MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_READWRITE, VirtualAlloc, VirtualFree,
    };
    const MEM_WRITE_WATCH: u32 = 0x20_0000;

    /// Two views of the same two physical pages, told as the runtime tells
    /// them: the two addresses.
    fn two_views() -> (usize, usize) {
        use windows_sys::Win32::System::Memory::{CreateFileMappingW, FILE_MAP_WRITE, MapViewOfFile};
        // SAFETY: a pagefile section of two blocks, mapped read-write twice.
        unsafe {
            let section = CreateFileMappingW(
                -1isize as _,
                std::ptr::null(),
                PAGE_READWRITE,
                0,
                (2 * BLOCK) as u32,
                std::ptr::null(),
            );
            assert!(!section.is_null());
            let one = MapViewOfFile(section, FILE_MAP_WRITE, 0, 0, 2 * BLOCK).Value as usize;
            let two = MapViewOfFile(section, FILE_MAP_WRITE, 0, 0, 2 * BLOCK).Value as usize;
            assert!(one != 0 && two != 0);
            title_memory_view(one, 2 * BLOCK, section as usize, 0);
            title_memory_view(two, 2 * BLOCK, section as usize, 0);
            (one, two)
        }
    }

    fn drop_views(views: &[usize]) {
        use windows_sys::Win32::System::Memory::{MEMORY_MAPPED_VIEW_ADDRESS, UnmapViewOfFile};
        for &view in views {
            title_memory_remapped(view, 2 * BLOCK);
            title_memory_view_gone(view, 2 * BLOCK);
            // SAFETY: our view.
            unsafe { UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS { Value: view as *mut _ }) };
        }
    }

    /// The title writes the pages of a guarded copy through another view of
    /// them: the guarded view's next check reports the page written.
    #[test]
    fn a_write_through_another_view_is_reported_at_the_guarded_one() {
        let (guarded, other) = two_views();
        let pages = |block: usize| (guarded + block * BLOCK) / PAGE;
        assert_eq!(guard(guarded, guarded + 2 * BLOCK), Some(vec![pages(0), pages(1)]));
        assert_eq!(guard(guarded, guarded + 2 * BLOCK), Some(Vec::new()), "nothing written yet");
        // SAFETY: the second view; the handler opens it on the fault.
        unsafe { ((other + BLOCK + 8) as *mut u8).write(1) };
        assert_eq!(guard(guarded, guarded + 2 * BLOCK), Some(vec![pages(1)]));
        assert_eq!(guard(guarded, guarded + 2 * BLOCK), Some(Vec::new()));
        drop_views(&[guarded, other]);
    }

    /// A run of views is views next to one another both here and in one
    /// section; a step out of order in the section, another section, a gap,
    /// or the edge of the reach ends it.
    #[test]
    fn views_make_a_run_when_they_follow_here_and_in_their_section() {
        let base = 0x7b00_0000_0000usize;
        let (section, other) = (0x61, 0x62);
        let reach = 1usize << 30;
        let (low, high) = (base, base + reach);
        // Three in order, then one out of order in the section, then one of
        // another section, then a gap and one more.
        for k in 0..3 {
            title_memory_view(base + k * BLOCK, BLOCK, section, (k * BLOCK) as u64);
        }
        title_memory_view(base + 3 * BLOCK, BLOCK, section, (7 * BLOCK) as u64);
        title_memory_view(base + 4 * BLOCK, BLOCK, other, (8 * BLOCK) as u64);
        title_memory_view(base + 6 * BLOCK, BLOCK, section, (9 * BLOCK) as u64);
        assert_eq!(
            run_of_views(base + BLOCK + 16, base + 2 * BLOCK + 32, low, high),
            Some((base, base + 3 * BLOCK, section, 0))
        );
        // From the middle of a run, the offset is the run's first page's.
        assert_eq!(
            run_of_views(base + 2 * BLOCK, base + 2 * BLOCK + 1, low, high),
            Some((base, base + 3 * BLOCK, section, 0))
        );
        assert_eq!(
            run_of_views(base + 3 * BLOCK, base + 4 * BLOCK, low, high),
            Some((base + 3 * BLOCK, base + 4 * BLOCK, section, (7 * BLOCK) as u64))
        );
        // Across the step out of order, across sections, across the gap.
        assert_eq!(run_of_views(base + 2 * BLOCK, base + 3 * BLOCK + 1, low, high), None);
        assert_eq!(run_of_views(base + 3 * BLOCK, base + 4 * BLOCK + 1, low, high), None);
        assert_eq!(run_of_views(base + 5 * BLOCK, base + 5 * BLOCK + 1, low, high), None);
        // The reach cuts the run.
        assert_eq!(
            run_of_views(base + 2 * BLOCK, base + 2 * BLOCK + 1, base + BLOCK, high),
            Some((base + BLOCK, base + 3 * BLOCK, section, BLOCK as u64))
        );
        assert_eq!(run_of_views(base, base + 1, low, base + 2 * BLOCK), Some((base, base + 2 * BLOCK, section, 0)));
        title_memory_view_gone(base, 7 * BLOCK);
        assert_eq!(run_of_views(base, base + 1, low, high), None);
    }

    /// A write of the host GPU in place is reported at the range and at the
    /// other views of its pages, and leaves every protection as it was.
    #[test]
    fn a_gpu_write_in_place_is_reported_at_every_view_of_its_pages() {
        let base = 0x7c00_0000_0000usize;
        let section = 0x71;
        title_memory_view(base, BLOCK, section, 0);
        title_memory_view(base + 8 * BLOCK, BLOCK, section, 0);
        let asked = JOURNAL.lock().unwrap_or_else(|e| e.into_inner()).asked;
        guards().clean.insert(base / BLOCK);
        guards().clean.insert(base / BLOCK + 8);
        gpu_wrote(base + 16, 32);
        for at in [base, base + 8 * BLOCK] {
            let stamp = JOURNAL.lock().unwrap_or_else(|e| e.into_inner()).written.get(&(at / PAGE)).copied();
            assert!(stamp.is_some_and(|w| w > asked), "{at:#x} not reported written");
            assert!(!guards().clean.contains(&(at / BLOCK)), "{at:#x} still clean");
        }
        // Not the blocks around.
        assert!(!JOURNAL.lock().unwrap_or_else(|e| e.into_inner()).written.get(&(base / PAGE + 1)).is_some_and(|&w| w > asked));
        title_memory_view_gone(base, 9 * BLOCK);
    }

    /// Both views have copies: a write through either is reported at both.
    #[test]
    fn a_write_is_reported_at_every_checked_view() {
        let (one, two) = two_views();
        let pages = |view: usize, block: usize| (view + block * BLOCK) / PAGE;
        assert_eq!(guard(one, one + 2 * BLOCK), Some(vec![pages(one, 0), pages(one, 1)]));
        // The other view was made read-only with the first, not noted clean.
        assert_eq!(guard(two, two + 2 * BLOCK), Some(vec![pages(two, 0), pages(two, 1)]));
        assert_eq!(guard(one, one + 2 * BLOCK), Some(Vec::new()));
        assert_eq!(guard(two, two + 2 * BLOCK), Some(Vec::new()));
        // SAFETY: the first view; the handler opens it on the fault.
        unsafe { ((one + 16) as *mut u8).write(7) };
        assert_eq!(guard(two, two + 2 * BLOCK), Some(vec![pages(two, 0)]));
        assert_eq!(guard(one, one + 2 * BLOCK), Some(vec![pages(one, 0)]));
        // SAFETY: the same byte through the other view.
        assert_eq!(unsafe { ((two + 16) as *const u8).read() }, 7);
        drop_views(&[one, two]);
    }

    /// A view mapped beside a guarded one is writable: the guarded view
    /// checks its pages again.
    #[test]
    fn a_view_mapped_beside_a_guarded_one_makes_it_check_again() {
        use windows_sys::Win32::System::Memory::{CreateFileMappingW, FILE_MAP_WRITE, MapViewOfFile};
        // SAFETY: a pagefile section of two blocks, mapped read-write.
        let (section, guarded) = unsafe {
            let section = CreateFileMappingW(
                -1isize as _,
                std::ptr::null(),
                PAGE_READWRITE,
                0,
                (2 * BLOCK) as u32,
                std::ptr::null(),
            );
            assert!(!section.is_null());
            (section, MapViewOfFile(section, FILE_MAP_WRITE, 0, 0, 2 * BLOCK).Value as usize)
        };
        title_memory_view(guarded, 2 * BLOCK, section as usize, 0);
        let pages = |block: usize| (guarded + block * BLOCK) / PAGE;
        assert_eq!(guard(guarded, guarded + 2 * BLOCK), Some(vec![pages(0), pages(1)]));
        assert_eq!(guard(guarded, guarded + 2 * BLOCK), Some(Vec::new()));
        // SAFETY: a second view of the second page only.
        let other = unsafe { MapViewOfFile(section, FILE_MAP_WRITE, 0, BLOCK as u32, BLOCK).Value as usize };
        assert_ne!(other, 0);
        title_memory_view(other, BLOCK, section as usize, BLOCK as u64);
        assert_eq!(guard(guarded, guarded + 2 * BLOCK), Some(vec![pages(1)]));
        title_memory_remapped(other, BLOCK);
        title_memory_view_gone(other, BLOCK);
        // SAFETY: our view.
        unsafe {
            windows_sys::Win32::System::Memory::UnmapViewOfFile(
                windows_sys::Win32::System::Memory::MEMORY_MAPPED_VIEW_ADDRESS { Value: other as *mut _ },
            )
        };
        drop_views(&[guarded]);
    }

    #[test]
    fn allocation_query_recognizes_file_views_and_rejects_nonviews() {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::System::Memory::{
            CreateFileMappingW, FILE_MAP_READ, MapViewOfFile, PAGE_READONLY, UnmapViewOfFile,
        };
        let file = tempfile::tempfile().unwrap();
        file.set_len(PAGE as u64).unwrap();
        // SAFETY: a valid, open file and a read-only mapping of its contents.
        let section = unsafe {
            CreateFileMappingW(
                file.as_raw_handle(),
                std::ptr::null(),
                PAGE_READONLY,
                0,
                0,
                std::ptr::null(),
            )
        };
        assert!(!section.is_null());
        // SAFETY: an open section containing at least one page.
        let view = unsafe { MapViewOfFile(section, FILE_MAP_READ, 0, 0, PAGE) };
        assert!(!view.Value.is_null());
        assert!(mapped(view.Value as usize + 13));
        // SAFETY: our successful mapping and its section handle.
        unsafe {
            UnmapViewOfFile(view);
            CloseHandle(section);
        }
        assert!(!mapped((&raw const file) as usize)); // private stack allocation
        assert!(!mapped(mapped as *const () as usize)); // executable image
        assert!(!mapped(0)); // no allocation to query
    }

    #[test]
    fn written_pages_are_reported_to_every_asker() {
        // SAFETY: a fresh write-watched allocation, freed at the end.
        let p = unsafe {
            VirtualAlloc(
                std::ptr::null(),
                4 * PAGE,
                MEM_RESERVE | MEM_COMMIT | MEM_WRITE_WATCH,
                PAGE_READWRITE,
            )
        } as usize;
        assert_ne!(p, 0);
        let (start, end) = (p, p + 4 * PAGE);
        let (_, before) = writes(start, end, false).expect("watched");
        assert!(before.is_empty());
        // SAFETY: committed pages of ours.
        unsafe { ((p + PAGE + 8) as *mut u8).write(1) };
        let (now, after) = writes(start, end, false).expect("watched");
        assert_eq!(after, [(p / PAGE + 1, now)]);
        // Another asker after the kernel's bit was cleared still sees it.
        let (later, again) = writes(p + PAGE, p + 2 * PAGE, false).expect("watched");
        assert!(later > now);
        assert_eq!(again, [(p / PAGE + 1, now)]);
        let mut plain = vec![0u8; PAGE];
        assert!(
            writes(
                plain.as_mut_ptr() as usize,
                plain.as_ptr() as usize + PAGE,
                true
            )
            .is_none()
        );
        // SAFETY: our allocation.
        unsafe { VirtualFree(p as *mut _, 0, MEM_RELEASE) };
    }

    /// One view of `blocks` blocks of a pagefile section, told as the
    /// runtime tells it.
    fn one_view(blocks: usize) -> usize {
        use windows_sys::Win32::System::Memory::{CreateFileMappingW, FILE_MAP_WRITE, MapViewOfFile};
        // SAFETY: a pagefile section, mapped read-write once.
        unsafe {
            let section = CreateFileMappingW(
                -1isize as _,
                std::ptr::null(),
                PAGE_READWRITE,
                0,
                (blocks * BLOCK) as u32,
                std::ptr::null(),
            );
            assert!(!section.is_null());
            let view = MapViewOfFile(section, FILE_MAP_WRITE, 0, 0, blocks * BLOCK).Value as usize;
            assert_ne!(view, 0);
            title_memory_view(view, blocks * BLOCK, section as usize, 0);
            unpin(view, blocks);
            view
        }
    }

    /// Lets go of pins over `[view, view + blocks)`: another test's pins
    /// last two seconds, and the address may be one it unmapped.
    fn unpin(view: usize, blocks: usize) {
        let (from, to) = (view / BLOCK, view / BLOCK + blocks);
        guards().pins.retain(|&(a, b, _)| b <= from || to <= a);
    }

    fn drop_view(view: usize, blocks: usize) {
        use windows_sys::Win32::System::Memory::{MEMORY_MAPPED_VIEW_ADDRESS, UnmapViewOfFile};
        unpin(view, blocks);
        title_memory_remapped(view, blocks * BLOCK);
        title_memory_view_gone(view, blocks * BLOCK);
        // SAFETY: our view.
        unsafe { UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS { Value: view as *mut _ }) };
    }

    /// The kernel writes `bytes` at `at` (`ReadFile` from a temporary
    /// file): true if it could, which it cannot into a read-only block.
    fn kernel_reads_into(at: usize, bytes: &[u8]) -> bool {
        use std::io::{Read, Seek, Write};
        let mut file = tempfile::tempfile().unwrap();
        file.write_all(bytes).unwrap();
        file.rewind().unwrap();
        // SAFETY: test memory of ours, nothing else refers to it.
        let into = unsafe { std::slice::from_raw_parts_mut(at as *mut u8, bytes.len()) };
        file.read(into).is_ok_and(|n| n == bytes.len())
    }

    /// The pages of blocks `[from, to)` of `view`.
    fn pages_of(view: usize, from: usize, to: usize) -> Vec<usize> {
        ((view + from * BLOCK) / PAGE..(view + to * BLOCK) / PAGE).collect()
    }

    /// A block pinned for a kernel write in the middle of a range whose
    /// first block is guarded (the check's fast path) stays writable and is
    /// reported written at every check, also where a check covers only part
    /// of the pin; the blocks around it are guarded as before.
    #[test]
    fn a_pinned_block_inside_a_guarded_range_stays_writable() {
        let view = one_view(4);
        assert_eq!(guard(view, view + 4 * BLOCK), Some(pages_of(view, 0, 4)));
        assert_eq!(guard(view, view + 4 * BLOCK), Some(Vec::new()), "nothing written yet");
        // Blocks 1 and 2 are about to be written by the kernel.
        writing(view + BLOCK + 100, BLOCK);
        // Block 0 guarded: the fast path, with the pin inside.
        assert_eq!(guard(view, view + 4 * BLOCK), Some(pages_of(view, 1, 3)));
        assert!(kernel_reads_into(view + BLOCK + 100, b"from the disk"), "pinned blocks stay writable");
        assert!(kernel_reads_into(view + 2 * BLOCK + 8, b"and more"), "pinned blocks stay writable");
        // Reported again while pinned, here over half of the pin.
        assert_eq!(guard(view, view + 2 * BLOCK), Some(pages_of(view, 1, 2)));
        assert_eq!(guard(view, view + 4 * BLOCK), Some(pages_of(view, 1, 3)));
        {
            let g = guards();
            let at = view / BLOCK;
            assert!(g.protected.contains(&at) && g.protected.contains(&(at + 3)));
            assert!(!g.protected.contains(&(at + 1)) && !g.protected.contains(&(at + 2)));
            assert!(!g.clean.contains(&(at + 1)) && !g.clean.contains(&(at + 2)));
        }
        // The blocks around it are still guarded: a store faults, is
        // opened by the handler and is reported.
        // SAFETY: our view; the handler opens it on the fault.
        unsafe { ((view + 3 * BLOCK + 4) as *mut u8).write(9) };
        assert_eq!(guard(view, view + 4 * BLOCK), Some([pages_of(view, 1, 3), pages_of(view, 3, 4)].concat()));
        // SAFETY: the bytes the kernel put in.
        assert_eq!(unsafe { std::slice::from_raw_parts((view + BLOCK + 100) as *const u8, 13) }, b"from the disk");
        drop_view(view, 4);
    }

    /// A kernel write announced at one view pins the page at its other
    /// views too: a check of another view, by its fast path, leaves it
    /// writable there and reports it.
    #[test]
    fn a_pinned_page_stays_writable_at_its_other_views() {
        let (one, two) = two_views();
        unpin(one, 2);
        unpin(two, 2);
        // Guarding the first view makes the second read-only too.
        assert_eq!(guard(one, one + 2 * BLOCK), Some(pages_of(one, 0, 2)));
        assert_eq!(guard(two, two + 2 * BLOCK), Some(pages_of(two, 0, 2)));
        assert_eq!(guard(two, two + 2 * BLOCK), Some(Vec::new()));
        writing(one + BLOCK + 100, 13);
        assert_eq!(guard(two, two + 2 * BLOCK), Some(pages_of(two, 1, 2)));
        assert_eq!(guard(one, one + 2 * BLOCK), Some(pages_of(one, 1, 2)));
        assert!(kernel_reads_into(two + BLOCK + 100, b"through two"), "the alias stays writable");
        assert!(kernel_reads_into(one + BLOCK + 200, b"through one"), "the view stays writable");
        assert_eq!(guard(two, two + 2 * BLOCK), Some(pages_of(two, 1, 2)));
        // SAFETY: the bytes the kernel put in, through the other view.
        assert_eq!(unsafe { std::slice::from_raw_parts((one + BLOCK + 100) as *const u8, 11) }, b"through two");
        unpin(one, 2);
        unpin(two, 2);
        drop_views(&[one, two]);
    }

    /// A pin that ended is let go by the next check, whichever path it
    /// takes: the block is guarded again and reported once, then clean.
    #[test]
    fn an_ended_pin_is_let_go_and_its_block_guarded_again() {
        let view = one_view(3);
        assert_eq!(guard(view, view + 3 * BLOCK), Some(pages_of(view, 0, 3)));
        writing(view + BLOCK, 8);
        let at = view / BLOCK + 1;
        {
            let mut g = guards();
            let now = std::time::Instant::now();
            for pin in g.pins.iter_mut().filter(|p| p.0 <= at && at < p.1) {
                pin.2 = Until::Time(now);
            }
        }
        // Fast path (block 0 guarded): the pin is gone, the block guarded.
        assert_eq!(guard(view, view + 3 * BLOCK), Some(pages_of(view, 1, 2)));
        assert!(!guards().pinned(at));
        assert_eq!(guard(view, view + 3 * BLOCK), Some(Vec::new()));
        assert!(!kernel_reads_into(view + BLOCK, b"late"), "guarded again once the pin ended");
        drop_view(view, 3);
    }

    /// Whether any pin holds a block of `[view, view + blocks)`.
    fn pinned_in(view: usize, blocks: usize) -> bool {
        let g = guards();
        (view / BLOCK..view / BLOCK + blocks).any(|b| g.pinned(b))
    }

    /// A write that ends with its caller pins its blocks while it runs
    /// (reported, writable, in a range checked by the fast path) and lets
    /// go when it is dropped: the next check guards them again and reports
    /// them once.
    #[test]
    fn a_held_pin_lasts_as_long_as_its_write() {
        let view = one_view(3);
        assert_eq!(guard(view, view + 3 * BLOCK), Some(pages_of(view, 0, 3)));
        {
            let _pinned = writing_held(view + BLOCK + 100, 13);
            assert!(pinned_in(view, 3));
            assert_eq!(guard(view, view + 3 * BLOCK), Some(pages_of(view, 1, 2)));
            assert!(kernel_reads_into(view + BLOCK + 100, b"from the disk"));
            assert_eq!(guard(view, view + 3 * BLOCK), Some(pages_of(view, 1, 2)));
        }
        assert!(!pinned_in(view, 3), "no pin outlives its write");
        // Guarded again, and reported once more: the write may have landed
        // after the last check.
        assert_eq!(guard(view, view + 3 * BLOCK), Some(pages_of(view, 1, 2)));
        assert_eq!(guard(view, view + 3 * BLOCK), Some(Vec::new()));
        assert!(!kernel_reads_into(view + BLOCK + 100, b"late"), "guarded again");
        // SAFETY: the bytes the kernel put in.
        assert_eq!(unsafe { std::slice::from_raw_parts((view + BLOCK + 100) as *const u8, 13) }, b"from the disk");
        drop_view(view, 3);
    }

    /// A held pin is let go when its write ends early, by a return or a
    /// panic.
    #[test]
    fn a_held_pin_is_let_go_when_its_write_ends_early() {
        let view = one_view(2);
        assert_eq!(guard(view, view + 2 * BLOCK), Some(pages_of(view, 0, 2)));
        let early = |fail: bool| -> Option<()> {
            let _pinned = writing_held(view + 8, 8);
            if fail {
                return None;
            }
            Some(())
        };
        assert_eq!(early(true), None);
        assert!(!pinned_in(view, 2));
        let panicked = std::panic::catch_unwind(|| {
            let _pinned = writing_held(view + BLOCK, 8);
            panic!("a failed write");
        });
        assert!(panicked.is_err());
        assert!(!pinned_in(view, 2));
        assert_eq!(guard(view, view + 2 * BLOCK), Some(pages_of(view, 0, 2)));
        assert_eq!(guard(view, view + 2 * BLOCK), Some(Vec::new()));
        drop_view(view, 2);
    }

    /// An asynchronous read keeps its blocks pinned after the call that
    /// began it returned, until its `OVERLAPPED` says it completed.
    #[test]
    fn an_asynchronous_read_is_pinned_until_it_completed() {
        let view = one_view(3);
        assert_eq!(guard(view, view + 3 * BLOCK), Some(pages_of(view, 0, 3)));
        // Internal, InternalHigh, offsets, event.
        let overlapped = Box::new(std::sync::atomic::AtomicUsize::new(STATUS_PENDING));
        writing_held(view + BLOCK, 64).until_done(std::ptr::from_ref(&*overlapped).cast());
        assert_eq!(guard(view, view + 3 * BLOCK), Some(pages_of(view, 1, 2)));
        assert!(pinned_in(view, 3), "still in flight");
        assert!(kernel_reads_into(view + BLOCK, b"lands later"), "writable while in flight");
        assert_eq!(guard(view, view + 3 * BLOCK), Some(pages_of(view, 1, 2)));
        // Completed: the next check lets go, guards and reports it.
        overlapped.store(0, std::sync::atomic::Ordering::Release);
        assert_eq!(guard(view, view + 3 * BLOCK), Some(pages_of(view, 1, 2)));
        assert!(!pinned_in(view, 3));
        assert_eq!(guard(view, view + 3 * BLOCK), Some(Vec::new()));
        assert!(!kernel_reads_into(view + BLOCK, b"late"), "guarded again");
        drop_view(view, 3);
    }

    /// An `OVERLAPPED` whose memory is let go reads as completed: the title
    /// frees it only once its read completed.
    #[test]
    fn an_asynchronous_read_whose_overlapped_is_gone_is_done() {
        let view = one_view(2);
        assert_eq!(guard(view, view + 2 * BLOCK), Some(pages_of(view, 0, 2)));
        // SAFETY: a page of our own, released below.
        let page = unsafe { VirtualAlloc(std::ptr::null(), PAGE, MEM_COMMIT | MEM_RESERVE, PAGE_READWRITE) } as usize;
        assert_ne!(page, 0);
        // SAFETY: the page just committed.
        unsafe { (page as *mut usize).write(STATUS_PENDING) };
        writing_held(view + 8, 8).until_done(page as *const _);
        assert_eq!(guard(view, view + 2 * BLOCK), Some(pages_of(view, 0, 1)));
        assert!(pinned_in(view, 2));
        // SAFETY: our page.
        unsafe { VirtualFree(page as *mut _, 0, MEM_RELEASE) };
        assert_eq!(guard(view, view + 2 * BLOCK), Some(pages_of(view, 0, 1)));
        assert!(!pinned_in(view, 2));
        drop_view(view, 2);
    }

    /// Kernel calls for a read that stays in flight until we write the
    /// other end of a pipe (declared here: the crate's bindings have no
    /// pipes or files).
    mod pipe {
        use std::ffi::c_void;
        #[link(name = "kernel32")]
        unsafe extern "system" {
            pub fn CreateNamedPipeW(
                name: *const u16,
                open: u32,
                mode: u32,
                instances: u32,
                out: u32,
                inb: u32,
                timeout: u32,
                security: *const c_void,
            ) -> isize;
            pub fn CreateFileW(
                name: *const u16,
                access: u32,
                share: u32,
                security: *const c_void,
                disposition: u32,
                flags: u32,
                template: isize,
            ) -> isize;
            pub fn ReadFile(file: isize, buffer: *mut u8, len: u32, read: *mut u32, overlapped: *mut c_void) -> i32;
            pub fn WriteFile(file: isize, buffer: *const u8, len: u32, written: *mut u32, overlapped: *mut c_void) -> i32;
            pub fn GetOverlappedResult(file: isize, overlapped: *mut c_void, done: *mut u32, wait: i32) -> i32;
            pub fn CloseHandle(handle: isize) -> i32;
            pub fn GetLastError() -> u32;
        }
        pub const PIPE_ACCESS_INBOUND: u32 = 1;
        pub const FILE_FLAG_OVERLAPPED: u32 = 0x4000_0000;
        pub const GENERIC_WRITE: u32 = 0x4000_0000;
        pub const OPEN_EXISTING: u32 = 3;
        pub const ERROR_IO_PENDING: u32 = 997;

        /// A pipe whose reading end reads asynchronously, and its writing end.
        pub fn open(tag: &str) -> (isize, isize) {
            let name: Vec<u16> = format!(r"\\.\pipe\victory-pin-{}-{tag}", std::process::id())
                .encode_utf16()
                .chain([0])
                .collect();
            // SAFETY: a NUL-terminated name; handles checked below.
            unsafe {
                let read = CreateNamedPipeW(
                    name.as_ptr(),
                    PIPE_ACCESS_INBOUND | FILE_FLAG_OVERLAPPED,
                    0,
                    1,
                    0,
                    4096,
                    0,
                    std::ptr::null(),
                );
                assert!(read != -1);
                let write = CreateFileW(name.as_ptr(), GENERIC_WRITE, 0, std::ptr::null(), OPEN_EXISTING, 0, 0);
                assert!(write != -1);
                (read, write)
            }
        }
    }

    /// The runtime's `ReadFile` on a read that goes on asynchronously: a
    /// pin until the `OVERLAPPED` completed. The kernel's write lands in a
    /// guarded view after the call returned and after checks of its range
    /// (by the fast path), the bytes are there, and the block is guarded
    /// again and reported once the read completed.
    #[test]
    fn a_pending_kernel_read_lands_in_a_guarded_view() {
        let view = one_view(2);
        assert_eq!(guard(view, view + 2 * BLOCK), Some(pages_of(view, 0, 2)));
        let (read, write) = pipe::open("lands");
        let at = view + BLOCK + 100;
        // Internal, InternalHigh, Offset and OffsetHigh, hEvent.
        let mut overlapped = Box::new([0usize; 4]);
        let ov = overlapped.as_mut_ptr().cast::<std::ffi::c_void>();
        // As files::ReadFile does it.
        let pinned = writing_held(at, 13);
        // SAFETY: 13 bytes of our view; the OVERLAPPED lives to the end.
        let done = unsafe { pipe::ReadFile(read, at as *mut u8, 13, std::ptr::null_mut(), ov) };
        // SAFETY: plain query.
        let error = unsafe { pipe::GetLastError() };
        assert!(done == 0 && error == pipe::ERROR_IO_PENDING, "in flight: {done} {error}");
        pinned.until_done(ov);
        // Checks while it is in flight: writable, reported each time.
        for _ in 0..3 {
            assert_eq!(guard(view, view + 2 * BLOCK), Some(pages_of(view, 1, 2)));
            assert!(pinned_in(view, 2), "pinned while in flight");
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        // The other end writes: the kernel completes the read into the view.
        let mut n = 0;
        // SAFETY: 13 bytes of a literal.
        assert!(unsafe { pipe::WriteFile(write, b"from the pipe".as_ptr(), 13, &mut n, std::ptr::null_mut()) } != 0);
        // SAFETY: the read's OVERLAPPED; waits for its completion.
        let completed = unsafe { pipe::GetOverlappedResult(read, ov, &mut n, 1) };
        assert!(completed != 0 && n == 13, "the read completed: {completed} {n}");
        // SAFETY: the bytes the kernel put in.
        assert_eq!(unsafe { std::slice::from_raw_parts(at as *const u8, 13) }, b"from the pipe");
        // The next check lets go of the pin, guards the block and reports it.
        assert_eq!(guard(view, view + 2 * BLOCK), Some(pages_of(view, 1, 2)));
        assert!(!pinned_in(view, 2));
        assert_eq!(guard(view, view + 2 * BLOCK), Some(Vec::new()));
        assert!(!kernel_reads_into(at, b"late"), "guarded again");
        // SAFETY: our handles.
        unsafe {
            pipe::CloseHandle(write);
            pipe::CloseHandle(read);
        }
        drop(overlapped);
        drop_view(view, 2);
    }

    /// Without the pin, the same pending read fails when it completes into
    /// a block guarded meanwhile: what the pin is for.
    #[test]
    fn a_pending_kernel_read_fails_into_a_block_guarded_meanwhile() {
        let view = one_view(2);
        assert_eq!(guard(view, view + 2 * BLOCK), Some(pages_of(view, 0, 2)));
        let (read, write) = pipe::open("fails");
        let at = view + BLOCK + 100;
        let mut overlapped = Box::new([0usize; 4]);
        let ov = overlapped.as_mut_ptr().cast::<std::ffi::c_void>();
        // Opened for the call only, as a pin held by the call would.
        {
            let _pinned = writing_held(at, 13);
            // SAFETY: as above.
            let done = unsafe { pipe::ReadFile(read, at as *mut u8, 13, std::ptr::null_mut(), ov) };
            // SAFETY: plain query.
            assert!(done == 0 && unsafe { pipe::GetLastError() } == pipe::ERROR_IO_PENDING);
        }
        // A check guards the block again while the read is in flight.
        assert_eq!(guard(view, view + 2 * BLOCK), Some(pages_of(view, 1, 2)));
        let mut n = 0;
        // SAFETY: as above.
        assert!(unsafe { pipe::WriteFile(write, b"from the pipe".as_ptr(), 13, &mut n, std::ptr::null_mut()) } != 0);
        // SAFETY: as above.
        let completed = unsafe { pipe::GetOverlappedResult(read, ov, &mut n, 1) };
        assert_eq!(completed, 0, "the kernel could not write a guarded block");
        // SAFETY: our handles.
        unsafe {
            pipe::CloseHandle(write);
            pipe::CloseHandle(read);
        }
        drop(overlapped);
        drop_view(view, 2);
    }

    /// A kernel read into a guarded view block fails; opened first, it
    /// lands and the block counts as written.
    #[test]
    fn kernel_reads_into_guarded_blocks_after_opening() {
        use std::io::{Read, Seek, Write};
        use windows_sys::Win32::System::Memory::{
            CreateFileMappingW, FILE_MAP_WRITE, MapViewOfFile, UnmapViewOfFile,
        };
        // SAFETY: a pagefile section of two blocks, mapped read-write.
        let view = unsafe {
            let section = CreateFileMappingW(
                -1isize as _,
                std::ptr::null(),
                PAGE_READWRITE,
                0,
                (2 * BLOCK) as u32,
                std::ptr::null(),
            );
            assert!(!section.is_null());
            MapViewOfFile(section, FILE_MAP_WRITE, 0, 0, 2 * BLOCK).Value as usize
        };
        assert_ne!(view, 0);
        assert!(guard(view, view + 2 * BLOCK).is_some()); // now read-only
        let mut file = tempfile::tempfile().unwrap();
        file.write_all(b"from the disk").unwrap();
        // `ReadFile` inside: the kernel writes the view.
        let read = |file: &mut std::fs::File| {
            file.rewind().unwrap();
            // SAFETY: 13 bytes of our view, nothing else refers to them.
            let into = unsafe { std::slice::from_raw_parts_mut((view + 100) as *mut u8, 13) };
            file.read(into).is_ok_and(|n| n == 13)
        };
        assert!(!read(&mut file), "the kernel does not fault into our handler");
        writing(view + 100, 13);
        assert!(read(&mut file));
        // SAFETY: the bytes just read.
        assert_eq!(unsafe { std::slice::from_raw_parts((view + 100) as *const u8, 13) }, b"from the disk");
        assert_eq!(guard(view, view + 2 * BLOCK), Some((view / PAGE..view / PAGE + 1).collect()));
        // SAFETY: our view.
        unsafe { UnmapViewOfFile(windows_sys::Win32::System::Memory::MEMORY_MAPPED_VIEW_ADDRESS { Value: view as *mut _ }) };
        title_memory_remapped(view, 2 * BLOCK);
    }
}
