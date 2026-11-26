//! Diagnostics only: evidence for two candidate optimizations, collected
//! without changing what the runtime does. Both switches are off by default
//! and exist only in diagnostics builds. With them on, the counting takes
//! locks on the submitting and recording threads: frame times of such a run
//! are not a benchmark.
//!
//! `VICTORY_STRETCH_AUDIT=<folder>` (O1, the idea of Belobug 0434b3ac):
//! which refreshed ranges of title memory are made of 64 KiB views that
//! follow one another in one section, so that the stretch could be mapped
//! once more at an address of ours and used in place. Per range refreshed
//! (`crate::host::update` and the bindless refreshes): refreshes, bytes
//! asked, bytes copied, time. Every fourth frame-rate line, for the twenty
//! that took most time: the views the runtime was told of
//! (`crate::watch::audit_layout`, read only), gaps, sections, runs of
//! section offsets, physical aliases, allocation bases, and once per range
//! a probe: the section's stretch mapped at another address and opened as a
//! host heap, both released at once (`probe`). The views of each range go
//! to `<folder>/stretch-<start>-<len>.csv` once.
//!
//! `VICTORY_WRITE_AUDIT=1` (O2, the idea of DexNom bc3cd4b0): which
//! declarations of GPU-written ranges (table UAVs, root UAVs, query
//! resolves) lead to copy-back tiles that differ. A declaration is tagged
//! with the pipeline bound on its thread (`bound`) and the resource
//! (`resource`); `Shadow::sparse_back` reports per call the tiles it looked
//! at, those that differed, the bytes its merges wrote (a span: an upper
//! bound of the bytes that differ) and tiles already compared in the same
//! submission. Both are joined by (address, length), which is how a
//! declared range reaches `sparse_back`. Nothing is compared again: the
//! counts are the comparisons `sparse_back` makes anyway.

use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

fn held<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

// ---------------------------------------------------------------- O1 ----

/// `VICTORY_STRETCH_AUDIT`: the folder for the views' lists.
fn stretch_dir() -> Option<&'static PathBuf> {
    static DIR: OnceLock<Option<PathBuf>> = OnceLock::new();
    DIR.get_or_init(|| {
        let dir = std::env::var_os("VICTORY_STRETCH_AUDIT").filter(|v| !v.is_empty()).map(PathBuf::from);
        if let Some(d) = &dir {
            let _ = std::fs::create_dir_all(d);
            tracing::warn!(folder = %d.display(), "VICTORY_STRETCH_AUDIT: refreshed ranges and their views logged (diagnostics; times not a benchmark)");
        }
        dir
    })
    .as_ref()
}

pub(crate) fn stretch_on() -> bool {
    stretch_dir().is_some()
}

#[derive(Default, Clone, Copy)]
struct Refresh {
    calls: u64,
    asked: u64,
    copied: u64,
    us: u64,
}

static REFRESHES: Mutex<Option<HashMap<(usize, usize), Refresh>>> = Mutex::new(None);
static REFRESH_FROM: AtomicU64 = AtomicU64::new(0);

/// A refresh of `[from, from + len)` copied `copied` bytes in `us`.
pub(crate) fn refreshed(from: usize, len: usize, copied: usize, us: u64) {
    let mut map = held(&REFRESHES);
    let e = map.get_or_insert_with(HashMap::new).entry((from, len)).or_default();
    e.calls += 1;
    e.asked += len as u64;
    e.copied += copied as u64;
    e.us += us;
}

/// Probes already made, by range: the outcome.
static PROBED: Mutex<Option<HashMap<(usize, usize), String>>> = Mutex::new(None);

/// What `MapViewOfFile` of the section's stretch and
/// `OpenExistingHeapFromAddress1` on it say; both undone at once.
fn probe(section: usize, offset: u64, len: usize) -> String {
    use windows::Win32::Graphics::Direct3D12::{ID3D12Device13, ID3D12Heap};
    use windows::core::Interface;
    use windows_sys::Win32::System::Memory::{FILE_MAP_WRITE, MEMORY_MAPPED_VIEW_ADDRESS, MapViewOfFile, UnmapViewOfFile};
    // SAFETY: a section handle the runtime keeps open; the view is ours and
    // unmapped below, nothing is written through it.
    let view = unsafe { MapViewOfFile(section as _, FILE_MAP_WRITE, (offset >> 32) as u32, offset as u32, len) };
    if view.Value.is_null() {
        return format!("map refused ({})", std::io::Error::last_os_error());
    }
    let device: Option<ID3D12Device13> = crate::host::host().and_then(|h| h.device.cast().ok());
    let heap = match &device {
        // SAFETY: committed memory of our view, released before the unmap.
        Some(d) => match unsafe { d.OpenExistingHeapFromAddress1::<ID3D12Heap>(view.Value, len) } {
            Ok(heap) => {
                drop(heap);
                "mapped, heap opened".to_string()
            }
            Err(e) => format!("mapped, heap refused ({e})"),
        },
        None => "mapped, no device to ask".to_string(),
    };
    // SAFETY: the view mapped above.
    unsafe { UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS { Value: view.Value }) };
    heap
}

/// Bytes of a refresh range of `n` refreshes over `frames` frames.
fn per_frame(v: u64, frames: f64) -> f64 {
    v as f64 / frames
}

fn stretch_report() {
    let Some(dir) = stretch_dir() else { return };
    let now = crate::queue::presented();
    let frames = now.saturating_sub(REFRESH_FROM.swap(now, Ordering::Relaxed)).max(1) as f64;
    let map = held(&REFRESHES).take().unwrap_or_default();
    let mut ranges: Vec<_> = map.into_iter().collect();
    let total: Refresh = ranges.iter().fold(Refresh::default(), |a, (_, r)| Refresh {
        calls: a.calls + r.calls,
        asked: a.asked + r.asked,
        copied: a.copied + r.copied,
        us: a.us + r.us,
    });
    tracing::info!(
        "stretch audit over {frames:.0} frames, per frame: {:.0} refreshes of {} ranges, {:.1} MB asked, {:.1} MB copied, {:.1} ms",
        per_frame(total.calls, frames),
        ranges.len(),
        per_frame(total.asked, frames) / 1e6,
        per_frame(total.copied, frames) / 1e6,
        per_frame(total.us, frames) / 1e3
    );
    ranges.sort_by_key(|&(_, r)| std::cmp::Reverse((r.us, r.copied)));
    let mut eligible = Refresh::default();
    for (rank, ((start, len), r)) in ranges.into_iter().take(20).enumerate() {
        let l = crate::watch::audit_layout(start, len);
        let end = start + len;
        // What Belobug's `watch::stretch` and `through_another_view` ask:
        // one run of views of one section, covering the range, in one
        // 1 GiB window, at least 4 MiB; then the map and the heap.
        const REACH: usize = 1 << 30;
        const LEAST: usize = 4 << 20;
        let low = start - start % REACH;
        let (verdict, why) = if l.views == 0 {
            if l.mapped {
                ("UNKNOWN", "section memory, but no views known (VICTORY_VIEW_GUARD=0?)".to_string())
            } else {
                ("INELIGIBLE", "not a view of a section (private memory)".to_string())
            }
        } else if l.covered < len {
            ("INELIGIBLE", format!("views cover {} of {len} bytes (gaps {})", l.covered, l.gaps))
        } else if l.sections > 1 {
            ("INELIGIBLE", format!("{} sections", l.sections))
        } else if l.runs > 1 {
            ("INELIGIBLE", format!("{} runs of section offsets (views not in order in the section)", l.runs))
        } else if end > low + REACH {
            ("INELIGIBLE", "crosses a 1 GiB window".to_string())
        } else if len < LEAST {
            ("INELIGIBLE", "under 4 MiB".to_string())
        } else {
            let outcome = held(&PROBED)
                .get_or_insert_with(HashMap::new)
                .entry((start, len))
                .or_insert_with(|| probe(l.section, l.first_offset + (start - l.first_view) as u64, len))
                .clone();
            if outcome == "mapped, heap opened" {
                ("ELIGIBLE", format!("one run in one section; probe: {outcome}"))
            } else {
                ("INELIGIBLE", format!("probe: {outcome}"))
            }
        };
        if verdict == "ELIGIBLE" {
            eligible.calls += r.calls;
            eligible.asked += r.asked;
            eligible.copied += r.copied;
            eligible.us += r.us;
        }
        tracing::info!(
            "  stretch #{rank} {start:#x}..{end:#x} ({:.2} MiB): {:.1} refreshes/frame, {:.1} MB asked, {:.1} MB copied, \
             {:.2} ms per frame; {} views (first {:#x}+{:#x}, last {:#x}+{:#x}), gaps {} B, {} section(s), {} run(s), \
             {} of {} blocks with other views ({} other addresses), allocation base same at both ends: {}; {verdict}: {why}",
            len as f64 / 1048576.0,
            per_frame(r.calls, frames),
            per_frame(r.asked, frames) / 1e6,
            per_frame(r.copied, frames) / 1e6,
            per_frame(r.us, frames) / 1e3,
            l.views,
            l.first_view,
            l.first_len,
            l.last_view,
            l.last_len,
            l.gaps,
            l.sections,
            l.runs,
            l.aliased_blocks,
            len.div_ceil(crate::watch::PAGE),
            l.alias_addresses,
            l.same_base,
        );
        let csv = dir.join(format!("stretch-{start:x}-{len:x}.csv"));
        if !csv.exists() {
            let mut text = String::from("view,len,offset_in_range,section,section_offset,follows_previous,other_views_of_first_block\n");
            for v in &l.list {
                text.push_str(&format!(
                    "{:#x},{:#x},{:#x},{:#x},{:#x},{},{}\n",
                    v.0,
                    v.1,
                    v.0.saturating_sub(start),
                    v.2,
                    v.3,
                    v.4,
                    v.5
                ));
            }
            match victory_core::ensure_outside_repo(&csv) {
                Err(e) => tracing::warn!(error = %e, "stretch audit list not written"),
                Ok(()) => {
                    if let Err(e) = std::fs::write(&csv, text) {
                        tracing::warn!(error = %e, "stretch audit list not written");
                    }
                }
            }
        }
    }
    tracing::info!(
        "stretch audit: ELIGIBLE among the twenty, per frame: {:.1} refreshes, {:.1} MB asked, {:.1} MB copied, {:.1} ms",
        per_frame(eligible.calls, frames),
        per_frame(eligible.asked, frames) / 1e6,
        per_frame(eligible.copied, frames) / 1e6,
        per_frame(eligible.us, frames) / 1e3
    );
}

// ---------------------------------------------------------------- O2 ----

pub(crate) fn write_on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        let on = std::env::var("VICTORY_WRITE_AUDIT").is_ok_and(|v| v.trim() == "1");
        if on {
            tracing::warn!("VICTORY_WRITE_AUDIT=1: declared GPU writes joined with copy-back tiles (diagnostics; times not a benchmark)");
        }
        on
    })
}

/// A pipeline as declarations see it.
struct Pipe {
    name: String,
    /// Some stage may write memory (`Translated::writes_memory`).
    writes_memory: bool,
    /// From the console code of its stages: stores, atomics, code not all
    /// known (`crate::pipeline::memory_ops`).
    stores: bool,
    atomics: bool,
    unknown: bool,
}

static PIPES: Mutex<Option<HashMap<usize, Arc<Pipe>>>> = Mutex::new(None);

thread_local! {
    static PIPE: Cell<usize> = const { Cell::new(0) };
    static RESOURCE: Cell<(u8, u32)> = const { Cell::new((0, 0)) };
}

/// The pipeline a draw or dispatch on this thread binds.
pub(crate) fn bound(p: &crate::pipeline::host::HostPipeline) {
    use windows::core::Interface;
    let key = p.pso.as_raw() as usize;
    PIPE.with(|c| c.set(key));
    let mut pipes = held(&PIPES);
    let pipes = pipes.get_or_insert_with(HashMap::new);
    if !pipes.contains_key(&key) {
        let (mut stores, mut atomics, mut unknown) = (false, false, false);
        for s in &p.stages {
            let (st, at, un) = crate::pipeline::memory_ops(&s.shader);
            stores |= st;
            atomics |= at;
            unknown |= un;
        }
        pipes.insert(
            key,
            Arc::new(Pipe {
                name: p.name.clone(),
                writes_memory: p.stages.iter().any(|s| s.translated().writes_memory),
                stores,
                atomics,
                unknown,
            }),
        );
    }
}

/// The resource the next declaration on this thread is for: 1 a table UAV
/// (`slot`: its shader register), 2 a root UAV (`slot`: 0), 3 a query
/// resolve.
pub(crate) fn resource(kind: u8, slot: u32) {
    RESOURCE.with(|c| c.set((kind, slot)));
}

/// Who declared: the pipeline, the kind of resource, its slot.
type Declarer = (usize, u8, u32);

#[derive(Default)]
struct Decl {
    count: u64,
    frames: u64,
    last_frame: u64,
    ranges: HashSet<(usize, usize)>,
}

#[derive(Default)]
struct Declared {
    by: HashMap<Declarer, Decl>,
    of_range: HashMap<(usize, usize), Vec<Declarer>>,
    first_frame: u64,
}

static DECLARED: Mutex<Option<Declared>> = Mutex::new(None);

/// `Shadow::gpu_writes` declared `[address, address + len)` (after its own
/// cut to the copy's end).
pub(crate) fn declared(address: usize, len: usize) {
    let who: Declarer = (PIPE.with(Cell::get), RESOURCE.with(Cell::get).0, RESOURCE.with(Cell::get).1);
    RESOURCE.with(|c| c.set((0, 0)));
    let frame = crate::queue::presented();
    let mut d = held(&DECLARED);
    let d = d.get_or_insert_with(|| Declared { first_frame: frame, ..Default::default() });
    let e = d.by.entry(who).or_default();
    e.count += 1;
    if e.frames == 0 || e.last_frame != frame {
        e.frames += 1;
        e.last_frame = frame;
    }
    if e.ranges.insert((address, len)) {
        let list = d.of_range.entry((address, len)).or_default();
        if !list.contains(&who) {
            list.push(who);
        }
    }
}

#[derive(Default, Clone, Copy)]
struct Back {
    calls: u64,
    tiles: u64,
    differ: u64,
    examined: u64,
    written: u64,
    repeated: u64,
    frames: u64,
    frames_differ: u64,
    last_frame: u64,
    last_differ_frame: u64,
}

/// The size of a sparse copy's tile (`crate::shadow`'s `PAGE`).
const TILE: u64 = 64 << 10;

static BACKS: Mutex<Option<HashMap<(usize, usize), Back>>> = Mutex::new(None);
/// Tiles (copy, tile) compared since the last submission's copy-back.
static SEEN: Mutex<Option<HashSet<(usize, u32)>>> = Mutex::new(None);

/// `sparse_back` of `[address, address + len)` on the copy `root`: the
/// tiles it compared (`tiles`, by number), how many of them differed, the
/// bytes compared and the bytes its merges wrote.
pub(crate) fn back(root: usize, address: usize, len: usize, tiles: &[u32], differ: u64, examined: u64, written: u64) {
    let repeated = {
        let mut seen = held(&SEEN);
        let seen = seen.get_or_insert_with(HashSet::new);
        tiles.iter().filter(|&&t| !seen.insert((root, t))).count() as u64
    };
    let frame = crate::queue::presented();
    let mut b = held(&BACKS);
    let e = b.get_or_insert_with(HashMap::new).entry((address, len)).or_default();
    e.calls += 1;
    e.tiles += tiles.len() as u64;
    e.differ += differ;
    e.examined += examined;
    e.written += written;
    e.repeated += repeated;
    if e.frames == 0 || e.last_frame != frame {
        e.frames += 1;
        e.last_frame = frame;
    }
    if differ > 0 && (e.frames_differ == 0 || e.last_differ_frame != frame) {
        e.frames_differ += 1;
        e.last_differ_frame = frame;
    }
}

/// A submission's copy-backs are done: tiles compared from now on are not
/// repeats.
pub(crate) fn submission_done() {
    if let Some(seen) = held(&SEEN).as_mut() {
        seen.clear();
    }
}

fn kind_name(kind: u8) -> &'static str {
    match kind {
        1 => "table UAV",
        2 => "root UAV (1 MiB)",
        3 => "query resolve",
        _ => "other",
    }
}

fn write_report() {
    let frame = crate::queue::presented();
    let (by, of_range, first) = {
        let d = held(&DECLARED);
        let Some(d) = d.as_ref() else { return };
        (
            d.by.iter().map(|(k, v)| (*k, (v.count, v.frames, v.ranges.iter().copied().collect::<Vec<_>>()))).collect::<Vec<_>>(),
            d.of_range.clone(),
            d.first_frame,
        )
    };
    let backs = held(&BACKS).clone().unwrap_or_default();
    let pipes = held(&PIPES).clone().unwrap_or_default();
    let frames = frame.saturating_sub(first).max(1) as f64;
    let pipe = |k: usize| pipes.get(&k).cloned();
    let read_only = |w: &Declarer| pipe(w.0).is_some_and(|p| !p.writes_memory);
    // Ranges: what was compared, by what their declarers are.
    let (mut all, mut never, mut ro, mut repeated, mut undeclared) = (0u64, 0u64, 0u64, 0u64, 0u64);
    for (range, b) in &backs {
        all += b.examined;
        repeated += b.repeated * TILE;
        if b.frames_differ == 0 {
            never += b.examined;
        }
        match of_range.get(range) {
            Some(who) if who.iter().all(read_only) => ro += b.examined,
            Some(_) => {}
            None => undeclared += b.examined,
        }
    }
    let mb = |v: u64| v as f64 / frames / 1e6;
    tracing::info!(
        "write audit over {frames:.0} frames, copy-back per frame: {:.1} MB compared in {} ranges; {:.1} MB in ranges no tile of \
         which ever differed; {:.1} MB in ranges declared only by pipelines whose code has no store or atomic \
         (READ_ONLY_CONFIRMED); {:.1} MB of tiles compared again in the same submission (upper bound); {:.1} MB in ranges \
         with no declarer seen",
        mb(all),
        backs.len(),
        mb(never),
        mb(ro),
        mb(repeated),
        mb(undeclared)
    );
    // Declarers, by the bytes compared in their ranges.
    struct Row {
        who: Declarer,
        n_ranges: usize,
        count: u64,
        frames: u64,
        declared: u64,
        b: Back,
        shared: bool,
        seen: bool,
    }
    let mut rows: Vec<Row> = by
        .into_iter()
        .map(|(who, (count, frames, ranges))| {
            let mut b = Back::default();
            let (mut shared, mut seen) = (false, false);
            let mut declared = 0;
            for r in &ranges {
                declared += r.1 as u64;
                shared |= of_range.get(r).is_some_and(|w| w.len() > 1);
                if let Some(x) = backs.get(r) {
                    seen = true;
                    b.calls += x.calls;
                    b.tiles += x.tiles;
                    b.differ += x.differ;
                    b.examined += x.examined;
                    b.written += x.written;
                    b.repeated += x.repeated;
                    b.frames = b.frames.max(x.frames);
                    b.frames_differ = b.frames_differ.max(x.frames_differ);
                }
            }
            Row { who, n_ranges: ranges.len(), count, frames, declared, b, shared, seen }
        })
        .collect();
    rows.sort_by_key(|r| std::cmp::Reverse(r.b.examined));
    for r in rows.iter().take(30) {
        let p = pipe(r.who.0);
        let name = p.as_ref().map_or("?", |p| p.name.as_str());
        let class = match &p {
            Some(p) if !p.writes_memory => "READ_ONLY_CONFIRMED",
            _ if !r.seen => "UNKNOWN_WRITE_BEHAVIOR (not copied back here)",
            _ if r.b.differ > 0 => "OBSERVED_DIFFERENCE",
            _ => "NO_OBSERVED_DIFFERENCE",
        };
        let code = p.as_ref().map_or("code ?".to_string(), |p| {
            format!(
                "code: stores {} atomics {} unknown {}",
                u8::from(p.stores),
                u8::from(p.atomics),
                u8::from(p.unknown)
            )
        });
        let same = if r.b.tiles > 0 {
            100.0 * (r.b.tiles - r.b.differ) as f64 / r.b.tiles as f64
        } else {
            0.0
        };
        tracing::info!(
            "  write {name} {} slot {}: {} declarations in {} frames, {} ranges {:.2} MiB declared; copy-back: {} frames seen, \
             {} with differences, {} tiles ({:.1} MB/frame), {} differed ({same:.1}% same), {:.2} MB/frame written (span), \
             {} tiles repeated in a submission; {}{}; {class}",
            kind_name(r.who.1),
            r.who.2,
            r.count,
            r.frames,
            r.n_ranges,
            r.declared as f64 / 1048576.0,
            r.b.frames,
            r.b.frames_differ,
            r.b.tiles,
            mb(r.b.examined),
            r.b.differ,
            mb(r.b.written),
            r.b.repeated,
            code,
            if r.shared { "; a range shared with other declarers" } else { "" },
        );
    }
    // Ranges: one line each for the twenty biggest, every range in a list
    // file next to the stretch audit's (VICTORY_STRETCH_AUDIT) when set.
    let mut ranges: Vec<(&(usize, usize), &Back)> = backs.iter().collect();
    ranges.sort_by_key(|(_, b)| std::cmp::Reverse(b.examined));
    let describe = |range: &(usize, usize)| {
        let who = of_range.get(range).map(Vec::as_slice).unwrap_or(&[]);
        let mut names: Vec<String> = who
            .iter()
            .map(|w| {
                let p = pipe(w.0);
                format!(
                    "{} {} {}{}",
                    p.as_ref().map_or("?", |p| p.name.as_str()),
                    kind_name(w.1),
                    w.2,
                    p.as_ref().map_or("", |p| if p.atomics { " (atomics)" } else { "" })
                )
            })
            .collect();
        names.sort();
        names.dedup();
        let all_ro = !who.is_empty() && who.iter().all(read_only);
        (who.len(), all_ro, names.join(" | "))
    };
    let class_of = |b: &Back, n: usize, all_ro: bool| {
        if all_ro {
            "READ_ONLY_CONFIRMED"
        } else if n == 0 {
            "UNKNOWN_WRITE_BEHAVIOR (no declarer seen)"
        } else if b.frames_differ > 0 {
            "OBSERVED_DIFFERENCE"
        } else {
            "NO_OBSERVED_DIFFERENCE"
        }
    };
    for (range, b) in ranges.iter().take(20) {
        let (n, all_ro, names) = describe(range);
        let same = if b.tiles > 0 { 100.0 * (b.tiles - b.differ) as f64 / b.tiles as f64 } else { 0.0 };
        tracing::info!(
            "  write range {:#x}+{:#x} ({:.2} MiB): {:.1} MB/frame compared, {} calls, {} frames seen, {} with differences,              {} tiles, {} differed ({same:.2}% same), {:.2} MB/frame written, {} repeated; {n} declarers; {}; [{names}]",
            range.0,
            range.1,
            range.1 as f64 / 1048576.0,
            mb(b.examined),
            b.calls,
            b.frames,
            b.frames_differ,
            b.tiles,
            b.differ,
            mb(b.written),
            b.repeated,
            class_of(b, n, all_ro),
        );
    }
    if let Some(dir) = stretch_dir() {
        let path = dir.join("write-ranges.csv");
        let mut text = String::from(
            "address,len,mb_per_frame_compared,calls,frames_seen,frames_with_differences,tiles,tiles_differed,             mb_per_frame_written,repeated,declarers,class,pipelines
",
        );
        for (range, b) in &ranges {
            let (n, all_ro, names) = describe(range);
            text.push_str(&format!(
                "{:#x},{:#x},{:.3},{},{},{},{},{},{:.3},{},{n},{},\"{names}\"
",
                range.0,
                range.1,
                mb(b.examined),
                b.calls,
                b.frames,
                b.frames_differ,
                b.tiles,
                b.differ,
                mb(b.written),
                b.repeated,
                class_of(b, n, all_ro),
            ));
        }
        match victory_core::ensure_outside_repo(&path) {
            Err(e) => tracing::warn!(error = %e, "write audit list not written"),
            Ok(()) => {
                if let Err(e) = std::fs::write(&path, text) {
                    tracing::warn!(error = %e, "write audit list not written");
                }
            }
        }
    }
}

// --------------------------------------------------------------------------

/// With the frame-rate line: every fourth one, what was gathered.
pub(crate) fn report() {
    if !stretch_on() && !write_on() {
        return;
    }
    static LINES: AtomicU64 = AtomicU64::new(0);
    if !LINES.fetch_add(1, Ordering::Relaxed).is_multiple_of(4) {
        return;
    }
    stretch_report();
    if write_on() {
        write_report();
    }
}
