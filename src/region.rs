//! Anvil region files (`r.X.Z.mca`), chunk (de)compression and `.dat` files.

use anyhow::{anyhow, bail, Context, Result};
use flate2::{Compression, Decompress, FlushDecompress, Status};
use std::borrow::Cow;
use std::collections::HashMap;
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

pub const SECTOR: usize = 4096;
pub const CHUNKS_PER_REGION: usize = 1024;

/// Reusable decompressor: one per worker thread, so inflating a chunk does
/// not allocate zlib state every time.
pub struct Inflater {
    z: Decompress,
}

impl Default for Inflater {
    fn default() -> Self {
        Inflater { z: Decompress::new(true) }
    }
}

impl Inflater {
    /// Decompress a chunk payload using the region compression scheme id.
    pub fn decompress(&mut self, scheme: u8, data: &[u8], out: &mut Vec<u8>) -> Result<()> {
        out.clear();
        match scheme {
            1 => {
                flate2::read::GzDecoder::new(data).read_to_end(out).context("gzip")?;
            }
            2 => self.zlib(data, out)?,
            3 => out.extend_from_slice(data),
            4 => lz4_block_stream(data, out)?,
            other => bail!("unsupported chunk compression scheme {other}"),
        }
        Ok(())
    }

    fn zlib(&mut self, data: &[u8], out: &mut Vec<u8>) -> Result<()> {
        self.z.reset(true);
        if out.capacity() < 1 << 16 {
            out.reserve(1 << 16);
        }
        loop {
            let consumed = self.z.total_in() as usize;
            let produced = self.z.total_out();
            let status = self
                .z
                .decompress_vec(&data[consumed.min(data.len())..], out, FlushDecompress::None)
                .map_err(|e| anyhow!("zlib: {e}"))?;
            if status == Status::StreamEnd {
                return Ok(());
            }
            if out.len() == out.capacity() {
                out.reserve(out.capacity().max(1 << 16));
                continue;
            }
            if self.z.total_in() as usize == consumed && self.z.total_out() == produced {
                bail!("truncated zlib stream");
            }
        }
    }
}

/// Minecraft's LZ4 chunks use lz4-java's `LZ4BlockOutputStream` framing:
/// repeated blocks of `"LZ4Block" token clen olen checksum data`.
fn lz4_block_stream(mut data: &[u8], out: &mut Vec<u8>) -> Result<()> {
    const MAGIC: &[u8; 8] = b"LZ4Block";
    const HEADER: usize = 21;
    while !data.is_empty() {
        if data.len() < HEADER || &data[..8] != MAGIC {
            bail!("bad LZ4 block header");
        }
        let token = data[8];
        let clen = u32::from_le_bytes([data[9], data[10], data[11], data[12]]) as usize;
        let olen = u32::from_le_bytes([data[13], data[14], data[15], data[16]]) as usize;
        data = &data[HEADER..];
        if olen == 0 {
            break; // end-of-stream marker
        }
        if clen > data.len() || olen > (1 << 28) {
            bail!("truncated LZ4 block");
        }
        let (block, rest) = data.split_at(clen);
        match token & 0xF0 {
            0x10 => out.extend_from_slice(block),
            0x20 => {
                let start = out.len();
                out.resize(start + olen, 0);
                let n = lz4_flex::block::decompress_into(block, &mut out[start..])
                    .map_err(|e| anyhow!("lz4: {e}"))?;
                if n != olen {
                    bail!("LZ4 block length mismatch");
                }
            }
            other => bail!("unknown LZ4 block method {other:#x}"),
        }
        data = rest;
    }
    Ok(())
}

pub fn compress_zlib(data: &[u8]) -> Result<Vec<u8>> {
    let mut enc = flate2::write::ZlibEncoder::new(Vec::with_capacity(data.len() / 4 + 64), Compression::default());
    enc.write_all(data)?;
    Ok(enc.finish()?)
}

/// `r.X.Z.mca` -> (X, Z)
pub fn region_coords(path: &Path) -> Option<(i32, i32)> {
    let name = path.file_name()?.to_str()?;
    let mut parts = name.split('.');
    if parts.next()? != "r" {
        return None;
    }
    let x = parts.next()?.parse().ok()?;
    let z = parts.next()?.parse().ok()?;
    Some((x, z))
}

/// Index of a chunk inside its region file's header.
pub fn chunk_index(cx: i32, cz: i32) -> usize {
    (cx.rem_euclid(32) + cz.rem_euclid(32) * 32) as usize
}

pub enum RawChunk<'a> {
    Inline { scheme: u8, data: &'a [u8] },
    /// Oversized chunk stored next to the region file as `c.X.Z.mcc`.
    External { scheme: u8 },
}

pub struct Region {
    pub buf: Vec<u8>,
    pub dir: PathBuf,
    pub rx: i32,
    pub rz: i32,
}

impl Region {
    pub fn open(path: &Path) -> Result<Region> {
        let (rx, rz) = region_coords(path).ok_or_else(|| anyhow!("not a region file name: {}", path.display()))?;
        let buf = fs::read(path).with_context(|| format!("reading {}", path.display()))?;
        let dir = path.parent().map(Path::to_path_buf).unwrap_or_default();
        Ok(Region { buf, dir, rx, rz })
    }

    /// Absolute chunk coordinates of header slot `idx`.
    pub fn chunk_pos(&self, idx: usize) -> (i32, i32) {
        (self.rx * 32 + (idx % 32) as i32, self.rz * 32 + (idx / 32) as i32)
    }

    fn mcc_path(&self, idx: usize) -> PathBuf {
        let (cx, cz) = self.chunk_pos(idx);
        self.dir.join(format!("c.{cx}.{cz}.mcc"))
    }

    /// The on-disk record (`length`, `scheme`, payload) for a header slot.
    /// `Ok(None)` means the chunk does not exist.
    fn record(&self, idx: usize) -> Result<Option<&[u8]>> {
        if self.buf.len() < 2 * SECTOR {
            return Ok(None);
        }
        let loc = u32::from_be_bytes([self.buf[idx * 4], self.buf[idx * 4 + 1], self.buf[idx * 4 + 2], self.buf[idx * 4 + 3]]);
        if loc == 0 {
            return Ok(None);
        }
        let start = (loc >> 8) as usize * SECTOR;
        if start < 2 * SECTOR || start + 5 > self.buf.len() {
            bail!("chunk slot {idx} points outside the file");
        }
        let length = u32::from_be_bytes([self.buf[start], self.buf[start + 1], self.buf[start + 2], self.buf[start + 3]]) as usize;
        if length == 0 {
            bail!("chunk slot {idx} has zero length");
        }
        let end = start + 4 + length;
        if end > self.buf.len() {
            bail!("chunk slot {idx} is truncated");
        }
        Ok(Some(&self.buf[start..end]))
    }

    pub fn raw(&self, idx: usize) -> Result<Option<RawChunk<'_>>> {
        let rec = match self.record(idx)? {
            Some(r) => r,
            None => return Ok(None),
        };
        let scheme = rec[4];
        Ok(Some(if scheme & 0x80 != 0 {
            RawChunk::External { scheme: scheme & 0x7F }
        } else {
            RawChunk::Inline { scheme, data: &rec[5..] }
        }))
    }

    /// Decompress chunk `idx` into `out`. Returns false if the chunk is absent.
    pub fn read(&self, idx: usize, inflater: &mut Inflater, out: &mut Vec<u8>) -> Result<bool> {
        match self.raw(idx)? {
            None => Ok(false),
            Some(RawChunk::Inline { scheme, data }) => {
                inflater.decompress(scheme, data, out)?;
                Ok(true)
            }
            Some(RawChunk::External { scheme }) => {
                let path = self.mcc_path(idx);
                let data = fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
                inflater.decompress(scheme, &data, out)?;
                Ok(true)
            }
        }
    }
}

/// Rewrite a region file, replacing the given chunks (header slot ->
/// uncompressed NBT). Untouched chunks are copied byte-for-byte. The new file
/// is written next to the old one and renamed over it. Returns how many
/// already-unreadable chunks had to be dropped.
pub fn rewrite(path: &Path, replace: &HashMap<usize, Vec<u8>>) -> Result<usize> {
    let region = Region::open(path)?;
    if region.buf.len() < 2 * SECTOR {
        bail!("{} has no region header", path.display());
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as u32)
        .unwrap_or(0);

    let mut out = Vec::with_capacity(region.buf.len() + 2 * SECTOR);
    out.resize(2 * SECTOR, 0);
    out[SECTOR..2 * SECTOR].copy_from_slice(&region.buf[SECTOR..2 * SECTOR]);

    let mut dropped = 0;
    let mut stale_mcc = Vec::new();
    for idx in 0..CHUNKS_PER_REGION {
        let rec: Cow<[u8]> = match replace.get(&idx) {
            Some(nbt) => {
                let z = compress_zlib(nbt)?;
                let was_external = matches!(region.raw(idx), Ok(Some(RawChunk::External { .. })));
                if z.len() + 5 > 255 * SECTOR {
                    write_atomic(&region.mcc_path(idx), &z)?;
                    Cow::Owned(vec![0, 0, 0, 1, 0x80 | 2])
                } else {
                    if was_external {
                        stale_mcc.push(region.mcc_path(idx));
                    }
                    let mut r = Vec::with_capacity(z.len() + 5);
                    r.extend_from_slice(&((z.len() + 1) as u32).to_be_bytes());
                    r.push(2);
                    r.extend_from_slice(&z);
                    Cow::Owned(r)
                }
            }
            None => match region.record(idx) {
                Ok(Some(r)) => Cow::Borrowed(r),
                Ok(None) => continue,
                Err(_) => {
                    dropped += 1;
                    continue;
                }
            },
        };
        let sector = out.len() / SECTOR;
        let sectors = (rec.len() + SECTOR - 1) / SECTOR;
        if sector > 0x00FF_FFFF || sectors > 255 {
            bail!("region file too large to rewrite");
        }
        out.extend_from_slice(&rec);
        out.resize((sector + sectors) * SECTOR, 0);
        let loc = ((sector as u32) << 8) | sectors as u32;
        out[idx * 4..idx * 4 + 4].copy_from_slice(&loc.to_be_bytes());
        if replace.contains_key(&idx) {
            out[SECTOR + idx * 4..SECTOR + idx * 4 + 4].copy_from_slice(&now.to_be_bytes());
        }
    }
    write_atomic(path, &out)?;
    for p in stale_mcc {
        let _ = fs::remove_file(p);
    }
    Ok(dropped)
}

/// Write to a temporary sibling and rename into place, keeping the original
/// file's mode and owner (so running as root does not lock the server out).
pub fn write_atomic(path: &Path, data: &[u8]) -> Result<()> {
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".mcscan-tmp");
    let tmp = PathBuf::from(tmp);
    {
        let mut f = fs::File::create(&tmp).with_context(|| format!("creating {}", tmp.display()))?;
        f.write_all(data)?;
        f.sync_all()?;
    }
    if let Ok(meta) = fs::metadata(path) {
        let _ = fs::set_permissions(&tmp, meta.permissions());
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let _ = std::os::unix::fs::chown(&tmp, Some(meta.uid()), Some(meta.gid()));
        }
    }
    fs::rename(&tmp, path).with_context(|| format!("replacing {}", path.display()))?;
    Ok(())
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DatCompression {
    Gzip,
    Zlib,
    None,
}

/// Read a standalone NBT file (`level.dat`, player data, ...) and return its
/// uncompressed bytes plus how it was compressed.
pub fn read_dat(path: &Path) -> Result<(Vec<u8>, DatCompression)> {
    let raw = fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let mut out = Vec::with_capacity(raw.len() * 4);
    if raw.len() >= 2 && raw[0] == 0x1F && raw[1] == 0x8B {
        flate2::read::GzDecoder::new(&raw[..]).read_to_end(&mut out).context("gzip")?;
        Ok((out, DatCompression::Gzip))
    } else if raw.len() >= 2 && raw[0] == 0x78 && (((raw[0] as u16) << 8) | raw[1] as u16) % 31 == 0 {
        flate2::read::ZlibDecoder::new(&raw[..]).read_to_end(&mut out).context("zlib")?;
        Ok((out, DatCompression::Zlib))
    } else {
        Ok((raw, DatCompression::None))
    }
}

pub fn write_dat(path: &Path, nbt: &[u8], how: DatCompression) -> Result<()> {
    let data = match how {
        DatCompression::Gzip => {
            let mut enc = flate2::write::GzEncoder::new(Vec::new(), Compression::default());
            enc.write_all(nbt)?;
            enc.finish()?
        }
        DatCompression::Zlib => compress_zlib(nbt)?,
        DatCompression::None => nbt.to_vec(),
    };
    write_atomic(path, &data)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coords() {
        assert_eq!(region_coords(Path::new("/w/region/r.-1.2.mca")), Some((-1, 2)));
        assert_eq!(region_coords(Path::new("level.dat")), None);
        assert_eq!(chunk_index(-1, -1), 31 + 31 * 32);
        assert_eq!(chunk_index(33, 64), 1);
    }

    #[test]
    fn zlib_round_trip_reuses_inflater() {
        let mut inf = Inflater::default();
        let mut out = Vec::new();
        for n in [0usize, 10, 200_000] {
            let data: Vec<u8> = (0..n).map(|i| (i * 7 % 251) as u8).collect();
            let z = compress_zlib(&data).unwrap();
            inf.decompress(2, &z, &mut out).unwrap();
            assert_eq!(out, data);
        }
        assert!(inf.decompress(2, &[0x78, 0x9c, 1, 2], &mut out).is_err());
    }

    #[test]
    fn rewrite_replaces_and_preserves() {
        let dir = std::env::temp_dir().join(format!("mcscan-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("r.0.0.mca");
        fs::write(&path, vec![0u8; 2 * SECTOR]).unwrap();

        let mut first = HashMap::new();
        first.insert(0usize, b"chunk zero".to_vec());
        first.insert(5usize, vec![9u8; 10_000]);
        assert_eq!(rewrite(&path, &first).unwrap(), 0);

        let mut second = HashMap::new();
        second.insert(0usize, b"changed".to_vec());
        rewrite(&path, &second).unwrap();

        let region = Region::open(&path).unwrap();
        let mut inf = Inflater::default();
        let mut out = Vec::new();
        assert!(region.read(0, &mut inf, &mut out).unwrap());
        assert_eq!(out, b"changed");
        assert!(region.read(5, &mut inf, &mut out).unwrap());
        assert_eq!(out, vec![9u8; 10_000]);
        assert!(!region.read(6, &mut inf, &mut out).unwrap());
        assert_eq!(region.buf.len() % SECTOR, 0);
        fs::remove_dir_all(&dir).unwrap();
    }
}
