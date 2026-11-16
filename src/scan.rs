//! `mcscan scan`: walk every inventory in a world, in parallel, and write the
//! matching item stacks to a results file.

use crate::filter::{EvalCtx, Filter};
use crate::item::{self, own_path, ItemView, SegRef};
use crate::nbt::{self, Compound};
use crate::preset::Preset;
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

#[derive(Default)]
struct Stats {
    files_done: AtomicU64,
    bytes_done: AtomicU64,
    chunks: AtomicU64,
    items: AtomicU64,
    matches: AtomicU64,
    errors: AtomicU64,
    first_errors: Mutex<Vec<String>>,
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

fn scan_region(file: &WorldFile, opts: &ScanOptions, stats: &Stats) -> Vec<Record> {
    let region = match Region::open(&file.path) {
        Ok(r) => r,
        Err(e) => {
            stats.error(format!("{}: {e:#}", file.rel));
            return Vec::new();
        }
    };
    if region.buf.len() < 2 * SECTOR {
        return Vec::new(); // empty placeholder region
    }
    (0..CHUNKS_PER_REGION)
        .into_par_iter()
        .map(|idx| {
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
        })
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

fn human_bytes(n: u64) -> String {
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

pub fn run(opts: ScanOptions) -> Result<()> {
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

    if !opts.quiet {
        if let Some(pid) = world::lock_holder(&world) {
            eprintln!(
                "note: this world is open in a running game/server (pid {pid}). Scanning is safe, but\n      \
                 changes still in memory are not on disk yet - run `save-all flush` first for an exact result."
            );
        }
        eprintln!(
            "Scanning {} ({} files, {}) with {threads} thread{}",
            world.display(),
            human_count(total_files),
            human_bytes(total_bytes),
            if threads == 1 { "" } else { "s" }
        );
    }

    let stats = Arc::new(Stats::default());
    let done = Arc::new(AtomicBool::new(false));
    let progress = if !opts.quiet && std::io::stderr().is_terminal() {
        let (stats, done) = (stats.clone(), done.clone());
        Some(std::thread::spawn(move || {
            while !done.load(Ordering::Relaxed) {
                eprint!(
                    "\r  {:>3}%  {}/{} files  {} chunks  {} stacks  {} matches   ",
                    stats.bytes_done.load(Ordering::Relaxed) * 100 / total_bytes.max(1),
                    stats.files_done.load(Ordering::Relaxed),
                    total_files,
                    human_count(stats.chunks.load(Ordering::Relaxed)),
                    human_count(stats.items.load(Ordering::Relaxed)),
                    human_count(stats.matches.load(Ordering::Relaxed)),
                );
                std::thread::sleep(Duration::from_millis(200));
            }
            eprint!("\r{:79}\r", "");
        }))
    } else {
        None
    };

    let started = Instant::now();
    let mut records: Vec<Record> = pool.install(|| {
        files
            .par_iter()
            .map(|file| {
                let found = if file.is_region() { scan_region(file, &opts, &stats) } else { scan_dat(file, &opts, &stats) };
                stats.files_done.fetch_add(1, Ordering::Relaxed);
                stats.bytes_done.fetch_add(file.size, Ordering::Relaxed);
                found
            })
            .reduce(Vec::new, |mut a, mut b| {
                a.append(&mut b);
                a
            })
    });
    let elapsed = started.elapsed();
    done.store(true, Ordering::Relaxed);
    if let Some(handle) = progress {
        let _ = handle.join();
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
        world: world.to_string_lossy().into_owned(),
        preset: opts.preset_spec.clone(),
        filters: opts.filters.iter().map(|f| f.source.clone()).collect(),
        scanned_at: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
        items_scanned: stats.items.load(Ordering::Relaxed),
        matches: records.len() as u64,
    };
    if opts.output == "-" {
        results::write(std::io::BufWriter::new(std::io::stdout().lock()), &header, &records)?;
    } else {
        let f = std::fs::File::create(&opts.output).with_context(|| format!("creating {}", opts.output))?;
        results::write(std::io::BufWriter::new(f), &header, &records)?;
    }

    if !opts.quiet {
        let secs = elapsed.as_secs_f64().max(0.001);
        eprintln!(
            "Scanned {} chunks and {} item stacks in {:.2}s ({}/s)",
            human_count(stats.chunks.load(Ordering::Relaxed)),
            human_count(stats.items.load(Ordering::Relaxed)),
            secs,
            human_bytes((total_bytes as f64 / secs) as u64),
        );
        let errors = stats.errors.load(Ordering::Relaxed);
        if errors > 0 {
            eprintln!("warning: {} chunk(s)/file(s) could not be read and were skipped:", human_count(errors));
            let list = stats.first_errors.lock().unwrap_or_else(|e| e.into_inner());
            for e in list.iter() {
                eprintln!("  {e}");
            }
            if errors as usize > list.len() {
                eprintln!("  ... and {} more", errors as usize - list.len());
            }
        }
        let refs: Vec<&Record> = records.iter().collect();
        results::print_summary(std::io::stderr().lock(), &refs)?;
        if opts.output != "-" {
            eprintln!("Results written to {}", opts.output);
            if !records.is_empty() {
                eprintln!("Next:  mcscan list {0}      then      mcscan apply {0} --all --fix --dry-run", opts.output);
            }
        }
    }
    Ok(())
}
