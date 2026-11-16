//! World folder discovery. Understands both the classic layout
//! (`region/`, `DIM-1/`, `playerdata/`) and the 26.1+ layout
//! (`dimensions/<namespace>/<name>/`, `players/data/`).

use anyhow::{bail, Result};
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Category {
    /// Block entities: chests, shulkers, hoppers, ...
    Chunks,
    /// Mobs, item frames, dropped items, minecarts, ...
    Entities,
    /// Player inventories and ender chests.
    Players,
    /// `level.dat` (holds the single-player inventory).
    Level,
    /// Anything else under a `data/` folder (command storage, ...).
    Data,
}

impl Category {
    pub const ALL: [Category; 5] =
        [Category::Chunks, Category::Entities, Category::Players, Category::Level, Category::Data];

    pub fn name(self) -> &'static str {
        match self {
            Category::Chunks => "chunks",
            Category::Entities => "entities",
            Category::Players => "players",
            Category::Level => "level",
            Category::Data => "data",
        }
    }

    pub fn parse(s: &str) -> Option<Category> {
        Category::ALL.into_iter().find(|c| c.name() == s)
    }
}

#[derive(Clone, Debug)]
pub struct WorldFile {
    pub path: PathBuf,
    /// Path relative to the world folder, always with forward slashes.
    pub rel: String,
    pub category: Category,
    pub dim: Option<String>,
    pub size: u64,
}

impl WorldFile {
    pub fn is_region(&self) -> bool {
        matches!(self.category, Category::Chunks | Category::Entities)
    }

    /// Player UUID for player data files.
    pub fn player(&self) -> Option<&str> {
        if self.category != Category::Players {
            return None;
        }
        self.rel.rsplit('/').next().and_then(|f| f.strip_suffix(".dat"))
    }
}

const SKIP_DIRS: [&str; 8] =
    ["poi", "datapacks", "advancements", "stats", "generated", "mcscan-backups", "logs", "crash-reports"];

fn dimension_of(parts: &[&str]) -> String {
    if let Some(i) = parts.iter().position(|p| *p == "dimensions") {
        if parts.len() > i + 2 {
            return format!("{}:{}", parts[i + 1], parts[i + 2]);
        }
    }
    if parts.contains(&"DIM-1") {
        "minecraft:the_nether".to_string()
    } else if parts.contains(&"DIM1") {
        "minecraft:the_end".to_string()
    } else {
        "minecraft:overworld".to_string()
    }
}

/// Classify a world-relative path. `None` means "not something we scan".
pub fn classify(rel: &str) -> Option<(Category, Option<String>)> {
    let parts: Vec<&str> = rel.split('/').collect();
    let (file, dirs) = parts.split_last()?;
    let parent = dirs.last().copied().unwrap_or("");
    if file.ends_with(".mca") {
        let cat = match parent {
            "region" => Category::Chunks,
            "entities" => Category::Entities,
            _ => return None,
        };
        return Some((cat, Some(dimension_of(dirs))));
    }
    if !file.ends_with(".dat") {
        return None;
    }
    if dirs.is_empty() {
        return (*file == "level.dat").then_some((Category::Level, None));
    }
    let grandparent = if dirs.len() >= 2 { dirs[dirs.len() - 2] } else { "" };
    if parent == "playerdata" || (parent == "data" && grandparent == "players") {
        return Some((Category::Players, None));
    }
    if dirs.contains(&"data") {
        return Some((Category::Data, Some(dimension_of(dirs))));
    }
    None
}

fn walk(dir: &Path, rel: &str, out: &mut Vec<WorldFile>) {
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = match name.to_str() {
            Some(n) => n,
            None => continue,
        };
        let child_rel = if rel.is_empty() { name.to_string() } else { format!("{rel}/{name}") };
        // Follow symlinks: server worlds are often linked onto other disks.
        let meta = match fs::metadata(entry.path()) {
            Ok(m) => m,
            Err(_) => continue,
        };
        if meta.is_dir() {
            if name.starts_with('.') || SKIP_DIRS.contains(&name) {
                continue;
            }
            walk(&entry.path(), &child_rel, out);
        } else if meta.is_file() {
            if let Some((category, dim)) = classify(&child_rel) {
                out.push(WorldFile { path: entry.path(), rel: child_rel, category, dim, size: meta.len() });
            }
        }
    }
}

/// Find every file in a world folder that can hold items.
pub fn discover(world: &Path) -> Result<Vec<WorldFile>> {
    if !world.is_dir() {
        bail!("{} is not a directory", world.display());
    }
    let mut out = Vec::new();
    walk(world, "", &mut out);
    if out.is_empty() {
        bail!(
            "{} does not look like a Minecraft world (no level.dat, region or player files found)",
            world.display()
        );
    }
    Ok(out)
}

/// PID of the process that has this world open (the game holds an fcntl lock
/// on `session.lock` while a world is loaded), if any.
#[cfg(unix)]
pub fn lock_holder(world: &Path) -> Option<i64> {
    use std::os::unix::io::AsRawFd;
    let f = fs::File::open(world.join("session.lock")).ok()?;
    // SAFETY: `flock` is a plain C struct; zero is a valid bit pattern and
    // F_GETLK only writes into the struct we pass.
    unsafe {
        let mut fl: libc::flock = std::mem::zeroed();
        fl.l_type = libc::F_WRLCK as libc::c_short;
        fl.l_whence = libc::SEEK_SET as libc::c_short;
        let ptr: *mut libc::flock = &mut fl;
        if libc::fcntl(f.as_raw_fd(), libc::F_GETLK, ptr) == 0 && fl.l_type != libc::F_UNLCK as libc::c_short {
            return Some(fl.l_pid as i64);
        }
    }
    None
}

#[cfg(not(unix))]
pub fn lock_holder(_world: &Path) -> Option<i64> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(rel: &str) -> Option<(Category, Option<String>)> {
        classify(rel)
    }

    #[test]
    fn classic_layout() {
        assert_eq!(c("region/r.0.0.mca"), Some((Category::Chunks, Some("minecraft:overworld".into()))));
        assert_eq!(c("DIM-1/entities/r.0.0.mca"), Some((Category::Entities, Some("minecraft:the_nether".into()))));
        assert_eq!(c("DIM1/region/r.0.0.mca"), Some((Category::Chunks, Some("minecraft:the_end".into()))));
        assert_eq!(c("playerdata/abc.dat"), Some((Category::Players, None)));
        assert_eq!(c("playerdata/abc.dat_old"), None);
        assert_eq!(c("level.dat"), Some((Category::Level, None)));
        assert_eq!(c("poi/r.0.0.mca"), None);
    }

    #[test]
    fn layout_26_1() {
        assert_eq!(
            c("dimensions/minecraft/the_nether/region/r.0.0.mca"),
            Some((Category::Chunks, Some("minecraft:the_nether".into())))
        );
        assert_eq!(
            c("dimensions/mypack/mine/entities/r.1.-1.mca"),
            Some((Category::Entities, Some("mypack:mine".into())))
        );
        assert_eq!(c("players/data/abc.dat"), Some((Category::Players, None)));
        assert_eq!(c("players/stats/abc.json"), None);
        assert_eq!(c("data/minecraft/command_storage.dat").map(|x| x.0), Some(Category::Data));
    }
}
