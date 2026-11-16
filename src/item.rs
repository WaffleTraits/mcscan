//! Finding item stacks inside arbitrary NBT, and a version-neutral view of
//! one stack that understands both the component format (1.20.5+, including
//! 26.x) and the legacy `tag` format.

use crate::nbt::{Compound, List, Seg, Tag};
use std::borrow::Cow;

// ---------------------------------------------------------------------------
// Walking
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug)]
pub enum SegRef<'a> {
    Key(&'a str),
    Idx(usize),
}

pub fn own_path(path: &[SegRef]) -> Vec<Seg> {
    path.iter()
        .map(|s| match s {
            SegRef::Key(k) => Seg::Key((*k).to_string()),
            SegRef::Idx(i) => Seg::Idx(*i),
        })
        .collect()
}

/// One item stack found while walking a tree.
pub struct Hit<'a, 'w> {
    pub item: &'a Compound,
    pub id: &'a str,
    pub path: &'w [SegRef<'a>],
    /// `id` of the nearest enclosing block entity / entity.
    pub holder: Option<&'a str>,
    pub pos: Option<[i32; 3]>,
    pub slot: Option<i32>,
    /// Ids of the items this one is nested in (outermost first).
    pub parents: &'w [&'a str],
}

#[derive(Clone, Copy, Default)]
struct Ctx<'a> {
    holder: Option<&'a str>,
    pos: Option<[i32; 3]>,
    slot: Option<i32>,
}

/// Keys whose value is an item stack even if it carries no count.
///
/// Block entities (checked against the 26.3 jar with
/// tools/survey_block_entities.py): `Items` for every container, campfire,
/// chiseled bookshelf and shelf; `item` for decorated pots and suspicious
/// sand/gravel; `Book` for lecterns; `RecordItem` for jukeboxes; `key_item`,
/// `items_to_eject` and `display_item` for vaults. The rest belong to
/// entities, players and villager trades.
const ITEM_KEYS: [&str; 30] = [
    "Items", "Inventory", "EnderItems", "item", "Item", "items", "Book", "RecordItem", "HandItems", "ArmorItems",
    "mainhand", "offhand", "head", "chest", "legs", "feet", "body", "saddle", "SelectedItem", "FireworksItem", "sell",
    "ArmorItem", "SaddleItem", "DecorItem", "key_item", "items_to_eject", "display_item", "buy", "buyB", "weapon",
];

/// Is this compound an item stack? Returns its id if so.
///
/// Every game version writes a count next to the id, so `id` + count is the
/// signature; well-known item keys are accepted without one. Entities also
/// have an `id` (and XP orbs even a `Count`), hence the `Pos`/`UUID` guard.
fn item_id<'a>(c: &'a Compound, key: Option<&str>) -> Option<&'a str> {
    let id = c.str("id")?;
    if c.contains("Pos") || c.contains("UUID") {
        return None;
    }
    let counted = c.get("count").or_else(|| c.get("Count")).and_then(Tag::as_i64).is_some();
    if counted {
        return Some(id);
    }
    match key {
        Some(k) if ITEM_KEYS.contains(&k) && !c.contains("x") => Some(id),
        _ => None,
    }
}

fn pos_of(c: &Compound) -> Option<[i32; 3]> {
    if let (Some(x), Some(y), Some(z)) = (c.int("x"), c.int("y"), c.int("z")) {
        return Some([x as i32, y as i32, z as i32]);
    }
    let l = c.list("Pos")?;
    if l.items.len() != 3 {
        return None;
    }
    let mut out = [0i32; 3];
    for (o, t) in out.iter_mut().zip(&l.items) {
        *o = t.as_f64()?.floor() as i32;
    }
    Some(out)
}

struct Walker<'a, 'f> {
    path: Vec<SegRef<'a>>,
    parents: Vec<&'a str>,
    f: &'f mut dyn FnMut(&Hit<'a, '_>),
}

impl<'a, 'f> Walker<'a, 'f> {
    fn compound(&mut self, c: &'a Compound, key: Option<&'a str>, mut ctx: Ctx<'a>) {
        if let Some(id) = item_id(c, key) {
            let slot = c.int("Slot").map(|s| s as i32).or(ctx.slot);
            (self.f)(&Hit {
                item: c,
                id,
                path: &self.path,
                holder: ctx.holder,
                pos: ctx.pos,
                slot,
                parents: &self.parents,
            });
            // Items can contain items: shulker boxes, bundles, loaded crossbows.
            self.parents.push(id);
            ctx.slot = None;
            for (k, v) in c.iter() {
                self.path.push(SegRef::Key(k));
                self.tag(v, k, ctx);
                self.path.pop();
            }
            self.parents.pop();
            return;
        }
        if let Some(id) = c.str("id") {
            ctx.holder = Some(id);
        }
        if let Some(p) = pos_of(c) {
            ctx.pos = Some(p);
        }
        // `{slot: 3, item: {...}}` wrappers used by container components.
        ctx.slot = c.int("slot").map(|s| s as i32);
        for (k, v) in c.iter() {
            self.path.push(SegRef::Key(k));
            self.tag(v, k, ctx);
            self.path.pop();
        }
    }

    fn tag(&mut self, t: &'a Tag, key: &'a str, ctx: Ctx<'a>) {
        match t {
            Tag::Compound(c) => self.compound(c, Some(key), ctx),
            Tag::List(l) if matches!(l.items.first(), Some(Tag::Compound(_)) | Some(Tag::List(_))) => {
                for (i, child) in l.items.iter().enumerate() {
                    self.path.push(SegRef::Idx(i));
                    self.tag(child, key, ctx);
                    self.path.pop();
                }
            }
            _ => {}
        }
    }
}

/// Call `f` for every item stack anywhere under `root`, however deeply nested.
pub fn walk_items<'a>(root: &'a Compound, f: &mut dyn FnMut(&Hit<'a, '_>)) {
    let mut w = Walker { path: Vec::with_capacity(16), parents: Vec::new(), f };
    w.compound(root, None, Ctx::default());
}

// ---------------------------------------------------------------------------
// Ids
// ---------------------------------------------------------------------------

/// Add the `minecraft:` namespace if none is given.
pub fn norm_id(s: &str) -> Cow<'_, str> {
    if s.contains(':') {
        Cow::Borrowed(s)
    } else {
        Cow::Owned(format!("minecraft:{s}"))
    }
}

/// Drop a leading `minecraft:` (for display and loose comparisons).
pub fn short_id(s: &str) -> &str {
    s.strip_prefix("minecraft:").unwrap_or(s)
}

fn norm_ench_id(s: &str) -> Cow<'_, str> {
    // Renamed in 1.20.5.
    if s == "minecraft:sweeping" || s == "sweeping" {
        return Cow::Borrowed("minecraft:sweeping_edge");
    }
    norm_id(s)
}

/// Numeric enchantment ids used before 1.13.
fn legacy_ench_name(n: i64) -> Option<&'static str> {
    Some(match n {
        0 => "protection",
        1 => "fire_protection",
        2 => "feather_falling",
        3 => "blast_protection",
        4 => "projectile_protection",
        5 => "respiration",
        6 => "aqua_affinity",
        7 => "thorns",
        8 => "depth_strider",
        9 => "frost_walker",
        10 => "binding_curse",
        16 => "sharpness",
        17 => "smite",
        18 => "bane_of_arthropods",
        19 => "knockback",
        20 => "fire_aspect",
        21 => "looting",
        22 => "sweeping_edge",
        32 => "efficiency",
        33 => "silk_touch",
        34 => "unbreaking",
        35 => "fortune",
        48 => "power",
        49 => "punch",
        50 => "flame",
        51 => "infinity",
        61 => "luck_of_the_sea",
        62 => "lure",
        70 => "mending",
        71 => "vanishing_curse",
        _ => return None,
    })
}

fn legacy_ench_id(c: &Compound) -> Option<String> {
    match c.get("id")? {
        Tag::String(s) => Some(norm_ench_id(s).into_owned()),
        other => {
            let n = other.as_i64()?;
            Some(match legacy_ench_name(n) {
                Some(name) => format!("minecraft:{name}"),
                None => format!("legacy:{n}"),
            })
        }
    }
}

/// Where a component lived inside the pre-1.20.5 `tag` compound.
pub fn legacy_path(component: &str) -> Option<&'static [&'static str]> {
    Some(match component {
        "minecraft:unbreakable" => &["Unbreakable"],
        "minecraft:attribute_modifiers" => &["AttributeModifiers"],
        "minecraft:custom_model_data" => &["CustomModelData"],
        "minecraft:can_break" => &["CanDestroy"],
        "minecraft:can_place_on" => &["CanPlaceOn"],
        "minecraft:entity_data" => &["EntityTag"],
        "minecraft:lore" => &["display", "Lore"],
        "minecraft:custom_name" => &["display", "Name"],
        "minecraft:damage" => &["Damage"],
        "minecraft:repair_cost" => &["RepairCost"],
        "minecraft:enchantments" => &["Enchantments"],
        "minecraft:stored_enchantments" => &["StoredEnchantments"],
        "minecraft:fireworks" => &["Fireworks"],
        _ => return None,
    })
}

const MODERN_ENCH: [(&str, bool); 2] = [("minecraft:enchantments", false), ("minecraft:stored_enchantments", true)];
const LEGACY_ENCH: [(&str, bool); 3] = [("Enchantments", false), ("ench", false), ("StoredEnchantments", true)];

// ---------------------------------------------------------------------------
// Read-only view
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub struct Ench {
    pub id: String,
    pub level: i64,
    /// True for the contents of an enchanted book.
    pub stored: bool,
}

pub struct ItemView<'a> {
    pub c: &'a Compound,
    pub id: &'a str,
    pub count: i64,
    /// 1.20.5+ `components` patch.
    pub comps: Option<&'a Compound>,
    /// Pre-1.20.5 `tag` compound.
    pub tag: Option<&'a Compound>,
    pub legacy: bool,
}

impl<'a> ItemView<'a> {
    pub fn new(c: &'a Compound) -> Option<ItemView<'a>> {
        let id = c.str("id")?;
        let count = c.int("count").or_else(|| c.int("Count")).unwrap_or(1);
        let comps = c.compound("components");
        let tag = c.compound("tag");
        let legacy = comps.is_none() && (c.contains("Count") || tag.is_some());
        Some(ItemView { c, id, count, comps, tag, legacy })
    }

    pub fn has_enchants(&self) -> bool {
        match (self.comps, self.tag) {
            (Some(c), _) => MODERN_ENCH.iter().any(|(k, _)| c.contains(k)),
            (None, Some(t)) => LEGACY_ENCH.iter().any(|(k, _)| t.contains(k)),
            _ => false,
        }
    }

    pub fn enchants(&self) -> Vec<Ench> {
        let mut out = Vec::new();
        if let Some(comps) = self.comps {
            for (key, stored) in MODERN_ENCH {
                let Some(m) = comps.compound(key) else { continue };
                // 1.20.5-1.21.4 wrapped the map in {levels:{...}}; 1.21.5+ and
                // 26.x store the map directly.
                let levels = m.compound("levels").unwrap_or(m);
                for (k, v) in levels.iter() {
                    if k == "show_in_tooltip" {
                        continue;
                    }
                    if let Some(level) = v.as_i64() {
                        out.push(Ench { id: norm_ench_id(k).into_owned(), level, stored });
                    }
                }
            }
        } else if let Some(tag) = self.tag {
            for (key, stored) in LEGACY_ENCH {
                let Some(list) = tag.list(key) else { continue };
                for e in list.items.iter().filter_map(Tag::as_compound) {
                    if let Some(id) = legacy_ench_id(e) {
                        out.push(Ench { id, level: e.int("lvl").unwrap_or(0), stored });
                    }
                }
            }
        }
        out
    }

    /// Look up a component by its full id, mapping to the legacy `tag`
    /// location for old items.
    pub fn component(&self, name: &str) -> Option<&'a Tag> {
        if let Some(comps) = self.comps {
            return comps.get(name);
        }
        let path = legacy_path(name)?;
        let (last, dirs) = path.split_last()?;
        let mut cur = self.tag?;
        for d in dirs {
            cur = cur.compound(d)?;
        }
        cur.get(last)
    }

    pub fn has_component(&self, name: &str) -> bool {
        match self.component(name) {
            None => false,
            // Legacy booleans such as Unbreakable:0b mean "absent".
            Some(t) if self.legacy => t.as_i64() != Some(0),
            Some(_) => true,
        }
    }

    pub fn damage(&self) -> Option<i64> {
        self.component("minecraft:damage").and_then(Tag::as_i64)
    }

    /// Plain text of the custom (anvil) name, if any.
    pub fn custom_name(&self) -> Option<String> {
        let t = self.component("minecraft:custom_name")?;
        Some(match t {
            Tag::String(s) => match serde_json::from_str::<serde_json::Value>(s) {
                Ok(v) if s.starts_with('{') || s.starts_with('"') || s.starts_with('[') => json_text(&v),
                _ => s.clone(),
            },
            other => tag_text(other),
        })
    }

    /// One-line description: enchantments and custom name.
    pub fn summary(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        if let Some(n) = self.custom_name() {
            parts.push(format!("\"{n}\""));
        }
        if self.has_enchants() {
            let ench = self.enchants();
            for e in ench.iter().take(8) {
                parts.push(format!("{} {}", short_id(&e.id), e.level));
            }
            if ench.len() > 8 {
                parts.push(format!("+{} more", ench.len() - 8));
            }
        }
        parts.join(", ")
    }
}

fn json_text(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Array(a) => a.iter().map(json_text).collect(),
        serde_json::Value::Object(o) => {
            let mut s = o.get("text").map(json_text).unwrap_or_default();
            if let Some(extra) = o.get("extra") {
                s.push_str(&json_text(extra));
            }
            s
        }
        _ => String::new(),
    }
}

fn tag_text(t: &Tag) -> String {
    match t {
        Tag::String(s) => s.clone(),
        Tag::List(l) => l.items.iter().map(tag_text).collect(),
        Tag::Compound(c) => {
            let mut s = c.get("text").map(tag_text).unwrap_or_default();
            if let Some(extra) = c.get("extra") {
                s.push_str(&tag_text(extra));
            }
            s
        }
        _ => String::new(),
    }
}

// ---------------------------------------------------------------------------
// Editing
// ---------------------------------------------------------------------------

pub enum EnchEdit {
    Keep,
    Set(i64),
    Remove,
}

/// Visit every enchantment on an item (regular and stored) and keep, change
/// or remove it. Returns the number of enchantments changed.
pub fn edit_enchants(item: &mut Compound, f: &mut dyn FnMut(&str, i64, bool) -> EnchEdit) -> usize {
    let mut changed = 0;
    if item.contains("components") {
        let Some(comps) = item.compound_mut("components") else { return 0 };
        for (key, stored) in MODERN_ENCH {
            let mut now_empty = false;
            if let Some(m) = comps.compound_mut(key) {
                let levels = if m.compound("levels").is_some() { m.compound_mut("levels").unwrap() } else { m };
                levels.0.retain_mut(|(k, v)| {
                    if k == "show_in_tooltip" {
                        return true;
                    }
                    let Some(level) = v.as_i64() else { return true };
                    match f(&norm_ench_id(k), level, stored) {
                        EnchEdit::Keep => true,
                        EnchEdit::Set(n) => {
                            if n != level {
                                v.set_int(n);
                                changed += 1;
                            }
                            true
                        }
                        EnchEdit::Remove => {
                            changed += 1;
                            false
                        }
                    }
                });
                now_empty = levels.0.iter().all(|(k, _)| k == "show_in_tooltip");
            }
            if now_empty {
                comps.remove(key);
            }
        }
        if comps.is_empty() {
            item.remove("components");
        }
        return changed;
    }
    let Some(tag) = item.compound_mut("tag") else { return 0 };
    for (key, stored) in LEGACY_ENCH {
        let mut now_empty = false;
        if let Some(Tag::List(list)) = tag.get_mut(key) {
            list.items.retain_mut(|t| {
                let Some(e) = t.as_compound_mut() else { return true };
                let Some(id) = legacy_ench_id(e) else { return true };
                let level = e.int("lvl").unwrap_or(0);
                match f(&id, level, stored) {
                    EnchEdit::Keep => true,
                    EnchEdit::Set(n) => {
                        if n != level {
                            if let Some(l) = e.get_mut("lvl") {
                                l.set_int(n);
                            }
                            changed += 1;
                        }
                        true
                    }
                    EnchEdit::Remove => {
                        changed += 1;
                        false
                    }
                }
            });
            now_empty = list.items.is_empty();
        }
        if now_empty {
            tag.remove(key);
        }
    }
    changed
}

/// DataVersion from which `minecraft:enchantments` is a plain id->level map
/// (1.21.5). Earlier component-format versions wrap it in `{levels:{...}}`.
const FLAT_ENCHANTMENTS_SINCE: i64 = 4300;

/// Set an enchantment level, adding the enchantment if the item lacks it.
pub fn set_enchant(item: &mut Compound, id: &str, level: i64, data_version: Option<i64>) {
    let mut found = false;
    edit_enchants(item, &mut |e, _, _| {
        if e == id {
            found = true;
            EnchEdit::Set(level)
        } else {
            EnchEdit::Keep
        }
    });
    if found {
        return;
    }
    let book = item.str("id").map(|i| short_id(i) == "enchanted_book").unwrap_or(false);
    let legacy = !item.contains("components") && (item.contains("Count") || item.contains("tag"));
    if legacy {
        if !item.contains("tag") {
            item.insert("tag", Tag::Compound(Compound::default()));
        }
        let Some(tag) = item.compound_mut("tag") else { return };
        let key = if book { "StoredEnchantments" } else { "Enchantments" };
        if !matches!(tag.get(key), Some(Tag::List(_))) {
            tag.insert(key, Tag::List(List { elem: 10, items: Vec::new() }));
        }
        let mut e = Compound::default();
        e.insert("id", Tag::String(id.to_string()));
        e.insert("lvl", Tag::Short(level.clamp(i16::MIN as i64, i16::MAX as i64) as i16));
        if let Some(Tag::List(l)) = tag.get_mut(key) {
            l.items.push(Tag::Compound(e));
        }
        return;
    }
    if !item.contains("components") {
        item.insert("components", Tag::Compound(Compound::default()));
    }
    let Some(comps) = item.compound_mut("components") else { return };
    let key = if book { "minecraft:stored_enchantments" } else { "minecraft:enchantments" };
    let value = Tag::Int(level.clamp(i32::MIN as i64, i32::MAX as i64) as i32);
    match comps.compound_mut(key) {
        Some(m) => {
            let levels = if m.compound("levels").is_some() { m.compound_mut("levels").unwrap() } else { m };
            levels.insert(id, value);
        }
        None => {
            let mut map = Compound::default();
            map.insert(id, value);
            let wrapped = matches!(data_version, Some(v) if v < FLAT_ENCHANTMENTS_SINCE);
            if wrapped {
                let mut outer = Compound::default();
                outer.insert("levels", Tag::Compound(map));
                comps.insert(key, Tag::Compound(outer));
            } else {
                comps.insert(key, Tag::Compound(map));
            }
        }
    }
}

/// Remove a component (full id, or a `!minecraft:x` removal marker).
/// Works on both formats. Returns whether anything was removed.
pub fn remove_component(item: &mut Compound, name: &str) -> bool {
    if item.contains("components") {
        let Some(comps) = item.compound_mut("components") else { return false };
        let removed = comps.remove(name).is_some();
        if comps.is_empty() {
            item.remove("components");
        }
        return removed;
    }
    let Some(path) = legacy_path(name) else { return false };
    let Some((last, dirs)) = path.split_last() else { return false };
    let Some(mut cur) = item.compound_mut("tag") else { return false };
    for d in dirs {
        cur = match cur.compound_mut(d) {
            Some(c) => c,
            None => return false,
        };
    }
    cur.remove(last).is_some()
}

#[cfg(test)]
pub mod testutil {
    use crate::nbt::{parse_snbt, Compound, Tag};

    pub fn item(snbt: &str) -> Compound {
        match parse_snbt(snbt).expect("valid SNBT in test") {
            Tag::Compound(c) => c,
            _ => panic!("not a compound"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testutil::item;
    use super::*;

    #[test]
    fn finds_nested_items_with_context() {
        let root = item(
            r#"{DataVersion:5023,block_entities:[
                {id:"minecraft:chest",x:10,y:64,z:-3,Items:[
                    {Slot:4b,id:"minecraft:red_shulker_box",count:1,components:{"minecraft:container":[
                        {slot:7,item:{id:"minecraft:diamond",count:3}}]}}]},
                {id:"minecraft:decorated_pot",x:0,y:0,z:0,item:{id:"minecraft:stick"}}],
              entities:[{id:"minecraft:experience_orb",Count:5,Pos:[1.5d,2d,-0.5d],UUID:[I;1,2,3,4]},
                        {id:"minecraft:item",Pos:[1.5d,2d,-0.5d],UUID:[I;1,2,3,5],Item:{id:"minecraft:egg",count:16}}]}"#,
        );
        let mut seen = Vec::new();
        walk_items(&root, &mut |h| {
            seen.push((h.id.to_string(), h.holder.map(str::to_string), h.pos, h.slot, h.parents.len(), own_path(h.path)));
        });
        assert_eq!(seen.len(), 4, "{seen:?}");
        assert_eq!(seen[0].0, "minecraft:red_shulker_box");
        assert_eq!(seen[0].1.as_deref(), Some("minecraft:chest"));
        assert_eq!(seen[0].2, Some([10, 64, -3]));
        assert_eq!(seen[0].3, Some(4));
        assert_eq!(seen[1].0, "minecraft:diamond");
        assert_eq!(seen[1].3, Some(7));
        assert_eq!(seen[1].4, 1);
        assert_eq!(
            seen[1].5,
            vec![
                Seg::key("block_entities"),
                Seg::Idx(0),
                Seg::key("Items"),
                Seg::Idx(0),
                Seg::key("components"),
                Seg::key("minecraft:container"),
                Seg::Idx(0),
                Seg::key("item")
            ]
        );
        assert_eq!(seen[2].0, "minecraft:stick");
        assert_eq!(seen[3].0, "minecraft:egg");
        assert_eq!(seen[3].1.as_deref(), Some("minecraft:item"));
        assert_eq!(seen[3].2, Some([1, 2, -1]));
    }

    #[test]
    fn enchants_in_every_format() {
        let flat = item(r#"{id:"minecraft:diamond_sword",count:1,components:{"minecraft:enchantments":{"minecraft:sharpness":255}}}"#);
        let wrapped = item(
            r#"{id:"minecraft:diamond_sword",count:1,components:{"minecraft:enchantments":{levels:{"minecraft:sharpness":255},show_in_tooltip:0b}}}"#,
        );
        let legacy = item(r#"{id:"minecraft:diamond_sword",Count:1b,tag:{Enchantments:[{id:"minecraft:sharpness",lvl:255s}]}}"#);
        let ancient = item(r#"{id:"minecraft:diamond_sword",Count:1b,Damage:0s,tag:{ench:[{id:16s,lvl:255s}]}}"#);
        for c in [&flat, &wrapped, &legacy, &ancient] {
            let v = ItemView::new(c).unwrap();
            assert_eq!(v.enchants(), vec![Ench { id: "minecraft:sharpness".into(), level: 255, stored: false }]);
        }
    }

    #[test]
    fn editing_enchants() {
        let mut c = item(
            r#"{id:"minecraft:diamond_sword",count:1,components:{"minecraft:enchantments":{"minecraft:sharpness":255,"minecraft:smite":5}}}"#,
        );
        let n = edit_enchants(&mut c, &mut |id, _, _| match id {
            "minecraft:sharpness" => EnchEdit::Set(5),
            _ => EnchEdit::Remove,
        });
        assert_eq!(n, 2);
        assert_eq!(
            c,
            item(r#"{id:"minecraft:diamond_sword",count:1,components:{"minecraft:enchantments":{"minecraft:sharpness":5}}}"#)
        );
        edit_enchants(&mut c, &mut |_, _, _| EnchEdit::Remove);
        assert_eq!(c, item(r#"{id:"minecraft:diamond_sword",count:1}"#));

        set_enchant(&mut c, "minecraft:looting", 3, Some(5023));
        assert_eq!(
            c,
            item(r#"{id:"minecraft:diamond_sword",count:1,components:{"minecraft:enchantments":{"minecraft:looting":3}}}"#)
        );

        let mut old = item(r#"{id:"minecraft:bow",Count:1b,tag:{Unbreakable:1b,Enchantments:[{id:"minecraft:power",lvl:10s}]}}"#);
        edit_enchants(&mut old, &mut |_, _, _| EnchEdit::Set(5));
        assert!(ItemView::new(&old).unwrap().has_component("minecraft:unbreakable"));
        assert!(remove_component(&mut old, "minecraft:unbreakable"));
        assert_eq!(old, item(r#"{id:"minecraft:bow",Count:1b,tag:{Enchantments:[{id:"minecraft:power",lvl:5s}]}}"#));
    }

    #[test]
    fn custom_names() {
        let modern = item(r#"{id:"minecraft:stick",count:1,components:{"minecraft:custom_name":{text:"Bo",extra:[{text:"b"}]}}}"#);
        assert_eq!(ItemView::new(&modern).unwrap().custom_name().as_deref(), Some("Bob"));
        let json = item(r#"{id:"minecraft:stick",Count:1b,tag:{display:{Name:'{"text":"Bob"}'}}}"#);
        assert_eq!(ItemView::new(&json).unwrap().custom_name().as_deref(), Some("Bob"));
    }
}
