//! Minimal, order-preserving NBT (binary + SNBT) implementation.
//!
//! Round-trips every tag type exactly, so a chunk that is parsed, edited and
//! written back only differs where it was edited.

use anyhow::{anyhow, bail, Result};
use serde::{Deserialize, Serialize};

const MAX_DEPTH: usize = 512;

#[derive(Debug, Clone, PartialEq)]
pub enum Tag {
    Byte(i8),
    Short(i16),
    Int(i32),
    Long(i64),
    Float(f32),
    Double(f64),
    ByteArray(Vec<u8>),
    String(String),
    List(List),
    Compound(Compound),
    IntArray(Vec<i32>),
    LongArray(Vec<i64>),
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct List {
    /// Element type id as stored on disk (kept so empty lists round-trip).
    pub elem: u8,
    pub items: Vec<Tag>,
}

/// Insertion-ordered compound. Compounds are small, so linear lookup wins.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Compound(pub Vec<(String, Tag)>);

impl Compound {
    pub fn get(&self, key: &str) -> Option<&Tag> {
        self.0.iter().find(|(k, _)| k == key).map(|(_, v)| v)
    }
    pub fn get_mut(&mut self, key: &str) -> Option<&mut Tag> {
        self.0.iter_mut().find(|(k, _)| k == key).map(|(_, v)| v)
    }
    pub fn contains(&self, key: &str) -> bool {
        self.0.iter().any(|(k, _)| k == key)
    }
    pub fn insert(&mut self, key: &str, value: Tag) {
        match self.get_mut(key) {
            Some(slot) => *slot = value,
            None => self.0.push((key.to_string(), value)),
        }
    }
    pub fn remove(&mut self, key: &str) -> Option<Tag> {
        let i = self.0.iter().position(|(k, _)| k == key)?;
        Some(self.0.remove(i).1)
    }
    pub fn len(&self) -> usize {
        self.0.len()
    }
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
    pub fn iter(&self) -> impl Iterator<Item = (&str, &Tag)> {
        self.0.iter().map(|(k, v)| (k.as_str(), v))
    }
    pub fn str(&self, key: &str) -> Option<&str> {
        self.get(key).and_then(Tag::as_str)
    }
    pub fn int(&self, key: &str) -> Option<i64> {
        self.get(key).and_then(Tag::as_i64)
    }
    pub fn compound(&self, key: &str) -> Option<&Compound> {
        self.get(key).and_then(Tag::as_compound)
    }
    pub fn compound_mut(&mut self, key: &str) -> Option<&mut Compound> {
        self.get_mut(key).and_then(Tag::as_compound_mut)
    }
    pub fn list(&self, key: &str) -> Option<&List> {
        self.get(key).and_then(Tag::as_list)
    }
}

impl Tag {
    pub fn id(&self) -> u8 {
        match self {
            Tag::Byte(_) => 1,
            Tag::Short(_) => 2,
            Tag::Int(_) => 3,
            Tag::Long(_) => 4,
            Tag::Float(_) => 5,
            Tag::Double(_) => 6,
            Tag::ByteArray(_) => 7,
            Tag::String(_) => 8,
            Tag::List(_) => 9,
            Tag::Compound(_) => 10,
            Tag::IntArray(_) => 11,
            Tag::LongArray(_) => 12,
        }
    }
    /// Any integer tag as i64.
    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Tag::Byte(v) => Some(*v as i64),
            Tag::Short(v) => Some(*v as i64),
            Tag::Int(v) => Some(*v as i64),
            Tag::Long(v) => Some(*v),
            _ => None,
        }
    }
    /// Any numeric tag as f64.
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Tag::Float(v) => Some(*v as f64),
            Tag::Double(v) => Some(*v),
            other => other.as_i64().map(|v| v as f64),
        }
    }
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Tag::String(s) => Some(s),
            _ => None,
        }
    }
    pub fn as_compound(&self) -> Option<&Compound> {
        match self {
            Tag::Compound(c) => Some(c),
            _ => None,
        }
    }
    pub fn as_compound_mut(&mut self) -> Option<&mut Compound> {
        match self {
            Tag::Compound(c) => Some(c),
            _ => None,
        }
    }
    pub fn as_list(&self) -> Option<&List> {
        match self {
            Tag::List(l) => Some(l),
            _ => None,
        }
    }
    pub fn as_list_mut(&mut self) -> Option<&mut List> {
        match self {
            Tag::List(l) => Some(l),
            _ => None,
        }
    }
    /// Overwrite an integer tag, keeping its on-disk width (saturating).
    pub fn set_int(&mut self, v: i64) -> bool {
        match self {
            Tag::Byte(x) => *x = v.clamp(i8::MIN as i64, i8::MAX as i64) as i8,
            Tag::Short(x) => *x = v.clamp(i16::MIN as i64, i16::MAX as i64) as i16,
            Tag::Int(x) => *x = v.clamp(i32::MIN as i64, i32::MAX as i64) as i32,
            Tag::Long(x) => *x = v,
            _ => return false,
        }
        true
    }
    fn child(&self, seg: &Seg) -> Option<&Tag> {
        match (self, seg) {
            (Tag::Compound(c), Seg::Key(k)) => c.get(k),
            (Tag::List(l), Seg::Idx(i)) => l.items.get(*i),
            _ => None,
        }
    }
    fn child_mut(&mut self, seg: &Seg) -> Option<&mut Tag> {
        match (self, seg) {
            (Tag::Compound(c), Seg::Key(k)) => c.get_mut(k),
            (Tag::List(l), Seg::Idx(i)) => l.items.get_mut(*i),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// Paths
// ---------------------------------------------------------------------------

/// One step of a path into an NBT tree. Serialises as a JSON number or string.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Seg {
    Idx(usize),
    Key(String),
}

impl Seg {
    pub fn key(s: &str) -> Seg {
        Seg::Key(s.to_string())
    }
}

pub fn get_path<'a>(root: &'a Compound, path: &[Seg]) -> Option<&'a Tag> {
    let (first, rest) = path.split_first()?;
    let mut cur = match first {
        Seg::Key(k) => root.get(k)?,
        Seg::Idx(_) => return None,
    };
    for seg in rest {
        cur = cur.child(seg)?;
    }
    Some(cur)
}

pub fn get_path_mut<'a>(root: &'a mut Compound, path: &[Seg]) -> Option<&'a mut Tag> {
    let (first, rest) = path.split_first()?;
    let mut cur = match first {
        Seg::Key(k) => root.get_mut(k)?,
        Seg::Idx(_) => return None,
    };
    for seg in rest {
        cur = cur.child_mut(seg)?;
    }
    Some(cur)
}

/// Compound at `path`; the empty path is the root itself.
pub fn compound_at<'a>(root: &'a Compound, path: &[Seg]) -> Option<&'a Compound> {
    if path.is_empty() {
        Some(root)
    } else {
        get_path(root, path).and_then(Tag::as_compound)
    }
}

pub fn compound_at_mut<'a>(root: &'a mut Compound, path: &[Seg]) -> Option<&'a mut Compound> {
    if path.is_empty() {
        Some(root)
    } else {
        get_path_mut(root, path).and_then(Tag::as_compound_mut)
    }
}

/// Document order: list indices compare numerically and a path sorts before
/// anything nested beneath it.
pub fn cmp_paths(a: &[Seg], b: &[Seg]) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    for (x, y) in a.iter().zip(b) {
        let o = match (x, y) {
            (Seg::Idx(i), Seg::Idx(j)) => i.cmp(j),
            (Seg::Key(i), Seg::Key(j)) => i.cmp(j),
            (Seg::Idx(_), Seg::Key(_)) => Ordering::Less,
            (Seg::Key(_), Seg::Idx(_)) => Ordering::Greater,
        };
        if o != Ordering::Equal {
            return o;
        }
    }
    a.len().cmp(&b.len())
}

/// Parse `a.b[3]."quoted.key".c` into segments.
pub fn parse_path(s: &str) -> Result<Vec<Seg>> {
    let chars: Vec<char> = s.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            '.' => i += 1,
            '[' => {
                let end = chars[i..]
                    .iter()
                    .position(|c| *c == ']')
                    .ok_or_else(|| anyhow!("unclosed '[' in path {s:?}"))?
                    + i;
                let num: String = chars[i + 1..end].iter().collect();
                out.push(Seg::Idx(
                    num.trim().parse().map_err(|_| anyhow!("bad index {num:?} in path {s:?}"))?,
                ));
                i = end + 1;
            }
            q @ ('"' | '\'') => {
                let mut key = String::new();
                i += 1;
                loop {
                    match chars.get(i) {
                        None => bail!("unclosed quote in path {s:?}"),
                        Some('\\') if i + 1 < chars.len() => {
                            key.push(chars[i + 1]);
                            i += 2;
                        }
                        Some(c) if *c == q => {
                            i += 1;
                            break;
                        }
                        Some(c) => {
                            key.push(*c);
                            i += 1;
                        }
                    }
                }
                out.push(Seg::Key(key));
            }
            _ => {
                let start = i;
                while i < chars.len() && chars[i] != '.' && chars[i] != '[' {
                    i += 1;
                }
                out.push(Seg::Key(chars[start..i].iter().collect()));
            }
        }
    }
    if out.is_empty() {
        bail!("empty NBT path");
    }
    Ok(out)
}

pub fn path_to_string(path: &[Seg]) -> String {
    let mut s = String::new();
    for seg in path {
        match seg {
            Seg::Idx(i) => {
                s.push('[');
                s.push_str(&i.to_string());
                s.push(']');
            }
            Seg::Key(k) => {
                if !s.is_empty() {
                    s.push('.');
                }
                if k.contains('.') || k.contains('[') || k.is_empty() {
                    s.push('"');
                    s.push_str(&k.replace('\\', "\\\\").replace('"', "\\\""));
                    s.push('"');
                } else {
                    s.push_str(k);
                }
            }
        }
    }
    s
}

// ---------------------------------------------------------------------------
// Binary reader
// ---------------------------------------------------------------------------

struct Reader<'a> {
    b: &'a [u8],
    p: usize,
}

impl<'a> Reader<'a> {
    #[inline]
    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        if self.b.len() - self.p < n {
            bail!("unexpected end of NBT data");
        }
        let s = &self.b[self.p..self.p + n];
        self.p += n;
        Ok(s)
    }
    #[inline]
    fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }
    #[inline]
    fn u16(&mut self) -> Result<u16> {
        let s = self.take(2)?;
        Ok(u16::from_be_bytes([s[0], s[1]]))
    }
    #[inline]
    fn i32(&mut self) -> Result<i32> {
        let s = self.take(4)?;
        Ok(i32::from_be_bytes([s[0], s[1], s[2], s[3]]))
    }
    #[inline]
    fn i64(&mut self) -> Result<i64> {
        let s = self.take(8)?;
        let mut a = [0u8; 8];
        a.copy_from_slice(s);
        Ok(i64::from_be_bytes(a))
    }
    fn len(&mut self) -> Result<usize> {
        Ok(self.i32()?.max(0) as usize)
    }
    fn string(&mut self) -> Result<String> {
        let n = self.u16()? as usize;
        Ok(decode_mutf8(self.take(n)?))
    }
    fn skip_string(&mut self) -> Result<()> {
        let n = self.u16()? as usize;
        self.take(n).map(|_| ())
    }

    fn payload(&mut self, id: u8, depth: usize) -> Result<Tag> {
        if depth > MAX_DEPTH {
            bail!("NBT nested too deeply");
        }
        Ok(match id {
            1 => Tag::Byte(self.u8()? as i8),
            2 => Tag::Short(self.u16()? as i16),
            3 => Tag::Int(self.i32()?),
            4 => Tag::Long(self.i64()?),
            5 => Tag::Float(f32::from_bits(self.i32()? as u32)),
            6 => Tag::Double(f64::from_bits(self.i64()? as u64)),
            7 => {
                let n = self.len()?;
                Tag::ByteArray(self.take(n)?.to_vec())
            }
            8 => Tag::String(self.string()?),
            9 => {
                let elem = self.u8()?;
                let n = self.len()?;
                if elem == 0 && n > 0 {
                    bail!("non-empty NBT list of TAG_End");
                }
                // Every element needs at least one byte unless it is TAG_End.
                let mut items = Vec::with_capacity(n.min(self.b.len() - self.p));
                for _ in 0..n {
                    items.push(self.payload(elem, depth + 1)?);
                }
                Tag::List(List { elem, items })
            }
            10 => Tag::Compound(self.compound(depth + 1, &[], false)?),
            11 => {
                let n = self.len()?;
                let raw = self.take(n.checked_mul(4).ok_or_else(|| anyhow!("NBT array too long"))?)?;
                Tag::IntArray(
                    raw.chunks_exact(4).map(|c| i32::from_be_bytes([c[0], c[1], c[2], c[3]])).collect(),
                )
            }
            12 => {
                let n = self.len()?;
                let raw = self.take(n.checked_mul(8).ok_or_else(|| anyhow!("NBT array too long"))?)?;
                Tag::LongArray(
                    raw.chunks_exact(8)
                        .map(|c| {
                            let mut a = [0u8; 8];
                            a.copy_from_slice(c);
                            i64::from_be_bytes(a)
                        })
                        .collect(),
                )
            }
            other => bail!("unknown NBT tag id {other}"),
        })
    }

    fn skip_payload(&mut self, id: u8, depth: usize) -> Result<()> {
        if depth > MAX_DEPTH {
            bail!("NBT nested too deeply");
        }
        match id {
            1 => self.take(1).map(|_| ()),
            2 => self.take(2).map(|_| ()),
            3 | 5 => self.take(4).map(|_| ()),
            4 | 6 => self.take(8).map(|_| ()),
            7 => {
                let n = self.len()?;
                self.take(n).map(|_| ())
            }
            8 => self.skip_string(),
            9 => {
                let elem = self.u8()?;
                let n = self.len()?;
                let fixed = match elem {
                    1 => 1,
                    2 => 2,
                    3 | 5 => 4,
                    4 | 6 => 8,
                    _ => 0,
                };
                if fixed > 0 {
                    let bytes = n.checked_mul(fixed).ok_or_else(|| anyhow!("NBT list too long"))?;
                    return self.take(bytes).map(|_| ());
                }
                if elem == 0 && n > 0 {
                    bail!("non-empty NBT list of TAG_End");
                }
                for _ in 0..n {
                    self.skip_payload(elem, depth + 1)?;
                }
                Ok(())
            }
            10 => loop {
                let t = self.u8()?;
                if t == 0 {
                    return Ok(());
                }
                self.skip_string()?;
                self.skip_payload(t, depth + 1)?;
            },
            11 => {
                let n = self.len()?;
                self.take(n.checked_mul(4).ok_or_else(|| anyhow!("NBT array too long"))?).map(|_| ())
            }
            12 => {
                let n = self.len()?;
                self.take(n.checked_mul(8).ok_or_else(|| anyhow!("NBT array too long"))?).map(|_| ())
            }
            other => bail!("unknown NBT tag id {other}"),
        }
    }

    /// `skip` lists keys to drop without materialising them. It applies to
    /// this compound and (for pre-1.18 chunks) to a nested "Level" compound.
    fn compound(&mut self, depth: usize, skip: &[&str], skipping: bool) -> Result<Compound> {
        if depth > MAX_DEPTH {
            bail!("NBT nested too deeply");
        }
        let mut out = Vec::new();
        loop {
            let id = self.u8()?;
            if id == 0 {
                break;
            }
            let name = self.string()?;
            if skipping {
                if skip.contains(&name.as_str()) {
                    self.skip_payload(id, depth + 1)?;
                    continue;
                }
                if id == 10 && name == "Level" {
                    out.push((name, Tag::Compound(self.compound(depth + 1, skip, true)?)));
                    continue;
                }
            }
            let value = self.payload(id, depth)?;
            out.push((name, value));
        }
        Ok(Compound(out))
    }
}

/// Parse an uncompressed NBT file. Returns the root tag's name and contents.
pub fn parse(data: &[u8]) -> Result<(String, Compound)> {
    parse_skipping(data, &[])
}

/// Like [`parse`] but drops the named top-level keys without decoding them.
pub fn parse_skipping(data: &[u8], skip: &[&str]) -> Result<(String, Compound)> {
    let mut r = Reader { b: data, p: 0 };
    let id = r.u8()?;
    if id != 10 {
        bail!("NBT root is not a compound (tag id {id})");
    }
    let name = r.string()?;
    let root = r.compound(0, skip, !skip.is_empty())?;
    Ok((name, root))
}

fn decode_mutf8(b: &[u8]) -> String {
    if let Ok(s) = std::str::from_utf8(b) {
        return s.to_owned();
    }
    // Java "modified UTF-8": NUL is C0 80 and astral characters are encoded
    // as two 3-byte surrogates. Decode to UTF-16 units and let std pair them.
    let mut units: Vec<u16> = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        if c < 0x80 {
            units.push(c as u16);
            i += 1;
        } else if (0xC0..0xE0).contains(&c) && i + 1 < b.len() {
            units.push(((c as u16 & 0x1F) << 6) | (b[i + 1] as u16 & 0x3F));
            i += 2;
        } else if (0xE0..0xF0).contains(&c) && i + 2 < b.len() {
            units.push(((c as u16 & 0x0F) << 12) | ((b[i + 1] as u16 & 0x3F) << 6) | (b[i + 2] as u16 & 0x3F));
            i += 3;
        } else {
            units.push(0xFFFD);
            i += 1;
        }
    }
    String::from_utf16_lossy(&units)
}

// ---------------------------------------------------------------------------
// Binary writer
// ---------------------------------------------------------------------------

fn write_string(s: &str, out: &mut Vec<u8>) -> Result<()> {
    let plain = !s.bytes().any(|b| b == 0 || b >= 0xF0);
    if plain {
        if s.len() > u16::MAX as usize {
            bail!("NBT string longer than 65535 bytes");
        }
        out.extend_from_slice(&(s.len() as u16).to_be_bytes());
        out.extend_from_slice(s.as_bytes());
        return Ok(());
    }
    let mut enc = Vec::with_capacity(s.len() + 8);
    let mut buf = [0u16; 2];
    for ch in s.chars() {
        if ch == '\0' {
            enc.extend_from_slice(&[0xC0, 0x80]);
        } else if (ch as u32) < 0x10000 {
            let mut tmp = [0u8; 4];
            enc.extend_from_slice(ch.encode_utf8(&mut tmp).as_bytes());
        } else {
            for unit in ch.encode_utf16(&mut buf).iter() {
                let u = *unit;
                enc.push(0xE0 | (u >> 12) as u8);
                enc.push(0x80 | ((u >> 6) & 0x3F) as u8);
                enc.push(0x80 | (u & 0x3F) as u8);
            }
        }
    }
    if enc.len() > u16::MAX as usize {
        bail!("NBT string longer than 65535 bytes");
    }
    out.extend_from_slice(&(enc.len() as u16).to_be_bytes());
    out.extend_from_slice(&enc);
    Ok(())
}

fn write_payload(tag: &Tag, out: &mut Vec<u8>) -> Result<()> {
    match tag {
        Tag::Byte(v) => out.push(*v as u8),
        Tag::Short(v) => out.extend_from_slice(&v.to_be_bytes()),
        Tag::Int(v) => out.extend_from_slice(&v.to_be_bytes()),
        Tag::Long(v) => out.extend_from_slice(&v.to_be_bytes()),
        Tag::Float(v) => out.extend_from_slice(&v.to_bits().to_be_bytes()),
        Tag::Double(v) => out.extend_from_slice(&v.to_bits().to_be_bytes()),
        Tag::ByteArray(v) => {
            out.extend_from_slice(&(v.len() as i32).to_be_bytes());
            out.extend_from_slice(v);
        }
        Tag::String(s) => write_string(s, out)?,
        Tag::List(l) => {
            let elem = l.items.first().map(Tag::id).unwrap_or(l.elem);
            if l.items.iter().any(|t| t.id() != elem) {
                bail!("NBT list with mixed element types");
            }
            out.push(elem);
            out.extend_from_slice(&(l.items.len() as i32).to_be_bytes());
            for t in &l.items {
                write_payload(t, out)?;
            }
        }
        Tag::Compound(c) => write_compound(c, out)?,
        Tag::IntArray(v) => {
            out.extend_from_slice(&(v.len() as i32).to_be_bytes());
            for x in v {
                out.extend_from_slice(&x.to_be_bytes());
            }
        }
        Tag::LongArray(v) => {
            out.extend_from_slice(&(v.len() as i32).to_be_bytes());
            for x in v {
                out.extend_from_slice(&x.to_be_bytes());
            }
        }
    }
    Ok(())
}

fn write_compound(c: &Compound, out: &mut Vec<u8>) -> Result<()> {
    for (k, v) in &c.0 {
        out.push(v.id());
        write_string(k, out)?;
        write_payload(v, out)?;
    }
    out.push(0);
    Ok(())
}

/// Serialise a root compound to uncompressed NBT bytes.
pub fn write_root(name: &str, root: &Compound) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(4096);
    out.push(10);
    write_string(name, &mut out)?;
    write_compound(root, &mut out)?;
    Ok(out)
}

/// Stable 64-bit fingerprint (FNV-1a over the binary encoding) of a compound.
/// Used to make sure an item is still what the scan saw before editing it.
pub fn fingerprint(c: &Compound) -> u64 {
    let mut buf = Vec::with_capacity(256);
    // A compound that cannot be serialised cannot be on disk either.
    let _ = write_compound(c, &mut buf);
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in buf {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

// ---------------------------------------------------------------------------
// SNBT
// ---------------------------------------------------------------------------

fn snbt_key(k: &str, out: &mut String) {
    let bare = !k.is_empty() && k.chars().all(|c| c.is_ascii_alphanumeric() || "._+-".contains(c));
    if bare {
        out.push_str(k);
    } else {
        snbt_quote(k, out);
    }
}

fn snbt_quote(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            c => out.push(c),
        }
    }
    out.push('"');
}

/// Append the SNBT form of `tag` (the syntax `/give` and `/data` use).
pub fn write_snbt(tag: &Tag, out: &mut String) {
    use std::fmt::Write;
    match tag {
        Tag::Byte(v) => {
            let _ = write!(out, "{v}b");
        }
        Tag::Short(v) => {
            let _ = write!(out, "{v}s");
        }
        Tag::Int(v) => {
            let _ = write!(out, "{v}");
        }
        Tag::Long(v) => {
            let _ = write!(out, "{v}L");
        }
        Tag::Float(v) => {
            let _ = write!(out, "{v}f");
        }
        Tag::Double(v) => {
            let _ = write!(out, "{v}d");
        }
        Tag::String(s) => snbt_quote(s, out),
        Tag::ByteArray(v) => {
            out.push_str("[B;");
            for (i, x) in v.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                let _ = write!(out, "{}b", *x as i8);
            }
            out.push(']');
        }
        Tag::IntArray(v) => {
            out.push_str("[I;");
            for (i, x) in v.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                let _ = write!(out, "{x}");
            }
            out.push(']');
        }
        Tag::LongArray(v) => {
            out.push_str("[L;");
            for (i, x) in v.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                let _ = write!(out, "{x}L");
            }
            out.push(']');
        }
        Tag::List(l) => {
            out.push('[');
            for (i, t) in l.items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_snbt(t, out);
            }
            out.push(']');
        }
        Tag::Compound(c) => write_snbt_compound(c, out),
    }
}

pub fn write_snbt_compound(c: &Compound, out: &mut String) {
    out.push('{');
    for (i, (k, v)) in c.0.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        snbt_key(k, out);
        out.push(':');
        write_snbt(v, out);
    }
    out.push('}');
}

pub fn snbt(c: &Compound) -> String {
    let mut s = String::new();
    write_snbt_compound(c, &mut s);
    s
}

struct Snbt {
    c: Vec<char>,
    p: usize,
}

impl Snbt {
    fn ws(&mut self) {
        while self.p < self.c.len() && self.c[self.p].is_whitespace() {
            self.p += 1;
        }
    }
    fn peek(&self) -> Option<char> {
        self.c.get(self.p).copied()
    }
    fn expect(&mut self, ch: char) -> Result<()> {
        self.ws();
        if self.peek() == Some(ch) {
            self.p += 1;
            Ok(())
        } else {
            bail!("SNBT: expected '{ch}' at offset {}", self.p)
        }
    }
    fn quoted(&mut self) -> Result<String> {
        let q = self.c[self.p];
        self.p += 1;
        let mut s = String::new();
        loop {
            match self.peek() {
                None => bail!("SNBT: unterminated string"),
                Some('\\') => {
                    self.p += 1;
                    match self.peek() {
                        Some('n') => s.push('\n'),
                        Some(c) => s.push(c),
                        None => bail!("SNBT: unterminated string"),
                    }
                    self.p += 1;
                }
                Some(c) if c == q => {
                    self.p += 1;
                    return Ok(s);
                }
                Some(c) => {
                    s.push(c);
                    self.p += 1;
                }
            }
        }
    }
    /// Unquoted token. Values may contain ':' (resource ids); keys may not,
    /// because ':' separates a key from its value.
    fn bare(&mut self, extra: &str) -> String {
        let start = self.p;
        while let Some(c) = self.peek() {
            if c.is_ascii_alphanumeric() || "._+-".contains(c) || extra.contains(c) {
                self.p += 1;
            } else {
                break;
            }
        }
        self.c[start..self.p].iter().collect()
    }
    fn key(&mut self) -> Result<String> {
        self.ws();
        match self.peek() {
            Some('"') | Some('\'') => self.quoted(),
            _ => {
                let k = self.bare("");
                if k.is_empty() {
                    bail!("SNBT: expected a key at offset {}", self.p);
                }
                Ok(k)
            }
        }
    }
    fn value(&mut self, depth: usize) -> Result<Tag> {
        if depth > MAX_DEPTH {
            bail!("SNBT nested too deeply");
        }
        self.ws();
        match self.peek() {
            None => bail!("SNBT: unexpected end of input"),
            Some('{') => {
                self.p += 1;
                let mut c = Compound::default();
                loop {
                    self.ws();
                    if self.peek() == Some('}') {
                        self.p += 1;
                        break;
                    }
                    let k = self.key()?;
                    self.expect(':')?;
                    let v = self.value(depth + 1)?;
                    c.insert(&k, v);
                    self.ws();
                    match self.peek() {
                        Some(',') => self.p += 1,
                        Some('}') => {}
                        _ => bail!("SNBT: expected ',' or '}}' at offset {}", self.p),
                    }
                }
                Ok(Tag::Compound(c))
            }
            Some('[') => {
                self.p += 1;
                let typed = match (self.peek(), self.c.get(self.p + 1).copied()) {
                    (Some(t @ ('B' | 'I' | 'L')), Some(';')) => {
                        self.p += 2;
                        Some(t)
                    }
                    _ => None,
                };
                let mut items = Vec::new();
                loop {
                    self.ws();
                    if self.peek() == Some(']') {
                        self.p += 1;
                        break;
                    }
                    items.push(self.value(depth + 1)?);
                    self.ws();
                    match self.peek() {
                        Some(',') => self.p += 1,
                        Some(']') => {}
                        _ => bail!("SNBT: expected ',' or ']' at offset {}", self.p),
                    }
                }
                match typed {
                    Some(t) => {
                        let mut ints = Vec::with_capacity(items.len());
                        for it in &items {
                            ints.push(it.as_i64().ok_or_else(|| anyhow!("SNBT: non-integer in typed array"))?);
                        }
                        Ok(match t {
                            'B' => Tag::ByteArray(ints.iter().map(|v| *v as u8).collect()),
                            'I' => Tag::IntArray(ints.iter().map(|v| *v as i32).collect()),
                            _ => Tag::LongArray(ints),
                        })
                    }
                    None => {
                        let elem = items.first().map(Tag::id).unwrap_or(0);
                        if items.iter().any(|t| t.id() != elem) {
                            bail!("SNBT: list elements must all have the same type");
                        }
                        Ok(Tag::List(List { elem, items }))
                    }
                }
            }
            Some('"') | Some('\'') => Ok(Tag::String(self.quoted()?)),
            Some(_) => {
                let tok = self.bare(":/#");
                if tok.is_empty() {
                    bail!("SNBT: unexpected character at offset {}", self.p);
                }
                Ok(bare_value(&tok))
            }
        }
    }
}

fn bare_value(tok: &str) -> Tag {
    match tok {
        "true" => return Tag::Byte(1),
        "false" => return Tag::Byte(0),
        _ => {}
    }
    let numeric_start = tok.starts_with(|c: char| c.is_ascii_digit() || c == '-' || c == '+' || c == '.');
    if numeric_start {
        let (body, suffix) = tok.split_at(tok.len() - 1);
        match suffix {
            "b" | "B" => {
                if let Ok(v) = body.parse::<i8>() {
                    return Tag::Byte(v);
                }
            }
            "s" | "S" => {
                if let Ok(v) = body.parse::<i16>() {
                    return Tag::Short(v);
                }
            }
            "l" | "L" => {
                if let Ok(v) = body.parse::<i64>() {
                    return Tag::Long(v);
                }
            }
            "f" | "F" => {
                if let Ok(v) = body.parse::<f32>() {
                    return Tag::Float(v);
                }
            }
            "d" | "D" => {
                if let Ok(v) = body.parse::<f64>() {
                    return Tag::Double(v);
                }
            }
            _ => {}
        }
        if let Ok(v) = tok.parse::<i32>() {
            return Tag::Int(v);
        }
        if tok.contains('.') || tok.contains('e') || tok.contains('E') {
            if let Ok(v) = tok.parse::<f64>() {
                return Tag::Double(v);
            }
        }
    }
    Tag::String(tok.to_string())
}

/// Parse an SNBT value such as `5`, `1b`, `"text"`, `{a:1}` or `[I;1,2]`.
pub fn parse_snbt(s: &str) -> Result<Tag> {
    let mut p = Snbt { c: s.chars().collect(), p: 0 };
    let v = p.value(0)?;
    p.ws();
    if p.p != p.c.len() {
        bail!("SNBT: trailing characters at offset {}", p.p);
    }
    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Compound {
        let mut item = Compound::default();
        item.insert("id", Tag::String("minecraft:diamond_sword".into()));
        item.insert("count", Tag::Int(1));
        let mut root = Compound::default();
        root.insert("DataVersion", Tag::Int(5023));
        root.insert("name", Tag::String("a\u{0}b\u{1F600}c".into()));
        root.insert("longs", Tag::LongArray(vec![1, -2, i64::MAX]));
        root.insert("ints", Tag::IntArray(vec![7, -7]));
        root.insert("bytes", Tag::ByteArray(vec![0, 255]));
        root.insert("f", Tag::Float(1.5));
        root.insert("d", Tag::Double(-2.25));
        root.insert("empty", Tag::List(List { elem: 0, items: vec![] }));
        root.insert("Items", Tag::List(List { elem: 10, items: vec![Tag::Compound(item)] }));
        root
    }

    #[test]
    fn binary_round_trip() {
        let root = sample();
        let bytes = write_root("", &root).unwrap();
        let (name, back) = parse(&bytes).unwrap();
        assert_eq!(name, "");
        assert_eq!(back, root);
        assert_eq!(write_root("", &back).unwrap(), bytes);
    }

    #[test]
    fn skipping_keeps_paths_valid() {
        let root = sample();
        let bytes = write_root("", &root).unwrap();
        let (_, lean) = parse_skipping(&bytes, &["longs", "ints", "bytes"]).unwrap();
        assert!(!lean.contains("longs"));
        let path = vec![Seg::key("Items"), Seg::Idx(0), Seg::key("id")];
        assert_eq!(get_path(&lean, &path), get_path(&root, &path));
    }

    #[test]
    fn snbt_round_trip() {
        let root = sample();
        let text = snbt(&root);
        let back = parse_snbt(&text).unwrap();
        assert_eq!(back, Tag::Compound(root));
        assert_eq!(parse_snbt("32767s").unwrap(), Tag::Short(32767));
        assert_eq!(parse_snbt("minecraft:stone").unwrap(), Tag::String("minecraft:stone".into()));
        assert_eq!(parse_snbt("true").unwrap(), Tag::Byte(1));
    }

    #[test]
    fn paths() {
        let p = parse_path("components.minecraft:container[2].item.\"a.b\"").unwrap();
        assert_eq!(
            p,
            vec![
                Seg::key("components"),
                Seg::key("minecraft:container"),
                Seg::Idx(2),
                Seg::key("item"),
                Seg::key("a.b")
            ]
        );
        assert_eq!(parse_path(&path_to_string(&p)).unwrap(), p);
    }

    #[test]
    fn truncated_input_is_an_error() {
        let bytes = write_root("", &sample()).unwrap();
        assert!(parse(&bytes[..bytes.len() - 3]).is_err());
    }
}
