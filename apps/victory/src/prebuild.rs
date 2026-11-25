//! The pipeline pre-build (`victory prebuild`): every pipeline of the
//! title's shader pack made ahead of a run into the host pipeline library
//! the run reads (`<cache>/pipelines`), its shaders translated into the
//! run's shader cache (`<cache>/shaders`), so that a first run, and a first
//! visit to any place, compiles nothing the pack lists. Ported from
//! dev-fxceless (Marvisak/VIctory 3001f5b, the crash journal of d47b441):
//! a run of this tree made some 900 pipelines while it played, even with
//! its library from an earlier run.
//!
//! Each pack record becomes the creation stream the engine would hand the
//! device for that pipeline (`victory_game::pipelines`), which goes through
//! the run's own code (`victory_gpu::pipeline::Prebuild`): a pipeline is
//! only ever used if its host description is the run's, byte for byte.
//! Records whose states cannot be named, or whose shaders are refused, are
//! counted by cause and skipped. The library is saved as it grows: an
//! interrupted pre-build keeps its work, and the next one finds those
//! pipelines there (it loads them from the library: this tree keeps no
//! list of what a pre-build made).

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use anyhow::Result;
use victory_game::pipelines::{self, Named, Unnamed};
use victory_game::{sgd5, shaderpack};
use victory_gpu::pipeline::{self, Prebuild};
use victory_shader::{Container, FourCC, HwStage, native_programs};

use crate::shader::{MAX_FILE, Source, sources};

/// Where a native program's container is: file, source, and its stage.
type Found = (usize, usize, HwStage);

/// One pipeline to make: a graphics or compute record of the pack.
#[derive(Clone, Copy, Debug)]
enum Job {
    Graphics(usize),
    Compute(usize),
}

pub fn run(paths: &[PathBuf], threads: usize) -> Result<()> {
    let started = Instant::now();
    // The caches a run opens, as it opens them.
    let shaders = victory_core::Config::load()?.cache_subdir("shaders")?;
    let dir = shaders.with_file_name("pipelines");
    victory_hle::modules::d3d12x::use_shader_cache(shaders);
    println!("cache        {}", dir.display());
    let journal = Journal::open(&dir);
    // The files that hold shaders (or the pack itself).
    let files: Vec<(PathBuf, Vec<u8>)> = paths
        .iter()
        .flat_map(|root| {
            walkdir::WalkDir::new(root)
                .into_iter()
                .filter_map(Result::ok)
        })
        .filter(|e| e.file_type().is_file() && e.metadata().is_ok_and(|m| m.len() <= MAX_FILE))
        .filter_map(|e| Some((e.path().to_owned(), std::fs::read(e.path()).ok()?)))
        .filter(|(_, d)| d.starts_with(shaderpack::MAGIC) || !sources(d).is_empty())
        .collect();
    // It ships beside the shader databases.
    let found = (files.iter())
        .filter(|(_, d)| d.starts_with(shaderpack::MAGIC) || sgd5::is_sgd5(d))
        .find_map(|(p, d)| Some((p, shaderpack::find(d)?)));
    let Some((pack_path, pack)) = found else {
        println!("pipelines    no shader pack among the files: nothing to pre-build");
        return Ok(());
    };
    let pack = shaderpack::Pack::parse(&pack).map_err(anyhow::Error::msg)?;
    let graphics: Vec<shaderpack::Graphics> = pack.graphics().collect();
    let compute: Vec<shaderpack::Compute> = pack.compute().collect();

    // Every container by the hash instructions its native program starts
    // with, which the pack's programs start with too.
    let sources: Vec<Vec<Source>> = files.iter().map(|(_, d)| sources(d)).collect();
    let programs: Mutex<HashMap<[u8; 24], Found>> = Mutex::new(HashMap::new());
    for (f, list) in sources.iter().enumerate() {
        let next = AtomicUsize::new(0);
        std::thread::scope(|s| {
            for _ in 0..threads.min(list.len()) {
                s.spawn(|| {
                    loop {
                        let i = next.fetch_add(1, Ordering::Relaxed);
                        let Some(source) = list.get(i) else { break };
                        let Some(bytes) = source.bytes() else {
                            continue;
                        };
                        for (id, stage) in program_ids(&bytes) {
                            lock(&programs).entry(id).or_insert((f, i, stage));
                        }
                    }
                });
            }
        });
    }
    let programs = programs.into_inner().unwrap_or_else(|e| e.into_inner());
    let databases: Vec<sgd5::Database> =
        files.iter().flat_map(|(_, d)| sgd5::databases(d)).collect();
    let named = Named::find(&pack, &databases);
    eprintln!(
        "{}: shader pack, {} graphics and {} compute pipelines; states named: {} depth-stencil, {} rasteriser, {} blend",
        pack_path.display(),
        graphics.len(),
        compute.len(),
        named.depth.len(),
        named.raster.len(),
        named.blend.len(),
    );

    let container = |code: &[u8]| -> Option<(Vec<u8>, HwStage)> {
        let &(f, i, stage) = programs.get(&shaderpack::program_id(code)?)?;
        let bytes = sources[f][i].bytes()?;
        // As the title passes it: the container's own size.
        let size = u32::from_le_bytes(bytes.get(0x18..0x1c)?.try_into().ok()?) as usize;
        Some((bytes.get(..size)?.to_vec(), stage))
    };
    // A job's pipeline, or why it has none.
    let prepare = |job: Job| -> Result<Prebuild, String> {
        let g = match job {
            Job::Graphics(i) => &graphics[i],
            Job::Compute(i) => {
                let (cs, _) = container(compute[i].stage.program).ok_or("shader not found")?;
                return Ok(Prebuild::compute(&cs));
            }
        };
        let [ps, vs, gs, ..] = g.stages;
        if g.stages[3..].iter().any(Option::is_some) {
            return Err("stages other than vertex, mesh and pixel".into());
        }
        let (first, stage) = vs
            .or(gs)
            .and_then(|s| container(s.program))
            .ok_or("shader not found")?;
        let pixel = match ps {
            Some(ps) => Some(container(ps.program).ok_or("shader not found")?.0),
            None => None,
        };
        let mesh = match stage {
            HwStage::Ms => true,
            HwStage::Vs => false,
            _ => return Err("first stage not a vertex or mesh shader".into()),
        };
        let stream = pipelines::stream(g, &named, &first, mesh, pixel.as_deref()).map_err(|u| {
            match u {
                Unnamed::DepthStencil => "depth-stencil state not named",
                Unnamed::Rasterizer => "rasteriser state not named",
                Unnamed::Blend => "blend state not named",
                Unnamed::Metadata => "metadata not read",
            }
            .to_owned()
        })?;
        // SAFETY: the stream points at `first`, `pixel` and its own input
        // elements, all alive here.
        Ok(unsafe { Prebuild::graphics(stream.bytes()) })
    };

    // A job's shaders as a run names them (diagnostics).
    let shaders_of = |job: Job| {
        let stages: Vec<(&str, &[u8])> = match job {
            Job::Compute(i) => vec![("compute", compute[i].stage.program)],
            Job::Graphics(i) => {
                let [ps, vs, gs, ..] = graphics[i].stages;
                let first = vs
                    .map(|s| ("vertex", s.program))
                    .or(gs.map(|s| ("mesh", s.program)));
                first
                    .into_iter()
                    .chain(ps.map(|s| ("pixel", s.program)))
                    .collect()
            }
        };
        let names: Vec<String> = (stages.iter())
            .map(|(stage, code)| match container(code) {
                Some((c, _)) => format!("{stage} {}", &blake3::hash(&c).to_hex()[..16]),
                None => format!("{stage} ?"),
            })
            .collect();
        format!("[{}]", names.join(", "))
    };
    let mut jobs: Vec<Job> = (0..compute.len()).map(Job::Compute).collect();
    jobs.extend((0..graphics.len()).map(Job::Graphics));
    // Diagnostics, `VICTORY_PREBUILD_KEYS=<file>`: make nothing, list each
    // record's key (what a run lists pipelines by) with its shaders and
    // state as a run's `VICTORY_PIPELINE_LOG` has them, or why it is
    // skipped; and the records each source named states for.
    let keys_only = std::env::var_os("VICTORY_PREBUILD_KEYS");
    if keys_only.is_some() {
        let mut by: BTreeMap<(&str, &str), usize> = BTreeMap::new();
        for g in &graphics {
            for (kind, h) in [
                ("depth-stencil", g.depth_stencil_hash),
                ("rasteriser", g.rasterizer_hash),
                ("blend", g.blend_hash),
            ] {
                *by.entry((kind, named.source.get(&h).copied().unwrap_or("not named")))
                    .or_default() += 1;
            }
        }
        for ((kind, source), n) in by {
            println!("  {kind:<14} {source:<12} {n:>6} records");
        }
    }
    let skipped: Mutex<BTreeMap<String, usize>> = Mutex::new(BTreeMap::new());
    let skip = |why: String| *lock(&skipped).entry(why).or_default() += 1;
    // First every record's key, which needs no translation: what an
    // earlier run drew with is made first. (Upstream also skips what an
    // earlier pre-build listed; this tree keeps no such list, see `make`'s
    // port note, so those are found by loading them from the library.)
    let lines: Mutex<Vec<String>> = Mutex::new(Vec::new());
    let todo: Mutex<Vec<(bool, Job)>> = Mutex::new(Vec::new());
    // Suspects of the last pre-build's end (see `Journal`), made first,
    // one at a time.
    let suspected: Mutex<Vec<Job>> = Mutex::new(Vec::new());
    each(&jobs, threads, |job| match prepare(job) {
        Ok(p) if keys_only.is_some() => {
            lock(&lines).push(format!("{}\t{job:?}\t{}", p.key(), p.describe()))
        }
        Ok(p) if journal.crashes.contains(&p.key()) => {
            skip("it crashed the host's compiler in an earlier pre-build".into())
        }
        Ok(p) if journal.suspects.contains(&p.key()) => lock(&suspected).push(job),
        Ok(p) => lock(&todo).push((!pipeline::wanted_early(&p.key()), job)),
        Err(why) => {
            if keys_only.is_some() {
                lock(&lines).push(format!("-\t{job:?}\t{}\t{why}", shaders_of(job)));
            }
            skip(why);
        }
    });
    if let Some(file) = keys_only {
        std::fs::write(file, lock(&lines).join("\n"))?;
    }
    let mut todo = todo.into_inner().unwrap_or_else(|e| e.into_inner());
    todo.sort_by_key(|&(later, _)| later);
    let todo: Vec<Job> = todo.into_iter().map(|(_, job)| job).collect();

    let (made0, loaded0) = pipeline::pipeline_counts();
    let counts = || {
        let (made, loaded) = pipeline::pipeline_counts();
        (made - made0, loaded - loaded0)
    };
    // A job, with its key in the journal while the host makes it.
    // Diagnostics, `VICTORY_PREBUILD_MADE_LOG=1` (with `--jobs 1`): each
    // pipeline the host made rather than loaded from the library, by key,
    // job and shaders (a library that should hold it does not).
    let made_log = std::env::var_os("VICTORY_PREBUILD_MADE_LOG").is_some();
    let make = |job: Job| {
        let made = prepare(job).and_then(|p| {
            let key = p.key();
            journal.begin(&key);
            let before = pipeline::pipeline_counts().0;
            let made = p.make();
            if made_log && pipeline::pipeline_counts().0 > before {
                eprintln!("made, not loaded: {key} {job:?} {}", p.describe());
            }
            journal.end(&key);
            made
        });
        if let Err(why) = made {
            skip(why);
        }
    };
    let suspected = suspected.into_inner().unwrap_or_else(|e| e.into_inner());
    if !suspected.is_empty() {
        eprintln!(
            "{} pipelines were being made when the last pre-build ended: made one at a time first",
            suspected.len()
        );
        for &job in &suspected {
            make(job);
        }
    }
    let next = AtomicUsize::new(0);
    std::thread::scope(|s| {
        let workers: Vec<_> = (0..threads.min(todo.len()))
            .map(|_| {
                s.spawn(|| {
                    while let Some(&job) = todo.get(next.fetch_add(1, Ordering::Relaxed)) {
                        make(job);
                    }
                })
            })
            .collect();
        let (n, what) = (todo.len(), "pipelines to make");
        report(pack_path, n, what, &next, started, &workers, || {
            let (made, loaded) = counts();
            format!("; {made} made, {loaded} found in the library")
        });
    });
    pipeline::save_pipeline_cache();
    journal.finish();
    let (made, loaded) = counts();
    println!(
        "pipelines    {} in the pack, {:.1} s: {made} made, {loaded} already in the library",
        jobs.len(),
        started.elapsed().as_secs_f64(),
    );
    for (why, n) in lock(&skipped).iter() {
        println!("  {n:>7}  skipped: {why}");
    }
    Ok(())
}

/// What a pre-build keeps on disk to get past a pipeline whose making kills
/// the process (the host's compiler crashing: on AMD every pre-build
/// stopped at the same pipeline, 6% in). While the host makes pipelines,
/// [`INFLIGHT_FILE`] lists their keys. A pre-build that finds it was not
/// left by a finished one: if it names one pipeline, that one killed the
/// process, and goes into [`pipeline::COMPILER_CRASHES_FILE`] (skipped by
/// pre-builds, left out by runs); if it names several, they are made first,
/// one at a time, so the next end names the culprit alone.
struct Journal {
    file: PathBuf,
    crashes_file: PathBuf,
    /// Keys that crashed the compiler before, and the last end's suspects.
    crashes: HashSet<String>,
    suspects: HashSet<String>,
    making: Mutex<HashSet<String>>,
}

/// Keys of the pipelines being made (see [`Journal`]).
const INFLIGHT_FILE: &str = "prebuild-inflight.txt";

impl Journal {
    fn open(dir: &std::path::Path) -> Journal {
        let file = dir.join(INFLIGHT_FILE);
        let crashes_file = dir.join(pipeline::COMPILER_CRASHES_FILE);
        let mut crashes =
            pipeline::parse_compiler_crashes(&std::fs::read_to_string(&crashes_file).unwrap_or_default());
        let inflight = std::fs::read_to_string(&file).unwrap_or_default();
        let (suspects, culprit) = after_an_end(&inflight, &crashes);
        if let Some(key) = culprit {
            eprintln!("pipeline {key} killed the last pre-build (the host's compiler): skipped from now on, and left out by runs");
            crashes.insert(key);
            let mut list: Vec<&str> = crashes.iter().map(String::as_str).collect();
            list.sort_unstable();
            if let Err(e) = std::fs::write(&crashes_file, list.join("\n") + "\n") {
                eprintln!("{}: not written ({e})", crashes_file.display());
            }
        }
        let _ = std::fs::remove_file(&file);
        Journal {
            file,
            crashes_file,
            crashes,
            suspects,
            making: Mutex::new(HashSet::new()),
        }
    }

    /// The host starts making `key`.
    fn begin(&self, key: &str) {
        let mut making = lock(&self.making);
        making.insert(key.to_owned());
        self.write(&making);
    }

    /// The host is done with `key` (made, refused or found).
    fn end(&self, key: &str) {
        let mut making = lock(&self.making);
        making.remove(key);
        self.write(&making);
    }

    /// Written whole each time; a process that dies leaves the last one (a
    /// write the system has taken survives the process).
    fn write(&self, making: &HashSet<String>) {
        let mut keys: Vec<&str> = making.iter().map(String::as_str).collect();
        keys.sort_unstable();
        let _ = std::fs::write(&self.file, keys.join("\n"));
    }

    /// The pre-build ended by itself: nothing was left being made.
    fn finish(&self) {
        let _ = std::fs::remove_file(&self.file);
        if !self.crashes.is_empty() {
            eprintln!(
                "{} pipelines skipped as crashing the host's compiler ({})",
                self.crashes.len(),
                self.crashes_file.display()
            );
        }
    }
}

/// What the journal of a pre-build that did not finish says: the keys it
/// was making that are not known to crash, and the culprit when it was
/// making one alone.
fn after_an_end(inflight: &str, crashes: &HashSet<String>) -> (HashSet<String>, Option<String>) {
    let mut suspects = pipeline::parse_compiler_crashes(inflight);
    suspects.retain(|k| !crashes.contains(k));
    if suspects.len() == 1 {
        return (HashSet::new(), suspects.into_iter().next());
    }
    (suspects, None)
}

/// Runs `f` on every job, on `threads` threads.
fn each(jobs: &[Job], threads: usize, f: impl Fn(Job) + Sync) {
    let next = AtomicUsize::new(0);
    std::thread::scope(|s| {
        for _ in 0..threads.min(jobs.len()) {
            s.spawn(|| {
                while let Some(&job) = jobs.get(next.fetch_add(1, Ordering::Relaxed)) {
                    f(job);
                }
            });
        }
    });
}

/// The hash instructions each native program of a container starts with.
fn program_ids(bytes: &[u8]) -> Vec<([u8; 24], HwStage)> {
    let Ok(c) = Container::parse(bytes) else {
        return Vec::new();
    };
    // The DXIL part only, as a run reads it here (upstream also reads an
    // ACCC part, from a commit this tree does not have).
    let Some(part) = c.part(FourCC::DXIL) else {
        return Vec::new();
    };
    native_programs(part.data)
        .map(|programs| {
            (programs.iter())
                .filter_map(|p| Some((shaderpack::program_id(&p.code)?, p.stage)))
                .collect()
        })
        .unwrap_or_default()
}

/// Progress on stderr while `workers` take `jobs` jobs (`next` counts
/// those handed out): every second on a terminal, every 10 s otherwise.
fn report<T>(
    file: &Path,
    jobs: usize,
    what: &str,
    next: &AtomicUsize,
    started: std::time::Instant,
    workers: &[std::thread::ScopedJoinHandle<'_, T>],
    extra: impl Fn() -> String,
) {
    use std::io::IsTerminal;
    if workers.is_empty() {
        return;
    }
    let terminal = std::io::stderr().is_terminal();
    let every = std::time::Duration::from_secs(if terminal { 1 } else { 10 });
    let line = || {
        // `next` counts jobs handed out; a worker still running holds one.
        let busy = workers.iter().filter(|w| !w.is_finished()).count();
        let taken = next.load(Ordering::Relaxed).min(jobs);
        let done = taken.saturating_sub(busy);
        format!(
            "{}: {done} of {jobs} {what} ({}%), {:.0} s{}",
            file.display(),
            done * 100 / jobs.max(1),
            started.elapsed().as_secs_f64(),
            extra()
        )
    };
    let mut shown = std::time::Instant::now();
    while !workers.iter().all(|w| w.is_finished()) {
        std::thread::sleep(std::time::Duration::from_millis(100));
        if shown.elapsed() >= every {
            shown = std::time::Instant::now();
            if terminal {
                eprint!("\r{}", line());
            } else {
                eprintln!("{}", line());
            }
        }
    }
    eprintln!("{}{}", if terminal { "\r" } else { "" }, line());
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(keys: &[&str]) -> HashSet<String> {
        keys.iter().map(|k| (*k).to_owned()).collect()
    }

    /// One pipeline being made when the process died is the culprit;
    /// several are suspects, made one at a time; known crashes and a
    /// finished pre-build (no journal) name nothing.
    #[test]
    fn the_journal_names_the_pipeline_that_killed_the_process() {
        assert_eq!(after_an_end("a\n", &set(&[])), (set(&[]), Some("a".to_owned())));
        assert_eq!(after_an_end("a\nb\nc", &set(&[])), (set(&["a", "b", "c"]), None));
        assert_eq!(after_an_end("a\nb", &set(&["b"])), (set(&[]), Some("a".to_owned())));
        assert_eq!(after_an_end("b", &set(&["b"])), (set(&[]), None));
        assert_eq!(after_an_end("", &set(&["b"])), (set(&[]), None));
    }

    /// A journal written, the process gone, and the next pre-build opening
    /// it: the culprit goes into the crash list, which a run reads too.
    #[test]
    fn a_culprit_is_kept_for_pre_builds_and_runs() {
        let dir = std::env::temp_dir().join(format!("victory-journal-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let first = Journal::open(&dir);
        first.begin("a");
        first.begin("b");
        first.end("b");
        drop(first); // as if the process had died making "a"
        let second = Journal::open(&dir);
        assert!(second.crashes.contains("a") && second.suspects.is_empty());
        let listed = std::fs::read_to_string(dir.join(pipeline::COMPILER_CRASHES_FILE)).unwrap();
        assert_eq!(pipeline::parse_compiler_crashes(&listed), set(&["a"]));
        second.finish();
        assert!(!dir.join(INFLIGHT_FILE).exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
