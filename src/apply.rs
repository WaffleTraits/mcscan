//! `mcscan apply` / `mcscan show`: go back to the items a scan found and
//! delete or modify them.
//!
//! Every item is re-read from disk and compared against the fingerprint taken
//! during the scan, so a stale results file can never damage an item that has
//! changed since.

use crate::item::{self, short_id, EnchEdit, ItemView};
use crate::nbt::{self, Compound, Seg, Tag};
use crate::preset::{self, Preset};
use crate::region::{self, Inflater, RawChunk, Region};
use crate::results::Record;
use anyhow::{anyhow, bail, Context, Result};
use rayon::prelude::*;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::io::{BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};

/// What to do to each selected item. Operations run in the order listed here.
#[derive(Default, Debug, Clone)]
pub struct Ops {
    pub delete: bool,
    pub set_id: Option<String>,
    pub set_count: Option<i64>,
    pub strip_enchants: bool,
    pub remove_enchant: Vec<String>,
    pub clamp_enchants: bool,
    pub set_enchant: Vec<(String, i64)>,
    pub remove_component: Vec<String>,
    pub set: Vec<(Vec<Seg>, Tag)>,
    pub remove: Vec<Vec<Seg>>,
    pub fix: bool,
}

impl Ops {
    pub fn is_empty(&self) -> bool {
        !(self.delete || self.fix || self.strip_enchants || self.clamp_enchants)
            && self.set_id.is_none()
            && self.set_count.is_none()
            && self.remove_enchant.is_empty()
            && self.set_enchant.is_empty()
            && self.remove_component.is_empty()
            && self.set.is_empty()
            && self.remove.is_empty()
    }

    pub fn needs_preset(&self) -> bool {
        self.fix || self.clamp_enchants
    }

    pub fn describe(&self) -> String {
        if self.delete {
            return "delete".to_string();
        }
        let mut parts: Vec<String> = Vec::new();
        if let Some(id) = &self.set_id {
            parts.push(format!("set id to {id}"));
        }
        if let Some(n) = self.set_count {
            parts.push(format!("set count to {n}"));
        }
        if self.strip_enchants {
            parts.push("strip all enchantments".into());
        }
        for e in &self.remove_enchant {
            parts.push(format!("remove {}", short_id(e)));
        }
        if self.clamp_enchants {
            parts.push("clamp enchantment levels".into());
        }
        for (e, l) in &self.set_enchant {
            parts.push(format!("set {} to {l}", short_id(e)));
        }
        for c in &self.remove_component {
            parts.push(format!("remove component {}", short_id(c)));
        }
        for (p, _) in &self.set {
            parts.push(format!("set {}", nbt::path_to_string(p)));
        }
        for p in &self.remove {
            parts.push(format!("remove {}", nbt::path_to_string(p)));
        }
        if self.fix {
            parts.push("fix what the scan flagged".into());
        }
        parts.join(", ")
    }
}

pub struct ApplyOptions {
    pub world: PathBuf,
    pub ops: Ops,
    pub preset: Option<Preset>,
    pub dry_run: bool,
    pub backup: bool,
    pub force: bool,
    pub yes: bool,
    pub interactive: bool,
    pub threads: Option<usize>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Status {
    Changed,
    Deleted,
    Unchanged,
    Skipped(String),
}

#[derive(Debug, Clone)]
pub struct Outcome {
    pub n: u64,
    pub status: Status,
    pub notes: Vec<String>,
}

fn skipped(rec: &Record, why: impl Into<String>) -> Outcome {
    Outcome { n: rec.n, status: Status::Skipped(why.into()), notes: Vec::new() }
}

/// Set the value at `path`, creating missing compounds on the way.
fn set_path(root: &mut Compound, path: &[Seg], value: Tag) -> Result<()> {
    let (last, dirs) = path.split_last().ok_or_else(|| anyhow!("empty path"))?;
    let Seg::Key(last) = last else {
        bail!("--set cannot replace a list element directly; set a field inside it instead");
    };
    let mut cur: &mut Compound = root;
    let mut i = 0;
    while i < dirs.len() {
        let Seg::Key(k) = &dirs[i] else {
            bail!("unexpected list index in {}", nbt::path_to_string(path));
        };
        if !cur.contains(k) {
            cur.insert(k, Tag::Compound(Compound::default()));
        }
        let mut tag = cur.get_mut(k).ok_or_else(|| anyhow!("{k} vanished"))?;
        i += 1;
        // `list[3]`: step through any indices that follow the key.
        while let Some(Seg::Idx(n)) = dirs.get(i) {
            tag = match tag {
                Tag::List(l) => l
                    .items
                    .get_mut(*n)
                    .ok_or_else(|| anyhow!("{} does not exist", nbt::path_to_string(&path[..=i])))?,
                _ => bail!("{} is not a list", nbt::path_to_string(&path[..i])),
            };
            i += 1;
        }
        cur = match tag {
            Tag::Compound(c) => c,
            _ => bail!("{} is not a compound", nbt::path_to_string(&path[..i])),
        };
    }
    cur.insert(last, value);
    Ok(())
}

/// Run the requested operations on one item. Returns (delete?, notes).
fn apply_ops(item: &mut Compound, rec: &Record, opts: &ApplyOptions, data_version: Option<i64>) -> Result<(bool, Vec<String>)> {
    let ops = &opts.ops;
    let mut notes = Vec::new();
    if ops.delete {
        return Ok((true, vec!["deleted".to_string()]));
    }
    if let Some(id) = &ops.set_id {
        item.insert("id", Tag::String(id.clone()));
        notes.push(format!("id -> {}", short_id(id)));
    }
    if let Some(n) = ops.set_count {
        preset::set_count(item, n);
        notes.push(format!("count -> {n}"));
    }
    if ops.strip_enchants {
        let k = item::edit_enchants(item, &mut |_, _, _| EnchEdit::Remove);
        if k > 0 {
            notes.push(format!("removed {k} enchantment(s)"));
        }
    }
    for name in &ops.remove_enchant {
        let k = item::edit_enchants(item, &mut |e, _, _| if e == name { EnchEdit::Remove } else { EnchEdit::Keep });
        if k > 0 {
            notes.push(format!("removed {}", short_id(name)));
        }
    }
    if ops.clamp_enchants {
        let preset = opts.preset.as_ref().ok_or_else(|| anyhow!("--clamp-enchants needs a preset"))?;
        let mut clamped = Vec::new();
        item::edit_enchants(item, &mut |e, level, _| match preset.max_level(e) {
            Some(max) if level > max => {
                clamped.push(format!("{} {level} -> {max}", short_id(e)));
                EnchEdit::Set(max)
            }
            _ => EnchEdit::Keep,
        });
        notes.append(&mut clamped);
    }
    for (name, level) in &ops.set_enchant {
        item::set_enchant(item, name, *level, data_version);
        notes.push(format!("{} -> {level}", short_id(name)));
    }
    for name in &ops.remove_component {
        if item::remove_component(item, name) {
            notes.push(format!("removed {}", short_id(name)));
        }
    }
    for (path, value) in &ops.set {
        set_path(item, path, value.clone())?;
        notes.push(format!("set {}", nbt::path_to_string(path)));
    }
    for path in &ops.remove {
        if preset::remove_path(item, path) {
            notes.push(format!("removed {}", nbt::path_to_string(path)));
        }
    }
    if ops.fix {
        let preset = opts.preset.as_ref().ok_or_else(|| anyhow!("--fix needs a preset"))?;
        // Only repair what the scan reported, so rules that were disabled for
        // the scan stay disabled here.
        let wanted: HashSet<&str> = rec.reasons.iter().map(|r| r.rule.as_str()).collect();
        let mut reasons = Vec::new();
        {
            let view = ItemView::new(item).ok_or_else(|| anyhow!("item lost its id"))?;
            let parents: Vec<&str> = vec!["a container"; rec.depth];
            preset.check(&view, &parents, &mut reasons);
        }
        for r in reasons.iter().filter(|r| wanted.contains(r.rule.id())) {
            if preset::apply_fix(item, &r.fix) {
                return Ok((true, vec![format!("deleted ({}: {})", r.rule.id(), r.detail)]));
            }
            notes.push(format!("fixed {}: {}", r.rule.id(), r.detail));
        }
    }
    Ok((false, notes))
}

/// What actually has to be removed to delete the item at `path`: usually the
/// item itself, but for `{slot, item}` wrappers and villager trades it is the
/// enclosing list entry.
fn deletion_target(root: &Compound, path: &[Seg]) -> Vec<Seg> {
    if let Some((Seg::Key(k), parent)) = path.split_last() {
        if matches!(parent.last(), Some(Seg::Idx(_))) {
            if let Some(pc) = nbt::compound_at(root, parent) {
                let wrapper = k == "item" && pc.iter().all(|(key, _)| matches!(key, "item" | "slot" | "Slot"));
                let trade = k == "sell" || k == "buy";
                if wrapper || trade {
                    return parent.to_vec();
                }
            }
        }
    }
    path.to_vec()
}

fn delete_at(root: &mut Compound, target: &[Seg]) -> bool {
    let Some((last, parent)) = target.split_last() else { return false };
    match last {
        Seg::Key(k) => nbt::compound_at_mut(root, parent).map_or(false, |c| c.remove(k).is_some()),
        Seg::Idx(i) => {
            // Mob equipment lists are positional: blank the slot instead of
            // shifting the others.
            let positional = matches!(parent.last(), Some(Seg::Key(k)) if k == "HandItems" || k == "ArmorItems");
            match nbt::get_path_mut(root, parent).and_then(Tag::as_list_mut) {
                Some(l) if *i < l.items.len() => {
                    if positional {
                        l.items[*i] = Tag::Compound(Compound::default());
                    } else {
                        l.items.remove(*i);
                    }
                    true
                }
                _ => false,
            }
        }
    }
}

/// Apply the operations to every selected item inside one NBT tree.
/// Returns the outcomes and whether the tree changed.
pub fn process_tree(root: &mut Compound, recs: &[&Record], opts: &ApplyOptions) -> (Vec<Outcome>, bool) {
    let data_version = root.int("DataVersion").or_else(|| root.compound("Data").and_then(|d| d.int("DataVersion")));
    let mut outcomes = Vec::new();

    // 1. Verify everything against the scan before touching anything, since
    //    editing an item changes the fingerprint of whatever contains it.
    let mut valid: Vec<&Record> = Vec::new();
    for &rec in recs {
        match nbt::get_path(root, &rec.path) {
            Some(Tag::Compound(c)) if format!("{:016x}", nbt::fingerprint(c)) == rec.hash => valid.push(rec),
            Some(_) => outcomes.push(skipped(rec, "changed since the scan - rescan first")),
            None => outcomes.push(skipped(rec, "no longer there - rescan first")),
        }
    }

    // 2. In-place edits, innermost items first so outer edits cannot move them.
    valid.sort_by(|a, b| b.path.len().cmp(&a.path.len()));
    let mut deletions: Vec<(Vec<Seg>, u64)> = Vec::new();
    let mut changed = false;
    for rec in valid {
        let Some(item) = nbt::get_path_mut(root, &rec.path).and_then(Tag::as_compound_mut) else {
            outcomes.push(skipped(rec, "removed by an earlier operation"));
            continue;
        };
        let before = nbt::fingerprint(item);
        // Work on a copy so a failing operation leaves the item untouched.
        let mut edited = item.clone();
        match apply_ops(&mut edited, rec, opts, data_version) {
            Err(e) => outcomes.push(skipped(rec, format!("{e:#}"))),
            Ok((true, notes)) => {
                deletions.push((rec.path.clone(), rec.n));
                outcomes.push(Outcome { n: rec.n, status: Status::Deleted, notes });
            }
            Ok((false, notes)) => {
                if nbt::fingerprint(&edited) == before {
                    outcomes.push(Outcome { n: rec.n, status: Status::Unchanged, notes });
                } else {
                    *item = edited;
                    changed = true;
                    outcomes.push(Outcome { n: rec.n, status: Status::Changed, notes });
                }
            }
        }
    }

    // 3. Deletions, last-in-document-order first so list indices stay valid.
    let mut targets: Vec<Vec<Seg>> = deletions.iter().map(|(p, _)| deletion_target(root, p)).collect();
    targets.sort_by(|a, b| nbt::cmp_paths(b, a));
    targets.dedup();
    for t in &targets {
        // A target nested in an already-deleted one is simply gone.
        changed |= delete_at(root, t);
    }
    (outcomes, changed)
}

fn timestamp() -> String {
    let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let days = (secs / 86400) as i64;
    let rem = secs % 86400;
    // Civil date from day count (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + if month <= 2 { 1 } else { 0 };
    format!("{year:04}{month:02}{day:02}-{:02}{:02}{:02}", rem / 3600, rem % 3600 / 60, rem % 60)
}

struct Run<'a> {
    opts: &'a ApplyOptions,
    backup_dir: Option<PathBuf>,
}

impl Run<'_> {
    fn backup(&self, path: &Path, rel: &str) -> Result<()> {
        let Some(dir) = &self.backup_dir else { return Ok(()) };
        let dest = dir.join(rel);
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
        }
        fs::copy(path, &dest).with_context(|| format!("backing up {} to {}", path.display(), dest.display()))?;
        Ok(())
    }

    fn region(&self, rel: &str, recs: &[&Record]) -> Result<Vec<Outcome>> {
        let path = self.opts.world.join(rel);
        let region = Region::open(&path)?;
        let mut by_chunk: BTreeMap<usize, Vec<&Record>> = BTreeMap::new();
        let mut outcomes = Vec::new();
        for &rec in recs {
            match rec.chunk {
                Some([cx, cz]) if (cx.div_euclid(32), cz.div_euclid(32)) == (region.rx, region.rz) => {
                    by_chunk.entry(region::chunk_index(cx, cz)).or_default().push(rec);
                }
                _ => outcomes.push(skipped(rec, "finding has no valid chunk position")),
            }
        }
        let mut inflater = Inflater::default();
        let mut buf = Vec::new();
        let mut replace: HashMap<usize, Vec<u8>> = HashMap::new();
        let mut external: Vec<usize> = Vec::new();
        for (idx, recs) in &by_chunk {
            let parsed = match region.read(*idx, &mut inflater, &mut buf) {
                Ok(true) => nbt::parse(&buf),
                Ok(false) => Err(anyhow!("chunk no longer exists")),
                Err(e) => Err(e),
            };
            let (name, mut root) = match parsed {
                Ok(v) => v,
                Err(e) => {
                    outcomes.extend(recs.iter().map(|r| skipped(r, format!("{e:#}"))));
                    continue;
                }
            };
            let (mut out, changed) = process_tree(&mut root, recs, self.opts);
            if changed {
                match nbt::write_root(&name, &root) {
                    Ok(bytes) => {
                        replace.insert(*idx, bytes);
                        if matches!(region.raw(*idx), Ok(Some(RawChunk::External { .. }))) {
                            external.push(*idx);
                        }
                    }
                    Err(e) => {
                        // Could not serialise: report every item of the chunk as untouched.
                        out = recs.iter().map(|r| skipped(r, format!("{e:#}"))).collect();
                    }
                }
            }
            outcomes.append(&mut out);
        }
        if !replace.is_empty() && !self.opts.dry_run {
            self.backup(&path, rel)?;
            for idx in external {
                let (cx, cz) = region.chunk_pos(idx);
                let name = format!("c.{cx}.{cz}.mcc");
                let rel_dir = rel.rsplit_once('/').map(|(d, _)| d).unwrap_or("");
                let mcc = region.dir.join(&name);
                if mcc.exists() {
                    self.backup(&mcc, &format!("{rel_dir}/{name}"))?;
                }
            }
            drop(region);
            let dropped = region::rewrite(&path, &replace)?;
            if dropped > 0 {
                eprintln!("warning: {rel}: {dropped} chunk(s) were already unreadable and have been dropped");
            }
        }
        Ok(outcomes)
    }

    fn dat(&self, rel: &str, recs: &[&Record]) -> Result<Vec<Outcome>> {
        let path = self.opts.world.join(rel);
        let (bytes, compression) = region::read_dat(&path)?;
        let (name, mut root) = nbt::parse(&bytes)?;
        let (outcomes, changed) = process_tree(&mut root, recs, self.opts);
        if changed && !self.opts.dry_run {
            let bytes = nbt::write_root(&name, &root)?;
            self.backup(&path, rel)?;
            region::write_dat(&path, &bytes, compression)?;
        }
        Ok(outcomes)
    }

    fn file(&self, rel: &str, recs: &[&Record]) -> Vec<Outcome> {
        let result = if rel.ends_with(".mca") { self.region(rel, recs) } else { self.dat(rel, recs) };
        result.unwrap_or_else(|e| recs.iter().map(|r| skipped(r, format!("{rel}: {e:#}"))).collect())
    }
}

fn ask(prompt: &str) -> Result<String> {
    eprint!("{prompt}");
    std::io::stderr().flush().ok();
    let mut line = String::new();
    if std::io::stdin().lock().read_line(&mut line)? == 0 {
        bail!("no answer on standard input (use --yes to run without prompting)");
    }
    Ok(line.trim().to_ascii_lowercase())
}

/// Step through the selection asking y/n for each finding.
fn pick_interactively<'a>(selected: Vec<&'a Record>, what: &str) -> Result<Vec<&'a Record>> {
    if !std::io::stdin().is_terminal() {
        bail!("--interactive needs a terminal");
    }
    let total = selected.len();
    let mut keep = Vec::new();
    let mut iter = selected.into_iter().enumerate();
    while let Some((i, rec)) = iter.next() {
        eprintln!("\n[{}/{total}] #{}  {}\n    where: {}\n    why:   {}", i + 1, rec.n, rec.item_text(), rec.location(), rec.why());
        loop {
            match ask(&format!("    {what}? [y]es / [n]o / [a]ll remaining / [q]uit: "))?.as_str() {
                "y" | "yes" => {
                    keep.push(rec);
                    break;
                }
                "n" | "no" | "" => break,
                "a" | "all" => {
                    keep.push(rec);
                    keep.extend(iter.by_ref().map(|(_, r)| r));
                    break;
                }
                "q" | "quit" => return Ok(keep),
                _ => continue,
            }
        }
    }
    Ok(keep)
}

pub fn run(opts: ApplyOptions, selected: Vec<&Record>) -> Result<()> {
    if opts.ops.is_empty() {
        bail!(
            "no action given. Use one or more of: --fix, --delete, --set-count, --clamp-enchants, --strip-enchants, \
             --remove-enchant, --set-enchant, --remove-component, --set, --remove, --set-id"
        );
    }
    if selected.is_empty() {
        println!("Nothing selected.");
        return Ok(());
    }
    if !opts.world.is_dir() {
        bail!("world folder {} not found (use --world if it has moved since the scan)", opts.world.display());
    }
    let action = opts.ops.describe();
    let selected = if opts.interactive { pick_interactively(selected, &action)? } else { selected };
    if selected.is_empty() {
        println!("Nothing selected.");
        return Ok(());
    }

    let mut by_file: BTreeMap<&str, Vec<&Record>> = BTreeMap::new();
    for &rec in &selected {
        by_file.entry(rec.file.as_str()).or_default().push(rec);
    }

    if !opts.dry_run {
        if let Some(pid) = world_lock(&opts.world) {
            if !opts.force {
                bail!(
                    "this world is open in a running game/server (pid {pid}). Stop it first: the server would \
                     overwrite the changes and can corrupt chunks it has loaded. (--force overrides; --dry-run is always safe)"
                );
            }
            eprintln!("warning: world is in use by pid {pid}; continuing because of --force");
        }
        if !opts.yes && !opts.interactive {
            eprintln!("About to {action} on {} item stack(s) in {} file(s) under {}", selected.len(), by_file.len(), opts.world.display());
            if !std::io::stdin().is_terminal() {
                bail!("refusing to modify the world without confirmation: pass --yes (or try --dry-run)");
            }
            if !matches!(ask("Proceed? [y/N] ")?.as_str(), "y" | "yes") {
                println!("Aborted; nothing was changed.");
                return Ok(());
            }
        }
    }

    let backup_dir = if opts.backup && !opts.dry_run {
        Some(opts.world.join("mcscan-backups").join(timestamp()))
    } else {
        None
    };
    let run = Run { opts: &opts, backup_dir: backup_dir.clone() };

    let mut builder = rayon::ThreadPoolBuilder::new();
    if let Some(n) = opts.threads {
        builder = builder.num_threads(n.max(1));
    }
    let pool = builder.build().context("starting worker threads")?;
    let groups: Vec<(&str, Vec<&Record>)> = by_file.into_iter().collect();
    let mut outcomes: Vec<Outcome> =
        pool.install(|| groups.par_iter().flat_map_iter(|(rel, recs)| run.file(rel, recs)).collect());
    outcomes.sort_by_key(|o| o.n);

    let by_n: HashMap<u64, &Record> = selected.iter().map(|r| (r.n, *r)).collect();
    let (mut changed, mut deleted, mut unchanged, mut skips) = (0, 0, 0, 0);
    let stdout = std::io::stdout();
    let mut w = std::io::BufWriter::new(stdout.lock());
    for o in &outcomes {
        let Some(rec) = by_n.get(&o.n) else { continue };
        let verb = match &o.status {
            Status::Changed => {
                changed += 1;
                if opts.dry_run { "would change" } else { "changed" }
            }
            Status::Deleted => {
                deleted += 1;
                if opts.dry_run { "would delete" } else { "deleted" }
            }
            Status::Unchanged => {
                unchanged += 1;
                "nothing to do"
            }
            Status::Skipped(_) => {
                skips += 1;
                "SKIPPED"
            }
        };
        let detail = match &o.status {
            Status::Skipped(why) => why.clone(),
            _ => o.notes.join("; "),
        };
        writeln!(w, "#{:<5} {:<13} {}  [{}]  {}", o.n, verb, rec.item_text(), rec.location(), detail)?;
    }
    writeln!(w)?;
    let prefix = if opts.dry_run { "DRY RUN - nothing was written. Would have: " } else { "Done: " };
    writeln!(w, "{prefix}{changed} changed, {deleted} deleted, {unchanged} already fine, {skips} skipped.")?;
    if let Some(dir) = backup_dir {
        if changed + deleted > 0 && dir.exists() {
            writeln!(w, "Originals of every modified file are in {}", dir.display())?;
        }
    }
    if skips > 0 {
        writeln!(w, "Skipped items no longer match the scan; run `mcscan scan` again to pick them up.")?;
    }
    w.flush()?;
    Ok(())
}

fn world_lock(world: &Path) -> Option<i64> {
    crate::world::lock_holder(world)
}

/// `mcscan show`: print the current on-disk NBT of the selected findings.
pub fn show(world: &Path, selected: &[&Record], pretty: bool) -> Result<()> {
    let mut inflater = Inflater::default();
    let mut buf = Vec::new();
    for rec in selected {
        println!("#{}  {}  [{}]", rec.n, rec.item_text(), rec.location());
        println!("  file: {}{}", rec.file, rec.chunk.map(|[x, z]| format!("  chunk {x},{z}")).unwrap_or_default());
        println!("  path: {}", nbt::path_to_string(&rec.path));
        if !rec.reasons.is_empty() {
            println!("  why:  {}", rec.why());
        }
        let path = world.join(&rec.file);
        let root = if rec.file.ends_with(".mca") {
            let region = Region::open(&path)?;
            let [cx, cz] = rec.chunk.ok_or_else(|| anyhow!("finding #{} has no chunk position", rec.n))?;
            if !region.read(region::chunk_index(cx, cz), &mut inflater, &mut buf)? {
                println!("  (chunk no longer exists)\n");
                continue;
            }
            nbt::parse(&buf)?.1
        } else {
            nbt::parse(&region::read_dat(&path)?.0)?.1
        };
        match nbt::get_path(&root, &rec.path) {
            Some(Tag::Compound(c)) => {
                if format!("{:016x}", nbt::fingerprint(c)) != rec.hash {
                    println!("  note: the item has changed since the scan");
                }
                if pretty {
                    let mut s = String::new();
                    pretty_snbt(&Tag::Compound(c.clone()), 1, &mut s);
                    println!("  {s}\n");
                } else {
                    println!("  {}\n", nbt::snbt(c));
                }
            }
            _ => println!("  (item is no longer there)\n"),
        }
    }
    Ok(())
}

fn pretty_snbt(tag: &Tag, depth: usize, out: &mut String) {
    let indent = "  ".repeat(depth + 1);
    match tag {
        Tag::Compound(c) if !c.is_empty() => {
            out.push_str("{\n");
            for (i, (k, v)) in c.iter().enumerate() {
                out.push_str(&indent);
                if k.chars().all(|ch| ch.is_ascii_alphanumeric() || "._+-".contains(ch)) && !k.is_empty() {
                    out.push_str(k);
                } else {
                    out.push_str(&format!("{k:?}"));
                }
                out.push_str(": ");
                pretty_snbt(v, depth + 1, out);
                if i + 1 < c.len() {
                    out.push(',');
                }
                out.push('\n');
            }
            out.push_str(&"  ".repeat(depth));
            out.push('}');
        }
        Tag::List(l) if l.items.iter().any(|t| matches!(t, Tag::Compound(_) | Tag::List(_))) => {
            out.push_str("[\n");
            for (i, v) in l.items.iter().enumerate() {
                out.push_str(&indent);
                pretty_snbt(v, depth + 1, out);
                if i + 1 < l.items.len() {
                    out.push(',');
                }
                out.push('\n');
            }
            out.push_str(&"  ".repeat(depth));
            out.push(']');
        }
        other => nbt::write_snbt(other, out),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::item::testutil::item;
    use crate::item::{own_path, walk_items};
    use crate::results::ReasonRec;

    fn opts(ops: Ops) -> ApplyOptions {
        ApplyOptions {
            world: PathBuf::new(),
            ops,
            preset: Some(Preset::load("illegal").unwrap()),
            dry_run: false,
            backup: false,
            force: false,
            yes: true,
            interactive: false,
            threads: None,
        }
    }

    /// Run the illegal preset over a tree the way `scan` does.
    fn scan(root: &Compound) -> Vec<Record> {
        let preset = Preset::load("illegal").unwrap();
        let mut out = Vec::new();
        walk_items(root, &mut |hit| {
            let view = ItemView::new(hit.item).unwrap();
            let mut reasons = Vec::new();
            preset.check(&view, hit.parents, &mut reasons);
            if reasons.is_empty() {
                return;
            }
            out.push(Record {
                n: out.len() as u64 + 1,
                id: view.id.to_string(),
                count: view.count,
                reasons: reasons.iter().map(|r| ReasonRec { rule: r.rule.id().into(), detail: r.detail.clone() }).collect(),
                info: String::new(),
                source: "block_entity".into(),
                holder: hit.holder.map(str::to_string),
                dim: None,
                pos: hit.pos,
                slot: hit.slot,
                uuid: None,
                depth: hit.parents.len(),
                file: "region/r.0.0.mca".into(),
                chunk: Some([0, 0]),
                path: own_path(hit.path),
                hash: format!("{:016x}", nbt::fingerprint(hit.item)),
                snbt: None,
            });
        });
        out
    }

    const CHUNK: &str = r#"{DataVersion:5023,block_entities:[{id:"minecraft:chest",x:1,y:2,z:3,Items:[
        {Slot:0b,id:"minecraft:bedrock",count:1},
        {Slot:1b,id:"minecraft:stone",count:64},
        {Slot:2b,id:"minecraft:diamond_sword",count:1,components:{"minecraft:enchantments":{"minecraft:sharpness":255}}},
        {Slot:3b,id:"minecraft:red_shulker_box",count:1,components:{"minecraft:container":[
            {slot:0,item:{id:"minecraft:barrier",count:1}},
            {slot:1,item:{id:"minecraft:dirt",count:1}},
            {slot:2,item:{id:"minecraft:blue_shulker_box",count:1}}]}},
        {Slot:4b,id:"minecraft:command_block",count:1}]}],
      entities:[{id:"minecraft:zombie",Pos:[0d,0d,0d],UUID:[I;1,2,3,4],HandItems:[{id:"minecraft:barrier",Count:1b},{}]}]}"#;

    #[test]
    fn fix_everything() {
        let mut root = item(CHUNK);
        let records = scan(&root);
        assert_eq!(records.len(), 6, "{records:#?}");
        let refs: Vec<&Record> = records.iter().collect();
        let (outcomes, changed) = process_tree(&mut root, &refs, &opts(Ops { fix: true, ..Default::default() }));
        assert!(changed);
        assert!(outcomes.iter().all(|o| !matches!(o.status, Status::Skipped(_))), "{outcomes:#?}");
        assert_eq!(
            root,
            item(
                r#"{DataVersion:5023,block_entities:[{id:"minecraft:chest",x:1,y:2,z:3,Items:[
                    {Slot:1b,id:"minecraft:stone",count:64},
                    {Slot:2b,id:"minecraft:diamond_sword",count:1,components:{"minecraft:enchantments":{"minecraft:sharpness":5}}},
                    {Slot:3b,id:"minecraft:red_shulker_box",count:1,components:{"minecraft:container":[
                        {slot:1,item:{id:"minecraft:dirt",count:1}}]}}]}],
                  entities:[{id:"minecraft:zombie",Pos:[0d,0d,0d],UUID:[I;1,2,3,4],HandItems:[{},{}]}]}"#
            )
        );
        assert!(scan(&root).is_empty());
    }

    #[test]
    fn stale_results_are_refused() {
        let mut root = item(CHUNK);
        let records = scan(&root);
        // Someone swaps the sword for a legit one between scan and apply.
        let sword = records.iter().find(|r| r.id == "minecraft:diamond_sword").unwrap();
        let live = nbt::get_path_mut(&mut root, &sword.path).unwrap().as_compound_mut().unwrap();
        live.remove("components");
        let before = root.clone();
        let (outcomes, changed) = process_tree(&mut root, &[sword], &opts(Ops { delete: true, ..Default::default() }));
        assert!(!changed);
        assert_eq!(root, before);
        assert!(matches!(outcomes[0].status, Status::Skipped(_)));
    }

    #[test]
    fn manual_operations() {
        let mut root = item(CHUNK);
        let records = scan(&root);
        let sword: Vec<&Record> = records.iter().filter(|r| r.id == "minecraft:diamond_sword").collect();
        let ops = Ops {
            set_count: Some(1),
            remove_enchant: vec!["minecraft:sharpness".into()],
            set_enchant: vec![("minecraft:looting".into(), 3)],
            set: vec![(nbt::parse_path("components.minecraft:damage").unwrap(), Tag::Int(10))],
            ..Default::default()
        };
        let (outcomes, changed) = process_tree(&mut root, &sword, &opts(ops));
        assert!(changed);
        assert_eq!(outcomes[0].status, Status::Changed);
        assert_eq!(
            nbt::get_path(&root, &sword[0].path),
            Some(&Tag::Compound(item(
                r#"{Slot:2b,id:"minecraft:diamond_sword",count:1,components:{"minecraft:enchantments":{"minecraft:looting":3},"minecraft:damage":10}}"#
            )))
        );
    }

    #[test]
    fn deleting_parent_and_child_together() {
        let mut root = item(CHUNK);
        let mut records = scan(&root);
        // Also select the outer shulker box itself, which the preset did not flag.
        let outer_path = nbt::parse_path("block_entities[0].Items[3]").unwrap();
        let outer = nbt::get_path(&root, &outer_path).unwrap().as_compound().unwrap();
        let mut fake = records[0].clone();
        fake.n = 99;
        fake.path = outer_path.clone();
        fake.hash = format!("{:016x}", nbt::fingerprint(outer));
        records.push(fake);
        let refs: Vec<&Record> = records.iter().collect();
        let (outcomes, _) = process_tree(&mut root, &refs, &opts(Ops { delete: true, ..Default::default() }));
        assert!(outcomes.iter().all(|o| o.status == Status::Deleted), "{outcomes:#?}");
        let items = nbt::get_path(&root, &nbt::parse_path("block_entities[0].Items").unwrap()).unwrap().as_list().unwrap();
        assert_eq!(items.items, vec![Tag::Compound(item(r#"{Slot:1b,id:"minecraft:stone",count:64}"#))]);
    }

    #[test]
    fn timestamps_look_like_dates() {
        let t = timestamp();
        assert_eq!(t.len(), 15);
        assert!(t.starts_with("20"));
    }
}
