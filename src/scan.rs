//! `mcscan scan`: walk every inventory in a world, in parallel, and write the
//! matching item stacks to a results file.

use crate::filter::{EvalCtx, Filter};
use crate::item::{self, own_path, ItemView, SegRef};
use crate::nbt::{self, Compound};
use crate::preset::Preset;
use crate::progress::Eta;
use crate::region::{self, Inflater, Region, CHUNKS_PER_REGION, SECTOR};
use crate::results::{self, Header, ReasonRec, Record, FORMAT_VERSION};
use crate::world::{self, Category, WorldFile};
use anyhow::{bail, Context, Result};
use rayon::prelude::*;
use std::cell::RefCell;
use std::io::IsTerminal;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Chunk data that can never contain items. Skipping it (block states,
/// heightmaps, light, structure references) is most of the scan's speed.
const CHUNK_SKIP: [&str; 22] = [
    "sections", "Sections", "Heightmaps", "HeightMap", "structures", "Structures", "block_ticks", "fluid_ticks",
    "TileTicks", "LiquidTicks", "PostProcessing", "blending_data", "CarvingMasks", "Lights", "LiquidsToBeTicked",
    "ToBeTicked", "Biomes", "UpgradeData", "BlockLight", "SkyLight", "TerrainPopulated", "LightPopulated",
];

pub struct ScanOptions {
    pub world: PathBuf,
    pub preset: Option<Preset>,
    pub preset_spec: Option<String>,
    pub filters: Vec<Filter>,
    /// Report every item stack (no preset or filter required).
    pub all_items: bool,
    pub categories: Vec<Category>,
    pub dims: Vec<String>,
    pub output: String,
    pub snbt: bool,
    pub quiet: bool,
    pub threads: Option<usize>,
}

/// Live counters of a running scan, shared with whatever displays progress.
#[derive(Default)]
pub struct Stats {
    pub total_files: AtomicU64,
    pub total_bytes: AtomicU64,
    pub files_done: AtomicU64,
    pub bytes_done: AtomicU64,
    pub chunks: AtomicU64,
    pub items: AtomicU64,
    pub matches: AtomicU64,
    pub errors: AtomicU64,
    first_errors: Mutex<Vec<String>>,
    /// Set to make the workers stop early; `execute` then fails with "cancelled".
    pub cancel: AtomicBool,
}

impl Stats {
    fn error(&self, msg: String) {
        self.errors.fetch_add(1, Ordering::Relaxed);
        let mut list = self.first_errors.lock().unwrap_or_else(|e| e.into_inner());
        if list.len() < 10 {
            list.push(msg);
        }
    }
}

thread_local! {
    /// Per-thread decompressor and buffer, reused for every chunk.
    static SCRATCH: RefCell<(Inflater, Vec<u8>)> = RefCell::new((Inflater::default(), Vec::with_capacity(1 << 18)));
}

fn has_key(path: &[SegRef], depth: usize, names: &[&str]) -> bool {
    path.iter().take(depth).any(|s| matches!(s, SegRef::Key(k) if names.contains(k)))
}

fn source_of(category: Category, path: &[SegRef]) -> &'static str {
    match category {
        Category::Players => "player",
        Category::Entities => "entity",
        Category::Data => "data",
        Category::Level => {
            if matches!(path, [SegRef::Key("Data"), SegRef::Key("Player"), ..]) {
                "player"
            } else {
                "level"
            }
        }
        Category::Chunks => {
            // Pre-1.18 chunks nest everything in "Level"; proto-chunks keep
            // their entities in the chunk itself.
            if has_key(path, 2, &["block_entities", "TileEntities"]) {
                "block_entity"
            } else if has_key(path, 2, &["entities", "Entities"]) {
                "entity"
            } else {
                "other"
            }
        }
    }
}

/// Scan one parsed NBT tree (a chunk, a player file, ...).
fn scan_tree(root: &Compound, file: &WorldFile, chunk: Option<(i32, i32)>, opts: &ScanOptions, stats: &Stats) -> Vec<Record> {
    let player = match file.category {
        Category::Players => Some(root),
        Category::Level => root.compound("Data").and_then(|d| d.compound("Player")),
        _ => None,
    };
    let player_dim = player.and_then(|p| p.str("Dimension"));
    let uuid = file.player();

    let mut out = Vec::new();
    let mut reasons = Vec::new();
    let mut items = 0u64;
    item::walk_items(root, &mut |hit| {
        items += 1;
        let Some(view) = ItemView::new(hit.item) else { return };
        reasons.clear();
        if let Some(preset) = &opts.preset {
            preset.check(&view, hit.parents, &mut reasons);
            if reasons.is_empty() {
                return;
            }
        }
        let source = source_of(file.category, hit.path);
        let is_player = source == "player";
        let holder = hit.holder.or(if is_player { Some("player") } else { None });
        let dim = if is_player { player_dim.or(file.dim.as_deref()) } else { file.dim.as_deref() };
        if !opts.filters.is_empty() {
            let cx = EvalCtx {
                view: &view,
                holder,
                source,
                dim,
                pos: hit.pos,
                slot: hit.slot,
                depth: hit.parents.len(),
                file: &file.rel,
                uuid,
            };
            if !opts.filters.iter().all(|f| f.matches(&cx)) {
                return;
            }
        }
        out.push(Record {
            n: 0,
            id: item::norm_id(hit.id).into_owned(),
            count: view.count,
            reasons: reasons
                .iter()
                .map(|r| ReasonRec { rule: r.rule.id().to_string(), detail: r.detail.clone() })
                .collect(),
            info: view.summary(),
            source: source.to_string(),
            holder: holder.map(str::to_string),
            dim: dim.map(|d| item::norm_id(d).into_owned()),
            pos: hit.pos,
            slot: hit.slot,
            uuid: if is_player { uuid.map(str::to_string) } else { None },
            depth: hit.parents.len(),
            file: file.rel.clone(),
            chunk: chunk.map(|(x, z)| [x, z]),
            path: own_path(hit.path),
            hash: format!("{:016x}", nbt::fingerprint(hit.item)),
            snbt: if opts.snbt { Some(nbt::snbt(hit.item)) } else { None },
        });
    });
    stats.items.fetch_add(items, Ordering::Relaxed);
    stats.matches.fetch_add(out.len() as u64, Ordering::Relaxed);
    out
}

/// Returns the findings and how many of the file's bytes were already
/// credited to `stats.bytes_done` (chunk by chunk, so progress and the ETA
/// move smoothly through big region files).
fn scan_region(file: &WorldFile, opts: &ScanOptions, stats: &Stats) -> (Vec<Record>, u64) {
    let region = match Region::open(&file.path) {
        Ok(r) => r,
        Err(e) => {
            stats.error(format!("{}: {e:#}", file.rel));
            return (Vec::new(), 0);
        }
    };
    if region.buf.len() < 2 * SECTOR {
        return (Vec::new(), 0); // empty placeholder region
    }
    let share = file.size / CHUNKS_PER_REGION as u64;
    let records = (0..CHUNKS_PER_REGION)
        .into_par_iter()
        .map(|idx| {
            stats.bytes_done.fetch_add(share, Ordering::Relaxed);
            if stats.cancel.load(Ordering::Relaxed) {
                return Vec::new();
            }
            SCRATCH.with(|cell| {
                let mut scratch = cell.borrow_mut();
                let (inflater, buf) = &mut *scratch;
                let parsed = match region.read(idx, inflater, buf) {
                    Ok(false) => return Vec::new(),
                    Ok(true) => nbt::parse_skipping(buf, &CHUNK_SKIP),
                    Err(e) => Err(e),
                };
                stats.chunks.fetch_add(1, Ordering::Relaxed);
                match parsed {
                    Ok((_, root)) => scan_tree(&root, file, Some(region.chunk_pos(idx)), opts, stats),
                    Err(e) => {
                        let (cx, cz) = region.chunk_pos(idx);
                        stats.error(format!("{}: chunk {cx},{cz}: {e:#}", file.rel));
                        Vec::new()
                    }
                }
            })
        })
        .reduce(Vec::new, |mut a, mut b| {
            a.append(&mut b);
            a
        });
    (records, share * CHUNKS_PER_REGION as u64)
}

fn scan_dat(file: &WorldFile, opts: &ScanOptions, stats: &Stats) -> Vec<Record> {
    let parsed = region::read_dat(&file.path).and_then(|(bytes, _)| nbt::parse(&bytes));
    match parsed {
        Ok((_, root)) => scan_tree(&root, file, None, opts, stats),
        Err(e) => {
            // Not every .dat under data/ is NBT; only complain about the ones
            // that should be.
            if file.category != Category::Data {
                stats.error(format!("{}: {e:#}", file.rel));
            }
            Vec::new()
        }
    }
}

pub fn human_count(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

pub fn human_bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = n as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", UNITS[u])
    }
}

/// Everything decided before the first byte is read: which files, how much
/// data, how many threads.
pub struct Plan {
    pub world: PathBuf,
    files: Vec<WorldFile>,
    pub total_files: u64,
    pub total_bytes: u64,
    pool: rayon::ThreadPool,
    pub threads: usize,
}

pub fn plan(opts: &ScanOptions) -> Result<Plan> {
    if opts.preset.is_none() && opts.filters.is_empty() && !opts.all_items {
        bail!("nothing to search for: pass --preset, one or more --filter expressions, or --all-items");
    }
    let world = opts.world.canonicalize().with_context(|| format!("world folder {}", opts.world.display()))?;
    let mut files = world::discover(&world)?;
    files.retain(|f| opts.categories.contains(&f.category));
    if !opts.dims.is_empty() {
        let wanted: Vec<String> = opts.dims.iter().map(|d| item::norm_id(d).into_owned()).collect();
        files.retain(|f| match &f.dim {
            Some(d) => wanted.iter().any(|w| w == d),
            None => true, // player files are not tied to a dimension
        });
    }
    if files.is_empty() {
        bail!("no files left to scan after applying --only / --dim");
    }
    // Biggest first, so the pool does not end on one huge region file.
    files.sort_by(|a, b| b.size.cmp(&a.size).then_with(|| a.rel.cmp(&b.rel)));
    let total_files = files.len() as u64;
    let total_bytes: u64 = files.iter().map(|f| f.size).sum();

    let mut builder = rayon::ThreadPoolBuilder::new();
    if let Some(n) = opts.threads {
        builder = builder.num_threads(n.max(1));
    }
    let pool = builder.build().context("starting worker threads")?;
    let threads = pool.current_num_threads();
    Ok(Plan { world, files, total_files, total_bytes, pool, threads })
}

pub struct ScanOutput {
    pub header: Header,
    pub records: Vec<Record>,
    pub elapsed: Duration,
    pub total_bytes: u64,
    pub chunks: u64,
    pub items: u64,
    pub errors: u64,
    pub first_errors: Vec<String>,
}

impl ScanOutput {
    /// "Scanned 1,234 chunks and 567 item stacks in 0.45s (5.0 GiB/s)"
    pub fn speed_line(&self) -> String {
        let secs = self.elapsed.as_secs_f64().max(0.001);
        format!(
            "Scanned {} chunks and {} item stacks in {:.2}s ({}/s)",
            human_count(self.chunks),
            human_count(self.items),
            secs,
            human_bytes((self.total_bytes as f64 / secs) as u64),
        )
    }
}

/// Run the scan. Progress is published through `stats` while it runs; nothing
/// is printed and nothing is written.
pub fn execute(plan: &Plan, opts: &ScanOptions, stats: &Stats) -> Result<ScanOutput> {
    stats.total_files.store(plan.total_files, Ordering::Relaxed);
    stats.total_bytes.store(plan.total_bytes, Ordering::Relaxed);
    let started = Instant::now();
    let mut records: Vec<Record> = plan.pool.install(|| {
        plan.files
            .par_iter()
            .map(|file| {
                if stats.cancel.load(Ordering::Relaxed) {
                    return Vec::new();
                }
                let (found, credited) =
                    if file.is_region() { scan_region(file, opts, stats) } else { (scan_dat(file, opts, stats), 0) };
                stats.files_done.fetch_add(1, Ordering::Relaxed);
                stats.bytes_done.fetch_add(file.size - credited, Ordering::Relaxed);
                found
            })
            .reduce(Vec::new, |mut a, mut b| {
                a.append(&mut b);
                a
            })
    });
    let elapsed = started.elapsed();
    if stats.cancel.load(Ordering::Relaxed) {
        bail!("scan cancelled");
    }

    // Deterministic numbering regardless of thread scheduling.
    records.par_sort_by(|a, b| {
        a.file.cmp(&b.file).then_with(|| a.chunk.cmp(&b.chunk)).then_with(|| nbt::cmp_paths(&a.path, &b.path))
    });
    for (i, r) in records.iter_mut().enumerate() {
        r.n = i as u64 + 1;
    }

    let header = Header {
        mcscan: FORMAT_VERSION,
        world: plan.world.to_string_lossy().into_owned(),
        preset: opts.preset_spec.clone(),
        filters: opts.filters.iter().map(|f| f.source.clone()).collect(),
        scanned_at: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
        items_scanned: stats.items.load(Ordering::Relaxed),
        matches: records.len() as u64,
    };
    let first_errors = stats.first_errors.lock().unwrap_or_else(|e| e.into_inner()).clone();
    Ok(ScanOutput {
        header,
        records,
        elapsed,
        total_bytes: plan.total_bytes,
        chunks: stats.chunks.load(Ordering::Relaxed),
        items: stats.items.load(Ordering::Relaxed),
        errors: stats.errors.load(Ordering::Relaxed),
        first_errors,
    })
}

/// Write a results file (`-` = standard output).
pub fn save(output: &str, header: &Header, records: &[Record]) -> Result<()> {
    if output == "-" {
        results::write(std::io::BufWriter::new(std::io::stdout().lock()), header, records)
    } else {
        let f = std::fs::File::create(output).with_context(|| format!("creating {output}"))?;
        results::write(std::io::BufWriter::new(f), header, records)
    }
}

/// One line of progress for a plain terminal, with rate and time remaining.
fn progress_line(stats: &Stats, eta: &Eta) -> String {
    let total = stats.total_bytes.load(Ordering::Relaxed);
    let done = stats.bytes_done.load(Ordering::Relaxed).min(total);
    format!(
        "  {:>3}%  {}/{} files  {} chunks  {} matches  {}/s  ETA {}",
        done * 100 / total.max(1),
        stats.files_done.load(Ordering::Relaxed),
        stats.total_files.load(Ordering::Relaxed),
        human_count(stats.chunks.load(Ordering::Relaxed)),
        human_count(stats.matches.load(Ordering::Relaxed)),
        human_bytes(eta.rate() as u64),
        eta.text(total),
    )
}

/// `mcscan scan`: scan, show progress on stderr, write the results file.
pub fn run(opts: ScanOptions) -> Result<()> {
    let plan = plan(&opts)?;
    if !opts.quiet {
        if let Some(pid) = world::lock_holder(&plan.world) {
            eprintln!(
                "note: this world is open in a running game/server (pid {pid}). Scanning is safe, but\n      \
                 changes still in memory are not on disk yet - run `save-all flush` first for an exact result."
            );
        }
        eprintln!(
            "Scanning {} ({} files, {}) with {} thread{}",
            plan.world.display(),
            human_count(plan.total_files),
            human_bytes(plan.total_bytes),
            plan.threads,
            if plan.threads == 1 { "" } else { "s" }
        );
    }

    let stats = Arc::new(Stats::default());
    let done = Arc::new(AtomicBool::new(false));
    let progress = if !opts.quiet && std::io::stderr().is_terminal() {
        let (stats, done) = (stats.clone(), done.clone());
        Some(std::thread::spawn(move || {
            let mut eta = Eta::new();
            while !done.load(Ordering::Relaxed) {
                eta.update(stats.bytes_done.load(Ordering::Relaxed));
                // \x1b[K clears whatever a longer previous line left behind.
                eprint!("\r{}\x1b[K", progress_line(&stats, &eta));
                std::thread::sleep(Duration::from_millis(200));
            }
            eprint!("\r\x1b[K");
        }))
    } else {
        None
    };

    let result = execute(&plan, &opts, &stats);
    done.store(true, Ordering::Relaxed);
    if let Some(handle) = progress {
        let _ = handle.join();
    }
    let out = result?;
    save(&opts.output, &out.header, &out.records)?;

    if !opts.quiet {
        eprintln!("{}", out.speed_line());
        if out.errors > 0 {
            eprintln!("warning: {} chunk(s)/file(s) could not be read and were skipped:", human_count(out.errors));
            for e in &out.first_errors {
                eprintln!("  {e}");
            }
            if out.errors as usize > out.first_errors.len() {
                eprintln!("  ... and {} more", out.errors as usize - out.first_errors.len());
            }
        }
        let refs: Vec<&Record> = out.records.iter().collect();
        results::print_summary(std::io::stderr().lock(), &refs)?;
        if opts.output != "-" {
            eprintln!("Results written to {}", opts.output);
            if !out.records.is_empty() {
                eprintln!(
                    "Next:  mcscan list {0}      then      mcscan apply {0} --all --fix --dry-run      (or: mcscan tui {0})",
                    opts.output
                );
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn progress_line_shows_rate_and_eta() {
        let stats = Stats::default();
        stats.total_bytes.store(1000 << 20, Ordering::Relaxed);
        stats.total_files.store(10, Ordering::Relaxed);
        stats.bytes_done.store(250 << 20, Ordering::Relaxed);
        stats.files_done.store(2, Ordering::Relaxed);
        let t = Instant::now();
        let mut eta = Eta::starting_at(t);
        eta.update_at(t + Duration::from_secs(5), 250 << 20);
        let line = progress_line(&stats, &eta);
        assert!(line.contains(" 25%"), "{line}");
        assert!(line.contains("2/10 files"), "{line}");
        assert!(line.contains("50.0 MiB/s"), "{line}");
        assert!(line.ends_with("ETA 15s"), "{line}");
    }
}
