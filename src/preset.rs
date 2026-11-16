//! Rule presets. The built-in `illegal` preset flags items that vanilla
//! survival gameplay cannot produce; its data tables are generated from the
//! game jar by `tools/gen_preset.py`.

use crate::item::{self, norm_id, short_id, EnchEdit, ItemView};
use crate::nbt::{self, Compound, Seg, Tag};
use anyhow::{anyhow, bail, Context, Result};
use serde::Deserialize;
use std::collections::{HashMap, HashSet};

const BUILTIN_ILLEGAL: &str = include_str!("../presets/illegal.json");

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Rule {
    EnchantOverMax,
    EnchantUnknown,
    EnchantWrongItem,
    EnchantConflict,
    EnchantConflictLegacy,
    StackOversize,
    BadCount,
    UnobtainableItem,
    DeniedComponent,
    CustomMarker,
    PotionIllegal,
    FireworkFlight,
    NegativeDamage,
    NestedShulker,
}

pub const RULES: [Rule; 14] = [
    Rule::EnchantOverMax,
    Rule::EnchantUnknown,
    Rule::EnchantWrongItem,
    Rule::EnchantConflict,
    Rule::EnchantConflictLegacy,
    Rule::StackOversize,
    Rule::BadCount,
    Rule::UnobtainableItem,
    Rule::DeniedComponent,
    Rule::CustomMarker,
    Rule::PotionIllegal,
    Rule::FireworkFlight,
    Rule::NegativeDamage,
    Rule::NestedShulker,
];

impl Rule {
    pub fn id(self) -> &'static str {
        match self {
            Rule::EnchantOverMax => "enchant_over_max",
            Rule::EnchantUnknown => "enchant_unknown",
            Rule::EnchantWrongItem => "enchant_wrong_item",
            Rule::EnchantConflict => "enchant_conflict",
            Rule::EnchantConflictLegacy => "enchant_conflict_legacy",
            Rule::StackOversize => "stack_oversize",
            Rule::BadCount => "bad_count",
            Rule::UnobtainableItem => "unobtainable_item",
            Rule::DeniedComponent => "denied_component",
            Rule::CustomMarker => "custom_marker",
            Rule::PotionIllegal => "potion_illegal",
            Rule::FireworkFlight => "firework_flight",
            Rule::NegativeDamage => "negative_damage",
            Rule::NestedShulker => "nested_shulker",
        }
    }

    pub fn parse(s: &str) -> Option<Rule> {
        RULES.into_iter().find(|r| r.id() == s)
    }

    /// (what it catches, what `--fix` does about it)
    pub fn describe(self) -> (&'static str, &'static str) {
        match self {
            Rule::EnchantOverMax => {
                ("enchantment level above the vanilla maximum (or below 1), e.g. Sharpness 255", "clamp to the maximum")
            }
            Rule::EnchantUnknown => ("enchantment that does not exist in vanilla", "remove the enchantment"),
            Rule::EnchantWrongItem => {
                ("enchantment on an item that cannot hold it even via anvil, e.g. Sharpness on a stick", "remove the enchantment")
            }
            Rule::EnchantConflict => {
                ("mutually exclusive enchantments together, e.g. Sharpness + Smite", "remove the later enchantment")
            }
            Rule::EnchantConflictLegacy => (
                "exclusive today but obtainable in old releases (1.14-1.14.2 stacked protections, Infinity + Mending before 1.11.1)",
                "remove the later enchantment",
            ),
            Rule::StackOversize => ("more items in one stack than the item's stack size allows", "clamp the count"),
            Rule::BadCount => ("zero or negative count", "delete the item"),
            Rule::UnobtainableItem => {
                ("item with no survival source: bedrock, barrier, command blocks, spawn eggs, spawners, ...", "delete the item")
            }
            Rule::DeniedComponent => (
                "component only commands/creative can set: unbreakable, attribute_modifiers, max_stack_size, ...",
                "remove the component",
            ),
            Rule::CustomMarker => (
                "custom_data / custom_model_data / lore - never from survival, but plugins and data packs use them",
                "remove the component",
            ),
            Rule::PotionIllegal => {
                ("potion or tipped arrow with custom effects, a custom colour, or no potion type", "strip the custom part / delete")
            }
            Rule::FireworkFlight => ("firework rocket with flight duration above 3", "clamp to 3"),
            Rule::NegativeDamage => ("negative damage value (more durability than the item can have)", "set damage to 0"),
            Rule::NestedShulker => ("shulker box inside another shulker box or bundle", "delete the inner shulker box"),
        }
    }

    /// Off unless `--strict` / `--enable` asks for it.
    pub fn strict_only(self) -> bool {
        matches!(self, Rule::CustomMarker)
    }
}

/// How to bring an item back in line with one finding.
#[derive(Debug, Clone, PartialEq)]
pub enum Fix {
    SetEnchant { id: String, stored: bool, level: i64 },
    RemoveEnchant { id: String, stored: bool },
    SetCount(i64),
    Delete,
    RemoveComponent(String),
    /// Path is relative to the item compound.
    SetInt { path: Vec<Seg>, value: i64 },
    RemovePath(Vec<Seg>),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Reason {
    pub rule: Rule,
    pub detail: String,
    pub fix: Fix,
}

#[derive(Deserialize)]
struct EnchDef {
    max_level: i64,
    #[serde(default)]
    supported: Vec<String>,
    #[serde(default)]
    exclusive: Vec<String>,
}

#[derive(Deserialize)]
struct PresetFile {
    #[serde(default)]
    name: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    game_version: String,
    #[serde(default)]
    data_version: i64,
    #[serde(default)]
    enchantments: HashMap<String, EnchDef>,
    #[serde(default)]
    item_tags: HashMap<String, Vec<String>>,
    #[serde(default)]
    legacy_conflict_ok: Vec<[String; 2]>,
    #[serde(default)]
    max_stack: HashMap<String, i64>,
    #[serde(default)]
    unobtainable: Vec<String>,
    #[serde(default)]
    denied_components: Vec<String>,
    #[serde(default)]
    strict_components: Vec<String>,
}

enum Supported {
    Tag(usize),
    Id(String),
}

struct Ench {
    max_level: i64,
    supported: Vec<Supported>,
    exclusive: HashSet<String>,
}

pub struct Preset {
    pub name: String,
    pub description: String,
    pub game_version: String,
    pub data_version: i64,
    ench: HashMap<String, Ench>,
    tags: Vec<HashSet<String>>,
    legacy_ok: HashSet<String>,
    max_stack: HashMap<String, i64>,
    unobtainable: HashSet<String>,
    denied: HashSet<String>,
    strict: HashSet<String>,
    /// Denied/strict components that have a pre-1.20.5 `tag` equivalent.
    legacy_components: Vec<(String, Rule)>,
    enabled: [bool; RULES.len()],
}

fn pair_key(a: &str, b: &str) -> String {
    if a <= b {
        format!("{a}|{b}")
    } else {
        format!("{b}|{a}")
    }
}

fn n(s: &str) -> String {
    norm_id(s).into_owned()
}

impl Preset {
    /// `spec` is a built-in preset name or the path of a preset JSON file.
    pub fn load(spec: &str) -> Result<Preset> {
        let text = match spec {
            "illegal" => BUILTIN_ILLEGAL.to_string(),
            path => std::fs::read_to_string(path)
                .with_context(|| format!("no built-in preset named {path:?}, and it could not be read as a file"))?,
        };
        Preset::from_json(&text).with_context(|| format!("loading preset {spec:?}"))
    }

    pub fn from_json(text: &str) -> Result<Preset> {
        let f: PresetFile = serde_json::from_str(text.trim_start_matches('\u{feff}'))?;
        let mut tag_index: HashMap<String, usize> = HashMap::new();
        let mut tags: Vec<HashSet<String>> = Vec::new();
        for (name, ids) in &f.item_tags {
            tag_index.insert(name.clone(), tags.len());
            tags.push(ids.iter().map(|i| n(i)).collect());
        }
        let mut ench = HashMap::new();
        for (id, def) in f.enchantments {
            let mut supported = Vec::new();
            for s in &def.supported {
                match s.strip_prefix('#') {
                    Some(tag) => supported.push(Supported::Tag(
                        *tag_index.get(tag).ok_or_else(|| anyhow!("enchantment {id} refers to unknown item tag #{tag}"))?,
                    )),
                    None => supported.push(Supported::Id(n(s))),
                }
            }
            if def.max_level < 1 {
                bail!("enchantment {id} has max_level {}", def.max_level);
            }
            ench.insert(
                n(&id),
                Ench { max_level: def.max_level, supported, exclusive: def.exclusive.iter().map(|e| n(e)).collect() },
            );
        }
        let denied: HashSet<String> = f.denied_components.iter().map(|c| n(c)).collect();
        let strict: HashSet<String> = f.strict_components.iter().map(|c| n(c)).collect();
        let mut legacy_components = Vec::new();
        for (set, rule) in [(&denied, Rule::DeniedComponent), (&strict, Rule::CustomMarker)] {
            for c in set {
                if item::legacy_path(c).is_some() {
                    legacy_components.push((c.clone(), rule));
                }
            }
        }
        legacy_components.sort_by(|a, b| a.0.cmp(&b.0));
        let mut enabled = [true; RULES.len()];
        for (i, r) in RULES.iter().enumerate() {
            enabled[i] = !r.strict_only();
        }
        Ok(Preset {
            name: f.name,
            description: f.description,
            game_version: f.game_version,
            data_version: f.data_version,
            ench,
            tags,
            legacy_ok: f.legacy_conflict_ok.iter().map(|[a, b]| pair_key(&n(a), &n(b))).collect(),
            max_stack: f.max_stack.iter().map(|(k, v)| (n(k), *v)).collect(),
            unobtainable: f.unobtainable.iter().map(|i| n(i)).collect(),
            denied,
            strict,
            legacy_components,
            enabled,
        })
    }

    pub fn set_enabled(&mut self, rule: Rule, on: bool) {
        self.enabled[RULES.iter().position(|r| *r == rule).expect("rule in table")] = on;
    }

    pub fn enable_all(&mut self) {
        self.enabled = [true; RULES.len()];
    }

    pub fn is_enabled(&self, rule: Rule) -> bool {
        self.enabled[RULES.iter().position(|r| *r == rule).expect("rule in table")]
    }

    pub fn max_level(&self, ench: &str) -> Option<i64> {
        self.ench.get(ench).map(|e| e.max_level)
    }

    pub fn max_stack(&self, id: &str) -> i64 {
        self.max_stack.get(id).copied().unwrap_or(64)
    }

    fn supports(&self, e: &Ench, item: &str) -> bool {
        e.supported.iter().any(|s| match s {
            Supported::Tag(i) => self.tags[*i].contains(item),
            Supported::Id(id) => id == item,
        })
    }

    /// Append every enabled rule this item breaks. `parents` are the ids of
    /// the items it is nested inside (shulker box, bundle, ...).
    pub fn check(&self, v: &ItemView, parents: &[&str], out: &mut Vec<Reason>) {
        let before = out.len();
        self.check_all(v, parents, out);
        if out.len() > before {
            let enabled = &self.enabled;
            let mut i = before;
            out.retain(|r| {
                let keep = i < before || enabled[RULES.iter().position(|x| *x == r.rule).unwrap_or(0)];
                i += 1;
                keep
            });
        }
    }

    fn check_all(&self, v: &ItemView, parents: &[&str], out: &mut Vec<Reason>) {
        let id = norm_id(v.id);
        let id: &str = &id;
        let short = short_id(id);
        let mut push = |rule: Rule, detail: String, fix: Fix| out.push(Reason { rule, detail, fix });

        // --- count -----------------------------------------------------------
        if v.count < 1 {
            push(Rule::BadCount, format!("count {}", v.count), Fix::Delete);
        } else if v.count > 1 {
            let max = self.max_stack(id);
            // An overridden stack size is reported as a denied component instead.
            if v.count > max && !v.has_component("minecraft:max_stack_size") {
                push(Rule::StackOversize, format!("{} in a stack (max {max})", v.count), Fix::SetCount(max));
            }
        }

        // --- the item itself ---------------------------------------------------
        if self.unobtainable.contains(id) {
            push(Rule::UnobtainableItem, short.to_string(), Fix::Delete);
        }
        if !parents.is_empty() && short.ends_with("shulker_box") {
            let outer = parents.last().map(|p| short_id(p)).unwrap_or("item");
            push(Rule::NestedShulker, format!("inside {outer}"), Fix::Delete);
        }

        // --- enchantments ------------------------------------------------------
        if v.has_enchants() {
            let enchants = v.enchants();
            for (i, e) in enchants.iter().enumerate() {
                let name = short_id(&e.id);
                let def = match self.ench.get(&e.id) {
                    Some(d) => d,
                    None => {
                        push(
                            Rule::EnchantUnknown,
                            format!("{name} {}", e.level),
                            Fix::RemoveEnchant { id: e.id.clone(), stored: e.stored },
                        );
                        continue;
                    }
                };
                if e.level > def.max_level {
                    push(
                        Rule::EnchantOverMax,
                        format!("{name} {} (max {})", e.level, def.max_level),
                        Fix::SetEnchant { id: e.id.clone(), stored: e.stored, level: def.max_level },
                    );
                } else if e.level < 1 {
                    push(
                        Rule::EnchantOverMax,
                        format!("{name} {} (below 1)", e.level),
                        Fix::RemoveEnchant { id: e.id.clone(), stored: e.stored },
                    );
                }
                if !e.stored && short != "enchanted_book" && !self.supports(def, id) {
                    push(
                        Rule::EnchantWrongItem,
                        format!("{name} on {short}"),
                        Fix::RemoveEnchant { id: e.id.clone(), stored: e.stored },
                    );
                }
                for prev in &enchants[..i] {
                    if prev.stored != e.stored || prev.id == e.id {
                        continue;
                    }
                    let clash = def.exclusive.contains(&prev.id)
                        || self.ench.get(&prev.id).map_or(false, |p| p.exclusive.contains(&e.id));
                    if clash {
                        let rule = if self.legacy_ok.contains(&pair_key(&prev.id, &e.id)) {
                            Rule::EnchantConflictLegacy
                        } else {
                            Rule::EnchantConflict
                        };
                        push(
                            rule,
                            format!("{} + {name}", short_id(&prev.id)),
                            Fix::RemoveEnchant { id: e.id.clone(), stored: e.stored },
                        );
                    }
                }
            }
        }

        // --- components --------------------------------------------------------
        if let Some(comps) = v.comps {
            for (k, _) in comps.iter() {
                if let Some(base) = k.strip_prefix('!') {
                    push(
                        Rule::DeniedComponent,
                        format!("default {} removed", short_id(base)),
                        Fix::RemoveComponent(k.to_string()),
                    );
                    continue;
                }
                let full = norm_id(k);
                let full: &str = &full;
                let rule = if self.denied.contains(full) {
                    Rule::DeniedComponent
                } else if self.strict.contains(full) {
                    Rule::CustomMarker
                } else {
                    continue;
                };
                push(rule, short_id(full).to_string(), Fix::RemoveComponent(k.to_string()));
            }
        } else if v.tag.is_some() {
            for (name, rule) in &self.legacy_components {
                if v.has_component(name) {
                    push(*rule, short_id(name).to_string(), Fix::RemoveComponent(name.clone()));
                }
            }
        }

        // --- item-specific data --------------------------------------------------
        if matches!(short, "potion" | "splash_potion" | "lingering_potion" | "tipped_arrow") {
            self.check_potion(v, &mut push);
        }
        if short == "firework_rocket" {
            let (path, flight) = if v.legacy {
                (vec![Seg::key("tag"), Seg::key("Fireworks"), Seg::key("Flight")], "Flight")
            } else {
                (vec![Seg::key("components"), Seg::key("minecraft:fireworks"), Seg::key("flight_duration")], "flight_duration")
            };
            let duration = v
                .component("minecraft:fireworks")
                .and_then(Tag::as_compound)
                .and_then(|f| f.int(flight))
                .unwrap_or(1);
            if duration > 3 {
                push(Rule::FireworkFlight, format!("flight duration {duration}"), Fix::SetInt { path, value: 3 });
            }
        }
        if let Some(d) = v.damage() {
            // Before 1.13 "Damage" was item metadata and lived outside `tag`.
            if d < 0 {
                let path = if v.legacy {
                    vec![Seg::key("tag"), Seg::key("Damage")]
                } else {
                    vec![Seg::key("components"), Seg::key("minecraft:damage")]
                };
                push(Rule::NegativeDamage, format!("damage {d}"), Fix::SetInt { path, value: 0 });
            }
        }
    }

    fn check_potion(&self, v: &ItemView, push: &mut dyn FnMut(Rule, String, Fix)) {
        let rule = Rule::PotionIllegal;
        if !v.legacy {
            let base = [Seg::key("components"), Seg::key("minecraft:potion_contents")];
            let sub = |k: &str| {
                let mut p = base.to_vec();
                p.push(Seg::key(k));
                p
            };
            match v.comps.and_then(|c| c.get("minecraft:potion_contents")) {
                None => push(rule, "no potion type (uncraftable)".into(), Fix::Delete),
                Some(Tag::Compound(pc)) => {
                    let custom = pc.list("custom_effects").map_or(false, |l| !l.items.is_empty());
                    if !pc.contains("potion") {
                        // Stripping the custom part would leave an uncraftable potion.
                        let what = if custom { "custom effects, no potion type" } else { "no potion type (uncraftable)" };
                        push(rule, what.into(), Fix::Delete);
                        return;
                    }
                    if custom {
                        push(rule, "custom effects".into(), Fix::RemovePath(sub("custom_effects")));
                    }
                    if pc.contains("custom_color") {
                        push(rule, "custom colour".into(), Fix::RemovePath(sub("custom_color")));
                    }
                }
                Some(_) => {}
            }
            return;
        }
        // Pre-1.9 potions encode their type in a top-level Damage value.
        let potion = v.tag.and_then(|t| t.str("Potion"));
        let untyped = (potion.is_none() || potion == Some("minecraft:empty")) && !v.c.contains("Damage");
        let effect_keys = ["CustomPotionEffects", "custom_potion_effects"];
        let custom_key = v.tag.and_then(|t| effect_keys.into_iter().find(|k| t.list(k).map_or(false, |l| !l.items.is_empty())));
        if untyped {
            let what = if custom_key.is_some() { "custom effects, no potion type" } else { "no potion type (uncraftable)" };
            push(rule, what.into(), Fix::Delete);
            return;
        }
        if let Some(key) = custom_key {
            push(rule, "custom effects".into(), Fix::RemovePath(vec![Seg::key("tag"), Seg::key(key)]));
        }
        if v.tag.map_or(false, |t| t.contains("CustomPotionColor")) {
            push(rule, "custom colour".into(), Fix::RemovePath(vec![Seg::key("tag"), Seg::key("CustomPotionColor")]));
        }
    }
}

/// Apply one fix to an item. Returns true if the item must be deleted.
pub fn apply_fix(item: &mut Compound, fix: &Fix) -> bool {
    match fix {
        Fix::Delete => return true,
        Fix::SetEnchant { id, stored, level } => {
            item::edit_enchants(item, &mut |e, _, s| {
                if e == id && s == *stored {
                    EnchEdit::Set(*level)
                } else {
                    EnchEdit::Keep
                }
            });
        }
        Fix::RemoveEnchant { id, stored } => {
            item::edit_enchants(item, &mut |e, _, s| {
                if e == id && s == *stored {
                    EnchEdit::Remove
                } else {
                    EnchEdit::Keep
                }
            });
        }
        Fix::SetCount(n) => set_count(item, *n),
        Fix::RemoveComponent(name) => {
            item::remove_component(item, name);
        }
        Fix::SetInt { path, value } => {
            if let Some(t) = nbt::get_path_mut(item, path) {
                t.set_int(*value);
            }
        }
        Fix::RemovePath(path) => {
            remove_path(item, path);
        }
    }
    false
}

pub fn set_count(item: &mut Compound, n: i64) {
    if item.contains("count") {
        if let Some(t) = item.get_mut("count") {
            t.set_int(n);
        }
    } else if let Some(t) = item.get_mut("Count") {
        t.set_int(n);
    } else {
        item.insert("count", Tag::Int(n as i32));
    }
}

/// Remove the value at `path` (a compound key or a list element).
pub fn remove_path(root: &mut Compound, path: &[Seg]) -> bool {
    let Some((last, parent)) = path.split_last() else { return false };
    match last {
        Seg::Key(k) => nbt::compound_at_mut(root, parent).map_or(false, |c| c.remove(k).is_some()),
        Seg::Idx(i) => match nbt::get_path_mut(root, parent).and_then(Tag::as_list_mut) {
            Some(l) if *i < l.items.len() => {
                l.items.remove(*i);
                true
            }
            _ => false,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::item::testutil::item;

    fn rules(p: &Preset, snbt: &str, parents: &[&str]) -> Vec<&'static str> {
        let c = item(snbt);
        let mut out = Vec::new();
        p.check(&ItemView::new(&c).unwrap(), parents, &mut out);
        out.iter().map(|r| r.rule.id()).collect()
    }

    /// Check, apply every fix, and return the repaired item (None = deleted).
    fn fixed(p: &Preset, snbt: &str) -> Option<Compound> {
        let mut c = item(snbt);
        let mut out = Vec::new();
        p.check(&ItemView::new(&c).unwrap(), &[], &mut out);
        assert!(!out.is_empty(), "expected findings for {snbt}");
        for r in &out {
            if apply_fix(&mut c, &r.fix) {
                return None;
            }
        }
        let mut again = Vec::new();
        p.check(&ItemView::new(&c).unwrap(), &[], &mut again);
        assert!(again.is_empty(), "still illegal after fix: {again:?}");
        Some(c)
    }

    #[test]
    fn builtin_preset_loads() {
        let p = Preset::load("illegal").unwrap();
        assert_eq!(p.max_level("minecraft:sharpness"), Some(5));
        assert_eq!(p.max_level("minecraft:lunge"), Some(3));
        assert_eq!(p.max_stack("minecraft:ender_pearl"), 16);
        assert_eq!(p.max_stack("minecraft:diamond_sword"), 1);
        assert_eq!(p.max_stack("minecraft:cobblestone"), 64);
        assert!(!p.is_enabled(Rule::CustomMarker));
    }

    #[test]
    fn legit_items_pass() {
        let p = Preset::load("illegal").unwrap();
        for s in [
            r#"{id:"minecraft:cobblestone",count:64}"#,
            r#"{id:"minecraft:diamond_sword",count:1,components:{"minecraft:enchantments":{"minecraft:sharpness":5,"minecraft:unbreaking":3,"minecraft:mending":1,"minecraft:looting":3,"minecraft:sweeping_edge":3,"minecraft:fire_aspect":2},"minecraft:damage":12,"minecraft:custom_name":"Stabby","minecraft:repair_cost":7}}"#,
            r#"{id:"minecraft:netherite_axe",count:1,components:{"minecraft:enchantments":{"minecraft:sharpness":5,"minecraft:efficiency":5,"minecraft:silk_touch":1}}}"#,
            r#"{id:"minecraft:enchanted_book",count:1,components:{"minecraft:stored_enchantments":{"minecraft:protection":4}}}"#,
            r#"{id:"minecraft:potion",count:1,components:{"minecraft:potion_contents":{potion:"minecraft:water"}}}"#,
            r#"{id:"minecraft:firework_rocket",count:64,components:{"minecraft:fireworks":{flight_duration:3b}}}"#,
            r#"{id:"minecraft:ender_pearl",count:16}"#,
            r#"{id:"minecraft:red_shulker_box",count:1,components:{"minecraft:container":[{slot:0,item:{id:"minecraft:stone",count:64}}]}}"#,
            r#"{id:"minecraft:diamond_sword",Count:1b,tag:{Damage:3,Enchantments:[{id:"minecraft:sharpness",lvl:5s}],RepairCost:1}}"#,
            r#"{id:"minecraft:potion",Count:1b,tag:{Potion:"minecraft:healing"}}"#,
            r#"{id:"minecraft:potion",Count:1b,Damage:8261s}"#,
        ] {
            assert_eq!(rules(&p, s, &[]), Vec::<&str>::new(), "{s}");
        }
    }

    #[test]
    fn illegal_items_are_flagged() {
        let p = Preset::load("illegal").unwrap();
        let cases: [(&str, &[&str]); 16] = [
            (r#"{id:"minecraft:diamond_sword",count:1,components:{"minecraft:enchantments":{"minecraft:sharpness":255}}}"#, &["enchant_over_max"]),
            (r#"{id:"minecraft:diamond_sword",Count:1b,tag:{Enchantments:[{id:"minecraft:sharpness",lvl:32767s}]}}"#, &["enchant_over_max"]),
            (r#"{id:"minecraft:diamond_sword",count:1,components:{"minecraft:enchantments":{"minecraft:sharpness":5,"minecraft:smite":5}}}"#, &["enchant_conflict"]),
            (r#"{id:"minecraft:diamond_chestplate",count:1,components:{"minecraft:enchantments":{"minecraft:protection":4,"minecraft:blast_protection":4}}}"#, &["enchant_conflict_legacy"]),
            (r#"{id:"minecraft:stick",count:1,components:{"minecraft:enchantments":{"minecraft:knockback":2}}}"#, &["enchant_wrong_item"]),
            (r#"{id:"minecraft:diamond_sword",count:1,components:{"minecraft:enchantments":{"mypack:zap":1}}}"#, &["enchant_unknown"]),
            (r#"{id:"minecraft:bedrock",count:1}"#, &["unobtainable_item"]),
            (r#"{id:"minecraft:zombie_spawn_egg",count:3}"#, &["unobtainable_item"]),
            (r#"{id:"minecraft:diamond_sword",count:64}"#, &["stack_oversize"]),
            (r#"{id:"minecraft:stone",count:0}"#, &["bad_count"]),
            (r#"{id:"minecraft:diamond_pickaxe",count:1,components:{"minecraft:unbreakable":{}}}"#, &["denied_component"]),
            (r#"{id:"minecraft:diamond_pickaxe",Count:1b,tag:{Unbreakable:1b}}"#, &["denied_component"]),
            (r#"{id:"minecraft:apple",count:1,components:{"!minecraft:food":{}}}"#, &["denied_component"]),
            (r#"{id:"minecraft:potion",count:1,components:{"minecraft:potion_contents":{custom_effects:[{id:"minecraft:instant_damage",amplifier:125b}]}}}"#, &["potion_illegal"]),
            (r#"{id:"minecraft:potion",count:1}"#, &["potion_illegal"]),
            (r#"{id:"minecraft:firework_rocket",count:1,components:{"minecraft:fireworks":{flight_duration:127b}}}"#, &["firework_flight"]),
        ];
        for (s, want) in cases {
            assert_eq!(rules(&p, s, &[]), want.to_vec(), "{s}");
        }
        assert_eq!(rules(&p, r#"{id:"minecraft:shulker_box",count:1}"#, &["minecraft:red_shulker_box"]), vec!["nested_shulker"]);
    }

    #[test]
    fn strict_rules_are_opt_in() {
        let mut p = Preset::load("illegal").unwrap();
        let s = r#"{id:"minecraft:paper",count:1,components:{"minecraft:custom_data":{vote:1b},"minecraft:lore":["hi"]}}"#;
        assert!(rules(&p, s, &[]).is_empty());
        p.enable_all();
        assert_eq!(rules(&p, s, &[]), vec!["custom_marker", "custom_marker"]);
        p.set_enabled(Rule::CustomMarker, false);
        p.set_enabled(Rule::UnobtainableItem, false);
        assert!(rules(&p, r#"{id:"minecraft:bedrock",count:1}"#, &[]).is_empty());
    }

    #[test]
    fn fixes_make_items_legal() {
        let p = Preset::load("illegal").unwrap();
        assert_eq!(
            fixed(&p, r#"{id:"minecraft:diamond_sword",count:1,components:{"minecraft:enchantments":{"minecraft:sharpness":255,"minecraft:smite":9},"minecraft:unbreakable":{}}}"#),
            Some(item(r#"{id:"minecraft:diamond_sword",count:1,components:{"minecraft:enchantments":{"minecraft:sharpness":5}}}"#))
        );
        assert_eq!(
            fixed(&p, r#"{id:"minecraft:diamond_sword",Count:64b,tag:{Enchantments:[{id:"minecraft:sharpness",lvl:32767s}]}}"#),
            Some(item(r#"{id:"minecraft:diamond_sword",Count:1b,tag:{Enchantments:[{id:"minecraft:sharpness",lvl:5s}]}}"#))
        );
        assert_eq!(
            fixed(&p, r#"{id:"minecraft:firework_rocket",count:1,components:{"minecraft:fireworks":{flight_duration:127b}}}"#),
            Some(item(r#"{id:"minecraft:firework_rocket",count:1,components:{"minecraft:fireworks":{flight_duration:3b}}}"#))
        );
        assert_eq!(
            fixed(&p, r#"{id:"minecraft:potion",count:1,components:{"minecraft:potion_contents":{potion:"minecraft:healing",custom_effects:[{id:"minecraft:wither"}]}}}"#),
            Some(item(r#"{id:"minecraft:potion",count:1,components:{"minecraft:potion_contents":{potion:"minecraft:healing"}}}"#))
        );
        assert_eq!(fixed(&p, r#"{id:"minecraft:barrier",count:1}"#), None);
    }
}
