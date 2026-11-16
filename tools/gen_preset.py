#!/usr/bin/env python3
"""Generate an mcscan "illegal items" preset from a vanilla Minecraft jar.

    python3 tools/gen_preset.py ~/.minecraft/versions/26.3/26.3.jar > presets/illegal.json

Enchantment limits, the items each enchantment may sit on and the mutually
exclusive enchantment sets are read straight from the jar's built-in data
pack, so re-running this against a newer jar is all that is needed to follow
a new game version.  Stack sizes and the unobtainable-item list are not
data-driven in the game, so they are described by the glob tables below and
expanded against the jar's item list.
"""
import fnmatch
import json
import sys
import zipfile

# ---- stack sizes (everything not matched stacks to 64) ----------------------
STACK_1 = [
    "*_sword", "*_pickaxe", "*_axe", "*_shovel", "*_hoe", "*_spear",
    "*_helmet", "*_chestplate", "*_leggings", "*_boots",
    "*_horse_armor", "*_nautilus_armor", "wolf_armor", "*_harness",
    "bow", "crossbow", "trident", "mace", "shield", "elytra", "fishing_rod",
    "carrot_on_a_stick", "warped_fungus_on_a_stick", "flint_and_steel",
    "shears", "brush", "spyglass", "totem_of_undying", "saddle",
    "*_bed", "*_boat", "*_raft", "minecart", "*_minecart",
    "*_bucket",  # filled buckets; the empty "bucket" stacks to 16
    "potion", "splash_potion", "lingering_potion",
    "enchanted_book", "writable_book", "knowledge_book", "debug_stick",
    "music_disc_*", "shulker_box", "*_shulker_box",
    "mushroom_stew", "rabbit_stew", "beetroot_soup", "suspicious_stew",
    "cake", "goat_horn", "bundle", "*_bundle", "*_banner_pattern",
]
STACK_16 = [
    "ender_pearl", "snowball", "egg", "blue_egg", "brown_egg",
    "*_sign", "*_banner", "bucket", "honey_bottle", "armor_stand",
    "written_book",
]

# ---- items with no survival source ------------------------------------------
UNOBTAINABLE = [
    "air", "barrier", "light", "bedrock", "structure_void", "structure_block",
    "jigsaw", "command_block", "chain_command_block", "repeating_command_block",
    "command_block_minecart", "test_block", "test_instance_block",
    "debug_stick", "knowledge_book", "end_portal_frame", "reinforced_deepslate",
    "budding_amethyst", "spawner", "trial_spawner", "vault", "farmland",
    "dirt_path", "frogspawn", "chorus_plant", "infested_*",
    "petrified_oak_slab", "player_head", "suspicious_sand", "suspicious_gravel",
    "*_spawn_egg",
]

# ---- item components that survival gameplay never puts on a stack -----------
DENIED_COMPONENTS = [
    "unbreakable", "attribute_modifiers", "max_stack_size", "max_damage",
    "enchantment_glint_override", "can_break", "can_place_on", "food",
    "consumable", "tool", "weapon", "equippable", "enchantable", "repairable",
    "glider", "death_protection", "damage_resistant", "fire_resistant",
    "use_cooldown", "use_remainder", "item_model", "tooltip_style",
    "hide_tooltip", "creative_slot_lock", "intangible_projectile", "lock",
    "container_loot", "entity_data", "block_entity_data", "debug_stick_state",
    "jukebox_playable", "blocks_attacks", "break_sound",
    "provides_trim_material", "provides_banner_patterns",
    "potion_duration_scale", "piercing_weapon", "kinetic_weapon",
    "swing_animation", "minimum_attack_charge", "damage_type", "use_effects",
    "attack_range", "block_transformer", "recipes",
]
# Only reported with --strict: harmless in vanilla terms, but never produced by
# survival play.  Plugins and data packs use these all the time.
STRICT_COMPONENTS = ["custom_data", "custom_model_data", "lore"]

# Combinations the current game refuses but that older releases really did
# hand out (1.14-1.14.2 stacked protections, pre-1.11.1 Infinity + Mending).
PROT = ["protection", "fire_protection", "blast_protection", "projectile_protection"]
LEGACY_OK = [[a, b] for i, a in enumerate(PROT) for b in PROT[i + 1:]]
LEGACY_OK.append(["infinity", "mending"])


def ns(s):
    return s if ":" in s else "minecraft:" + s


def main():
    if len(sys.argv) != 2:
        sys.exit(__doc__)
    z = zipfile.ZipFile(sys.argv[1])
    names = z.namelist()
    version = json.loads(z.read("version.json"))

    items = sorted(
        n[len("assets/minecraft/items/"):-5]
        for n in names
        if n.startswith("assets/minecraft/items/") and n.endswith(".json")
    )

    def expand(globs):
        out = []
        for g in globs:
            hit = fnmatch.filter(items, g)
            if not hit:
                print(f"warning: pattern {g!r} matches no item", file=sys.stderr)
            out += hit
        return sorted(set(out))

    def read_tag(kind, name, seen=()):
        """Flatten data/minecraft/tags/<kind>/<name>.json into a list of ids."""
        path = f"data/minecraft/tags/{kind}/{name}.json"
        out = []
        for v in json.loads(z.read(path))["values"]:
            if isinstance(v, dict):
                v = v["id"]
            if v.startswith("#"):
                sub = v[1:].split(":", 1)[-1]
                if sub not in seen:
                    out += read_tag(kind, sub, seen + (name,))
            else:
                out.append(ns(v))
        return sorted(set(out))

    item_tags = {}
    enchantments = {}
    prefix = "data/minecraft/enchantment/"
    for n in sorted(names):
        if not (n.startswith(prefix) and n.endswith(".json")):
            continue
        key = n[len(prefix):-5]
        d = json.loads(z.read(n))

        def holder_set(v, kind):
            """A holder set is a "#tag", a single id, or a list of ids."""
            if v is None:
                return []
            if isinstance(v, list):
                return [ns(x) for x in v]
            if v.startswith("#"):
                t = v[1:].split(":", 1)[-1]
                if kind == "item":
                    item_tags.setdefault(t, read_tag("item", t))
                    return ["#" + t]
                return read_tag(kind, t)
            return [ns(v)]

        me = ns(key)
        enchantments[me] = {
            "max_level": d["max_level"],
            "supported": holder_set(d.get("supported_items"), "item"),
            "exclusive": [e for e in holder_set(d.get("exclusive_set"), "enchantment") if e != me],
        }

    max_stack = {ns(i): 1 for i in expand(STACK_1)}
    for i in expand(STACK_16):
        max_stack[ns(i)] = 16

    preset = {
        "name": "illegal",
        "description": "Items that vanilla survival gameplay cannot produce",
        "game_version": version["name"],
        "data_version": version["world_version"],
        "enchantments": enchantments,
        "item_tags": item_tags,
        "legacy_conflict_ok": [[ns(a), ns(b)] for a, b in LEGACY_OK],
        "max_stack": dict(sorted(max_stack.items())),
        "unobtainable": [ns(i) for i in expand(UNOBTAINABLE)],
        "denied_components": [ns(c) for c in DENIED_COMPONENTS],
        "strict_components": [ns(c) for c in STRICT_COMPONENTS],
    }
    json.dump(preset, sys.stdout, indent=1, sort_keys=False)
    sys.stdout.write("\n")


if __name__ == "__main__":
    main()
