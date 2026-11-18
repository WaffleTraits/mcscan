//! Talking to a running server through the optional `mcscan-live` Fabric mod
//! (see `fabric-mod/`).
//!
//! With `--live`, `scan` asks the server to save first so the files are
//! current, and `apply` edits the items inside the running game instead of
//! in the files: for every block entity, entity or player it fetches the
//! live NBT, runs the usual checks and operations on it, and sends back a
//! compare-and-swap. The mod applies that on the server thread only if the
//! parts being replaced still are what was fetched, so an item that a hopper
//! or a player moved in between is never lost or duplicated.
//!
//! The wire format is a 4-byte big-endian length followed by one uncompressed
//! NBT compound per message.

use crate::apply::{self, ApplyOptions, Outcome, Progress, Report, Status};
use crate::nbt::{self, Compound, List, Seg, Tag};
use crate::region::{self, Inflater, Region};
use crate::results::Record;
use crate::world::{self, Category};
use anyhow::{anyhow, bail, Context, Result};
use std::collections::{BTreeMap, HashMap};
use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::time::Duration;

pub const PROTOCOL: i32 = 1;
const CONFIG_NAME: &str = "mcscan-live.json";
const MAX_FRAME: usize = 256 << 20;
/// How often one holder is re-fetched when it changed under us.
const ATTEMPTS: usize = 4;

/// `--live` and friends.
#[derive(Debug, Clone, Default)]
pub struct Settings {
    /// The mod's config file; found from the world folder when not given.
    pub config: Option<PathBuf>,
}

#[derive(Debug)]
struct Endpoint {
    addr: String,
    token: String,
}

fn read_config(path: &Path) -> Result<Endpoint> {
    let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let json: serde_json::Value =
        serde_json::from_str(&text).with_context(|| format!("{} is not valid JSON", path.display()))?;
    let token = json.get("token").and_then(|t| t.as_str()).unwrap_or_default().to_string();
    if token.is_empty() {
        bail!("{} has no token; start the server once with the mod installed", path.display());
    }
    if json.get("enabled").and_then(|e| e.as_bool()) == Some(false) {
        bail!("the mod is switched off in {} (\"enabled\": false)", path.display());
    }
    let port = json.get("port").and_then(|p| p.as_u64()).unwrap_or(25599);
    let bind = json.get("bind").and_then(|b| b.as_str()).unwrap_or("127.0.0.1");
    // "host" is for a config copied to another machine; a wildcard bind means "this machine".
    let host = match json.get("host").and_then(|h| h.as_str()) {
        Some(h) if !h.is_empty() => h,
        _ if matches!(bind, "0.0.0.0" | "::" | "") => "127.0.0.1",
        _ => bind,
    };
    let addr = if host.contains(':') { format!("[{host}]:{port}") } else { format!("{host}:{port}") };
    Ok(Endpoint { addr, token })
}

/// The server's `config/` folder sits next to the world folder on a dedicated
/// server and two levels up (next to `saves/`) in a single-player game.
fn find_config(world: &Path, settings: &Settings) -> Result<PathBuf> {
    if let Some(p) = &settings.config {
        if !p.is_file() {
            bail!("{} does not exist", p.display());
        }
        return Ok(p.clone());
    }
    let world = world.canonicalize().unwrap_or_else(|_| world.to_path_buf());
    let mut tried = Vec::new();
    for up in world.ancestors().skip(1).take(2) {
        let candidate = up.join("config").join(CONFIG_NAME);
        if candidate.is_file() {
            return Ok(candidate);
        }
        tried.push(candidate.display().to_string());
    }
    bail!(
        "--live needs the mcscan-live mod on the server, but its config file was not found (looked for {}). \
         Install the mod (see fabric-mod/ in the mcscan repository), start the server once, or point \
         --live-config at the file",
        tried.join(" and ")
    )
}

/// What the server said about itself when we connected.
#[derive(Debug, Clone, Default)]
pub struct Info {
    pub mod_version: String,
    pub minecraft: String,
    pub data_version: i64,
    pub world: String,
    pub journal: String,
}

/// What an item lives in, as the mod addresses it.
#[derive(Debug, Clone, PartialEq)]
pub enum Holder {
    Block { dim: String, pos: [i32; 3] },
    Entity { dim: String, uuid: String, chunk: Option<[i32; 2]> },
    Player { uuid: String },
}

impl Holder {
    fn to_nbt(&self) -> Compound {
        let mut c = Compound::default();
        match self {
            Holder::Block { dim, pos } => {
                c.insert("type", Tag::String("block".into()));
                c.insert("dim", Tag::String(dim.clone()));
                c.insert("x", Tag::Int(pos[0]));
                c.insert("y", Tag::Int(pos[1]));
                c.insert("z", Tag::Int(pos[2]));
            }
            Holder::Entity { dim, uuid, chunk } => {
                c.insert("type", Tag::String("entity".into()));
                c.insert("dim", Tag::String(dim.clone()));
                c.insert("uuid", Tag::String(uuid.clone()));
                if let Some([cx, cz]) = chunk {
                    c.insert("cx", Tag::Int(*cx));
                    c.insert("cz", Tag::Int(*cz));
                }
            }
            Holder::Player { uuid } => {
                c.insert("type", Tag::String("player".into()));
                c.insert("uuid", Tag::String(uuid.clone()));
            }
        }
        c
    }

    /// Groups findings that share a holder.
    fn key(&self) -> String {
        match self {
            Holder::Block { dim, pos } => format!("b {dim} {} {} {}", pos[0], pos[1], pos[2]),
            Holder::Entity { uuid, .. } => format!("e {uuid}"),
            Holder::Player { uuid } => format!("p {uuid}"),
        }
    }
}

pub enum Fetched {
    Found(Compound),
    Missing(String),
}

pub enum Swap {
    /// Done. `Some(key)` if the game changed what we sent (it drops what it cannot read).
    Applied(Option<String>),
    /// The holder changed since it was fetched; nothing was written.
    Conflict,
    Missing(String),
}

pub struct Client {
    stream: TcpStream,
    pub info: Info,
}

impl Client {
    /// Find the mod's config for `world`, connect and authenticate.
    pub fn connect(world: &Path, settings: &Settings) -> Result<Client> {
        let config = find_config(world, settings)?;
        let endpoint = read_config(&config)?;
        let addr = endpoint
            .addr
            .to_socket_addrs()
            .with_context(|| format!("bad address {} in {}", endpoint.addr, config.display()))?
            .next()
            .ok_or_else(|| anyhow!("{} does not resolve", endpoint.addr))?;
        let stream = TcpStream::connect_timeout(&addr, Duration::from_secs(5)).with_context(|| {
            format!(
                "cannot reach the mcscan-live mod at {} (from {}). Is the server running with the mod installed?",
                endpoint.addr,
                config.display()
            )
        })?;
        stream.set_nodelay(true).ok();
        let mut client = Client { stream, info: Info::default() };
        let mut hello = Compound::default();
        hello.insert("op", Tag::String("hello".into()));
        hello.insert("token", Tag::String(endpoint.token));
        hello.insert("protocol", Tag::Int(PROTOCOL));
        hello.insert("client", Tag::String(format!("mcscan {}", env!("CARGO_PKG_VERSION"))));
        let reply = client.call(hello, 30).context("greeting the mcscan-live mod")?;
        let protocol = reply.int("protocol").unwrap_or(0);
        if protocol != PROTOCOL as i64 {
            bail!("the mod speaks protocol {protocol}, this mcscan speaks {PROTOCOL}; update both to matching versions");
        }
        let text = |k: &str| reply.str(k).unwrap_or_default().to_string();
        client.info = Info {
            mod_version: text("mod"),
            minecraft: text("minecraft"),
            data_version: reply.int("data_version").unwrap_or(0),
            world: text("world"),
            journal: text("journal"),
        };
        Ok(client)
    }

    /// A note if the server runs a different world folder than `world`.
    /// (Not fatal: every edit is still checked against the scan's fingerprint.)
    pub fn world_mismatch(&self, world: &Path) -> Option<String> {
        let ours = world.canonicalize().ok()?;
        let theirs = Path::new(&self.info.world);
        let theirs = theirs.canonicalize().unwrap_or_else(|_| theirs.to_path_buf());
        if ours == theirs {
            return None;
        }
        Some(format!(
            "the server at this address is running the world {}, not {} - items that do not match the scan are skipped",
            theirs.display(),
            ours.display()
        ))
    }

    fn call(&mut self, request: Compound, timeout_secs: u64) -> Result<Compound> {
        let body = nbt::write_root("", &request)?;
        let timeout = Some(Duration::from_secs(timeout_secs));
        self.stream.set_read_timeout(timeout)?;
        self.stream.set_write_timeout(timeout)?;
        let mut frame = Vec::with_capacity(body.len() + 4);
        frame.extend_from_slice(&(body.len() as u32).to_be_bytes());
        frame.extend_from_slice(&body);
        self.stream.write_all(&frame).context("sending to the server")?;
        let mut len = [0u8; 4];
        self.stream.read_exact(&mut len).context("the server closed the connection")?;
        let len = u32::from_be_bytes(len) as usize;
        if len == 0 || len > MAX_FRAME {
            bail!("the server sent a malformed reply");
        }
        let mut buf = vec![0u8; len];
        self.stream.read_exact(&mut buf).context("reading the server's reply")?;
        let (_, reply) = nbt::parse(&buf).context("the server sent a malformed reply")?;
        if reply.int("ok") != Some(1) {
            bail!("{}", reply.str("error").unwrap_or("the server refused the request"));
        }
        Ok(reply)
    }

    /// `save-all flush`: afterwards the world files are what the game has in memory.
    pub fn save(&mut self) -> Result<()> {
        let mut req = Compound::default();
        req.insert("op", Tag::String("save".into()));
        let reply = self.call(req, 1800).context("asking the server to save")?;
        if reply.int("saved") != Some(1) {
            bail!("the server could not save the world (see its log)");
        }
        Ok(())
    }

    pub fn get(&mut self, holder: &Holder) -> Result<Fetched> {
        let mut req = Compound::default();
        req.insert("op", Tag::String("get".into()));
        req.insert("holder", Tag::Compound(holder.to_nbt()));
        let mut reply = self.call(req, 180)?;
        if reply.int("found") != Some(1) {
            return Ok(Fetched::Missing(reply.str("why").unwrap_or("not found on the server").to_string()));
        }
        match reply.remove("data") {
            Some(Tag::Compound(c)) => Ok(Fetched::Found(c)),
            _ => bail!("the server sent a reply without data"),
        }
    }

    /// Replace the top-level `keys` of `holder` with their values in `new`,
    /// provided they still have their values in `old`.
    pub fn swap(&mut self, holder: &Holder, keys: &[&str], old: &Compound, new: &Compound, journal: bool) -> Result<Swap> {
        let pick = |from: &Compound| {
            let mut c = Compound::default();
            for k in keys {
                if let Some(v) = from.get(k) {
                    c.insert(k, v.clone());
                }
            }
            Tag::Compound(c)
        };
        let mut req = Compound::default();
        req.insert("op", Tag::String("cas".into()));
        req.insert("holder", Tag::Compound(holder.to_nbt()));
        req.insert("keys", Tag::List(List { elem: 8, items: keys.iter().map(|k| Tag::String(k.to_string())).collect() }));
        req.insert("expect", pick(old));
        req.insert("replace", pick(new));
        req.insert("journal", Tag::Byte(journal as i8));
        let reply = self.call(req, 180)?;
        if reply.int("found") != Some(1) {
            return Ok(Swap::Missing(reply.str("why").unwrap_or("not found on the server").to_string()));
        }
        if reply.int("applied") != Some(1) {
            return Ok(Swap::Conflict);
        }
        Ok(Swap::Applied(reply.str("adjusted").map(str::to_string)))
    }
}

// ---------------------------------------------------------------------------
// From a finding in a file to a holder on the server
// ---------------------------------------------------------------------------

fn uuid_text(c: &Compound) -> Option<String> {
    match c.get("UUID")? {
        Tag::IntArray(v) if v.len() == 4 => {
            let hex: String = v.iter().map(|i| format!("{:08x}", *i as u32)).collect();
            Some(format!("{}-{}-{}-{}-{}", &hex[0..8], &hex[8..12], &hex[12..16], &hex[16..20], &hex[20..32]))
        }
        _ => None,
    }
}

/// Work out which holder the item at `path` (inside the chunk `root`) belongs
/// to and where the item is inside that holder.
fn locate(root: &Compound, rec: &Record) -> Result<(Holder, Vec<Seg>), String> {
    let path = &rec.path;
    let is_list_of = |i: usize, names: &[&str]| {
        matches!((&path[i], path.get(i + 1)), (Seg::Key(k), Some(Seg::Idx(_))) if names.contains(&k.as_str()))
    };
    let dim = || rec.dim.clone().ok_or_else(|| "the finding does not say which dimension it is in".to_string());
    // A block entity encloses everything below it (even the mob in a spawner).
    if let Some(i) = (0..path.len()).find(|&i| is_list_of(i, &["block_entities", "TileEntities"])) {
        let be = nbt::compound_at(root, &path[..i + 2]).ok_or("no longer there - rescan first")?;
        let (Some(x), Some(y), Some(z)) = (be.int("x"), be.int("y"), be.int("z")) else {
            return Err("the block entity has no position".into());
        };
        let rel = path[i + 2..].to_vec();
        return Ok((Holder::Block { dim: dim()?, pos: [x as i32, y as i32, z as i32] }, rel));
    }
    // Otherwise the innermost entity: a passenger is its own entity in the game.
    for i in (0..path.len()).rev() {
        if !is_list_of(i, &["Entities", "entities", "Passengers"]) {
            continue;
        }
        let Some(entity) = nbt::compound_at(root, &path[..i + 2]) else { continue };
        let Some(uuid) = uuid_text(entity) else { continue };
        return Ok((Holder::Entity { dim: dim()?, uuid, chunk: rec.chunk }, path[i + 2..].to_vec()));
    }
    Err("live edits reach block entities, entities and players; this item is in neither".into())
}

struct Group {
    holder: Holder,
    /// Copies of the findings with `path` relative to the holder.
    recs: Vec<Record>,
}

fn skipped(rec: &Record, why: impl Into<String>) -> Outcome {
    Outcome { n: rec.n, status: Status::Skipped(why.into()), notes: Vec::new() }
}

/// Group the findings by holder. The world files are only read to learn which
/// block entity or entity a finding's path points into.
fn resolve(world: &Path, selected: &[&Record], outcomes: &mut Vec<Outcome>) -> Vec<Group> {
    let mut groups: BTreeMap<String, Group> = BTreeMap::new();
    let mut add = |holder: Holder, rec: &Record, rel: Vec<Seg>, outcomes: &mut Vec<Outcome>| {
        if !matches!(rel.first(), Some(Seg::Key(_))) {
            outcomes.push(skipped(rec, "unexpected place for an item"));
            return;
        }
        let mut copy = rec.clone();
        copy.path = rel;
        groups.entry(holder.key()).or_insert_with(|| Group { holder, recs: Vec::new() }).recs.push(copy);
    };

    let mut by_file: BTreeMap<&str, Vec<&Record>> = BTreeMap::new();
    for &rec in selected {
        by_file.entry(rec.file.as_str()).or_default().push(rec);
    }
    let mut inflater = Inflater::default();
    let mut buf = Vec::new();
    for (rel, recs) in by_file {
        if !rel.ends_with(".mca") {
            let player = match world::classify(rel) {
                Some((Category::Players, _)) => Path::new(rel).file_stem().and_then(|s| s.to_str()),
                _ => None,
            };
            for rec in recs {
                match player {
                    Some(uuid) => add(Holder::Player { uuid: uuid.to_string() }, rec, rec.path.clone(), outcomes),
                    None => outcomes.push(skipped(
                        rec,
                        format!("live edits cannot reach {rel}; stop the server and apply without --live"),
                    )),
                }
            }
            continue;
        }
        let region = match Region::open(&world.join(rel)) {
            Ok(r) => r,
            Err(e) => {
                outcomes.extend(recs.iter().map(|r| skipped(r, format!("{rel}: {e:#}"))));
                continue;
            }
        };
        let mut by_chunk: HashMap<[i32; 2], Vec<&Record>> = HashMap::new();
        for rec in recs {
            match rec.chunk {
                Some(c) => by_chunk.entry(c).or_default().push(rec),
                None => outcomes.push(skipped(rec, "finding has no valid chunk position")),
            }
        }
        for ([cx, cz], recs) in by_chunk {
            let root = match region.read(region::chunk_index(cx, cz), &mut inflater, &mut buf) {
                Ok(true) => nbt::parse(&buf).map(|(_, root)| root),
                Ok(false) => Err(anyhow!("chunk no longer exists - rescan first")),
                Err(e) => Err(e),
            };
            match root {
                Err(e) => outcomes.extend(recs.iter().map(|r| skipped(r, format!("{e:#}")))),
                Ok(root) => {
                    for rec in recs {
                        match locate(&root, rec) {
                            Ok((holder, rel)) => add(holder, rec, rel, outcomes),
                            Err(why) => outcomes.push(skipped(rec, why)),
                        }
                    }
                }
            }
        }
    }
    groups.into_values().collect()
}

// ---------------------------------------------------------------------------
// Applying
// ---------------------------------------------------------------------------

fn edit_holder(client: &mut Client, group: &Group, opts: &ApplyOptions) -> Result<Vec<Outcome>> {
    let refs: Vec<&Record> = group.recs.iter().collect();
    let all_skipped = |why: &str| refs.iter().map(|r| skipped(r, why)).collect::<Vec<_>>();
    for _ in 0..ATTEMPTS {
        let live = match client.get(&group.holder)? {
            Fetched::Found(c) => c,
            Fetched::Missing(why) => return Ok(all_skipped(&why)),
        };
        let mut edited = live.clone();
        // Lets version-dependent operations (enchantment format) see the game version.
        if !edited.contains("DataVersion") && client.info.data_version > 0 {
            edited.insert("DataVersion", Tag::Int(client.info.data_version as i32));
        }
        let (mut outcomes, changed) = apply::process_tree(&mut edited, &refs, opts);
        if !changed || opts.dry_run {
            return Ok(outcomes);
        }
        let mut keys: Vec<&str> = Vec::new();
        for rec in &refs {
            if let Some(Seg::Key(k)) = rec.path.first() {
                if !keys.contains(&k.as_str()) && live.get(k) != edited.get(k) {
                    keys.push(k);
                }
            }
        }
        match client.swap(&group.holder, &keys, &live, &edited, opts.backup)? {
            Swap::Applied(adjusted) => {
                if let Some(key) = adjusted {
                    for o in outcomes.iter_mut().filter(|o| matches!(o.status, Status::Changed)) {
                        o.notes.push(format!("the game adjusted the result when it read {key} back - check it"));
                    }
                }
                return Ok(outcomes);
            }
            Swap::Missing(why) => return Ok(all_skipped(&why)),
            // Something moved in that inventory in the last few milliseconds: look again.
            Swap::Conflict => continue,
        }
    }
    Ok(all_skipped("the inventory kept changing on the server while editing - try again"))
}

/// The live counterpart of `apply::execute`.
pub fn execute(opts: &ApplyOptions, selected: &[&Record], progress: &Progress, settings: &Settings) -> Result<Report> {
    let mut client = Client::connect(&opts.world, settings)?;
    let mut warnings = Vec::new();
    warnings.extend(client.world_mismatch(&opts.world));

    let mut outcomes = Vec::new();
    let groups = resolve(&opts.world, selected, &mut outcomes);
    progress.files_total.store(groups.len() as u64, Ordering::Relaxed);
    let mut broken: Option<String> = None;
    for group in &groups {
        if let Some(why) = &broken {
            outcomes.extend(group.recs.iter().map(|r| skipped(r, why.clone())));
        } else {
            match edit_holder(&mut client, group, opts) {
                Ok(mut out) => outcomes.append(&mut out),
                Err(e) => {
                    let why = format!("{e:#}");
                    outcomes.extend(group.recs.iter().map(|r| skipped(r, why.clone())));
                    // A refused request leaves the connection usable; a dead one does not.
                    let mut ping = Compound::default();
                    ping.insert("op", Tag::String("ping".into()));
                    if client.call(ping, 10).is_err() {
                        broken = Some(format!("lost the connection to the server ({why})"));
                    }
                }
            }
        }
        progress.files_done.fetch_add(1, Ordering::Relaxed);
    }
    outcomes.sort_by_key(|o| o.n);
    let wrote = outcomes.iter().any(|o| matches!(o.status, Status::Changed | Status::Deleted));
    Ok(Report {
        outcomes,
        dry_run: opts.dry_run,
        files: groups.len(),
        backup_dir: None,
        journal: (wrote && !opts.dry_run && opts.backup && !client.info.journal.is_empty())
            .then(|| client.info.journal.clone()),
        live: true,
        warnings,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::item::testutil::item;

    fn rec(path: &str, dim: Option<&str>) -> Record {
        Record {
            n: 1,
            id: "minecraft:bedrock".into(),
            count: 1,
            reasons: Vec::new(),
            info: String::new(),
            source: "block_entity".into(),
            holder: None,
            dim: dim.map(str::to_string),
            pos: None,
            slot: None,
            uuid: None,
            depth: 0,
            file: "region/r.0.0.mca".into(),
            chunk: Some([3, -2]),
            path: nbt::parse_path(path).unwrap(),
            hash: String::new(),
            snbt: None,
        }
    }

    #[test]
    fn findings_map_to_holders() {
        let root = item(
            r#"{block_entities:[{id:"minecraft:chest",x:1,y:2,z:3,Items:[{Slot:0b,id:"minecraft:bedrock",count:1}]},
                 {id:"minecraft:mob_spawner",x:4,y:5,z:6,SpawnData:{entity:{id:"minecraft:zombie",
                   Passengers:[{id:"minecraft:zombie",equipment:{head:{id:"minecraft:bedrock",count:1}}}]}}}],
               Entities:[{id:"minecraft:boat",UUID:[I;1,-2,3,-4],Passengers:[
                 {id:"minecraft:zombie",UUID:[I;16,32,48,64],equipment:{head:{id:"minecraft:bedrock",count:1}}}]},
                 {id:"minecraft:item",UUIDMost:1L,UUIDLeast:2L,Item:{id:"minecraft:bedrock",count:1}}]}"#,
        );
        let over = Some("minecraft:overworld");
        let (h, rel) = locate(&root, &rec("block_entities[0].Items[0]", over)).unwrap();
        assert_eq!(h, Holder::Block { dim: "minecraft:overworld".into(), pos: [1, 2, 3] });
        assert_eq!(nbt::path_to_string(&rel), "Items[0]");
        // The mob inside a spawner belongs to the spawner.
        let (h, rel) = locate(&root, &rec("block_entities[1].SpawnData.entity.Passengers[0].equipment.head", over)).unwrap();
        assert_eq!(h, Holder::Block { dim: "minecraft:overworld".into(), pos: [4, 5, 6] });
        assert_eq!(nbt::path_to_string(&rel), "SpawnData.entity.Passengers[0].equipment.head");
        // A passenger is addressed by its own UUID, not its vehicle's.
        let (h, rel) = locate(&root, &rec("Entities[0].Passengers[0].equipment.head", over)).unwrap();
        assert_eq!(
            h,
            Holder::Entity {
                dim: "minecraft:overworld".into(),
                uuid: "00000010-0000-0020-0000-003000000040".into(),
                chunk: Some([3, -2])
            }
        );
        assert_eq!(nbt::path_to_string(&rel), "equipment.head");
        assert_eq!(h.to_nbt().str("uuid"), Some("00000010-0000-0020-0000-003000000040"));
        // Negative ints are the two's-complement halves of the UUID.
        let boat = nbt::compound_at(&root, &nbt::parse_path("Entities[0]").unwrap()).unwrap();
        assert_eq!(uuid_text(boat).unwrap(), "00000001-ffff-fffe-0000-0003fffffffc");
        // Entities without a modern UUID and findings without a dimension are refused, not guessed.
        assert!(locate(&root, &rec("Entities[1].Item", over)).is_err());
        assert!(locate(&root, &rec("block_entities[0].Items[0]", None)).is_err());
        assert!(locate(&root, &rec("Data.Player.Inventory[0]", over)).is_err());
    }

    #[test]
    fn config_file() {
        let dir = std::env::temp_dir().join(format!("mcscan-live-test-{}", std::process::id()));
        let world = dir.join("server").join("world");
        std::fs::create_dir_all(&world).unwrap();
        let none = Settings::default();
        assert!(find_config(&world, &none).unwrap_err().to_string().contains("mcscan-live mod"));
        let cfg = dir.join("server").join("config");
        std::fs::create_dir_all(&cfg).unwrap();
        let file = cfg.join(CONFIG_NAME);
        std::fs::write(&file, r#"{"enabled":true,"bind":"0.0.0.0","port":4242,"token":"abcdefghijklmnop"}"#).unwrap();
        let found = find_config(&world, &none).unwrap();
        assert_eq!(found.canonicalize().unwrap(), file.canonicalize().unwrap());
        let ep = read_config(&found).unwrap();
        assert_eq!((ep.addr.as_str(), ep.token.as_str()), ("127.0.0.1:4242", "abcdefghijklmnop"));
        std::fs::write(&file, r#"{"enabled":false,"token":"abcdefghijklmnop"}"#).unwrap();
        assert!(read_config(&found).unwrap_err().to_string().contains("switched off"));
        std::fs::write(&file, r#"{"host":"::1","token":"abcdefghijklmnop"}"#).unwrap();
        assert_eq!(read_config(&found).unwrap().addr, "[::1]:25599");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
