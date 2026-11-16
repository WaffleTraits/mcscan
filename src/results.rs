//! The results file written by `scan` (JSON Lines: one header line, then one
//! finding per line), selecting findings from it, and printing them.

use crate::filter::id_glob;
use crate::item::short_id;
use crate::nbt::Seg;
use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io::{BufRead, Write};

pub const FORMAT_VERSION: u32 = 1;

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Header {
    /// Results format version; also marks the line as the header.
    pub mcscan: u32,
    pub world: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preset: Option<String>,
    #[serde(default)]
    pub filters: Vec<String>,
    #[serde(default)]
    pub scanned_at: u64,
    #[serde(default)]
    pub items_scanned: u64,
    #[serde(default)]
    pub matches: u64,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct ReasonRec {
    pub rule: String,
    pub detail: String,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Record {
    /// Stable number used to select this finding.
    pub n: u64,
    pub id: String,
    pub count: i64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reasons: Vec<ReasonRec>,
    /// Enchantments / custom name, for humans.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub info: String,
    pub source: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub holder: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dim: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pos: Option<[i32; 3]>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slot: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uuid: Option<String>,
    /// How many items this one is nested inside (0 = directly in an inventory).
    #[serde(default)]
    pub depth: usize,
    /// World-relative file holding the item.
    pub file: String,
    /// Absolute chunk coordinates, for region files.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chunk: Option<[i32; 2]>,
    /// Location of the item compound inside the chunk / file NBT.
    pub path: Vec<Seg>,
    /// Fingerprint of the item as scanned; `apply` refuses to touch an item
    /// whose fingerprint no longer matches.
    pub hash: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snbt: Option<String>,
}

impl Record {
    /// "chest @ overworld 10 64 -3 slot 5"
    pub fn location(&self) -> String {
        let mut s = String::new();
        match (&self.uuid, &self.holder) {
            (Some(u), _) => {
                s.push_str("player ");
                s.push_str(u);
            }
            (None, Some(h)) => s.push_str(short_id(h)),
            (None, None) => s.push_str(&self.source),
        }
        if self.dim.is_some() || self.pos.is_some() {
            s.push_str(" @");
            if let Some(d) = &self.dim {
                s.push(' ');
                s.push_str(short_id(d));
            }
            if let Some([x, y, z]) = self.pos {
                s.push_str(&format!(" {x} {y} {z}"));
            }
        }
        if let Some(slot) = self.slot {
            s.push_str(&format!(" slot {slot}"));
        }
        if self.depth > 0 {
            s.push_str(&format!(" (nested x{})", self.depth));
        }
        s
    }

    pub fn item_text(&self) -> String {
        if self.count == 1 {
            short_id(&self.id).to_string()
        } else {
            format!("{} x{}", short_id(&self.id), self.count)
        }
    }

    pub fn why(&self) -> String {
        let mut parts: Vec<String> = self.reasons.iter().map(|r| format!("{}: {}", r.rule, r.detail)).collect();
        if parts.is_empty() && !self.info.is_empty() {
            parts.push(self.info.clone());
        }
        parts.join("; ")
    }
}

pub fn write<W: Write>(mut w: W, header: &Header, records: &[Record]) -> Result<()> {
    serde_json::to_writer(&mut w, header)?;
    w.write_all(b"\n")?;
    for r in records {
        serde_json::to_writer(&mut w, r)?;
        w.write_all(b"\n")?;
    }
    w.flush()?;
    Ok(())
}

/// Load a results file (`-` reads standard input).
pub fn load(path: &str) -> Result<(Header, Vec<Record>)> {
    let reader: Box<dyn BufRead> = if path == "-" {
        Box::new(std::io::BufReader::new(std::io::stdin()))
    } else {
        Box::new(std::io::BufReader::new(
            std::fs::File::open(path).with_context(|| format!("opening results file {path}"))?,
        ))
    };
    let mut header = None;
    let mut records = Vec::new();
    for (i, line) in reader.lines().enumerate() {
        let line = line.map_err(|e| anyhow!("{path} is not an mcscan results file ({e})"))?;
        if line.trim().is_empty() {
            continue;
        }
        if header.is_none() {
            let h: Header = serde_json::from_str(&line)
                .map_err(|e| anyhow!("{path} is not an mcscan results file (bad header: {e})"))?;
            if h.mcscan != FORMAT_VERSION {
                bail!("{path} uses results format {} but this mcscan reads format {FORMAT_VERSION}", h.mcscan);
            }
            header = Some(h);
            continue;
        }
        records.push(serde_json::from_str(&line).with_context(|| format!("{path}: line {}", i + 1))?);
    }
    let header = header.ok_or_else(|| anyhow!("{path} is empty"))?;
    Ok((header, records))
}

// ---------------------------------------------------------------------------
// Selection
// ---------------------------------------------------------------------------

/// Which findings to act on. Different options narrow the selection (AND);
/// several values for one option widen it (OR).
#[derive(clap::Args, Debug, Default, Clone)]
pub struct SelectArgs {
    /// Finding numbers from `list`, e.g. 3  or  1,4,10-20  or  50-
    #[arg(short = 'n', long = "select", value_name = "NUMS", value_delimiter = ',', help_heading = "Selection")]
    pub select: Vec<String>,
    /// Finding numbers to leave out (same syntax as --select)
    #[arg(long, value_name = "NUMS", value_delimiter = ',', help_heading = "Selection")]
    pub exclude: Vec<String>,
    /// Only findings that broke this preset rule (see `mcscan rules`)
    #[arg(long, value_name = "RULE", value_delimiter = ',', help_heading = "Selection")]
    pub rule: Vec<String>,
    /// Only these item ids; globs allowed, e.g. '*_spawn_egg'
    #[arg(long, value_name = "GLOB", value_delimiter = ',', help_heading = "Selection")]
    pub id: Vec<String>,
    /// Only items held by this block entity / entity, e.g. chest, 'item_frame', player
    #[arg(long, value_name = "GLOB", value_delimiter = ',', help_heading = "Selection")]
    pub holder: Vec<String>,
    /// Only items from: block_entity, entity, player, level, data
    #[arg(long, value_name = "SOURCE", value_delimiter = ',', help_heading = "Selection")]
    pub source: Vec<String>,
    /// Only this dimension, e.g. the_nether
    #[arg(long, value_name = "DIM", value_delimiter = ',', help_heading = "Selection")]
    pub dim: Vec<String>,
    /// Only this player's inventory / ender chest (UUID glob)
    #[arg(long, value_name = "UUID", value_delimiter = ',', help_heading = "Selection")]
    pub uuid: Vec<String>,
    /// Every finding in the file
    #[arg(long, help_heading = "Selection")]
    pub all: bool,
}

fn parse_ranges(specs: &[String]) -> Result<Vec<(u64, u64)>> {
    let mut out = Vec::new();
    for s in specs {
        let s = s.trim();
        if s.is_empty() {
            continue;
        }
        let bad = || anyhow!("bad finding number {s:?} (use 7, 3-9 or 50-)");
        let range = match s.split_once('-') {
            None => {
                let n = s.parse().map_err(|_| bad())?;
                (n, n)
            }
            Some((a, b)) => {
                let lo = if a.is_empty() { 1 } else { a.parse().map_err(|_| bad())? };
                let hi = if b.is_empty() { u64::MAX } else { b.parse().map_err(|_| bad())? };
                (lo, hi)
            }
        };
        out.push(range);
    }
    Ok(out)
}

pub struct Selector {
    select: Vec<(u64, u64)>,
    exclude: Vec<(u64, u64)>,
    args: SelectArgs,
}

impl SelectArgs {
    /// True if no option narrows the selection (and --all was not given).
    pub fn is_unrestricted(&self) -> bool {
        self.select.is_empty()
            && self.rule.is_empty()
            && self.id.is_empty()
            && self.holder.is_empty()
            && self.source.is_empty()
            && self.dim.is_empty()
            && self.uuid.is_empty()
    }

    pub fn compile(&self) -> Result<Selector> {
        Ok(Selector { select: parse_ranges(&self.select)?, exclude: parse_ranges(&self.exclude)?, args: self.clone() })
    }
}

impl Selector {
    pub fn matches(&self, r: &Record) -> bool {
        let in_ranges = |rs: &[(u64, u64)]| rs.iter().any(|(lo, hi)| (*lo..=*hi).contains(&r.n));
        let a = &self.args;
        if !self.select.is_empty() && !in_ranges(&self.select) {
            return false;
        }
        if in_ranges(&self.exclude) {
            return false;
        }
        if !a.rule.is_empty() && !r.reasons.iter().any(|x| a.rule.iter().any(|w| w == &x.rule)) {
            return false;
        }
        let globs = |pats: &[String], value: Option<&str>| {
            pats.is_empty() || value.map_or(false, |v| pats.iter().any(|p| id_glob(p, v)))
        };
        globs(&a.id, Some(r.id.as_str()))
            && globs(&a.holder, r.holder.as_deref())
            && globs(&a.source, Some(r.source.as_str()))
            && globs(&a.dim, r.dim.as_deref())
            && globs(&a.uuid, r.uuid.as_deref())
    }

    pub fn pick<'a>(&self, records: &'a [Record]) -> Vec<&'a Record> {
        records.iter().filter(|r| self.matches(r)).collect()
    }
}

// ---------------------------------------------------------------------------
// Printing
// ---------------------------------------------------------------------------

fn pad(s: &str, width: usize) -> String {
    let len = s.chars().count();
    let mut out = s.to_string();
    out.extend(std::iter::repeat(' ').take(width.saturating_sub(len)));
    out
}

pub fn print_table<W: Write>(mut w: W, records: &[&Record]) -> Result<()> {
    let rows: Vec<[String; 4]> =
        records.iter().map(|r| [format!("#{}", r.n), r.item_text(), r.location(), r.why()]).collect();
    let mut width = [1usize, 4, 5];
    for row in &rows {
        for (i, col) in width.iter_mut().enumerate() {
            *col = (*col).max(row[i].chars().count());
        }
    }
    writeln!(w, "{}  {}  {}  WHY", pad("#", width[0]), pad("ITEM", width[1]), pad("WHERE", width[2]))?;
    for row in &rows {
        writeln!(w, "{}  {}  {}  {}", pad(&row[0], width[0]), pad(&row[1], width[1]), pad(&row[2], width[2]), row[3])?;
    }
    Ok(())
}

fn csv_field(s: &str) -> String {
    if s.contains(|c: char| c == ',' || c == '"' || c == '\n') {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

pub fn print_csv<W: Write>(mut w: W, records: &[&Record]) -> Result<()> {
    writeln!(w, "n,id,count,rules,details,source,holder,dim,x,y,z,slot,uuid,depth,file")?;
    for r in records {
        let rules: Vec<&str> = r.reasons.iter().map(|x| x.rule.as_str()).collect();
        let details: Vec<&str> = r.reasons.iter().map(|x| x.detail.as_str()).collect();
        let pos = |i: usize| r.pos.map(|p| p[i].to_string()).unwrap_or_default();
        let fields = [
            r.n.to_string(),
            r.id.clone(),
            r.count.to_string(),
            rules.join(" "),
            details.join("; "),
            r.source.clone(),
            r.holder.clone().unwrap_or_default(),
            r.dim.clone().unwrap_or_default(),
            pos(0),
            pos(1),
            pos(2),
            r.slot.map(|s| s.to_string()).unwrap_or_default(),
            r.uuid.clone().unwrap_or_default(),
            r.depth.to_string(),
            r.file.clone(),
        ];
        let line: Vec<String> = fields.iter().map(|f| csv_field(f)).collect();
        writeln!(w, "{}", line.join(","))?;
    }
    Ok(())
}

pub fn print_json<W: Write>(mut w: W, records: &[&Record]) -> Result<()> {
    for r in records {
        serde_json::to_writer(&mut w, r)?;
        w.write_all(b"\n")?;
    }
    Ok(())
}

/// Counts per rule and per item id.
pub fn print_summary<W: Write>(mut w: W, records: &[&Record]) -> Result<()> {
    let mut by_rule: BTreeMap<&str, u64> = BTreeMap::new();
    let mut by_item: BTreeMap<&str, u64> = BTreeMap::new();
    let mut by_source: BTreeMap<&str, u64> = BTreeMap::new();
    for r in records {
        for reason in &r.reasons {
            *by_rule.entry(reason.rule.as_str()).or_default() += 1;
        }
        *by_item.entry(short_id(&r.id)).or_default() += 1;
        *by_source.entry(r.source.as_str()).or_default() += 1;
    }
    let section = |w: &mut W, title: &str, map: BTreeMap<&str, u64>, limit: usize| -> Result<()> {
        if map.is_empty() {
            return Ok(());
        }
        let mut rows: Vec<(&str, u64)> = map.into_iter().collect();
        rows.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
        writeln!(w, "{title}")?;
        for (name, count) in rows.iter().take(limit) {
            writeln!(w, "  {count:>7}  {name}")?;
        }
        if rows.len() > limit {
            writeln!(w, "  {:>7}  ... {} more", "", rows.len() - limit)?;
        }
        Ok(())
    };
    writeln!(w, "{} finding(s)", records.len())?;
    section(&mut w, "By rule:", by_rule, usize::MAX)?;
    section(&mut w, "By source:", by_source, usize::MAX)?;
    section(&mut w, "By item:", by_item, 25)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(n: u64, id: &str, rule: &str) -> Record {
        Record {
            n,
            id: id.to_string(),
            count: 1,
            reasons: vec![ReasonRec { rule: rule.to_string(), detail: "d".into() }],
            info: String::new(),
            source: "block_entity".into(),
            holder: Some("minecraft:chest".into()),
            dim: Some("minecraft:overworld".into()),
            pos: Some([1, 2, 3]),
            slot: Some(5),
            uuid: None,
            depth: 0,
            file: "region/r.0.0.mca".into(),
            chunk: Some([0, 0]),
            path: vec![Seg::key("block_entities"), Seg::Idx(0), Seg::key("Items"), Seg::Idx(2)],
            hash: "00".into(),
            snbt: None,
        }
    }

    fn select(args: SelectArgs, records: &[Record]) -> Vec<u64> {
        args.compile().unwrap().pick(records).iter().map(|r| r.n).collect()
    }

    #[test]
    fn selection() {
        let records: Vec<Record> = vec![
            rec(1, "minecraft:bedrock", "unobtainable_item"),
            rec(2, "minecraft:diamond_sword", "enchant_over_max"),
            rec(3, "minecraft:zombie_spawn_egg", "unobtainable_item"),
            rec(4, "minecraft:stick", "enchant_wrong_item"),
        ];
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        assert_eq!(select(SelectArgs::default(), &records), vec![1, 2, 3, 4]);
        assert_eq!(select(SelectArgs { select: s(&["1", "3-"]), ..Default::default() }, &records), vec![1, 3, 4]);
        assert_eq!(select(SelectArgs { exclude: s(&["-2"]), ..Default::default() }, &records), vec![3, 4]);
        assert_eq!(select(SelectArgs { rule: s(&["unobtainable_item"]), ..Default::default() }, &records), vec![1, 3]);
        assert_eq!(
            select(SelectArgs { rule: s(&["unobtainable_item"]), id: s(&["*_spawn_egg"]), ..Default::default() }, &records),
            vec![3]
        );
        assert_eq!(select(SelectArgs { holder: s(&["chest"]), dim: s(&["the_nether"]), ..Default::default() }, &records), Vec::<u64>::new());
        assert!(SelectArgs { select: s(&["x"]), ..Default::default() }.compile().is_err());
    }

    #[test]
    fn round_trip_and_text() {
        let r = rec(7, "minecraft:bedrock", "unobtainable_item");
        let header = Header {
            mcscan: FORMAT_VERSION,
            world: "/srv/world".into(),
            preset: Some("illegal".into()),
            filters: vec![],
            scanned_at: 0,
            items_scanned: 10,
            matches: 1,
        };
        let mut buf = Vec::new();
        write(&mut buf, &header, std::slice::from_ref(&r)).unwrap();
        let text = String::from_utf8(buf).unwrap();
        let mut lines = text.lines();
        let h: Header = serde_json::from_str(lines.next().unwrap()).unwrap();
        assert_eq!(h.world, "/srv/world");
        let back: Record = serde_json::from_str(lines.next().unwrap()).unwrap();
        assert_eq!(back.path, r.path);
        assert_eq!(back.location(), "chest @ overworld 1 2 3 slot 5");
        assert_eq!(back.why(), "unobtainable_item: d");
        assert!(text.contains(r#""path":["block_entities",0,"Items",2]"#));
    }
}
